//! Transport to the dedicated Linux environment.
//!
//! On Windows every command runs through `wsl.exe` with an explicit
//! distribution and user, a clean environment and a fixed program. On Linux the
//! program runs directly. No shell is involved in either case.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use workspace_core::config::RuntimeBackendKind;
use workspace_core::config::RuntimeConfig;

use crate::{RuntimeBackend, RuntimeError, RuntimeSettings};

/// Programs the server is allowed to execute inside the dedicated environment.
///
/// This allowlist is the only path from the control plane to the host: it
/// prevents a compromised caller from executing an arbitrary binary by name.
pub const ALLOWED_PROGRAMS: &[&str] = &[
    "/usr/bin/docker",
    "/usr/sbin/setquota",
    "/usr/bin/findmnt",
    "/usr/bin/df",
    "/usr/bin/cat",
    "/usr/bin/uname",
    "/usr/bin/sudo",
    "/usr/bin/timeout",
    "/usr/bin/tee",
];

/// Runner for fixed commands inside the dedicated runtime.
#[derive(Debug, Clone)]
pub struct Host {
    backend: RuntimeBackend,
    startup_timeout: Duration,
    command_timeout: Duration,
}

impl Host {
    pub fn new(settings: &RuntimeSettings) -> Self {
        Self {
            backend: settings.backend.clone(),
            startup_timeout: settings.startup_timeout,
            command_timeout: settings.command_timeout,
        }
    }

    /// Build settings directly, validating the transport shape.
    pub fn from_config(config: &RuntimeConfig) -> Result<(Self, RuntimeSettings), RuntimeError> {
        if !matches!(config.backend, RuntimeBackendKind::Wsl2) && cfg!(windows) {
            return Err(RuntimeError::NotConfigured);
        }

        let settings = RuntimeSettings::from_config(config);

        Ok((Self::new(&settings), settings))
    }

    pub fn backend(&self) -> &RuntimeBackend {
        &self.backend
    }

    pub fn startup_timeout(&self) -> Duration {
        self.startup_timeout
    }

    pub fn command_timeout(&self) -> Duration {
        self.command_timeout
    }

    /// Build a command for an allowlisted program.
    pub fn command(&self, program: &str, args: &[String]) -> Result<Command, RuntimeError> {
        if !ALLOWED_PROGRAMS.contains(&program) {
            return Err(RuntimeError::Protocol(format!(
                "program {program} is not allowlisted"
            )));
        }

        Ok(self.build(program, args))
    }

    /// Build a command for the installed helper binary.
    ///
    /// The helper path comes from validated configuration, never from a caller.
    pub fn helper_command(
        &self,
        helper_path: &str,
        args: &[String],
    ) -> Result<Command, RuntimeError> {
        if !helper_path.starts_with('/') || helper_path.contains("..") {
            return Err(RuntimeError::Protocol(
                "helper path must be an absolute, closed path".to_owned(),
            ));
        }

        let mut command = self.build(helper_path, args);

        command.stdin(Stdio::piped());

        Ok(command)
    }

    /// Build a command whose stdin is a pipe, for bounded streaming uploads.
    pub fn stream_stdin_command(
        &self,
        program: &str,
        args: &[String],
    ) -> Result<Command, RuntimeError> {
        if !ALLOWED_PROGRAMS.contains(&program) {
            return Err(RuntimeError::Protocol(format!(
                "program {program} is not allowlisted"
            )));
        }

        let mut command = self.build(program, args);

        // Streaming programs must not echo the payload back: an unread stdout
        // pipe would fill and block the upload.
        command.stdin(Stdio::piped()).stdout(Stdio::null());

        Ok(command)
    }

    fn build(&self, program: &str, args: &[String]) -> Command {
        let mut command = match &self.backend {
            RuntimeBackend::Wsl2 { distro, user } => {
                let system_root = std::env::var_os("SystemRoot").unwrap_or_default();

                let wsl = PathBuf::from(system_root).join("System32").join("wsl.exe");

                let mut command = Command::new(wsl);

                command
                    .arg("-d")
                    .arg(distro)
                    .arg("-u")
                    .arg(user)
                    .arg("--cd")
                    .arg("/")
                    .arg("--exec")
                    .arg("/usr/bin/env")
                    .arg("-i")
                    .arg(format!("HOME=/home/{user}"))
                    .arg("PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
                    .arg("XDG_RUNTIME_DIR=/run/user/1000")
                    .arg(program)
                    .args(args);

                command
            }
            RuntimeBackend::RootlessDocker => {
                let mut command = Command::new(program);

                command
                    .env_clear()
                    .env("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
                    .env("HOME", "/root")
                    .args(args);

                command
            }
        };

        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        command
    }

    /// Run a short control command and return its standard output.
    pub async fn run(&self, program: &str, args: &[String]) -> Result<String, RuntimeError> {
        self.run_with_timeout(program, args, self.command_timeout)
            .await
    }

    /// Run a command with an explicit timeout.
    pub async fn run_with_timeout(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<String, RuntimeError> {
        let mut command = self.command(program, args)?;

        let output = tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| RuntimeError::Timeout(timeout.as_secs()))?
            .map_err(|error| RuntimeError::Transport(error.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);

            return Err(RuntimeError::Transport(format!(
                "{program} exited with {}: {}{}",
                output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_owned()),
                stderr.trim(),
                if stderr.trim().is_empty() {
                    stdout.trim().to_owned()
                } else {
                    String::new()
                }
            )));
        }

        String::from_utf8(output.stdout)
            .map(|text| text.trim_end_matches(['\r', '\n']).to_owned())
            .map_err(|_| RuntimeError::Protocol("command output is not UTF-8".into()))
    }

    /// Wait for the dedicated environment to answer, tolerating a cold start.
    ///
    /// A stopped WSL2 distribution and its rootless engine need several seconds
    /// before the socket accepts connections; every attempt is a bounded,
    /// read-only query.
    pub async fn wait_for_startup<F, Fut>(&self, mut attempt: F) -> Result<(), RuntimeError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<(), RuntimeError>>,
    {
        let deadline = tokio::time::Instant::now() + self.startup_timeout;

        loop {
            match attempt().await {
                Ok(()) => return Ok(()),
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(backend: RuntimeBackend) -> RuntimeSettings {
        RuntimeSettings {
            backend,
            socket: "unix:///run/user/1000/docker.sock".into(),
            docker_path: "/usr/bin/docker".into(),
            engine_name: "alxprgs-workspace".into(),
            data_root: "/srv/alxprgs-workspace/data".into(),
            helper_path: "/srv/alxprgs-workspace/bin/workspace-helper".into(),
            startup_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(5),
            workspace_storage_mib: 1024,
            workspace_files: 1000,
            minimum_free_mib: 100,
            base_image: "debian@sha256:00".into(),
        }
    }

    #[test]
    fn arbitrary_programs_are_rejected() {
        let host = Host::new(&settings(RuntimeBackend::RootlessDocker));

        assert!(matches!(
            host.command("/bin/sh", &["-c".into(), "id".into()]),
            Err(RuntimeError::Protocol(_))
        ));
        assert!(host.command("/usr/bin/docker", &[]).is_ok());
    }

    #[test]
    fn no_user_value_reaches_a_shell() {
        // Every argument is passed as a distinct argv entry; there is no shell
        // string to interpolate into.
        let host = Host::new(&settings(RuntimeBackend::RootlessDocker));

        let command = host
            .command("/usr/bin/docker", &["run".into(), "image; rm -rf /".into()])
            .unwrap();

        let arguments: Vec<_> = command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();

        assert_eq!(arguments, vec!["run", "image; rm -rf /"]);
    }
}
