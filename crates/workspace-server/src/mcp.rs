//! MCP gateway (stage 007).
//!
//! The gateway publishes discovery metadata (RFC 9728) and, when the transport
//! can be built, a Streamable HTTP endpoint guarded by an MCP access token.
//! It is absent entirely unless the deployment declares its audience: an
//! unconfigured gateway must fail closed rather than serve an unverified
//! resource.

mod catalog;
mod dispatch;
mod tools;
mod transport;

use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use serde::Serialize;
use std::sync::Arc;
use workspace_core::config::McpConfig;

/// Services the MCP gateway exposes to tools.
pub struct GatewayServices {
    pub schema_version: u32,
    pub runtime: Arc<workspace_runtime::status::RuntimeMonitor>,
    pub workspaces: Option<Arc<crate::workspaces::Manager>>,
    pub files: Option<Arc<crate::files::Files>>,
    pub downloader: Option<Arc<crate::import::Downloader>>,
    pub artifacts: Option<Arc<crate::artifacts::Artifacts>>,
    pub jobs: Option<Arc<crate::jobs::Executor>>,
}

impl GatewayServices {
    /// Services for a build without workspace or job management.
    #[cfg(test)]
    pub fn runtime_only(
        schema_version: u32,
        runtime: Arc<workspace_runtime::status::RuntimeMonitor>,
    ) -> Self {
        Self {
            schema_version,
            runtime,
            workspaces: None,
            jobs: None,
            files: None,
            artifacts: None,
            downloader: None,
        }
    }
}

/// Path an MCP client connects to.
pub const MCP_ENDPOINT: &str = "/chatgpt/mcp";

#[derive(Serialize)]
struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: [String; 1],
    scopes_supported: Vec<String>,
    bearer_methods_supported: [&'static str; 1],
    resource_name: &'static str,
}

impl ProtectedResourceMetadata {
    fn from_config(config: &McpConfig) -> Self {
        Self {
            resource: config.resource.clone(),
            authorization_servers: [config.issuer.clone()],
            scopes_supported: config.required_scopes.clone(),
            bearer_methods_supported: ["header"],
            resource_name: "ALXPRGS Workspace",
        }
    }
}

async fn protected_resource(State(config): State<Arc<McpConfig>>) -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(ProtectedResourceMetadata::from_config(&config)),
    )
}

/// The protocol endpoint exists but no transport could be built.
async fn transport_pending() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "mcp_transport_pending",
            "message": "The MCP Streamable HTTP transport is not enabled in this build.",
        })),
    )
}

/// Discovery routes shared by both the working and the pending transport.
fn discovery_router(config: McpConfig) -> Router {
    Router::new()
        // RFC 9728, including the path-specific form a client derives from
        // the MCP endpoint URL.
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/chatgpt/mcp",
            get(protected_resource),
        )
        .with_state(Arc::new(config))
}

pub fn router(config: McpConfig, services: Arc<GatewayServices>) -> Router {
    let discovery = discovery_router(config.clone());

    match transport::router(&config, services) {
        Ok(transport) => discovery.merge(transport),
        Err(error) => {
            tracing::warn!(%error, "MCP transport unavailable; endpoint stays closed");

            discovery.route(MCP_ENDPOINT, get(transport_pending).post(transport_pending))
        }
    }
}

pub fn local_router(
    config: McpConfig,
    services: Arc<GatewayServices>,
    store: Arc<crate::oauth_store::OAuthStore>,
) -> Router {
    match transport::local_router(&config, services, store) {
        Ok(transport) => discovery_router(config).merge(transport),
        Err(error) => {
            tracing::error!(%error, "Local MCP transport unavailable");
            Router::new()
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    fn config() -> McpConfig {
        McpConfig {
            issuer: "https://auth.alxprgs.tech".to_owned(),
            audience: "https://workspace.example.com/chatgpt/mcp".to_owned(),
            resource: "https://workspace.example.com/chatgpt/mcp".to_owned(),
            required_scopes: vec!["workspace.read".to_owned()],
        }
    }

    fn test_services() -> Arc<GatewayServices> {
        Arc::new(GatewayServices::runtime_only(
            3,
            Arc::new(workspace_runtime::status::RuntimeMonitor::new(
                None,
                "550e8400-e29b-41d4-a716-446655440000",
            )),
        ))
    }

    async fn get_json(uri: &str) -> (StatusCode, serde_json::Value) {
        let response = router(config(), test_services())
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();

        let status = response.status();

        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();

        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn advertises_the_resource_and_its_authorization_server() {
        for uri in [
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/chatgpt/mcp",
        ] {
            let (status, body) = get_json(uri).await;

            assert_eq!(status, StatusCode::OK, "{uri}");
            assert_eq!(body["resource"], config().resource);
            assert_eq!(
                body["authorization_servers"],
                serde_json::json!([config().issuer])
            );
            assert_eq!(
                body["scopes_supported"],
                serde_json::json!(config().required_scopes)
            );
            assert_eq!(
                body["bearer_methods_supported"],
                serde_json::json!(["header"])
            );
        }
    }

    #[tokio::test]
    async fn protocol_endpoint_requires_a_token() {
        let response = router(config(), test_services())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(MCP_ENDPOINT)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let challenge = response.headers()["www-authenticate"].to_str().unwrap();

        assert!(challenge.contains("resource_metadata="), "{challenge}");
    }
}
