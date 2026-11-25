//! Streamable HTTP transport for the MCP gateway (stage 007).

use super::tools::{capabilities_catalog, server_information};
use crate::mcp_auth::{
    Jwk, McpCaller, McpExpectations, TokenError, parse_jwks, verify_access_token,
};
use axum::{
    Router,
    extract::Request,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::any,
};
use openidconnect::reqwest;
use rmcp::{
    ErrorData as McpError, RoleServer,
    handler::server::ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use workspace_core::config::McpConfig;

const JWKS_TTL: Duration = Duration::from_secs(300);
const JWKS_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_JWKS_RESPONSE: usize = 512 * 1024;
const MAX_TOKEN_BYTES: usize = 8 * 1024;

/// Cached signing keys for MCP access tokens.
///
/// Keys are fetched from `{issuer}/.well-known/jwks.json`. An unknown `kid`
/// forces one refresh so an SSO key rotation does not require a restart.
pub struct KeyCache {
    url: String,
    http: reqwest::Client,
    cached: tokio::sync::Mutex<Option<(Instant, Arc<Vec<Jwk>>)>>,
    refresh_lock: tokio::sync::Mutex<()>,
}

impl KeyCache {
    pub fn new(issuer: &str) -> Result<Self, TokenError> {
        let http = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| TokenError::Malformed)?;

        Ok(Self {
            url: format!("{}/.well-known/jwks.json", issuer.trim_end_matches('/')),
            http,
            cached: tokio::sync::Mutex::new(None),
            refresh_lock: tokio::sync::Mutex::new(()),
        })
    }

    async fn fetch(&self) -> Result<Arc<Vec<Jwk>>, TokenError> {
        let mut response = tokio::time::timeout(JWKS_TIMEOUT, self.http.get(&self.url).send())
            .await
            .map_err(|_| TokenError::UnknownKey)?
            .map_err(|_| TokenError::UnknownKey)?;

        if response.status() != reqwest::StatusCode::OK {
            return Err(TokenError::UnknownKey);
        }

        let mut body = Vec::new();

        while let Some(chunk) = tokio::time::timeout(JWKS_TIMEOUT, response.chunk())
            .await
            .map_err(|_| TokenError::UnknownKey)?
            .map_err(|_| TokenError::UnknownKey)?
        {
            if body.len() + chunk.len() > MAX_JWKS_RESPONSE {
                return Err(TokenError::UnknownKey);
            }

            body.extend_from_slice(&chunk);
        }

        let document = std::str::from_utf8(&body).map_err(|_| TokenError::Malformed)?;

        let keys = parse_jwks(document)?;

        if keys.is_empty() {
            return Err(TokenError::UnknownKey);
        }

        let keys = Arc::new(keys);

        *self.cached.lock().await = Some((Instant::now(), Arc::clone(&keys)));

        Ok(keys)
    }

    async fn keys(&self) -> Result<Arc<Vec<Jwk>>, TokenError> {
        if let Some((fetched_at, keys)) = self.cached.lock().await.as_ref()
            && fetched_at.elapsed() < JWKS_TTL
        {
            return Ok(Arc::clone(keys));
        }

        self.fetch().await
    }

    async fn invalidate(&self) {
        *self.cached.lock().await = None;
    }

    /// Verify a token, refreshing keys once when the `kid` is unknown.
    pub async fn verify(
        &self,
        token: &str,
        expectations: &McpExpectations,
        now: i64,
    ) -> Result<McpCaller, TokenError> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(TokenError::Malformed);
        }

        let keys = self.keys().await?;

        match verify_access_token(token, &keys, expectations, now) {
            Err(TokenError::UnknownKey) => {
                // One refresh at a time; a stampede of unknown-key requests
                // must not hammer the identity provider.
                let _guard = self.refresh_lock.lock().await;

                self.invalidate().await;

                let keys = self.fetch().await?;

                verify_access_token(token, &keys, expectations, now)
            }
            result => result,
        }
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;

    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;

    (!token.is_empty() && !token.contains(' ')).then_some(token)
}

async fn require_token(auth: Arc<GatewayAuth>, request: Request, next: Next) -> Response {
    let Some(token) = bearer(request.headers()).map(str::to_owned) else {
        return unauthorized("invalid_token", "An access token is required.");
    };

    let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(now) => now.as_secs() as i64,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    match auth.keys.verify(&token, &auth.expectations, now).await {
        Ok(caller) => {
            let mut request = request;

            request.extensions_mut().insert(Arc::new(caller));

            next.run(request).await
        }
        Err(_) => unauthorized(
            "invalid_token",
            "The access token is not valid for this resource.",
        ),
    }
}

fn unauthorized(kind: &str, message: &str) -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({ "error": kind, "message": message })),
    )
        .into_response();

    if let Ok(value) = HeaderValue::from_str(&format!(
        "Bearer error=\"{kind}\", resource_metadata=\"/.well-known/oauth-protected-resource\""
    )) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
    }

    response
}

pub struct GatewayAuth {
    keys: KeyCache,
    expectations: McpExpectations,
}

#[derive(Clone)]
pub struct GatewayServer {
    info: ServerConfig,
    tools: Vec<Tool>,
    schema_version: u32,
    runtime: Arc<crate::mcp::SharedRuntime>,
}

impl ServerHandler for GatewayServer {
    fn get_info(&self) -> ServerConfig {
        self.info.clone()
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tools.clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let runtime = self.runtime.status().await;

        let payload = match request.name.as_ref() {
            "server_info" => server_information(self.schema_version, &runtime),
            "capabilities_list" => capabilities_catalog(self.schema_version, &runtime),
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown tool: {other}"),
                    None,
                ));
            }
        };

        let text = serde_json::to_string(&payload)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;

        Ok(CallToolResponse::Complete(CallToolResult::success(vec![
            ContentBlock::text(text),
        ])))
    }
}

/// Both tools take no arguments, so their schema is a closed empty object.
fn no_arguments() -> Arc<serde_json::Map<String, serde_json::Value>> {
    let mut schema = serde_json::Map::new();

    schema.insert("type".to_owned(), json!("object"));
    schema.insert("properties".to_owned(), json!({}));
    schema.insert("additionalProperties".to_owned(), json!(false));

    Arc::new(schema)
}

fn tool_definitions() -> Vec<Tool> {
    let mut tools = vec![
        Tool::new(
            "server_info",
            "Read-only build and runtime information for this Workspace instance.",
            no_arguments(),
        ),
        Tool::new(
            "capabilities_list",
            "Read-only catalog of capabilities with their real implementation status.",
            no_arguments(),
        ),
    ];

    for tool in &mut tools {
        tool.annotations = Some(
            rmcp::model::ToolAnnotations::new()
                .read_only(true)
                .destructive(false)
                .open_world(false),
        );

        tool.meta = Some(
            serde_json::from_value(json!({
                "securitySchemes": [{
                    "type": "oauth2",
                    "scopes": ["workspace.read"]
                }]
            }))
            .expect("valid static tool metadata"),
        );
    }

    tools
}

fn host_of(resource: &str) -> Option<String> {
    let url = openidconnect::reqwest::Url::parse(resource).ok()?;

    url.host_str().map(|host| match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

pub type GatewayService = StreamableHttpService<GatewayServer, LocalSessionManager>;

const MCP_PATH: &str = "/chatgpt/mcp";

pub fn service(
    config: &McpConfig,
    schema_version: u32,
    runtime: Arc<crate::mcp::SharedRuntime>,
) -> Result<GatewayService, TokenError> {
    service_with_mode(config, schema_version, runtime, true)
}

fn service_with_mode(
    config: &McpConfig,
    schema_version: u32,
    runtime: Arc<crate::mcp::SharedRuntime>,
    stateful: bool,
) -> Result<GatewayService, TokenError> {
    let mut allowed_hosts = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];

    if let Some(host) = host_of(&config.resource)
        && !allowed_hosts.contains(&host)
    {
        allowed_hosts.push(host);
    }

    let server = GatewayServer {
        info: ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                workspace_core::PRODUCT_NAME,
                workspace_core::VERSION,
            )),
        tools: tool_definitions(),
        schema_version,
        runtime,
    };

    let mut http = StreamableHttpServerConfig::default();
    http.allowed_hosts = allowed_hosts;
    http.legacy_session_mode = stateful;
    http.max_request_body_bytes = 64 * 1024;

    Ok(StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        http,
    ))
}

/// Router fragment for the MCP endpoint, mounted behind a bearer-token gate.
pub fn router(
    config: &McpConfig,
    schema_version: u32,
    runtime: Arc<crate::mcp::SharedRuntime>,
) -> Result<Router, TokenError> {
    build_router(config, schema_version, runtime, None)
}

/// Same router with pre-seeded signing keys, for hermetic end-to-end tests.
#[cfg(test)]
pub fn router_with_keys(
    config: &McpConfig,
    schema_version: u32,
    runtime: Arc<crate::mcp::SharedRuntime>,
    keys: Vec<Jwk>,
) -> Result<Router, TokenError> {
    build_router(config, schema_version, runtime, Some(keys))
}

fn build_router(
    config: &McpConfig,
    schema_version: u32,
    runtime: Arc<crate::mcp::SharedRuntime>,
    keys: Option<Vec<Jwk>>,
) -> Result<Router, TokenError> {
    let expectations = McpExpectations {
        issuer: config.issuer.clone(),
        audience: config.audience.clone(),
        resource: config.resource.clone(),
        required_scopes: config.required_scopes.clone(),
    };

    let keys = match keys {
        Some(keys) => {
            let mut cache = KeyCache::new(&config.issuer)?;

            cache.cached = tokio::sync::Mutex::new(Some((Instant::now(), Arc::new(keys))));

            cache
        }
        None => KeyCache::new(&config.issuer)?,
    };

    let auth = Arc::new(GatewayAuth { keys, expectations });

    let service = service(config, schema_version, Arc::clone(&runtime))?;

    let auth = Arc::clone(&auth);

    let gate = move |request: Request, next: Next| {
        let auth = Arc::clone(&auth);

        async move { require_token(auth, request, next).await }
    };

    Ok(Router::new()
        // Streamable HTTP uses POST for messages, GET for the server stream
        // and DELETE for session termination; the transport decides.
        .route(
            MCP_PATH,
            any(move |request: Request| handle(service.clone(), request)),
        )
        .layer(middleware::from_fn(gate)))
}

async fn handle(service: GatewayService, request: Request) -> Response {
    use tower::ServiceExt;

    let response = service.oneshot(request).await;

    match response {
        Ok(response) => response.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub fn local_router(
    config: &McpConfig,
    schema_version: u32,
    runtime: Arc<crate::mcp::SharedRuntime>,
    store: Arc<crate::oauth_store::OAuthStore>,
) -> Result<Router, TokenError> {
    let service = service_with_mode(config, schema_version, runtime, false)?;

    let metadata_url = format!(
        "{}/.well-known/oauth-protected-resource/chatgpt/mcp",
        config.issuer,
    );

    let challenge = HeaderValue::from_str(&format!(
        "Bearer resource_metadata=\"{metadata_url}\", scope=\"workspace.read\""
    ))
    .map_err(|_| TokenError::Malformed)?;

    let origin = config.issuer.clone();

    let gate = move |request: Request, next: Next| {
        let store = Arc::clone(&store);
        let challenge = challenge.clone();
        let origin = origin.clone();

        async move {
            let reject = || {
                let mut response = StatusCode::UNAUTHORIZED.into_response();

                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, challenge.clone());

                response
            };

            if request.headers().contains_key(header::ORIGIN)
                && !request
                    .headers()
                    .get(header::ORIGIN)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value == origin || value == "https://chatgpt.com")
            {
                return StatusCode::FORBIDDEN.into_response();
            }

            if request
                .headers()
                .get_all(header::AUTHORIZATION)
                .iter()
                .count()
                != 1
            {
                return reject();
            }

            let Some(token) = bearer(request.headers()).map(str::to_owned) else {
                return reject();
            };

            if token.len() > 128 {
                return reject();
            }

            let identity = tokio::task::spawn_blocking(move || store.verify_access(&token)).await;

            match identity {
                Ok(Ok(Some(identity))) => {
                    let caller = McpCaller {
                        subject: identity.subject.clone(),
                        scopes: vec![crate::oauth_store::SCOPE.to_owned()],
                    };

                    let mut request = request;

                    request.extensions_mut().insert(Arc::new(caller));
                    request.extensions_mut().insert(Arc::new(identity));

                    next.run(request).await
                }

                Ok(Ok(None)) => reject(),

                _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            }
        }
    };

    Ok(Router::new()
        .route(
            MCP_PATH,
            any(move |request: Request| handle(service.clone(), request)),
        )
        .layer(middleware::from_fn(gate)))
}
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request as HttpRequest};
    use tower::ServiceExt;

    #[test]
    fn only_bearer_tokens_are_accepted() {
        let mut headers = HeaderMap::new();

        assert!(bearer(&headers).is_none());

        headers.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert!(bearer(&headers).is_none());

        headers.insert(header::AUTHORIZATION, "Bearer ".parse().unwrap());
        assert!(bearer(&headers).is_none());

        headers.insert(header::AUTHORIZATION, "Bearer a.b.c".parse().unwrap());
        assert_eq!(bearer(&headers), Some("a.b.c"));

        headers.insert(header::AUTHORIZATION, "Bearer two tokens".parse().unwrap());
        assert!(bearer(&headers).is_none());
    }

    #[test]
    fn host_is_derived_from_the_resource() {
        assert_eq!(
            host_of("https://workspace.example.com/chatgpt/mcp").as_deref(),
            Some("workspace.example.com")
        );

        assert_eq!(
            host_of("https://workspace.example.com:8443/chatgpt/mcp").as_deref(),
            Some("workspace.example.com:8443")
        );

        assert!(host_of("not a url").is_none());
    }

    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use rsa::{
        RsaPrivateKey,
        pkcs1v15::SigningKey,
        sha2::Sha256,
        signature::{SignatureEncoding, Signer as RsaSigner},
        traits::PublicKeyParts,
    };

    struct Signer {
        key: SigningKey<Sha256>,
    }

    impl Signer {
        fn token(&self, config: &McpConfig) -> String {
            let header = URL_SAFE_NO_PAD.encode(
                serde_json::json!({"alg": "RS256", "typ": "JWT", "kid": "e2e-key"}).to_string(),
            );

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();

            let claims = serde_json::json!({
                "iss": config.issuer,
                "aud": config.audience,
                "resource": config.resource,
                "sub": "550e8400-e29b-41d4-a716-446655440000",
                "exp": now + 300,
                "scope": "workspace.read",
            });

            let payload = URL_SAFE_NO_PAD.encode(claims.to_string());

            let signed = format!("{header}.{payload}");

            let signature = self.key.sign(signed.as_bytes());

            format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
        }
    }

    fn signing_fixture() -> (Signer, Vec<Jwk>) {
        let private = RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();

        let document = serde_json::json!({
            "keys": [{
                "kty": "RSA",
                "kid": "e2e-key",
                "use": "sig",
                "alg": "RS256",
                "n": URL_SAFE_NO_PAD.encode(private.n().to_bytes_be()),
                "e": URL_SAFE_NO_PAD.encode(private.e().to_bytes_be()),
            }]
        });

        (
            Signer {
                key: SigningKey::<Sha256>::new(private),
            },
            parse_jwks(&document.to_string()).unwrap(),
        )
    }

    fn test_runtime() -> Arc<crate::mcp::SharedRuntime> {
        Arc::new(crate::mcp::SharedRuntime::new(
            None,
            "550e8400-e29b-41d4-a716-446655440000",
        ))
    }

    fn mcp_config() -> McpConfig {
        McpConfig {
            issuer: "https://auth.alxprgs.tech".to_owned(),
            audience: "https://workspace.example.com/chatgpt/mcp".to_owned(),
            resource: "https://workspace.example.com/chatgpt/mcp".to_owned(),
            required_scopes: vec!["workspace.read".to_owned()],
        }
    }

    #[tokio::test]
    async fn authenticated_client_can_list_and_call_tools() {
        let (signer, keys) = signing_fixture();
        let config = mcp_config();
        let token = signer.token(&config);

        let router = super::router_with_keys(&config, 3, test_runtime(), keys).unwrap();

        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "e2e", "version": "1.0.0" }
            }
        });

        let response = router
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/chatgpt/mcp")
                    .header(header::HOST, "workspace.example.com")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .body(Body::from(initialize.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let session = response
            .headers()
            .get("mcp-session-id")
            .expect("initialize must establish a session")
            .to_str()
            .unwrap()
            .to_owned();

        let body = axum::body::to_bytes(Body::new(response.into_body()), 1024 * 1024)
            .await
            .unwrap();

        assert!(String::from_utf8_lossy(&body).contains("serverInfo"));

        let list = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});

        let response = router
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/chatgpt/mcp")
                    .header(header::HOST, "workspace.example.com")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .header("mcp-session-id", &session)
                    .header("mcp-protocol-version", "2025-06-18")
                    .body(Body::from(list.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(Body::new(response.into_body()), 1024 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);

        assert!(body.contains("server_info"), "{body}");
        assert!(body.contains("capabilities_list"), "{body}");

        let call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "server_info", "arguments": {} }
        });

        let response = router
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/chatgpt/mcp")
                    .header(header::HOST, "workspace.example.com")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .header("mcp-session-id", &session)
                    .header("mcp-protocol-version", "2025-06-18")
                    .body(Body::from(call.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(Body::new(response.into_body()), 1024 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);

        assert!(body.contains("runtime_ready"), "{body}");
        assert!(body.contains("schema_version"), "{body}");
    }

    #[tokio::test]
    async fn service_answers_initialize_and_lists_tools() {
        let service = service(
            &McpConfig {
                issuer: "https://auth.alxprgs.tech".to_owned(),
                audience: "https://workspace.example.com/chatgpt/mcp".to_owned(),
                resource: "https://workspace.example.com/chatgpt/mcp".to_owned(),
                required_scopes: vec!["workspace.read".to_owned()],
            },
            3,
            test_runtime(),
        )
        .unwrap();

        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "1.0.0" }
            }
        });

        let request = HttpRequest::builder()
            .method("POST")
            .uri("/chatgpt/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::HOST, "workspace.example.com")
            .body(Body::from(initialize.to_string()))
            .unwrap();

        let response = service.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let session = response.headers().get("mcp-session-id").cloned();

        assert!(session.is_some(), "initialize must establish a session");

        let body = axum::body::to_bytes(Body::new(response.into_body()), 1024 * 1024)
            .await
            .unwrap();

        let body = String::from_utf8_lossy(&body);

        assert!(body.contains("serverInfo"), "{body}");
    }
}
