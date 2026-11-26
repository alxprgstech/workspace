//! Published artifacts: immutable copies with their own lifetime.
//!
//! Publishing copies the file out of the workspace, so deleting the workspace
//! never removes an artifact. Downloads use opaque, expiring, revocable links
//! that reveal no storage structure.

use base64::Engine;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

use crate::files::{Files, FilesError};
use crate::storage;

/// Maximum artifact size accepted by one publish.
pub const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("{0}")]
    Invalid(&'static str),

    #[error("artifact not found")]
    NotFound,

    #[error("artifact belongs to another owner")]
    Forbidden,

    #[error("artifact is too large")]
    TooLarge,

    #[error("artifact link is invalid or expired")]
    LinkInvalid,

    #[error("file operation failed: {0}")]
    Files(#[from] FilesError),

    #[error("database operation failed: {0}")]
    Database(String),
}

impl ArtifactError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_request",
            Self::NotFound => "artifact_not_found",
            Self::Forbidden => "forbidden",
            Self::TooLarge => "artifact_too_large",
            Self::LinkInvalid => "link_invalid",
            Self::Files(error) => error.code(),
            Self::Database(_) => "database_error",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_) => 400,
            Self::NotFound | Self::LinkInvalid => 404,
            Self::Forbidden => 403,
            Self::TooLarge => 413,
            Self::Files(error) => error.status(),
            Self::Database(_) => 500,
        }
    }
}

/// One published artifact.
#[derive(Debug, Clone, Serialize)]
pub struct Artifact {
    pub id: String,
    pub workspace_id: Option<String>,
    pub name: String,
    pub size_bytes: u64,
    pub media_type: String,
    pub sha256: String,
    pub created_at_unix: i64,
    pub expires_at_unix: i64,
    pub revoked: bool,
}

/// Expiring download link.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadLink {
    pub artifact_id: String,
    pub url: String,
    pub expires_at_unix: i64,
    pub sha256: String,
    pub size_bytes: u64,
}

/// Artifact store settings.
#[derive(Debug, Clone)]
pub struct ArtifactSettings {
    pub ttl_seconds: u64,
    pub link_ttl_seconds: u64,
    pub public_base: String,
}

/// Durable artifact metadata and links.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    data_dir: PathBuf,
}

impl ArtifactStore {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    fn connection(&self) -> Result<rusqlite::Connection, ArtifactError> {
        storage::open(&self.data_dir).map_err(|error| ArtifactError::Database(error.to_string()))
    }

    fn insert(
        &self,
        artifact: &Artifact,
        owner: &str,
        stored_name: &str,
    ) -> Result<(), ArtifactError> {
        let connection = self.connection()?;

        connection
            .execute(
                "INSERT INTO artifacts (
                    id, owner_id, workspace_id, display_name, stored_name,
                    media_type, size_bytes, sha256, created_at_unix, expires_at_unix
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    artifact.id,
                    owner,
                    artifact.workspace_id,
                    artifact.name,
                    stored_name,
                    artifact.media_type,
                    artifact.size_bytes as i64,
                    artifact.sha256,
                    artifact.created_at_unix,
                    artifact.expires_at_unix,
                ],
            )
            .map_err(|error| ArtifactError::Database(error.to_string()))?;

        Ok(())
    }

    fn row(&self, id: &str) -> Result<Option<(Artifact, String, String)>, ArtifactError> {
        let connection = self.connection()?;

        connection
            .query_row(
                "SELECT id, workspace_id, display_name, media_type, size_bytes, sha256,
                        created_at_unix, expires_at_unix, revoked_at_unix, stored_name, owner_id
                 FROM artifacts WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        Artifact {
                            id: row.get(0)?,
                            workspace_id: row.get(1)?,
                            name: row.get(2)?,
                            media_type: row.get(3)?,
                            size_bytes: row.get::<_, i64>(4)? as u64,
                            sha256: row.get(5)?,
                            created_at_unix: row.get(6)?,
                            expires_at_unix: row.get(7)?,
                            revoked: row.get::<_, Option<i64>>(8)?.is_some(),
                        },
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| ArtifactError::Database(error.to_string()))
    }

    pub fn get(&self, id: &str) -> Result<Option<Artifact>, ArtifactError> {
        Ok(self.row(id)?.map(|(artifact, _, _)| artifact))
    }

    pub fn owned(&self, owner: &str, id: &str) -> Result<Artifact, ArtifactError> {
        let (artifact, _, owner_id) = self.row(id)?.ok_or(ArtifactError::NotFound)?;

        if owner_id != owner {
            return Err(ArtifactError::Forbidden);
        }

        Ok(artifact)
    }

    pub fn list(&self, owner: &str) -> Result<Vec<Artifact>, ArtifactError> {
        let connection = self.connection()?;

        let mut statement = connection
            .prepare(
                "SELECT id, workspace_id, display_name, media_type, size_bytes, sha256,
                        created_at_unix, expires_at_unix, revoked_at_unix
                 FROM artifacts WHERE owner_id = ?1 ORDER BY created_at_unix DESC LIMIT 200",
            )
            .map_err(|error| ArtifactError::Database(error.to_string()))?;

        let rows = statement
            .query_map(params![owner], |row| {
                Ok(Artifact {
                    id: row.get(0)?,
                    workspace_id: row.get(1)?,
                    name: row.get(2)?,
                    media_type: row.get(3)?,
                    size_bytes: row.get::<_, i64>(4)? as u64,
                    sha256: row.get(5)?,
                    created_at_unix: row.get(6)?,
                    expires_at_unix: row.get(7)?,
                    revoked: row.get::<_, Option<i64>>(8)?.is_some(),
                })
            })
            .map_err(|error| ArtifactError::Database(error.to_string()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| ArtifactError::Database(error.to_string()))
    }

    pub fn revoke(&self, owner: &str, id: &str) -> Result<Artifact, ArtifactError> {
        let artifact = self.owned(owner, id)?;

        let connection = self.connection()?;

        connection
            .execute(
                "UPDATE artifacts SET revoked_at_unix = unixepoch() WHERE id = ?1",
                params![id],
            )
            .map_err(|error| ArtifactError::Database(error.to_string()))?;

        connection
            .execute(
                "UPDATE artifact_links SET revoked_at_unix = unixepoch()
                 WHERE artifact_id = ?1 AND revoked_at_unix IS NULL",
                params![id],
            )
            .map_err(|error| ArtifactError::Database(error.to_string()))?;

        Ok(Artifact {
            revoked: true,
            ..artifact
        })
    }

    fn create_link(
        &self,
        artifact_id: &str,
        ttl_seconds: u64,
    ) -> Result<(String, i64), ArtifactError> {
        let connection = self.connection()?;
        let token = format!(
            "alx_at_{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                crate::crypto::random_handle()
                    .map_err(|_| ArtifactError::Database("cannot generate token".into()))?
                    .as_bytes()
            )
        );

        let expires_at: i64 = connection
            .query_row(
                // The link can never outlive the artifact, even if a caller
                // supplies a longer TTL.
                "SELECT min(unixepoch() + ?1, COALESCE(
                     (SELECT expires_at_unix FROM artifacts WHERE id = ?2),
                     unixepoch()
                 ))",
                params![ttl_seconds as i64, artifact_id],
                |row| row.get(0),
            )
            .map_err(|error| ArtifactError::Database(error.to_string()))?;

        connection
            .execute(
                "INSERT INTO artifact_links
                 (token_hash, artifact_id, created_at_unix, expires_at_unix)
                 VALUES (?1, ?2, unixepoch(), ?3)",
                params![
                    &crate::crypto::hash_handle(&token)[..],
                    artifact_id,
                    expires_at
                ],
            )
            .map_err(|error| ArtifactError::Database(error.to_string()))?;

        Ok((token, expires_at))
    }

    /// Resolve a download token to a live artifact.
    pub fn resolve_link(&self, token: &str) -> Result<(Artifact, String), ArtifactError> {
        if token.len() > 256 || !token.starts_with("alx_at_") {
            return Err(ArtifactError::LinkInvalid);
        }

        let connection = self.connection()?;

        let record = connection
            .query_row(
                "SELECT a.id, a.workspace_id, a.display_name, a.media_type, a.size_bytes,
                        a.sha256, a.created_at_unix, a.expires_at_unix,
                        a.revoked_at_unix, a.stored_name
                 FROM artifact_links l
                 JOIN artifacts a ON a.id = l.artifact_id
                 WHERE l.token_hash = ?1
                   AND l.revoked_at_unix IS NULL
                   AND l.expires_at_unix > unixepoch()
                   AND a.revoked_at_unix IS NULL
                   AND a.expires_at_unix > unixepoch()",
                params![&crate::crypto::hash_handle(token)[..]],
                |row| {
                    Ok((
                        Artifact {
                            id: row.get(0)?,
                            workspace_id: row.get(1)?,
                            name: row.get(2)?,
                            media_type: row.get(3)?,
                            size_bytes: row.get::<_, i64>(4)? as u64,
                            sha256: row.get(5)?,
                            created_at_unix: row.get(6)?,
                            expires_at_unix: row.get(7)?,
                            revoked: row.get::<_, Option<i64>>(8)?.is_some(),
                        },
                        row.get::<_, String>(9)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| ArtifactError::Database(error.to_string()))?;

        record.ok_or(ArtifactError::LinkInvalid)
    }

    /// Delete metadata for expired artifacts and return their identifiers.
    pub fn take_expired(&self) -> Result<Vec<String>, ArtifactError> {
        let connection = self.connection()?;

        let mut statement = connection
            .prepare(
                "SELECT id FROM artifacts
                 WHERE expires_at_unix <= unixepoch() LIMIT 50",
            )
            .map_err(|error| ArtifactError::Database(error.to_string()))?;

        let ids: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .map_err(|error| ArtifactError::Database(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ArtifactError::Database(error.to_string()))?;

        for id in &ids {
            connection
                .execute("DELETE FROM artifacts WHERE id = ?1", params![id])
                .map_err(|error| ArtifactError::Database(error.to_string()))?;
        }

        Ok(ids)
    }
}

/// Artifact service.
pub struct Artifacts {
    store: ArtifactStore,
    files: Arc<Files>,
    settings: ArtifactSettings,
}

impl Artifacts {
    pub fn new(store: ArtifactStore, files: Arc<Files>, settings: ArtifactSettings) -> Self {
        Self {
            store,
            files,
            settings,
        }
    }

    pub fn store(&self) -> &ArtifactStore {
        &self.store
    }

    /// Publish a workspace file as an immutable artifact.
    pub async fn publish(
        &self,
        owner: &str,
        workspace_id: &str,
        path: &str,
        name: Option<&str>,
    ) -> Result<Artifact, ArtifactError> {
        if path.is_empty() {
            return Err(ArtifactError::Invalid("a file path is required"));
        }

        let display_name = match name {
            Some(name) => sanitise_name(name)?,
            None => {
                let base = path.rsplit('/').next().unwrap_or(path);
                sanitise_name(base)?
            }
        };

        let id = uuid::Uuid::new_v4().to_string();
        let stored_name = format!("file-{display_name}");

        let copied = self
            .files
            .copy_to_artifacts(owner, workspace_id, path, &id, &stored_name, false)
            .await?;

        if copied.size > MAX_ARTIFACT_BYTES {
            let _ = self.files.remove_artifact(&id).await;
            return Err(ArtifactError::TooLarge);
        }

        let now = unix_now();

        let artifact = Artifact {
            id: id.clone(),
            workspace_id: Some(workspace_id.to_owned()),
            name: display_name.clone(),
            size_bytes: copied.size,
            media_type: media_type(&display_name).to_owned(),
            sha256: copied.sha256,
            created_at_unix: now,
            expires_at_unix: now + self.settings.ttl_seconds as i64,
            revoked: false,
        };

        if let Err(error) = self.store.insert(&artifact, owner, &stored_name) {
            let _ = self.files.remove_artifact(&id).await;
            return Err(error);
        }

        Ok(artifact)
    }

    pub fn list(&self, owner: &str) -> Result<Vec<Artifact>, ArtifactError> {
        self.store.list(owner)
    }

    /// Create an expiring, revocable download link.
    pub fn download_link(
        &self,
        owner: &str,
        artifact_id: &str,
        ttl_seconds: Option<u64>,
    ) -> Result<DownloadLink, ArtifactError> {
        let artifact = self.store.owned(owner, artifact_id)?;

        if artifact.revoked || artifact.expires_at_unix <= unix_now() {
            return Err(ArtifactError::LinkInvalid);
        }

        let remaining = (artifact.expires_at_unix - unix_now()).max(1) as u64;
        let ttl = link_ttl(
            ttl_seconds,
            self.settings.link_ttl_seconds,
            self.settings.ttl_seconds,
            remaining,
        );

        let (token, expires_at) = self.store.create_link(artifact_id, ttl)?;

        Ok(DownloadLink {
            artifact_id: artifact_id.to_owned(),
            url: format!(
                "{}/artifacts/{}",
                self.settings.public_base.trim_end_matches('/'),
                token
            ),
            expires_at_unix: expires_at,
            sha256: artifact.sha256,
            size_bytes: artifact.size_bytes,
        })
    }

    pub fn revoke(&self, owner: &str, artifact_id: &str) -> Result<Artifact, ArtifactError> {
        self.store.revoke(owner, artifact_id)
    }

    /// Resolve a public download token.
    pub fn resolve(&self, token: &str) -> Result<(Artifact, String), ArtifactError> {
        self.store.resolve_link(token)
    }

    /// Read part of an artifact.
    pub async fn read(
        &self,
        artifact_id: &str,
        stored_name: &str,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, ArtifactError> {
        Ok(self
            .files
            .read_artifact(artifact_id, stored_name, offset, length)
            .await?)
    }

    /// Delete expired artifacts and their files.
    pub async fn cleanup(&self) -> Result<usize, ArtifactError> {
        let expired = self.store.take_expired()?;

        for id in &expired {
            let _ = self.files.remove_artifact(id).await;
        }

        Ok(expired.len())
    }
}

fn sanitise_name(name: &str) -> Result<String, ArtifactError> {
    let name = name.trim();

    if name.is_empty() || name.len() > 120 || name.chars().any(char::is_control) {
        return Err(ArtifactError::Invalid("invalid artifact name"));
    }

    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(ArtifactError::Invalid("artifact name must be a file name"));
    }

    let sanitised: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || "._- ()[]".contains(character) {
                character
            } else {
                '_'
            }
        })
        .collect();

    Ok(sanitised)
}

/// Best-effort media type from the file name.
pub fn media_type(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();

    match lower.rsplit_once('.').map(|(_, extension)| extension) {
        Some("txt") => "text/plain; charset=utf-8",
        Some("md") => "text/markdown; charset=utf-8",
        Some("json") => "application/json",
        Some("toml") => "application/toml",
        Some("rs") | Some("py") | Some("c") | Some("h") | Some("cpp") => {
            "text/plain; charset=utf-8"
        }
        Some("zip") => "application/zip",
        Some("gz") | Some("tgz") => "application/gzip",
        Some("tar") => "application/x-tar",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("mp4") => "video/mp4",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        _ => "application/octet-stream",
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

/// Effective link lifetime: the requested value, bounded by the link policy and
/// by what is left of the artifact's own lifetime.
fn link_ttl(
    requested: Option<u64>,
    link_ttl_seconds: u64,
    artifact_ttl: u64,
    remaining: u64,
) -> u64 {
    let requested = requested.unwrap_or(link_ttl_seconds);

    requested
        .clamp(1, link_ttl_seconds.max(1))
        .min(artifact_ttl.max(1))
        .min(remaining.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitised_to_file_names() {
        assert_eq!(sanitise_name("report.txt").unwrap(), "report.txt");
        assert_eq!(sanitise_name("a/b").unwrap_err().code(), "invalid_request");
        assert_eq!(sanitise_name("..").unwrap_err().code(), "invalid_request");
        assert_eq!(sanitise_name("with space.pdf").unwrap(), "with space.pdf");
        assert_eq!(sanitise_name("semi;colon").unwrap(), "semi_colon");
    }

    #[test]
    fn media_types_are_derived_from_the_extension() {
        assert_eq!(media_type("a.zip"), "application/zip");
        assert_eq!(media_type("a.rs"), "text/plain; charset=utf-8");
        assert_eq!(media_type("a.unknown"), "application/octet-stream");
    }

    fn seeded_store() -> (tempfile::TempDir, ArtifactStore) {
        let directory = tempfile::tempdir().unwrap();

        crate::storage::initialize(directory.path()).unwrap();

        let connection = crate::storage::open(directory.path()).unwrap();
        connection
            .execute_batch(
                "INSERT INTO users (id, issuer, subject, created_at_unix, updated_at_unix)
                 VALUES ('owner-1', 'https://auth.example.com', 'subject-1', 0, 0);
                 INSERT INTO users (id, issuer, subject, created_at_unix, updated_at_unix)
                 VALUES ('owner-2', 'https://auth.example.com', 'subject-2', 0, 0);
                 INSERT INTO artifacts (
                    id, owner_id, workspace_id, display_name, stored_name, media_type,
                    size_bytes, sha256, created_at_unix, expires_at_unix
                 ) VALUES (
                    'artifact-1', 'owner-1', NULL, 'report.txt', 'file-report.txt',
                    'text/plain', 5, 'abc', 1, 9999999999
                 );",
            )
            .unwrap();
        drop(connection);

        let store = ArtifactStore::new(directory.path());

        (directory, store)
    }

    #[test]
    fn artifacts_are_isolated_between_owners() {
        let (_directory, store) = seeded_store();

        assert!(matches!(
            store.owned("owner-2", "artifact-1"),
            Err(ArtifactError::Forbidden)
        ));
        assert!(store.list("owner-2").unwrap().is_empty());
        assert_eq!(store.list("owner-1").unwrap().len(), 1);
        assert!(matches!(
            store.revoke("owner-2", "artifact-1"),
            Err(ArtifactError::Forbidden)
        ));
        assert!(store.owned("owner-1", "artifact-1").is_ok());
    }

    #[test]
    fn a_link_never_outlives_its_artifact() {
        // Plenty of artifact lifetime left: the policy applies.
        assert_eq!(link_ttl(Some(3600), 600, 86_400, 86_400), 600);
        // The artifact expires in 30 seconds: the link must not outlive it.
        assert_eq!(link_ttl(Some(3600), 600, 86_400, 30), 30);
        // An artifact with one second left still gets a usable, short link.
        assert_eq!(link_ttl(None, 600, 86_400, 1), 1);
        // A requested zero is still a positive lifetime.
        assert_eq!(link_ttl(Some(0), 600, 86_400, 500), 1);
    }
}
