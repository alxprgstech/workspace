#![forbid(unsafe_code)]

//! Core domain models and policies for ALXPRGS Workspace.

pub const PRODUCT_NAME: &str = "ALXPRGS Workspace";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Full commit SHA the binary was built from, with a `-dirty` suffix when the
/// working tree had uncommitted changes.
pub const GIT_SHA: &str = env!("WORKSPACE_BUILD_SHA");

/// Build timestamp in the `unix:<seconds>` form (or a value supplied by the
/// release script). Informational only.
pub const BUILD_TIME: &str = env!("WORKSPACE_BUILD_TIME");

/// First 12 characters of [`GIT_SHA`], suitable for compact displays.
pub fn short_sha() -> &'static str {
    let sha = GIT_SHA.trim();
    let end = sha
        .char_indices()
        .nth(12)
        .map(|(index, _)| index)
        .unwrap_or(sha.len());

    &sha[..end]
}

/// Whether this build came from a working tree with uncommitted changes.
pub fn is_dirty_build() -> bool {
    GIT_SHA.ends_with("-dirty")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_metadata_is_available() {
        assert_eq!(PRODUCT_NAME, "ALXPRGS Workspace");
        assert!(!VERSION.is_empty());
        assert!(!GIT_SHA.is_empty());
        assert!(!short_sha().is_empty());
    }
}

pub mod capabilities;
pub mod config;
pub mod helper;
pub mod job;
pub mod patch;
pub mod workspace;
