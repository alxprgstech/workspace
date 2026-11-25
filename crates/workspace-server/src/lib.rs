#![forbid(unsafe_code)]

//! HTTP API, MCP gateway, authentication and administration.

#[cfg(not(feature = "web-ui"))]
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
#[cfg(feature = "web-ui")]
mod web;

pub mod artifacts;
mod auth_http;
pub mod auth_store;
pub mod crypto;
pub mod files;
mod http_files;
mod http_jobs;
mod http_workspaces;
pub mod jobs;
mod mcp;
pub mod mcp_auth;
pub mod oauth_store;
pub mod runtime_bridge;
pub mod secrets;
pub mod sso;
pub mod storage;
pub mod users;
pub mod workspaces;

use serde::Serialize;
use serde_json::{Value, json};
use std::{io, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use workspace_core::{PRODUCT_NAME, VERSION, config::Config};
use workspace_runtime::status::RuntimeMonitor;

/// How often the supervisor re-proves isolation while the server runs.
const ACCEPTANCE_REFRESH: Duration = Duration::from_secs(300);

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct ReadinessResponse {
    status: &'static str,
    runtime_ready: bool,
    infrastructure_ready: bool,
    execution_ready: bool,
    reason: String,
    blocking: Vec<String>,
    checks: Vec<workspace_runtime::StatusCheck>,
}

/// State shared by the HTTP API and, indirectly, by the MCP gateway.
#[derive(Clone)]
pub struct AppState {
    pub(crate) storage: storage::StorageInfo,
    pub(crate) resources: Value,
    pub(crate) runtime: Arc<RuntimeMonitor>,
    pub(crate) workspaces: Arc<workspaces::Manager>,
    pub(crate) jobs: Arc<jobs::Executor>,
    pub(crate) files: Option<Arc<files::Files>>,
    pub(crate) artifacts: Option<Arc<artifacts::Artifacts>>,
    pub(crate) max_import_bytes: u64,
}

impl AppState {
    /// MCP view of the same services the HTTP API uses.
    fn mcp_services(&self) -> Arc<mcp::GatewayServices> {
        Arc::new(mcp::GatewayServices {
            schema_version: self.storage.schema_version,
            runtime: Arc::clone(&self.runtime),
            workspaces: Some(Arc::clone(&self.workspaces)),
            jobs: Some(Arc::clone(&self.jobs)),
            files: self.files.clone(),
            artifacts: self.artifacts.clone(),
        })
    }

    fn new(config: &Config, storage: storage::StorageInfo, runtime: Arc<RuntimeMonitor>) -> Self {
        let resources = json!({
            "cpu_millis": config.resources.cpu_millis,
            "memory_mib": config.resources.memory_mib,
            "storage_mib": config.resources.storage_mib,
            "max_active_workspaces": config.resources.max_active_workspaces,
            "max_concurrent_jobs": config.resources.max_concurrent_jobs,
            "max_processes_per_workspace": config.resources.max_processes_per_workspace,
            "max_job_seconds": config.resources.max_job_seconds,
        });

        let budget = workspace_core::workspace::Limits {
            cpu_millis: config.resources.cpu_millis,
            memory_mib: config.resources.memory_mib,
            pids: config.resources.max_processes_per_workspace,
            disk_mib: config
                .runtime
                .as_ref()
                .map(|runtime| runtime.workspace_storage_mib)
                .unwrap_or(1024),
            files: config
                .runtime
                .as_ref()
                .map(|runtime| runtime.workspace_files)
                .unwrap_or(100_000),
            max_job_seconds: config.resources.max_job_seconds,
        };

        let settings = workspaces::ManagerSettings {
            budget,
            default_ttl_seconds: config.workspace.ttl_seconds,
            idle_timeout_seconds: config.workspace.idle_timeout_seconds,
            network_mode: match config.network.default_mode {
                workspace_core::config::NetworkMode::None => "none".to_owned(),
                workspace_core::config::NetworkMode::Allowlist => "allowlist".to_owned(),
            },
            base_image: config
                .runtime
                .as_ref()
                .map(|runtime| runtime.base_image.clone())
                .unwrap_or_default(),
        };

        let runtime_impl: Arc<dyn workspaces::WorkspaceRuntime> =
            match runtime_bridge::DockerWorkspaceRuntime::new(
                Arc::clone(&runtime),
                storage.instance_id.clone(),
            ) {
                Ok(bridge) => Arc::new(bridge),
                Err(error) => Arc::new(runtime_bridge::UnavailableRuntime::new(error.to_string())),
            };

        let manager = Arc::new(workspaces::Manager::new(
            workspaces::Store::new(config.storage.data_dir.clone()),
            runtime_impl,
            settings,
        ));

        let job_runtime: Arc<dyn jobs::JobRuntime> =
            match jobs::DockerJobRuntime::new(&runtime, storage.instance_id.clone()) {
                Ok(job_runtime) => Arc::new(job_runtime),
                Err(error) => Arc::new(jobs::UnavailableJobRuntime::new(error.to_string())),
            };

        let executor = jobs::Executor::new(
            jobs::JobStore::new(config.storage.data_dir.clone()),
            Arc::clone(&manager),
            job_runtime,
            Arc::clone(&runtime),
            jobs::ExecSettings {
                max_concurrent_jobs: config.resources.max_concurrent_jobs,
                default_timeout_seconds: config
                    .workspace
                    .ttl_seconds
                    .min(config.resources.max_job_seconds),
                max_timeout_seconds: config.resources.max_job_seconds,
            },
            storage.instance_id.clone(),
        );

        let max_import_bytes = config.storage.max_import_mib * 1024 * 1024;

        let files = files::Files::new(&runtime, Arc::clone(&manager), max_import_bytes)
            .ok()
            .map(Arc::new);

        let artifacts = files.as_ref().map(|files| {
            Arc::new(artifacts::Artifacts::new(
                artifacts::ArtifactStore::new(config.storage.data_dir.clone()),
                Arc::clone(files),
                artifacts::ArtifactSettings {
                    ttl_seconds: config.workspace.artifact_ttl_seconds,
                    link_ttl_seconds: 900.min(config.workspace.artifact_ttl_seconds),
                    public_base: public_base(config),
                },
            ))
        });

        Self {
            storage,
            resources,
            runtime,
            workspaces: manager,
            jobs: Arc::new(executor),
            files,
            artifacts,
            max_import_bytes,
        }
    }
}

/// Public origin used in artifact download links.
fn public_base(config: &Config) -> String {
    let candidate = config
        .mcp
        .as_ref()
        .map(|mcp| mcp.resource.clone())
        .or_else(|| config.sso.as_ref().map(|sso| sso.redirect_uri.clone()));

    if let Some(url) = candidate
        && let Ok(parsed) = openidconnect::reqwest::Url::parse(&url)
    {
        return parsed.origin().ascii_serialization();
    }

    format!("http://{}", config.server.listen)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn ready(State(state): State<AppState>) -> (StatusCode, Json<ReadinessResponse>) {
    // Readiness is a policy decision, not a guess: it is true only when the
    // dedicated runtime has passed its practical isolation acceptance.
    let runtime = state.runtime.status().await;

    let body = ReadinessResponse {
        status: if runtime.execution_ready {
            "ready"
        } else {
            "not_ready"
        },
        runtime_ready: runtime.execution_ready,
        infrastructure_ready: runtime.infrastructure_ready,
        execution_ready: runtime.execution_ready,
        reason: runtime.blocking.first().cloned().unwrap_or_default(),
        blocking: runtime.blocking.clone(),
        checks: runtime.checks.clone(),
    };

    let status = if runtime.execution_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (status, Json(body))
}

async fn server_info(State(state): State<AppState>) -> Json<Value> {
    let runtime = state.runtime.status().await;

    Json(json!({
        "product": PRODUCT_NAME,
        "version": VERSION,
        "api_version": "v1",
        "instance_id": state.storage.instance_id.clone(),
        "platform": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "schema_version": state.storage.schema_version,
        "runtime_ready": runtime.execution_ready,
        "storage": state.storage.clone(),
        "resources": state.resources.clone(),
        "runtime": runtime,
    }))
}

#[cfg(test)]
fn router(
    config: &Config,
    storage: storage::StorageInfo,
    auth: Option<Arc<auth_http::AuthState>>,
) -> Router {
    let monitor = Arc::new(RuntimeMonitor::new(
        config.runtime.as_ref(),
        &storage.instance_id,
    ));

    let state = AppState::new(config, storage, Arc::clone(&monitor));

    router_with_runtime(config, state, auth)
}

/// Router bound to a monitor shared with the background supervisor.
fn router_with_runtime(
    config: &Config,
    state: AppState,
    auth: Option<Arc<auth_http::AuthState>>,
) -> Router {
    let mcp_services = state.mcp_services();
    let upload_limit = state.max_import_bytes as usize;

    // Public download endpoint: authorisation is the opaque token itself.
    let public: Router = Router::new()
        .route("/artifacts/{token}", get(http_files::download))
        .with_state(state.clone());

    let protected: Router = Router::new()
        .route("/ready", get(ready))
        .route("/api/v1/server/info", get(server_info))
        .route("/api/v1/runtime", get(http_workspaces::runtime_status))
        .route(
            "/api/v1/workspaces",
            get(http_workspaces::list).post(http_workspaces::create),
        )
        .route(
            "/api/v1/workspaces/{id}",
            get(http_workspaces::info)
                .patch(http_workspaces::update)
                .delete(http_workspaces::delete),
        )
        .route(
            "/api/v1/workspaces/{id}/stop",
            axum::routing::post(http_workspaces::stop),
        )
        .route(
            "/api/v1/workspaces/{id}/start",
            axum::routing::post(http_workspaces::start),
        )
        .route(
            "/api/v1/workspaces/{id}/exec",
            axum::routing::post(http_jobs::exec),
        )
        .route("/api/v1/jobs", get(http_jobs::list))
        .route("/api/v1/jobs/{id}", get(http_jobs::info))
        .route("/api/v1/jobs/{id}/logs", get(http_jobs::logs))
        .route(
            "/api/v1/jobs/{id}/cancel",
            axum::routing::post(http_jobs::cancel),
        )
        .route("/api/v1/workspaces/{id}/files", get(http_files::list))
        .route(
            "/api/v1/workspaces/{id}/file",
            get(http_files::read).put(http_files::write),
        )
        .route(
            "/api/v1/workspaces/{id}/patch",
            axum::routing::post(http_files::patch),
        )
        .route(
            "/api/v1/workspaces/{id}/uploads",
            axum::routing::post(http_files::upload)
                .layer(axum::extract::DefaultBodyLimit::max(upload_limit)),
        )
        .route(
            "/api/v1/workspaces/{id}/artifacts",
            axum::routing::post(http_files::publish),
        )
        .route("/api/v1/artifacts", get(http_files::artifacts_list))
        .route(
            "/api/v1/artifacts/{id}/link",
            axum::routing::post(http_files::artifact_link),
        )
        .route(
            "/api/v1/artifacts/{id}/revoke",
            axum::routing::post(http_files::artifact_revoke),
        )
        .with_state(state);

    let protected = match auth.as_ref() {
        Some(auth_state) => protected.route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(auth_state),
            auth_http::require_session,
        )),
        None => protected,
    };

    let router = Router::new()
        .route("/health", get(health))
        .merge(public)
        .merge(protected);

    let router = match auth.as_ref() {
        Some(auth_state) => router.merge(auth_http::router(Arc::clone(auth_state))),
        None => router,
    };

    // No MCP surface at all unless the deployment declares its audience:
    // an unconfigured gateway must fail closed rather than answer with an
    // unverified resource.
    let router = match config.mcp.clone() {
        Some(mcp) => match (config.sso.as_ref(), auth.as_ref()) {
            (Some(sso), Some(auth_state)) => {
                match oauth_store::OAuthStore::new(&config.storage.data_dir, sso) {
                    Ok(store) if valid_local_mcp(config) => {
                        let store = Arc::new(store);

                        router
                            .merge(auth_http::oauth_router(
                                Arc::clone(auth_state),
                                Arc::clone(&store),
                            ))
                            .merge(mcp::local_router(mcp, Arc::clone(&mcp_services), store))
                    }
                    _ => {
                        tracing::error!("Invalid local MCP configuration");
                        router
                    }
                }
            }
            // Preserve the existing isolated external-token tests.
            _ => router.merge(mcp::router(mcp, mcp_services)),
        },
        None => router,
    };

    #[cfg(feature = "web-ui")]
    let router = router.merge(web::router());

    // The SSO callback always redirects to `/`. A build without the bundled
    // management interface must still answer there, otherwise a successful
    // sign-in would land on a 404.
    #[cfg(not(feature = "web-ui"))]
    let router = router.route("/", get(headless_index));

    router.layer(axum::middleware::from_fn(auth_http::security_headers))
}

/// Small, dependency-free landing page for headless builds.
#[cfg(not(feature = "web-ui"))]
async fn headless_index() -> Response {
    const PAGE: &str = concat!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">",
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">",
        "<title>ALXPRGS Workspace</title></head><body>",
        "<main style=\"font-family: system-ui, sans-serif; max-width: 32rem; ",
        "margin: 4rem auto; padding: 0 1rem; line-height: 1.5\">",
        "<h1>ALXPRGS Workspace</h1>",
        "<p>This is a headless build: the management interface is not bundled.</p>",
        "<p>You are signed in. Session details are available from ",
        "<code>GET /auth/me</code> and server details from ",
        "<code>GET /api/v1/server/info</code>.</p>",
        "</main></body></html>",
    );

    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        PAGE,
    )
        .into_response()
}

fn valid_local_mcp(config: &Config) -> bool {
    let Some(mcp) = config.mcp.as_ref() else {
        return true;
    };

    let Some(sso) = config.sso.as_ref() else {
        return false;
    };

    let Ok(url) = openidconnect::reqwest::Url::parse(&sso.redirect_uri) else {
        return false;
    };

    let issuer = url.origin().ascii_serialization();
    let resource = format!("{issuer}/chatgpt/mcp");

    mcp.issuer == issuer
        && mcp.resource == resource
        && mcp.audience == resource
        && mcp.required_scopes == [oauth_store::SCOPE]
}

/// Keep the practical isolation acceptance fresh while the server runs.
///
/// A stopped WSL2 distribution is woken by the same probe: the first pass
/// tolerates the cold start through the bounded engine retry, and later passes
/// only re-verify. Nothing here starts user workloads.
fn spawn_runtime_supervisor(runtime: Arc<RuntimeMonitor>, workspaces: Arc<workspaces::Manager>) {
    tokio::spawn(async move {
        if !runtime.configured() {
            tracing::warn!(
                reason = runtime.configuration_error().unwrap_or("not configured"),
                "Execution runtime is not configured; workloads stay disabled"
            );

            return;
        }

        // Wake the dedicated environment before the first probe; a stopped
        // WSL2 distribution is a normal state, not an outage.
        runtime.warm_up().await;

        let mut first = true;

        loop {
            let status = runtime.refresh().await;

            if status.infrastructure_ready {
                match runtime.run_acceptance().await {
                    Ok(report) if report.passed => {
                        tracing::info!(
                            duration_ms = report.duration_ms,
                            "Runtime isolation acceptance passed"
                        );
                    }
                    Ok(report) => {
                        tracing::error!(
                            blocking = ?report.checks.iter()
                                .filter(|check| check.required && !check.ok)
                                .map(|check| format!("{}: {}", check.id, check.detail))
                                .collect::<Vec<_>>(),
                            "Runtime isolation acceptance failed"
                        );
                    }
                    Err(error) => {
                        tracing::error!(%error, "Runtime isolation acceptance could not run");
                    }
                }
            } else if first {
                tracing::warn!(
                    blocking = ?status.blocking,
                    "Execution runtime is not ready; workloads stay disabled"
                );
            }

            first = false;

            // Retry quickly while the runtime is unavailable, so a cold start
            // or a restarted engine is picked up without waiting a full cycle.
            let delay = if status.execution_ready {
                ACCEPTANCE_REFRESH
            } else {
                Duration::from_secs(15)
            };

            tokio::time::sleep(delay).await;
        }
    });

    // TTL and idle-timeout enforcement run on a short, fixed cadence.
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;

            match workspaces.reap(now_unix()).await {
                Ok((deleted, stopped)) if deleted + stopped > 0 => {
                    tracing::info!(deleted, stopped, "Workspace reaper completed a pass");
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "Workspace reaper failed"),
            }
        }
    });
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

/// Remove resources this instance owns that no live workspace needs.
async fn runtime_reconcile(
    docker: &workspace_runtime::docker::Docker,
    manager: &workspaces::Manager,
    instance: &str,
) -> Result<(), workspace_runtime::RuntimeError> {
    let containers = docker.list_managed(instance).await?;

    for container in containers {
        match container.kind.as_deref() {
            Some("job") => {
                tracing::warn!(name = %container.name, "Removing orphan job container");
                docker.remove(&container.name).await?;
            }
            Some("workspace") => {
                let workspace = container
                    .workspace
                    .as_deref()
                    .and_then(|id| manager.store().get(id).ok().flatten());

                let keep = workspace.as_ref().is_some_and(|workspace| {
                    workspace.execution_mode == workspace_core::workspace::ExecutionMode::Persistent
                        && workspace.state.holds_compute()
                });

                if !keep {
                    tracing::warn!(name = %container.name, "Removing untracked workspace container");
                    docker.remove(&container.name).await?;

                    if let Some(workspace) = workspace {
                        let _ = manager.store().set_container(&workspace.id, None);

                        if workspace.state.holds_compute() {
                            let _ = manager.store().set_state(
                                &workspace.id,
                                workspace_core::workspace::WorkspaceState::Stopped,
                                Some("container was not running after restart"),
                            );
                        }
                    }
                }
            }
            _ => {
                tracing::warn!(name = %container.name, "Removing unlabelled instance container");
                docker.remove(&container.name).await?;
            }
        }
    }

    Ok(())
}
pub async fn serve(config: Config) -> io::Result<()> {
    if !valid_local_mcp(&config) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MCP requires SSO and the canonical local issuer/resource/scope",
        ));
    }

    if !config.server.listen.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Non-loopback listeners are disabled until authentication is implemented",
        ));
    }

    let auth = config
        .sso
        .as_ref()
        .map(|sso| {
            let secrets = secrets::AuthSecrets::from_env().map_err(io::Error::other)?;

            auth_http::AuthState::new(sso, &config.storage.data_dir, secrets).map(Arc::new)
        })
        .transpose()?;

    if auth.is_some() {
        tracing::info!("ALXPRGS SSO authentication enabled");
    } else {
        tracing::warn!("SSO is not configured; local development mode");
    }

    let listener = TcpListener::bind(config.server.listen).await?;
    let address = listener.local_addr()?;

    let data_dir = config.storage.data_dir.clone();

    let storage = tokio::task::spawn_blocking(move || storage::initialize(&data_dir))
        .await
        .map_err(io::Error::other)?
        .map_err(io::Error::other)?;

    tracing::info!(
        instance_id = %storage.instance_id,
        schema_version = storage.schema_version,
        sqlite_version = %storage.sqlite_version,
        journal_mode = %storage.journal_mode,
        "SQLite storage initialized"
    );

    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let shutdown = async move {
        #[cfg(unix)]
        {
            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    if let Err(error) = result {
                        tracing::error!(%error, "Signal handler failed");
                    }
                }
                _ = terminate.recv() => {}
            }
        }

        #[cfg(not(unix))]
        {
            if let Err(error) = tokio::signal::ctrl_c().await {
                tracing::error!(%error, "Signal handler failed");
            }
        }

        tracing::info!("Shutdown requested");
    };

    tracing::info!(
        %address,
        version = VERSION,
        "Workspace HTTP server started"
    );

    let runtime = Arc::new(RuntimeMonitor::new(
        config.runtime.as_ref(),
        &storage.instance_id,
    ));

    let state = AppState::new(&config, storage, Arc::clone(&runtime));

    // Nothing from a previous process may keep running: unfinished jobs are
    // marked interrupted and this instance's orphan containers are removed.
    match state.jobs.recover() {
        Ok(0) => {}
        Ok(count) => tracing::warn!(count, "Unfinished jobs marked as interrupted"),
        Err(error) => tracing::error!(%error, "Job recovery failed"),
    }

    if let Some(docker) = runtime.docker().cloned() {
        let manager = Arc::clone(&state.workspaces);
        let instance = state.storage.instance_id.clone();

        tokio::spawn(async move {
            if let Err(error) = runtime_reconcile(&docker, &manager, &instance).await {
                tracing::warn!(%error, "Runtime reconciliation failed");
            }
        });
    }

    spawn_runtime_supervisor(Arc::clone(&runtime), Arc::clone(&state.workspaces));

    tracing::info!(
        backend = runtime
            .settings()
            .map(|settings| settings.backend.name())
            .unwrap_or("none"),
        "Execution runtime supervisor started"
    );

    axum::serve(
        listener,
        router_with_runtime(&config, state, auth)
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;

    tracing::info!("Workspace HTTP server stopped");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request},
        response::Response,
    };
    use serde_json::Value;
    use tower::ServiceExt;

    fn config() -> Config {
        Config::parse(include_str!("../../../config.example.toml")).unwrap()
    }

    async fn request(method: Method, path: &str) -> Response {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage::initialize(dir.path()).unwrap();

        router(&config(), storage, None)
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn json(response: Response) -> Value {
        let body = to_bytes(response.into_body(), 65536).await.unwrap();

        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let response = request(Method::GET, "/health").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json(response).await["status"], "ok");
    }

    #[tokio::test]
    async fn readiness_fails_closed() {
        let response = request(Method::GET, "/ready").await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = json(response).await;

        assert_eq!(body["runtime_ready"], false);
        assert_eq!(body["status"], "not_ready");

        // The reason must name the real blocker instead of a generic string.
        let reason = body["reason"].as_str().unwrap();

        assert!(reason.len() > 20, "{reason}");
        assert_ne!(reason, "Runtime initialization is not implemented");
    }

    #[tokio::test]
    async fn server_info_returns_metadata() {
        let response = request(Method::GET, "/api/v1/server/info").await;

        assert_eq!(response.status(), StatusCode::OK);

        let body = json(response).await;

        assert_eq!(body["product"], PRODUCT_NAME);
        assert_eq!(body["version"], VERSION);
        assert_eq!(body["api_version"], "v1");
        assert_eq!(body["runtime_ready"], false);
        assert_eq!(body["resources"]["cpu_millis"], 2000);
        assert_eq!(body["storage"]["schema_version"], storage::SCHEMA_VERSION);
        assert_eq!(body["storage"]["journal_mode"], "wal");

        assert!(uuid::Uuid::parse_str(body["storage"]["instance_id"].as_str().unwrap()).is_ok());
    }

    #[tokio::test]
    async fn unknown_route_returns_not_found() {
        let response = request(Method::GET, "/unknown").await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unsupported_method_is_rejected() {
        let response = request(Method::POST, "/health").await;

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn non_loopback_listener_is_rejected() {
        let mut config = config();

        config.server.listen = "0.0.0.0:8765".parse().unwrap();

        let error = serve(config).await.unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}

#[cfg(test)]
mod auth_http_tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Method, Request, header},
        response::Response,
    };
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tower::ServiceExt;
    use workspace_core::config::SsoConfig;

    const KEY: [u8; 32] = [42; 32];

    const SUBJECT: &str = "550e8400-e29b-41d4-a716-446655440000";

    const ORIGIN: &str = "https://workspace.example.com";

    fn setup() -> (tempfile::TempDir, Router, SsoConfig) {
        let dir = tempfile::tempdir().unwrap();

        let mut config = Config::parse(include_str!("../../../config.example.toml")).unwrap();

        let sso = SsoConfig {
            issuer: "https://auth.alxprgs.tech".to_owned(),
            client_id: "test-client".to_owned(),
            redirect_uri: format!("{ORIGIN}/auth/callback"),
            trusted_proxies: Vec::new(),
        };

        config.sso = Some(sso.clone());
        config.storage.data_dir = dir.path().to_path_buf();

        let storage = storage::initialize(dir.path()).unwrap();

        let secrets =
            secrets::AuthSecrets::from_values("test-client-secret-123456", &STANDARD.encode(KEY))
                .unwrap();

        let auth = auth_http::AuthState::new_with_sso_revocation(&sso, dir.path(), secrets, false)
            .unwrap();

        let router = router(&config, storage, Some(Arc::new(auth)));

        (dir, router, sso)
    }

    async fn request(router: &Router, request: Request<Body>) -> Response {
        router.clone().oneshot(request).await.unwrap()
    }

    async fn get(router: &Router, uri: &str) -> Response {
        request(
            router,
            Request::builder().uri(uri).body(Body::empty()).unwrap(),
        )
        .await
    }

    /// The SSO callback redirects to `/`; that target must exist even when the
    /// management interface is not part of the build.
    #[tokio::test]
    async fn root_is_served_by_a_headless_build() {
        let (_dir, router, _sso) = setup();

        let response = get(&router, "/").await;

        assert_eq!(response.status(), StatusCode::OK);

        let content_type = response.headers()[axum::http::header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .to_owned();

        assert!(content_type.starts_with("text/html"), "{content_type}");
    }
    fn create_session(dir: &std::path::Path, config: &SsoConfig) -> auth_store::SessionCredentials {
        users::grant_access(dir, config, SUBJECT, "admin").unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let login = sso::LoginResult {
            identity: sso::VerifiedIdentity {
                issuer: config.issuer.clone(),
                subject: SUBJECT.to_owned(),
                expires_at_unix: now + 300,
            },
            username: Some("test-user".to_owned()),
            email: None,
            email_verified: false,
            tokens: sso::SessionTokens {
                access_token: "test-access".to_owned(),
                refresh_token: "test-refresh".to_owned(),
                id_token: "test-id".to_owned(),
                access_expires_at_unix: now + 300,
            },
        };

        auth_store::AuthStore::new(dir, &KEY)
            .unwrap()
            .create_session(config, &login)
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn local_oauth_and_mcp_end_to_end() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use openidconnect::reqwest::Url;
        use serde_json::{Value, json};

        const REDIRECT: &str = "https://chatgpt.com/connector_platform_oauth_redirect";

        const VERIFIER: &str = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";

        fn encode(pairs: &[(&str, &str)]) -> String {
            let mut url = Url::parse("https://example.com/").unwrap();

            url.query_pairs_mut().extend_pairs(pairs.iter().copied());

            url.query().unwrap().to_owned()
        }

        async fn read_json(response: Response) -> Value {
            let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();

            serde_json::from_slice(&body).unwrap()
        }

        let dir = tempfile::tempdir().unwrap();

        let mut config = Config::parse(include_str!("../../../config.example.toml")).unwrap();

        let sso = SsoConfig {
            issuer: "https://auth.alxprgs.tech".to_owned(),
            client_id: "test-client".to_owned(),
            redirect_uri: format!("{ORIGIN}/auth/callback"),
            trusted_proxies: Vec::new(),
        };

        let resource = format!("{ORIGIN}/chatgpt/mcp");

        config.sso = Some(sso.clone());
        config.storage.data_dir = dir.path().to_path_buf();

        config.mcp = Some(workspace_core::config::McpConfig {
            issuer: ORIGIN.to_owned(),
            audience: resource.clone(),
            resource: resource.clone(),
            required_scopes: vec!["workspace.read".to_owned()],
        });

        assert!(valid_local_mcp(&config));

        let storage_info = storage::initialize(dir.path()).unwrap();

        let secrets =
            secrets::AuthSecrets::from_values("test-client-secret-123456", &STANDARD.encode(KEY))
                .unwrap();

        let auth = auth_http::AuthState::new_with_sso_revocation(&sso, dir.path(), secrets, false)
            .unwrap();

        let credentials = create_session(dir.path(), &sso);

        let store = oauth_store::OAuthStore::new(dir.path(), &sso).unwrap();

        let client = store.register_client("ChatGPT Test", REDIRECT).unwrap();

        let router = router(&config, storage_info, Some(Arc::new(auth)));

        let response = get(&router, "/.well-known/oauth-authorization-server").await;

        assert_eq!(response.status(), StatusCode::OK);

        let metadata = read_json(response).await;
        assert_eq!(metadata["issuer"], ORIGIN);

        let challenge = URL_SAFE_NO_PAD.encode(crypto::hash_handle(VERIFIER));

        let query = encode(&[
            ("response_type", "code"),
            ("client_id", &client.client_id),
            ("redirect_uri", REDIRECT),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", "integration-test-state"),
            ("scope", "workspace.read"),
            ("resource", &resource),
        ]);

        let response = get(&router, &format!("/oauth/authorize?{query}")).await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let pending_cookie = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .find_map(|value| {
                value
                    .to_str()
                    .ok()
                    .filter(|value| value.starts_with("__Host-workspace-mcp-request="))
            })
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();

        let pending_handle = pending_cookie.split_once('=').unwrap().1.to_owned();

        let cookies = format!(
            "__Host-workspace-session={}; \
             __Host-workspace-csrf={}; {}",
            credentials.handle, credentials.csrf_token, pending_cookie,
        );

        let response = request(
            &router,
            Request::builder()
                .uri("/oauth/consent")
                .header(header::COOKIE, &cookies)
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();

        let html = String::from_utf8(body.to_vec()).unwrap();

        let tag = html
            .split_once("name=\"session_tag\" value=\"")
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap()
            .to_owned();

        let post_cookies = format!(
            "__Host-workspace-session={}; __Host-workspace-csrf={}",
            credentials.handle, credentials.csrf_token,
        );

        let consent = encode(&[
            ("csrf", &credentials.csrf_token),
            ("request_handle", &pending_handle),
            ("session_tag", &tag),
            ("decision", "approve"),
        ]);

        let consent_request = |origin: &str| {
            Request::builder()
                .method(Method::POST)
                .uri("/oauth/consent")
                .header(header::ORIGIN, origin)
                .header(header::COOKIE, &post_cookies)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(consent.clone()))
                .unwrap()
        };

        let response = request(&router, consent_request("https://untrusted.example")).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let mut forged = consent_request(ORIGIN);

        *forged.body_mut() = Body::from(consent.replace(&tag, &"A".repeat(43)));

        let response = request(&router, forged).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let response = request(&router, consent_request(ORIGIN)).await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let location = Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();

        let fields: std::collections::HashMap<_, _> = location.query_pairs().into_owned().collect();

        assert_eq!(fields["state"], "integration-test-state");
        assert_eq!(fields["iss"], ORIGIN);

        let code = fields["code"].clone();

        // Consent must be single-use.
        let response = request(&router, consent_request(ORIGIN)).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let basic = format!(
            "Basic {}",
            STANDARD.encode(format!("{}:{}", client.client_id, client.client_secret)),
        );

        let exchange = encode(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
            ("resource", &resource),
        ]);

        let post = |path: &str, body: String| {
            Request::builder()
                .method(Method::POST)
                .uri(path)
                .header(header::AUTHORIZATION, &basic)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap()
        };

        let response = request(&router, post("/oauth/token", exchange)).await;

        assert_eq!(response.status(), StatusCode::OK);

        let tokens = read_json(response).await;

        let access = tokens["access_token"].as_str().unwrap();
        let refresh = tokens["refresh_token"].as_str().unwrap();

        let mcp_request = |token: &str, payload: Value| {
            Request::builder()
                .method(Method::POST)
                .uri("/chatgpt/mcp")
                .header(header::HOST, "workspace.example.com")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .header("mcp-protocol-version", "2025-06-18")
                .body(Body::from(payload.to_string()))
                .unwrap()
        };

        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {
                    "name": "test",
                    "version": "1.0.0"
                }
            }
        });

        let response = request(&router, mcp_request("invalid", initialize.clone())).await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = request(&router, mcp_request(access, initialize)).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key("mcp-session-id"));

        let list = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        });

        let response = request(&router, mcp_request(access, list.clone())).await;

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();

        assert!(String::from_utf8_lossy(&body).contains("server_info"));
        assert!(String::from_utf8_lossy(&body).contains("capabilities_list"));

        let call = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "server_info",
                "arguments": {}
            }
        });

        let response = request(&router, mcp_request(access, call)).await;

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();

        assert!(String::from_utf8_lossy(&body).contains("ALXPRGS Workspace"));

        let rotation = encode(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("resource", &resource),
        ]);

        let response = request(&router, post("/oauth/token", rotation)).await;

        assert_eq!(response.status(), StatusCode::OK);

        let rotated = read_json(response).await;

        let new_access = rotated["access_token"].as_str().unwrap();
        let new_refresh = rotated["refresh_token"].as_str().unwrap();

        assert_ne!(refresh, new_refresh);

        let revocation = encode(&[("token", new_refresh), ("token_type_hint", "refresh_token")]);

        let response = request(&router, post("/oauth/revoke", revocation)).await;

        assert_eq!(response.status(), StatusCode::OK);

        let response = request(&router, mcp_request(new_access, list)).await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    fn session_cookie(credentials: &auth_store::SessionCredentials) -> String {
        format!(
            "__Host-workspace-session={}; \
             __Host-workspace-csrf={}",
            credentials.handle, credentials.csrf_token,
        )
    }

    #[tokio::test]
    async fn health_is_public_but_private_api_requires_session() {
        let (_dir, router, _sso) = setup();

        assert_eq!(get(&router, "/health").await.status(), StatusCode::OK,);

        for uri in ["/ready", "/api/v1/server/info", "/auth/me"] {
            let response = get(&router, uri).await;

            assert_eq!(response.status(), StatusCode::UNAUTHORIZED,);

            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store",);
        }
    }

    #[tokio::test]
    async fn login_redirect_sets_secure_flow_cookie() {
        let (_dir, router, _sso) = setup();

        let response = get(&router, "/auth/login").await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let location = response.headers()[header::LOCATION].to_str().unwrap();

        assert!(location.starts_with("https://auth.alxprgs.tech/oauth/authorize?"));

        assert!(location.contains("code_challenge_method=S256"));

        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();

        assert!(cookie.starts_with("__Host-workspace-flow="));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("Max-Age=600"));
        assert!(!cookie.contains("Domain="));
    }

    #[tokio::test]
    async fn malformed_callbacks_are_rejected() {
        let (_dir, router, _sso) = setup();

        for uri in [
            "/auth/callback",
            "/auth/callback?code=a&code=b&state=test",
            "/auth/callback?error=denied&code=a&state=test",
            "/auth/callback?code=a&state=x&state=y",
        ] {
            assert_eq!(get(&router, uri).await.status(), StatusCode::BAD_REQUEST,);
        }
    }

    #[tokio::test]
    async fn callback_without_flow_redirects_to_clean_url() {
        let (_dir, router, _sso) = setup();

        let response = get(&router, "/auth/callback?code=test&state=test").await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let location = response.headers()[header::LOCATION].to_str().unwrap();

        assert_eq!(location, "/?auth_error=session_expired");
        assert!(!location.contains("code="));
        assert!(!location.contains("state="));
    }

    fn peer(address: &str) -> std::net::SocketAddr {
        address.parse().unwrap()
    }

    async fn get_from(router: &Router, uri: &str, address: std::net::SocketAddr) -> Response {
        let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();

        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(address));

        self::request(router, request).await
    }

    #[tokio::test]
    async fn authorization_attempts_are_rate_limited() {
        let (_dir, router, _sso) = setup();

        let client = peer("203.0.113.7:50000");

        for _ in 0..20 {
            assert_eq!(
                get_from(&router, "/auth/login", client).await.status(),
                StatusCode::SEE_OTHER
            );
        }

        assert_eq!(
            get_from(&router, "/auth/login", client).await.status(),
            StatusCode::TOO_MANY_REQUESTS,
        );
    }

    /// One noisy client must not exhaust the budget of every other client,
    /// which is what a single shared window used to do.
    #[tokio::test]
    async fn exhausted_client_does_not_block_other_clients() {
        let (_dir, router, _sso) = setup();

        let noisy = peer("203.0.113.7:50000");
        let quiet = peer("198.51.100.20:50000");

        for _ in 0..21 {
            let _ = get_from(&router, "/auth/login", noisy).await;
        }

        assert_eq!(
            get_from(&router, "/auth/login", noisy).await.status(),
            StatusCode::TOO_MANY_REQUESTS,
        );

        assert_eq!(
            get_from(&router, "/auth/login", quiet).await.status(),
            StatusCode::SEE_OTHER,
        );
    }

    /// A spoofed X-Forwarded-For must not move a client into a fresh bucket.
    #[tokio::test]
    async fn forwarded_header_cannot_evade_the_limit() {
        let (_dir, router, _sso) = setup();

        let client = peer("203.0.113.7:50000");

        for attempt in 0..20 {
            let mut request = Request::builder()
                .uri("/auth/login")
                .body(Body::empty())
                .unwrap();

            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(client));

            request.headers_mut().insert(
                "x-forwarded-for",
                format!("10.0.0.{attempt}").parse().unwrap(),
            );

            assert_eq!(
                self::request(&router, request).await.status(),
                StatusCode::SEE_OTHER
            );
        }

        let mut request = Request::builder()
            .uri("/auth/login")
            .body(Body::empty())
            .unwrap();

        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(client));

        request
            .headers_mut()
            .insert("x-forwarded-for", "10.0.0.200".parse().unwrap());

        assert_eq!(
            self::request(&router, request).await.status(),
            StatusCode::TOO_MANY_REQUESTS,
        );
    }

    /// Without `[mcp]` the gateway exposes nothing, not even metadata.
    #[tokio::test]
    async fn mcp_surface_is_absent_without_configuration() {
        let (_dir, router, _sso) = setup();

        for uri in [
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/chatgpt/mcp",
            "/chatgpt/mcp",
        ] {
            assert_eq!(
                get(&router, uri).await.status(),
                StatusCode::NOT_FOUND,
                "{uri}"
            );
        }
    }

    /// With an audience declared, discovery is served and the protocol
    /// endpoint is mounted but requires an MCP access token.
    #[tokio::test]
    async fn configured_mcp_requires_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::parse(include_str!("../../../config.example.toml")).unwrap();

        config.storage.data_dir = dir.path().to_path_buf();
        config.mcp = Some(workspace_core::config::McpConfig {
            issuer: "https://auth.alxprgs.tech".to_owned(),
            audience: "https://workspace.example.com/chatgpt/mcp".to_owned(),
            resource: "https://workspace.example.com/chatgpt/mcp".to_owned(),
            required_scopes: vec!["workspace.read".to_owned()],
        });

        let storage = storage::initialize(dir.path()).unwrap();
        let router = router(&config, storage, None);

        assert_eq!(
            get(&router, "/.well-known/oauth-protected-resource")
                .await
                .status(),
            StatusCode::OK
        );

        assert_eq!(
            request(
                &router,
                Request::builder()
                    .method(Method::POST)
                    .uri("/chatgpt/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#
                    ))
                    .unwrap(),
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn head_does_not_start_or_consume_login() {
        let (_dir, router, _sso) = setup();

        for uri in ["/auth/login", "/auth/callback"] {
            let response = request(
                &router,
                Request::builder()
                    .method(Method::HEAD)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;

            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED,);
        }
    }

    #[tokio::test]
    async fn valid_session_can_access_private_api() {
        let (dir, router, sso) = setup();

        let credentials = create_session(dir.path(), &sso);

        for uri in ["/auth/me", "/api/v1/server/info"] {
            let response = request(
                &router,
                Request::builder()
                    .uri(uri)
                    .header(header::COOKIE, session_cookie(&credentials))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;

            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn duplicate_session_cookies_are_rejected() {
        let (dir, router, sso) = setup();

        let credentials = create_session(dir.path(), &sso);

        let cookie = format!(
            "{}; __Host-workspace-session={}",
            session_cookie(&credentials),
            credentials.handle,
        );

        let response = request(
            &router,
            Request::builder()
                .uri("/auth/me")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED,);
    }

    #[tokio::test]
    async fn logout_requires_origin_and_csrf_and_revokes_session() {
        let (dir, router, sso) = setup();

        let credentials = create_session(dir.path(), &sso);

        for (origin, csrf) in [
            ("https://other.example.com", credentials.csrf_token.as_str()),
            (ORIGIN, "invalid-csrf"),
        ] {
            let response = request(
                &router,
                Request::builder()
                    .method(Method::POST)
                    .uri("/auth/logout")
                    .header(header::ORIGIN, origin)
                    .header("x-workspace-csrf", csrf)
                    .header(header::COOKIE, session_cookie(&credentials))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;

            assert_eq!(response.status(), StatusCode::FORBIDDEN,);
        }

        let response = request(
            &router,
            Request::builder()
                .method(Method::POST)
                .uri("/auth/logout")
                .header(header::ORIGIN, ORIGIN)
                .header("x-workspace-csrf", &credentials.csrf_token)
                .header(header::COOKIE, session_cookie(&credentials))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT,);

        let response = request(
            &router,
            Request::builder()
                .uri("/auth/me")
                .header(header::COOKIE, session_cookie(&credentials))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED,);
    }

    #[tokio::test]
    async fn pending_refresh_does_not_log_user_out() {
        let (dir, router, sso) = setup();

        let credentials = create_session(dir.path(), &sso);

        let connection = storage::open(dir.path()).unwrap();

        connection
            .execute(
                "UPDATE web_sessions
             SET refresh_started_at_unix = unixepoch()",
                [],
            )
            .unwrap();

        drop(connection);

        let path = dir.path().to_path_buf();

        let completion = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(350)).await;

            tokio::task::spawn_blocking(move || {
                let connection = storage::open(&path).unwrap();

                connection
                    .execute(
                        "UPDATE web_sessions
                     SET refresh_started_at_unix = NULL",
                        [],
                    )
                    .unwrap();
            })
            .await
            .unwrap();
        });

        let response = request(
            &router,
            Request::builder()
                .uri("/auth/me")
                .header(header::COOKIE, session_cookie(&credentials))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);

        completion.await.unwrap();
    }
}
