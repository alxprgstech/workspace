//! Capability-scoped filesystem access.
//!
//! Every operation resolves a relative path inside a [`SafeRoot`], which wraps
//! a `cap-std` directory handle. `cap-std` resolves paths with openat-style
//! semantics and refuses escapes: `..`, absolute components and symlinks that
//! leave the capability all fail instead of touching the host filesystem.

use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use workspace_core::helper::{self, HelperError};

/// An opened capability root.
pub struct SafeRoot {
    dir: Dir,
}

/// One directory entry returned by [`SafeRoot::list`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct Entry {
    pub path: String,
    pub name: String,
    pub kind: &'static str,
    pub size: u64,
    pub modified_unix: Option<i64>,
}

/// Recursive size information.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Usage {
    pub bytes: u64,
    pub files: u64,
    pub directories: u64,
    pub truncated: bool,
}

impl SafeRoot {
    /// Open a root directory, refusing any root outside the allowed prefixes.
    pub fn open(prefixes: &[String], root: &str) -> Result<Self, HelperError> {
        helper::validate_root(root)?;

        let normalized = normalize(root);

        if !prefixes.iter().any(|prefix| is_within(&normalized, prefix)) {
            return Err(HelperError::new(
                helper::code::PATH_ESCAPE,
                "root is outside the allowed prefixes",
            ));
        }

        let dir = Dir::open_ambient_dir(&normalized, ambient_authority()).map_err(map_io)?;

        if !dir.dir_metadata().map_err(map_io)?.is_dir() {
            return Err(HelperError::new(
                helper::code::NOT_A_DIRECTORY,
                "root is not a directory",
            ));
        }

        Ok(Self { dir })
    }

    /// Metadata without following the final symlink.
    pub fn stat(&self, path: &str) -> Result<serde_json::Value, HelperError> {
        helper::validate_relative_path(path)?;

        if path.is_empty() {
            return Err(HelperError::new(
                helper::code::PATH_INVALID,
                "a path is required",
            ));
        }

        let metadata = self.dir.symlink_metadata(path).map_err(map_io)?;

        Ok(serde_json::json!({
            "kind": kind_of(&metadata),
            "size": metadata.len(),
            "modified_unix": modified_unix(&metadata),
        }))
    }

    /// List a directory, optionally recursive.
    pub fn list(
        &self,
        path: &str,
        depth: u32,
        limit: u32,
    ) -> Result<(Vec<Entry>, bool), HelperError> {
        helper::validate_relative_path(path)?;

        let limit = limit.clamp(1, helper::MAX_LIST_ENTRIES) as usize;
        let depth = depth.min(32);

        let start = if path.is_empty() {
            self.dir.try_clone().map_err(map_io)?
        } else {
            self.dir.open_dir(path).map_err(map_io)?
        };

        let mut entries = Vec::new();
        let mut truncated = false;

        // Breadth-first walk so the shallow entries are always returned first.
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((start, String::new(), 0u32));

        while let Some((directory, prefix, current_depth)) = queue.pop_front() {
            let mut children: Vec<(String, String, Metadata)> = Vec::new();

            let iterator = directory.entries().map_err(map_io)?;

            for entry in iterator {
                let entry = entry.map_err(map_io)?;

                let name = entry.file_name();

                let Some(name) = name.to_str() else {
                    // The protocol carries UTF-8 paths; non-UTF-8 names are not
                    // addressable and are skipped rather than mangled.
                    continue;
                };

                if name == "." || name == ".." {
                    continue;
                }

                let child_path = join(&prefix, name);
                let metadata = entry.metadata().map_err(map_io)?;

                children.push((child_path, name.to_owned(), metadata));
            }

            children.sort_by(|left, right| left.1.cmp(&right.1));

            for (child_path, name, metadata) in children {
                if entries.len() >= limit {
                    truncated = true;
                    break;
                }

                entries.push(Entry {
                    path: child_path.clone(),
                    name,
                    kind: kind_of(&metadata),
                    size: metadata.len(),
                    modified_unix: modified_unix(&metadata),
                });

                if metadata.is_dir()
                    && current_depth < depth
                    && let Ok(child) = directory.open_dir(&child_path)
                {
                    queue.push_back((child, child_path, current_depth + 1));
                }
            }

            if truncated {
                break;
            }
        }

        Ok((entries, truncated))
    }

    /// Open a regular file for reading, reporting its size.
    pub fn open_regular(&self, path: &str) -> Result<(File, Metadata), HelperError> {
        helper::validate_relative_path(path)?;

        if path.is_empty() {
            return Err(HelperError::new(
                helper::code::PATH_INVALID,
                "a path is required",
            ));
        }

        let file = self.dir.open(path).map_err(map_io)?;
        let metadata = file.metadata().map_err(map_io)?;

        if metadata.is_dir() {
            return Err(HelperError::new(
                helper::code::NOT_A_FILE,
                "path is a directory",
            ));
        }

        if !metadata.is_file() {
            return Err(HelperError::new(
                helper::code::NOT_A_FILE,
                "path is not a regular file",
            ));
        }

        Ok((file, metadata))
    }

    /// Create a directory.
    pub fn mkdir(&self, path: &str, parents: bool) -> Result<(), HelperError> {
        helper::validate_relative_path(path)?;

        if path.is_empty() {
            return Err(HelperError::new(
                helper::code::PATH_INVALID,
                "a path is required",
            ));
        }

        if parents {
            self.dir.create_dir_all(path).map_err(map_io)
        } else {
            self.dir.create_dir(path).map_err(map_io)
        }
    }

    /// Remove a file or directory.
    pub fn remove(&self, path: &str, recursive: bool) -> Result<serde_json::Value, HelperError> {
        helper::validate_relative_path(path)?;

        if path.is_empty() {
            return Err(HelperError::new(
                helper::code::PATH_INVALID,
                "refusing to remove the root itself",
            ));
        }

        let metadata = self.dir.symlink_metadata(path).map_err(map_io)?;

        if metadata.is_dir() {
            if recursive {
                self.dir.remove_dir_all(path).map_err(map_io)?;
            } else {
                self.dir.remove_dir(path).map_err(map_io)?;
            }
        } else {
            self.dir.remove_file(path).map_err(map_io)?;
        }

        Ok(serde_json::json!({ "removed": kind_of(&metadata) }))
    }

    /// Exclusive create of a temporary file next to `path`.
    fn create_temp(&self, path: &str) -> Result<(String, File), HelperError> {
        for attempt in 0..8 {
            let candidate = temp_path(path, attempt);

            let mut options = OpenOptions::new();
            options.write(true).create_new(true);

            match self.dir.open_with(&candidate, &options) {
                Ok(file) => return Ok((candidate, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(map_io(error)),
            }
        }

        Err(HelperError::new(
            helper::code::INTERNAL,
            "cannot allocate a temporary file",
        ))
    }

    /// Write a file atomically and return its digest.
    pub fn write(
        &self,
        path: &str,
        content: &[u8],
        create_parents: bool,
        replace: bool,
        expected_sha256: Option<&str>,
    ) -> Result<serde_json::Value, HelperError> {
        helper::validate_relative_path(path)?;

        if path.is_empty() {
            return Err(HelperError::new(
                helper::code::PATH_INVALID,
                "a path is required",
            ));
        }

        if let Some(parent) = parent_of(path)
            && create_parents
            && !parent.is_empty()
        {
            self.dir.create_dir_all(parent).map_err(map_io)?;
        }

        if expected_sha256.is_some() {
            self.verify_expected(path, expected_sha256)?;
        } else if !replace {
            match self.dir.try_exists(path) {
                Ok(true) => {
                    return Err(HelperError::new(
                        helper::code::EXISTS,
                        "file already exists",
                    ));
                }
                Ok(false) => {}
                Err(error) => return Err(map_io(error)),
            }
        }

        let digest = hex(&sha256(content));

        let (temporary, mut file) = self.create_temp(path)?;

        let result = file.write_all(content).and_then(|()| file.sync_all());

        drop(file);

        if let Err(error) = result {
            let _ = self.dir.remove_file(&temporary);
            return Err(map_io(error));
        }

        if let Err(error) = self.dir.rename(&temporary, &self.dir, path) {
            let _ = self.dir.remove_file(&temporary);
            return Err(map_io(error));
        }

        Ok(serde_json::json!({
            "sha256": digest,
            "size": content.len() as u64,
        }))
    }

    /// Verify that an existing file matches an expected digest.
    pub fn verify_expected(
        &self,
        path: &str,
        expected_sha256: Option<&str>,
    ) -> Result<(), HelperError> {
        let Some(expected) = expected_sha256 else {
            return Ok(());
        };

        let (mut file, _) = self.open_regular(path).map_err(|error| {
            if error.code == helper::code::NOT_FOUND {
                HelperError::new(helper::code::VERSION_MISMATCH, "file does not exist")
            } else {
                error
            }
        })?;

        let actual = digest_reader(&mut file)?;

        if !actual.eq_ignore_ascii_case(expected) {
            return Err(HelperError::new(
                helper::code::VERSION_MISMATCH,
                "file digest does not match the expected value",
            ));
        }

        Ok(())
    }

    /// Copy a regular file inside the capability.
    pub fn copy(
        &self,
        source_path: &str,
        target_path: &str,
        replace: bool,
        expected_sha256: Option<&str>,
    ) -> Result<serde_json::Value, HelperError> {
        helper::validate_relative_path(target_path)?;
        self.verify_expected(source_path, expected_sha256)?;

        if target_path.is_empty() {
            return Err(HelperError::new(
                helper::code::PATH_INVALID,
                "a target path is required",
            ));
        }

        if !replace && self.dir.try_exists(target_path).unwrap_or(false) {
            return Err(HelperError::new(
                helper::code::EXISTS,
                "target already exists",
            ));
        }

        let (mut source, metadata) = self.open_regular(source_path)?;

        if metadata.len() > MAX_COPY_BYTES {
            return Err(HelperError::new(
                helper::code::TOO_LARGE,
                "file exceeds the maximum copy size",
            ));
        }

        let (temporary, mut target) = self.create_temp(target_path)?;

        let mut hasher = Sha256::new();
        let mut copied = 0u64;
        let mut buffer = vec![0u8; 64 * 1024];

        let result = (|| -> Result<(), HelperError> {
            loop {
                let read = source.read(&mut buffer).map_err(map_io)?;

                if read == 0 {
                    break;
                }

                hasher.update(&buffer[..read]);
                target.write_all(&buffer[..read]).map_err(map_io)?;
                copied += read as u64;

                if copied > MAX_COPY_BYTES {
                    return Err(HelperError::new(
                        helper::code::TOO_LARGE,
                        "file exceeds the maximum copy size",
                    ));
                }
            }

            target.sync_all().map_err(map_io)
        })();

        drop(target);

        if let Err(error) = result {
            let _ = self.dir.remove_file(&temporary);
            return Err(error);
        }

        if let Err(error) = self.dir.rename(&temporary, &self.dir, target_path) {
            let _ = self.dir.remove_file(&temporary);
            return Err(map_io(error));
        }

        Ok(serde_json::json!({
            "sha256": hex(&hasher.finalize()),
            "size": copied,
        }))
    }

    /// Compute the SHA-256 of a regular file.
    pub fn sha256(&self, path: &str) -> Result<serde_json::Value, HelperError> {
        let (mut file, metadata) = self.open_regular(path)?;
        let digest = digest_reader(&mut file)?;

        Ok(serde_json::json!({
            "sha256": digest,
            "size": metadata.len(),
        }))
    }

    /// Recursive size and file count, bounded by `max_entries`.
    pub fn usage(&self, path: &str, max_entries: u32) -> Result<Usage, HelperError> {
        helper::validate_relative_path(path)?;

        let max_entries = max_entries.clamp(1, 1_000_000) as u64;

        let start = if path.is_empty() {
            self.dir.try_clone().map_err(map_io)?
        } else {
            self.dir.open_dir(path).map_err(map_io)?
        };

        let mut usage = Usage {
            bytes: 0,
            files: 0,
            directories: 0,
            truncated: false,
        };

        let mut stack = vec![(start, String::new())];
        let mut visited = 0u64;

        while let Some((directory, prefix)) = stack.pop() {
            let iterator = directory.entries().map_err(map_io)?;

            for entry in iterator {
                let entry = entry.map_err(map_io)?;

                let name = entry.file_name();

                let Some(name) = name.to_str() else {
                    continue;
                };

                if name == "." || name == ".." {
                    continue;
                }

                visited += 1;

                if visited > max_entries {
                    usage.truncated = true;
                    return Ok(usage);
                }

                let child_path = join(&prefix, name);
                let metadata = entry.metadata().map_err(map_io)?;

                if metadata.is_dir() {
                    usage.directories += 1;

                    if let Ok(child) = directory.open_dir(&child_path) {
                        stack.push((child, child_path));
                    }
                } else {
                    usage.files += 1;
                    usage.bytes = usage.bytes.saturating_add(metadata.len());
                }
            }
        }

        Ok(usage)
    }
}

/// Copy limit; larger files must use the chunked import path instead.
const MAX_COPY_BYTES: u64 = 2 * 1024 * 1024 * 1024;

use sha2::{Digest, Sha256};

fn digest_reader(reader: &mut impl io::Read) -> Result<String, HelperError> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];

    loop {
        let read = reader.read(&mut buffer).map_err(map_io)?;

        if read == 0 {
            break;
        }

        hasher.update(&buffer[..read]);
    }

    Ok(hex(&hasher.finalize()))
}

fn sha256(content: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(content);
    hasher.finalize().to_vec()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);

    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }

    output
}

fn kind_of(metadata: &Metadata) -> &'static str {
    let file_type = metadata.file_type();

    if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_dir() {
        "dir"
    } else if file_type.is_file() {
        "file"
    } else {
        "other"
    }
}

fn modified_unix(metadata: &Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    let epoch = cap_std::time::SystemTime::from_std(std::time::UNIX_EPOCH);
    let duration = modified.duration_since(epoch).ok()?;

    i64::try_from(duration.as_secs()).ok()
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

fn parent_of(path: &str) -> Option<&str> {
    path.rfind('/').map(|index| &path[..index])
}

fn temp_path(path: &str, attempt: u32) -> String {
    let name = format!(
        ".alx-tmp-{}-{}-{attempt}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    );

    match parent_of(path) {
        Some(parent) if !parent.is_empty() => format!("{parent}/{name}"),
        _ => name,
    }
}

/// Collapse duplicate separators and trailing slashes.
fn normalize(path: &str) -> String {
    let mut normalized = String::with_capacity(path.len());
    let mut previous_slash = false;

    for character in path.chars() {
        if character == '/' || character == '\\' {
            if previous_slash {
                continue;
            }

            previous_slash = true;
        } else {
            previous_slash = false;
        }

        normalized.push(character);
    }

    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }

    normalized
}

fn is_within(path: &str, prefix: &str) -> bool {
    let prefix = normalize(prefix);

    path == prefix
        || (path.starts_with(&prefix)
            && (prefix == "/" || path.as_bytes().get(prefix.len()) == Some(&b'/')))
}

/// Map an I/O error to a stable helper error.
pub fn map_io(error: io::Error) -> HelperError {
    let code = match error.kind() {
        io::ErrorKind::NotFound => helper::code::NOT_FOUND,
        io::ErrorKind::AlreadyExists => helper::code::EXISTS,
        io::ErrorKind::PermissionDenied => {
            if error.to_string().contains("led outside") {
                helper::code::PATH_ESCAPE
            } else {
                helper::code::PERMISSION
            }
        }
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => helper::code::INVALID_REQUEST,
        _ => helper::code::IO,
    };

    HelperError::new(code, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(prefixes: &[&str], path: &std::path::Path) -> SafeRoot {
        let prefixes: Vec<String> = prefixes.iter().map(|value| (*value).to_owned()).collect();
        SafeRoot::open(&prefixes, &path.to_string_lossy()).unwrap()
    }

    #[test]
    fn rejects_roots_outside_the_allowed_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let allowed = directory.path().join("data");
        std::fs::create_dir_all(&allowed).unwrap();

        let prefixes = vec![allowed.to_string_lossy().to_string()];

        assert!(SafeRoot::open(&prefixes, "/etc").is_err());
        assert!(SafeRoot::open(&prefixes, &directory.path().to_string_lossy()).is_err());
        assert!(SafeRoot::open(&prefixes, &allowed.to_string_lossy()).is_ok());
    }

    #[test]
    fn writes_reads_and_lists_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let allowed = directory.path().join("data");
        std::fs::create_dir_all(&allowed).unwrap();
        let root = root(&[&allowed.to_string_lossy()], &allowed);

        let result = root.write("a/b.txt", b"hello", true, false, None).unwrap();

        assert_eq!(result["size"], 5);

        let (mut file, metadata) = root.open_regular("a/b.txt").unwrap();
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut content).unwrap();

        assert_eq!(content, b"hello");
        assert_eq!(metadata.len(), 5);
        assert_eq!(root.stat("a/b.txt").unwrap()["kind"], "file");

        let (entries, truncated) = root.list("", 2, 100).unwrap();

        assert!(!truncated);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "a/b.txt"]
        );
    }

    #[test]
    fn refuses_to_overwrite_without_replace() {
        let directory = tempfile::tempdir().unwrap();
        let root = root(&[&directory.path().to_string_lossy()], directory.path());

        root.write("f", b"one", false, false, None).unwrap();

        assert_eq!(
            root.write("f", b"two", false, false, None)
                .unwrap_err()
                .code,
            helper::code::EXISTS
        );
    }

    #[test]
    fn enforces_expected_digest() {
        let directory = tempfile::tempdir().unwrap();
        let root = root(&[&directory.path().to_string_lossy()], directory.path());

        root.write("f", b"one", false, false, None).unwrap();

        let digest = root.sha256("f").unwrap()["sha256"]
            .as_str()
            .unwrap()
            .to_owned();

        root.write("f", b"two", false, true, Some(&digest)).unwrap();

        assert_eq!(
            root.write("f", b"three", false, true, Some(&digest))
                .unwrap_err()
                .code,
            helper::code::VERSION_MISMATCH
        );
    }

    #[test]
    fn path_traversal_and_symlink_escapes_fail() {
        let directory = tempfile::tempdir().unwrap();
        let allowed = directory.path().join("data");
        std::fs::create_dir_all(&allowed).unwrap();
        let root = root(&[&allowed.to_string_lossy()], &allowed);

        assert_eq!(
            root.write("../escape", b"x", false, false, None)
                .unwrap_err()
                .code,
            helper::code::PATH_ESCAPE
        );

        // An absolute destination is rejected before touching the filesystem.
        assert!(root.write("/etc/passwd", b"x", false, false, None).is_err());
    }

    #[test]
    fn usage_counts_files_and_reports_truncation() {
        let directory = tempfile::tempdir().unwrap();
        let root = root(&[&directory.path().to_string_lossy()], directory.path());

        root.write("a.txt", b"12345", false, false, None).unwrap();
        root.write("b/c.txt", b"123", true, false, None).unwrap();

        let usage = root.usage("", 1000).unwrap();

        assert_eq!(usage.files, 2);
        assert_eq!(usage.bytes, 8);
        assert!(!usage.truncated);

        let truncated = root.usage("", 1).unwrap();

        assert!(truncated.truncated);
    }

    #[test]
    fn copy_moves_content_and_digest() {
        let directory = tempfile::tempdir().unwrap();
        let root = root(&[&directory.path().to_string_lossy()], directory.path());

        root.write("source.bin", b"payload", false, false, None)
            .unwrap();

        let copied = root.copy("source.bin", "nested/target.bin", false, None);

        // The parent does not exist, so the copy fails instead of guessing.
        assert!(copied.is_err());

        root.mkdir("nested", false).unwrap();

        let copied = root
            .copy("source.bin", "nested/target.bin", false, None)
            .unwrap();

        assert_eq!(copied["size"], 7);
        assert_eq!(
            root.sha256("source.bin").unwrap()["sha256"],
            copied["sha256"]
        );
    }

    #[test]
    fn remove_rejects_the_root_and_deletes_children() {
        let directory = tempfile::tempdir().unwrap();
        let root = root(&[&directory.path().to_string_lossy()], directory.path());

        root.write("a/b", b"x", true, false, None).unwrap();

        assert!(root.remove("", true).is_err());
        root.remove("a", true).unwrap();

        assert!(!root.dir.try_exists("a").unwrap());
    }
}
