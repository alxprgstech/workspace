//! Administrator API for checking and applying updates.
//!
//! Every handler proves the caller is an authenticated administrator on the
//! server side. The browser interface is a convenience, not the security
//! boundary. No request can name a command, a URL or a filesystem path: the
//! only choices are "check", "apply the verified release" and "restore the
//! recorded backup".

use super::{RollbackOptions, UpdateError, UpdateOptions, start_rollback, start_update, status};
use crate::auth_store::SessionInfo;
use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use workspace_core::config::Config;

#[derive(Clone)]
pub struct UpdateHttpState {
    pub config: Arc<Config>,
    pub config_path: PathBuf,
    busy: Arc<AtomicBool>,
}

impl UpdateHttpState {
    pub fn new(config: Arc<Config>, config_path: PathBuf) -> Self {
        Self {
            config,
            config_path,
            busy: Arc::new(AtomicBool::new(false)),
        }
    }

    #[allow(clippy::result_large_err)]
    fn require_admin(&self, request: &Request) -> Result<String, Response> {
        let session = request.extensions().get::<Arc<SessionInfo>>().cloned();

        match session {
            Some(session) if session.user.role == "admin" && session.user.is_enabled => {
                Ok(session.user.subject.clone())
            }
            Some(_) => Err(forbidden("administrator role required")),
            None => Err(forbidden("authentication required")),
        }
    }

    #[allow(clippy::result_large_err)]
    fn begin(&self) -> Result<(), Response> {
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(conflict("update_in_progress", "another update is starting"));
        }

        Ok(())
    }

    fn end(&self) {
        self.busy.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ApplyRequest {
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    active_jobs_confirmed: bool,
    #[serde(default)]
    allow_downgrade: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RollbackRequest {
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    allow_database_restore: bool,
}

#[derive(Debug, Serialize)]
struct StartedResponse {
    started: bool,
    action: &'static str,
    journal: super::Journal,
    journal_path: String,
    updater_pid: Option<u32>,
    completed: bool,
    message: String,
}

pub fn router(state: Arc<UpdateHttpState>) -> Router {
    Router::new()
        .route("/api/v1/admin/update", get(update_status))
        .route("/api/v1/admin/update/check", post(update_check))
        .route("/api/v1/admin/update/apply", post(update_apply))
        .route("/api/v1/admin/update/rollback", post(update_rollback))
        .with_state(state)
}

async fn update_status(State(state): State<Arc<UpdateHttpState>>, request: Request) -> Response {
    if let Err(response) = state.require_admin(&request) {
        return response;
    }

    let config = Arc::clone(&state.config);
    let config_path = state.config_path.clone();

    let result = tokio::task::spawn_blocking(move || status(&config, &config_path)).await;

    match result {
        Ok(Ok(report)) => {
            let mut value = serde_json::to_value(report).unwrap_or(Value::Null);

            if let Some(object) = value.as_object_mut() {
                object.insert("admin".into(), json!(true));
                object.insert("update_supported".into(), json!(cfg!(windows)));
            }

            Json(value).into_response()
        }
        Ok(Err(error)) => error_response(&error),
        Err(error) => internal(error.to_string()),
    }
}

async fn update_check(State(state): State<Arc<UpdateHttpState>>, request: Request) -> Response {
    let actor = match state.require_admin(&request) {
        Ok(actor) => actor,
        Err(response) => return response,
    };

    let config = Arc::clone(&state.config);
    let config_path = state.config_path.clone();

    let options = UpdateOptions::default();

    match super::check(&config, &config_path, &options).await {
        Ok(report) => {
            super::audit_event(
                &config,
                &config_path,
                "update.check",
                &actor,
                &report.status,
            );

            Json(report).into_response()
        }
        Err(error) => error_response(&error),
    }
}

async fn update_apply(State(state): State<Arc<UpdateHttpState>>, request: Request) -> Response {
    let actor = match state.require_admin(&request) {
        Ok(actor) => actor,
        Err(response) => return response,
    };

    let body: ApplyRequest = match read_json(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };

    if !body.confirm {
        return conflict(
            "confirmation_required",
            "the update must be confirmed explicitly",
        );
    }

    let active = super::platform::active_jobs(&state.config.storage.data_dir).unwrap_or(0);

    if active > 0 && !body.active_jobs_confirmed {
        return conflict(
            "active_jobs",
            &format!(
                "{active} job(s) are queued or running; confirm that the update may wait for them"
            ),
        );
    }

    if let Err(response) = state.begin() {
        return response;
    }

    let options = UpdateOptions {
        package: None,
        install_dir: None,
        force: false,
        allow_downgrade: body.allow_downgrade,
        drain_timeout: None,
        bootstrap: false,
        wait_for_invoker: true,
    };

    let result = start_update(&state.config, &state.config_path, &options, &actor).await;

    state.end();

    match result {
        Ok(report) => (
            StatusCode::ACCEPTED,
            Json(StartedResponse {
                started: true,
                action: "update",
                journal: report.journal,
                journal_path: report.journal_path,
                updater_pid: report.updater_pid,
                completed: report.completed,
                message: if report.completed {
                    "the update finished in this process".into()
                } else {
                    "the updater started; the server will restart".into()
                },
            }),
        )
            .into_response(),
        Err(error) => error_response(&error),
    }
}

async fn update_rollback(State(state): State<Arc<UpdateHttpState>>, request: Request) -> Response {
    let actor = match state.require_admin(&request) {
        Ok(actor) => actor,
        Err(response) => return response,
    };

    let body: RollbackRequest = match read_json(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };

    if !body.confirm {
        return conflict(
            "confirmation_required",
            "the rollback must be confirmed explicitly",
        );
    }

    if let Err(response) = state.begin() {
        return response;
    }

    let options = RollbackOptions {
        backup: None,
        install_dir: None,
        allow_database_restore: body.allow_database_restore,
        force: false,
    };

    let result = start_rollback(&state.config, &state.config_path, &options, &actor).await;

    state.end();

    match result {
        Ok(report) => (
            StatusCode::ACCEPTED,
            Json(StartedResponse {
                started: true,
                action: "rollback",
                journal: report.journal,
                journal_path: report.journal_path,
                updater_pid: report.updater_pid,
                completed: report.completed,
                message: "the rollback started; the server will restart".into(),
            }),
        )
            .into_response(),
        Err(error) => error_response(&error),
    }
}

#[allow(clippy::result_large_err)]
async fn read_json<T: for<'de> Deserialize<'de>>(request: Request) -> Result<T, Response> {
    let bytes = axum::body::to_bytes(request.into_body(), 16 * 1024)
        .await
        .map_err(|_| conflict("invalid_request", "request body is too large"))?;

    serde_json::from_slice(&bytes)
        .map_err(|_| conflict("invalid_request", "the request body is not valid JSON"))
}

fn forbidden(message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": "admin_required", "message": message })),
    )
        .into_response()
}

fn conflict(code: &str, message: &str) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": code, "message": message })),
    )
        .into_response()
}

fn internal(message: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal", "message": message })),
    )
        .into_response()
}

fn error_response(error: &UpdateError) -> Response {
    use UpdateError::*;

    let (status, code) = match error {
        Busy | Platform(super::PlatformError::Busy) => (StatusCode::CONFLICT, "update_in_progress"),
        NoUpdate => (StatusCode::CONFLICT, "no_update"),
        Incompatible(_) => (StatusCode::CONFLICT, "incompatible"),
        Secrets(_) => (StatusCode::CONFLICT, "secrets_unavailable"),
        Manifest(super::ManifestError::Signature | super::ManifestError::Untrusted) => {
            (StatusCode::UNPROCESSABLE_ENTITY, "release_untrusted")
        }
        Manifest(super::ManifestError::File { .. }) => {
            (StatusCode::UNPROCESSABLE_ENTITY, "release_corrupt")
        }
        Manifest(_) => (StatusCode::UNPROCESSABLE_ENTITY, "release_invalid"),
        Download(_) => (StatusCode::BAD_GATEWAY, "download_failed"),
        Platform(super::PlatformError::Wsl(_)) => {
            (StatusCode::SERVICE_UNAVAILABLE, "wsl_unavailable")
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    };

    (
        status,
        Json(json!({ "error": code, "message": error.to_string() })),
    )
        .into_response()
}
