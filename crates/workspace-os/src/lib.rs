//! Narrow operating-system primitives used by the updater.
//!
//! The rest of the workspace forbids `unsafe` code. Release and update
//! operations need three Windows API calls that have no safe standard-library
//! equivalent:
//!
//! * DPAPI protection for the locally persisted authentication secrets;
//! * process liveness / image path checks so the updater can prove it is about
//!   to stop *its own* server before replacing files;
//! * free-space queries used by the preflight.
//!
//! Everything is kept in this crate, takes plain Rust values and never exposes
//! a raw handle or pointer. On non-Windows targets the functions either use
//! `/proc` or return [`OsError::Unsupported`].

#![allow(unsafe_code)]

pub mod disk;
pub mod dpapi;
pub mod process;

#[derive(Debug, thiserror::Error)]
pub enum OsError {
    #[error("unsupported on this platform")]
    Unsupported,

    #[error("operating system call failed: {0}")]
    System(String),

    #[error("invalid input: {0}")]
    Invalid(String),
}
