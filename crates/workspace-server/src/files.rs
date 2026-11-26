//! Workspace file service.
//!
//! File operations go through the trusted Linux helper, which resolves every
//! path inside a capability root. They work even when no container is running,
//! exactly as the architecture requires, and they verify workspace ownership
//! before touching a byte.

use base64::Engine;
use serde::Serialize;
use std::sync::Arc;
use thiserror::Error;
use workspace_core::helper::{self, Operation};
use workspace_runtime::status::RuntimeMonitor;
use workspace_runtime::{Helper, RuntimeSettings};

use crate::workspaces::{Manager, WorkspaceError, valid_id};
use workspace_core::workspace::WorkspaceState;

/// Largest text chunk returned by one read.
pub const MAX_READ_BYTES: u64 = 1024 * 1024;

/// Largest inline write accepted from a client.
pub const MAX_WRITE_BYTES: u64 = 8 * 1024 * 1024;

/// File service failures.
#[derive(Debug, Error)]
pub enum FilesError {
    #[error("{0}")]
    Invalid(&'static str),

    #[error("workspace not found")]
    NotFound,

    #[error("workspace belongs to another owner")]
    Forbidden,

    #[error("file is too large for this operation")]
    TooLarge,

    #[error("file changed concurrently; re-read it first")]
    Conflict,

    #[error("file already exists")]
    AlreadyExists,

    #[error("a file operation limit was exceeded")]
    LimitExceeded,

    #[error("file does not exist")]
    Missing,

    #[error("file operation failed: {0}")]
    Storage(String),

    #[error("the execution runtime is not ready: {0}")]
    RuntimeUnavailable(String),

    #[error("the workspace is not ready for file operations (state: {0})")]
    NotReady(&'static str),

    #[error("workspace error: {0}")]
    Workspace(#[from] WorkspaceError),
}

impl FilesError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_request",
            Self::NotFound => "workspace_not_found",
            Self::Forbidden => "forbidden",
            Self::TooLarge => "file_too_large",
            Self::Conflict => "file_conflict",
            Self::AlreadyExists => "file_exists",
            Self::LimitExceeded => "limit_exceeded",
            Self::Missing => "file_not_found",
            Self::Storage(_) => "storage_error",
            Self::RuntimeUnavailable(_) => "runtime_not_ready",
            Self::NotReady(_) => "workspace_not_ready",
            Self::Workspace(error) => error.code(),
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_) => 400,
            Self::NotFound | Self::Missing => 404,
            Self::Forbidden => 403,
            Self::TooLarge => 413,
            Self::Conflict | Self::AlreadyExists => 409,
            Self::LimitExceeded => 413,
            Self::Storage(_) => 500,
            Self::RuntimeUnavailable(_) => 503,
            Self::NotReady(_) => 409,
            Self::Workspace(error) => error.status(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub size: u64,
    pub modified_unix: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileChunk {
    pub path: String,
    pub size: u64,
    pub offset: u64,
    pub read: u64,
    pub next_offset: u64,
    pub eof: bool,
    pub sha256: String,
    pub encoding: &'static str,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WriteOutcome {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtractOutcome {
    pub entries: u64,
    pub bytes: u64,
}

/// Encode a file chunk without losing bytes.
///
/// Valid UTF-8 travels as text; anything else is base64 so binary content is
/// preserved exactly and the client always knows which form it received.
fn encode_content(body: &[u8]) -> (&'static str, String) {
    match std::str::from_utf8(body) {
        Ok(text) => ("utf-8", text.to_owned()),
        Err(_) => (
            "base64",
            base64::engine::general_purpose::STANDARD.encode(body),
        ),
    }
}

/// Offset and end-of-file flag for one chunk, without overflowing.
fn chunk_bounds(offset: u64, read: u64, size: u64) -> (u64, bool) {
    let end = offset.saturating_add(read);

    (end, end >= size)
}

/// Whether the workspace passed preparation, so its storage exists.///
/// `Stopped` keeps the storage on disk, so files remain readable and writable
/// without a container; every transitional or failed state does not.
fn operations_ready(state: WorkspaceState) -> bool {
    matches!(
        state,
        WorkspaceState::Created
            | WorkspaceState::Running
            | WorkspaceState::Idle
            | WorkspaceState::Stopped
    )
}

fn map_helper(error: workspace_runtime::RuntimeError) -> FilesError {
    use workspace_runtime::RuntimeError;

    match error {
        RuntimeError::Helper { code, message } => match code.as_str() {
            helper::code::NOT_FOUND => FilesError::Missing,
            helper::code::EXISTS => FilesError::AlreadyExists,
            helper::code::PATH_ESCAPE => FilesError::Invalid("path escapes the workspace"),
            helper::code::PATH_INVALID => FilesError::Invalid("invalid path"),
            helper::code::NOT_A_FILE => FilesError::Invalid("the path is not a regular file"),
            helper::code::NOT_A_DIRECTORY => FilesError::Invalid("the path is not a directory"),
            helper::code::TOO_LARGE => FilesError::TooLarge,
            helper::code::LIMIT => FilesError::LimitExceeded,
            helper::code::ARCHIVE => FilesError::Invalid("invalid archive"),
            helper::code::UNSUPPORTED => FilesError::Invalid("unsupported operation"),
            helper::code::INVALID_REQUEST => FilesError::Invalid("invalid file request"),
            helper::code::VERSION_MISMATCH => FilesError::Conflict,
            // A permission or I/O failure is a server-side problem: reporting
            // it as such keeps client errors distinguishable.
            _ => FilesError::Storage(message),
        },
        RuntimeError::NotConfigured | RuntimeError::Timeout(_) => {
            FilesError::RuntimeUnavailable(error.to_string())
        }
        other => FilesError::Storage(other.to_string()),
    }
}

/// File operations bound to one deployment.
pub struct Files {
    helper: Helper,
    manager: Arc<Manager>,
    settings: RuntimeSettings,
    max_import_bytes: u64,
}

impl Files {
    pub fn new(
        monitor: &RuntimeMonitor,
        manager: Arc<Manager>,
        max_import_bytes: u64,
    ) -> Result<Self, FilesError> {
        let settings = monitor.settings().cloned().ok_or_else(|| {
            FilesError::RuntimeUnavailable("execution runtime is not configured".to_owned())
        })?;

        let helper = monitor
            .helper()
            .cloned()
            .ok_or_else(|| FilesError::RuntimeUnavailable("helper is unavailable".to_owned()))?;

        Ok(Self {
            helper,
            manager,
            settings,
            max_import_bytes,
        })
    }

    pub fn staging_root(&self) -> String {
        format!("{}/staging", self.settings.data_root)
    }

    pub fn artifacts_root(&self) -> String {
        format!("{}/artifacts", self.settings.data_root)
    }

    pub fn max_import_bytes(&self) -> u64 {
        self.max_import_bytes
    }

    pub fn data_root(&self) -> &str {
        &self.settings.data_root
    }

    fn files_root(&self, workspace_id: &str) -> Result<String, FilesError> {
        if !valid_id(workspace_id) {
            return Err(FilesError::NotFound);
        }

        Ok(format!(
            "{}/workspaces/{workspace_id}/files",
            self.settings.data_root
        ))
    }

    /// Verify ownership before any file operation.
    async fn ensure_owned(&self, owner: &str, workspace_id: &str) -> Result<(), FilesError> {
        let workspace = self.manager.get_owned(owner, workspace_id).await?;

        if !operations_ready(workspace.state) {
            return Err(FilesError::NotReady(workspace.state.as_str()));
        }

        // Significant activity: reading and editing files extends the TTL.
        let _ = self.manager.touch(&workspace.id);

        Ok(())
    }

    pub async fn list(
        &self,
        owner: &str,
        workspace_id: &str,
        path: &str,
        depth: u32,
        limit: u32,
    ) -> Result<Listing, FilesError> {
        self.ensure_owned(owner, workspace_id).await?;

        let root = self.files_root(workspace_id)?;

        let (value, _) = self
            .helper
            .call(
                Operation::List {
                    root,
                    path: path.to_owned(),
                    depth: depth.min(8),
                    limit: limit.clamp(1, 5000),
                },
                &[],
            )
            .await
            .map_err(map_helper)?;

        let entries = value["entries"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        Some(Entry {
                            path: entry["path"].as_str()?.to_owned(),
                            name: entry["name"].as_str()?.to_owned(),
                            kind: entry["kind"].as_str().unwrap_or("other").to_owned(),
                            size: entry["size"].as_u64().unwrap_or(0),
                            modified_unix: entry["modified_unix"].as_i64(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Listing {
            entries,
            truncated: value["truncated"].as_bool().unwrap_or(false),
        })
    }

    pub async fn read(
        &self,
        owner: &str,
        workspace_id: &str,
        path: &str,
        offset: u64,
        length: u64,
    ) -> Result<FileChunk, FilesError> {
        self.ensure_owned(owner, workspace_id).await?;

        if path.is_empty() {
            return Err(FilesError::Invalid("a file path is required"));
        }

        let root = self.files_root(workspace_id)?;
        let length = length.clamp(1, MAX_READ_BYTES);

        let (value, body) = self
            .helper
            .call(
                Operation::Read {
                    root,
                    path: path.to_owned(),
                    offset,
                    length,
                },
                &[],
            )
            .await
            .map_err(map_helper)?;

        let size = value["size"].as_u64().unwrap_or(0);
        let read = value["read"].as_u64().unwrap_or(0);
        let sha256 = value["sha256"].as_str().unwrap_or_default().to_owned();

        let (encoding, content) = encode_content(&body);

        let (next_offset, eof) = chunk_bounds(offset, read, size);

        Ok(FileChunk {
            path: path.to_owned(),
            size,
            offset,
            read,
            next_offset,
            eof,
            sha256,
            encoding,
            content,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn write(
        &self,
        owner: &str,
        workspace_id: &str,
        path: &str,
        content: &[u8],
        expected_sha256: Option<&str>,
        create_parents: bool,
        replace: bool,
    ) -> Result<WriteOutcome, FilesError> {
        self.ensure_owned(owner, workspace_id).await?;

        if path.is_empty() {
            return Err(FilesError::Invalid("a file path is required"));
        }

        if content.len() as u64 > MAX_WRITE_BYTES {
            return Err(FilesError::TooLarge);
        }

        let root = self.files_root(workspace_id)?;

        let (value, _) = self
            .helper
            .call(
                Operation::Write {
                    root,
                    path: path.to_owned(),
                    create_parents,
                    replace,
                    expected_sha256: expected_sha256.map(str::to_owned),
                },
                content,
            )
            .await
            .map_err(map_helper)?;

        Ok(WriteOutcome {
            path: path.to_owned(),
            size: value["size"].as_u64().unwrap_or(content.len() as u64),
            sha256: value["sha256"].as_str().unwrap_or_default().to_owned(),
        })
    }

    /// Read, apply a strict unified diff and write the result atomically.
    pub async fn patch(
        &self,
        owner: &str,
        workspace_id: &str,
        path: &str,
        patch: &str,
        expected_sha256: Option<&str>,
    ) -> Result<WriteOutcome, FilesError> {
        self.ensure_owned(owner, workspace_id).await?;

        if patch.len() as u64 > MAX_WRITE_BYTES {
            return Err(FilesError::TooLarge);
        }

        if path.is_empty() {
            return Err(FilesError::Invalid("a file path is required"));
        }

        let root = self.files_root(workspace_id)?;

        // Read the whole file in bounded chunks.
        let mut original = Vec::new();
        let mut offset = 0u64;

        loop {
            let (value, body) = self
                .helper
                .call(
                    Operation::Read {
                        root: root.clone(),
                        path: path.to_owned(),
                        offset,
                        length: MAX_READ_BYTES,
                    },
                    &[],
                )
                .await
                .map_err(map_helper)?;

            let size = value["size"].as_u64().unwrap_or(0);

            if size > MAX_WRITE_BYTES {
                return Err(FilesError::TooLarge);
            }

            original.extend_from_slice(&body);
            offset += body.len() as u64;

            if offset >= size || body.is_empty() {
                break;
            }
        }

        let original_text = String::from_utf8(original)
            .map_err(|_| FilesError::Invalid("file_patch requires a UTF-8 text file"))?;

        let patched = workspace_core::patch::apply(&original_text, patch)
            .map_err(|_| FilesError::Conflict)?;

        self.write(
            owner,
            workspace_id,
            path,
            patched.as_bytes(),
            expected_sha256,
            false,
            true,
        )
        .await
    }

    pub fn staging_name(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    pub fn staging_path(&self, name: &str) -> Result<String, FilesError> {
        let parsed = uuid::Uuid::parse_str(name)
            .map_err(|_| FilesError::Invalid("invalid staging identifier"))?;

        if parsed.to_string() != name {
            return Err(FilesError::Invalid("invalid staging identifier"));
        }

        Ok(format!("{}/{}", self.staging_root(), name))
    }

    /// Import a file that was streamed into the staging directory.
    pub async fn import_staged(
        &self,
        owner: &str,
        workspace_id: &str,
        staging: &str,
        destination: &str,
        replace: bool,
    ) -> Result<WriteOutcome, FilesError> {
        self.ensure_owned(owner, workspace_id).await?;

        let staging_path = self.staging_path(staging)?;
        let root = self.files_root(workspace_id)?;

        let (value, _) = self
            .helper
            .call(
                Operation::Copy {
                    source_root: self.staging_root(),
                    source_path: staging_path
                        .rsplit_once('/')
                        .map(|(_, name)| name.to_owned())
                        .unwrap_or_default(),
                    target_root: root,
                    target_path: destination.to_owned(),
                    replace,
                    expected_sha256: None,
                },
                &[],
            )
            .await
            .map_err(map_helper)?;

        let _ = self.discard_staged(staging).await;

        Ok(WriteOutcome {
            path: destination.to_owned(),
            size: value["size"].as_u64().unwrap_or(0),
            sha256: value["sha256"].as_str().unwrap_or_default().to_owned(),
        })
    }

    /// Extract an archive that was streamed into the staging directory.
    pub async fn extract_staged(
        &self,
        owner: &str,
        workspace_id: &str,
        staging: &str,
        destination: &str,
        max_entries: u32,
        max_bytes: u64,
    ) -> Result<ExtractOutcome, FilesError> {
        self.ensure_owned(owner, workspace_id).await?;

        let staging_path = self.staging_path(staging)?;
        let root = self.files_root(workspace_id)?;

        let limit = max_bytes.min(self.max_import_bytes.saturating_mul(8));

        let (value, _) = self
            .helper
            .call(
                Operation::Extract {
                    root,
                    path: destination.to_owned(),
                    archive_root: self.staging_root(),
                    archive_path: staging_path
                        .rsplit_once('/')
                        .map(|(_, name)| name.to_owned())
                        .unwrap_or_default(),
                    max_entries: max_entries.clamp(1, 20_000),
                    max_bytes: limit,
                },
                &[],
            )
            .await
            .map_err(map_helper)?;

        let _ = self.discard_staged(staging).await;

        Ok(ExtractOutcome {
            entries: value["entries"].as_u64().unwrap_or(0),
            bytes: value["bytes"].as_u64().unwrap_or(0),
        })
    }

    /// Remove a staging file, ignoring an already absent one.
    pub async fn discard_staged(&self, staging: &str) -> Result<(), FilesError> {
        let staging_path = self.staging_path(staging)?;

        let result = self
            .helper
            .call(
                Operation::Remove {
                    root: self.staging_root(),
                    path: staging_path
                        .rsplit_once('/')
                        .map(|(_, name)| name.to_owned())
                        .unwrap_or_default(),
                    recursive: false,
                },
                &[],
            )
            .await;

        match result {
            Ok(_) => Ok(()),
            Err(workspace_runtime::RuntimeError::Helper { code, .. })
                if code == helper::code::NOT_FOUND =>
            {
                Ok(())
            }
            Err(error) => Err(map_helper(error)),
        }
    }

    /// SHA-256 of a workspace file.
    pub async fn sha256(
        &self,
        owner: &str,
        workspace_id: &str,
        path: &str,
    ) -> Result<(u64, String), FilesError> {
        self.ensure_owned(owner, workspace_id).await?;

        let root = self.files_root(workspace_id)?;

        let (value, _) = self
            .helper
            .call(
                Operation::Sha256 {
                    root,
                    path: path.to_owned(),
                },
                &[],
            )
            .await
            .map_err(map_helper)?;

        Ok((
            value["size"].as_u64().unwrap_or(0),
            value["sha256"].as_str().unwrap_or_default().to_owned(),
        ))
    }

    /// Copy a workspace file into the artifact store.
    pub async fn copy_to_artifacts(
        &self,
        owner: &str,
        workspace_id: &str,
        source: &str,
        artifact_id: &str,
        stored_name: &str,
        replace: bool,
    ) -> Result<WriteOutcome, FilesError> {
        self.ensure_owned(owner, workspace_id).await?;

        let root = self.files_root(workspace_id)?;
        let artifacts = self.artifacts_root();

        // Ensure the artifact directory exists before copying into it.
        self.helper
            .call(
                Operation::Mkdir {
                    root: artifacts.clone(),
                    path: artifact_id.to_owned(),
                    parents: true,
                },
                &[],
            )
            .await
            .map_err(map_helper)?;

        let (value, _) = self
            .helper
            .call(
                Operation::Copy {
                    source_root: root,
                    source_path: source.to_owned(),
                    target_root: artifacts,
                    target_path: format!("{artifact_id}/{stored_name}"),
                    replace,
                    expected_sha256: None,
                },
                &[],
            )
            .await
            .map_err(map_helper)?;

        Ok(WriteOutcome {
            path: format!("{artifact_id}/{stored_name}"),
            size: value["size"].as_u64().unwrap_or(0),
            sha256: value["sha256"].as_str().unwrap_or_default().to_owned(),
        })
    }

    /// Read a byte range of an artifact file.
    pub async fn read_artifact(
        &self,
        artifact_id: &str,
        path: &str,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, FilesError> {
        if !valid_id(artifact_id) {
            return Err(FilesError::NotFound);
        }

        let (_, body) = self
            .helper
            .call(
                Operation::Read {
                    root: self.artifacts_root(),
                    path: format!("{artifact_id}/{path}"),
                    offset,
                    length: length.clamp(1, MAX_READ_BYTES),
                },
                &[],
            )
            .await
            .map_err(map_helper)?;

        Ok(body)
    }

    /// Remove an artifact directory.
    pub async fn remove_artifact(&self, artifact_id: &str) -> Result<(), FilesError> {
        if !valid_id(artifact_id) {
            return Err(FilesError::NotFound);
        }

        self.helper
            .call(
                Operation::Remove {
                    root: self.artifacts_root(),
                    path: artifact_id.to_owned(),
                    recursive: true,
                },
                &[],
            )
            .await
            .map_err(map_helper)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_operations_require_prepared_storage() {
        // Storage exists only after preparation completed, so a workspace in a
        // transitional or terminal-failure state must answer `workspace_not_ready`
        // instead of a misleading `file_not_found`.
        for state in [
            WorkspaceState::Preparing,
            WorkspaceState::Starting,
            WorkspaceState::Stopping,
            WorkspaceState::Deleting,
            WorkspaceState::Failed,
            WorkspaceState::Expired,
        ] {
            assert!(!operations_ready(state), "{state:?}");
        }

        for state in [
            WorkspaceState::Created,
            WorkspaceState::Running,
            WorkspaceState::Idle,
            WorkspaceState::Stopped,
        ] {
            assert!(operations_ready(state), "{state:?}");
        }
    }

    #[test]
    fn not_ready_is_a_client_error_not_a_storage_failure() {
        let error = FilesError::NotReady("preparing");

        assert_eq!(error.code(), "workspace_not_ready");
        assert_eq!(error.status(), 409);
        assert!(error.to_string().contains("preparing"));
    }

    #[test]
    fn staging_identifiers_are_server_generated_uuids() {
        let staging = uuid::Uuid::new_v4().to_string();

        assert!(staging.contains('-'));
        assert!(uuid::Uuid::parse_str(&staging).is_ok());
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(FilesError::TooLarge.code(), "file_too_large");
        assert_eq!(FilesError::Conflict.code(), "file_conflict");
        assert_eq!(FilesError::Missing.status(), 404);
        assert_eq!(FilesError::AlreadyExists.code(), "file_exists");
        assert_eq!(FilesError::AlreadyExists.status(), 409);
    }

    fn helper_error(code: &str) -> workspace_runtime::RuntimeError {
        workspace_runtime::RuntimeError::Helper {
            code: code.to_owned(),
            message: "detail".into(),
        }
    }

    #[test]
    fn client_mistakes_are_not_reported_as_storage_failures() {
        // Absolute paths and duplicate `create` requests were classified as
        // `storage_error` (500). They are caller errors and must stay 4xx.
        let cases = [
            (helper::code::PATH_INVALID, "invalid_request", 400),
            (helper::code::EXISTS, "file_exists", 409),
            (helper::code::NOT_A_FILE, "invalid_request", 400),
            (helper::code::NOT_A_DIRECTORY, "invalid_request", 400),
            (helper::code::LIMIT, "limit_exceeded", 413),
            (helper::code::ARCHIVE, "invalid_request", 400),
            (helper::code::UNSUPPORTED, "invalid_request", 400),
        ];

        for (helper_code, expected_code, expected_status) in cases {
            let mapped = map_helper(helper_error(helper_code));

            assert_eq!(mapped.code(), expected_code, "{helper_code}");
            assert_eq!(mapped.status(), expected_status, "{helper_code}");
        }
    }

    #[test]
    fn genuine_server_failures_stay_storage_errors() {
        for code in [helper::code::IO, helper::code::PERMISSION, "unknown"] {
            let mapped = map_helper(helper_error(code));

            assert_eq!(mapped.code(), "storage_error", "{code}");
            assert_eq!(mapped.status(), 500, "{code}");
        }
    }

    #[test]
    fn utf8_and_binary_content_survive_the_wire_format() {
        let text = "привет, мир\n";
        let (encoding, content) = encode_content(text.as_bytes());

        assert_eq!(encoding, "utf-8");
        assert_eq!(content, text);

        let binary = [0xFFu8, 0x00, 0xFE, 0x80];
        let (encoding, content) = encode_content(&binary);

        assert_eq!(encoding, "base64");

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&content)
            .unwrap();
        assert_eq!(decoded, binary);
    }

    #[test]
    fn chunk_pagination_is_exact_and_never_overflows() {
        // A full chunk in the middle of a file continues.
        assert_eq!(chunk_bounds(0, 10, 100), (10, false));
        // The last byte ends the file.
        assert_eq!(chunk_bounds(90, 10, 100), (100, true));
        // A short read before the end still continues.
        assert_eq!(chunk_bounds(10, 5, 100), (15, false));
        // An empty file is complete at offset zero.
        assert_eq!(chunk_bounds(0, 0, 0), (0, true));
        // A hostile offset cannot wrap around.
        assert_eq!(chunk_bounds(u64::MAX, 10, 100), (u64::MAX, true));
    }
}
