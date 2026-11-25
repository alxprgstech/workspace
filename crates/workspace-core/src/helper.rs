//! Framed protocol between the Workspace server and the privileged Linux helper.
//!
//! The server never passes paths to a shell: it writes one framed JSON request
//! to the helper's stdin and reads one framed response from its stdout. Every
//! filesystem path in a request is relative to an explicit root directory that
//! the helper opens as a capability, so a path can never escape that root even
//! through symlinks.
//!
//! Frame layout (both directions):
//!
//! ```text
//! u32 big-endian json length | json header | body_len raw bytes
//! ```

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Bumped whenever the request/response shape changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest JSON header the helper accepts or emits.
pub const MAX_HEADER_BYTES: u32 = 64 * 1024;

/// Largest inline body moved through one frame (file writes, reads, patches).
pub const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;

/// Largest path string a request may carry.
pub const MAX_PATH_BYTES: usize = 4096;

/// Largest directory listing the helper returns in one response.
pub const MAX_LIST_ENTRIES: u32 = 10_000;

/// One request. `body_len` bytes of raw body follow the header, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub protocol: u32,
    pub request_id: String,
    pub body_len: u64,
    #[serde(flatten)]
    pub operation: Operation,
}

/// Helper operations.
///
/// Roots are absolute paths inside the dedicated runtime data directory. The
/// helper refuses roots outside the prefix passed on its command line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    /// Liveness and identity check.
    Ping,
    /// Metadata for one path.
    Stat { root: String, path: String },
    /// Directory listing, optionally recursive, breadth-first.
    List {
        root: String,
        path: String,
        depth: u32,
        limit: u32,
    },
    /// Read a byte range. The response body carries the bytes.
    Read {
        root: String,
        path: String,
        offset: u64,
        length: u64,
    },
    /// Write a file atomically. The request body carries the content.
    Write {
        root: String,
        path: String,
        create_parents: bool,
        replace: bool,
        expected_sha256: Option<String>,
    },
    /// Create a directory.
    Mkdir {
        root: String,
        path: String,
        parents: bool,
    },
    /// Remove a file or directory.
    Remove {
        root: String,
        path: String,
        recursive: bool,
    },
    /// Copy a regular file between two roots of the same helper invocation.
    Copy {
        source_root: String,
        source_path: String,
        target_root: String,
        target_path: String,
        replace: bool,
        expected_sha256: Option<String>,
    },
    /// SHA-256 of a regular file.
    Sha256 { root: String, path: String },
    /// Recursive size and file count of a subtree.
    Usage {
        root: String,
        path: String,
        max_entries: u32,
    },
}

/// Stable helper error codes. Clients branch on these, never on the message.
pub mod code {
    pub const INVALID_REQUEST: &str = "invalid_request";
    pub const PATH_INVALID: &str = "path_invalid";
    pub const PATH_ESCAPE: &str = "path_escape";
    pub const NOT_FOUND: &str = "not_found";
    pub const EXISTS: &str = "already_exists";
    pub const NOT_A_FILE: &str = "not_a_file";
    pub const NOT_A_DIRECTORY: &str = "not_a_directory";
    pub const TOO_LARGE: &str = "too_large";
    pub const VERSION_MISMATCH: &str = "conflict";
    pub const UNSUPPORTED: &str = "unsupported";
    pub const PERMISSION: &str = "permission_denied";
    pub const IO: &str = "io_error";
    pub const INTERNAL: &str = "internal";
}

/// Machine-readable failure returned by the helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperError {
    pub code: String,
    pub message: String,
}

impl HelperError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// One response. `body_len` bytes of raw body follow the header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub protocol: u32,
    pub request_id: String,
    pub body_len: u64,
    pub result: Result<serde_json::Value, HelperError>,
}

/// Write a frame.
pub fn write_frame(writer: &mut impl Write, header: &[u8], body: &[u8]) -> io::Result<()> {
    if header.len() > MAX_HEADER_BYTES as usize || body.len() as u64 > MAX_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame exceeds protocol limits",
        ));
    }

    writer.write_all(&(header.len() as u32).to_be_bytes())?;
    writer.write_all(header)?;
    writer.write_all(body)?;
    writer.flush()
}

/// Read a frame, enforcing the protocol size limits before allocating.
pub fn read_frame(reader: &mut impl Read) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut length = [0u8; 4];
    reader.read_exact(&mut length)?;

    let header_len = u32::from_be_bytes(length);

    if header_len > MAX_HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request header exceeds protocol limits",
        ));
    }

    let mut header = vec![0u8; header_len as usize];
    reader.read_exact(&mut header)?;

    // A malformed header leaves the body length unknown. Return the header so
    // the peer can answer with a structured protocol error; the ambiguous body
    // stays unread, and the one-shot helper exits right after the response.
    let body_len = match serde_json::from_slice::<serde_json::Value>(&header) {
        Ok(probe) => probe
            .get("body_len")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        Err(_) => 0,
    };

    if body_len > MAX_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request body exceeds protocol limits",
        ));
    }

    let mut body = vec![0u8; body_len as usize];
    reader.read_exact(&mut body)?;

    Ok((header, body))
}

/// Validate a path that will be resolved inside a capability root.
///
/// The helper additionally relies on operating-system confinement; this check
/// rejects obviously hostile input early and keeps error codes stable.
pub fn validate_relative_path(path: &str) -> Result<(), HelperError> {
    if path.is_empty() {
        return Ok(());
    }

    if path.len() > MAX_PATH_BYTES {
        return Err(HelperError::new(
            code::PATH_INVALID,
            "path is longer than the protocol limit",
        ));
    }

    if path.starts_with('/') {
        return Err(HelperError::new(
            code::PATH_INVALID,
            "absolute paths are not accepted",
        ));
    }

    if path.contains('\0') {
        return Err(HelperError::new(
            code::PATH_INVALID,
            "NUL bytes are not allowed in paths",
        ));
    }

    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }

        if component == ".." {
            return Err(HelperError::new(
                code::PATH_ESCAPE,
                "parent traversal is not allowed",
            ));
        }

        if component.len() > 255 {
            return Err(HelperError::new(
                code::PATH_INVALID,
                "path component is longer than 255 bytes",
            ));
        }

        if component.chars().any(char::is_control) {
            return Err(HelperError::new(
                code::PATH_INVALID,
                "control characters are not allowed in paths",
            ));
        }
    }

    Ok(())
}

/// Validate an absolute capability root.
pub fn validate_root(root: &str) -> Result<(), HelperError> {
    // The deployed helper always runs on Linux, where roots are POSIX paths.
    // Windows hosts only exercise this code in unit tests, so an absolute
    // platform path is accepted there for testability.
    let absolute = root.starts_with('/') || std::path::Path::new(root).is_absolute();

    if !absolute {
        return Err(HelperError::new(
            code::PATH_INVALID,
            "capability root must be absolute",
        ));
    }

    if root.len() > MAX_PATH_BYTES || root.contains('\0') {
        return Err(HelperError::new(code::PATH_INVALID, "invalid root"));
    }

    if root.split(['/', '\\']).any(|component| component == "..") {
        return Err(HelperError::new(
            code::PATH_ESCAPE,
            "root must not traverse upwards",
        ));
    }

    Ok(())
}

impl Operation {
    /// Stable operation name used in logs and diagnostics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Stat { .. } => "stat",
            Self::List { .. } => "list",
            Self::Read { .. } => "read",
            Self::Write { .. } => "write",
            Self::Mkdir { .. } => "mkdir",
            Self::Remove { .. } => "remove",
            Self::Copy { .. } => "copy",
            Self::Sha256 { .. } => "sha256",
            Self::Usage { .. } => "usage",
        }
    }
}

impl Request {
    /// Convenience accessor for logging.
    pub fn operation_kind(&self) -> &'static str {
        self.operation.kind()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_traversal() {
        assert_eq!(
            validate_relative_path("../outside").unwrap_err().code,
            code::PATH_ESCAPE
        );
        assert_eq!(
            validate_relative_path("a/../../b").unwrap_err().code,
            code::PATH_ESCAPE
        );
    }

    #[test]
    fn rejects_absolute_and_control_characters() {
        assert_eq!(
            validate_relative_path("/etc/passwd").unwrap_err().code,
            code::PATH_INVALID
        );
        assert_eq!(
            validate_relative_path("a\u{7}b").unwrap_err().code,
            code::PATH_INVALID
        );
        assert_eq!(
            validate_relative_path("a\0b").unwrap_err().code,
            code::PATH_INVALID
        );
    }

    #[test]
    fn accepts_ordinary_relative_paths() {
        assert!(validate_relative_path("").is_ok());
        assert!(validate_relative_path("src/main.rs").is_ok());
        assert!(validate_relative_path("./src//lib.rs").is_ok());
        assert!(validate_relative_path("a b/c d.txt").is_ok());
    }

    #[test]
    fn roots_must_be_absolute_and_closed() {
        assert!(validate_root("/srv/alxprgs-workspace/data/workspaces/x").is_ok());
        assert!(validate_root("relative/path").is_err());
        assert!(validate_root("/srv/../../etc").is_err());
    }

    #[test]
    fn frames_round_trip() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, b"{\"body_len\":7}", b"payload").unwrap();

        let (header, body) = read_frame(&mut buffer.as_slice()).unwrap();

        assert_eq!(header, b"{\"body_len\":7}");
        assert_eq!(body, b"payload");
    }

    #[test]
    fn oversized_header_is_rejected_before_allocation() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&(MAX_HEADER_BYTES + 1).to_be_bytes());

        let error = read_frame(&mut buffer.as_slice()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn oversized_body_is_rejected() {
        let header = serde_json::json!({ "body_len": MAX_BODY_BYTES + 1 });
        let encoded = serde_json::to_vec(&header).unwrap();

        let mut buffer = Vec::new();
        buffer.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
        buffer.extend_from_slice(&encoded);

        assert!(read_frame(&mut buffer.as_slice()).is_err());
    }

    #[test]
    fn request_round_trips_through_json() {
        let request = Request {
            protocol: PROTOCOL_VERSION,
            request_id: "r1".into(),
            body_len: 0,
            operation: Operation::List {
                root: "/data".into(),
                path: "src".into(),
                depth: 1,
                limit: 100,
            },
        };

        let encoded = serde_json::to_vec(&request).unwrap();
        let decoded: Request = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(decoded, request);
        assert_eq!(decoded.operation_kind(), "list");
    }
}
