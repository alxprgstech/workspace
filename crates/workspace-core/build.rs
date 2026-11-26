//! Embed the source revision and build time into every binary.
//!
//! The updater must be able to report which build is actually installed, and a
//! release manifest is only meaningful when it names the exact commit it was
//! built from. `git` is optional: a tarball build falls back to `unknown`
//! unless the release script provides the metadata explicitly.

use std::process::Command;

fn git(arguments: &[&str]) -> Option<String> {
    let output = Command::new("git").args(arguments).output().ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8(output.stdout).ok()?;
    let value = text.trim();

    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn main() {
    for name in [
        "WORKSPACE_BUILD_SHA",
        "WORKSPACE_BUILD_TIME",
        "SOURCE_DATE_EPOCH",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }

    // A changed HEAD or index means the embedded revision is stale. The whole
    // `.git` directory is not watched: it changes on every git command.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    let sha = std::env::var("WORKSPACE_BUILD_SHA")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            let commit = git(&["rev-parse", "HEAD"])?;
            let dirty = git(&["status", "--porcelain"]).is_some_and(|status| !status.is_empty());

            Some(if dirty {
                format!("{commit}-dirty")
            } else {
                commit
            })
        })
        .unwrap_or_else(|| "unknown".to_owned());

    let build_time = std::env::var("WORKSPACE_BUILD_TIME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("SOURCE_DATE_EPOCH")
                .ok()
                .map(|epoch| format!("unix:{epoch}"))
        })
        .unwrap_or_else(|| {
            format!(
                "unix:{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or_default()
            )
        });

    println!("cargo:rustc-env=WORKSPACE_BUILD_SHA={sha}");
    println!("cargo:rustc-env=WORKSPACE_BUILD_TIME={build_time}");
}
