//! Workspace lifecycle: persistence, validation and runtime orchestration.
//!
//! Ordering rule: the database row is written before the runtime is touched,
//! so an interrupted operation always leaves a record that reconciliation can
//! finish or clean up. Deleting a workspace only ever touches resources named by
//! its own row (its UUID directory and its project identifier).

use crate::storage;
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use uuid::Uuid;
use workspace_core::workspace::{ExecutionMode, Limits, Workspace, WorkspaceState, validate_name};

/// Maximum number of workspaces one owner may keep at once.
pub const MAX_WORKSPACES_PER_OWNER: u32 = 3;

/// Workspace-level failures with stable codes for HTTP and MCP.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("{0}")]
    Invalid(&'static str),

    #[error("workspace not found")]
    NotFound,

    #[error("workspace limit reached")]
    LimitReached,

    #[error("{0}")]
    Capability(String),

    #[error("the execution runtime is not ready: {0}")]
    RuntimeUnavailable(String),

    #[error("the workspace has active jobs")]
    Busy,

    #[error("storage operation failed: {0}")]
    Storage(String),

    #[error("database operation failed: {0}")]
    Database(String),

    #[error("workspace belongs to another owner")]
    Forbidden,
}

impl WorkspaceError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_request",
            Self::NotFound => "workspace_not_found",
            Self::LimitReached => "workspace_limit_reached",
            Self::Capability(_) => "capability_error",
            Self::RuntimeUnavailable(_) => "runtime_not_ready",
            Self::Busy => "workspace_busy",
            Self::Storage(_) => "storage_error",
            Self::Database(_) => "database_error",
            Self::Forbidden => "forbidden",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_) | Self::Capability(_) => 400,
            Self::NotFound => 404,
            Self::Forbidden => 403,
            Self::LimitReached | Self::Busy => 409,
            Self::RuntimeUnavailable(_) => 503,
            Self::Storage(_) | Self::Database(_) => 500,
        }
    }
}

/// Per-workspace quota numbers returned to clients.
#[derive(Debug, Clone, Default, Serialize)]
pub struct QuotaSnapshot {
    pub used_bytes: u64,
    pub used_files: u64,
    pub hard_bytes: u64,
    pub hard_files: u64,
}

/// Runtime operations the manager needs.
///
/// Implemented by the Docker bridge in production and by a fake in tests; the
/// manager itself never talks to Docker, so its rules are testable without a
/// daemon.
#[async_trait]
pub trait WorkspaceRuntime: Send + Sync {
    async fn prepare_storage(&self, workspace: &Workspace) -> Result<(), WorkspaceError>;
    async fn release_storage(&self, workspace: &Workspace) -> Result<(), WorkspaceError>;
    async fn start_container(&self, workspace: &Workspace) -> Result<String, WorkspaceError>;
    async fn stop_container(&self, workspace: &Workspace) -> Result<(), WorkspaceError>;
    async fn quota_usage(&self, workspace: &Workspace) -> Result<QuotaSnapshot, WorkspaceError>;

    /// Recreate runtime networking for an existing workspace after a restart.
    async fn ensure_network(&self, _workspace: &Workspace) -> Result<(), WorkspaceError> {
        Ok(())
    }
}

/// Instance-wide settings applied to every workspace.
#[derive(Debug, Clone)]
pub struct ManagerSettings {
    pub budget: Limits,
    pub default_ttl_seconds: u64,
    pub idle_timeout_seconds: u64,
    pub network_mode: String,
    pub base_image: String,
}

/// Creation request after transport-level parsing.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateRequest {
    pub name: String,
    #[serde(default)]
    pub execution_mode: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    #[serde(default)]
    pub cpu_millis: Option<u32>,
    #[serde(default)]
    pub memory_mib: Option<u64>,
    #[serde(default)]
    pub persistent: Option<bool>,
}

/// Mutable parameters of an existing workspace.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    #[serde(default)]
    pub pinned: Option<bool>,
    #[serde(default)]
    pub execution_mode: Option<String>,
}

/// Durable workspace records.
#[derive(Debug, Clone)]
pub struct Store {
    data_dir: PathBuf,
}

impl Store {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    fn connection(&self) -> Result<rusqlite::Connection, WorkspaceError> {
        storage::open(&self.data_dir).map_err(|error| WorkspaceError::Database(error.to_string()))
    }

    pub fn insert(&self, workspace: &Workspace) -> Result<(), WorkspaceError> {
        let connection = self.connection()?;

        insert_in(&connection, workspace)
    }

    /// Enforce the owner limit, allocate a project identifier and insert the
    /// record as one atomic decision.
    ///
    /// `IMMEDIATE` takes SQLite's write lock before the count, so two concurrent
    /// creations cannot both observe a free slot or the same project id.
    pub fn reserve(
        &self,
        workspace: &mut Workspace,
        owner_limit: u32,
    ) -> Result<(), WorkspaceError> {
        let mut connection = self.connection()?;

        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite)?;

        let owned = count_owned_in(&transaction, &workspace.owner_id)?;

        if owned >= owner_limit {
            return Err(WorkspaceError::LimitReached);
        }

        workspace.project_id = next_project_id_in(&transaction)?;

        insert_in(&transaction, workspace)?;

        transaction.commit().map_err(map_sqlite)
    }

    pub fn get(&self, id: &str) -> Result<Option<Workspace>, WorkspaceError> {
        let connection = self.connection()?;

        connection
            .query_row(
                "SELECT id, owner_id, name, execution_mode, state, capabilities, image,
                        network_mode, cpu_millis, memory_mib, pids, disk_mib, files,
                        max_job_seconds, project_id, container_name, created_at_unix,
                        updated_at_unix, last_activity_unix, ttl_seconds, expires_at_unix,
                        pinned, error
                 FROM workspaces WHERE id = ?1",
                params![id],
                row_to_workspace,
            )
            .optional()
            .map_err(map_sqlite)
    }

    pub fn list(&self, owner: &str) -> Result<Vec<Workspace>, WorkspaceError> {
        let connection = self.connection()?;

        let mut statement = connection
            .prepare(
                "SELECT id, owner_id, name, execution_mode, state, capabilities, image,
                        network_mode, cpu_millis, memory_mib, pids, disk_mib, files,
                        max_job_seconds, project_id, container_name, created_at_unix,
                        updated_at_unix, last_activity_unix, ttl_seconds, expires_at_unix,
                        pinned, error
                 FROM workspaces WHERE owner_id = ?1 ORDER BY created_at_unix DESC",
            )
            .map_err(map_sqlite)?;

        let rows = statement
            .query_map(params![owner], row_to_workspace)
            .map_err(map_sqlite)?;

        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)
    }

    pub fn list_all(&self) -> Result<Vec<Workspace>, WorkspaceError> {
        let connection = self.connection()?;

        let mut statement = connection
            .prepare(
                "SELECT id, owner_id, name, execution_mode, state, capabilities, image,
                        network_mode, cpu_millis, memory_mib, pids, disk_mib, files,
                        max_job_seconds, project_id, container_name, created_at_unix,
                        updated_at_unix, last_activity_unix, ttl_seconds, expires_at_unix,
                        pinned, error
                 FROM workspaces ORDER BY created_at_unix DESC",
            )
            .map_err(map_sqlite)?;

        let rows = statement
            .query_map([], row_to_workspace)
            .map_err(map_sqlite)?;

        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)
    }

    pub fn count_owned(&self, owner: &str) -> Result<u32, WorkspaceError> {
        let connection = self.connection()?;

        count_owned_in(&connection, owner)
    }

    /// Next free project identifier for this instance.
    pub fn next_project_id(&self) -> Result<u32, WorkspaceError> {
        let connection = self.connection()?;

        next_project_id_in(&connection)
    }

    pub fn set_state(
        &self,
        id: &str,
        state: WorkspaceState,
        error: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        let connection = self.connection()?;

        connection
            .execute(
                "UPDATE workspaces SET state = ?2, error = ?3, updated_at_unix = unixepoch()
                 WHERE id = ?1",
                params![id, state.as_str(), error],
            )
            .map_err(map_sqlite)?;

        Ok(())
    }

    pub fn set_image(&self, id: &str, image: &str) -> Result<(), WorkspaceError> {
        let connection = self.connection()?;

        connection
            .execute(
                "UPDATE workspaces SET image = ?2, updated_at_unix = unixepoch()
                 WHERE id = ?1",
                params![id, image],
            )
            .map_err(map_sqlite)?;

        Ok(())
    }

    pub fn set_container(&self, id: &str, container: Option<&str>) -> Result<(), WorkspaceError> {
        let connection = self.connection()?;

        connection
            .execute(
                "UPDATE workspaces SET container_name = ?2, updated_at_unix = unixepoch()
                 WHERE id = ?1",
                params![id, container],
            )
            .map_err(map_sqlite)?;

        Ok(())
    }

    pub fn touch(&self, id: &str, extend_ttl: bool) -> Result<(), WorkspaceError> {
        let connection = self.connection()?;

        connection
            .execute(
                "UPDATE workspaces
                 SET last_activity_unix = unixepoch(),
                     updated_at_unix = unixepoch(),
                     expires_at_unix = CASE WHEN ?2 THEN unixepoch() + ttl_seconds
                                            ELSE expires_at_unix END
                 WHERE id = ?1",
                params![id, i64::from(extend_ttl)],
            )
            .map_err(map_sqlite)?;

        Ok(())
    }

    pub fn update(
        &self,
        id: &str,
        name: Option<&str>,
        ttl_seconds: Option<u64>,
        pinned: Option<bool>,
        execution_mode: Option<ExecutionMode>,
    ) -> Result<(), WorkspaceError> {
        let connection = self.connection()?;

        connection
            .execute(
                "UPDATE workspaces SET
                    name = COALESCE(?2, name),
                    ttl_seconds = COALESCE(?3, ttl_seconds),
                    expires_at_unix = CASE WHEN ?3 IS NULL THEN expires_at_unix
                                           ELSE unixepoch() + ?3 END,
                    pinned = COALESCE(?4, pinned),
                    execution_mode = COALESCE(?5, execution_mode),
                    updated_at_unix = unixepoch()
                 WHERE id = ?1",
                params![
                    id,
                    name,
                    ttl_seconds.map(|value| value as i64),
                    pinned.map(i64::from),
                    execution_mode.map(ExecutionMode::as_str),
                ],
            )
            .map_err(map_sqlite)?;

        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<bool, WorkspaceError> {
        let connection = self.connection()?;

        let removed = connection
            .execute("DELETE FROM workspaces WHERE id = ?1", params![id])
            .map_err(map_sqlite)?;

        Ok(removed > 0)
    }

    pub fn has_active_jobs(&self, id: &str) -> Result<bool, WorkspaceError> {
        let connection = self.connection()?;

        let count: i64 = connection
            .query_row(
                "SELECT count(*) FROM jobs
                 WHERE workspace_id = ?1 AND state IN ('queued', 'running')",
                params![id],
                |row| row.get(0),
            )
            .map_err(map_sqlite)?;

        Ok(count > 0)
    }

    pub fn audit(
        &self,
        actor: &str,
        action: &str,
        target: Option<&str>,
        outcome: &str,
        detail: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        let connection = self.connection()?;

        connection
            .execute(
                "INSERT INTO audit_events (at_unix, actor, action, target, outcome, detail)
                 VALUES (unixepoch(), ?1, ?2, ?3, ?4, ?5)",
                params![actor, action, target, outcome, detail],
            )
            .map_err(map_sqlite)?;

        Ok(())
    }
}

fn map_sqlite(error: rusqlite::Error) -> WorkspaceError {
    WorkspaceError::Database(error.to_string())
}

fn count_owned_in(connection: &Connection, owner: &str) -> Result<u32, WorkspaceError> {
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM workspaces WHERE owner_id = ?1 AND state != 'deleting'",
            params![owner],
            |row| row.get(0),
        )
        .map_err(map_sqlite)?;

    Ok(count as u32)
}

fn next_project_id_in(connection: &Connection) -> Result<u32, WorkspaceError> {
    let maximum: Option<i64> = connection
        .query_row("SELECT max(project_id) FROM workspaces", [], |row| {
            row.get(0)
        })
        .map_err(map_sqlite)?;

    let next = match maximum {
        Some(value) => (value as u32).saturating_add(1),
        None => workspace_runtime::quota::FIRST_WORKSPACE_PROJECT_ID,
    };

    if next > workspace_runtime::quota::MAX_PROJECT_ID {
        return Err(WorkspaceError::Storage(
            "no project identifiers left on this instance".into(),
        ));
    }

    Ok(next)
}

fn insert_in(connection: &Connection, workspace: &Workspace) -> Result<(), WorkspaceError> {
    connection
        .execute(
            "INSERT INTO workspaces (
                id, owner_id, name, execution_mode, state, capabilities, image,
                network_mode, cpu_millis, memory_mib, pids, disk_mib, files,
                max_job_seconds, project_id, container_name, created_at_unix,
                updated_at_unix, last_activity_unix, ttl_seconds, expires_at_unix,
                pinned, error
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23
             )",
            params![
                workspace.id,
                workspace.owner_id,
                workspace.name,
                workspace.execution_mode.as_str(),
                workspace.state.as_str(),
                serde_json::to_string(&workspace.capabilities)
                    .map_err(|error| WorkspaceError::Database(error.to_string()))?,
                workspace.image,
                workspace.network_mode,
                workspace.limits.cpu_millis,
                workspace.limits.memory_mib as i64,
                workspace.limits.pids,
                workspace.limits.disk_mib as i64,
                workspace.limits.files as i64,
                workspace.limits.max_job_seconds as i64,
                workspace.project_id,
                workspace.container_name,
                workspace.created_at_unix,
                workspace.updated_at_unix,
                workspace.last_activity_unix,
                workspace.ttl_seconds as i64,
                workspace.expires_at_unix,
                i64::from(workspace.pinned),
                workspace.error,
            ],
        )
        .map_err(map_sqlite)?;

    Ok(())
}

fn row_to_workspace(row: &rusqlite::Row<'_>) -> rusqlite::Result<Workspace> {
    let capabilities: String = row.get(5)?;
    let mode: String = row.get(3)?;
    let state: String = row.get(4)?;

    Ok(Workspace {
        id: row.get(0)?,
        owner_id: row.get(1)?,
        name: row.get(2)?,
        execution_mode: ExecutionMode::parse(&mode).unwrap_or(ExecutionMode::Ephemeral),
        state: WorkspaceState::parse(&state).unwrap_or(WorkspaceState::Failed),
        capabilities: serde_json::from_str(&capabilities).unwrap_or_default(),
        image: row.get(6)?,
        network_mode: row.get(7)?,
        limits: Limits {
            cpu_millis: row.get(8)?,
            memory_mib: row.get::<_, i64>(9)? as u64,
            pids: row.get(10)?,
            disk_mib: row.get::<_, i64>(11)? as u64,
            files: row.get::<_, i64>(12)? as u64,
            max_job_seconds: row.get::<_, i64>(13)? as u64,
        },
        project_id: row.get(14)?,
        container_name: row.get(15)?,
        created_at_unix: row.get(16)?,
        updated_at_unix: row.get(17)?,
        last_activity_unix: row.get(18)?,
        ttl_seconds: row.get::<_, i64>(19)? as u64,
        expires_at_unix: row.get(20)?,
        pinned: row.get::<_, i64>(21)? != 0,
        error: row.get(22)?,
    })
}

/// Workspace manager.
pub struct Manager {
    store: Store,
    runtime: Arc<dyn WorkspaceRuntime>,
    settings: ManagerSettings,
    images: Option<Arc<crate::images::ImageBuilder>>,
}

impl Manager {
    pub fn new(
        store: Store,
        runtime: Arc<dyn WorkspaceRuntime>,
        settings: ManagerSettings,
    ) -> Self {
        Self {
            store,
            runtime,
            settings,
            images: None,
        }
    }

    /// Attach the trusted image builder.
    pub fn with_images(mut self, images: Arc<crate::images::ImageBuilder>) -> Self {
        self.images = Some(images);
        self
    }

    pub fn images(&self) -> Option<&Arc<crate::images::ImageBuilder>> {
        self.images.as_ref()
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn settings(&self) -> &ManagerSettings {
        &self.settings
    }

    /// Create a workspace for `owner`.
    /// Create and fully prepare a workspace.
    pub async fn create(
        &self,
        owner: &str,
        request: &CreateRequest,
    ) -> Result<Workspace, WorkspaceError> {
        let workspace = self.reserve(owner, request).await?;

        self.prepare(&workspace.id).await
    }

    /// Reserve a workspace record in the `preparing` state.
    ///
    /// Image preparation can take minutes, so transports reserve first, return
    /// the identifier and run [`Self::prepare`] in the background.
    pub async fn reserve(
        &self,
        owner: &str,
        request: &CreateRequest,
    ) -> Result<Workspace, WorkspaceError> {
        validate_name(&request.name).map_err(WorkspaceError::Invalid)?;

        let execution_mode = match request.execution_mode.as_deref() {
            Some(value) => ExecutionMode::parse(value)
                .ok_or(WorkspaceError::Invalid("unknown execution mode"))?,
            None if request.persistent == Some(true) => ExecutionMode::Persistent,
            None => ExecutionMode::Ephemeral,
        };

        let modules = workspace_core::capabilities::resolve(&request.capabilities)
            .map_err(|error| WorkspaceError::Capability(error.message()))?;

        let capabilities = workspace_core::capabilities::canonical_ids(&modules);

        // Requested values are clamped; the server decides the final limits.
        let requested = Limits {
            cpu_millis: request
                .cpu_millis
                .unwrap_or(self.settings.budget.cpu_millis / 2),
            memory_mib: request
                .memory_mib
                .unwrap_or(self.settings.budget.memory_mib / 2),
            pids: self.settings.budget.pids,
            disk_mib: self.settings.budget.disk_mib,
            files: self.settings.budget.files,
            max_job_seconds: self.settings.budget.max_job_seconds,
        };

        let limits = requested.clamp_to(&self.settings.budget);

        let ttl_seconds = request
            .ttl_seconds
            .unwrap_or(self.settings.default_ttl_seconds)
            .clamp(60, self.settings.default_ttl_seconds.max(60));

        let now = unix_now();
        let id = Uuid::new_v4().to_string();

        let mut workspace = Workspace {
            id: id.clone(),
            owner_id: owner.to_owned(),
            name: request.name.trim().to_owned(),
            execution_mode,
            state: WorkspaceState::Preparing,
            capabilities,
            image: self.settings.base_image.clone(),
            network_mode: self.settings.network_mode.clone(),
            limits,
            project_id: 0,
            container_name: None,
            created_at_unix: now,
            updated_at_unix: now,
            last_activity_unix: now,
            ttl_seconds,
            expires_at_unix: now + ttl_seconds as i64,
            pinned: false,
            error: None,
        };

        // Write-ahead: the record exists before any external resource. The
        // owner limit, the project identifier and the insert are one
        // transaction, so concurrent creations cannot overshoot either.
        self.store
            .reserve(&mut workspace, MAX_WORKSPACES_PER_OWNER)?;

        self.store
            .audit(owner, "workspace.reserve", Some(&id), "ok", None)?;

        self.get_owned(owner, &id).await
    }

    /// Prepare the image, storage and (for persistent workspaces) the container.
    pub async fn prepare(&self, id: &str) -> Result<Workspace, WorkspaceError> {
        let workspace = self.store.get(id)?.ok_or(WorkspaceError::NotFound)?;

        if workspace.state != WorkspaceState::Preparing {
            return Ok(workspace);
        }

        let image = match self.images.as_ref() {
            Some(builder) => match builder.ensure(&workspace.capabilities).await {
                Ok(image) => image.reference,
                Err(error) => {
                    self.fail_preparation(&workspace, &error.to_string())?;

                    return Err(WorkspaceError::Capability(error.to_string()));
                }
            },
            None => self.settings.base_image.clone(),
        };

        self.store.set_image(id, &image)?;

        let prepared = self.store.get(id)?.ok_or(WorkspaceError::NotFound)?;

        if let Err(error) = self.runtime.prepare_storage(&prepared).await {
            self.fail_preparation(&prepared, &error.to_string())?;

            return Err(error);
        }

        if prepared.execution_mode == ExecutionMode::Persistent {
            match self.runtime.start_container(&prepared).await {
                Ok(container) => {
                    self.store.set_container(id, Some(&container))?;
                    self.store.set_state(id, WorkspaceState::Idle, None)?;
                }
                Err(error) => {
                    self.fail_preparation(&prepared, &error.to_string())?;

                    return Err(error);
                }
            }
        } else {
            self.store.set_state(id, WorkspaceState::Created, None)?;
        }

        self.store.audit(
            &prepared.owner_id,
            "workspace.prepare",
            Some(id),
            "ok",
            None,
        )?;

        self.get_owned(&prepared.owner_id, id).await
    }

    fn fail_preparation(&self, workspace: &Workspace, error: &str) -> Result<(), WorkspaceError> {
        self.store
            .set_state(&workspace.id, WorkspaceState::Failed, Some(error))?;

        let _ = self.store.audit(
            &workspace.owner_id,
            "workspace.prepare",
            Some(&workspace.id),
            "failed",
            Some(error),
        );

        Ok(())
    }

    /// Load a workspace and enforce ownership.
    pub fn owned(&self, owner: &str, id: &str) -> Result<Workspace, WorkspaceError> {
        let workspace = self.store.get(id)?.ok_or(WorkspaceError::NotFound)?;

        if workspace.owner_id != owner {
            return Err(WorkspaceError::Forbidden);
        }

        Ok(workspace)
    }

    pub async fn get_owned(&self, owner: &str, id: &str) -> Result<Workspace, WorkspaceError> {
        let workspace = self.owned(owner, id)?;

        // Reading is not "significant activity": it must not extend the TTL.
        Ok(workspace)
    }

    pub fn list(&self, owner: &str) -> Result<Vec<Workspace>, WorkspaceError> {
        self.store.list(owner)
    }

    pub fn list_all(&self) -> Result<Vec<Workspace>, WorkspaceError> {
        self.store.list_all()
    }

    pub async fn usage(&self, workspace: &Workspace) -> Result<QuotaSnapshot, WorkspaceError> {
        self.runtime.quota_usage(workspace).await
    }

    /// Update mutable parameters. Changing the execution mode stops compute
    /// first and is refused while jobs are active.
    pub async fn update(
        &self,
        owner: &str,
        id: &str,
        request: &UpdateRequest,
    ) -> Result<Workspace, WorkspaceError> {
        let workspace = self.owned(owner, id)?;

        if let Some(name) = request.name.as_deref() {
            validate_name(name).map_err(WorkspaceError::Invalid)?;
        }

        let new_mode = match request.execution_mode.as_deref() {
            Some(value) => Some(
                ExecutionMode::parse(value)
                    .ok_or(WorkspaceError::Invalid("unknown execution mode"))?,
            ),
            None => None,
        };

        let mode_changed = new_mode.is_some_and(|mode| mode != workspace.execution_mode);

        if mode_changed {
            if self.store.has_active_jobs(id)? {
                return Err(WorkspaceError::Busy);
            }

            if workspace.state.holds_compute() {
                self.runtime.stop_container(&workspace).await?;
                self.store.set_container(id, None)?;
            }
        }

        if request.ttl_seconds.is_some_and(|ttl| ttl < 60) {
            return Err(WorkspaceError::Invalid("ttl must be at least 60 seconds"));
        }

        self.store.update(
            id,
            request.name.as_deref().map(str::trim),
            request.ttl_seconds,
            request.pinned,
            new_mode,
        )?;

        if mode_changed {
            let refreshed = self.store.get(id)?.ok_or(WorkspaceError::NotFound)?;

            if refreshed.execution_mode == ExecutionMode::Persistent
                && refreshed.state == WorkspaceState::Idle
            {
                self.store.set_state(id, WorkspaceState::Stopped, None)?;
            } else {
                self.store.set_state(id, WorkspaceState::Created, None)?;
            }
        }

        self.store
            .audit(owner, "workspace.update", Some(id), "ok", None)?;

        self.get_owned(owner, id).await
    }

    /// Stop compute while preserving data.
    pub async fn stop(&self, owner: &str, id: &str) -> Result<Workspace, WorkspaceError> {
        let workspace = self.owned(owner, id)?;

        if workspace.state.is_transitional() {
            return Err(WorkspaceError::Invalid(
                "workspace operation already in progress",
            ));
        }

        if self.store.has_active_jobs(id)? {
            return Err(WorkspaceError::Busy);
        }

        self.store.set_state(id, WorkspaceState::Stopping, None)?;

        if let Err(error) = self.runtime.stop_container(&workspace).await {
            let _ = self
                .store
                .set_state(id, WorkspaceState::Failed, Some(&error.to_string()));

            return Err(error);
        }

        self.store.set_container(id, None)?;

        let next = if workspace.execution_mode == ExecutionMode::Persistent {
            WorkspaceState::Stopped
        } else {
            WorkspaceState::Created
        };

        self.store.set_state(id, next, None)?;
        self.store
            .audit(owner, "workspace.stop", Some(id), "ok", None)?;

        self.get_owned(owner, id).await
    }

    /// Delete a workspace and every resource owned by its row.
    pub async fn delete(&self, owner: &str, id: &str) -> Result<(), WorkspaceError> {
        let workspace = self.owned(owner, id)?;

        if self.store.has_active_jobs(id)? {
            return Err(WorkspaceError::Busy);
        }

        self.store.set_state(id, WorkspaceState::Deleting, None)?;

        if let Err(error) = self.runtime.stop_container(&workspace).await {
            tracing::warn!(%error, workspace = id, "container cleanup failed during delete");
        }

        self.runtime.release_storage(&workspace).await?;

        self.store.delete(id)?;
        self.store
            .audit(owner, "workspace.delete", Some(id), "ok", None)?;

        Ok(())
    }

    /// Bring a stopped workspace back to a usable state.
    pub async fn start(&self, owner: &str, id: &str) -> Result<Workspace, WorkspaceError> {
        let workspace = self.owned(owner, id)?;

        if workspace.state.holds_compute() {
            return Ok(workspace);
        }

        if workspace.state.is_transitional() {
            return Err(WorkspaceError::Invalid(
                "workspace operation already in progress",
            ));
        }

        if workspace.execution_mode == ExecutionMode::Persistent {
            self.store.set_state(id, WorkspaceState::Starting, None)?;

            match self.runtime.start_container(&workspace).await {
                Ok(container) => {
                    self.store.set_container(id, Some(&container))?;
                    self.store.set_state(id, WorkspaceState::Idle, None)?;
                }
                Err(error) => {
                    let _ =
                        self.store
                            .set_state(id, WorkspaceState::Failed, Some(&error.to_string()));

                    return Err(error);
                }
            }
        } else {
            self.store.set_state(id, WorkspaceState::Created, None)?;
        }

        self.store.touch(id, false)?;

        self.get_owned(owner, id).await
    }

    /// Record a significant activity and optionally extend the TTL.
    pub fn touch(&self, id: &str) -> Result<(), WorkspaceError> {
        self.store.touch(id, true)
    }

    /// Repair per-workspace runtime networking after a restart.
    ///
    /// Workspaces whose policy needs an internal network have their network and
    /// egress proxy recreated. Failures are reported but never block startup:
    /// the workspace still fails closed (no network) until the next attempt.
    pub async fn ensure_networks(&self) -> Vec<(String, String)> {
        let mut failures = Vec::new();

        let workspaces = match self.store.list_all() {
            Ok(workspaces) => workspaces,
            Err(error) => return vec![("store".to_owned(), error.to_string())],
        };

        for workspace in workspaces {
            if workspace.network_mode != "allowlist"
                || matches!(
                    workspace.state,
                    WorkspaceState::Deleting | WorkspaceState::Expired
                )
            {
                continue;
            }

            if let Err(error) = self.runtime.ensure_network(&workspace).await {
                failures.push((workspace.id, error.to_string()));
            }
        }

        failures
    }

    /// Delete expired, unpinned workspaces and stop idle containers.
    ///
    /// Returns `(deleted, stopped)` counts.
    pub async fn reap(&self, now: i64) -> Result<(usize, usize), WorkspaceError> {
        let mut deleted = 0;
        let mut stopped = 0;

        for workspace in self.store.list_all()? {
            if workspace.state == WorkspaceState::Deleting {
                continue;
            }

            if workspace.is_expired_at(now) {
                if self.store.has_active_jobs(&workspace.id)? {
                    continue;
                }

                let _ = self.store.set_state(
                    &workspace.id,
                    WorkspaceState::Expired,
                    Some("time to live elapsed"),
                );

                match self.delete_expired(&workspace).await {
                    Ok(()) => deleted += 1,
                    Err(error) => {
                        tracing::warn!(%error, workspace = %workspace.id, "expired workspace cleanup failed");
                    }
                }

                continue;
            }

            let idle_for = now - workspace.last_activity_unix;
            let idle_limit = self.settings.idle_timeout_seconds as i64;

            if workspace.execution_mode == ExecutionMode::Persistent
                && workspace.state == WorkspaceState::Idle
                && idle_for >= idle_limit
            {
                if self.store.has_active_jobs(&workspace.id)? {
                    continue;
                }

                if self.runtime.stop_container(&workspace).await.is_ok() {
                    let _ = self.store.set_container(&workspace.id, None);
                    let _ = self
                        .store
                        .set_state(&workspace.id, WorkspaceState::Stopped, None);
                    stopped += 1;
                }
            }
        }

        Ok((deleted, stopped))
    }

    async fn delete_expired(&self, workspace: &Workspace) -> Result<(), WorkspaceError> {
        let _ = self.runtime.stop_container(workspace).await;

        self.runtime.release_storage(workspace).await?;
        self.store.delete(&workspace.id)?;

        let _ = self.store.audit(
            "system",
            "workspace.expire",
            Some(&workspace.id),
            "ok",
            Some("ttl elapsed"),
        );

        Ok(())
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

/// Directory of the workspace inside `runtime.data_root` (Linux path).
pub fn workspace_data_dir(data_root: &str, id: &str) -> String {
    format!("{data_root}/workspaces/{id}")
}

/// Path helper used by the Docker bridge and the file service.
pub fn files_path(data_root: &str, id: &str) -> String {
    format!("{}/files", workspace_data_dir(data_root, id))
}

/// Path of the persistent cache mount.
pub fn cache_path(data_root: &str, id: &str) -> String {
    format!("{}/cache", workspace_data_dir(data_root, id))
}

/// Validate a workspace identifier before it is turned into a path.
pub fn valid_id(id: &str) -> bool {
    Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == id)
}

/// Local directory of a workspace on the control-plane filesystem.
pub fn control_dir(data_dir: &Path, id: &str) -> PathBuf {
    data_dir.join("workspaces").join(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeRuntime {
        prepared: Mutex<Vec<String>>,
        released: Mutex<Vec<String>>,
        started: Mutex<Vec<String>>,
        stopped: Mutex<Vec<String>>,
        fail_prepare: Mutex<Option<String>>,
        fail_start: Mutex<Option<String>>,
        usage: Mutex<Option<QuotaSnapshot>>,
    }

    #[async_trait]
    impl WorkspaceRuntime for FakeRuntime {
        async fn prepare_storage(&self, workspace: &Workspace) -> Result<(), WorkspaceError> {
            if let Some(message) = self.fail_prepare.lock().unwrap().as_ref() {
                return Err(WorkspaceError::Storage(message.clone()));
            }

            self.prepared.lock().unwrap().push(workspace.id.clone());

            Ok(())
        }

        async fn release_storage(&self, workspace: &Workspace) -> Result<(), WorkspaceError> {
            self.released.lock().unwrap().push(workspace.id.clone());

            Ok(())
        }

        async fn start_container(&self, workspace: &Workspace) -> Result<String, WorkspaceError> {
            if let Some(message) = self.fail_start.lock().unwrap().as_ref() {
                return Err(WorkspaceError::RuntimeUnavailable(message.clone()));
            }

            self.started.lock().unwrap().push(workspace.id.clone());

            Ok(format!("container-{}", workspace.id))
        }

        async fn stop_container(&self, workspace: &Workspace) -> Result<(), WorkspaceError> {
            self.stopped.lock().unwrap().push(workspace.id.clone());

            Ok(())
        }

        async fn quota_usage(
            &self,
            _workspace: &Workspace,
        ) -> Result<QuotaSnapshot, WorkspaceError> {
            Ok(self.usage.lock().unwrap().clone().unwrap_or_default())
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        manager: Manager,
        runtime: Arc<FakeRuntime>,
        owner: String,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();

        crate::storage::initialize(directory.path()).unwrap();

        let connection = crate::storage::open(directory.path()).unwrap();
        connection
            .execute_batch(
                "INSERT INTO users (id, issuer, subject, role, is_enabled, created_at_unix, updated_at_unix)
                 VALUES ('owner-1', 'https://auth.example.com', 'subject-1', 'admin', 1, 1, 1);
                 INSERT INTO users (id, issuer, subject, role, is_enabled, created_at_unix, updated_at_unix)
                 VALUES ('owner-2', 'https://auth.example.com', 'subject-2', 'user', 1, 1, 1);",
            )
            .unwrap();
        drop(connection);

        let runtime = Arc::new(FakeRuntime::default());

        let settings = ManagerSettings {
            budget: Limits {
                cpu_millis: 2000,
                memory_mib: 4096,
                pids: 256,
                disk_mib: 8192,
                files: 200_000,
                max_job_seconds: 3600,
            },
            default_ttl_seconds: 86_400,
            idle_timeout_seconds: 300,
            network_mode: "none".into(),
            base_image: "debian@sha256:00".into(),
        };

        let manager = Manager::new(
            Store::new(directory.path()),
            Arc::clone(&runtime) as Arc<dyn WorkspaceRuntime>,
            settings,
        );

        Fixture {
            _directory: directory,
            manager,
            runtime,
            owner: "owner-1".to_owned(),
        }
    }

    fn request() -> CreateRequest {
        CreateRequest {
            name: "probe".into(),
            execution_mode: None,
            capabilities: vec![],
            ttl_seconds: None,
            cpu_millis: None,
            memory_mib: None,
            persistent: None,
        }
    }

    #[tokio::test]
    async fn creates_an_ephemeral_workspace_and_its_storage() {
        let fixture = fixture();

        let workspace = fixture
            .manager
            .create(&fixture.owner, &request())
            .await
            .unwrap();

        assert_eq!(workspace.state, WorkspaceState::Created);
        assert_eq!(workspace.execution_mode, ExecutionMode::Ephemeral);
        assert_eq!(workspace.capabilities, vec!["base".to_owned()]);
        assert_eq!(fixture.runtime.prepared.lock().unwrap().len(), 1);
        assert!(fixture.runtime.started.lock().unwrap().is_empty());
        assert!(fixture.manager.list(&fixture.owner).unwrap().len() == 1);
    }

    #[tokio::test]
    async fn persistent_workspaces_start_a_container() {
        let fixture = fixture();
        let mut request = request();
        request.execution_mode = Some("persistent".into());

        let workspace = fixture
            .manager
            .create(&fixture.owner, &request)
            .await
            .unwrap();

        assert_eq!(workspace.state, WorkspaceState::Idle);
        assert_eq!(fixture.runtime.started.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn storage_failure_marks_the_workspace_failed() {
        let fixture = fixture();
        *fixture.runtime.fail_prepare.lock().unwrap() = Some("no quota".into());

        assert!(
            fixture
                .manager
                .create(&fixture.owner, &request())
                .await
                .is_err()
        );

        let workspace = fixture.manager.list(&fixture.owner).unwrap().pop().unwrap();

        assert_eq!(workspace.state, WorkspaceState::Failed);
    }

    #[tokio::test]
    async fn owners_cannot_see_each_others_workspaces() {
        let fixture = fixture();
        let workspace = fixture
            .manager
            .create(&fixture.owner, &request())
            .await
            .unwrap();

        assert!(matches!(
            fixture.manager.owned("owner-2", &workspace.id),
            Err(WorkspaceError::Forbidden)
        ));
        assert!(fixture.manager.list("owner-2").unwrap().is_empty());
    }

    #[tokio::test]
    async fn workspace_limit_is_enforced() {
        let fixture = fixture();

        for index in 0..MAX_WORKSPACES_PER_OWNER {
            let mut request = request();
            request.name = format!("probe-{index}");
            fixture
                .manager
                .create(&fixture.owner, &request)
                .await
                .unwrap();
        }

        let mut extra = request();
        extra.name = "one-too-many".into();

        assert!(matches!(
            fixture.manager.create(&fixture.owner, &extra).await,
            Err(WorkspaceError::LimitReached)
        ));
    }

    #[tokio::test]
    async fn delete_removes_only_the_workspace_resources() {
        let fixture = fixture();

        let first = fixture
            .manager
            .create(&fixture.owner, &request())
            .await
            .unwrap();
        let second = fixture
            .manager
            .create(&fixture.owner, &request())
            .await
            .unwrap();

        fixture
            .manager
            .delete(&fixture.owner, &first.id)
            .await
            .unwrap();

        assert!(fixture.runtime.released.lock().unwrap().contains(&first.id));
        assert!(
            !fixture
                .runtime
                .released
                .lock()
                .unwrap()
                .contains(&second.id)
        );
        assert!(fixture.manager.owned(&fixture.owner, &first.id).is_err());
        assert!(fixture.manager.owned(&fixture.owner, &second.id).is_ok());
    }

    #[tokio::test]
    async fn stop_rejects_unknown_transition_and_keeps_state_consistent() {
        let fixture = fixture();
        let workspace = fixture
            .manager
            .create(&fixture.owner, &request())
            .await
            .unwrap();

        let stopped = fixture
            .manager
            .stop(&fixture.owner, &workspace.id)
            .await
            .unwrap();

        assert_eq!(stopped.state, WorkspaceState::Created);
        assert_eq!(fixture.runtime.stopped.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pinning_prevents_expiry_cleanup() {
        let fixture = fixture();
        let workspace = fixture
            .manager
            .create(&fixture.owner, &request())
            .await
            .unwrap();

        fixture
            .manager
            .update(
                &fixture.owner,
                &workspace.id,
                &UpdateRequest {
                    pinned: Some(true),
                    ..UpdateRequest::default()
                },
            )
            .await
            .unwrap();

        let pinned = fixture
            .manager
            .owned(&fixture.owner, &workspace.id)
            .unwrap();

        assert!(pinned.pinned);
        assert!(!pinned.is_expired_at(i64::MAX / 2));
    }

    #[tokio::test]
    async fn mode_change_stops_the_container_first() {
        let fixture = fixture();
        let mut create = request();
        create.execution_mode = Some("persistent".into());

        let workspace = fixture
            .manager
            .create(&fixture.owner, &create)
            .await
            .unwrap();

        let updated = fixture
            .manager
            .update(
                &fixture.owner,
                &workspace.id,
                &UpdateRequest {
                    execution_mode: Some("ephemeral".into()),
                    ..UpdateRequest::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.execution_mode, ExecutionMode::Ephemeral);
        assert_eq!(updated.state, WorkspaceState::Created);
        assert_eq!(fixture.runtime.stopped.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn ttl_expiry_deletes_unpinned_workspaces() {
        let fixture = fixture();
        let workspace = fixture
            .manager
            .create(&fixture.owner, &request())
            .await
            .unwrap();

        let (deleted, _) = fixture.manager.reap(i64::MAX / 2).await.unwrap();

        assert_eq!(deleted, 1);
        assert!(
            fixture
                .manager
                .owned(&fixture.owner, &workspace.id)
                .is_err()
        );
    }

    #[test]
    fn identifiers_are_validated_before_becoming_paths() {
        assert!(valid_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!valid_id("../etc"));
        assert!(!valid_id("550E8400-E29B-41D4-A716-446655440000"));
        assert_eq!(
            files_path("/data", "550e8400-e29b-41d4-a716-446655440000"),
            "/data/workspaces/550e8400-e29b-41d4-a716-446655440000/files"
        );
    }

    fn store_fixture() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();

        crate::storage::initialize(directory.path()).unwrap();

        let connection = crate::storage::open(directory.path()).unwrap();
        connection
            .execute_batch(
                "INSERT INTO users (id, issuer, subject, role, is_enabled, created_at_unix, updated_at_unix)
                 VALUES ('owner-1', 'https://auth.example.com', 'subject-1', 'admin', 1, 1, 1);
                 INSERT INTO users (id, issuer, subject, role, is_enabled, created_at_unix, updated_at_unix)
                 VALUES ('owner-2', 'https://auth.example.com', 'subject-2', 'user', 1, 1, 1);",
            )
            .unwrap();
        drop(connection);

        let store = Store::new(directory.path());

        (directory, store)
    }

    fn reservation(owner: &str) -> Workspace {
        Workspace {
            id: Uuid::new_v4().to_string(),
            owner_id: owner.to_owned(),
            name: "probe".into(),
            execution_mode: ExecutionMode::Ephemeral,
            state: WorkspaceState::Preparing,
            capabilities: vec!["base".into()],
            image: "debian@sha256:00".into(),
            network_mode: "none".into(),
            limits: Limits {
                cpu_millis: 1000,
                memory_mib: 512,
                pids: 64,
                disk_mib: 1024,
                files: 1000,
                max_job_seconds: 60,
            },
            project_id: 0,
            container_name: None,
            created_at_unix: 0,
            updated_at_unix: 0,
            last_activity_unix: 0,
            ttl_seconds: 3600,
            expires_at_unix: 3600,
            pinned: false,
            error: None,
        }
    }

    #[test]
    fn concurrent_reservations_never_exceed_the_owner_limit() {
        let (_directory, store) = store_fixture();

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();

                std::thread::spawn(move || {
                    let mut workspace = reservation("owner-1");

                    store
                        .reserve(&mut workspace, MAX_WORKSPACES_PER_OWNER)
                        .map(|()| workspace.project_id)
                })
            })
            .collect();

        let mut reserved = Vec::new();
        let mut refused = 0;

        for handle in handles {
            match handle.join().expect("thread") {
                Ok(project_id) => reserved.push(project_id),
                Err(WorkspaceError::LimitReached) => refused += 1,
                Err(error) => panic!("unexpected reservation error: {error}"),
            }
        }

        assert_eq!(reserved.len(), MAX_WORKSPACES_PER_OWNER as usize);
        assert_eq!(refused, 8 - MAX_WORKSPACES_PER_OWNER as usize);

        reserved.sort_unstable();
        reserved.dedup();
        assert_eq!(
            reserved.len(),
            MAX_WORKSPACES_PER_OWNER as usize,
            "project ids must be unique"
        );

        assert_eq!(
            store.count_owned("owner-1").unwrap(),
            MAX_WORKSPACES_PER_OWNER
        );
    }

    #[test]
    fn a_failed_reservation_leaves_no_partial_record() {
        let (_directory, store) = store_fixture();

        let mut first = reservation("owner-1");
        store.reserve(&mut first, MAX_WORKSPACES_PER_OWNER).unwrap();

        // Reusing the primary key fails after the project id was chosen; the
        // whole transaction must roll back, so neither the row nor the
        // counter moves.
        let mut duplicate = first.clone();
        duplicate.name = "duplicate".into();

        assert!(
            store
                .reserve(&mut duplicate, MAX_WORKSPACES_PER_OWNER)
                .is_err()
        );

        assert_eq!(store.count_owned("owner-1").unwrap(), 1);
        assert_eq!(store.get(&first.id).unwrap().unwrap().name, "probe");
    }

    #[test]
    fn the_owner_limit_is_checked_before_a_project_id_is_allocated() {
        let (_directory, store) = store_fixture();

        for _ in 0..MAX_WORKSPACES_PER_OWNER {
            let mut workspace = reservation("owner-2");
            store
                .reserve(&mut workspace, MAX_WORKSPACES_PER_OWNER)
                .unwrap();
        }

        let mut extra = reservation("owner-2");
        assert!(matches!(
            store.reserve(&mut extra, MAX_WORKSPACES_PER_OWNER),
            Err(WorkspaceError::LimitReached)
        ));
        assert_eq!(extra.project_id, 0);
    }
}
