//! Single update engine shared by the CLI and the administration interface.
//!
//! The engine never trusts its inputs: a release must be signed by the trusted
//! Ed25519 key, every file hash must match the signed manifest, the database is
//! snapshotted before any change, and a failed update restores the Windows
//! binary, every replaced WSL component and the database before reporting.
//!
//! The running `workspace.exe` cannot replace itself on Windows. Therefore
//! [`start_update`] stages the verified *new* binary and launches it as a
//! detached updater. The launcher keeps a pipe to the updater and exits; the
//! pipe closing is the updater's proof that the old image is no longer locked.
//! Because the invocation binary may be an older release without these
//! commands, the updater always runs the new binary.

pub mod engine;
pub mod http;
pub mod journal;
pub mod manifest;
pub mod platform;
pub mod release;
mod source;

use crate::secrets::AuthSecrets;
use journal::{
    Journal, JournalPaths, UpdateAction, UpdateStatus, acquire_lock, load_journal, save_journal,
};
use manifest::{
    Compatibility, Manifest, ManifestError, ReleaseVersion, TrustAnchor, VerifiedPackage,
    sha256_file, verify_package,
};
use platform::{Layout, PlatformError, Wsl};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    time::Duration,
};
use workspace_core::config::Config;

pub use journal::{Step, UpdateStatus as Status};
pub use manifest::{MANIFEST_FILE, PRODUCT, SIGNATURE_FILE};
pub use platform::{InstalledInfo, Layout as InstallLayout, RuntimePaths, WslBackup};

const CHECK_CACHE: &str = "last-check.json";
const MAX_AUDIT_LINE: usize = 512;

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("platform error: {0}")]
    Platform(#[from] PlatformError),

    #[error("release package error: {0}")]
    Manifest(#[from] ManifestError),

    #[error("update lock error: {0}")]
    Lock(#[from] journal::LockError),

    #[error("another update is already running")]
    Busy,

    #[error("no update is available")]
    NoUpdate,

    #[error("the release is not compatible: {0}")]
    Incompatible(String),

    #[error("release downloads are not configured: {0}")]
    Download(String),

    #[error("authentication secrets are not available: {0}")]
    Secrets(#[from] crate::secrets::SecretsError),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("internal update error: {0}")]
    Internal(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl UpdateError {
    /// Stable machine code for the API and the journal.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Platform(PlatformError::Busy) | Self::Busy => "update_in_progress",
            Self::Lock(journal::LockError::Busy { .. }) => "update_in_progress",
            Self::Lock(_) => "internal",
            Self::Platform(PlatformError::Wsl(_)) => "wsl_unavailable",
            Self::Platform(PlatformError::Health(_)) => "health_check_failed",
            Self::Manifest(ManifestError::Signature | ManifestError::Untrusted) => {
                "release_untrusted"
            }
            Self::Manifest(ManifestError::File { .. }) => "release_corrupt",
            Self::Manifest(ManifestError::Io(_)) => "release_missing",
            Self::Manifest(_) => "release_invalid",
            Self::NoUpdate => "no_update",
            Self::Incompatible(_) => "incompatible",
            Self::Secrets(_) => "secrets_unavailable",
            Self::Config(_) => "invalid_configuration",
            Self::Download(_) => "download_failed",
            Self::Io(_) | Self::Internal(_) => "internal",
            _ => "internal",
        }
    }
}

/// What the caller asked the engine to do.
#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    pub package: Option<PathBuf>,
    pub install_dir: Option<PathBuf>,
    pub force: bool,
    pub allow_downgrade: bool,
    pub drain_timeout: Option<Duration>,
    pub bootstrap: bool,
    /// Wait for the invoking process to exit before replacing its binary.
    pub wait_for_invoker: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RollbackOptions {
    pub backup: Option<PathBuf>,
    pub install_dir: Option<PathBuf>,
    pub allow_database_restore: bool,
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseSummary {
    pub version: String,
    pub git_sha: String,
    pub build_time_unix: i64,
    pub channel: String,
    pub schema_version: u32,
    pub compatibility: Compatibility,
    pub changelog: Vec<String>,
    pub manifest_sha256: String,
    pub package_sha256: Option<String>,
    pub source: String,
}

impl ReleaseSummary {
    fn from_package(package: &VerifiedPackage, package_sha256: Option<String>) -> Self {
        Self {
            version: package.manifest.version.clone(),
            git_sha: package.manifest.git_sha.clone(),
            build_time_unix: package.manifest.build_time_unix,
            channel: package.manifest.channel.clone(),
            schema_version: package.manifest.schema_version,
            compatibility: package.manifest.compatibility.clone(),
            changelog: package.manifest.changelog.clone(),
            manifest_sha256: package.manifest_sha256.clone(),
            package_sha256,
            source: package.source.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckReport {
    pub checked_at_unix: i64,
    pub installed: InstalledInfo,
    pub available: Option<ReleaseSummary>,
    pub update_available: bool,
    /// `update_available`, `up_to_date`, `incompatible` or `error`.
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackCandidate {
    pub backup_dir: String,
    pub from_version: String,
    pub to_version: String,
    pub created_at_unix: i64,
    pub database_restore_requires_ack: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusReport {
    pub installed: InstalledInfo,
    pub journal: Option<Journal>,
    pub last_check: Option<CheckReport>,
    pub active_jobs: u64,
    pub server_pid: Option<u32>,
    pub rollback: Option<RollbackCandidate>,
    pub busy: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StartReport {
    pub journal: Journal,
    pub journal_path: String,
    pub updater_pid: Option<u32>,
    /// True when the update finished in this process (bootstrap).
    pub completed: bool,
}

#[derive(Debug, Clone)]
pub struct LoadedPackage {
    pub package: VerifiedPackage,
    pub package_sha256: Option<String>,
}

/// Resolve the trust anchor: an explicit configuration key wins, otherwise the
/// key compiled into the binary.
pub fn trust_anchor(config: &Config) -> Result<TrustAnchor, UpdateError> {
    match config.update.public_key_path.as_ref() {
        Some(path) => TrustAnchor::from_file(path).map_err(UpdateError::Manifest),
        None => TrustAnchor::embedded().map_err(UpdateError::Manifest),
    }
}

pub fn journal_paths(layout: &Layout) -> JournalPaths {
    JournalPaths::new(&layout.state_dir())
}

/// Files in the installation that belong to the updater.
pub fn installed_info(layout: &Layout) -> Result<InstalledInfo, UpdateError> {
    Ok(InstalledInfo::read(layout)?)
}

/// Probe another executable's build metadata by running `--version`.
///
/// The command is handled before configuration is loaded, so this is a pure
/// question about the binary on disk. A guard variable prevents recursion.
pub fn probe_binary_version(exe: &Path) -> Option<(String, String)> {
    platform::probe_binary(exe).map(|probe| (probe.version, probe.build_sha))
}

/// Effective current version: installation metadata, else the installed
/// binary, else the build this process was compiled from.
pub fn current_installation(layout: &Layout) -> Result<InstalledInfo, UpdateError> {
    let mut info = InstalledInfo::read(layout)?;

    if info.managed {
        return Ok(info);
    }

    // Never execute the binary that is currently running to ask for its own
    // version: in the update lab and during `workspace --version` that would
    // recurse. A different on-disk binary (the bootstrap case) is safe to
    // probe.
    let running = std::env::current_exe()
        .ok()
        .and_then(|exe| fs::canonicalize(exe).ok());
    let target = fs::canonicalize(&layout.exe).ok();

    if running.is_some() && running == target {
        return Ok(info);
    }

    if let Some((version, sha)) = probe_binary_version(&layout.exe) {
        info.version = version;
        info.git_sha = sha;
    }

    Ok(info)
}

pub async fn check(
    config: &Config,
    config_path: &Path,
    options: &UpdateOptions,
) -> Result<CheckReport, UpdateError> {
    let layout = Layout::resolve(config_path, config, options.install_dir.as_deref())?;
    let installed = current_installation(&layout)?;
    let trust = trust_anchor(config)?;

    let loaded =
        match source::load_package(config, &layout, options.package.as_deref(), &trust).await {
            Ok(package) => package,
            Err(error) => {
                let report = CheckReport {
                    checked_at_unix: journal::unix_now(),
                    installed,
                    available: None,
                    update_available: false,
                    status: "error".into(),
                    message: Some(error.to_string()),
                };

                let _ = cache_check(&layout, &report);

                return Ok(report);
            }
        };

    let summary = ReleaseSummary::from_package(&loaded.package, loaded.package_sha256.clone());

    let evaluation = evaluate_release(config, &layout, &installed, &loaded.package.manifest);

    let (status, update_available, message) = match evaluation {
        Ok(()) => {
            let available = parse_version(&summary.version)?;

            let current = parse_version(&installed.version)?;

            if available > current {
                ("update_available".to_owned(), true, None)
            } else {
                (
                    "up_to_date".to_owned(),
                    false,
                    Some(format!(
                        "installed version {} is current",
                        installed.version
                    )),
                )
            }
        }
        Err(message) => ("incompatible".to_owned(), false, Some(message)),
    };

    let report = CheckReport {
        checked_at_unix: journal::unix_now(),
        installed,
        available: Some(summary),
        update_available,
        status,
        message,
    };

    let _ = cache_check(&layout, &report);

    Ok(report)
}

fn parse_version(value: &str) -> Result<ReleaseVersion, UpdateError> {
    ReleaseVersion::from_str(value)
        .map_err(|_| UpdateError::Config(format!("invalid version {value}")))
}

fn validate_compatibility(
    config: &Config,
    layout: &Layout,
    installed: &InstalledInfo,
    release: &Manifest,
) -> Result<(), String> {
    if !release
        .compatibility
        .platforms
        .iter()
        .any(|platform| platform == manifest::PLATFORM_WINDOWS_X64)
    {
        return Err(format!(
            "release {} does not support Windows x86-64",
            release.version
        ));
    }

    let runtime_expected = if config.runtime.is_some() {
        "wsl2"
    } else {
        "none"
    };

    if !release
        .compatibility
        .runtime
        .iter()
        .any(|runtime| runtime == runtime_expected)
    {
        return Err(format!(
            "release {} is not tested with the {runtime_expected} runtime",
            release.version
        ));
    }

    if installed.schema_version > 0
        && !(release.compatibility.min_database_schema..=release.compatibility.max_database_schema)
            .contains(&installed.schema_version)
    {
        return Err(format!(
            "database schema {} is outside the supported range {}..={}",
            installed.schema_version,
            release.compatibility.min_database_schema,
            release.compatibility.max_database_schema
        ));
    }

    // A layout without a runtime never touches WSL, but a configured runtime
    // must at least look usable before staging anything.
    if let Some(runtime) = layout.runtime.as_ref()
        && config.runtime.is_some()
        && runtime.distro.is_empty()
    {
        return Err("runtime.distro is empty".into());
    }

    Ok(())
}

fn evaluate_release(
    config: &Config,
    layout: &Layout,
    installed: &InstalledInfo,
    release: &Manifest,
) -> Result<(), String> {
    validate_compatibility(config, layout, installed, release)?;

    Ok(())
}

fn cache_check(layout: &Layout, report: &CheckReport) -> Result<(), UpdateError> {
    let paths = journal_paths(layout);
    paths.ensure()?;

    platform::write_json(&paths.directory.join(CHECK_CACHE), report)?;

    Ok(())
}

pub fn last_check(layout: &Layout) -> Option<CheckReport> {
    let paths = journal_paths(layout);

    platform::read_json::<CheckReport>(&paths.directory.join(CHECK_CACHE)).ok()
}

pub fn status(config: &Config, config_path: &Path) -> Result<StatusReport, UpdateError> {
    status_with_install_dir(config, config_path, None)
}

pub fn status_with_install_dir(
    config: &Config,
    config_path: &Path,
    install_dir: Option<&Path>,
) -> Result<StatusReport, UpdateError> {
    let layout = Layout::resolve(config_path, config, install_dir)?;
    let installed = current_installation(&layout)?;
    let paths = journal_paths(&layout);

    let journal = load_journal(&paths);
    let active_jobs = platform::active_jobs(&layout.data_dir).unwrap_or(0);

    let busy = paths.lock().is_file()
        && journal
            .as_ref()
            .is_some_and(|journal| journal.status.is_active() && !journal.is_stale());

    Ok(StatusReport {
        installed,
        journal,
        last_check: normalized_check(&layout, last_check(&layout)),
        active_jobs,
        server_pid: platform::server_pid(&layout),
        rollback: rollback_candidate(&layout).ok().flatten(),
        busy,
    })
}

/// A cached check can be older than the installed release. Reporting "update
/// available" for the version that is already installed is misleading, so the
/// cached report is normalised before it reaches the UI.
fn normalized_check(layout: &Layout, report: Option<CheckReport>) -> Option<CheckReport> {
    let mut report = report?;

    let installed = current_installation(layout).ok()?;
    let Some(available) = report.available.as_ref() else {
        return Some(report);
    };

    let updates_available = parse_version(&available.version)
        .ok()
        .zip(parse_version(&installed.version).ok())
        .map(|(available, current)| available > current)
        .unwrap_or(available.version != installed.version);

    if !updates_available {
        report.update_available = false;
        report.status = "up_to_date".into();
        report.message = Some(format!(
            "installed version {} is current",
            installed.version
        ));
    }

    Some(report)
}

pub fn rollback_candidate(layout: &Layout) -> Result<Option<RollbackCandidate>, UpdateError> {
    let directory = layout.backups_dir();
    let installed = current_installation(layout)?;

    let Ok(entries) = fs::read_dir(&directory) else {
        return Ok(None);
    };

    let mut candidates = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let Ok(backup) =
            platform::read_json::<platform::BackupManifest>(&path.join(platform::BACKUP_FILE))
        else {
            continue;
        };

        // A backup is relevant when it produced the installed release
        // (ordinary rollback) or when it belongs to an interrupted update that
        // started from the installed release (recovery).
        if backup.action != "update"
            || (backup.to_version != installed.version && backup.from_version != installed.version)
        {
            continue;
        }

        candidates.push((backup.created_at_unix, path, backup));
    }

    candidates.sort_by_key(|(created, _, _)| *created);

    let Some((_, path, backup)) = candidates.pop() else {
        return Ok(None);
    };

    let database_restore_requires_ack = database_restore_requires_ack(layout, &installed, &backup);

    Ok(Some(RollbackCandidate {
        backup_dir: path.display().to_string(),
        from_version: backup.from_version,
        to_version: backup.to_version,
        created_at_unix: backup.created_at_unix,
        database_restore_requires_ack,
    }))
}

/// Whether restoring this backup would replace data written after the update.
fn database_restore_requires_ack(
    layout: &Layout,
    installed: &InstalledInfo,
    backup: &platform::BackupManifest,
) -> bool {
    let current_database =
        manifest::sha256_file(&crate::storage::database_path(&layout.data_dir)).ok();

    let database_changed = match (
        installed.database_sha256.as_deref(),
        current_database.as_deref(),
    ) {
        (Some(expected), Some(actual)) => expected != actual,
        // Without a recorded hash we cannot prove that no data was written,
        // so the operator has to confirm the restore explicitly.
        _ => true,
    };

    database_changed
        || installed.version != backup.to_version
        || installed.schema_version != backup.to_schema
}

fn secrets_for_update(config: &Config, layout: &Layout) -> Result<AuthSecrets, UpdateError> {
    if config.sso.is_none() {
        return AuthSecrets::from_values("local-development-secret", &base64_key())
            .map_err(UpdateError::Secrets);
    }

    Ok(AuthSecrets::load(&layout.data_dir)?)
}

fn base64_key() -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};

    STANDARD.encode([7u8; 32])
}

fn write_audit(layout: &Layout, action: &str, actor: &str, outcome: &str) {
    use std::io::Write;

    let paths = journal_paths(layout);

    if paths.ensure().is_err() {
        return;
    }

    let clean = |value: &str| -> String {
        value
            .chars()
            .filter(|character| *character != '\n' && *character != '\r')
            .take(200)
            .collect()
    };

    let line = format!(
        "[{}] action={} actor={} outcome={}\n",
        journal::unix_now(),
        clean(action),
        clean(actor),
        clean(outcome)
    );

    if line.len() <= MAX_AUDIT_LINE
        && let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.directory.join("audit.log"))
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Record an administrative action even when the caller only has the
/// configuration path (the HTTP layer).
pub fn audit_event(config: &Config, config_path: &Path, action: &str, actor: &str, outcome: &str) {
    if let Ok(layout) = Layout::resolve(config_path, config, None) {
        write_audit(&layout, action, actor, outcome);
    }
}

/// Preflight + staging, then either run the update in this process (bootstrap,
/// where the invoking binary is not the installed one) or start the detached
/// updater and return.
pub async fn start_update(
    config: &Config,
    config_path: &Path,
    options: &UpdateOptions,
    actor: &str,
) -> Result<StartReport, UpdateError> {
    let layout = Layout::resolve(config_path, config, options.install_dir.as_deref())?;

    if !layout.install_dir.is_dir() {
        return Err(UpdateError::Config(format!(
            "installation directory {} does not exist",
            layout.install_dir.display()
        )));
    }

    let installed = current_installation(&layout)?;

    if !options.bootstrap && !layout.exe.is_file() {
        return Err(UpdateError::Config(format!(
            "{} does not exist; run the bootstrap command for a first installation",
            layout.exe.display()
        )));
    }

    let trust = trust_anchor(config)?;

    let loaded = source::load_package(config, &layout, options.package.as_deref(), &trust).await?;

    evaluate_release(config, &layout, &installed, &loaded.package.manifest)
        .map_err(UpdateError::Incompatible)?;

    let from = parse_version(&installed.version)?;
    let to = parse_version(&loaded.package.manifest.version)?;

    if to < from && !options.allow_downgrade {
        return Err(UpdateError::Incompatible(format!(
            "refusing to downgrade {} to {} without --allow-downgrade",
            installed.version, loaded.package.manifest.version
        )));
    }

    if to == from && !options.force {
        return Err(UpdateError::NoUpdate);
    }

    // Secrets must exist before anything is stopped. They are read from the
    // environment or from the DPAPI-protected copy written at startup.
    let _secrets = secrets_for_update(config, &layout)?;

    preflight_environment(&layout, &loaded.package, config).await?;

    let paths = journal_paths(&layout);
    paths.ensure()?;

    let server_was_running = platform::server_pid(&layout).is_some();

    let mut journal = Journal::new(
        UpdateAction::Update,
        &layout.install_dir,
        &paths.log(),
        &installed.version,
        &installed.git_sha,
        &loaded.package.manifest.version,
        installed.schema_version,
        loaded.package.manifest.schema_version,
        server_was_running,
    );

    journal.package_dir = Some(loaded.package.root.display().to_string());

    let lock = acquire_lock(
        &paths,
        &journal.token,
        UpdateAction::Update,
        &paths.journal(),
    )?;

    // Materialise the verified package inside the installation. The original
    // directory may be a network share or a temporary extraction that is
    // removed when the launcher exits.
    let staging = layout.updates_dir().join("staging").join(&journal.id);

    let staged = match materialize_package(&loaded.package, &staging).and_then(|()| {
        verify_package(&staging, &trust, loaded.package.source.clone()).map_err(UpdateError::from)
    }) {
        Ok(staged) => staged,
        Err(error) => {
            lock.release();
            return Err(error);
        }
    };

    journal.package_dir = Some(staged.root.display().to_string());
    journal.backup_dir = Some(
        layout
            .backups_dir()
            .join(format!(
                "release-{}-{}",
                journal.started_at_unix,
                &journal.token[..8.min(journal.token.len())]
            ))
            .display()
            .to_string(),
    );
    journal.status = UpdateStatus::Prepared;

    save_journal(&paths, &journal)?;
    journal::log(
        &paths,
        &format!(
            "prepared update {} -> {} (package {})",
            journal.from_version, journal.to_version, staged.manifest_sha256
        ),
    );

    write_audit(
        &layout,
        "update.start",
        actor,
        &format!("{} -> {}", journal.from_version, journal.to_version),
    );

    let current_exe = std::env::current_exe()?;
    let running_installed_binary = same_path(&current_exe, &layout.exe);

    if options.bootstrap || !running_installed_binary {
        // The invoking binary is not the installation's image: it can replace
        // the target while it runs. This is the first-update path.
        let secrets = secrets_for_update(config, &layout)?;

        let mut foreground = options.clone();
        foreground.wait_for_invoker = false;

        let result = engine::run_update(
            &layout,
            config,
            &paths,
            &mut journal,
            &staged,
            &secrets,
            &foreground,
        )
        .await;

        lock.release();

        result?;

        return Ok(StartReport {
            journal,
            journal_path: paths.journal().display().to_string(),
            updater_pid: None,
            completed: true,
        });
    }

    // Detached updater: the new binary performs every change. The parent holds
    // the pipe until it exits, which is how the child knows the old image is
    // unlocked.
    let updater = staging.join(if cfg!(windows) {
        "workspace.exe"
    } else {
        "workspace"
    });

    if !updater.is_file() {
        lock.release();
        return Err(UpdateError::Manifest(ManifestError::Invalid(
            "staged release does not contain the updater binary".into(),
        )));
    }

    let child = match spawn_updater(&updater, &layout, &paths.journal(), &journal) {
        Ok(child) => child,
        Err(error) => {
            lock.release();
            return Err(error);
        }
    };

    let pid = child.id();

    // Keep the pipe (and the process handle) open until this process exits.
    // The child waits for EOF before touching the installation.
    std::mem::forget(child);
    std::mem::forget(lock);

    write_audit(
        &layout,
        "update.spawned",
        actor,
        &format!("updater pid {pid}"),
    );

    Ok(StartReport {
        journal,
        journal_path: paths.journal().display().to_string(),
        updater_pid: Some(pid),
        completed: false,
    })
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn spawn_updater(
    updater: &Path,
    layout: &Layout,
    journal_path: &Path,
    journal: &Journal,
) -> Result<std::process::Child, UpdateError> {
    use std::process::Stdio;

    let paths = journal_paths(layout);

    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.directory.join("apply.log"))?;

    let mut command = Command::new(updater);

    command
        .arg("--config")
        .arg(&layout.config_path)
        .arg("update")
        .arg("apply")
        .arg("--install-dir")
        .arg(&layout.install_dir)
        .arg("--journal")
        .arg(journal_path)
        .arg("--wait-invoker")
        .current_dir(&layout.install_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    let child = command
        .spawn()
        .map_err(|error| UpdateError::Internal(format!("cannot start the updater: {error}")))?;

    journal::log(
        &paths,
        &format!(
            "launcher started updater {} for journal {}",
            journal.id,
            journal_path.display()
        ),
    );

    Ok(child)
}

async fn preflight_environment(
    layout: &Layout,
    package: &VerifiedPackage,
    config: &Config,
) -> Result<(), UpdateError> {
    let package_bytes: u64 = package.manifest.files.iter().map(|file| file.size).sum();
    let database_bytes = fs::metadata(crate::storage::database_path(&layout.data_dir))
        .map(|metadata| metadata.len())
        .unwrap_or(0);

    let required = package_bytes
        .saturating_mul(2)
        .saturating_add(database_bytes.saturating_mul(3))
        .saturating_add(config.update.reserve_free_mib().saturating_mul(1024 * 1024));

    let free = workspace_os::disk::free_bytes(&layout.install_dir)
        .map_err(|error| UpdateError::Platform(PlatformError::Process(error.to_string())))?;

    if free < required {
        return Err(UpdateError::Incompatible(format!(
            "not enough free space on the Windows volume: {} MiB required, {} MiB available",
            required / (1024 * 1024),
            free / (1024 * 1024)
        )));
    }

    if layout.wsl_required() {
        let runtime = layout.runtime.as_ref().expect("checked");
        let wsl = Wsl::new(runtime)?;

        wsl.available()
            .map_err(|error| UpdateError::Incompatible(error.to_string()))?;

        let free = wsl.free_bytes(&runtime.data_root)?;

        if free < required {
            return Err(UpdateError::Incompatible(format!(
                "not enough free space in the WSL runtime: {} MiB required, {} MiB available",
                required / (1024 * 1024),
                free / (1024 * 1024)
            )));
        }

        for (role, path) in [
            ("helper", &runtime.helper_path),
            ("egress", &runtime.egress_path),
            ("quota", &runtime.quota_path),
            ("sudoers", &runtime.sudoers_path),
        ] {
            let parent = path
                .rsplit_once('/')
                .map(|(directory, _)| directory)
                .unwrap_or("/");

            let output = wsl
                .run(
                    "root",
                    "/usr/bin/test",
                    &["-d".to_owned(), parent.to_owned()],
                )
                .map_err(|error| UpdateError::Incompatible(error.to_string()))?;

            if !output.status.success() {
                return Err(UpdateError::Incompatible(format!(
                    "runtime component directory {parent} for the {role} is missing; run the provisioning script first"
                )));
            }
        }

        if platform::server_pid(layout).is_none() && platform::health(layout).await.is_ok() {
            return Err(UpdateError::Incompatible(
                "the configured port is served by a process that does not belong to this installation"
                    .into(),
            ));
        }
    }

    Ok(())
}

fn materialize_package(package: &VerifiedPackage, staging: &Path) -> Result<(), UpdateError> {
    if staging.exists() {
        fs::remove_dir_all(staging)?;
    }

    fs::create_dir_all(staging)?;

    fs::copy(
        package.root.join(MANIFEST_FILE),
        staging.join(MANIFEST_FILE),
    )?;

    fs::copy(
        package.root.join(SIGNATURE_FILE),
        staging.join(SIGNATURE_FILE),
    )?;

    for file in &package.manifest.files {
        let source = package.root.join(&file.path);
        let target = staging.join(&file.path);

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::copy(&source, &target)?;

        if sha256_file(&target)? != file.sha256 {
            return Err(UpdateError::Manifest(ManifestError::File {
                path: file.path.clone(),
                reason: "staging copy is damaged".into(),
            }));
        }
    }

    Ok(())
}

/// Entry point of the detached updater process.
pub async fn apply_child(
    config: &Config,
    config_path: &Path,
    install_dir: &Path,
    journal_path: &Path,
    wait_for_launcher: bool,
) -> Result<StartReport, UpdateError> {
    let layout = Layout::resolve(config_path, config, Some(install_dir))?;
    let paths = journal_paths(&layout);

    let mut journal = load_journal(&paths).ok_or_else(|| {
        UpdateError::Internal(format!("journal {} is missing", journal_path.display()))
    })?;

    if journal.status.is_terminal() {
        return Err(UpdateError::Internal(
            "the journal is already finished".into(),
        ));
    }

    // Verify this process owns the lock before touching anything.
    let lock_bytes = fs::read(paths.lock()).map_err(|_| UpdateError::Busy)?;
    let lock: serde_json::Value = serde_json::from_slice(&lock_bytes)
        .map_err(|_| UpdateError::Internal("the update lock is invalid".into()))?;

    if lock["token"].as_str() != Some(journal.token.as_str()) {
        return Err(UpdateError::Busy);
    }

    // The launcher exits immediately after spawning this process. Take over the
    // lock (its pid must name this process) before the launcher disappears.
    journal::adopt_lock(&paths, &journal.token, journal.action, journal_path)?;

    // The journal must name this process: a dead launcher pid would otherwise
    // make the operation look interrupted while the updater is working.
    journal.pid = std::process::id();
    journal.updated_at_unix = journal::unix_now();
    save_journal(&paths, &journal)?;

    let secrets = secrets_for_update(config, &layout)?;

    let outcome = async {
        if journal.action == UpdateAction::Rollback {
            let backup = journal.backup_dir.clone().map(PathBuf::from);

            return engine::run_rollback(
                &layout,
                config,
                &paths,
                &mut journal,
                &secrets,
                &RollbackOptions {
                    backup,
                    install_dir: Some(layout.install_dir.clone()),
                    allow_database_restore: true,
                    force: true,
                },
            )
            .await;
        }

        let package_root = journal
            .package_dir
            .as_ref()
            .map(PathBuf::from)
            .ok_or_else(|| UpdateError::Internal("journal has no package".into()))?;

        let trust = trust_anchor(config)?;

        let package = verify_package(&package_root, &trust, package_root.display().to_string())?;

        let options = UpdateOptions {
            package: Some(package_root),
            install_dir: Some(layout.install_dir.clone()),
            force: true,
            allow_downgrade: false,
            drain_timeout: Some(config.update.drain_timeout()),
            bootstrap: false,
            wait_for_invoker: wait_for_launcher,
        };

        engine::run_update(
            &layout,
            config,
            &paths,
            &mut journal,
            &package,
            &secrets,
            &options,
        )
        .await
    }
    .await;

    if let Err(error) = outcome {
        // Package/trust errors happen before the engine touches anything and
        // would otherwise leave a "prepared" journal and a dead lock behind.
        if journal.status.is_active() {
            journal.status = UpdateStatus::Failed;
            journal.stage = "failed".into();
            journal.error = Some(error.to_string());
            journal.updated_at_unix = journal::unix_now();
            let _ = save_journal(&paths, &journal);
        }

        journal::release_lock(&paths, &journal.token);

        return Err(error);
    }

    Ok(StartReport {
        journal,
        journal_path: paths.journal().display().to_string(),
        updater_pid: Some(std::process::id()),
        completed: true,
    })
}

pub(crate) async fn wait_for_invoker_exit(timeout: Duration) -> Result<(), UpdateError> {
    let read = tokio::task::spawn_blocking(|| {
        use std::io::Read;

        let mut sink = Vec::new();

        std::io::stdin().read_to_end(&mut sink)
    });

    match tokio::time::timeout(timeout, read).await {
        Ok(Ok(Ok(_))) => Ok(()),
        Ok(Ok(Err(error))) => Err(UpdateError::Internal(format!(
            "cannot wait for the launcher: {error}"
        ))),
        Ok(Err(error)) => Err(UpdateError::Internal(format!(
            "cannot wait for the launcher: {error}"
        ))),
        Err(_) => Err(UpdateError::Internal(
            "the launcher did not exit in time; no changes were made".into(),
        )),
    }
}

/// Start a rollback of the last successful update.
pub async fn start_rollback(
    config: &Config,
    config_path: &Path,
    options: &RollbackOptions,
    actor: &str,
) -> Result<StartReport, UpdateError> {
    let layout = Layout::resolve(config_path, config, options.install_dir.as_deref())?;
    let installed = current_installation(&layout)?;

    if config.sso.is_some() {
        AuthSecrets::load(&layout.data_dir)?;
    }

    let backup_dir = match options.backup.as_ref() {
        Some(path) => path.clone(),
        None => rollback_candidate(&layout)?
            .map(|candidate| PathBuf::from(candidate.backup_dir))
            .ok_or_else(|| UpdateError::Config("no update backup is available".into()))?,
    };

    let backup: platform::BackupManifest =
        platform::read_json(&backup_dir.join(platform::BACKUP_FILE))?;

    if backup.action != "update" {
        return Err(UpdateError::Config(
            "the selected backup is not an update backup".into(),
        ));
    }

    if installed.version != backup.to_version
        && installed.version != backup.from_version
        && !options.force
    {
        return Err(UpdateError::Incompatible(format!(
            "the installation is at {} but the backup belongs to {} -> {}; pass force to override",
            installed.version, backup.from_version, backup.to_version
        )));
    }

    let candidate_requires_ack = database_restore_requires_ack(&layout, &installed, &backup);

    if candidate_requires_ack && !options.allow_database_restore {
        return Err(UpdateError::Incompatible(
            "the database changed after the update; confirm that data written since then may be lost"
                .into(),
        ));
    }

    let paths = journal_paths(&layout);
    paths.ensure()?;

    let server_was_running = platform::server_pid(&layout).is_some();

    let mut journal = Journal::new(
        UpdateAction::Rollback,
        &layout.install_dir,
        &paths.log(),
        &installed.version,
        &installed.git_sha,
        &backup.from_version,
        installed.schema_version,
        backup.from_schema,
        server_was_running,
    );

    journal.backup_dir = Some(backup_dir.display().to_string());
    journal.warnings.push(if candidate_requires_ack {
        "database changes made after the update will be replaced by the pre-update snapshot; a safety copy is kept".into()
    } else {
        "no database changes were detected after the update".into()
    });

    let lock = acquire_lock(
        &paths,
        &journal.token,
        UpdateAction::Rollback,
        &paths.journal(),
    )?;

    save_journal(&paths, &journal)?;

    write_audit(&layout, "rollback.start", actor, &backup.id);

    let current_exe = std::env::current_exe()?;
    let running_installed_binary = same_path(&current_exe, &layout.exe);

    if !running_installed_binary {
        let secrets = secrets_for_update(config, &layout)?;

        let result =
            engine::run_rollback(&layout, config, &paths, &mut journal, &secrets, options).await;

        lock.release();

        result?;

        return Ok(StartReport {
            journal,
            journal_path: paths.journal().display().to_string(),
            updater_pid: None,
            completed: true,
        });
    }

    // The running image cannot be replaced by itself. Copy it next to the
    // installation and let the copy perform the rollback after this process
    // exits.
    let updater_dir = layout
        .updates_dir()
        .join(format!("rollback-{}", journal.id));
    fs::create_dir_all(&updater_dir)?;

    let updater = updater_dir.join(
        layout
            .exe
            .file_name()
            .ok_or_else(|| UpdateError::Internal("executable has no name".into()))?,
    );

    fs::copy(&layout.exe, &updater)?;

    let child = match spawn_updater(&updater, &layout, &paths.journal(), &journal) {
        Ok(child) => child,
        Err(error) => {
            lock.release();
            return Err(error);
        }
    };

    let pid = child.id();

    std::mem::forget(child);
    std::mem::forget(lock);

    write_audit(
        &layout,
        "rollback.spawned",
        actor,
        &format!("updater pid {pid}"),
    );

    Ok(StartReport {
        journal,
        journal_path: paths.journal().display().to_string(),
        updater_pid: Some(pid),
        completed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn version_ranges_are_checked_numerically() {
        assert!(
            ReleaseVersion::from_str("1.10.0").unwrap()
                > ReleaseVersion::from_str("1.9.9").unwrap()
        );
    }

    #[test]
    fn status_codes_are_stable() {
        assert_eq!(UpdateError::NoUpdate.code(), "no_update");
        assert_eq!(UpdateError::Busy.code(), "update_in_progress");
        assert_eq!(UpdateError::Incompatible("x".into()).code(), "incompatible");
    }
}
