//! Request dispatch for the helper protocol.

use crate::safe::{SafeRoot, hex};
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom};
use workspace_core::helper::{self, HelperError, Operation, Request};

/// Result of a successful operation: a JSON header plus optional raw body.
#[derive(Debug)]
pub struct Outcome {
    pub value: Value,
    pub body: Vec<u8>,
}

impl Outcome {
    fn json(value: Value) -> Self {
        Self {
            value,
            body: Vec::new(),
        }
    }
}

/// Execute one request.
pub fn execute(
    prefixes: &[String],
    request: &Request,
    body: &[u8],
) -> Result<Outcome, HelperError> {
    if request.body_len != body.len() as u64 {
        return Err(HelperError::new(
            helper::code::INVALID_REQUEST,
            "declared body length does not match the received body",
        ));
    }

    match &request.operation {
        Operation::Ping => Ok(Outcome::json(json!({
            "version": workspace_core::VERSION,
            "protocol": helper::PROTOCOL_VERSION,
            "uid": current_uid(),
            "process_id": std::process::id(),
        }))),

        Operation::Stat { root, path } => {
            let root = SafeRoot::open(prefixes, root)?;
            Ok(Outcome::json(root.stat(path)?))
        }

        Operation::List {
            root,
            path,
            depth,
            limit,
        } => {
            let root = SafeRoot::open(prefixes, root)?;
            let (entries, truncated) = root.list(path, *depth, *limit)?;

            Ok(Outcome::json(json!({
                "entries": entries,
                "truncated": truncated,
            })))
        }

        Operation::Read {
            root,
            path,
            offset,
            length,
        } => {
            let root = SafeRoot::open(prefixes, root)?;
            let (mut file, metadata) = root.open_regular(path)?;

            let total = metadata.len();

            if *offset > total {
                return Err(HelperError::new(
                    helper::code::INVALID_REQUEST,
                    "offset is beyond the end of the file",
                ));
            }

            let requested = (*length).min(helper::MAX_BODY_BYTES);
            let available = total - offset;
            let to_read = requested.min(available);

            file.seek(SeekFrom::Start(*offset))
                .map_err(crate::safe::map_io)?;

            let mut buffer = vec![0u8; to_read as usize];

            if to_read > 0 {
                file.read_exact(&mut buffer).map_err(crate::safe::map_io)?;
            }

            let mut hasher = sha2::Sha256::new();
            use sha2::Digest;
            hasher.update(&buffer);
            let digest = hex(&hasher.finalize());

            Ok(Outcome {
                value: json!({
                    "sha256": digest,
                    "size": total,
                    "offset": offset,
                    "read": to_read,
                    "eof": offset + to_read >= total,
                    "truncated": to_read < requested,
                }),
                body: buffer,
            })
        }

        Operation::Write {
            root,
            path,
            create_parents,
            replace,
            expected_sha256,
        } => {
            let root = SafeRoot::open(prefixes, root)?;

            Ok(Outcome::json(root.write(
                path,
                body,
                *create_parents,
                *replace,
                expected_sha256.as_deref(),
            )?))
        }

        Operation::Mkdir {
            root,
            path,
            parents,
        } => {
            let root = SafeRoot::open(prefixes, root)?;
            root.mkdir(path, *parents)?;

            Ok(Outcome::json(json!({ "created": path })))
        }

        Operation::Remove {
            root,
            path,
            recursive,
        } => {
            let root = SafeRoot::open(prefixes, root)?;

            Ok(Outcome::json(root.remove(path, *recursive)?))
        }

        Operation::Copy {
            source_root,
            source_path,
            target_root,
            target_path,
            replace,
            expected_sha256,
        } => {
            let source = SafeRoot::open(prefixes, source_root)?;

            if source_root == target_root {
                return Ok(Outcome::json(source.copy(
                    source_path,
                    target_path,
                    *replace,
                    expected_sha256.as_deref(),
                )?));
            }

            // Cross-root copies are not needed by the server and would widen
            // the helper's authority; refuse instead of implementing them.
            Err(HelperError::new(
                helper::code::UNSUPPORTED,
                "cross-root copies are not supported",
            ))
        }

        Operation::Sha256 { root, path } => {
            let root = SafeRoot::open(prefixes, root)?;
            Ok(Outcome::json(root.sha256(path)?))
        }

        Operation::Usage {
            root,
            path,
            max_entries,
        } => {
            let root = SafeRoot::open(prefixes, root)?;
            let usage = root.usage(path, *max_entries)?;

            Ok(Outcome::json(json!({
                "bytes": usage.bytes,
                "files": usage.files,
                "directories": usage.directories,
                "truncated": usage.truncated,
            })))
        }
    }
}

/// Best-effort real UID without pulling in a syscall dependency.
fn current_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;

    status.lines().find_map(|line| {
        line.strip_prefix("Uid:")
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse().ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn request(operation: Operation) -> Request {
        Request {
            protocol: helper::PROTOCOL_VERSION,
            request_id: "test".into(),
            body_len: 0,
            operation,
        }
    }

    fn prefixes(path: &Path) -> Vec<String> {
        vec![path.to_string_lossy().to_string()]
    }

    #[test]
    fn ping_reports_identity() {
        let directory = tempfile::tempdir().unwrap();
        let outcome = execute(&prefixes(directory.path()), &request(Operation::Ping), &[]).unwrap();

        assert_eq!(outcome.value["protocol"], helper::PROTOCOL_VERSION);
        assert!(outcome.value["process_id"].is_number());
    }

    #[test]
    fn write_requires_the_declared_body() {
        let directory = tempfile::tempdir().unwrap();

        let mut write = request(Operation::Write {
            root: directory.path().to_string_lossy().to_string(),
            path: "f".into(),
            create_parents: false,
            replace: false,
            expected_sha256: None,
        });

        write.body_len = 5;

        let error = execute(&prefixes(directory.path()), &write, b"abc").unwrap_err();

        assert_eq!(error.code, helper::code::INVALID_REQUEST);
    }

    #[test]
    fn read_returns_a_bounded_body_and_digest() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_string_lossy().to_string();

        let mut write = request(Operation::Write {
            root: root.clone(),
            path: "f".into(),
            create_parents: false,
            replace: false,
            expected_sha256: None,
        });

        write.body_len = 10;

        execute(&prefixes(directory.path()), &write, b"0123456789").unwrap();

        let outcome = execute(
            &prefixes(directory.path()),
            &request(Operation::Read {
                root: root.clone(),
                path: "f".into(),
                offset: 2,
                length: 4,
            }),
            &[],
        )
        .unwrap();

        assert_eq!(outcome.body, b"2345");
        assert_eq!(outcome.value["size"], 10);
        assert_eq!(outcome.value["read"], 4);
        assert_eq!(outcome.value["eof"], false);

        let outcome = execute(
            &prefixes(directory.path()),
            &request(Operation::Read {
                root,
                path: "f".into(),
                offset: 8,
                length: 100,
            }),
            &[],
        )
        .unwrap();

        assert_eq!(outcome.body, b"89");
        assert_eq!(outcome.value["eof"], true);
    }

    #[test]
    fn cross_root_copy_is_refused() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();

        let mut all = prefixes(first.path());
        all.extend(prefixes(second.path()));

        let error = execute(
            &all,
            &request(Operation::Copy {
                source_root: first.path().to_string_lossy().to_string(),
                source_path: "a".into(),
                target_root: second.path().to_string_lossy().to_string(),
                target_path: "b".into(),
                replace: false,
                expected_sha256: None,
            }),
            &[],
        )
        .unwrap_err();

        assert_eq!(error.code, helper::code::UNSUPPORTED);
    }
}
