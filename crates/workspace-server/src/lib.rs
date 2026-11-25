#![forbid(unsafe_code)]

//! HTTP API, MCP gateway, authentication and administration.

#[cfg(not(feature = "web-ui"))]
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
#[cfg(feature = "web-ui")]
mod web;

mod auth_http;
pub mod auth_store;
pub mod crypto;
mod mcp;
pub mod mcp_auth;
pub mod secrets;
pub mod sso;
pub mod storage;
pub mod users;

use serde::Serialize;
use std::{io, sync::Arc};
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

fn router(
    config: &Config,
    storage: storage::StorageInfo,
    auth: Option<Arc<auth_http::AuthState>>,
) -> Router {
    let schema_version = storage.schema_version;

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

    let protected: Router = Router::new()
        .route("/ready", get(ready))
        .route("/api/v1/server/info", get(server_info))
        .with_state(info);

    let protected = match auth.as_ref() {
        Some(state) => protected.route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(state),
            auth_http::require_session,
        )),
        None => protected,
    };

    let router = Router::new().route("/health", get(health)).merge(protected);

    let router = match auth {
        Some(state) => router.merge(auth_http::router(state)),
        None => router,
    };

    // No MCP surface at all unless the deployment declares its audience:
    // an unconfigured gateway must fail closed rather than answer with an
    // unverified resource.
    let router = match config.mcp.clone() {
        Some(mcp) => router.merge(mcp::router(mcp, schema_version)),
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

pub async fn serve(config: Config) -> io::Result<()> {
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

    tracing::warn!("Execution runtime is not ready");

    axum::serve(
        listener,
        router(&config, storage, auth)
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
