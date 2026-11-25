//! Typed Docker control.
//!
//! Every operation is a fixed `docker` invocation with server-generated
//! arguments. There is no code path that accepts a Docker flag, mount, device,
//! network mode or image from a caller: the only knob a caller controls is the
//! set of already validated fields in [`ContainerSpec`].

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use workspace_core::helper::MAX_PATH_BYTES;

use crate::host::Host;
use crate::{RuntimeError, RuntimeSettings};

/// Hardened container description.
#[derive(Debug, Clone)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub labels: Vec<(String, String)>,
    pub workspace_dir: String,
    pub cache_dir: Option<String>,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cpu_millis: u32,
    pub memory_mib: u64,
    pub pids: u32,
    pub tmpfs_mib: u64,
    /// `none` only: the allowlist mode requires the egress proxy (stage 013).
    pub network: bool,
}

impl ContainerSpec {
    /// Build the `docker create`/`docker run` argument list.
    ///
    /// Called by the server only; every value is validated here so a malformed
    /// field can never reach the engine.
    pub fn arguments(&self, subcommand: &str) -> Result<Vec<String>, RuntimeError> {
        if !matches!(subcommand, "create" | "run") {
            return Err(RuntimeError::Protocol("unsupported docker verb".into()));
        }

        validate_container_name(&self.name)?;
        validate_image(&self.image)?;
        validate_path(&self.workspace_dir)?;

        if let Some(cache) = self.cache_dir.as_deref() {
            validate_path(cache)?;
        }

        if self.argv.is_empty() {
            return Err(RuntimeError::Protocol(
                "a container command is required".into(),
            ));
        }

        if !(100..=32_000).contains(&self.cpu_millis) {
            return Err(RuntimeError::Protocol("cpu limit out of range".into()));
        }

        if !(64..=65_536).contains(&self.memory_mib) {
            return Err(RuntimeError::Protocol("memory limit out of range".into()));
        }

        if !(16..=4096).contains(&self.pids) {
            return Err(RuntimeError::Protocol("pid limit out of range".into()));
        }

        if !(16..=1024).contains(&self.tmpfs_mib) {
            return Err(RuntimeError::Protocol("tmpfs limit out of range".into()));
        }

        if !self.network {
            // Nothing to do: `--network none` is always passed below.
        } else {
            return Err(RuntimeError::Protocol(
                "allowlist networking requires the egress proxy".into(),
            ));
        }

        let mut arguments = vec![
            subcommand.to_owned(),
            "--name".to_owned(),
            self.name.clone(),
        ];

        for (key, value) in &self.labels {
            validate_label(key, value)?;
            arguments.push("--label".to_owned());
            arguments.push(format!("{key}={value}"));
        }

        // No inbound ports, no host namespaces, no extra capabilities.
        arguments.extend([
            "--network".to_owned(),
            "none".to_owned(),
            "--read-only".to_owned(),
            "--tmpfs".to_owned(),
            format!("/tmp:rw,nosuid,nodev,noexec,size={}m", self.tmpfs_mib),
            "--cap-drop".to_owned(),
            "ALL".to_owned(),
            "--security-opt".to_owned(),
            "no-new-privileges".to_owned(),
            "--pids-limit".to_owned(),
            self.pids.to_string(),
            "--memory".to_owned(),
            format!("{}m", self.memory_mib),
            "--memory-swap".to_owned(),
            format!("{}m", self.memory_mib),
            "--cpus".to_owned(),
            format!("{:.3}", f64::from(self.cpu_millis) / 1000.0),
            // Rootless Docker maps container root to the unprivileged host
            // user, so root inside the container has no host authority.
            "--user".to_owned(),
            "0:0".to_owned(),
            "--workdir".to_owned(),
            "/workspace".to_owned(),
            "--log-driver".to_owned(),
            "local".to_owned(),
            "--log-opt".to_owned(),
            "max-size=8m".to_owned(),
            "--log-opt".to_owned(),
            "max-file=2".to_owned(),
            "--mount".to_owned(),
            format!("type=bind,source={},target=/workspace", self.workspace_dir),
        ]);

        if let Some(cache) = self.cache_dir.as_deref() {
            arguments.push("--mount".to_owned());
            arguments.push(format!("type=bind,source={cache},target=/cache"));
        }

        for (key, value) in &self.env {
            validate_env(key, value)?;
            arguments.push("--env".to_owned());
            arguments.push(format!("{key}={value}"));
        }

        arguments.push(self.image.clone());
        arguments.extend(self.argv.iter().cloned());

        Ok(arguments)
    }
}

/// Engine properties observed through `docker info`.
#[derive(Debug, Clone, Serialize)]
pub struct EngineInfo {
    pub name: String,
    pub server_version: String,
    pub ostype: String,
    pub rootless: bool,
    pub seccomp: bool,
    pub cgroup_version: String,
    pub cgroup_driver: String,
    pub root_dir: String,
    pub storage_driver: String,
    pub security_options: Vec<String>,
}

impl EngineInfo {
    /// Parse `docker info --format {{json .}}`.
    pub fn parse(raw: &str) -> Result<Self, RuntimeError> {
        let value: Value = serde_json::from_str(raw)
            .map_err(|error| RuntimeError::Protocol(format!("invalid docker info: {error}")))?;

        let text = |field: &str| -> Result<String, RuntimeError> {
            value[field]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| RuntimeError::Protocol(format!("docker info lacks {field}")))
        };

        let security_options: Vec<String> = value["SecurityOptions"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();

        let has_option = |wanted: &str| {
            security_options
                .iter()
                .any(|option| option.split(',').any(|part| part == wanted))
        };

        Ok(Self {
            name: text("Name")?,
            server_version: text("ServerVersion")?,
            ostype: text("OSType")?,
            rootless: has_option("name=rootless"),
            seccomp: has_option("name=seccomp"),
            cgroup_version: text("CgroupVersion")?,
            cgroup_driver: text("CgroupDriver")?,
            root_dir: text("DockerRootDir")?,
            storage_driver: text("Driver")?,
            security_options,
        })
    }
}

/// Container state as reported by `docker inspect`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerState {
    pub status: String,
    pub running: bool,
    pub exit_code: i64,
    pub oom_killed: bool,
    pub error: Option<String>,
}

impl ContainerState {
    pub fn is_finished(&self) -> bool {
        !self.running && self.status != "created"
    }
}

/// A container created by this instance.
#[derive(Debug, Clone, Serialize)]
pub struct ManagedContainer {
    pub name: String,
    pub id: String,
    pub state: String,
    pub workspace: Option<String>,
    pub job: Option<String>,
    pub kind: Option<String>,
}

/// Docker client bound to one explicit dedicated endpoint.
#[derive(Debug, Clone)]
pub struct Docker {
    host: Host,
    socket: String,
    docker_path: String,
}

impl Docker {
    pub fn new(host: Host, settings: &RuntimeSettings) -> Self {
        Self {
            host,
            socket: settings.socket.clone(),
            docker_path: settings.docker_path.clone(),
        }
    }

    fn arguments(&self, extra: Vec<String>) -> Vec<String> {
        let mut arguments = vec!["--host".to_owned(), self.socket.clone()];

        arguments.extend(extra);
        arguments
    }

    /// Direct engine query. Never cached here; callers decide about caching.
    pub async fn info(&self) -> Result<EngineInfo, RuntimeError> {
        let raw = self
            .host
            .run(
                &self.docker_path,
                &self.arguments(vec![
                    "info".to_owned(),
                    "--format".to_owned(),
                    "{{json .}}".to_owned(),
                ]),
            )
            .await?;

        EngineInfo::parse(&raw)
    }

    /// Wait for a cold start and return the verified engine description.
    ///
    /// A stopped WSL2 distribution and its rootless engine need several seconds
    /// before the socket accepts connections. Every attempt is a bounded,
    /// read-only query and the total wait is capped by the startup timeout.
    pub async fn wait_until_ready(&self) -> Result<EngineInfo, RuntimeError> {
        let deadline = tokio::time::Instant::now() + self.host.startup_timeout();

        loop {
            let error = match self.info().await {
                Ok(info) if info.ostype == "linux" => return Ok(info),
                Ok(_) => RuntimeError::Engine("engine is not a Linux engine".into()),
                Err(error) => error,
            };

            if tokio::time::Instant::now() >= deadline {
                return Err(error);
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Whether an exact image reference (preferably digest-pinned) exists.
    pub async fn image_present(&self, reference: &str) -> Result<bool, RuntimeError> {
        let result = self
            .host
            .run(
                &self.docker_path,
                &self.arguments(vec![
                    "image".to_owned(),
                    "inspect".to_owned(),
                    "--format".to_owned(),
                    "{{.Id}}".to_owned(),
                    reference.to_owned(),
                ]),
            )
            .await;

        match result {
            Ok(_) => Ok(true),
            Err(RuntimeError::Transport(message)) if message.contains("No such image") => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Immutable identifier of an existing image.
    pub async fn image_id(&self, reference: &str) -> Result<String, RuntimeError> {
        validate_reference(reference)?;

        self.host
            .run(
                &self.docker_path,
                &self.arguments(vec![
                    "image".to_owned(),
                    "inspect".to_owned(),
                    "--format".to_owned(),
                    "{{.Id}}".to_owned(),
                    reference.to_owned(),
                ]),
            )
            .await
    }

    /// Retag an image, used to pin a build to its immutable identifier.
    pub async fn tag(&self, source: &str, target: &str) -> Result<(), RuntimeError> {
        validate_reference(source)?;
        validate_reference(target)?;

        self.host
            .run(
                &self.docker_path,
                &self.arguments(vec!["tag".to_owned(), source.to_owned(), target.to_owned()]),
            )
            .await
            .map(|_| ())
    }

    /// Pull an immutable image reference. Used only for digest-pinned images.
    pub async fn pull(&self, reference: &str) -> Result<(), RuntimeError> {
        validate_image(reference)?;

        self.host
            .run_with_timeout(
                &self.docker_path,
                &self.arguments(vec!["pull".to_owned(), reference.to_owned()]),
                Duration::from_secs(600),
            )
            .await
            .map(|_| ())
    }

    /// Create a container without starting it.
    pub async fn create(&self, spec: &ContainerSpec) -> Result<(), RuntimeError> {
        self.host
            .run(
                &self.docker_path,
                &self.arguments(spec.arguments("create")?),
            )
            .await
            .map(|_| ())
    }

    /// Start a previously created container.
    pub async fn start(&self, name: &str) -> Result<(), RuntimeError> {
        validate_container_name(name)?;

        self.host
            .run(
                &self.docker_path,
                &self.arguments(vec!["start".to_owned(), name.to_owned()]),
            )
            .await
            .map(|_| ())
    }

    /// Run a container attached and return its captured output.
    pub async fn run_attached(
        &self,
        spec: &ContainerSpec,
        timeout: Duration,
    ) -> Result<AttachedRun, RuntimeError> {
        let arguments = self.arguments(spec.arguments("run")?);

        let mut command = self.host.command(&self.docker_path, &arguments)?;

        let output = tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| RuntimeError::Timeout(timeout.as_secs()))?
            .map_err(|error| RuntimeError::Transport(error.to_string()))?;

        let exit_code = i64::from(output.status.code().unwrap_or(-1));

        // Docker itself uses 125 for daemon-level failures; the container never
        // started in that case.
        if exit_code == 125 {
            return Err(RuntimeError::Engine(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }

        Ok(AttachedRun {
            exit_code,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    /// Build an attached `docker run` command for a job.
    pub fn command_run(
        &self,
        spec: &ContainerSpec,
    ) -> Result<tokio::process::Command, RuntimeError> {
        let arguments = self.arguments(spec.arguments("run")?);

        self.host.command(&self.docker_path, &arguments)
    }

    /// Build an attached `docker exec` command inside a workspace container.
    pub fn command_exec(
        &self,
        name: &str,
        argv: &[String],
        workdir: &str,
        env: &[(String, String)],
    ) -> Result<tokio::process::Command, RuntimeError> {
        validate_container_name(name)?;

        if argv.is_empty() {
            return Err(RuntimeError::Protocol(
                "an exec command is required".to_owned(),
            ));
        }

        let mut arguments = vec![
            "exec".to_owned(),
            "--workdir".to_owned(),
            workdir.to_owned(),
        ];

        for (key, value) in env {
            validate_env(key, value)?;
            arguments.push("--env".to_owned());
            arguments.push(format!("{key}={value}"));
        }

        arguments.push(name.to_owned());
        arguments.extend(argv.iter().cloned());

        self.host
            .command(&self.docker_path, &self.arguments(arguments))
    }

    /// Inspect one container.
    pub async fn inspect(&self, name: &str) -> Result<ContainerState, RuntimeError> {
        validate_container_name(name)?;

        let raw = self
            .host
            .run(
                &self.docker_path,
                &self.arguments(vec![
                    "inspect".to_owned(),
                    "--format".to_owned(),
                    "{{json .State}}".to_owned(),
                    name.to_owned(),
                ]),
            )
            .await?;

        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| RuntimeError::Protocol(format!("invalid inspect: {error}")))?;

        Ok(ContainerState {
            status: value["Status"].as_str().unwrap_or("unknown").to_owned(),
            running: value["Running"].as_bool().unwrap_or(false),
            exit_code: value["ExitCode"].as_i64().unwrap_or(-1),
            oom_killed: value["OOMKilled"].as_bool().unwrap_or(false),
            error: value["Error"].as_str().map(str::to_owned),
        })
    }

    /// Tail a container's log, bounded by `tail` lines.
    pub async fn logs(&self, name: &str, tail: u32) -> Result<String, RuntimeError> {
        validate_container_name(name)?;

        self.host
            .run(
                &self.docker_path,
                &self.arguments(vec![
                    "logs".to_owned(),
                    "--tail".to_owned(),
                    tail.clamp(1, 5000).to_string(),
                    name.to_owned(),
                ]),
            )
            .await
    }

    /// Kill a container immediately.
    pub async fn kill(&self, name: &str) -> Result<(), RuntimeError> {
        validate_container_name(name)?;

        self.host
            .run(
                &self.docker_path,
                &self.arguments(vec!["kill".to_owned(), name.to_owned()]),
            )
            .await
            .map(|_| ())
    }

    /// Remove a container, ignoring an already absent one.
    pub async fn remove(&self, name: &str) -> Result<(), RuntimeError> {
        validate_container_name(name)?;

        let result = self
            .host
            .run(
                &self.docker_path,
                &self.arguments(vec!["rm".to_owned(), "--force".to_owned(), name.to_owned()]),
            )
            .await;

        match result {
            Ok(_) => Ok(()),
            Err(RuntimeError::Transport(message)) if message.contains("No such container") => {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// List containers carrying this instance's label.
    pub async fn list_managed(
        &self,
        instance: &str,
    ) -> Result<Vec<ManagedContainer>, RuntimeError> {
        validate_label(INSTANCE_LABEL, instance)?;

        let raw = self
            .host
            .run(
                &self.docker_path,
                &self.arguments(vec![
                    "ps".to_owned(),
                    "--all".to_owned(),
                    "--no-trunc".to_owned(),
                    "--filter".to_owned(),
                    format!("label={INSTANCE_LABEL}={instance}"),
                    "--format".to_owned(),
                    "{{json .}}".to_owned(),
                ]),
            )
            .await?;

        let mut containers = Vec::new();

        for line in raw.lines().filter(|line| !line.trim().is_empty()) {
            let value: Value = serde_json::from_str(line).map_err(|error| {
                RuntimeError::Protocol(format!("invalid container listing: {error}"))
            })?;

            let labels = value["Labels"].as_str().unwrap_or_default();

            let label = |key: &str| {
                labels
                    .split(',')
                    .find_map(|entry| entry.strip_prefix(&format!("{key}=")))
                    .map(str::to_owned)
            };

            containers.push(ManagedContainer {
                name: value["Names"].as_str().unwrap_or_default().to_owned(),
                id: value["ID"].as_str().unwrap_or_default().to_owned(),
                state: value["State"].as_str().unwrap_or_default().to_owned(),
                workspace: label(WORKSPACE_LABEL),
                job: label(JOB_LABEL),
                kind: label(KIND_LABEL),
            });
        }

        Ok(containers)
    }
}

/// Output of an attached run.
#[derive(Debug)]
pub struct AttachedRun {
    pub exit_code: i64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Server label names. Every resource created by this instance carries them.
pub const INSTANCE_LABEL: &str = "tech.alxprgs.workspace.instance";
pub const WORKSPACE_LABEL: &str = "tech.alxprgs.workspace.workspace";
pub const JOB_LABEL: &str = "tech.alxprgs.workspace.job";
pub const KIND_LABEL: &str = "tech.alxprgs.workspace.kind";

fn validate_container_name(name: &str) -> Result<(), RuntimeError> {
    let valid = (3..=63).contains(&name.len())
        && name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
        && name.starts_with(|character: char| character.is_ascii_alphanumeric());

    if !valid {
        return Err(RuntimeError::Protocol("invalid container name".to_owned()));
    }

    Ok(())
}

/// Syntax check for references used by internal image operations.
fn validate_reference(reference: &str) -> Result<(), RuntimeError> {
    let allowed = reference.len() <= 255
        && reference
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "/:._-@".contains(character))
        && (reference.contains(':') || reference.contains('@'));

    if !allowed {
        return Err(RuntimeError::Protocol("invalid image reference".to_owned()));
    }

    Ok(())
}

fn validate_image(reference: &str) -> Result<(), RuntimeError> {
    let allowed = reference
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "/:._-@".contains(character));

    // Either a registry digest reference, or one of this instance's own build
    // tags whose tag component is the immutable image identifier.
    let digest_pinned = reference.contains("@sha256:");
    let local_pinned = reference
        .strip_prefix("alxprgs-workspace/")
        .and_then(|rest| rest.rsplit_once(':'))
        .is_some_and(|(_, tag)| tag.len() == 64 && tag.chars().all(|c| c.is_ascii_hexdigit()));

    if reference.len() > 255 || !allowed || !(digest_pinned || local_pinned) {
        return Err(RuntimeError::Protocol(
            "images must be pinned references".to_owned(),
        ));
    }

    Ok(())
}

fn validate_path(path: &str) -> Result<(), RuntimeError> {
    let valid = path.starts_with('/')
        && path.len() <= MAX_PATH_BYTES
        && !path.contains("..")
        && !path.chars().any(char::is_control);

    if !valid {
        return Err(RuntimeError::Protocol("invalid runtime path".to_owned()));
    }

    Ok(())
}

fn validate_label(key: &str, value: &str) -> Result<(), RuntimeError> {
    let key_valid = !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".-_".contains(character));

    let value_valid = value.len() <= 200
        && !value.contains('=')
        && !value.chars().any(|character| character.is_control());

    if !key_valid || !value_valid {
        return Err(RuntimeError::Protocol("invalid container label".to_owned()));
    }

    Ok(())
}

fn validate_env(key: &str, value: &str) -> Result<(), RuntimeError> {
    let key_valid = !key.is_empty()
        && key.len() <= 128
        && key.chars().enumerate().all(|(index, character)| {
            if index == 0 {
                character.is_ascii_alphabetic() || character == '_'
            } else {
                character.is_ascii_alphanumeric() || character == '_'
            }
        });

    if !key_valid || value.contains('\0') || value.len() > 4096 {
        return Err(RuntimeError::Protocol(
            "invalid environment variable".to_owned(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ContainerSpec {
        ContainerSpec {
            name: "alxprgs-12345678-job-87654321".into(),
            image: "debian@sha256:8aa5a7bf5a1955bddbe7d74fcdd0b91ab30e74362a8b73af672b0481b35b44a0"
                .into(),
            labels: vec![
                (INSTANCE_LABEL.into(), "f32ac08f-5f8c-41df".into()),
                (KIND_LABEL.into(), "job".into()),
            ],
            workspace_dir: "/srv/alxprgs-workspace/data/workspaces/550e8400".into(),
            cache_dir: Some("/srv/alxprgs-workspace/data/workspaces/550e8400/cache".into()),
            argv: vec!["/bin/sh".into(), "-c".into(), "cargo build".into()],
            env: vec![("HOME".into(), "/tmp".into())],
            cpu_millis: 1000,
            memory_mib: 1024,
            pids: 128,
            tmpfs_mib: 64,
            network: false,
        }
    }

    #[test]
    fn hardening_flags_are_always_present() {
        let arguments = spec().arguments("create").unwrap();
        let joined = arguments.join(" ");

        for expected in [
            "--network none",
            "--read-only",
            "--cap-drop ALL",
            "--security-opt no-new-privileges",
            "--pids-limit 128",
            "--memory 1024m",
            "--memory-swap 1024m",
            "--user 0:0",
            "--log-driver local",
            "--mount type=bind,source=/srv/alxprgs-workspace/data/workspaces/550e8400,target=/workspace",
        ] {
            assert!(joined.contains(expected), "missing {expected}");
        }

        assert!(!joined.contains("--privileged"));
        assert!(!joined.contains("--network host"));
        assert!(!joined.contains("--pid host"));
        assert!(!joined.contains("docker.sock"));
        assert!(!joined.contains("--publish"));
    }

    #[test]
    fn images_must_be_digest_pinned() {
        let mut container = spec();
        container.image = "debian:13-slim".into();

        assert!(container.arguments("create").is_err());
    }

    #[test]
    fn user_input_cannot_inject_flags() {
        let mut container = spec();
        container.argv = vec!["--privileged".into()];

        let arguments = container.arguments("create").unwrap();

        // The value after the image is a command, not a Docker flag.
        let image_index = arguments
            .iter()
            .position(|argument| argument.starts_with("debian@sha256:"))
            .unwrap();

        assert_eq!(arguments[image_index + 1], "--privileged");
        assert!(!arguments[..image_index].contains(&"--privileged".to_owned()));
    }

    #[test]
    fn allowlist_networking_is_blocked_until_the_proxy_exists() {
        let mut container = spec();
        container.network = true;

        assert!(container.arguments("create").is_err());
    }

    #[test]
    fn engine_information_detects_rootless_and_seccomp() {
        let raw = serde_json::json!({
            "Name": "alxprgs-workspace",
            "ServerVersion": "29.10.1",
            "OSType": "linux",
            "SecurityOptions": ["name=seccomp,profile=builtin", "name=rootless", "name=cgroupns"],
            "CgroupVersion": "2",
            "CgroupDriver": "systemd",
            "DockerRootDir": "/home/workspace/.local/share/docker",
            "Driver": "overlayfs"
        })
        .to_string();

        let info = EngineInfo::parse(&raw).unwrap();

        assert!(info.rootless);
        assert!(info.seccomp);
        assert_eq!(info.cgroup_version, "2");
        assert_eq!(info.name, "alxprgs-workspace");

        let rootful = raw.replace("name=rootless", "name=selinux");
        assert!(!EngineInfo::parse(&rootful).unwrap().rootless);
    }

    #[test]
    fn invalid_container_names_are_rejected() {
        let mut container = spec();
        container.name = "bad name; rm -rf /".into();

        assert!(container.arguments("create").is_err());
    }
}
