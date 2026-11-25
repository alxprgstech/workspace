//! Host probes that populate [`RuntimeFacts`].
//!
//! The static probe deliberately performs no process execution: it only looks
//! at the filesystem and environment. Because of that it can never prove that
//! the engine enforces isolation, so it always reports `engine_verified:
//! false` and readiness stays closed until a live query runs.

use crate::{Platform, RuntimeFacts};

/// Source of host facts.
pub trait RuntimeProbe {
    fn facts(&self) -> RuntimeFacts;
}

/// Filesystem and environment based probe.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemProbe;

fn platform() -> Platform {
    match std::env::consts::OS {
        "windows" => Platform::Windows,
        "linux" => Platform::Linux,
        _ => Platform::Other,
    }
}

/// WSL2 is reachable when the `\\wsl$` share can be listed.
fn wsl2_available(platform: Platform) -> Option<bool> {
    if platform != Platform::Windows {
        return None;
    }

    match std::fs::read_dir(r"\\wsl$") {
        Ok(mut entries) => Some(entries.next().is_some()),
        Err(_) => Some(false),
    }
}

/// Locate the Docker client on the PATH, or accept an explicit endpoint.
fn docker_present() -> bool {
    if std::env::var_os("DOCKER_HOST").is_some() {
        return true;
    }

    let binary = if cfg!(windows) {
        "docker.exe"
    } else {
        "docker"
    };

    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };

    std::env::split_paths(&path)
        .map(|directory| directory.join(binary))
        .any(|candidate| candidate.is_file())
}

/// A user-scoped socket is the only rootless evidence available statically.
fn docker_rootless(platform: Platform) -> Option<bool> {
    if platform != Platform::Linux {
        return None;
    }

    let host = std::env::var("DOCKER_HOST").ok()?;

    host.starts_with("unix:///run/user/").then_some(true)
}

impl RuntimeProbe for SystemProbe {
    fn facts(&self) -> RuntimeFacts {
        let platform = platform();

        RuntimeFacts {
            platform,
            wsl2_available: wsl2_available(platform),
            docker_present: docker_present(),
            docker_rootless: docker_rootless(platform),
            // Static discovery cannot see daemon defaults.
            docker_privileged_default: false,
            engine_verified: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluate;

    #[test]
    fn static_discovery_never_claims_readiness() {
        let facts = SystemProbe.facts();

        // Whatever the host looks like, an unqueried engine is not readiness.
        // The concrete issue depends on the host, so only fail-closed matters.
        assert!(evaluate(&facts).is_err(), "{facts:?}");
    }

    #[test]
    fn probe_reports_the_real_platform() {
        let facts = SystemProbe.facts();

        let expected = match std::env::consts::OS {
            "windows" => Platform::Windows,
            "linux" => Platform::Linux,
            _ => Platform::Other,
        };

        assert_eq!(facts.platform, expected);
        assert!(!facts.engine_verified);
    }

    #[test]
    fn wsl_probe_is_windows_only() {
        assert_eq!(wsl2_available(Platform::Linux), None);
        assert_eq!(wsl2_available(Platform::Other), None);
        assert!(wsl2_available(Platform::Windows).is_some());
    }

    #[test]
    fn rootless_is_only_inferred_from_a_user_socket() {
        // Windows hosts have no rootless concept at this layer.
        assert_eq!(docker_rootless(Platform::Windows), None);

        // Linux is either unproven or proven rootless, never "rootful".
        assert!(matches!(
            docker_rootless(Platform::Linux),
            None | Some(true)
        ));
    }
}
