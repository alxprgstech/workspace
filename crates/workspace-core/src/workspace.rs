//! Workspace domain model.
//!
//! State transitions are validated here so no caller (HTTP, MCP or the
//! supervisor) can put a workspace into an inconsistent state.

use serde::{Deserialize, Serialize};

/// Execution mode chosen at creation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// A fresh container per job; nothing runs between jobs.
    Ephemeral,
    /// One container per workspace, stopped after the idle timeout.
    Persistent,
}

impl ExecutionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ephemeral => "ephemeral",
            Self::Persistent => "persistent",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "ephemeral" => Some(Self::Ephemeral),
            "persistent" => Some(Self::Persistent),
            _ => None,
        }
    }
}

/// Workspace lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    /// Configuration and storage exist; no container.
    Created,
    /// The trusted image is being prepared.
    Preparing,
    /// A container is starting.
    Starting,
    /// A job is executing.
    Running,
    /// A persistent container is up without an active job.
    Idle,
    /// A container is being stopped.
    Stopping,
    /// Storage is preserved; no container.
    Stopped,
    /// The environment could not be prepared or was lost.
    Failed,
    /// Deletion is in progress.
    Deleting,
    /// The TTL elapsed.
    Expired,
}

impl WorkspaceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Preparing => "preparing",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Deleting => "deleting",
            Self::Expired => "expired",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "created" => Self::Created,
            "preparing" => Self::Preparing,
            "starting" => Self::Starting,
            "running" => Self::Running,
            "idle" => Self::Idle,
            "stopping" => Self::Stopping,
            "stopped" => Self::Stopped,
            "failed" => Self::Failed,
            "deleting" => Self::Deleting,
            "expired" => Self::Expired,
            _ => return None,
        })
    }

    /// Whether the state means a container may hold compute budget.
    pub fn holds_compute(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Idle)
    }

    /// Whether the state means a job or container operation is in flight.
    pub fn is_transitional(self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Starting | Self::Stopping | Self::Deleting
        )
    }

    /// Allowed next states.
    pub fn can_transition(self, next: Self) -> bool {
        match self {
            Self::Created => matches!(
                next,
                Self::Preparing
                    | Self::Starting
                    | Self::Running
                    | Self::Stopped
                    | Self::Deleting
                    | Self::Expired
            ),
            Self::Preparing => matches!(
                next,
                Self::Created | Self::Starting | Self::Failed | Self::Deleting
            ),
            Self::Starting => matches!(
                next,
                Self::Running | Self::Idle | Self::Failed | Self::Stopping | Self::Deleting
            ),
            Self::Running => matches!(
                next,
                Self::Idle | Self::Stopping | Self::Failed | Self::Deleting
            ),
            Self::Idle => matches!(
                next,
                Self::Running | Self::Stopping | Self::Failed | Self::Deleting
            ),
            Self::Stopping => matches!(
                next,
                Self::Stopped | Self::Created | Self::Failed | Self::Deleting
            ),
            Self::Stopped => matches!(
                next,
                Self::Starting | Self::Created | Self::Failed | Self::Deleting | Self::Expired
            ),
            Self::Failed => matches!(
                next,
                Self::Starting | Self::Stopping | Self::Deleting | Self::Expired
            ),
            Self::Deleting => matches!(next, Self::Deleting),
            Self::Expired => matches!(next, Self::Deleting),
        }
    }
}

/// Enforcement limits applied to one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub cpu_millis: u32,
    pub memory_mib: u64,
    pub pids: u32,
    pub disk_mib: u64,
    pub files: u64,
    pub max_job_seconds: u64,
}

impl Limits {
    /// Clamp requested limits to the instance budget.
    ///
    /// A caller may ask for less, never for more; the server decides.
    pub fn clamp_to(self, budget: &Limits) -> Self {
        Self {
            cpu_millis: self.cpu_millis.clamp(100, budget.cpu_millis),
            memory_mib: self.memory_mib.clamp(64, budget.memory_mib),
            pids: self.pids.clamp(16, budget.pids),
            disk_mib: self.disk_mib.clamp(16, budget.disk_mib),
            files: self.files.clamp(16, budget.files),
            max_job_seconds: self.max_job_seconds.clamp(1, budget.max_job_seconds),
        }
    }
}

/// One workspace as stored and returned to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub owner_id: String,
    pub name: String,
    pub execution_mode: ExecutionMode,
    pub state: WorkspaceState,
    pub capabilities: Vec<String>,
    pub image: String,
    pub network_mode: String,
    pub limits: Limits,
    pub project_id: u32,
    pub container_name: Option<String>,
    pub created_at_unix: i64,
    pub updated_at_unix: i64,
    pub last_activity_unix: i64,
    pub ttl_seconds: u64,
    pub expires_at_unix: i64,
    pub pinned: bool,
    pub error: Option<String>,
}

impl Workspace {
    /// Whether automatic TTL cleanup may delete this workspace right now.
    pub fn is_expired_at(&self, now: i64) -> bool {
        !self.pinned && now >= self.expires_at_unix
    }

    /// Whether the workspace still has room before its TTL.
    pub fn ttl_remaining_seconds(&self, now: i64) -> i64 {
        (self.expires_at_unix - now).max(0)
    }
}

/// Validate a user-supplied workspace name.
pub fn validate_name(name: &str) -> Result<(), &'static str> {
    let trimmed = name.trim();

    if trimmed.is_empty() {
        return Err("workspace name must not be empty");
    }

    if trimmed.chars().count() > 64 {
        return Err("workspace name must be at most 64 characters");
    }

    if trimmed.chars().any(char::is_control) {
        return Err("workspace name must not contain control characters");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip_through_strings() {
        for state in [
            WorkspaceState::Created,
            WorkspaceState::Preparing,
            WorkspaceState::Starting,
            WorkspaceState::Running,
            WorkspaceState::Idle,
            WorkspaceState::Stopping,
            WorkspaceState::Stopped,
            WorkspaceState::Failed,
            WorkspaceState::Deleting,
            WorkspaceState::Expired,
        ] {
            assert_eq!(WorkspaceState::parse(state.as_str()), Some(state));
        }
    }

    #[test]
    fn invalid_transitions_are_refused() {
        assert!(WorkspaceState::Created.can_transition(WorkspaceState::Running));
        assert!(!WorkspaceState::Created.can_transition(WorkspaceState::Idle));
        assert!(WorkspaceState::Stopped.can_transition(WorkspaceState::Starting));
        assert!(!WorkspaceState::Running.can_transition(WorkspaceState::Preparing));
    }

    #[test]
    fn limits_are_clamped_to_the_budget() {
        let budget = Limits {
            cpu_millis: 2000,
            memory_mib: 4096,
            pids: 256,
            disk_mib: 8192,
            files: 200_000,
            max_job_seconds: 3600,
        };

        let requested = Limits {
            cpu_millis: 16_000,
            memory_mib: 32_768,
            pids: 4096,
            disk_mib: 1_000_000,
            files: 10_000_000,
            max_job_seconds: 86_400,
        };

        let clamped = requested.clamp_to(&budget);

        assert_eq!(clamped, budget);

        let smaller = Limits {
            cpu_millis: 500,
            ..requested
        };

        assert_eq!(smaller.clamp_to(&budget).cpu_millis, 500);
    }

    #[test]
    fn names_are_validated() {
        assert!(validate_name("my project").is_ok());
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"x".repeat(65)).is_err());
        assert!(validate_name("bad\u{7}name").is_err());
    }

    #[test]
    fn expiry_respects_pinning() {
        let mut workspace = Workspace {
            id: "id".into(),
            owner_id: "owner".into(),
            name: "name".into(),
            execution_mode: ExecutionMode::Ephemeral,
            state: WorkspaceState::Created,
            capabilities: vec!["base".into()],
            image: "debian@sha256:00".into(),
            network_mode: "none".into(),
            limits: Limits {
                cpu_millis: 1000,
                memory_mib: 512,
                pids: 64,
                disk_mib: 1024,
                files: 1000,
                max_job_seconds: 600,
            },
            project_id: 1020,
            container_name: None,
            created_at_unix: 0,
            updated_at_unix: 0,
            last_activity_unix: 0,
            ttl_seconds: 60,
            expires_at_unix: 100,
            pinned: false,
            error: None,
        };

        assert!(!workspace.is_expired_at(99));
        assert!(workspace.is_expired_at(100));

        workspace.pinned = true;
        assert!(!workspace.is_expired_at(1000));
    }
}
