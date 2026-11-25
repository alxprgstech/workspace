#![forbid(unsafe_code)]

//! Container runtime, job execution, storage and resource management.
//!
//! Stage 008 starts with runtime discovery: nothing here creates containers.
//! The evaluation is a pure function over observed facts so the fail-closed
//! rules can be tested without touching a host runtime.

mod probe;

pub use probe::{RuntimeProbe, SystemProbe};

/// Host family that determines which runtime shape is acceptable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Linux,
    Other,
}

/// Runtime shape that satisfies the isolation architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    /// Dedicated WSL2 distribution hosting its own Docker Engine.
    Wsl2,
    /// Dedicated rootless Docker Engine on Linux.
    RootlessDocker,
}

/// Facts collected by host probes.
///
/// `None` always means "could not be determined" and is treated as a failure
/// by [`evaluate`], never as an implicit success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeFacts {
    pub platform: Platform,
    /// Whether a WSL2 distribution is installed and running.
    pub wsl2_available: Option<bool>,
    /// Whether a Docker Engine binary is reachable at all.
    pub docker_present: bool,
    /// Whether the reachable engine runs rootless.
    pub docker_rootless: Option<bool>,
    /// Whether the engine would grant privileged containers by default.
    pub docker_privileged_default: bool,
    /// Whether the engine was actually queried, rather than merely found on
    /// the PATH. Static discovery can never prove isolation, so it leaves this
    /// false and readiness stays closed.
    pub engine_verified: bool,
}

/// Why a host cannot host Workspace workspaces yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeIssue {
    UnsupportedPlatform,
    /// Windows without a usable WSL2 distribution.
    Wsl2Missing,
    DockerMissing,
    /// Rootless mode is a hard requirement on Linux.
    RootfulDocker,
    /// Rootless state was not proven; assume the worst.
    RootlessUnknown,
    /// An engine that grants privileged containers is never acceptable.
    PrivilegedContainersAllowed,
    /// The engine was not queried directly, so its policies are unproven.
    EngineNotVerified,
}

impl RuntimeIssue {
    /// Operator-facing explanation; these strings end up in diagnostics.
    pub fn reason(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => {
                "unsupported host platform: only Windows with WSL2 and Linux are supported"
            }
            Self::Wsl2Missing => "WSL2 is not available; a dedicated WSL2 distribution is required",
            Self::DockerMissing => "no Docker Engine is reachable from the dedicated environment",
            Self::RootfulDocker => {
                "the Docker Engine runs as root; a dedicated rootless engine is required"
            }
            Self::RootlessUnknown => {
                "rootless mode could not be proven; refusing to assume isolation"
            }
            Self::PrivilegedContainersAllowed => {
                "the Docker Engine permits privileged containers; isolation cannot be guaranteed"
            }
            Self::EngineNotVerified => {
                "the Docker Engine has not been queried directly; isolation is unproven"
            }
        }
    }
}

/// Decide whether the host may host workspaces.
///
/// The function fails closed: every uncertainty resolves to an issue.
pub fn evaluate(facts: &RuntimeFacts) -> Result<RuntimeKind, RuntimeIssue> {
    if facts.docker_privileged_default {
        return Err(RuntimeIssue::PrivilegedContainersAllowed);
    }

    let kind = match facts.platform {
        Platform::Windows => {
            if facts.wsl2_available != Some(true) {
                return Err(RuntimeIssue::Wsl2Missing);
            }

            if !facts.docker_present {
                return Err(RuntimeIssue::DockerMissing);
            }

            // Windows relies on the WSL2 boundary; a rootful engine inside the
            // dedicated distribution is still confined to it, but we only
            // accept it when rootless state is not contradicted.
            if facts.docker_rootless == Some(false) {
                return Err(RuntimeIssue::RootfulDocker);
            }

            RuntimeKind::Wsl2
        }
        Platform::Linux => {
            if !facts.docker_present {
                return Err(RuntimeIssue::DockerMissing);
            }

            match facts.docker_rootless {
                Some(true) => RuntimeKind::RootlessDocker,
                Some(false) => return Err(RuntimeIssue::RootfulDocker),
                None => return Err(RuntimeIssue::RootlessUnknown),
            }
        }
        Platform::Other => return Err(RuntimeIssue::UnsupportedPlatform),
    };

    // Static discovery can describe an engine but never prove its policies.
    if !facts.engine_verified {
        return Err(RuntimeIssue::EngineNotVerified);
    }

    Ok(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(platform: Platform) -> RuntimeFacts {
        RuntimeFacts {
            platform,
            wsl2_available: None,
            docker_present: false,
            docker_rootless: None,
            docker_privileged_default: false,
            engine_verified: false,
        }
    }

    #[test]
    fn windows_requires_a_running_wsl2_distribution() {
        let mut host = facts(Platform::Windows);

        assert_eq!(evaluate(&host), Err(RuntimeIssue::Wsl2Missing));

        host.wsl2_available = Some(false);
        assert_eq!(evaluate(&host), Err(RuntimeIssue::Wsl2Missing));

        host.wsl2_available = Some(true);
        assert_eq!(evaluate(&host), Err(RuntimeIssue::DockerMissing));

        host.docker_present = true;
        assert_eq!(evaluate(&host), Err(RuntimeIssue::EngineNotVerified));

        host.engine_verified = true;
        assert_eq!(evaluate(&host), Ok(RuntimeKind::Wsl2));

        host.docker_rootless = Some(false);
        assert_eq!(evaluate(&host), Err(RuntimeIssue::RootfulDocker));
    }

    #[test]
    fn linux_requires_a_proven_rootless_engine() {
        let mut host = facts(Platform::Linux);

        assert_eq!(evaluate(&host), Err(RuntimeIssue::DockerMissing));

        host.docker_present = true;

        // Unknown is a failure, never an implicit pass.
        assert_eq!(evaluate(&host), Err(RuntimeIssue::RootlessUnknown));

        host.docker_rootless = Some(false);
        assert_eq!(evaluate(&host), Err(RuntimeIssue::RootfulDocker));

        host.docker_rootless = Some(true);
        assert_eq!(evaluate(&host), Err(RuntimeIssue::EngineNotVerified));

        host.engine_verified = true;
        assert_eq!(evaluate(&host), Ok(RuntimeKind::RootlessDocker));
    }

    #[test]
    fn privileged_engines_are_rejected_even_when_otherwise_ready() {
        for platform in [Platform::Windows, Platform::Linux] {
            let host = RuntimeFacts {
                platform,
                wsl2_available: Some(true),
                docker_present: true,
                docker_rootless: Some(true),
                docker_privileged_default: true,
                engine_verified: true,
            };

            assert_eq!(
                evaluate(&host),
                Err(RuntimeIssue::PrivilegedContainersAllowed),
                "{platform:?}"
            );
        }
    }

    #[test]
    fn unsupported_platforms_fail_closed() {
        let host = RuntimeFacts {
            platform: Platform::Other,
            wsl2_available: Some(true),
            docker_present: true,
            docker_rootless: Some(true),
            docker_privileged_default: false,
            engine_verified: true,
        };

        assert_eq!(evaluate(&host), Err(RuntimeIssue::UnsupportedPlatform));
    }

    #[test]
    fn every_issue_explains_itself() {
        for issue in [
            RuntimeIssue::UnsupportedPlatform,
            RuntimeIssue::Wsl2Missing,
            RuntimeIssue::DockerMissing,
            RuntimeIssue::RootfulDocker,
            RuntimeIssue::RootlessUnknown,
            RuntimeIssue::PrivilegedContainersAllowed,
        ] {
            assert!(!issue.reason().is_empty());
        }
    }
}
