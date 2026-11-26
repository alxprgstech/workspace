//! Durable update journal and the single-instance update lock.
//!
//! The journal lives on disk because the updater replaces and restarts the
//! server: the CLI, the admin UI and the next server start all read the same
//! file. Writes go through a temporary file so a crash never leaves a
//! half-written JSON document; the previous valid document is kept as `.prev`
//! and is used as a fallback.

use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const JOURNAL_FILE: &str = "journal.json";
pub const LOCK_FILE: &str = "update.lock";
pub const LOG_FILE: &str = "update.log";

const STALE_AFTER_SECONDS: i64 = 600;

/// A journal whose process is gone is reported as interrupted after this short
/// grace period, even though its heartbeat is still recent. The longer
/// `STALE_AFTER_SECONDS` window only applies to a live-but-silent process and
/// to lock reclamation.
const INTERRUPTED_AFTER_SECONDS: i64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateAction {
    Update,
    Rollback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStatus {
    Prepared,
    Running,
    Succeeded,
    Failed,
    RolledBack,
    RollbackFailed,
    Interrupted,
}

impl UpdateStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::RolledBack | Self::RollbackFailed
        )
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Prepared | Self::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Running,
    Completed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    pub status: StepStatus,
    pub at_unix: i64,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub format: u32,
    pub id: String,
    pub token: String,
    pub action: UpdateAction,
    pub status: UpdateStatus,
    pub stage: String,
    pub progress: u8,
    pub message: Option<String>,
    pub error: Option<String>,
    pub from_version: String,
    #[serde(default)]
    pub from_git_sha: String,
    pub to_version: String,
    pub from_schema: u32,
    pub to_schema: u32,
    pub started_at_unix: i64,
    pub updated_at_unix: i64,
    pub pid: u32,
    pub install_dir: String,
    pub package_dir: Option<String>,
    pub backup_dir: Option<String>,
    pub log_path: String,
    pub server_was_running: bool,
    pub steps: Vec<Step>,
    pub warnings: Vec<String>,
}

impl Journal {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        action: UpdateAction,
        install_dir: &Path,
        log_path: &Path,
        from_version: &str,
        from_git_sha: &str,
        to_version: &str,
        from_schema: u32,
        to_schema: u32,
        server_was_running: bool,
    ) -> Self {
        let now = unix_now();
        let stamp = now;
        let id = format!("{}-{stamp}", random_token(6));

        Self {
            format: 1,
            id,
            token: random_token(16),
            action,
            status: UpdateStatus::Prepared,
            stage: "prepared".into(),
            progress: 0,
            message: None,
            error: None,
            from_version: from_version.into(),
            from_git_sha: from_git_sha.into(),
            to_version: to_version.into(),
            from_schema,
            to_schema,
            started_at_unix: now,
            updated_at_unix: now,
            pid: std::process::id(),
            install_dir: install_dir.display().to_string(),
            package_dir: None,
            backup_dir: None,
            log_path: log_path.display().to_string(),
            server_was_running,
            steps: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn begin_step(&mut self, id: &str, progress: u8, detail: Option<String>) {
        self.status = UpdateStatus::Running;
        self.stage = id.to_owned();
        self.progress = progress;
        self.updated_at_unix = unix_now();
        self.steps.retain(|step| step.id != id);
        self.steps.push(Step {
            id: id.to_owned(),
            status: StepStatus::Running,
            at_unix: self.updated_at_unix,
            detail,
        });
    }

    pub fn complete_step(&mut self, id: &str, detail: Option<String>) {
        self.updated_at_unix = unix_now();

        if let Some(step) = self.steps.iter_mut().rev().find(|step| step.id == id) {
            step.status = StepStatus::Completed;
            step.detail = detail;
            step.at_unix = self.updated_at_unix;
        } else {
            self.steps.push(Step {
                id: id.to_owned(),
                status: StepStatus::Completed,
                at_unix: self.updated_at_unix,
                detail,
            });
        }
    }

    pub fn fail_step(&mut self, id: &str, detail: String) {
        self.updated_at_unix = unix_now();

        if let Some(step) = self.steps.iter_mut().rev().find(|step| step.id == id) {
            step.status = StepStatus::Failed;
            step.detail = Some(detail);
            step.at_unix = self.updated_at_unix;
        } else {
            self.steps.push(Step {
                id: id.to_owned(),
                status: StepStatus::Failed,
                at_unix: self.updated_at_unix,
                detail: Some(detail),
            });
        }
    }

    /// True when the journal claims activity but its process is gone and the
    /// heartbeat is stale. The status endpoint reports such a journal as
    /// interrupted instead of pretending an update is still running.
    pub fn is_stale(&self) -> bool {
        if !self.status.is_active() {
            return false;
        }

        let age = unix_now().saturating_sub(self.updated_at_unix);

        if workspace_os::process::is_alive(self.pid) {
            // A live process that stopped writing for ten minutes is stuck.
            return age > STALE_AFTER_SECONDS;
        }

        // The process is gone. Give the file a moment in case a new updater is
        // just taking over, then report the operation as interrupted.
        age > INTERRUPTED_AFTER_SECONDS
    }

    pub fn effective_status(&self) -> UpdateStatus {
        if self.is_stale() {
            UpdateStatus::Interrupted
        } else {
            self.status
        }
    }

    /// Stable label used by the CLI and the journal file itself.
    pub fn status_as_str(&self) -> &'static str {
        match self.effective_status() {
            UpdateStatus::Prepared => "prepared",
            UpdateStatus::Running => "running",
            UpdateStatus::Succeeded => "succeeded",
            UpdateStatus::Failed => "failed",
            UpdateStatus::RolledBack => "rolled_back",
            UpdateStatus::RollbackFailed => "rollback_failed",
            UpdateStatus::Interrupted => "interrupted",
        }
    }
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

pub fn random_token(bytes: usize) -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    let mut buffer = vec![0u8; bytes];

    if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut buffer).is_err() {
        // A predictable journal id is not a security boundary, but it must
        // still be unique. Mix in the clock and the process id.
        let seed = format!(
            "{}-{}-{:?}",
            unix_now(),
            std::process::id(),
            std::thread::current().id()
        )
        .into_bytes();

        for (index, byte) in buffer.iter_mut().enumerate() {
            *byte = seed[index % seed.len()].wrapping_add(index as u8);
        }
    }

    URL_SAFE_NO_PAD.encode(buffer)
}

/// Paths of all mutable update state for one installation.
#[derive(Debug, Clone)]
pub struct JournalPaths {
    pub directory: PathBuf,
}

impl JournalPaths {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            directory: state_dir.join("update"),
        }
    }

    pub fn journal(&self) -> PathBuf {
        self.directory.join(JOURNAL_FILE)
    }

    pub fn lock(&self) -> PathBuf {
        self.directory.join(LOCK_FILE)
    }

    pub fn log(&self) -> PathBuf {
        self.directory.join(LOG_FILE)
    }

    pub fn ensure(&self) -> io::Result<()> {
        fs::create_dir_all(&self.directory)
    }
}

/// Load the newest valid journal document.
pub fn load_journal(paths: &JournalPaths) -> Option<Journal> {
    for candidate in [paths.journal(), paths.journal().with_extension("json.prev")] {
        let Ok(bytes) = fs::read(&candidate) else {
            continue;
        };

        if let Ok(journal) = serde_json::from_slice::<Journal>(&bytes) {
            return Some(journal);
        }
    }

    None
}

pub fn save_journal(paths: &JournalPaths, journal: &Journal) -> io::Result<()> {
    paths.ensure()?;

    let target = paths.journal();
    let temporary = target.with_extension("json.tmp");
    let previous = target.with_extension("json.prev");

    let bytes =
        serde_json::to_vec_pretty(journal).map_err(|error| io::Error::other(error.to_string()))?;

    {
        let mut file = File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }

    if target.exists() {
        let _ = fs::remove_file(&previous);
        let _ = fs::rename(&target, &previous);
    }

    fs::rename(&temporary, &target).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })
}

/// Append a human-readable line to the update log. Callers must never pass
/// secret values here.
pub fn log(paths: &JournalPaths, message: &str) {
    let path = paths.log();

    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return;
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "[{}] {message}", unix_now());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LockDocument {
    token: String,
    pid: u32,
    action: UpdateAction,
    journal: String,
    started_at_unix: i64,
}

/// Exclusive update lock for one installation.
#[derive(Debug)]
pub struct UpdateLock {
    path: PathBuf,
    token: String,
}

impl UpdateLock {
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Release the lock, but only when this owner still holds it.
    pub fn release(self) {
        if let Ok(bytes) = fs::read(&self.path)
            && let Ok(document) = serde_json::from_slice::<LockDocument>(&bytes)
            && document.token == self.token
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Release the lock when this token still owns it. A stale owner never removes
/// somebody else's lock.
pub fn release_lock(paths: &JournalPaths, token: &str) {
    if let Ok(bytes) = fs::read(paths.lock())
        && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && value["token"].as_str() == Some(token)
    {
        let _ = fs::remove_file(paths.lock());
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another update is already running (pid {pid})")]
    Busy { pid: u32 },

    #[error("cannot create the update lock: {0}")]
    Io(#[from] io::Error),
}

pub fn acquire_lock(
    paths: &JournalPaths,
    token: &str,
    action: UpdateAction,
    journal: &Path,
) -> Result<UpdateLock, LockError> {
    paths.ensure()?;

    let path = paths.lock();

    for attempt in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let document = LockDocument {
                    token: token.to_owned(),
                    pid: std::process::id(),
                    action,
                    journal: journal.display().to_string(),
                    started_at_unix: unix_now(),
                };

                let bytes = serde_json::to_vec_pretty(&document)
                    .map_err(|error| io::Error::other(error.to_string()))?;

                file.write_all(&bytes)?;
                file.sync_all()?;

                return Ok(UpdateLock {
                    path,
                    token: token.to_owned(),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if let Ok(bytes) = fs::read(&path)
                    && let Ok(document) = serde_json::from_slice::<LockDocument>(&bytes)
                {
                    let stale = !workspace_os::process::is_alive(document.pid)
                        || unix_now().saturating_sub(document.started_at_unix)
                            > STALE_AFTER_SECONDS;

                    if stale {
                        let _ = fs::remove_file(&path);
                        continue;
                    }

                    return Err(LockError::Busy { pid: document.pid });
                }

                // An unreadable lock cannot be trusted: remove it once.
                if attempt == 0 {
                    let _ = fs::remove_file(&path);
                    continue;
                }

                return Err(LockError::Busy { pid: 0 });
            }
            Err(error) => return Err(LockError::Io(error)),
        }
    }

    Err(LockError::Busy { pid: 0 })
}

/// Hand the lock over to the detached updater process.
///
/// The launcher exits immediately after spawning the updater; without this
/// handover its dead pid would make the lock look stale and allow a second
/// concurrent update.
pub fn adopt_lock(
    paths: &JournalPaths,
    token: &str,
    action: UpdateAction,
    journal: &Path,
) -> Result<(), LockError> {
    let path = paths.lock();

    let bytes = fs::read(&path)?;
    let existing: LockDocument = serde_json::from_slice(&bytes)
        .map_err(|error| LockError::Io(io::Error::other(error.to_string())))?;

    if existing.token != token {
        return Err(LockError::Busy { pid: existing.pid });
    }

    let document = LockDocument {
        token: token.to_owned(),
        pid: std::process::id(),
        action,
        journal: journal.display().to_string(),
        started_at_unix: existing.started_at_unix,
    };

    let bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| LockError::Io(io::Error::other(error.to_string())))?;

    let temporary = path.with_extension("lock.tmp");
    fs::write(&temporary, &bytes)?;
    let _ = fs::remove_file(&path);
    fs::rename(&temporary, &path)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_roundtrip_and_previous_copy() {
        let directory = tempfile::tempdir().unwrap();
        let paths = JournalPaths::new(directory.path());

        let mut journal = Journal::new(
            UpdateAction::Update,
            Path::new("C:\\install"),
            &paths.log(),
            "0.1.0",
            "660023c",
            "0.2.0",
            9,
            11,
            true,
        );

        journal.begin_step("backup", 20, None);
        save_journal(&paths, &journal).unwrap();

        let loaded = load_journal(&paths).unwrap();
        assert_eq!(loaded.to_version, "0.2.0");
        assert_eq!(loaded.effective_status(), UpdateStatus::Running);

        journal.complete_step("backup", None);
        journal.status = UpdateStatus::Succeeded;
        save_journal(&paths, &journal).unwrap();

        assert_eq!(
            load_journal(&paths).unwrap().effective_status(),
            UpdateStatus::Succeeded
        );

        // The previous document is still readable if the newest one is damaged.
        fs::write(paths.journal(), b"{not json").unwrap();
        assert!(load_journal(&paths).is_some());
    }

    #[test]
    fn stale_journal_becomes_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let paths = JournalPaths::new(directory.path());

        let mut journal = Journal::new(
            UpdateAction::Update,
            Path::new("C:\\install"),
            &paths.log(),
            "0.1.0",
            "660023c",
            "0.2.0",
            9,
            11,
            true,
        );

        journal.status = UpdateStatus::Running;
        journal.pid = 0;
        journal.updated_at_unix = unix_now() - INTERRUPTED_AFTER_SECONDS - 10;

        assert!(journal.is_stale());
        assert_eq!(journal.effective_status(), UpdateStatus::Interrupted);
    }

    #[test]
    fn second_lock_is_refused_until_released() {
        let directory = tempfile::tempdir().unwrap();
        let paths = JournalPaths::new(directory.path());

        let lock = acquire_lock(&paths, "token-a", UpdateAction::Update, &paths.journal()).unwrap();

        assert!(matches!(
            acquire_lock(&paths, "token-b", UpdateAction::Update, &paths.journal()),
            Err(LockError::Busy { .. })
        ));

        lock.release();

        assert!(acquire_lock(&paths, "token-b", UpdateAction::Update, &paths.journal()).is_ok());
    }

    #[test]
    fn dead_owner_lock_is_reclaimed() {
        let directory = tempfile::tempdir().unwrap();
        let paths = JournalPaths::new(directory.path());
        paths.ensure().unwrap();

        let document = LockDocument {
            token: "dead".into(),
            pid: 0,
            action: UpdateAction::Update,
            journal: paths.journal().display().to_string(),
            started_at_unix: unix_now(),
        };

        fs::write(paths.lock(), serde_json::to_vec(&document).unwrap()).unwrap();

        assert!(acquire_lock(&paths, "new", UpdateAction::Update, &paths.journal()).is_ok());
    }
}
