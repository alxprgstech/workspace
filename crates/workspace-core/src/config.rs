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
    #[serde(default)]
    pub update: UpdateConfig,
}

/// Where a release package comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpdateSource {
    /// A verified package directory on the local disk.
    #[default]
    Local,
    /// The latest release published for `update.repository`.
    Github,
}

/// Release/update settings.
///
/// Every field is optional: a default installation updates from
/// `<install_dir>/updates/package` and needs no configuration at all. Nothing
/// here can execute a command or fetch an arbitrary URL: the source is either a
/// local directory or the configured repository.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateConfig {
    #[serde(default)]
    pub source: UpdateSource,

    /// Directory containing `workspace-release.json` and the release files.
    ///
    /// Defaults to `<install_dir>/updates/package`.
    #[serde(default)]
    pub package_dir: Option<PathBuf>,

    /// Explicit installation root. Defaults to the directory of the running
    /// executable.
    #[serde(default)]
    pub install_dir: Option<PathBuf>,

    /// Ed25519 public key (base64, 32 bytes) that must have signed the release
    /// manifest. Defaults to the key embedded in the binary.
    #[serde(default)]
    pub public_key_path: Option<PathBuf>,

    /// `owner/name` of the GitHub repository used by the `github` source.
    #[serde(default)]
    pub repository: Option<String>,

    /// Name of the environment variable holding a GitHub token. Empty means an
    /// anonymous request, which is enough for a public repository.
    #[serde(default)]
    pub api_token_env: Option<String>,

    /// Stop waiting for running jobs after this many seconds and abort the
    /// update without changing anything.
    #[serde(default)]
    pub drain_timeout_seconds: Option<u64>,

    /// How long the new server may take to report healthy.
    #[serde(default)]
    pub health_timeout_seconds: Option<u64>,

    /// Additional free space that must remain on both the Windows volume and
    /// the Linux runtime filesystem after staging a backup.
    #[serde(default)]
    pub reserve_free_mib: Option<u64>,

    /// Restart the server after a successful update (default: true).
    #[serde(default)]
    pub restart: Option<bool>,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            source: UpdateSource::Local,
            package_dir: None,
            install_dir: None,
            public_key_path: None,
            repository: None,
            api_token_env: None,
            drain_timeout_seconds: None,
            health_timeout_seconds: None,
            reserve_free_mib: None,
            restart: None,
        }
    }
}

impl UpdateConfig {
    pub fn drain_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.drain_timeout_seconds.unwrap_or(300).clamp(1, 3600))
    }

    pub fn health_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.health_timeout_seconds.unwrap_or(120).clamp(5, 900))
    }

    pub fn reserve_free_mib(&self) -> u64 {
        self.reserve_free_mib.unwrap_or(512).clamp(64, 65_536)
    }

    pub fn restart(&self) -> bool {
        self.restart.unwrap_or(true)
    }

    pub fn repository(&self) -> &str {
        self.repository
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("alxprgstech/workspace")
    }

    pub fn api_token_env(&self) -> &str {
        self.api_token_env
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("WORKSPACE_UPDATE_TOKEN")
    }
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

    /// Quota helper installed with root privileges. Defaults to
    /// `/usr/sbin/alxprgs-quota`.
    #[serde(default)]
    pub quota_helper_path: Option<String>,

    /// Sudoers rule for the quota helper. Defaults to
    /// `/etc/sudoers.d/alxprgs-workspace-quota`.
    #[serde(default)]
    pub sudoers_path: Option<String>,

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

        if let Some(path) = self.quota_helper_path.as_deref()
            && (!path.starts_with('/') || path.contains("..") || has_control_or_space(path))
        {
            return Err("runtime.quota_helper_path must be an absolute, closed Linux path");
        }

        if let Some(path) = self.sudoers_path.as_deref()
            && (!path.starts_with('/') || path.contains("..") || has_control_or_space(path))
        {
            return Err("runtime.sudoers_path must be an absolute, closed Linux path");
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

    /// Path of the root-run quota helper.
    pub fn quota_helper(&self) -> &str {
        self.quota_helper_path
            .as_deref()
            .unwrap_or("/usr/sbin/alxprgs-quota")
    }

    /// Path of the sudoers rule for the quota helper.
    pub fn sudoers(&self) -> &str {
        self.sudoers_path
            .as_deref()
            .unwrap_or("/etc/sudoers.d/alxprgs-workspace-quota")
    }

    /// Path of the egress proxy: `network.egress_binary`, else the trusted
    /// runtime directory that holds the helper.
    pub fn egress_binary(&self, configured: Option<&str>) -> String {
        if let Some(path) = configured
            && !path.trim().is_empty()
        {
            return path.to_owned();
        }

        match self.helper_path.rsplit_once('/') {
            Some((directory, _)) => format!("{directory}/workspace-egress"),
            None => format!("{}/workspace-egress", self.data_root.trim_end_matches('/')),
        }
    }
}

/// Reject values that could break out of a single `argv` entry or a sudoers
/// line. Paths are passed as separate arguments, but a newline in a path is
/// still a configuration error.
fn has_control_or_space(value: &str) -> bool {
    value
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
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
    /// Absolute path of the trusted egress proxy binary inside the runtime.
    /// Defaults to `<runtime.data_root>/bin/workspace-egress`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_binary: Option<String>,
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

        self.validate_update()?;

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

        // Entries become the trusted proxy's policy: hostnames only, no
        // wildcards, ports, paths or IP literals.
        for entry in &self.network.allowlist {
            let entry = entry.trim();

            let valid = entry.len() <= 253
                && !entry.starts_with('.')
                && !entry.ends_with('.')
                && entry
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
                && entry.contains('.');

            if !valid {
                return Err(ConfigError::Validation(
                    "allowlist entries must be lowercase hostnames such as crates.io",
                ));
            }
        }

        if let Some(binary) = self.network.egress_binary.as_deref()
            && !binary.starts_with('/')
        {
            return Err(ConfigError::Validation(
                "network.egress_binary must be an absolute path",
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

    /// The update section may point at local paths only; it can never name a
    /// command, a URL template or an arbitrary executable.
    fn validate_update(&self) -> Result<(), ConfigError> {
        let update = &self.update;

        if let Some(directory) = update.package_dir.as_ref()
            && directory.as_os_str().is_empty()
        {
            return Err(ConfigError::Validation(
                "update.package_dir must not be empty",
            ));
        }

        if let Some(directory) = update.install_dir.as_ref()
            && directory.as_os_str().is_empty()
        {
            return Err(ConfigError::Validation(
                "update.install_dir must not be empty",
            ));
        }

        if let Some(path) = update.public_key_path.as_ref()
            && path.as_os_str().is_empty()
        {
            return Err(ConfigError::Validation(
                "update.public_key_path must not be empty",
            ));
        }

        if let Some(repository) = update.repository.as_deref() {
            let valid = !repository.is_empty()
                && repository.len() <= 200
                && repository.split('/').count() == 2
                && repository.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "-_./".contains(character)
                });

            if !valid {
                return Err(ConfigError::Validation(
                    "update.repository must look like owner/name",
                ));
            }
        }

        if let Some(name) = update.api_token_env.as_deref() {
            let valid = !name.is_empty()
                && name.len() <= 128
                && name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_');

            if !valid {
                return Err(ConfigError::Validation(
                    "update.api_token_env must be an environment variable name",
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

    #[test]
    fn update_defaults_are_safe() {
        let config = Config::parse(EXAMPLE).unwrap();

        assert_eq!(config.update.source, UpdateSource::Local);
        assert!(config.update.package_dir.is_none());
        assert!(config.update.public_key_path.is_none());
        assert_eq!(config.update.drain_timeout().as_secs(), 300);
        assert!(config.update.restart());
        assert_eq!(config.update.repository(), "alxprgstech/workspace");
    }

    #[test]
    fn accepts_update_and_runtime_component_paths() {
        let input = format!(
            "{EXAMPLE}\n[update]\nsource = \"local\"\n\
             package_dir = \"C:\\\\updates\"\n\
             public_key_path = \"C:\\\\keys\\\\release.pub\"\n\
             drain_timeout_seconds = 42\n"
        );

        let config = Config::parse(&input).unwrap();

        assert_eq!(config.update.drain_timeout().as_secs(), 42);
        assert_eq!(
            config.update.package_dir.as_deref(),
            Some(std::path::Path::new("C:\\updates"))
        );
    }

    #[test]
    fn rejects_relative_update_install_dir() {
        let input = format!("{EXAMPLE}\n[update]\ninstall_dir = \"\"\n");
        assert!(Config::parse(&input).is_err());
    }

    #[test]
    fn runtime_component_defaults_match_provisioning() {
        let runtime: RuntimeConfig = toml::from_str(
            r#"
backend = "rootless_docker"
docker_path = "/usr/bin/docker"
engine_name = "alxprgs-workspace"
socket = "unix:///run/user/1000/docker.sock"
data_root = "/srv/alxprgs-workspace/data"
helper_path = "/srv/alxprgs-workspace/bin/workspace-helper"
startup_timeout_seconds = 120
command_timeout_seconds = 60
workspace_storage_mib = 8192
workspace_files = 200000
minimum_free_mib = 2048
base_image = "debian@sha256:aa"
"#,
        )
        .unwrap();

        assert_eq!(runtime.quota_helper(), "/usr/sbin/alxprgs-quota");
        assert_eq!(runtime.sudoers(), "/etc/sudoers.d/alxprgs-workspace-quota");
        assert_eq!(
            runtime.egress_binary(None),
            "/srv/alxprgs-workspace/bin/workspace-egress"
        );
        assert_eq!(runtime.egress_binary(Some("/opt/egress")), "/opt/egress");
        assert!(runtime.validate().is_ok());
    }

    #[test]
    fn rejects_unsafe_quota_paths() {
        let runtime: RuntimeConfig = toml::from_str(
            r#"
backend = "rootless_docker"
docker_path = "/usr/bin/docker"
engine_name = "alxprgs-workspace"
socket = "unix:///run/user/1000/docker.sock"
data_root = "/srv/alxprgs-workspace/data"
helper_path = "/srv/alxprgs-workspace/bin/workspace-helper"
quota_helper_path = "/usr/sbin/../../etc/passwd"
startup_timeout_seconds = 120
command_timeout_seconds = 60
workspace_storage_mib = 8192
workspace_files = 200000
minimum_free_mib = 2048
base_image = "debian@sha256:aa"
"#,
        )
        .unwrap();

        assert!(runtime.validate().is_err());
    }
}
