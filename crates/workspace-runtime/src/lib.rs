#![forbid(unsafe_code)]

//! Dedicated container runtime: Docker control, quota storage, the trusted
//! Linux helper and one unified readiness model.
//!
//! Two rules shape this crate:
//!
//! 1. Nothing here guesses. A property is either proven by a direct query or
//!    the corresponding check fails closed.
//! 2. Nothing here builds a shell command. Every invocation is a fixed program
//!    with fixed arguments; user input never reaches a shell.

pub mod docker;
pub mod helper;
pub mod host;
pub mod quota;
pub mod status;
pub mod storage;

pub use docker::{ContainerSpec, ContainerState, EngineInfo};
pub use helper::Helper;
pub use host::Host;
pub use status::{RuntimeStatus, StatusCheck};

use thiserror::Error;
use workspace_core::config::RuntimeConfig;

/// Failure while talking to the dedicated runtime.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("the execution runtime is not configured")]
    NotConfigured,

    #[error("runtime transport failed: {0}")]
    Transport(String),

    #[error("runtime command timed out after {0} seconds")]
    Timeout(u64),

    #[error("the Docker engine rejected the request: {0}")]
    Engine(String),

    #[error("unexpected runtime response: {0}")]
    Protocol(String),

    #[error("runtime helper failed [{code}]: {message}")]
    Helper { code: String, message: String },

    #[error("storage operation failed: {0}")]
    Storage(String),

    #[error("quota operation failed: {0}")]
    Quota(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl RuntimeError {
    /// Whether the runtime itself, rather than one command, is unavailable.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::NotConfigured | Self::Timeout(_))
    }
}

/// Resolved runtime settings, derived from configuration.
#[derive(Debug, Clone)]
pub struct RuntimeSettings {
    pub backend: RuntimeBackend,
    pub socket: String,
    pub docker_path: String,
    pub engine_name: String,
    pub data_root: String,
    pub helper_path: String,
    pub startup_timeout: std::time::Duration,
    pub command_timeout: std::time::Duration,
    pub workspace_storage_mib: u64,
    pub workspace_files: u64,
    pub minimum_free_mib: u64,
    pub base_image: String,
}

/// Concrete transport shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeBackend {
    /// Dedicated WSL2 distribution.
    Wsl2 { distro: String, user: String },
    /// Rootless Docker on the local Linux host.
    RootlessDocker,
}

impl RuntimeBackend {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Wsl2 { .. } => "wsl2",
            Self::RootlessDocker => "rootless_docker",
        }
    }
}

impl RuntimeSettings {
    /// Build settings from configuration.
    pub fn from_config(config: &RuntimeConfig) -> Self {
        let backend = match config.backend {
            workspace_core::config::RuntimeBackendKind::Wsl2 => RuntimeBackend::Wsl2 {
                distro: config.distro.clone().unwrap_or_default(),
                user: config.user.clone().unwrap_or_default(),
            },
            workspace_core::config::RuntimeBackendKind::RootlessDocker => {
                RuntimeBackend::RootlessDocker
            }
        };

        Self {
            backend,
            socket: config.socket.clone(),
            docker_path: config.docker_path.clone(),
            engine_name: config.engine_name.clone(),
            data_root: config.data_root.trim_end_matches('/').to_owned(),
            helper_path: config.helper_path.clone(),
            startup_timeout: std::time::Duration::from_secs(config.startup_timeout_seconds),
            command_timeout: std::time::Duration::from_secs(config.command_timeout_seconds),
            workspace_storage_mib: config.workspace_storage_mib,
            workspace_files: config.workspace_files,
            minimum_free_mib: config.minimum_free_mib,
            base_image: config.base_image.clone(),
        }
    }
}

/// Docker network name for one workspace's internal network.
///
/// The workspace only reaches the trusted egress proxy from this network.
pub fn workspace_network(instance: &str, workspace_id: &str) -> Result<String, RuntimeError> {
    let short = |value: &str| -> Result<String, RuntimeError> {
        let cleaned: String = value
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .take(8)
            .collect();

        if cleaned.len() < 6 {
            return Err(RuntimeError::Protocol(
                "identifier is too short to name a network".into(),
            ));
        }

        Ok(cleaned)
    };

    Ok(format!(
        "alxprgs-{}-ws-{}",
        short(instance)?,
        short(workspace_id)?
    ))
}

/// Container name of the instance-wide egress proxy.
pub fn egress_proxy_name(instance: &str) -> Result<String, RuntimeError> {
    let cleaned: String = instance
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(8)
        .collect();

    if cleaned.len() < 6 {
        return Err(RuntimeError::Protocol(
            "identifier is too short to name the egress proxy".into(),
        ));
    }

    Ok(format!("alxprgs-{cleaned}-egress"))
}

/// Generate a runtime container name from server-side identifiers only.
///
/// Names never contain user input: the instance id and the resource ids are
/// server-assigned, and the function refuses anything unexpected.
pub fn container_name(instance: &str, kind: &str, id: &str) -> Result<String, RuntimeError> {
    let short = |value: &str| -> Result<String, RuntimeError> {
        let cleaned: String = value
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .take(8)
            .collect();

        if cleaned.len() < 6 {
            return Err(RuntimeError::Protocol(
                "identifier is too short to name a container".into(),
            ));
        }

        Ok(cleaned)
    };

    if !matches!(kind, "job" | "workspace" | "build" | "probe") {
        return Err(RuntimeError::Protocol("unknown container kind".into()));
    }

    Ok(format!(
        "alxprgs-{}-{kind}-{}",
        short(instance)?,
        short(id)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_names_are_server_generated() {
        let name = container_name(
            "f32ac08f-5f8c-41df-8a72-820d9ff123d8",
            "job",
            "550e8400-e29b-41d4-a716-446655440000",
        )
        .unwrap();

        assert_eq!(name, "alxprgs-f32ac08f-job-550e8400");
        assert!(!name.contains('='));
    }

    #[test]
    fn container_names_reject_unknown_kinds_and_short_ids() {
        assert!(
            container_name("f32ac08f-5f8c-41df-8a72-820d9ff123d8", "shell", "550e8400").is_err()
        );
        assert!(container_name("abc", "job", "550e8400").is_err());
    }
}
