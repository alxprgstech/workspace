//! HTTP API for jobs.

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::AppState;
use crate::auth_store::SessionInfo;
use crate::jobs::{ExecRequest, JobError};
use crate::workspaces::valid_id;

fn problem(error: JobError) -> Response {
    let status = StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    (
        status,
        Json(json!({
            "error": error.code(),
            "message": error.to_string(),
        })),
    )
        .into_response()
}

#[allow(clippy::result_large_err)]
fn session(extension: Option<Extension<Arc<SessionInfo>>>) -> Result<Arc<SessionInfo>, Response> {
    extension
        .map(|Extension(info)| info)
        .ok_or_else(|| StatusCode::UNAUTHORIZED.into_response())
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    workspace_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LogQuery {
    #[serde(default)]
    stream: Option<String>,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
}

pub async fn exec(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(workspace_id): Path<String>,
    Json(request): Json<ExecRequest>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    if !valid_id(&workspace_id) {
        return problem(JobError::NotFound);
    }

    match state
        .jobs
        .submit(&session.user.id, &workspace_id, request)
        .await
    {
        Ok(job) => (StatusCode::ACCEPTED, Json(json!(job))).into_response(),
        Err(error) => problem(error),
    }
}

pub async fn list(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Query(query): Query<ListQuery>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    if let Some(workspace_id) = query.workspace_id.as_deref()
        && !valid_id(workspace_id)
    {
        return problem(JobError::NotFound);
    }

    match state
        .jobs
        .list(&session.user.id, query.workspace_id.as_deref())
    {
        Ok(jobs) => Json(json!({ "jobs": jobs })).into_response(),
        Err(error) => problem(error),
    }
}

pub async fn info(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(id): Path<String>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    match state.jobs.owned_job(&session.user.id, &id) {
        Ok(job) => Json(json!(job)).into_response(),
        Err(error) => problem(error),
    }
}

pub async fn logs(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    let stream = query.stream.unwrap_or_else(|| "stdout".to_owned());
    let offset = query.offset.unwrap_or(0);
    let limit = query.limit.unwrap_or(64 * 1024);

    match state
        .jobs
        .logs(&session.user.id, &id, &stream, offset, limit)
    {
        Ok(page) => Json(json!(page)).into_response(),
        Err(error) => problem(error),
    }
}

pub async fn cancel(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(id): Path<String>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    match state.jobs.cancel(&session.user.id, &id).await {
        Ok(job) => Json(json!(job)).into_response(),
        Err(error) => problem(error),
    }
}
