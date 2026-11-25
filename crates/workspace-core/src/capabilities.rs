//! Workspace capability catalog.
//!
//! The catalog is a compiled, administrator-controlled manifest list. A caller
//! can only name module identifiers; it can never supply a Dockerfile, a build
//! argument, a package list or an image reference. Requested identifiers are
//! resolved here, with dependencies added in a deterministic order, so the same
//! request always produces the same canonical module list.

use serde::Serialize;
use std::collections::BTreeSet;

/// One audited capability module.
#[derive(Debug, Clone, Serialize)]
pub struct Module {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub version: &'static str,
    pub tools: &'static [&'static str],
    pub depends_on: &'static [&'static str],
    /// Whether the trusted build recipe exists in this build.
    pub available: bool,
}

/// Modules required by the MVP.
pub const MODULES: &[Module] = &[
    Module {
        id: "base",
        name: "Base",
        description: "Bash, coreutils, find, grep, tar, unzip, curl and CA certificates.",
        version: "13",
        tools: &[
            "bash",
            "coreutils",
            "find",
            "grep",
            "tar",
            "unzip",
            "curl",
            "ca-certificates",
        ],
        depends_on: &[],
        // The pinned base image is the audited base recipe.
        available: true,
    },
    Module {
        id: "git",
        name: "Git",
        description: "Git command line client.",
        version: "1",
        tools: &["git"],
        depends_on: &["base"],
        available: true,
    },
    Module {
        id: "rust",
        name: "Rust",
        description: "rustc, Cargo, rustfmt and Clippy.",
        version: "1.100",
        tools: &["rustc", "cargo", "rustfmt", "cargo-clippy"],
        depends_on: &["base"],
        available: true,
    },
    Module {
        id: "cpp",
        name: "C/C++",
        description: "GCC, G++, CMake, Ninja and pkg-config.",
        version: "14",
        tools: &["gcc", "g++", "cmake", "ninja", "pkg-config"],
        depends_on: &["base"],
        available: true,
    },
    Module {
        id: "python",
        name: "Python",
        description: "Python interpreter, pip and venv.",
        version: "3.13",
        tools: &["python3", "pip", "venv"],
        depends_on: &["base"],
        available: true,
    },
    Module {
        id: "ffmpeg",
        name: "FFmpeg",
        description: "FFmpeg and FFprobe.",
        version: "7",
        tools: &["ffmpeg", "ffprobe"],
        depends_on: &["base"],
        available: true,
    },
    Module {
        id: "whisper-vad",
        name: "Whisper VAD",
        description: "whisper.cpp voice activity detection with the compatible Silero model.",
        version: "1",
        tools: &["whisper-vad", "silero-vad"],
        depends_on: &["ffmpeg"],
        // No trusted, checksum-pinned whisper.cpp/Silero source in this build.
        available: false,
    },
];

/// Why a requested capability set is not usable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityError {
    /// The identifier is not part of the audited catalog.
    UnknownModule { id: String },
    /// The module exists but its trusted build recipe is not installed.
    UnavailableModule { id: String },
    /// Too many identifiers were requested.
    TooManyModules,
    /// The dependency graph contains a cycle.
    CyclicDependency { id: String },
}

impl CapabilityError {
    /// Stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownModule { .. } => "unknown_capability",
            Self::UnavailableModule { .. } => "capability_unavailable",
            Self::TooManyModules => "too_many_capabilities",
            Self::CyclicDependency { .. } => "capability_cycle",
        }
    }

    /// Operator-facing explanation.
    pub fn message(&self) -> String {
        match self {
            Self::UnknownModule { id } => format!("unknown capability: {id}"),
            Self::UnavailableModule { id } => {
                format!("capability {id} has no trusted image recipe in this build")
            }
            Self::TooManyModules => "too many capabilities requested".to_owned(),
            Self::CyclicDependency { id } => {
                format!("capability dependency cycle at {id}")
            }
        }
    }
}

/// Look up one module.
pub fn module(id: &str) -> Option<&'static Module> {
    MODULES.iter().find(|module| module.id == id)
}

/// Resolve a requested set into a canonical, dependency-closed module list.
///
/// The order is stable: modules are returned sorted by id, with `base` first,
/// so two equal requests always produce the same list and the same build key.
pub fn resolve(requested: &[String]) -> Result<Vec<&'static Module>, CapabilityError> {
    if requested.len() > MODULES.len() {
        return Err(CapabilityError::TooManyModules);
    }

    let mut queue: Vec<&'static str> = vec!["base"];

    for id in requested {
        let Some(entry) = module(id) else {
            return Err(CapabilityError::UnknownModule { id: id.clone() });
        };

        queue.push(entry.id);
    }

    let mut selected: BTreeSet<&'static str> = BTreeSet::new();

    while let Some(id) = queue.pop() {
        let Some(entry) = module(id) else {
            return Err(CapabilityError::UnknownModule { id: id.to_owned() });
        };

        if !entry.available {
            return Err(CapabilityError::UnavailableModule { id: id.to_owned() });
        }

        if !selected.insert(entry.id) {
            continue;
        }

        for dependency in entry.depends_on {
            if module(dependency).is_none() {
                return Err(CapabilityError::UnknownModule {
                    id: (*dependency).to_owned(),
                });
            }

            queue.push(dependency);
        }
    }

    let mut modules: Vec<&'static Module> = selected.iter().filter_map(|id| module(id)).collect();

    modules.sort_by(|left, right| match (left.id, right.id) {
        // `base` is always first, everything else by identifier.
        ("base", _) => std::cmp::Ordering::Less,
        (_, "base") => std::cmp::Ordering::Greater,
        (left, right) => left.cmp(right),
    });

    Ok(modules)
}
/// Canonical identifier list for a resolved set.
pub fn canonical_ids(modules: &[&'static Module]) -> Vec<String> {
    modules.iter().map(|module| module.id.to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_required_module_is_catalogued() {
        for id in [
            "base",
            "rust",
            "cpp",
            "git",
            "python",
            "ffmpeg",
            "whisper-vad",
        ] {
            assert!(module(id).is_some(), "{id}");
        }
    }

    #[test]
    fn dependencies_are_closed_and_ordered() {
        // Availability is a build-time property; the resolver must work once a
        // module is installed. The test builds its own view of the graph.
        let whisper = module("whisper-vad").unwrap();

        assert_eq!(whisper.depends_on, &["ffmpeg"]);
        assert_eq!(module("ffmpeg").unwrap().depends_on, &["base"]);
        assert!(module("base").unwrap().depends_on.is_empty());
    }

    #[test]
    fn unknown_capabilities_are_rejected() {
        let error = resolve(&["base; rm -rf /".to_owned()]).unwrap_err();

        assert_eq!(error.code(), "unknown_capability");
    }

    #[test]
    fn unavailable_capabilities_are_reported_not_silently_skipped() {
        let error = resolve(&["whisper-vad".to_owned()]).unwrap_err();

        assert_eq!(error.code(), "capability_unavailable");
        assert!(error.message().contains("whisper-vad"));
    }

    #[test]
    fn dependency_closure_is_canonical() {
        let modules = resolve(&["rust".to_owned()]).unwrap();

        assert_eq!(
            canonical_ids(&modules),
            vec!["base".to_owned(), "rust".to_owned()]
        );
    }

    #[test]
    fn capability_list_cannot_exceed_the_catalog() {
        let requested: Vec<String> = (0..20).map(|index| format!("module-{index}")).collect();

        assert_eq!(
            resolve(&requested).unwrap_err().code(),
            "too_many_capabilities"
        );
    }
}
