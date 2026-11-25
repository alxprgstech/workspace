//! HTTP API for workspace files and artifacts.

use axum::{
    Extension, Json,
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::AppState;
use crate::artifacts::ArtifactError;
use crate::auth_store::SessionInfo;
use crate::files::FilesError;
use crate::workspaces::valid_id;

fn session(extension: Option<Extension<Arc<SessionInfo>>>) -> Result<Arc<SessionInfo>, Response> {
    extension
        .map(|Extension(info)| info)
        .ok_or_else(|| StatusCode::UNAUTHORIZED.into_response())
}

fn files_error(error: FilesError) -> Response {
    let status = StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    (
        status,
        Json(json!({"error": error.code(), "message": error.to_string()})),
    )
        .into_response()
}

fn artifact_error(error: ArtifactError) -> Response {
    let status = StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    (
        status,
        Json(json!({"error": error.code(), "message": error.to_string()})),
    )
        .into_response()
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "not_available", "message": "file service is not configured"})),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    path: String,
    #[serde(default)]
    depth: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct ReadQuery {
    path: String,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    length: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct WriteBody {
    path: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    content_base64: Option<String>,
    #[serde(default)]
    expected_sha256: Option<String>,
    #[serde(default)]
    create: bool,
}

#[derive(Debug, Deserialize)]
pub struct PatchBody {
    path: String,
    patch: String,
    #[serde(default)]
    expected_sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PublishBody {
    path: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LinkQuery {
    #[serde(default)]
    ttl_seconds: Option<u64>,
}

pub async fn list(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(workspace_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    if !valid_id(&workspace_id) {
        return files_error(FilesError::NotFound);
    }

    let Some(files) = state.files.as_ref() else {
        return unavailable();
    };

    match files
        .list(
            &session.user.id,
            &workspace_id,
            &query.path,
            query.depth.unwrap_or(2),
            query.limit.unwrap_or(500),
        )
        .await
    {
        Ok(listing) => Json(json!(listing)).into_response(),
        Err(error) => files_error(error),
    }
}

pub async fn read(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(workspace_id): Path<String>,
    Query(query): Query<ReadQuery>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    if !valid_id(&workspace_id) {
        return files_error(FilesError::NotFound);
    }

    let Some(files) = state.files.as_ref() else {
        return unavailable();
    };

    match files
        .read(
            &session.user.id,
            &workspace_id,
            &query.path,
            query.offset.unwrap_or(0),
            query.length.unwrap_or(64 * 1024),
        )
        .await
    {
        Ok(chunk) => Json(json!(chunk)).into_response(),
        Err(error) => files_error(error),
    }
}

pub async fn write(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(workspace_id): Path<String>,
    Json(body): Json<WriteBody>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    if !valid_id(&workspace_id) {
        return files_error(FilesError::NotFound);
    }

    let Some(files) = state.files.as_ref() else {
        return unavailable();
    };

    let content = match (body.content, body.content_base64) {
        (Some(text), None) => text.into_bytes(),
        (None, Some(encoded)) => {
            use base64::Engine;

            match base64::engine::general_purpose::STANDARD.decode(encoded) {
                Ok(bytes) => bytes,
                Err(_) => return files_error(FilesError::Invalid("content_base64 is invalid")),
            }
        }
        _ => {
            return files_error(FilesError::Invalid(
                "provide exactly one of content or content_base64",
            ));
        }
    };

    match files
        .write(
            &session.user.id,
            &workspace_id,
            &body.path,
            &content,
            body.expected_sha256.as_deref(),
            body.create,
            !body.create,
        )
        .await
    {
        Ok(outcome) => Json(json!(outcome)).into_response(),
        Err(error) => files_error(error),
    }
}

pub async fn patch(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(workspace_id): Path<String>,
    Json(body): Json<PatchBody>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    if !valid_id(&workspace_id) {
        return files_error(FilesError::NotFound);
    }

    let Some(files) = state.files.as_ref() else {
        return unavailable();
    };

    match files
        .patch(
            &session.user.id,
            &workspace_id,
            &body.path,
            &body.patch,
            body.expected_sha256.as_deref(),
        )
        .await
    {
        Ok(outcome) => Json(json!(outcome)).into_response(),
        Err(error) => files_error(error),
    }
}

pub async fn upload(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(workspace_id): Path<String>,
    Query(query): Query<ListQuery>,
    body: Body,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    if !valid_id(&workspace_id) {
        return files_error(FilesError::NotFound);
    }

    let Some(files) = state.files.as_ref() else {
        return unavailable();
    };

    let Some(host) = state.runtime.host().cloned() else {
        return unavailable();
    };

    let staging = files.staging_name();
    let staging_path = match files.staging_path(&staging) {
        Ok(path) => path,
        Err(error) => return files_error(error),
    };

    // Stream the request body straight into the dedicated filesystem.
    let mut command = match host.stream_stdin_command("/usr/bin/tee", &[staging_path]) {
        Ok(command) => command,
        Err(error) => return files_error(FilesError::Storage(error.to_string())),
    };

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return files_error(FilesError::Storage(error.to_string())),
    };

    let Some(mut stdin) = child.stdin.take() else {
        return files_error(FilesError::Storage("cannot open upload pipe".into()));
    };

    let limit = state.max_import_bytes;
    let mut total = 0u64;
    let mut stream = body.into_data_stream();

    use futures_util::StreamExt;

    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            let _ = child.kill().await;
            let _ = files.discard_staged(&staging).await;
            return files_error(FilesError::Storage("upload aborted".into()));
        };

        total += chunk.len() as u64;

        if total > limit {
            let _ = child.kill().await;
            let _ = files.discard_staged(&staging).await;
            return files_error(FilesError::TooLarge);
        }

        if stdin.write_all(&chunk).await.is_err() {
            break;
        }
    }

    drop(stdin);

    if !matches!(child.wait().await, Ok(status) if status.success()) {
        let _ = files.discard_staged(&staging).await;
        return files_error(FilesError::Storage("upload failed".into()));
    }

    match files
        .import_staged(&session.user.id, &workspace_id, &staging, &query.path, true)
        .await
    {
        Ok(outcome) => Json(json!(outcome)).into_response(),
        Err(error) => files_error(error),
    }
}

pub async fn publish(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(workspace_id): Path<String>,
    Json(body): Json<PublishBody>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    if !valid_id(&workspace_id) {
        return artifact_error(ArtifactError::NotFound);
    }

    let Some(artifacts) = state.artifacts.as_ref() else {
        return unavailable();
    };

    match artifacts
        .publish(
            &session.user.id,
            &workspace_id,
            &body.path,
            body.name.as_deref(),
        )
        .await
    {
        Ok(artifact) => (StatusCode::CREATED, Json(json!(artifact))).into_response(),
        Err(error) => artifact_error(error),
    }
}

pub async fn artifacts_list(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    let Some(artifacts) = state.artifacts.as_ref() else {
        return unavailable();
    };

    match artifacts.list(&session.user.id) {
        Ok(artifacts) => Json(json!({ "artifacts": artifacts })).into_response(),
        Err(error) => artifact_error(error),
    }
}

pub async fn artifact_link(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(artifact_id): Path<String>,
    Query(query): Query<LinkQuery>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    let Some(artifacts) = state.artifacts.as_ref() else {
        return unavailable();
    };

    match artifacts.download_link(&session.user.id, &artifact_id, query.ttl_seconds) {
        Ok(link) => Json(json!(link)).into_response(),
        Err(error) => artifact_error(error),
    }
}

pub async fn artifact_revoke(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(artifact_id): Path<String>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    let Some(artifacts) = state.artifacts.as_ref() else {
        return unavailable();
    };

    match artifacts.revoke(&session.user.id, &artifact_id) {
        Ok(artifact) => Json(json!(artifact)).into_response(),
        Err(error) => artifact_error(error),
    }
}

/// Public, token-authorised artifact download.
pub async fn download(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let Some(artifacts) = state.artifacts.as_ref() else {
        return unavailable();
    };

    let (artifact, stored_name) = match artifacts.resolve(&token) {
        Ok(resolved) => resolved,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    // Stream the file through a duplex pipe so memory stays bounded.
    let (mut writer, reader) = tokio::io::duplex(128 * 1024);
    let artifacts = Arc::clone(artifacts);
    let id = artifact.id.clone();
    let name = stored_name.clone();

    tokio::spawn(async move {
        let mut offset = 0u64;

        loop {
            let chunk = match artifacts.read(&id, &name, offset, 1024 * 1024).await {
                Ok(chunk) => chunk,
                Err(_) => break,
            };

            if chunk.is_empty() {
                break;
            }

            offset += chunk.len() as u64;

            if writer.write_all(&chunk).await.is_err() {
                break;
            }
        }

        let _ = writer.shutdown().await;
    });

    let body = Body::from_stream(ReaderStream::new(reader));

    (
        [
            (header::CONTENT_TYPE, artifact.media_type.clone()),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"{}\"",
                    artifact.name.replace('"', "")
                ),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        body,
    )
        .into_response()
}
