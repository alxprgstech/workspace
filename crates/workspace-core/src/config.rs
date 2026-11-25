use serde::Deserialize;
use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub resources: ResourceConfig,
    pub workspace: WorkspaceConfig,
    pub network: NetworkConfig,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub sso: Option<SsoConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: SocketAddr,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub data_dir: PathBuf,
    pub max_import_mib: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceConfig {
    pub cpu_millis: u32,
    pub memory_mib: u64,
    pub storage_mib: u64,
    pub max_active_workspaces: u32,
    pub max_concurrent_jobs: u32,
    pub max_processes_per_workspace: u32,
    pub max_job_seconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerMode {
    Ephemeral,
    Persistent,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub default_mode: ContainerMode,
    pub idle_timeout_seconds: u64,
    pub ttl_seconds: u64,
    pub artifact_ttl_seconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    None,
    Allowlist,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    pub default_mode: NetworkMode,
    pub allowlist: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: LogLevel,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SsoConfig {
    pub issuer: String,
    pub client_id: String,
    pub redirect_uri: String,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot access configuration {path:?}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid TOML: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("invalid configuration: {0}")]
    Validation(&'static str),
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();

        let absolute_path = fs::canonicalize(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;

        let contents = fs::read_to_string(&absolute_path).map_err(|source| ConfigError::Read {
            path: absolute_path.clone(),
            source,
        })?;

        let mut config = Self::parse(&contents)?;

        if config.storage.data_dir.is_relative()
            && let Some(parent) = absolute_path.parent()
        {
            config.storage.data_dir = parent.join(&config.storage.data_dir);
        }

        Ok(config)
    }

    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(contents)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.server.listen.port() == 0 {
            return Err(ConfigError::Validation(
                "server.listen must use a non-zero port",
            ));
        }

        if self.storage.data_dir.as_os_str().is_empty() {
            return Err(ConfigError::Validation(
                "storage.data_dir must not be empty",
            ));
        }

        let positive_values = [
            (u64::from(self.resources.cpu_millis), "cpu_millis"),
            (self.resources.memory_mib, "memory_mib"),
            (self.resources.storage_mib, "storage_mib"),
            (
                u64::from(self.resources.max_active_workspaces),
                "max_active_workspaces",
            ),
            (
                u64::from(self.resources.max_concurrent_jobs),
                "max_concurrent_jobs",
            ),
            (
                u64::from(self.resources.max_processes_per_workspace),
                "max_processes_per_workspace",
            ),
            (self.resources.max_job_seconds, "max_job_seconds"),
            (self.storage.max_import_mib, "max_import_mib"),
            (self.workspace.idle_timeout_seconds, "idle_timeout_seconds"),
            (self.workspace.ttl_seconds, "ttl_seconds"),
            (self.workspace.artifact_ttl_seconds, "artifact_ttl_seconds"),
        ];

        for (value, name) in positive_values {
            if value == 0 {
                return Err(ConfigError::Validation(name));
            }
        }

        if self.resources.max_concurrent_jobs > self.resources.max_active_workspaces {
            return Err(ConfigError::Validation(
                "max_concurrent_jobs exceeds max_active_workspaces",
            ));
        }

        if self.storage.max_import_mib > self.resources.storage_mib {
            return Err(ConfigError::Validation(
                "max_import_mib exceeds storage_mib",
            ));
        }

        if self.network.default_mode == NetworkMode::Allowlist && self.network.allowlist.is_empty()
        {
            return Err(ConfigError::Validation(
                "allowlist mode requires at least one allowed destination",
            ));
        }

        if self
            .network
            .allowlist
            .iter()
            .any(|entry| entry.trim().is_empty())
        {
            return Err(ConfigError::Validation(
                "allowlist entries must not be empty",
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../config.example.toml");

    #[test]
    fn example_config_is_valid() {
        let config = Config::parse(EXAMPLE).unwrap();

        assert_eq!(config.resources.cpu_millis, 2000);
        assert_eq!(config.workspace.default_mode, ContainerMode::Ephemeral);
        assert_eq!(config.network.default_mode, NetworkMode::None);
        assert!(config.server.listen.ip().is_loopback());
    }

    #[test]
    fn rejects_zero_memory_limit() {
        let input = EXAMPLE.replace("memory_mib = 4096", "memory_mib = 0");
        assert!(Config::parse(&input).is_err());
    }

    #[test]
    fn rejects_excessive_concurrency() {
        let input = EXAMPLE.replace("max_concurrent_jobs = 1", "max_concurrent_jobs = 10");
        assert!(Config::parse(&input).is_err());
    }

    #[test]
    fn rejects_empty_allowlist() {
        let input = EXAMPLE.replace("default_mode = \"none\"", "default_mode = \"allowlist\"");
        assert!(Config::parse(&input).is_err());
    }

    #[test]
    fn accepts_persistent_mode() {
        let input = EXAMPLE.replace("ephemeral", "persistent");
        let config = Config::parse(&input).unwrap();
        assert_eq!(config.workspace.default_mode, ContainerMode::Persistent);
    }

    #[test]
    fn rejects_unknown_fields() {
        let input = EXAMPLE.replace("[server]", "[server]\nunknown_option = true");
        assert!(Config::parse(&input).is_err());
    }
}
