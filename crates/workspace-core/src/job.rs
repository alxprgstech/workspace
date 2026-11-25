//! Job domain model.

use serde::{Deserialize, Serialize};

/// Job lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Interrupted,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "timed_out" => Self::TimedOut,
            "interrupted" => Self::Interrupted,
            _ => return None,
        })
    }

    /// Whether the job reached a terminal state.
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

/// One job as stored and returned to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub workspace_id: String,
    pub state: JobState,
    pub argv: Vec<String>,
    pub cwd: String,
    pub exit_code: Option<i64>,
    pub created_at_unix: i64,
    pub started_at_unix: Option<i64>,
    pub finished_at_unix: Option<i64>,
    pub timeout_seconds: u64,
    pub error: Option<String>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl Job {
    /// Wall-clock duration, once the job started.
    pub fn duration_ms(&self) -> Option<u64> {
        let started = self.started_at_unix?;
        let finished = self.finished_at_unix.unwrap_or_else(now);

        Some(((finished - started).max(0) as u64) * 1000)
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

/// Validate an argv vector supplied by a client.
///
/// A command is a list of arguments, never a shell string. Empty commands,
/// NUL bytes and oversized arguments are rejected; anything longer belongs in
/// a script file inside the workspace.
pub fn validate_argv(argv: &[String]) -> Result<(), &'static str> {
    if argv.is_empty() {
        return Err("argv must contain at least the program name");
    }

    if argv.len() > 256 {
        return Err("argv contains too many arguments");
    }

    let mut total = 0usize;

    for argument in argv {
        if argument.contains('\0') {
            return Err("argv must not contain NUL bytes");
        }

        if argument.len() > 4096 {
            return Err("a single argument is longer than 4096 bytes");
        }

        total += argument.len();
    }

    if total > 64 * 1024 {
        return Err("argv is larger than 64 KiB");
    }

    Ok(())
}

/// Validate a workspace-relative working directory.
pub fn validate_cwd(cwd: &str) -> Result<(), &'static str> {
    if cwd.len() > 1024 || cwd.contains('\0') || cwd.chars().any(char::is_control) {
        return Err("invalid working directory");
    }

    if cwd.starts_with('/') || cwd.split(['/', '\\']).any(|part| part == "..") {
        return Err("working directory must stay inside the workspace");
    }

    Ok(())
}

/// Validate environment variables supplied by a client.
pub fn validate_env(env: &[(String, String)]) -> Result<(), &'static str> {
    if env.len() > 64 {
        return Err("too many environment variables");
    }

    let mut total = 0usize;

    for (key, value) in env {
        let valid_key = !key.is_empty()
            && key.len() <= 128
            && key.chars().enumerate().all(|(index, character)| {
                if index == 0 {
                    character.is_ascii_alphabetic() || character == '_'
                } else {
                    character.is_ascii_alphanumeric() || character == '_'
                }
            });

        if !valid_key {
            return Err("invalid environment variable name");
        }

        if value.contains('\0') || value.len() > 8192 {
            return Err("invalid environment variable value");
        }

        total += key.len() + value.len();
    }

    if total > 32 * 1024 {
        return Err("environment is larger than 32 KiB");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip() {
        for state in [
            JobState::Queued,
            JobState::Running,
            JobState::Succeeded,
            JobState::Failed,
            JobState::Cancelled,
            JobState::TimedOut,
            JobState::Interrupted,
        ] {
            assert_eq!(JobState::parse(state.as_str()), Some(state));
            assert_eq!(
                state.is_terminal(),
                !matches!(state, JobState::Queued | JobState::Running)
            );
        }
    }

    #[test]
    fn argv_is_a_list_not_a_shell_string() {
        assert!(validate_argv(&["cargo".into(), "build".into()]).is_ok());
        assert!(validate_argv(&[]).is_err());
        assert!(validate_argv(&["sh".into(), "-c".into(), "a\0b".into()]).is_err());
        assert!(validate_argv(&["x".repeat(5000)]).is_err());
    }

    #[test]
    fn working_directories_stay_inside_the_workspace() {
        assert!(validate_cwd("").is_ok());
        assert!(validate_cwd("src").is_ok());
        assert!(validate_cwd("../etc").is_err());
        assert!(validate_cwd("/etc").is_err());
        assert!(validate_cwd("a/../../b").is_err());
    }

    #[test]
    fn environment_is_validated() {
        assert!(validate_env(&[("HOME".into(), "/tmp".into())]).is_ok());
        assert!(validate_env(&[("1BAD".into(), "x".into())]).is_err());
        assert!(validate_env(&[("BAD-NAME".into(), "x".into())]).is_err());
        assert!(validate_env(&[("OK".into(), "a\0b".into())]).is_err());
    }
}
