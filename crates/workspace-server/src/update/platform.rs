//! Operating-system side of an update: layout, files, WSL components and the
//! Workspace server process itself.
//!
//! Everything here is deliberately explicit: fixed programs, fixed paths from
//! validated configuration, no shell, no user-supplied command line.

use crate::{
    storage,
    update::journal::unix_now,
    update::manifest::{ManifestError, sha256_file, sha256_hex},
};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};
use workspace_core::{
    BUILD_TIME, GIT_SHA, PRODUCT_NAME, VERSION,
    config::{Config, RuntimeBackendKind, UpdateConfig},
};

pub const INSTALLED_FILE: &str = "installed.json";
pub const BACKUP_FILE: &str = "backup.json";
pub const SERVER_PID_FILE: &str = "server.pid";
pub const SHUTDOWN_REQUEST_FILE: &str = "shutdown.request";

const SHUTDOWN_GRACE: Duration = Duration::from_secs(8);
const TERMINATE_GRACE: Duration = Duration::from_secs(10);
const MAX_LOG_TAIL: usize = 32 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("installation path error: {0}")]
    Layout(String),

    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Database(#[from] storage::StorageError),

    #[error("WSL runtime error: {0}")]
    Wsl(String),

    #[error("server did not become healthy: {0}")]
    Health(String),

    #[error("process error: {0}")]
    Process(String),

    #[error("release package error: {0}")]
    Manifest(#[from] ManifestError),

    #[error("another update owns the installation")]
    Busy,
}

impl From<rusqlite::Error> for PlatformError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(storage::StorageError::from(error))
    }
}

/// Resolved Linux runtime component layout.
#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub distro: String,
    pub user: String,
    pub data_root: String,
    pub helper_path: String,
    pub egress_path: String,
    pub quota_path: String,
    pub sudoers_path: String,
}

/// Resolved installation layout for one Workspace instance.
#[derive(Debug, Clone)]
pub struct Layout {
    pub install_dir: PathBuf,
    pub exe: PathBuf,
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub listen: std::net::SocketAddr,
    pub update: UpdateConfig,
    pub runtime: Option<RuntimePaths>,
}

impl Layout {
    pub fn resolve(
        config_path: &Path,
        config: &Config,
        install_override: Option<&Path>,
    ) -> Result<Self, PlatformError> {
        let config_path = fs::canonicalize(config_path)?;

        let install_dir = match install_override {
            Some(directory) => absolute(directory)?,
            None => match config.update.install_dir.as_ref() {
                Some(directory) => absolute(directory)?,
                None => {
                    let exe = std::env::current_exe()?;

                    exe.parent().map(Path::to_path_buf).ok_or_else(|| {
                        PlatformError::Layout("cannot locate the executable".into())
                    })?
                }
            },
        };

        let data_dir = absolute(&config.storage.data_dir)?;

        let exe = if cfg!(windows) {
            install_dir.join("workspace.exe")
        } else {
            install_dir.join("workspace")
        };

        let runtime = match config.runtime.as_ref() {
            Some(runtime) => {
                let (distro, user) = match runtime.backend {
                    RuntimeBackendKind::Wsl2 => (
                        runtime.distro.clone().ok_or_else(|| {
                            PlatformError::Layout("runtime.distro is required".into())
                        })?,
                        runtime.user.clone().ok_or_else(|| {
                            PlatformError::Layout("runtime.user is required".into())
                        })?,
                    ),
                    RuntimeBackendKind::RootlessDocker => (String::new(), String::new()),
                };

                Some(RuntimePaths {
                    distro,
                    user,
                    data_root: runtime.data_root.clone(),
                    helper_path: runtime.helper_path.clone(),
                    egress_path: runtime.egress_binary(config.network.egress_binary.as_deref()),
                    quota_path: runtime.quota_helper().to_owned(),
                    sudoers_path: runtime.sudoers().to_owned(),
                })
            }
            None => None,
        };

        Ok(Self {
            install_dir,
            exe,
            config_path,
            data_dir,
            listen: config.server.listen,
            update: config.update.clone(),
            runtime,
        })
    }

    pub fn state_dir(&self) -> PathBuf {
        self.install_dir.join("state")
    }

    pub fn updates_dir(&self) -> PathBuf {
        self.install_dir.join("updates")
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.install_dir.join("backups")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.install_dir.join("logs")
    }

    pub fn deploy_dir(&self) -> PathBuf {
        self.install_dir.join("deploy")
    }

    pub fn installed_file(&self) -> PathBuf {
        self.state_dir().join(INSTALLED_FILE)
    }

    pub fn server_pid_file(&self) -> PathBuf {
        self.state_dir().join(SERVER_PID_FILE)
    }

    pub fn shutdown_request(&self) -> PathBuf {
        self.state_dir().join(SHUTDOWN_REQUEST_FILE)
    }

    pub fn server_log(&self, name: &str) -> PathBuf {
        self.logs_dir().join(name)
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.listen)
    }

    pub fn wsl_required(&self) -> bool {
        self.runtime
            .as_ref()
            .is_some_and(|runtime| !runtime.distro.is_empty())
    }
}

fn absolute(path: &Path) -> Result<PathBuf, PlatformError> {
    if path.as_os_str().is_empty() {
        return Err(PlatformError::Layout("empty path".into()));
    }

    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    // Existing directories are canonicalised; a directory that does not exist
    // yet is still returned as an absolute path (the caller creates it).
    match fs::canonicalize(&path) {
        Ok(canonical) => Ok(canonical),
        Err(_) => Ok(dedup_components(&path)),
    }
}

fn dedup_components(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();

    for component in path.components() {
        output.push(component);
    }

    output
}

/// Copy of the installation metadata written after a successful update.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledInfo {
    pub product: String,
    pub version: String,
    pub git_sha: String,
    pub build_time: String,
    pub installed_at_unix: Option<i64>,
    pub schema_version: u32,
    #[serde(default)]
    pub package_sha256: Option<String>,
    #[serde(default)]
    pub previous_version: Option<String>,
    #[serde(default)]
    pub backup: Option<String>,
    #[serde(default)]
    pub database_sha256: Option<String>,
    pub managed: bool,
    pub install_dir: String,
}

impl InstalledInfo {
    pub fn read(layout: &Layout) -> Result<Self, PlatformError> {
        let path = layout.installed_file();

        let schema = storage::inspect(&layout.data_dir)
            .map(|info| info.map(|info| info.schema_version).unwrap_or(0))
            .unwrap_or(0);

        if let Ok(bytes) = fs::read(&path)
            && let Ok(mut info) = serde_json::from_slice::<Self>(&bytes)
        {
            // The database is the authority for the schema actually in use.
            info.schema_version = schema;
            info.install_dir = layout.install_dir.display().to_string();
            return Ok(info);
        }

        Ok(Self {
            product: PRODUCT_NAME.to_owned(),
            version: VERSION.to_owned(),
            git_sha: GIT_SHA.to_owned(),
            build_time: BUILD_TIME.to_owned(),
            installed_at_unix: None,
            schema_version: schema,
            package_sha256: None,
            previous_version: None,
            backup: None,
            database_sha256: None,
            managed: false,
            install_dir: layout.install_dir.display().to_string(),
        })
    }

    pub fn write(&self, layout: &Layout) -> Result<(), PlatformError> {
        fs::create_dir_all(layout.state_dir())?;

        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| PlatformError::Layout(error.to_string()))?;

        fs::write(layout.installed_file(), bytes)?;

        Ok(())
    }
}

/// Running server identity reported by the public health endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHealth {
    pub status: String,
    #[serde(default)]
    pub product: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub build_sha: String,
    #[serde(default)]
    pub schema_version: u32,
}

pub async fn health(layout: &Layout) -> Result<ServerHealth, PlatformError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|error| PlatformError::Health(error.to_string()))?;

    let response = client
        .get(format!("{}/health", layout.base_url()))
        .send()
        .await
        .map_err(|error| PlatformError::Health(error.to_string()))?;

    if !response.status().is_success() {
        return Err(PlatformError::Health(format!(
            "health returned HTTP {}",
            response.status()
        )));
    }

    response
        .json::<ServerHealth>()
        .await
        .map_err(|error| PlatformError::Health(error.to_string()))
}

/// Wait until the server reports this exact version and (optionally) schema.
pub async fn wait_healthy(
    layout: &Layout,
    expected_version: &str,
    expected_sha: Option<&str>,
    expected_schema: Option<u32>,
    timeout: Duration,
) -> Result<ServerHealth, PlatformError> {
    let deadline = Instant::now() + timeout;
    let mut last = String::from("no response");

    while Instant::now() < deadline {
        match health(layout).await {
            Ok(info) if info.status == "ok" => {
                if info.version != expected_version {
                    last = format!(
                        "server reports version {}, expected {expected_version}",
                        info.version
                    );
                } else if expected_schema.is_some_and(|schema| info.schema_version != schema) {
                    last = format!(
                        "server reports schema {}, expected {}",
                        info.schema_version,
                        expected_schema.unwrap_or_default()
                    );
                } else if let Some(expected) = expected_sha
                    && !expected.is_empty()
                    && info.build_sha != expected
                {
                    last = format!(
                        "server reports build {}, expected {expected}",
                        info.build_sha
                    );
                } else {
                    return Ok(info);
                }
            }
            Ok(info) => last = format!("health status {}", info.status),
            Err(error) => last = error.to_string(),
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    Err(PlatformError::Health(format!(
        "timed out after {}s: {last}",
        timeout.as_secs()
    )))
}

/// Number of queued/running jobs, read directly from SQLite.
pub fn active_jobs(data_dir: &Path) -> Result<u64, PlatformError> {
    let path = storage::database_path(data_dir);

    if !path.exists() {
        return Ok(0);
    }

    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    connection.busy_timeout(Duration::from_secs(5))?;

    let count: i64 = connection.query_row(
        "SELECT count(*) FROM jobs WHERE state IN ('queued', 'running')",
        [],
        |row| row.get(0),
    )?;

    Ok(u64::try_from(count).unwrap_or(0))
}

pub fn wait_for_drain(
    data_dir: &Path,
    timeout: Duration,
    mut on_wait: impl FnMut(u64),
) -> Result<(), PlatformError> {
    let deadline = Instant::now() + timeout;
    let mut last = u64::MAX;

    loop {
        let active = active_jobs(data_dir)?;

        if active == 0 {
            return Ok(());
        }

        if active != last {
            on_wait(active);
            last = active;
        }

        if Instant::now() >= deadline {
            return Err(PlatformError::Process(format!(
                "{active} active job(s) did not finish within {}s",
                timeout.as_secs()
            )));
        }

        std::thread::sleep(Duration::from_millis(500));
    }
}

fn canonical_compare(left: &Path, right: &Path) -> bool {
    let normalise = |path: &Path| {
        fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .to_lowercase()
            .replace('/', "\\")
    };

    normalise(left) == normalise(right)
}

/// Pid of this installation's server, if it is actually running its binary.
pub fn server_pid(layout: &Layout) -> Option<u32> {
    let text = fs::read_to_string(layout.server_pid_file()).ok()?;
    let pid: u32 = text.trim().parse().ok()?;

    if !workspace_os::process::is_alive(pid) {
        return None;
    }

    match workspace_os::process::image_path(pid) {
        Some(path) if canonical_compare(&path, &layout.exe) => Some(pid),
        // The path could not be read: refuse to treat an unknown process as
        // ours.
        _ => None,
    }
}

pub fn stop_server(layout: &Layout) -> Result<bool, PlatformError> {
    let Some(pid) = server_pid(layout) else {
        let _ = fs::remove_file(layout.server_pid_file());
        return Ok(false);
    };

    let request = layout.shutdown_request();

    if let Some(parent) = request.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(&request, unix_now().to_string())?;

    if workspace_os::process::wait_for_exit(pid, SHUTDOWN_GRACE) {
        let _ = fs::remove_file(&request);
        let _ = fs::remove_file(layout.server_pid_file());
        return Ok(true);
    }

    // The previous build may not watch for the request file. Terminating is
    // still safe: SQLite runs in WAL mode and the process is the one whose
    // image path is exactly this installation's binary.
    workspace_os::process::terminate(pid)
        .map_err(|error| PlatformError::Process(error.to_string()))?;

    if !workspace_os::process::wait_for_exit(pid, TERMINATE_GRACE) {
        return Err(PlatformError::Process(format!(
            "server process {pid} did not stop"
        )));
    }

    let _ = fs::remove_file(&request);
    let _ = fs::remove_file(layout.server_pid_file());

    Ok(true)
}

/// Start the installed server detached, with the same process shape the
/// deployment script uses (hidden window, redirected logs, pid file).
pub fn start_server(
    layout: &Layout,
    client_secret: &str,
    auth_key: &str,
) -> Result<u32, PlatformError> {
    if !layout.exe.is_file() {
        return Err(PlatformError::Layout(format!(
            "{} does not exist",
            layout.exe.display()
        )));
    }

    fs::create_dir_all(layout.logs_dir())?;
    fs::create_dir_all(layout.state_dir())?;

    let stdout = File::options()
        .create(true)
        .append(true)
        .open(layout.server_log("server.out.log"))?;

    let stderr = File::options()
        .create(true)
        .append(true)
        .open(layout.server_log("server.err.log"))?;

    let mut command = Command::new(&layout.exe);

    command
        .arg("--config")
        .arg(&layout.config_path)
        .arg("serve")
        .current_dir(&layout.install_dir)
        .env("WORKSPACE_SSO_CLIENT_SECRET", client_secret)
        .env("WORKSPACE_AUTH_KEY", auth_key)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

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
        .map_err(|error| PlatformError::Process(error.to_string()))?;

    let pid = child.id();

    fs::write(layout.server_pid_file(), pid.to_string())?;

    Ok(pid)
}

/// Tail of a server log, bounded and stripped of control characters.
pub fn log_tail(layout: &Layout, name: &str) -> Option<String> {
    let bytes = fs::read(layout.server_log(name)).ok()?;

    let start = bytes.len().saturating_sub(MAX_LOG_TAIL);
    let text = String::from_utf8_lossy(&bytes[start..]);

    Some(
        text.chars()
            .filter(|character| *character == '\n' || !character.is_control())
            .take(MAX_LOG_TAIL)
            .collect(),
    )
}

/// Run a read-only self-check (`runtime status`, `doctor --quick`) of the
/// installed binary. Returns success and a bounded combined output.
pub fn self_check(layout: &Layout, arguments: &[&str]) -> Result<(bool, String), PlatformError> {
    let mut command = Command::new(&layout.exe);

    command
        .arg("--config")
        .arg(&layout.config_path)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("WORKSPACE_SSO_CLIENT_SECRET")
        .env_remove("WORKSPACE_AUTH_KEY");

    let output = command
        .output()
        .map_err(|error| PlatformError::Process(error.to_string()))?;

    Ok((output.status.success(), bounded_output(output)))
}

fn bounded_output(output: Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();

    if !output.stderr.is_empty() {
        text.push('\n');
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }

    text.chars()
        .filter(|character| *character == '\n' || !character.is_control())
        .take(8 * 1024)
        .collect()
}

// ---------------------------------------------------------------------------
// Database snapshots
// ---------------------------------------------------------------------------

pub fn snapshot_database(data_dir: &Path, target: &Path) -> Result<(), PlatformError> {
    let source = storage::database_path(data_dir);

    if !source.exists() {
        return Err(PlatformError::Database(storage::StorageError::Io(
            std::io::Error::new(std::io::ErrorKind::NotFound, "database is missing"),
        )));
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    if target.exists() {
        fs::remove_file(target)?;
    }

    let connection = Connection::open_with_flags(&source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    connection.busy_timeout(Duration::from_secs(10))?;

    // `VACUUM INTO` writes a consistent single-file snapshot and never
    // modifies the source. If SQLite refuses on a read-only handle, fall back
    // to copying the database and its WAL/SHM side files.
    let result = connection.execute(
        "VACUUM INTO ?1",
        rusqlite::params![target.to_string_lossy().to_string()],
    );

    if result.is_err() {
        drop(connection);

        fs::copy(&source, target)?;

        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{suffix}", source.display()));

            if side.exists() {
                let copy = PathBuf::from(format!("{}{suffix}", target.display()));
                fs::copy(&side, &copy)?;
            }
        }
    }

    if !target.is_file() {
        return Err(PlatformError::Database(storage::StorageError::Io(
            std::io::Error::new(std::io::ErrorKind::NotFound, "snapshot is missing"),
        )));
    }

    Ok(())
}

pub fn restore_database(layout: &Layout, snapshot: &Path) -> Result<(), PlatformError> {
    let target = storage::database_path(&layout.data_dir);

    if !snapshot.is_file() {
        return Err(PlatformError::Layout(format!(
            "database snapshot {} is missing",
            snapshot.display()
        )));
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", target.display()));

        if path.exists() {
            fs::remove_file(path)?;
        }
    }

    fs::copy(snapshot, &target)?;

    // Prove the restored file is a valid Workspace database before the server
    // is allowed to use it. `inspect` is read-only on purpose: it tolerates an
    // older schema, which the restored (previous) binary will migrate itself.
    storage::inspect(&layout.data_dir)?.ok_or_else(|| {
        PlatformError::Database(storage::StorageError::Integrity(
            "restored database cannot be opened".into(),
        ))
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Windows files
// ---------------------------------------------------------------------------

pub fn copy_with_hash(source: &Path, target: &Path) -> Result<String, PlatformError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::copy(source, target)?;

    Ok(sha256_file(target)?)
}

pub fn backup_windows_binary(
    layout: &Layout,
    backup_dir: &Path,
) -> Result<Option<String>, PlatformError> {
    if !layout.exe.is_file() {
        return Ok(None);
    }

    let target = backup_dir.join(
        layout
            .exe
            .file_name()
            .ok_or_else(|| PlatformError::Layout("executable has no name".into()))?,
    );

    Ok(Some(copy_with_hash(&layout.exe, &target)?))
}

pub fn install_windows_binary(
    layout: &Layout,
    staged: &Path,
    expected_sha256: &str,
) -> Result<(), PlatformError> {
    let actual = sha256_file(staged)?;

    if actual != expected_sha256 {
        return Err(PlatformError::Manifest(ManifestError::File {
            path: staged.display().to_string(),
            reason: "staged binary SHA-256 mismatch".into(),
        }));
    }

    let replacement = layout.install_dir.join("workspace.exe.new");
    let displaced = layout.install_dir.join("workspace.exe.replaced");

    fs::copy(staged, &replacement)?;

    if layout.exe.exists() {
        let _ = fs::remove_file(&displaced);
        fs::rename(&layout.exe, &displaced)?;
    }

    match fs::rename(&replacement, &layout.exe) {
        Ok(()) => {
            let _ = fs::remove_file(&displaced);
        }
        Err(error) => {
            // Put the previous binary back before reporting the failure.
            if displaced.exists() {
                let _ = fs::rename(&displaced, &layout.exe);
            }
            let _ = fs::remove_file(&replacement);
            return Err(PlatformError::Io(error));
        }
    }

    let installed = sha256_file(&layout.exe)?;

    if installed != expected_sha256 {
        return Err(PlatformError::Manifest(ManifestError::File {
            path: layout.exe.display().to_string(),
            reason: "installed binary SHA-256 mismatch".into(),
        }));
    }

    Ok(())
}

pub fn restore_windows_binary(layout: &Layout, backup_dir: &Path) -> Result<(), PlatformError> {
    let source = backup_dir.join(
        layout
            .exe
            .file_name()
            .ok_or_else(|| PlatformError::Layout("executable has no name".into()))?,
    );

    if !source.is_file() {
        return Err(PlatformError::Layout(format!(
            "backup binary {} is missing",
            source.display()
        )));
    }

    install_windows_binary(layout, &source, &sha256_file(&source)?)
}

/// Copy the packaged deployment scripts next to the installation, keeping the
/// previous versions in the backup directory.
pub fn install_deploy_scripts(
    layout: &Layout,
    package_root: &Path,
    deploy_files: &[(String, String)],
    backup_dir: &Path,
) -> Result<(), PlatformError> {
    if deploy_files.is_empty() {
        return Ok(());
    }

    fs::create_dir_all(layout.deploy_dir())?;

    for (path, name) in deploy_files {
        let source = package_root.join(path);

        if !source.is_file() {
            continue;
        }

        let target = layout.deploy_dir().join(name);

        if target.exists() {
            let previous = backup_dir.join("deploy").join(name);
            fs::create_dir_all(previous.parent().unwrap_or(backup_dir))?;
            fs::copy(&target, &previous)?;
        }

        fs::copy(&source, &target)?;
    }

    Ok(())
}

/// Restore deployment scripts that were replaced by a newer package.
pub fn restore_deploy_scripts(layout: &Layout, backup_dir: &Path) -> Result<(), PlatformError> {
    let directory = backup_dir.join("deploy");

    let Ok(entries) = fs::read_dir(&directory) else {
        return Ok(());
    };

    fs::create_dir_all(layout.deploy_dir())?;

    for entry in entries.flatten() {
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        if let Some(name) = path.file_name() {
            fs::copy(&path, layout.deploy_dir().join(name))?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// WSL runtime components
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentState {
    pub path: String,
    pub present: bool,
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WslBackup {
    pub distro: String,
    pub created_at_unix: i64,
    pub components: Vec<ComponentBackup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentBackup {
    pub role: String,
    pub state: ComponentState,
    #[serde(default)]
    pub file: Option<String>,
}

/// Programs the updater is allowed to execute inside the runtime.
const WSL_PROGRAMS: &[&str] = &[
    "/bin/true",
    "/usr/bin/cat",
    "/usr/bin/df",
    "/usr/bin/env",
    "/usr/bin/id",
    "/usr/bin/install",
    "/usr/bin/mkdir",
    "/usr/bin/rm",
    "/usr/bin/sha256sum",
    "/usr/bin/stat",
    "/usr/bin/tee",
    "/usr/bin/test",
    "/usr/sbin/visudo",
];

#[derive(Debug, Clone)]
pub struct Wsl {
    distro: String,
    exe: PathBuf,
}

impl Wsl {
    pub fn new(runtime: &RuntimePaths) -> Result<Self, PlatformError> {
        let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());

        Ok(Self {
            distro: runtime.distro.clone(),
            exe: PathBuf::from(system_root).join("System32").join("wsl.exe"),
        })
    }

    fn command(&self, user: &str, program: &str) -> Result<Command, PlatformError> {
        if !WSL_PROGRAMS.contains(&program) {
            return Err(PlatformError::Wsl(format!(
                "refusing to execute {program} in the runtime"
            )));
        }

        self.command_for(user, program)
    }

    /// Build a command for the trusted helper binary.
    ///
    /// The path comes from validated configuration and is checked again here;
    /// it is deliberately not part of the general program allowlist.
    fn helper_command(&self, user: &str, helper_path: &str) -> Result<Command, PlatformError> {
        if !helper_path.starts_with('/') || helper_path.contains("..") {
            return Err(PlatformError::Wsl(
                "helper path must be an absolute, closed path".into(),
            ));
        }

        self.command_for(user, helper_path)
    }

    fn command_for(&self, user: &str, program: &str) -> Result<Command, PlatformError> {
        if self.distro.is_empty() {
            return Err(PlatformError::Wsl("runtime distribution is empty".into()));
        }

        let home = if user == "root" {
            "/root".to_owned()
        } else {
            format!("/home/{user}")
        };

        let mut command = Command::new(&self.exe);

        command
            .arg("-d")
            .arg(&self.distro)
            .arg("-u")
            .arg(user)
            .arg("--cd")
            .arg("/")
            .arg("--exec")
            .arg("/usr/bin/env")
            .arg("-i")
            .arg("PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
            .arg(format!("HOME={home}"))
            .arg(program);

        Ok(command)
    }

    pub fn run(
        &self,
        user: &str,
        program: &str,
        arguments: &[String],
    ) -> Result<Output, PlatformError> {
        let mut command = self.command(user, program)?;

        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = command
            .output()
            .map_err(|error| PlatformError::Wsl(format!("{program}: {error}")))?;

        Ok(output)
    }

    pub fn run_text(
        &self,
        user: &str,
        program: &str,
        arguments: &[String],
    ) -> Result<String, PlatformError> {
        let output = self.run(user, program, arguments)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            return Err(PlatformError::Wsl(format!(
                "{program} exited with {}: {}",
                output.status.code().unwrap_or(-1),
                stderr.trim()
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    pub fn available(&self) -> Result<(), PlatformError> {
        if !self.exe.is_file() {
            return Err(PlatformError::Wsl("wsl.exe is not available".into()));
        }

        let distros = Command::new(&self.exe)
            .args(["-l", "-q"])
            .output()
            .map_err(|error| PlatformError::Wsl(error.to_string()))?;

        if !distros.status.success() {
            return Err(PlatformError::Wsl("cannot list WSL distributions".into()));
        }

        // `wsl -l -q` emits UTF-16LE on current Windows versions.
        let names = decode_wsl_names(&distros.stdout);

        if !names.iter().any(|name| name == &self.distro) {
            return Err(PlatformError::Wsl(format!(
                "WSL distribution {} is not installed",
                self.distro
            )));
        }

        self.run_text("root", "/bin/true", &[])?;

        Ok(())
    }

    pub fn free_bytes(&self, path: &str) -> Result<u64, PlatformError> {
        let text = self.run_text("root", "/usr/bin/df", &["-Pk".to_owned(), path.to_owned()])?;

        let line = text
            .lines()
            .nth(1)
            .ok_or_else(|| PlatformError::Wsl("df returned no data".into()))?;

        let available_kib: u64 = line
            .split_whitespace()
            .nth(3)
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| PlatformError::Wsl("cannot parse df output".into()))?;

        Ok(available_kib.saturating_mul(1024))
    }

    pub fn id(&self, user: &str) -> Result<(u32, u32), PlatformError> {
        let uid = self.run_text("root", "/usr/bin/id", &["-u".to_owned(), user.to_owned()])?;
        let gid = self.run_text("root", "/usr/bin/id", &["-g".to_owned(), user.to_owned()])?;

        let uid: u32 = uid
            .trim()
            .parse()
            .map_err(|_| PlatformError::Wsl(format!("cannot resolve the uid of {user}")))?;
        let gid: u32 = gid
            .trim()
            .parse()
            .map_err(|_| PlatformError::Wsl(format!("cannot resolve the gid of {user}")))?;

        Ok((uid, gid))
    }

    pub fn stat(&self, path: &str) -> Result<Option<ComponentState>, PlatformError> {
        let output = self.run(
            "root",
            "/usr/bin/stat",
            &["-c".into(), "%s %a %u %g".into(), path.to_owned()],
        )?;

        if !output.status.success() {
            return Ok(None);
        }

        let text = String::from_utf8_lossy(&output.stdout);
        let mut fields = text.split_whitespace();

        let size: u64 = fields
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let mode = u32::from_str_radix(fields.next().unwrap_or("0"), 8).unwrap_or(0);
        let uid: u32 = fields
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let gid: u32 = fields
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);

        let sha = self.run_text("root", "/usr/bin/sha256sum", &[path.to_owned()])?;
        let sha256 = sha
            .split_whitespace()
            .next()
            .filter(|value| value.len() == 64)
            .ok_or_else(|| PlatformError::Wsl("sha256sum returned no digest".into()))?
            .to_owned();

        Ok(Some(ComponentState {
            path: path.to_owned(),
            present: true,
            size,
            mode,
            uid,
            gid,
            sha256,
        }))
    }

    pub fn read(&self, path: &str) -> Result<Vec<u8>, PlatformError> {
        let output = self.run("root", "/usr/bin/cat", &[path.to_owned()])?;

        if !output.status.success() {
            return Err(PlatformError::Wsl(format!(
                "cannot read {path}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        Ok(output.stdout)
    }

    /// Upload bytes and install them atomically with explicit owner/mode.
    pub fn install_bytes(
        &self,
        destination: &str,
        bytes: &[u8],
        uid: u32,
        gid: u32,
        mode: u32,
    ) -> Result<(), PlatformError> {
        let temporary = format!(
            "/tmp/alxprgs-update-{}",
            crate::update::journal::random_token(8)
        );

        let mut command = self.command("root", "/usr/bin/tee")?;

        command
            .arg(&temporary)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|error| PlatformError::Wsl(format!("cannot upload: {error}")))?;

        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| PlatformError::Wsl("upload pipe is unavailable".into()))?;

            stdin.write_all(bytes)?;
            stdin.flush()?;
        }

        let status = child
            .wait()
            .map_err(|error| PlatformError::Wsl(error.to_string()))?;

        if !status.success() {
            return Err(PlatformError::Wsl("cannot upload the component".into()));
        }

        let arguments = vec![
            "-o".to_owned(),
            uid.to_string(),
            "-g".to_owned(),
            gid.to_string(),
            "-m".to_owned(),
            format!("{mode:04o}"),
            temporary.clone(),
            destination.to_owned(),
        ];

        let result = self.run("root", "/usr/bin/install", &arguments);

        let _ = self.run("root", "/usr/bin/rm", &["-f".to_owned(), temporary]);

        let output = result?;

        if !output.status.success() {
            return Err(PlatformError::Wsl(format!(
                "cannot install {destination}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        Ok(())
    }

    pub fn remove(&self, path: &str) -> Result<(), PlatformError> {
        let output = self.run("root", "/usr/bin/rm", &["-f".to_owned(), path.to_owned()])?;

        if !output.status.success() {
            return Err(PlatformError::Wsl(format!("cannot remove {path}")));
        }

        Ok(())
    }

    /// Framed helper handshake, proving the installed binary speaks the
    /// protocol the server expects.
    pub fn ping_helper(&self, runtime: &RuntimePaths, user: &str) -> Result<(), PlatformError> {
        use workspace_core::helper::{self, Operation, PROTOCOL_VERSION, Request};

        let request = Request {
            protocol: PROTOCOL_VERSION,
            request_id: crate::update::journal::random_token(8),
            body_len: 0,
            operation: Operation::Ping,
        };

        let json =
            serde_json::to_vec(&request).map_err(|error| PlatformError::Wsl(error.to_string()))?;

        let mut command = self.helper_command(user, &runtime.helper_path)?;

        command
            .arg("--allow-prefix")
            .arg(&runtime.data_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|error| PlatformError::Wsl(format!("cannot start the helper: {error}")))?;

        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| PlatformError::Wsl("helper stdin is unavailable".into()))?;

            let mut frame = Vec::with_capacity(4 + json.len());
            frame.extend_from_slice(&(json.len() as u32).to_be_bytes());
            frame.extend_from_slice(&json);

            stdin.write_all(&frame)?;
            stdin.flush()?;
        }

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| PlatformError::Wsl("helper stdout is unavailable".into()))?;

        let mut length = [0u8; 4];

        stdout
            .read_exact(&mut length)
            .map_err(|error| PlatformError::Wsl(format!("helper did not answer: {error}")))?;

        let header_len = u32::from_be_bytes(length);

        if header_len == 0 || header_len > helper::MAX_HEADER_BYTES {
            return Err(PlatformError::Wsl("invalid helper response".into()));
        }

        let mut header = vec![0u8; header_len as usize];

        stdout
            .read_exact(&mut header)
            .map_err(|error| PlatformError::Wsl(format!("truncated helper response: {error}")))?;

        let response: helper::Response = serde_json::from_slice(&header)
            .map_err(|error| PlatformError::Wsl(format!("invalid helper response: {error}")))?;

        let status = child
            .wait()
            .map_err(|error| PlatformError::Wsl(error.to_string()))?;

        if !status.success() {
            return Err(PlatformError::Wsl(format!(
                "helper exited with {}",
                status.code().unwrap_or(-1)
            )));
        }

        if response.protocol != PROTOCOL_VERSION {
            return Err(PlatformError::Wsl(
                "helper protocol version mismatch".into(),
            ));
        }

        if let Err(error) = response.result {
            return Err(PlatformError::Wsl(format!(
                "helper rejected the handshake: {}",
                error.message
            )));
        }

        Ok(())
    }
}

fn decode_wsl_names(bytes: &[u8]) -> Vec<String> {
    // Current Windows builds answer `wsl -l -q` in UTF-16LE. Older ones use
    // the active console code page; fall back to UTF-8 when the byte pattern
    // does not look like UTF-16.
    let pairs = bytes.as_chunks::<2>().0;
    let looks_utf16 = bytes.len().is_multiple_of(2) && pairs.iter().any(|pair| pair[1] == 0);

    let text = if looks_utf16 {
        let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();

        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };

    text.replace('\0', "")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "*")
        .map(str::to_owned)
        .collect()
}

fn component_specs(runtime: &RuntimePaths) -> [(&'static str, &str); 4] {
    [
        ("helper", &runtime.helper_path),
        ("egress", &runtime.egress_path),
        ("quota", &runtime.quota_path),
        ("sudoers", &runtime.sudoers_path),
    ]
}

pub fn backup_wsl(
    layout: &Layout,
    wsl: &Wsl,
    backup_dir: &Path,
) -> Result<WslBackup, PlatformError> {
    let runtime = layout
        .runtime
        .as_ref()
        .ok_or_else(|| PlatformError::Layout("runtime is not configured".into()))?;

    let directory = backup_dir.join("wsl");
    fs::create_dir_all(&directory)?;

    let mut components = Vec::new();

    for (role, path) in component_specs(runtime) {
        match wsl.stat(path)? {
            Some(state) => {
                let bytes = wsl.read(path)?;
                let file = directory.join(role);
                fs::write(&file, &bytes)?;

                let actual = sha256_hex(&bytes);

                if actual != state.sha256 {
                    return Err(PlatformError::Wsl(format!(
                        "{path} changed while it was being backed up"
                    )));
                }

                components.push(ComponentBackup {
                    role: role.to_owned(),
                    state,
                    file: Some(format!("wsl/{role}")),
                });
            }
            None => components.push(ComponentBackup {
                role: role.to_owned(),
                state: ComponentState {
                    path: path.to_owned(),
                    present: false,
                    size: 0,
                    mode: 0,
                    uid: 0,
                    gid: 0,
                    sha256: String::new(),
                },
                file: None,
            }),
        }
    }

    let backup = WslBackup {
        distro: runtime.distro.clone(),
        created_at_unix: unix_now(),
        components,
    };

    fs::write(
        directory.join("components.json"),
        serde_json::to_vec_pretty(&backup)
            .map_err(|error| PlatformError::Layout(error.to_string()))?,
    )?;

    Ok(backup)
}

/// Install the runtime components from the package.
///
/// `components` carries already verified bytes for the helper, egress proxy and
/// quota helper. The sudoers rule is generated from the validated runtime user
/// and quota path instead of shipping a copy that could drift.
pub fn install_wsl_components(
    layout: &Layout,
    wsl: &Wsl,
    components: &[(FileRoleRef, Vec<u8>)],
) -> Result<Vec<String>, PlatformError> {
    let runtime = layout
        .runtime
        .as_ref()
        .ok_or_else(|| PlatformError::Layout("runtime is not configured".into()))?;

    let (user_uid, user_gid) = wsl.id(&runtime.user)?;

    let mut installed = Vec::new();

    for (role, bytes) in components {
        let (destination, uid, gid, mode) = match role {
            FileRoleRef::Helper => (&runtime.helper_path, user_uid, user_gid, 0o755),
            FileRoleRef::Egress => (&runtime.egress_path, user_uid, user_gid, 0o755),
            FileRoleRef::Quota => (&runtime.quota_path, 0, 0, 0o755),
            FileRoleRef::Sudoers => continue,
        };

        wsl.install_bytes(destination, bytes, uid, gid, mode)?;

        installed.push(role.as_str().to_owned());
    }

    // The rule is generated from the validated user and quota path, so it can
    // never drift from the installed helper.
    let content = sudoers_content(&runtime.user, &runtime.quota_path);

    wsl.install_bytes(&runtime.sudoers_path, content.as_bytes(), 0, 0, 0o440)?;

    let check = wsl.run(
        "root",
        "/usr/sbin/visudo",
        &[
            "-c".to_owned(),
            "-f".to_owned(),
            runtime.sudoers_path.clone(),
        ],
    )?;

    if !check.status.success() {
        return Err(PlatformError::Wsl(format!(
            "the sudoers rule is invalid: {}",
            String::from_utf8_lossy(&check.stderr).trim()
        )));
    }

    installed.push("sudoers".to_owned());

    // The helper must speak the protocol after installation; this catches a
    // corrupted binary or a wrong architecture before the server is switched.
    wsl.ping_helper(runtime, &runtime.user)?;

    Ok(installed)
}

#[derive(Debug, Clone, Copy)]
pub enum FileRoleRef {
    Helper,
    Egress,
    Quota,
    Sudoers,
}

impl FileRoleRef {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Helper => "helper",
            Self::Egress => "egress",
            Self::Quota => "quota",
            Self::Sudoers => "sudoers",
        }
    }
}

pub fn sudoers_content(user: &str, quota_path: &str) -> String {
    format!(
        "Defaults:{user} !requiretty\n\
         {user} ALL=(root) NOPASSWD: {quota_path}\n"
    )
}

pub fn restore_wsl(layout: &Layout, wsl: &Wsl, backup_dir: &Path) -> Result<(), PlatformError> {
    let runtime = layout
        .runtime
        .as_ref()
        .ok_or_else(|| PlatformError::Layout("runtime is not configured".into()))?;

    let path = backup_dir.join("wsl").join("components.json");
    let bytes = fs::read(&path)?;
    let backup: WslBackup = serde_json::from_slice(&bytes)
        .map_err(|error| PlatformError::Layout(format!("invalid WSL backup: {error}")))?;

    for component in &backup.components {
        if !component.state.present {
            if matches!(
                component.role.as_str(),
                "helper" | "egress" | "quota" | "sudoers"
            ) {
                wsl.remove(&component.state.path)?;
            }
            continue;
        }

        let file = component
            .file
            .as_ref()
            .ok_or_else(|| PlatformError::Layout("backup file is missing".into()))?;

        let data = fs::read(backup_dir.join(file))?;

        let actual = sha256_hex(&data);

        if actual != component.state.sha256 {
            return Err(PlatformError::Wsl(format!(
                "backup of {} is damaged",
                component.state.path
            )));
        }

        wsl.install_bytes(
            &component.state.path,
            &data,
            component.state.uid,
            component.state.gid,
            component.state.mode,
        )?;

        if component.role == "sudoers" {
            let check = wsl.run(
                "root",
                "/usr/sbin/visudo",
                &[
                    "-c".to_owned(),
                    "-f".to_owned(),
                    component.state.path.clone(),
                ],
            )?;

            if !check.status.success() {
                return Err(PlatformError::Wsl(
                    "the restored sudoers rule is invalid".into(),
                ));
            }
        }
    }

    if backup
        .components
        .iter()
        .any(|component| component.role == "helper" && component.state.present)
    {
        wsl.ping_helper(runtime, &runtime.user)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Backup metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupManifest {
    pub format: u32,
    pub id: String,
    pub created_at_unix: i64,
    pub action: String,
    pub from_version: String,
    #[serde(default)]
    pub from_git_sha: String,
    pub to_version: String,
    pub from_schema: u32,
    pub to_schema: u32,
    pub install_dir: String,
    pub server_was_running: bool,
    pub windows_binary_sha256: Option<String>,
    pub database_sha256: String,
    pub database_size: u64,
    pub wsl: Option<WslBackup>,
    pub notes: Vec<String>,
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), PlatformError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| PlatformError::Layout(error.to_string()))?;

    fs::write(path, bytes)?;

    Ok(())
}

pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, PlatformError> {
    let bytes = fs::read(path)?;

    serde_json::from_slice(&bytes).map_err(|error| PlatformError::Layout(error.to_string()))
}

/// Result of the post-start verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    pub healthy: bool,
    pub version: String,
    pub build_sha: String,
    pub schema_version: u32,
    pub runtime_ready: bool,
    pub doctor_ok: bool,
    pub detail: String,
}

/// Build metadata reported by an installed binary.
#[derive(Debug, Clone)]
pub struct BinaryProbe {
    pub version: String,
    pub build_sha: String,
    /// False for binaries that predate the updater: they print only
    /// `workspace <version>` and their `/health` has no version fields.
    pub modern: bool,
}

/// Ask an on-disk binary for its version without trusting its configuration.
pub fn probe_binary(exe: &Path) -> Option<BinaryProbe> {
    if !exe.is_file() || std::env::var_os("WORKSPACE_VERSION_PROBE").is_some() {
        return None;
    }

    let output = Command::new(exe)
        .arg("--version")
        .env("WORKSPACE_VERSION_PROBE", "1")
        .stdin(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut version = None;
    let mut build_sha = String::new();
    let mut modern = false;

    for line in text.lines() {
        let line = line.trim();

        if let Some(rest) = line
            .strip_prefix("ALXPRGS Workspace ")
            .or_else(|| line.strip_prefix("workspace "))
        {
            version = Some(rest.split_whitespace().next()?.to_owned());
        }

        if let Some(rest) = line.strip_prefix("build:") {
            modern = true;
            build_sha = rest
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned();
        }
    }

    Some(BinaryProbe {
        version: version?,
        build_sha,
        modern,
    })
}

/// Wait until the server answers with `status: ok`, without requiring fields a
/// legacy build does not expose.
pub async fn wait_healthy_status(
    layout: &Layout,
    timeout: Duration,
) -> Result<ServerHealth, PlatformError> {
    let deadline = Instant::now() + timeout;
    let mut last = String::from("no response");

    while Instant::now() < deadline {
        match health(layout).await {
            Ok(info) if info.status == "ok" => return Ok(info),
            Ok(info) => last = format!("health status {}", info.status),
            Err(error) => last = error.to_string(),
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    Err(PlatformError::Health(format!(
        "timed out after {}s: {last}",
        timeout.as_secs()
    )))
}

/// Verify a server that may be a pre-updater build.
///
/// Modern builds report version and build SHA over `/health`; legacy builds
/// only return `{"status":"ok"}`, so the version comes from the binary itself.
pub async fn wait_and_verify_previous(
    layout: &Layout,
    expected_version: &str,
    expected_sha: Option<&str>,
    timeout: Duration,
) -> Result<(), PlatformError> {
    let probe = probe_binary(&layout.exe);

    match probe.as_ref() {
        Some(probe) if !probe.modern => {
            wait_healthy_status(layout, timeout).await?;

            if probe.version != expected_version {
                return Err(PlatformError::Health(format!(
                    "the restored binary reports {}, expected {expected_version}",
                    probe.version
                )));
            }
        }
        _ => {
            wait_healthy(layout, expected_version, expected_sha, None, timeout).await?;
        }
    }

    storage::inspect(&layout.data_dir)?;

    Ok(())
}

pub async fn verify_installation(
    layout: &Layout,
    expected_version: &str,
    expected_sha: &str,
    expected_schema: Option<u32>,
    require_runtime_ready: bool,
) -> Result<Verification, PlatformError> {
    let health = health(layout).await?;

    let healthy = health.version == expected_version
        && expected_schema.is_none_or(|schema| health.schema_version == schema)
        && (expected_sha.is_empty() || health.build_sha == expected_sha);

    let mut runtime_ready = false;
    let mut doctor_ok = false;
    let mut details = Vec::new();

    if layout.runtime.is_some() && cfg!(windows) {
        match self_check(layout, &["runtime", "status"]) {
            Ok((ok, output)) => {
                runtime_ready = ok;
                details.push(format!(
                    "runtime status: {}",
                    if ok { "ready" } else { "not ready" }
                ));
                if !ok {
                    details.push(output);
                }
            }
            Err(error) => details.push(format!("runtime status failed: {error}")),
        }

        match self_check(layout, &["doctor", "--quick"]) {
            Ok((ok, _)) => {
                doctor_ok = ok;
                details.push(format!("doctor: {}", if ok { "ok" } else { "not ready" }));
            }
            Err(error) => details.push(format!("doctor failed: {error}")),
        }
    }

    if require_runtime_ready && !runtime_ready {
        return Err(PlatformError::Health(format!(
            "runtime was ready before the update but not after it: {}",
            details.join("; ")
        )));
    }

    let detail = if healthy {
        details.join("; ")
    } else {
        format!(
            "health reported version {} / schema {} / build {} (expected {expected_version} / {:?} / {expected_sha})",
            health.version, health.schema_version, health.build_sha, expected_schema
        )
    };

    Ok(Verification {
        healthy,
        version: health.version,
        build_sha: health.build_sha,
        schema_version: health.schema_version,
        runtime_ready,
        doctor_ok,
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wsl_names_in_utf16_and_utf8() {
        let utf16: Vec<u8> = "ALXPRGS-Workspace\r\nUbuntu\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();

        assert_eq!(
            decode_wsl_names(&utf16),
            vec!["ALXPRGS-Workspace", "Ubuntu"]
        );

        assert_eq!(
            decode_wsl_names(b"ALXPRGS-Workspace\nUbuntu\n"),
            vec!["ALXPRGS-Workspace", "Ubuntu"]
        );
    }

    #[test]
    fn sudoers_rule_is_closed_and_explicit() {
        let rule = sudoers_content("workspace", "/usr/sbin/alxprgs-quota");

        assert_eq!(
            rule,
            "Defaults:workspace !requiretty\nworkspace ALL=(root) NOPASSWD: /usr/sbin/alxprgs-quota\n"
        );
        assert!(!rule.contains("ALL=(ALL)"));
    }

    #[test]
    fn installed_info_falls_back_to_build_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let layout = Layout {
            install_dir: directory.path().to_path_buf(),
            exe: directory.path().join("workspace.exe"),
            config_path: directory.path().join("config.toml"),
            data_dir: directory.path().to_path_buf(),
            listen: "127.0.0.1:1".parse().unwrap(),
            update: UpdateConfig::default(),
            runtime: None,
        };

        let info = InstalledInfo::read(&layout).unwrap();

        assert!(!info.managed);
        assert_eq!(info.version, VERSION);
        assert_eq!(info.git_sha, GIT_SHA);
    }

    #[test]
    fn database_snapshot_is_restorable() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();

        let info = storage::initialize(source.path()).unwrap();
        let snapshot = target.path().join("snapshot.db");

        snapshot_database(source.path(), &snapshot).unwrap();

        let layout = Layout {
            install_dir: source.path().to_path_buf(),
            exe: source.path().join("workspace.exe"),
            config_path: source.path().join("config.toml"),
            data_dir: source.path().to_path_buf(),
            listen: "127.0.0.1:1".parse().unwrap(),
            update: UpdateConfig::default(),
            runtime: None,
        };

        restore_database(&layout, &snapshot).unwrap();

        let restored = storage::inspect(source.path()).unwrap().unwrap();

        assert_eq!(restored.instance_id, info.instance_id);
        assert_eq!(restored.schema_version, info.schema_version);
    }

    #[test]
    fn log_tail_never_returns_binary_noise() {
        let directory = tempfile::tempdir().unwrap();
        let layout = Layout {
            install_dir: directory.path().to_path_buf(),
            exe: directory.path().join("workspace.exe"),
            config_path: directory.path().join("config.toml"),
            data_dir: directory.path().to_path_buf(),
            listen: "127.0.0.1:1".parse().unwrap(),
            update: UpdateConfig::default(),
            runtime: None,
        };

        fs::create_dir_all(layout.logs_dir()).unwrap();
        fs::write(layout.server_log("server.err.log"), b"ok\n\x01bad").unwrap();

        let tail = log_tail(&layout, "server.err.log").unwrap();

        assert!(tail.contains("ok"));
        assert!(!tail.contains('\u{1}'));
    }
}
