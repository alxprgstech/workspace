#![forbid(unsafe_code)]

//! Core domain models and policies for ALXPRGS Workspace.

pub const PRODUCT_NAME: &str = "ALXPRGS Workspace";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_metadata_is_available() {
        assert_eq!(PRODUCT_NAME, "ALXPRGS Workspace");
        assert!(!VERSION.is_empty());
    }
}

pub mod capabilities;
pub mod config;
pub mod helper;
pub mod job;
pub mod patch;
pub mod workspace;
