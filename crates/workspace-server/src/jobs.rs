//! Job execution inside the dedicated runtime.
//!
//! A job is always a argv vector executed in one workspace container: there is
//! no shell interpolation, no host execution path and no caller-controlled
//! Docker option. Output is captured separately for stdout and stderr and
//! bounded, so a runaway command cannot fill the database or the disk.

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;
use uuid::Uuid;
use workspace_core::job::{self, Job, JobState};
use workspace_core::workspace::ExecutionMode;
use workspace_runtime::docker::{
    ContainerSpec, Docker, INSTANCE_LABEL, JOB_LABEL, KIND_LABEL, WORKSPACE_LABEL,
};
use workspace_runtime::status::RuntimeMonitor;
use workspace_runtime::{RuntimeSettings, container_name};

use crate::storage;
use crate::workspaces::{Manager, WorkspaceError};

/// Maximum bytes kept per stream in the database.
pub const MAX_LOG_BYTES: usize = 1024 * 1024;

/// Maximum number of jobs returned by one listing.
pub const MAX_LISTED_JOBS: usize = 200;

/// Job failures with stable codes.
#[derive(Debug, Error)]
pub enum JobError {
    #[error("{0}")]
    Invalid(&'static str),

    #[error("job not found")]
    NotFound,

    #[error("job belongs to another owner")]
    Forbidden,

    #[error("the workspace already has an active job")]
    WorkspaceBusy,

    #[error("the idempotency key was already used with different parameters")]
    IdempotencyConflict,

    #[error("the execution runtime is not ready: {0}")]
    RuntimeUnavailable(String),

    #[error("database operation failed: {0}")]
    Database(String),

    #[error("workspace error: {0}")]
    Workspace(#[from] WorkspaceError),
}

impl JobError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_request",
            Self::NotFound => "job_not_found",
            Self::Forbidden => "forbidden",
            Self::WorkspaceBusy => "workspace_busy",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::RuntimeUnavailable(_) => "runtime_not_ready",
            Self::Database(_) => "database_error",
            Self::Workspace(error) => error.code(),
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_) => 400,
            Self::NotFound => 404,
            Self::Forbidden => 403,
            Self::WorkspaceBusy | Self::IdempotencyConflict => 409,
            Self::RuntimeUnavailable(_) => 503,
            Self::Database(_) => 500,
            Self::Workspace(error) => error.status(),
        }
    }
}

/// Submission request.
#[derive(Debug, Clone, Deserialize)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// One page of captured output.
#[derive(Debug, Clone, Serialize)]
pub struct LogPage {
    pub job_id: String,
    pub stream: String,
    pub offset: u64,
    pub next_offset: u64,
    pub total_bytes: u64,
    pub truncated: bool,
    pub data: String,
}

#[derive(Debug, Clone)]
struct JobRecord {
    job: Job,
    owner_id: String,
}

/// Result of storing a submission.
#[derive(Debug, Clone)]
pub enum InsertOutcome {
    /// A new job row was written; the caller owns starting it.
    Created(Job),
    /// The idempotency key already identified this job; it must not restart.
    Existing(Job),
}

fn validate_idempotency_key(key: &str) -> Result<(), JobError> {
    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
        return Err(JobError::Invalid("invalid idempotency key"));
    }

    Ok(())
}

/// Look up the earlier submission for this key and prove it is the same request.
fn existing_submission(
    connection: &Connection,
    owner: &str,
    workspace_id: &str,
    key: &str,
    request: &ExecRequest,
    timeout_seconds: u64,
) -> Result<Option<String>, JobError> {
    let row: Option<(String, String, String, String, String, i64)> = connection
        .query_row(
            "SELECT id, owner_id, argv, cwd, env, timeout_seconds FROM jobs
             WHERE workspace_id = ?1 AND idempotency_key = ?2",
            params![workspace_id, key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()
        .map_err(|error| JobError::Database(error.to_string()))?;

    let Some((id, stored_owner, argv, cwd, env, stored_timeout)) = row else {
        return Ok(None);
    };

    if stored_owner != owner {
        return Err(JobError::Forbidden);
    }

    if !same_submission(&argv, &cwd, &env, stored_timeout, request, timeout_seconds)? {
        return Err(JobError::IdempotencyConflict);
    }

    Ok(Some(id))
}

fn same_submission(
    stored_argv: &str,
    stored_cwd: &str,
    stored_env: &str,
    stored_timeout: i64,
    request: &ExecRequest,
    timeout_seconds: u64,
) -> Result<bool, JobError> {
    let argv: Vec<String> =
        serde_json::from_str(stored_argv).map_err(|error| JobError::Database(error.to_string()))?;

    let env: Vec<(String, String)> =
        serde_json::from_str(stored_env).map_err(|error| JobError::Database(error.to_string()))?;

    Ok(argv == request.argv
        && stored_cwd == request.cwd
        && env == request.env
        && stored_timeout == timeout_seconds as i64)
}

fn record_in(connection: &Connection, id: &str) -> Result<Option<JobRecord>, JobError> {
    connection
        .query_row(
            "SELECT id, workspace_id, state, argv, cwd, env, exit_code,
                    created_at_unix, started_at_unix, finished_at_unix,
                    timeout_seconds, error, stdout_bytes, stderr_bytes,
                    stdout_truncated, stderr_truncated, owner_id, cancel_requested
             FROM jobs WHERE id = ?1",
            params![id],
            row_to_record,
        )
        .optional()
        .map_err(|error| JobError::Database(error.to_string()))
}

/// Durable job storage.
#[derive(Debug, Clone)]
pub struct JobStore {
    data_dir: std::path::PathBuf,
}

impl JobStore {
    pub fn new(data_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    fn connection(&self) -> Result<rusqlite::Connection, JobError> {
        storage::open(&self.data_dir).map_err(|error| JobError::Database(error.to_string()))
    }

    pub fn insert(
        &self,
        owner: &str,
        workspace_id: &str,
        request: &ExecRequest,
        timeout_seconds: u64,
    ) -> Result<InsertOutcome, JobError> {
        let mut connection = self.connection()?;

        // `IMMEDIATE` takes the write lock before the duplicate check, so the
        // check and the insert are one atomic decision even with concurrent
        // submissions of the same key.
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| JobError::Database(error.to_string()))?;

        if let Some(key) = request.idempotency_key.as_deref() {
            validate_idempotency_key(key)?;

            if let Some(existing) = existing_submission(
                &transaction,
                owner,
                workspace_id,
                key,
                request,
                timeout_seconds,
            )? {
                let job = record_in(&transaction, &existing)?
                    .map(|record| record.job)
                    .ok_or(JobError::NotFound)?;

                transaction
                    .commit()
                    .map_err(|error| JobError::Database(error.to_string()))?;

                return Ok(InsertOutcome::Existing(job));
            }
        }

        let id = Uuid::new_v4().to_string();

        let inserted = transaction
            .execute(
                "INSERT INTO jobs (
                    id, workspace_id, owner_id, state, argv, cwd, env,
                    created_at_unix, timeout_seconds, idempotency_key
                 )
                 SELECT ?1, ?2, ?3, 'queued', ?4, ?5, ?6, unixepoch(), ?7, ?8
                 WHERE EXISTS (
                     SELECT 1 FROM workspaces
                     WHERE id = ?2
                       AND state IN ('created', 'running', 'idle', 'stopped')
                 )
                 AND NOT EXISTS (
                     SELECT 1 FROM jobs
                     WHERE workspace_id = ?2 AND state IN ('queued', 'running')
                 )",
                params![
                    id,
                    workspace_id,
                    owner,
                    serde_json::to_string(&request.argv)
                        .map_err(|error| JobError::Database(error.to_string()))?,
                    request.cwd,
                    serde_json::to_string(&request.env)
                        .map_err(|error| JobError::Database(error.to_string()))?,
                    timeout_seconds as i64,
                    request.idempotency_key,
                ],
            )
            .map_err(|error| JobError::Database(error.to_string()))?;

        if inserted == 0 {
            // The guarded insert is the atomic decision point: classify why it
            // refused without ever starting anything.
            let state: Option<String> = transaction
                .query_row(
                    "SELECT state FROM workspaces WHERE id = ?1",
                    params![workspace_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| JobError::Database(error.to_string()))?;

            let active: i64 = transaction
                .query_row(
                    "SELECT count(*) FROM jobs
                     WHERE workspace_id = ?1 AND state IN ('queued', 'running')",
                    params![workspace_id],
                    |row| row.get(0),
                )
                .map_err(|error| JobError::Database(error.to_string()))?;

            return match state.as_deref() {
                None => Err(JobError::Workspace(WorkspaceError::NotFound)),
                Some("deleting") => Err(JobError::WorkspaceBusy),
                Some(_) if active > 0 => Err(JobError::WorkspaceBusy),
                Some(_) => Err(JobError::Invalid("workspace is not ready yet")),
            };
        }

        transaction
            .commit()
            .map_err(|error| JobError::Database(error.to_string()))?;

        self.record(&id)?
            .map(|record| InsertOutcome::Created(record.job))
            .ok_or(JobError::NotFound)
    }

    /// Return the job already stored for this idempotency key, if any.
    ///
    /// A key reused with different parameters is a client error, never a second
    /// execution.
    pub fn existing_submission(
        &self,
        owner: &str,
        workspace_id: &str,
        request: &ExecRequest,
        timeout_seconds: u64,
    ) -> Result<Option<Job>, JobError> {
        let Some(key) = request.idempotency_key.as_deref() else {
            return Ok(None);
        };

        validate_idempotency_key(key)?;

        let connection = self.connection()?;

        let Some(existing) = existing_submission(
            &connection,
            owner,
            workspace_id,
            key,
            request,
            timeout_seconds,
        )?
        else {
            return Ok(None);
        };

        record_in(&connection, &existing).map(|record| record.map(|record| record.job))
    }

    fn record(&self, id: &str) -> Result<Option<JobRecord>, JobError> {
        let connection = self.connection()?;

        record_in(&connection, id)
    }

    pub fn get(&self, id: &str) -> Result<Option<Job>, JobError> {
        Ok(self.record(id)?.map(|record| record.job))
    }

    pub fn owned(&self, owner: &str, id: &str) -> Result<Job, JobError> {
        let record = self.record(id)?.ok_or(JobError::NotFound)?;

        if record.owner_id != owner {
            return Err(JobError::Forbidden);
        }

        Ok(record.job)
    }

    pub fn list(&self, owner: &str, workspace_id: Option<&str>) -> Result<Vec<Job>, JobError> {
        let connection = self.connection()?;

        let mut statement = connection
            .prepare(
                "SELECT id, workspace_id, state, argv, cwd, env, exit_code,
                        created_at_unix, started_at_unix, finished_at_unix,
                        timeout_seconds, error, stdout_bytes, stderr_bytes,
                        stdout_truncated, stderr_truncated, owner_id, cancel_requested
                 FROM jobs
                 WHERE owner_id = ?1 AND (?2 IS NULL OR workspace_id = ?2)
                 ORDER BY created_at_unix DESC LIMIT ?3",
            )
            .map_err(|error| JobError::Database(error.to_string()))?;

        let rows = statement
            .query_map(
                params![owner, workspace_id, MAX_LISTED_JOBS as i64],
                row_to_record,
            )
            .map_err(|error| JobError::Database(error.to_string()))?;

        let mut jobs = Vec::new();

        for row in rows {
            let record = row.map_err(|error| JobError::Database(error.to_string()))?;

            jobs.push(record.job);
        }

        Ok(jobs)
    }

    fn set_running(&self, id: &str) -> Result<(), JobError> {
        let connection = self.connection()?;

        connection
            .execute(
                "UPDATE jobs SET state = 'running', started_at_unix = unixepoch()
                 WHERE id = ?1 AND state = 'queued'",
                params![id],
            )
            .map_err(|error| JobError::Database(error.to_string()))?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        id: &str,
        state: JobState,
        exit_code: Option<i64>,
        error: Option<&str>,
        stdout: &[u8],
        stderr: &[u8],
        stdout_truncated: bool,
        stderr_truncated: bool,
    ) -> Result<(), JobError> {
        let connection = self.connection()?;

        connection
            .execute(
                "UPDATE jobs SET
                    state = ?2, exit_code = ?3, error = ?4,
                    stdout = ?5, stderr = ?6,
                    stdout_bytes = ?7, stderr_bytes = ?8,
                    stdout_truncated = ?9, stderr_truncated = ?10,
                    finished_at_unix = unixepoch()
                 WHERE id = ?1",
                params![
                    id,
                    state.as_str(),
                    exit_code,
                    error,
                    stdout,
                    stderr,
                    stdout.len() as i64,
                    stderr.len() as i64,
                    i64::from(stdout_truncated),
                    i64::from(stderr_truncated),
                ],
            )
            .map_err(|error| JobError::Database(error.to_string()))?;

        Ok(())
    }

    fn request_cancel(&self, id: &str) -> Result<bool, JobError> {
        let connection = self.connection()?;

        let changed = connection
            .execute(
                "UPDATE jobs SET cancel_requested = 1
                 WHERE id = ?1 AND state IN ('queued', 'running')",
                params![id],
            )
            .map_err(|error| JobError::Database(error.to_string()))?;

        Ok(changed > 0)
    }

    fn cancel_requested(&self, id: &str) -> Result<bool, JobError> {
        let connection = self.connection()?;

        connection
            .query_row(
                "SELECT cancel_requested FROM jobs WHERE id = ?1",
                params![id],
                |row| row.get::<_, i64>(0),
            )
            .map(|value| value != 0)
            .map_err(|error| JobError::Database(error.to_string()))
    }

    /// Mark every unfinished job as interrupted. Used once at startup.
    pub fn interrupt_unfinished(&self) -> Result<usize, JobError> {
        let connection = self.connection()?;

        let changed = connection
            .execute(
                "UPDATE jobs SET state = 'interrupted',
                    error = 'the server restarted while the job was active',
                    finished_at_unix = unixepoch()
                 WHERE state IN ('queued', 'running')",
                [],
            )
            .map_err(|error| JobError::Database(error.to_string()))?;

        Ok(changed)
    }

    pub fn logs(
        &self,
        owner: &str,
        id: &str,
        stream: &str,
        offset: u64,
        limit: u64,
    ) -> Result<LogPage, JobError> {
        let connection = self.connection()?;

        let record = self.record(id)?.ok_or(JobError::NotFound)?;

        if record.owner_id != owner {
            return Err(JobError::Forbidden);
        }

        let column = match stream {
            "stdout" => "stdout",
            "stderr" => "stderr",
            _ => return Err(JobError::Invalid("stream must be stdout or stderr")),
        };

        let data: Vec<u8> = connection
            .query_row(
                &format!("SELECT {column} FROM jobs WHERE id = ?1"),
                params![id],
                |row| row.get(0),
            )
            .map_err(|error| JobError::Database(error.to_string()))?;

        let total = data.len() as u64;
        let start = offset.min(total) as usize;
        let limit = limit.clamp(1, MAX_LOG_BYTES as u64) as usize;
        let end = (start + limit).min(data.len());
        let chunk = &data[start..end];

        Ok(LogPage {
            job_id: id.to_owned(),
            stream: stream.to_owned(),
            offset,
            next_offset: end as u64,
            total_bytes: total,
            truncated: end < data.len(),
            data: String::from_utf8_lossy(chunk).into_owned(),
        })
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<JobRecord> {
    let state: String = row.get(2)?;
    let argv: String = row.get(3)?;

    Ok(JobRecord {
        job: Job {
            id: row.get(0)?,
            workspace_id: row.get(1)?,
            state: JobState::parse(&state).unwrap_or(JobState::Failed),
            argv: serde_json::from_str(&argv).unwrap_or_default(),
            cwd: row.get(4)?,
            exit_code: row.get(6)?,
            created_at_unix: row.get(7)?,
            started_at_unix: row.get(8)?,
            finished_at_unix: row.get(9)?,
            timeout_seconds: row.get::<_, i64>(10)? as u64,
            error: row.get(11)?,
            stdout_bytes: row.get::<_, i64>(12)? as u64,
            stderr_bytes: row.get::<_, i64>(13)? as u64,
            stdout_truncated: row.get::<_, i64>(14)? != 0,
            stderr_truncated: row.get::<_, i64>(15)? != 0,
        },
        owner_id: row.get(16)?,
    })
}

/// Runtime operations jobs need.
#[async_trait]
pub trait JobRuntime: Send + Sync {
    /// Execute a command and return its captured output.
    async fn execute(
        &self,
        workspace: &workspace_core::workspace::Workspace,
        spec: JobSpec,
    ) -> Result<ExecutionResult, String>;

    /// Whether an administrator configured allowlist egress.
    fn allowlist_configured(&self) -> bool {
        false
    }

    /// Remove a leftover job container.
    async fn cleanup(&self, workspace: &workspace_core::workspace::Workspace, container: &str);
}

/// Fully prepared job execution request.
#[derive(Debug, Clone)]
pub struct JobSpec {
    pub job_id: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
    pub cancel: Arc<AtomicBool>,
}

/// Captured execution result.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub exit_code: i64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub oom_killed: bool,
}

/// Executor configuration.
#[derive(Debug, Clone)]
pub struct ExecSettings {
    pub max_concurrent_jobs: u32,
    pub default_timeout_seconds: u64,
    pub max_timeout_seconds: u64,
}

/// Job service.
pub struct Executor {
    store: JobStore,
    manager: Arc<Manager>,
    runtime: Arc<dyn JobRuntime>,
    monitor: Arc<RuntimeMonitor>,
    settings: ExecSettings,
    permits: Arc<Semaphore>,
    cancellations: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    instance_id: String,
}

impl Executor {
    pub fn new(
        store: JobStore,
        manager: Arc<Manager>,
        runtime: Arc<dyn JobRuntime>,
        monitor: Arc<RuntimeMonitor>,
        settings: ExecSettings,
        instance_id: String,
    ) -> Self {
        let permits = Arc::new(Semaphore::new(settings.max_concurrent_jobs.max(1) as usize));

        Self {
            store,
            manager,
            runtime,
            monitor,
            settings,
            permits,
            cancellations: Arc::new(Mutex::new(HashMap::new())),
            instance_id,
        }
    }

    /// Start a job and return its record immediately.
    pub async fn submit(
        &self,
        owner: &str,
        workspace_id: &str,
        request: ExecRequest,
    ) -> Result<Job, JobError> {
        job::validate_argv(&request.argv).map_err(JobError::Invalid)?;
        job::validate_cwd(&request.cwd).map_err(JobError::Invalid)?;
        job::validate_env(&request.env).map_err(JobError::Invalid)?;

        let mut status = self.monitor.status().await;

        // A stopped WSL2 distribution or a restarted rootless engine is a
        // normal idle state, not an outage. Retry once under the startup
        // timeout before answering `runtime_not_ready`.
        if !status.execution_ready && self.monitor.recover().await {
            status = self.monitor.refresh().await;
        }

        if !status.execution_ready {
            return Err(JobError::RuntimeUnavailable(
                status
                    .blocking
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "runtime acceptance has not passed".to_owned()),
            ));
        }

        let workspace = self.manager.owned(owner, workspace_id)?;

        if workspace.state.is_transitional()
            || workspace.state == workspace_core::workspace::WorkspaceState::Failed
        {
            return Err(JobError::Invalid("workspace is not ready yet"));
        }

        if !matches!(workspace.network_mode.as_str(), "none" | "allowlist") {
            return Err(JobError::Invalid("unsupported workspace network mode"));
        }

        if workspace.network_mode == "allowlist" && !self.runtime.allowlist_configured() {
            return Err(JobError::Invalid("allowlist networking is not configured"));
        }

        let timeout = request
            .timeout_seconds
            .unwrap_or(workspace.limits.max_job_seconds)
            .clamp(
                1,
                workspace
                    .limits
                    .max_job_seconds
                    .min(self.settings.max_timeout_seconds),
            );

        // A retry of an accepted submission returns the original job and never
        // starts it again. This is checked before the busy guard on purpose: a
        // retry of a running job must observe that job, not `workspace_busy`.
        if let Some(job) = self
            .store
            .existing_submission(owner, workspace_id, &request, timeout)?
        {
            return Ok(job);
        }

        let job = match self.store.insert(owner, workspace_id, &request, timeout)? {
            InsertOutcome::Existing(job) => return Ok(job),
            InsertOutcome::Created(job) => job,
        };

        // Queued jobs already count as activity, so the TTL follows the job.
        let _ = self.manager.touch(workspace_id);

        let cancel = Arc::new(AtomicBool::new(false));

        self.cancellations
            .lock()
            .unwrap()
            .insert(job.id.clone(), Arc::clone(&cancel));

        let executor = self.handle();
        let job_id = job.id.clone();
        let owner = owner.to_owned();

        tokio::spawn(async move {
            executor
                .run(&owner, workspace, job_id, request, timeout, cancel)
                .await;
        });

        Ok(job)
    }

    fn handle(&self) -> ExecutorHandle {
        ExecutorHandle {
            store: self.store.clone(),
            manager: Arc::clone(&self.manager),
            runtime: Arc::clone(&self.runtime),
            permits: Arc::clone(&self.permits),
            instance_id: self.instance_id.clone(),
        }
    }

    pub fn owned_job(&self, owner: &str, job_id: &str) -> Result<Job, JobError> {
        self.store.owned(owner, job_id)
    }

    pub fn list(&self, owner: &str, workspace_id: Option<&str>) -> Result<Vec<Job>, JobError> {
        self.store.list(owner, workspace_id)
    }

    pub fn logs(
        &self,
        owner: &str,
        job_id: &str,
        stream: &str,
        offset: u64,
        limit: u64,
    ) -> Result<LogPage, JobError> {
        self.store.logs(owner, job_id, stream, offset, limit)
    }

    /// Ask a queued or running job to stop.
    pub async fn cancel(&self, owner: &str, job_id: &str) -> Result<Job, JobError> {
        let job = self.store.owned(owner, job_id)?;

        if job.state.is_terminal() {
            return Ok(job);
        }

        if !self.store.request_cancel(job_id)? {
            return self.store.owned(owner, job_id);
        }

        if let Some(flag) = self.cancellations.lock().unwrap().get(job_id) {
            flag.store(true, Ordering::SeqCst);
        }

        if job.state == JobState::Queued {
            self.store.finish(
                job_id,
                JobState::Cancelled,
                None,
                Some("cancelled before start"),
                &[],
                &[],
                false,
                false,
            )?;
        }

        self.store.owned(owner, job_id)
    }

    /// Startup recovery: nothing from a previous process may keep running.
    pub fn recover(&self) -> Result<usize, JobError> {
        self.store.interrupt_unfinished()
    }
}

/// Cloneable runner used by the spawned task.
struct ExecutorHandle {
    store: JobStore,
    manager: Arc<Manager>,
    runtime: Arc<dyn JobRuntime>,
    permits: Arc<Semaphore>,
    instance_id: String,
}

impl ExecutorHandle {
    async fn run(
        &self,
        _owner: &str,
        workspace: workspace_core::workspace::Workspace,
        job_id: String,
        request: ExecRequest,
        timeout: u64,
        cancel: Arc<AtomicBool>,
    ) {
        let _permit = match self.permits.acquire().await {
            Ok(permit) => permit,
            Err(_) => return,
        };

        // A job cancelled while queued never starts.
        if cancel.load(Ordering::SeqCst) || self.store.cancel_requested(&job_id).unwrap_or(false) {
            let _ = self.store.finish(
                &job_id,
                JobState::Cancelled,
                None,
                Some("cancelled before start"),
                &[],
                &[],
                false,
                false,
            );

            return;
        }

        if let Err(error) = self.store.set_running(&job_id) {
            let _ = self.store.finish(
                &job_id,
                JobState::Failed,
                None,
                Some(&error.to_string()),
                &[],
                &[],
                false,
                false,
            );

            return;
        }

        let _ = self.manager.store().set_state(
            &workspace.id,
            workspace_core::workspace::WorkspaceState::Running,
            None,
        );

        let spec = JobSpec {
            job_id: job_id.clone(),
            argv: request.argv,
            cwd: request.cwd,
            env: request.env,
            timeout: Duration::from_secs(timeout.max(1)),
            cancel: Arc::clone(&cancel),
        };

        let result = self.runtime.execute(&workspace, spec).await;

        let _ = self.manager.store().set_state(
            &workspace.id,
            WorkspaceStateAfter::idle_or_created(&workspace),
            None,
        );

        let _ = self.manager.store().touch(&workspace.id, true);

        match result {
            Ok(result) => {
                let (state, error) = if result.cancelled {
                    (JobState::Cancelled, Some("cancelled by request".to_owned()))
                } else if result.timed_out {
                    (JobState::TimedOut, Some("time limit exceeded".to_owned()))
                } else if result.oom_killed {
                    (
                        JobState::Failed,
                        Some("the kernel killed the job out of memory".to_owned()),
                    )
                } else if result.exit_code == 0 {
                    (JobState::Succeeded, None)
                } else {
                    (
                        JobState::Failed,
                        Some(format!("exited with code {}", result.exit_code)),
                    )
                };

                let _ = self.store.finish(
                    &job_id,
                    state,
                    Some(result.exit_code),
                    error.as_deref(),
                    &result.stdout,
                    &result.stderr,
                    result.stdout_truncated,
                    result.stderr_truncated,
                );
            }
            Err(message) => {
                let _ = self.store.finish(
                    &job_id,
                    JobState::Failed,
                    None,
                    Some(&message),
                    &[],
                    &[],
                    false,
                    false,
                );
            }
        }

        if let Ok(container) = container_name(&self.instance_id, "job", &job_id) {
            self.runtime.cleanup(&workspace, &container).await;
        }
    }
}

/// State helper: persistent workspaces return to idle, ephemeral to created.
struct WorkspaceStateAfter;

impl WorkspaceStateAfter {
    fn idle_or_created(
        workspace: &workspace_core::workspace::Workspace,
    ) -> workspace_core::workspace::WorkspaceState {
        match workspace.execution_mode {
            ExecutionMode::Persistent => workspace_core::workspace::WorkspaceState::Idle,
            ExecutionMode::Ephemeral => workspace_core::workspace::WorkspaceState::Created,
        }
    }
}

/// Runtime that refuses execution because no dedicated runtime is configured.
pub struct UnavailableJobRuntime {
    reason: String,
}

impl UnavailableJobRuntime {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

#[async_trait]
impl JobRuntime for UnavailableJobRuntime {
    async fn execute(
        &self,
        _workspace: &workspace_core::workspace::Workspace,
        _spec: JobSpec,
    ) -> Result<ExecutionResult, String> {
        Err(self.reason.clone())
    }

    async fn cleanup(&self, _workspace: &workspace_core::workspace::Workspace, _container: &str) {}
}

/// Real Docker implementation of [`JobRuntime`].
pub struct DockerJobRuntime {
    docker: Docker,
    settings: RuntimeSettings,
    instance_id: String,
    allowlist: Vec<String>,
}

impl DockerJobRuntime {
    pub fn new(
        monitor: &RuntimeMonitor,
        instance_id: String,
        allowlist: Vec<String>,
    ) -> Result<Self, JobError> {
        let settings = monitor.settings().cloned().ok_or_else(|| {
            JobError::RuntimeUnavailable("execution runtime is not configured".to_owned())
        })?;

        let docker = monitor.docker().cloned().ok_or_else(|| {
            JobError::RuntimeUnavailable("docker client is unavailable".to_owned())
        })?;

        Ok(Self {
            docker,
            settings,
            instance_id,
            allowlist,
        })
    }
}

#[async_trait]
impl JobRuntime for DockerJobRuntime {
    fn allowlist_configured(&self) -> bool {
        !self.allowlist.is_empty()
    }

    async fn execute(
        &self,
        workspace: &workspace_core::workspace::Workspace,
        spec: JobSpec,
    ) -> Result<ExecutionResult, String> {
        let job_container = container_name(&self.instance_id, "job", &spec.job_id)
            .map_err(|error| error.to_string())?;

        let workspace_dir = format!("{}/workspaces/{}", self.settings.data_root, workspace.id);

        let allowlisted = !self.allowlist.is_empty() && workspace.network_mode == "allowlist";

        let network = if allowlisted {
            workspace_runtime::docker::Network::Named(
                workspace_runtime::workspace_network(&self.instance_id, &workspace.id)
                    .map_err(|error| error.to_string())?,
            )
        } else {
            workspace_runtime::docker::Network::None
        };

        let mut environment = default_environment(&spec.env);

        if allowlisted {
            let proxy = workspace_runtime::egress_proxy_name(&self.instance_id)
                .map_err(|error| error.to_string())?;

            let url = format!("http://{proxy}:3128");

            for (key, value) in [
                ("HTTP_PROXY", url.clone()),
                ("HTTPS_PROXY", url.clone()),
                ("http_proxy", url.clone()),
                ("https_proxy", url),
                ("NO_PROXY", String::new()),
            ] {
                environment.push((key.to_owned(), value));
            }
        }

        let container = ContainerSpec {
            name: job_container.clone(),
            image: workspace.image.clone(),
            labels: vec![
                (INSTANCE_LABEL.to_owned(), self.instance_id.clone()),
                (WORKSPACE_LABEL.to_owned(), workspace.id.clone()),
                (JOB_LABEL.to_owned(), spec.job_id.clone()),
                (KIND_LABEL.to_owned(), "job".to_owned()),
            ],
            workspace_dir: format!("{workspace_dir}/files"),
            cache_dir: Some(format!("{workspace_dir}/cache")),
            argv: spec.argv.clone(),
            env: environment,
            cpu_millis: workspace.limits.cpu_millis,
            memory_mib: workspace.limits.memory_mib,
            pids: workspace.limits.pids,
            tmpfs_mib: 64,
            network,
            extra_mounts: Vec::new(),
        };

        let workdir = if spec.cwd.is_empty() {
            "/workspace".to_owned()
        } else {
            format!("/workspace/{}", spec.cwd.trim_matches('/'))
        };

        let mut command = if workspace.execution_mode == ExecutionMode::Persistent {
            let container_name = match workspace.container_name.clone() {
                Some(name) => name,
                None => return Err("persistent container is not running".to_owned()),
            };

            self.docker
                .command_exec(
                    &container_name,
                    &spec.argv,
                    &workdir,
                    &default_environment(&spec.env),
                )
                .map_err(|error| error.to_string())?
        } else {
            let mut command = self
                .docker
                .command_run(&container)
                .map_err(|error| error.to_string())?;

            command.env("DOCKER_CLI_HINTS", "false");

            command
        };

        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = command.spawn().map_err(|error| error.to_string())?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let stdout_task = stdout.map(|reader| tokio::spawn(capture(reader, MAX_LOG_BYTES)));
        let stderr_task = stderr.map(|reader| tokio::spawn(capture(reader, MAX_LOG_BYTES)));

        let deadline = tokio::time::Instant::now() + spec.timeout;
        let mut timed_out = false;
        let mut cancelled = false;
        let status;

        loop {
            tokio::select! {
                result = child.wait() => {
                    status = result.map_err(|error| error.to_string())?;
                    break;
                }
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    let requested = spec.cancel.load(Ordering::SeqCst);

                    if requested && !cancelled {
                        cancelled = true;

                        if workspace.execution_mode == ExecutionMode::Persistent {
                            if let Some(name) = workspace.container_name.as_deref() {
                                let _ = self.docker.kill(name).await;
                            }
                        } else {
                            let _ = self.docker.kill(&job_container).await;
                        }
                    }

                    if !timed_out && tokio::time::Instant::now() >= deadline {
                        timed_out = true;

                        if workspace.execution_mode == ExecutionMode::Persistent {
                            if let Some(name) = workspace.container_name.as_deref() {
                                let _ = self.docker.kill(name).await;
                            }
                        } else {
                            let _ = self.docker.kill(&job_container).await;
                        }
                    }
                }
            }
        }

        let (stdout, stdout_truncated) = match stdout_task {
            Some(task) => task.await.unwrap_or_default(),
            None => (Vec::new(), false),
        };

        let (stderr, stderr_truncated) = match stderr_task {
            Some(task) => task.await.unwrap_or_default(),
            None => (Vec::new(), false),
        };

        let mut oom_killed = false;

        if workspace.execution_mode == ExecutionMode::Ephemeral {
            if let Ok(state) = self.docker.inspect(&job_container).await {
                oom_killed = state.oom_killed;
            }

            let _ = self.docker.remove(&job_container).await;
        }

        Ok(ExecutionResult {
            exit_code: status.code().map(i64::from).unwrap_or(-1),
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
            timed_out,
            cancelled,
            oom_killed,
        })
    }
    async fn cleanup(&self, workspace: &workspace_core::workspace::Workspace, container: &str) {
        if workspace.execution_mode == ExecutionMode::Ephemeral {
            let _ = self.docker.remove(container).await;
        }
    }
}
fn default_environment(extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut environment = vec![
        ("HOME".to_owned(), "/tmp".to_owned()),
        (
            "PATH".to_owned(),
            "/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin".to_owned(),
        ),
        ("CARGO_HOME".to_owned(), "/cache/cargo".to_owned()),
        ("CARGO_TARGET_DIR".to_owned(), "/cache/target".to_owned()),
        ("PIP_CACHE_DIR".to_owned(), "/cache/pip".to_owned()),
        ("RUSTUP_HOME".to_owned(), "/usr/local/rustup".to_owned()),
        // Pin the toolchain explicitly: the container root filesystem is
        // read-only, so rustup must never try to sync a channel from a
        // project toolchain file.
        (
            "RUSTUP_TOOLCHAIN".to_owned(),
            "1.100.0-x86_64-unknown-linux-gnu".to_owned(),
        ),
    ];

    environment.extend(extra.iter().cloned());

    environment
}

async fn capture(mut reader: impl tokio::io::AsyncRead + Unpin, cap: usize) -> (Vec<u8>, bool) {
    let mut stored = Vec::new();
    let mut total = 0usize;
    let mut chunk = vec![0u8; 16 * 1024];

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => {
                total += read;

                if stored.len() < cap {
                    let take = (cap - stored.len()).min(read);
                    stored.extend_from_slice(&chunk[..take]);
                }
            }
            Err(_) => break,
        }
    }

    let truncated = total > stored.len();

    (stored, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_environment_keeps_caches_inside_the_workspace() {
        let environment = default_environment(&[("MY_VAR".into(), "1".into())]);

        assert!(environment.contains(&("MY_VAR".to_owned(), "1".to_owned())));

        for (key, value) in &environment {
            if key.starts_with("CARGO") || key == "PIP_CACHE_DIR" {
                assert!(value.starts_with("/cache/"), "{key}");
            }
        }
    }

    #[tokio::test]
    async fn capture_bounds_output_and_reports_truncation() {
        let data = vec![b'x'; 100];

        let (stored, truncated) = capture(data.as_slice(), 40).await;

        assert_eq!(stored.len(), 40);
        assert!(truncated);

        let (stored, truncated) = capture(&b"small"[..], 40).await;

        assert_eq!(stored, b"small");
        assert!(!truncated);
    }

    fn test_store() -> (tempfile::TempDir, JobStore) {
        let dir = tempfile::tempdir().unwrap();

        crate::storage::initialize(dir.path()).unwrap();

        let connection =
            rusqlite::Connection::open(crate::storage::database_path(dir.path())).unwrap();

        connection
            .execute(
                "INSERT INTO users (id, issuer, subject, created_at_unix, updated_at_unix)
                 VALUES ('owner', 'https://issuer.example', 'subject', 0, 0)",
                [],
            )
            .unwrap();

        connection
            .execute(
                "INSERT INTO workspaces (
                    id, owner_id, name, execution_mode, state, capabilities, image,
                    network_mode, cpu_millis, memory_mib, pids, disk_mib, files,
                    max_job_seconds, project_id, created_at_unix, updated_at_unix,
                    last_activity_unix, ttl_seconds, expires_at_unix
                 ) VALUES (
                    'ws', 'owner', 'test', 'ephemeral', 'created', '[\"base\"]', 'image',
                    'none', 100, 128, 32, 10, 100, 60, 1020, 0, 0, 0, 60, 3600
                 )",
                [],
            )
            .unwrap();

        let store = JobStore::new(dir.path());

        (dir, store)
    }

    fn request(key: Option<&str>) -> ExecRequest {
        ExecRequest {
            argv: vec!["sh".to_owned(), "-c".to_owned(), "echo once".to_owned()],
            cwd: String::new(),
            env: Vec::new(),
            timeout_seconds: Some(30),
            idempotency_key: key.map(str::to_owned),
        }
    }

    #[test]
    fn a_repeated_key_returns_the_original_job_instead_of_a_new_one() {
        let (_dir, store) = test_store();
        let request = request(Some("key-1"));

        let job = match store.insert("owner", "ws", &request, 30).unwrap() {
            InsertOutcome::Created(job) => job,
            InsertOutcome::Existing(_) => panic!("first insert must create a job"),
        };

        match store.insert("owner", "ws", &request, 30).unwrap() {
            InsertOutcome::Created(job) => panic!("duplicate insert created {}", job.id),
            InsertOutcome::Existing(existing) => assert_eq!(existing.id, job.id),
        }

        let via_lookup = store
            .existing_submission("owner", "ws", &request, 30)
            .unwrap()
            .expect("the key must resolve to the stored job");

        assert_eq!(via_lookup.id, job.id);
    }

    #[test]
    fn a_repeated_key_with_different_parameters_is_a_conflict() {
        let (_dir, store) = test_store();

        let created = store
            .insert("owner", "ws", &request(Some("key-2")), 30)
            .unwrap();

        let mut changed = request(Some("key-2"));
        changed.argv = vec!["sh".to_owned(), "-c".to_owned(), "echo twice".to_owned()];

        assert!(matches!(
            store.insert("owner", "ws", &changed, 30),
            Err(JobError::IdempotencyConflict)
        ));

        assert!(matches!(
            store.existing_submission("owner", "ws", &changed, 30),
            Err(JobError::IdempotencyConflict)
        ));

        let job = match created {
            InsertOutcome::Created(job) => job,
            InsertOutcome::Existing(_) => panic!("expected a created job"),
        };

        let unchanged = store
            .existing_submission("owner", "ws", &request(Some("key-2")), 30)
            .unwrap()
            .expect("matching request must resolve");
        assert_eq!(unchanged.id, job.id);
    }

    #[test]
    fn a_different_owner_cannot_reuse_the_key() {
        let (_dir, store) = test_store();

        store
            .insert("owner", "ws", &request(Some("key-3")), 30)
            .unwrap();

        assert!(matches!(
            store.insert("intruder", "ws", &request(Some("key-3")), 30),
            Err(JobError::Forbidden)
        ));
    }

    #[test]
    fn concurrent_duplicates_create_exactly_one_job() {
        let (_dir, store) = test_store();

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let store = store.clone();

                std::thread::spawn(move || store.insert("owner", "ws", &request(Some("race")), 30))
            })
            .collect();

        let mut created = 0;
        let mut existing = 0;
        let mut ids = Vec::new();

        for handle in handles {
            match handle.join().expect("thread").expect("insert") {
                InsertOutcome::Created(job) => {
                    created += 1;
                    ids.push(job.id);
                }
                InsertOutcome::Existing(job) => {
                    existing += 1;
                    ids.push(job.id);
                }
            }
        }

        assert_eq!(created, 1, "exactly one submission may create a job");
        assert_eq!(existing, 3);
        assert!(ids.windows(2).all(|pair| pair[0] == pair[1]), "{ids:?}");
    }

    #[test]
    fn rejection_of_a_completed_job_does_not_create_another() {
        let (_dir, store) = test_store();

        let job = match store
            .insert("owner", "ws", &request(Some("done")), 30)
            .unwrap()
        {
            InsertOutcome::Created(job) => job,
            InsertOutcome::Existing(_) => panic!("expected a created job"),
        };

        store
            .finish(
                &job.id,
                JobState::Succeeded,
                Some(0),
                None,
                b"once",
                b"",
                false,
                false,
            )
            .unwrap();

        let replay = store
            .existing_submission("owner", "ws", &request(Some("done")), 30)
            .unwrap()
            .expect("a completed job must still answer its key");

        assert_eq!(replay.id, job.id);
        assert_eq!(replay.state, JobState::Succeeded);
    }
}
