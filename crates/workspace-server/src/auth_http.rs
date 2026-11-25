use crate::{
    auth_store::{AuthStore, AuthStoreError, SessionInfo},
    secrets::AuthSecrets,
    sso::{self, SsoClient, SsoError},
};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use openidconnect::reqwest::Url;
use std::{
    io,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;
use workspace_core::config::SsoConfig;

#[path = "refresh_http.rs"]
mod refresh_http;

const FLOW_COOKIE: &str = "__Host-workspace-flow";
const SESSION_COOKIE: &str = "__Host-workspace-session";
const CSRF_COOKIE: &str = "__Host-workspace-csrf";
const CSRF_HEADER: &str = "x-workspace-csrf";

const DISCOVERY_TTL: Duration = Duration::from_secs(600);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);
const REVOCATION_TIMEOUT: Duration = Duration::from_secs(5);
const RATE_WINDOW: Duration = Duration::from_secs(60);
const LOGIN_ATTEMPTS_PER_WINDOW: u32 = 20;
const CALLBACK_ATTEMPTS_PER_WINDOW: u32 = 30;

struct CachedDiscovery {
    fetched_at: Instant,
    client: SsoClient,
}

struct RateWindow {
    started_at: Instant,
    attempts: u32,
}

impl RateWindow {
    fn new() -> Self {
        Self {
            started_at: Instant::now(),
            attempts: 0,
        }
    }

    fn allow(&mut self, limit: u32) -> bool {
        if self.started_at.elapsed() >= RATE_WINDOW {
            self.started_at = Instant::now();
            self.attempts = 0;
        }

        if self.attempts >= limit {
            return false;
        }

        self.attempts += 1;

        true
    }
}

pub struct AuthState {
    config: SsoConfig,
    secrets: AuthSecrets,
    store: AuthStore,
    origin: String,
    discovery: AsyncMutex<Option<CachedDiscovery>>,
    revoke_at_sso: bool,
    login_window: Mutex<RateWindow>,
    callback_window: Mutex<RateWindow>,
}

impl AuthState {
    pub fn new(config: &SsoConfig, data_dir: &Path, secrets: AuthSecrets) -> io::Result<Self> {
        Self::new_with_sso_revocation(config, data_dir, secrets, true)
    }

    pub fn new_with_sso_revocation(
        config: &SsoConfig,
        data_dir: &Path,
        secrets: AuthSecrets,
        revoke_at_sso: bool,
    ) -> io::Result<Self> {
        sso::validate_config(config).map_err(io::Error::other)?;

        let redirect = Url::parse(&config.redirect_uri).map_err(io::Error::other)?;

        let store = AuthStore::new(data_dir, secrets.encryption_key()).map_err(io::Error::other)?;

        Ok(Self {
            config: config.clone(),
            secrets,
            store,
            origin: redirect.origin().ascii_serialization(),
            discovery: AsyncMutex::new(None),
            revoke_at_sso,
            login_window: Mutex::new(RateWindow::new()),
            callback_window: Mutex::new(RateWindow::new()),
        })
    }

    /// Cached discovery metadata and signing keys.
    ///
    /// The cache is only used for outbound OAuth calls; every ID token is
    /// still verified against the currently cached JWKS.
    async fn sso_client(&self) -> Result<SsoClient, SsoError> {
        let mut cached = self.discovery.lock().await;

        if let Some(entry) = cached.as_ref()
            && entry.fetched_at.elapsed() < DISCOVERY_TTL
        {
            return Ok(entry.client.clone());
        }

        let client = tokio::time::timeout(DISCOVERY_TIMEOUT, SsoClient::discover(&self.config))
            .await
            .map_err(|_| SsoError::Transport)??;

        *cached = Some(CachedDiscovery {
            fetched_at: Instant::now(),
            client: client.clone(),
        });

        Ok(client)
    }

    /// The local session is already invalid at this point; revocation at the
    /// SSO is bounded and never blocks the logout result.
    async fn revoke_at_sso(&self, refresh_token: String) {
        if !self.revoke_at_sso {
            return;
        }

        let result = tokio::time::timeout(REVOCATION_TIMEOUT, async {
            let client = self.sso_client().await?;

            client
                .revoke_refresh_token(self.secrets.client_secret(), &refresh_token)
                .await
        })
        .await;

        match result {
            Ok(Ok(())) => tracing::info!("SSO refresh token revoked"),
            Ok(Err(error)) => tracing::warn!(%error, "SSO refresh token revocation failed"),
            Err(_) => tracing::warn!("SSO refresh token revocation timed out"),
        }
    }

    fn allow_login(&self) -> bool {
        self.login_window
            .lock()
            .map(|mut window| window.allow(LOGIN_ATTEMPTS_PER_WINDOW))
            .unwrap_or(false)
    }

    fn allow_callback(&self) -> bool {
        self.callback_window
            .lock()
            .map(|mut window| window.allow(CALLBACK_ATTEMPTS_PER_WINDOW))
            .unwrap_or(false)
    }
}

fn valid_handle(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut result = None;

    for value in headers.get_all(header::COOKIE) {
        let value = value.to_str().ok()?;

        for pair in value.split(';') {
            let Some((key, value)) = pair.trim().split_once('=') else {
                continue;
            };

            if key == name {
                if result.is_some() || !valid_handle(value) {
                    return None;
                }

                result = Some(value.to_owned());
            }
        }
    }

    result
}

fn set_cookie(response: &mut Response, name: &str, value: &str, max_age: u32, http_only: bool) {
    let http_only = if http_only { "; HttpOnly" } else { "" };

    let value = format!(
        "{name}={value}; Path=/; Secure; SameSite=Lax; \
         Max-Age={max_age}{http_only}"
    );

    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&value).expect("invalid generated cookie"),
    );
}

fn clear_flow(response: &mut Response) {
    set_cookie(response, FLOW_COOKIE, "", 0, true);
}

fn clear_session(response: &mut Response) {
    set_cookie(response, SESSION_COOKIE, "", 0, true);
    set_cookie(response, CSRF_COOKIE, "", 0, false);
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let values = headers.get_all(name);
    let mut values = values.iter();

    let value = values.next()?.to_str().ok()?;

    if values.next().is_some() {
        return None;
    }

    Some(value)
}

fn valid_origin(headers: &HeaderMap, state: &AuthState) -> bool {
    single_header(headers, "origin") == Some(state.origin.as_str())
}

fn csrf_token(headers: &HeaderMap) -> Option<String> {
    let cookie = cookie(headers, CSRF_COOKIE)?;
    let supplied = single_header(headers, CSRF_HEADER)?;

    if supplied != cookie {
        return None;
    }

    Some(cookie)
}

async fn blocking<T, F>(operation: F) -> Result<T, StatusCode>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AuthStoreError> + Send + 'static,
{
    match tokio::task::spawn_blocking(operation).await {
        Ok(Ok(result)) => Ok(result),
        _ => {
            tracing::error!("Authentication storage failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn session(
    state: Arc<AuthState>,
    headers: &HeaderMap,
    csrf: Option<String>,
) -> Result<Option<SessionInfo>, StatusCode> {
    let Some(handle) = cookie(headers, SESSION_COOKIE) else {
        return Ok(None);
    };

    refresh_http::resolve_session(state, handle, csrf).await
}

async fn login(State(state): State<Arc<AuthState>>, method: Method) -> Response {
    if method != Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    if !state.allow_login() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many authorization attempts. Try again later.",
        )
            .into_response();
    }

    let request = match sso::start_authorization(&state.config) {
        Ok(request) => request,
        Err(_) => {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let url = request.url.to_string();

    let handle = match blocking(move || state.store.create_flow(&state.config, &request)).await {
        Ok(handle) => handle,
        Err(status) => return status.into_response(),
    };

    let mut response = Redirect::to(&url).into_response();

    set_cookie(&mut response, FLOW_COOKIE, &handle, 600, true);

    response
}

struct Callback {
    state: String,
    code: Option<String>,
}

fn parse_callback(uri: &Uri, issuer: &str) -> Option<Callback> {
    let query = uri.query()?;

    if query.len() > 4096 {
        return None;
    }

    let mut url = Url::parse("https://workspace.invalid/").ok()?;

    url.set_query(Some(query));

    let mut parameters = std::collections::HashMap::new();

    for (key, value) in url.query_pairs() {
        if parameters
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return None;
        }
    }

    let state = parameters.remove("state")?;

    if state.is_empty() || state.len() > 128 {
        return None;
    }

    if parameters.get("iss").is_some_and(|value| value != issuer) {
        return None;
    }

    let code = parameters.remove("code");
    let error = parameters.remove("error");

    match (&code, &error) {
        (Some(code), None) if !code.is_empty() && code.len() <= 128 => {}
        (None, Some(error)) if !error.is_empty() => {}
        _ => return None,
    }

    Some(Callback { state, code })
}

fn callback_error(status: StatusCode, message: &'static str) -> Response {
    let mut response = (status, message).into_response();
    clear_flow(&mut response);
    response
}

fn auth_error(kind: &str) -> Response {
    let mut response = Redirect::to(&format!("/?auth_error={kind}")).into_response();

    clear_flow(&mut response);

    response
}

async fn callback(
    State(state): State<Arc<AuthState>>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if method != Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    if !state.allow_callback() {
        return (StatusCode::TOO_MANY_REQUESTS, "Too many callback attempts.").into_response();
    }

    let Some(callback) = parse_callback(&uri, &state.config.issuer) else {
        return callback_error(
            StatusCode::BAD_REQUEST,
            "Invalid authorization response. Start a new login.",
        );
    };

    let Some(handle) = cookie(&headers, FLOW_COOKIE) else {
        return auth_error("session_expired");
    };

    let flow_state = Arc::clone(&state);

    let secrets = match blocking(move || {
        flow_state
            .store
            .consume_flow(&flow_state.config, &handle, &callback.state)
    })
    .await
    {
        Ok(Some(secrets)) => secrets,
        Ok(None) => return auth_error("login_expired"),
        Err(_) => return auth_error("storage_error"),
    };

    let Some(code) = callback.code else {
        return auth_error("sso_denied");
    };

    let client = match state.sso_client().await {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(%error, "SSO discovery failed");

            return auth_error("sso_unavailable");
        }
    };

    let login = match client
        .exchange_code(
            state.secrets.client_secret(),
            &code,
            &secrets.verifier,
            &secrets.nonce,
        )
        .await
    {
        Ok(login) => login,
        Err(error) => {
            tracing::warn!(%error, "SSO login failed");

            return auth_error("login_failed");
        }
    };

    let old_handle = cookie(&headers, SESSION_COOKIE);
    let old_csrf = cookie(&headers, CSRF_COOKIE);

    let storage_state = Arc::clone(&state);

    let created = blocking(move || {
        let old_tokens = if let (Some(handle), Some(csrf)) = (old_handle, old_csrf) {
            storage_state
                .store
                .revoke_session(&storage_state.config, &handle, &csrf)?
        } else {
            None
        };

        let credentials = storage_state
            .store
            .create_session(&storage_state.config, &login)?;

        Ok((credentials, old_tokens))
    })
    .await;

    let (credentials, old_tokens) = match created {
        Ok(created) => created,
        Err(_) => return auth_error("storage_error"),
    };

    if let Some(tokens) = old_tokens {
        state.revoke_at_sso(tokens.refresh_token).await;
    }

    let Some(credentials) = credentials else {
        let mut response = auth_error("access_denied");

        clear_session(&mut response);

        return response;
    };

    let mut response = Redirect::to("/").into_response();

    clear_flow(&mut response);

    set_cookie(
        &mut response,
        SESSION_COOKIE,
        &credentials.handle,
        28_800,
        true,
    );

    set_cookie(
        &mut response,
        CSRF_COOKIE,
        &credentials.csrf_token,
        28_800,
        false,
    );

    response
}

async fn me(State(state): State<Arc<AuthState>>, headers: HeaderMap) -> Response {
    match session(state, &headers, None).await {
        Ok(Some(info)) => Json(info).into_response(),
        Ok(None) => StatusCode::UNAUTHORIZED.into_response(),
        Err(status) => status.into_response(),
    }
}

async fn logout(State(state): State<Arc<AuthState>>, headers: HeaderMap) -> Response {
    if !valid_origin(&headers, &state) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let Some(csrf) = csrf_token(&headers) else {
        return StatusCode::FORBIDDEN.into_response();
    };

    let Some(handle) = cookie(&headers, SESSION_COOKIE) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    let storage_state = Arc::clone(&state);

    match blocking(move || {
        storage_state
            .store
            .revoke_session(&storage_state.config, &handle, &csrf)
    })
    .await
    {
        Ok(Some(tokens)) => {
            // Local invalidation already happened; the SSO call is bounded.
            state.revoke_at_sso(tokens.refresh_token).await;

            let mut response = StatusCode::NO_CONTENT.into_response();

            clear_session(&mut response);
            clear_flow(&mut response);

            response
        }
        Ok(None) => StatusCode::FORBIDDEN.into_response(),
        Err(status) => status.into_response(),
    }
}

pub async fn require_session(
    State(state): State<Arc<AuthState>>,
    request: Request,
    next: Next,
) -> Response {
    let csrf = if matches!(*request.method(), Method::GET | Method::HEAD) {
        None
    } else {
        if !valid_origin(request.headers(), &state) {
            return StatusCode::FORBIDDEN.into_response();
        }

        let Some(token) = csrf_token(request.headers()) else {
            return StatusCode::FORBIDDEN.into_response();
        };

        Some(token)
    };

    match session(state, request.headers(), csrf).await {
        Ok(Some(info)) => {
            let mut request = request;
            request.extensions_mut().insert(Arc::new(info));

            next.run(request).await
        }
        Ok(None) => StatusCode::UNAUTHORIZED.into_response(),
        Err(status) => status.into_response(),
    }
}

pub async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;

    for (name, value) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
    ] {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }

    response
}

pub fn router(state: Arc<AuthState>) -> Router {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
        .with_state(state)
}
