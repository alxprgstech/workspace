use serde::Deserialize;
use std::{
    fs,
    net::{IpAddr, SocketAddr},
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
    #[serde(default)]
    pub mcp: Option<McpConfig>,
    #[serde(default)]
    pub runtime: Option<RuntimeConfig>,
}

/// Container runtime backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeBackendKind {
    /// Dedicated WSL2 distribution hosting its own rootless Docker Engine.
    Wsl2,
    /// Dedicated rootless Docker Engine on the local Linux host.
    RootlessDocker,
}

/// Explicit description of the dedicated execution runtime.
///
/// The section is optional so the control plane can run without execution, but
/// execution stays disabled unless every field below names the dedicated
/// runtime explicitly. There are deliberately no defaults that could connect
/// to Docker Desktop or another engine.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub backend: RuntimeBackendKind,

    /// WSL2 distribution name (`wsl2` backend only).
    #[serde(default)]
    pub distro: Option<String>,

    /// Linux user inside the distribution (`wsl2` backend only).
    #[serde(default)]
    pub user: Option<String>,

    /// Absolute path of the Docker client inside the dedicated environment.
    pub docker_path: String,

    /// Engine name that must be reported by `docker info`.
    ///
    /// This is what keeps the server from silently using Docker Desktop or any
    /// other engine that happens to be reachable: a different name fails the
    /// dedicated-runtime check.
    pub engine_name: String,

    /// Dedicated engine socket. Only `unix://` endpoints are accepted, so no
    /// TCP Docker API can ever be used.
    pub socket: String,

    /// Root of the dedicated Linux data filesystem.
    pub data_root: String,

    /// Trusted helper binary inside the dedicated environment.
    pub helper_path: String,

    /// How long a cold WSL2/distribution start may take.
    pub startup_timeout_seconds: u64,

    /// Upper bound for a single control command.
    pub command_timeout_seconds: u64,

    /// Per-workspace hard storage quota.
    pub workspace_storage_mib: u64,

    /// Per-workspace hard file (inode) quota.
    pub workspace_files: u64,

    /// Minimum free space that must remain on the dedicated filesystem.
    pub minimum_free_mib: u64,

    /// Immutable base image reference (`name@sha256:...`).
    pub base_image: String,
}

impl RuntimeConfig {
    /// Check the parts that do not require touching the host.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.docker_path.starts_with('/') {
            return Err("runtime.docker_path must be an absolute Linux path");
        }

        if self.engine_name.is_empty()
            || self.engine_name.len() > 64
            || !self
                .engine_name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            return Err("runtime.engine_name must be a simple engine name");
        }

        if !self.socket.starts_with("unix:///") {
            return Err("runtime.socket must be a unix:// endpoint");
        }

        if !self.data_root.starts_with('/') || self.data_root.contains("..") {
            return Err("runtime.data_root must be an absolute, closed Linux path");
        }

        if !self.helper_path.starts_with('/') || self.helper_path.contains("..") {
            return Err("runtime.helper_path must be an absolute, closed Linux path");
        }

        if !self.helper_path.starts_with(&self.data_root)
            && !self.helper_path.starts_with("/usr/local/")
            && !self.helper_path.starts_with("/srv/")
        {
            return Err("runtime.helper_path must be a trusted installation path");
        }

        match self.backend {
            RuntimeBackendKind::Wsl2 => {
                if self.distro.as_deref().unwrap_or_default().is_empty() {
                    return Err("runtime.distro is required for the wsl2 backend");
                }

                if self.user.as_deref().unwrap_or_default().is_empty() {
                    return Err("runtime.user is required for the wsl2 backend");
                }
            }
            RuntimeBackendKind::RootlessDocker => {
                if self.distro.is_some() || self.user.is_some() {
                    return Err("runtime.distro/user are only valid for the wsl2 backend");
                }
            }
        }

        if [
            self.startup_timeout_seconds,
            self.command_timeout_seconds,
            self.workspace_storage_mib,
            self.workspace_files,
            self.minimum_free_mib,
        ]
        .contains(&0)
        {
            return Err("runtime timeouts and quotas must be positive");
        }

        if !self.base_image.contains("@sha256:") {
            return Err("runtime.base_image must be pinned by digest");
        }

        Ok(())
    }
}

/// MCP gateway settings.
///
/// The gateway stays disabled unless this section exists: an unconfigured
/// deployment must never expose an MCP endpoint with no verified audience.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    /// Identity provider that issues MCP access tokens.
    pub issuer: String,
    /// Token audience that identifies this deployment's MCP resource.
    pub audience: String,
    /// Canonical resource identifier advertised in protected-resource metadata.
    pub resource: String,
    /// Scopes every MCP request must carry.
    #[serde(default)]
    pub required_scopes: Vec<String>,
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

    /// Addresses of reverse proxies whose `X-Forwarded-For` may be trusted.
    /// Empty by default: forwarded headers are then ignored entirely.
    #[serde(default)]
    pub trusted_proxies: Vec<IpAddr>,
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

        if let Some(runtime) = self.runtime.as_ref() {
            runtime.validate().map_err(ConfigError::Validation)?;

            if runtime.workspace_storage_mib > self.resources.storage_mib {
                return Err(ConfigError::Validation(
                    "runtime.workspace_storage_mib exceeds the storage budget",
                ));
            }

            let budget = runtime
                .workspace_storage_mib
                .saturating_mul(u64::from(self.resources.max_active_workspaces));

            if budget > self.resources.storage_mib {
                return Err(ConfigError::Validation(
                    "per-workspace quotas exceed the global storage budget",
                ));
            }

            if runtime.backend == RuntimeBackendKind::Wsl2 && !cfg!(windows) {
                return Err(ConfigError::Validation(
                    "the wsl2 backend is only available on Windows",
                ));
            }

            if runtime.backend == RuntimeBackendKind::RootlessDocker && cfg!(windows) {
                return Err(ConfigError::Validation(
                    "the rootless_docker backend requires a Linux host",
                ));
            }
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
