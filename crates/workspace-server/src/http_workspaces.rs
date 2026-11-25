//! HTTP API for workspaces.
//!
//! Every handler runs behind `require_session`, so the owner identity always
//! comes from the verified session, never from the request body.

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::sync::Arc;

use crate::AppState;
use crate::auth_store::SessionInfo;
use crate::workspaces::valid_id;
use crate::workspaces::{CreateRequest, UpdateRequest, WorkspaceError};

fn problem(error: WorkspaceError) -> Response {
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

pub async fn list(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    match state.workspaces.list(&session.user.id) {
        Ok(workspaces) => Json(json!({ "workspaces": workspaces })).into_response(),
        Err(error) => problem(error),
    }
}

pub async fn create(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Json(request): Json<CreateRequest>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    let runtime = state.runtime.status().await;

    if !runtime.infrastructure_ready {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "runtime_not_ready",
                "message": "the execution runtime is not ready",
                "blocking": runtime.blocking,
            })),
        )
            .into_response();
    }

    let becomes_persistent = match request.execution_mode.as_deref() {
        Some(mode) => mode == "persistent",
        None => request.persistent.unwrap_or(false),
    };

    if becomes_persistent && !runtime.execution_ready {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "runtime_not_ready",
                "message": "persistent workspaces need a proven execution runtime",
                "blocking": runtime.blocking,
            })),
        )
            .into_response();
    }

    match state.workspaces.reserve(&session.user.id, &request).await {
        Ok(workspace) => {
            // Image preparation may take minutes; the client polls
            // workspace_info until the workspace leaves `preparing`.
            let manager = Arc::clone(&state.workspaces);
            let id = workspace.id.clone();

            tokio::spawn(async move {
                if let Err(error) = manager.prepare(&id).await {
                    tracing::warn!(%error, workspace = id, "Workspace preparation failed");
                }
            });

            (StatusCode::CREATED, Json(json!(workspace))).into_response()
        }
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

    if !valid_id(&id) {
        return problem(WorkspaceError::NotFound);
    }

    let workspace = match state.workspaces.get_owned(&session.user.id, &id).await {
        Ok(workspace) => workspace,
        Err(error) => return problem(error),
    };

    let usage = state.workspaces.usage(&workspace).await.ok();

    Json(json!({
        "workspace": workspace,
        "usage": usage,
        "ttl_remaining_seconds": workspace.ttl_remaining_seconds(now()),
    }))
    .into_response()
}

pub async fn update(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(id): Path<String>,
    Json(request): Json<UpdateRequest>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    if !valid_id(&id) {
        return problem(WorkspaceError::NotFound);
    }

    match state
        .workspaces
        .update(&session.user.id, &id, &request)
        .await
    {
        Ok(workspace) => Json(json!(workspace)).into_response(),
        Err(error) => problem(error),
    }
}

pub async fn stop(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(id): Path<String>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    if !valid_id(&id) {
        return problem(WorkspaceError::NotFound);
    }

    match state.workspaces.stop(&session.user.id, &id).await {
        Ok(workspace) => Json(json!(workspace)).into_response(),
        Err(error) => problem(error),
    }
}

pub async fn start(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(id): Path<String>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    if !valid_id(&id) {
        return problem(WorkspaceError::NotFound);
    }

    let runtime = state.runtime.status().await;

    let workspace = match state.workspaces.owned(&session.user.id, &id) {
        Ok(workspace) => workspace,
        Err(error) => return problem(error),
    };

    if workspace.execution_mode == workspace_core::workspace::ExecutionMode::Persistent
        && !runtime.execution_ready
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "runtime_not_ready",
                "message": "persistent workspaces need a proven execution runtime",
                "blocking": runtime.blocking,
            })),
        )
            .into_response();
    }

    match state.workspaces.start(&session.user.id, &id).await {
        Ok(workspace) => Json(json!(workspace)).into_response(),
        Err(error) => problem(error),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
    Path(id): Path<String>,
) -> Response {
    let session = match session(extension) {
        Ok(session) => session,
        Err(response) => return response,
    };

    if !valid_id(&id) {
        return problem(WorkspaceError::NotFound);
    }

    match state.workspaces.delete(&session.user.id, &id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => problem(error),
    }
}

pub async fn runtime_status(
    State(state): State<AppState>,
    extension: Option<Extension<Arc<SessionInfo>>>,
) -> Response {
    if let Err(response) = session(extension) {
        return response;
    }

    Json(json!(state.runtime.status().await)).into_response()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}
