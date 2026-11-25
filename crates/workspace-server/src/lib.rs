#![forbid(unsafe_code)]

//! HTTP API, MCP gateway, authentication and administration.

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
#[cfg(feature = "web-ui")]
mod web;

pub mod auth_store;
pub mod crypto;
pub mod sso;
pub mod storage;

use serde::Serialize;
use std::io;
use tokio::net::TcpListener;
use workspace_core::{PRODUCT_NAME, VERSION, config::Config};

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct ReadinessResponse {
    status: &'static str,
    runtime_ready: bool,
    reason: &'static str,
}

#[derive(Clone, Serialize)]
struct ResourceBudget {
    cpu_millis: u32,
    memory_mib: u64,
    storage_mib: u64,
    max_active_workspaces: u32,
    max_concurrent_jobs: u32,
}

#[derive(Clone, Serialize)]
struct ServerInfo {
    product: &'static str,
    version: &'static str,
    api_version: &'static str,
    platform: &'static str,
    architecture: &'static str,
    runtime_ready: bool,
    resources: ResourceBudget,
    storage: storage::StorageInfo,
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn ready() -> (StatusCode, Json<ReadinessResponse>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ReadinessResponse {
            status: "not_ready",
            runtime_ready: false,
            reason: "Runtime initialization is not implemented",
        }),
    )
}

async fn server_info(State(info): State<ServerInfo>) -> Json<ServerInfo> {
    Json(info)
}

fn router(config: &Config, storage: storage::StorageInfo) -> Router {
    let info = ServerInfo {
        product: PRODUCT_NAME,
        version: VERSION,
        api_version: "v1",
        platform: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        runtime_ready: false,
        storage,
        resources: ResourceBudget {
            cpu_millis: config.resources.cpu_millis,
            memory_mib: config.resources.memory_mib,
            storage_mib: config.resources.storage_mib,
            max_active_workspaces: config.resources.max_active_workspaces,
            max_concurrent_jobs: config.resources.max_concurrent_jobs,
        },
    };

    let router: Router = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/api/v1/server/info", get(server_info))
        .with_state(info);

    #[cfg(feature = "web-ui")]
    let router = router.merge(web::router());

    router
}

pub async fn serve(config: Config) -> io::Result<()> {
    if !config.server.listen.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Non-loopback listeners are disabled until authentication is implemented",
        ));
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

    tracing::warn!("Execution runtime is not ready");

    axum::serve(listener, router(&config, storage))
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

        router(&config(), storage)
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
