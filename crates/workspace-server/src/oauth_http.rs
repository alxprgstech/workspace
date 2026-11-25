use super::*;
use crate::oauth_store::{OAuthError, OAuthStore, SCOPE, TokenPair, browser::AuthorizationRequest};
use axum::{
    Form,
    extract::{DefaultBodyLimit, Query},
    response::Html,
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use serde::Deserialize;
use serde_json::json;

struct OAuthHttp {
    auth: Arc<AuthState>,
    store: Arc<OAuthStore>,
}

async fn db<T, F>(operation: F) -> Result<T, OAuthError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, OAuthError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| OAuthError::InvalidRequest)?
}

fn error_response(error: OAuthError) -> Response {
    let (status, name) = match error {
        OAuthError::InvalidClient => (StatusCode::UNAUTHORIZED, "invalid_client"),
        OAuthError::InvalidGrant => (StatusCode::BAD_REQUEST, "invalid_grant"),
        OAuthError::InvalidRequest => (StatusCode::BAD_REQUEST, "invalid_request"),
        OAuthError::AccessDenied => (StatusCode::FORBIDDEN, "access_denied"),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    };

    let mut response = (status, Json(json!({"error": name}))).into_response();

    if status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"Workspace OAuth\""),
        );
    }

    response
}

async fn metadata(State(state): State<Arc<OAuthHttp>>) -> Json<serde_json::Value> {
    let issuer = &state.auth.origin;

    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/oauth/authorize"),
        "token_endpoint": format!("{issuer}/oauth/token"),
        "revocation_endpoint": format!("{issuer}/oauth/revoke"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": [
            "client_secret_basic", "client_secret_post"
        ],
        "revocation_endpoint_auth_methods_supported": [
            "client_secret_basic", "client_secret_post"
        ],
        "authorization_response_iss_parameter_supported": true,
        "scopes_supported": [SCOPE]
    }))
}

async fn authorize(State(state): State<Arc<OAuthHttp>>, request: Request) -> Response {
    if request.method() != Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    if request.uri().query().is_none_or(|q| q.len() > 8192) {
        return error_response(OAuthError::InvalidRequest);
    }

    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);

    if !state
        .auth
        .allow_login(state.auth.client_address(request.headers(), peer))
    {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }

    let Ok(Query(parameters)) = Query::<AuthorizationRequest>::try_from_uri(request.uri()) else {
        return error_response(OAuthError::InvalidRequest);
    };

    let handle = match db(move || {
        state
            .store
            .begin_authorization(&parameters, state.auth.secrets.encryption_key())
    })
    .await
    {
        Ok(handle) => handle,
        Err(error) => return error_response(error),
    };

    let mut response = Redirect::to("/oauth/consent").into_response();

    set_cookie(&mut response, MCP_REQUEST_COOKIE, &handle, 600, true);

    response
}

fn escape(value: &str) -> String {
    value
        .replace(char::from(38), "\x26amp;")
        .replace(char::from(60), "\x26lt;")
        .replace(char::from(62), "\x26gt;")
        .replace(char::from(34), "\x26quot;")
        .replace(char::from(39), "\x26#39;")
}

fn consent_message(session: &str, request: &str, csrf: &str) -> String {
    format!("workspace:mcp-consent:v1:{session}:{request}:{csrf}")
}

fn consent_tag(key: &[u8; 32], session: &str, request: &str, csrf: &str) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let message = consent_message(session, request, csrf);

    URL_SAFE_NO_PAD.encode(ring::hmac::sign(&key, message.as_bytes()).as_ref())
}

fn verify_consent_tag(key: &[u8; 32], session: &str, request: &str, csrf: &str, tag: &str) -> bool {
    if !valid_handle(session) || !valid_handle(request) || !valid_handle(csrf) || !valid_handle(tag)
    {
        return false;
    }

    let Ok(signature) = URL_SAFE_NO_PAD.decode(tag) else {
        return false;
    };

    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let message = consent_message(session, request, csrf);

    ring::hmac::verify(&key, message.as_bytes(), &signature).is_ok()
}

async fn consent_page(
    State(state): State<Arc<OAuthHttp>>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    if method != Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    let Some(handle) = cookie(&headers, MCP_REQUEST_COOKIE) else {
        return error_response(OAuthError::InvalidGrant);
    };

    let copy = Arc::clone(&state);
    let request_handle = handle.clone();

    let pending = match db(move || {
        copy.store
            .pending_authorization(&request_handle, copy.auth.secrets.encryption_key())
    })
    .await
    {
        Ok(pending) => pending,
        Err(error) => return error_response(error),
    };

    let Some(csrf) = cookie(&headers, CSRF_COOKIE) else {
        return Redirect::to("/auth/login").into_response();
    };

    let Some(session_handle) = cookie(&headers, SESSION_COOKIE) else {
        return Redirect::to("/auth/login").into_response();
    };

    let info = match session(Arc::clone(&state.auth), &headers, Some(csrf.clone())).await {
        Ok(Some(info)) => info,
        Ok(None) => return Redirect::to("/auth/login").into_response(),
        Err(status) => return status.into_response(),
    };

    let username = info.user.username.as_deref().unwrap_or(&info.user.subject);

    let page = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Authorize Workspace access</title>
<style>
html {{color-scheme:light dark}}
body {{font:16px system-ui,sans-serif;max-width:480px;margin:12vh auto;padding:24px}}
main {{border:1px solid #8885;border-radius:16px;padding:28px}}
h1 {{font-size:24px;margin-top:0}}
p {{line-height:1.5}}
button {{padding:12px 22px;border-radius:8px;cursor:pointer;margin:6px}}
.approve {{background:#2563eb;color:white;border:0}}
small {{opacity:.7;overflow-wrap:anywhere}}
</style>
</head>
<body>
<main>
<small>ALXPRGS WORKSPACE</small>
<h1>Authorize application</h1>
<p><strong>{client}</strong> requests access to Workspace.</p>
<p>Signed in as <strong>{username}</strong>.</p>
<p>Permission: read server information and available capabilities.</p>
<p><small>Resource: {resource}</small></p>
<form method="post" action="/oauth/consent">
<input type="hidden" name="csrf" value="{csrf}">
<input type="hidden" name="request_handle" value="{handle}">
<input type="hidden" name="session_tag" value="{tag}">
<button class="approve" name="decision" value="approve">Allow access</button>
<button name="decision" value="deny">Deny</button>
</form>
</main>
</body>
</html>"#,
        client = escape(&pending.client_name),
        username = escape(username),
        resource = escape(state.store.resource()),
        csrf = escape(&csrf),
        handle = escape(&handle),
        tag = consent_tag(
            state.auth.secrets.encryption_key(),
            &session_handle,
            &handle,
            &csrf,
        ),
    );

    let mut response = Html(page).into_response();

    let csp = [
        "default-src 'none'",
        "style-src 'unsafe-inline'",
        "form-action 'self'",
        "frame-ancestors 'none'",
        "base-uri 'none'",
    ]
    .join("; ");

    let Ok(value) = HeaderValue::from_str(&csp) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    response
        .headers_mut()
        .insert(header::CONTENT_SECURITY_POLICY, value);

    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsentForm {
    csrf: String,
    request_handle: String,
    session_tag: String,
    decision: String,
}

async fn consent_submit(
    State(state): State<Arc<OAuthHttp>>,
    headers: HeaderMap,
    Form(form): Form<ConsentForm>,
) -> Response {
    if !valid_origin(&headers, &state.auth)
        || cookie(&headers, CSRF_COOKIE).as_deref() != Some(form.csrf.as_str())
        || cookie(&headers, MCP_REQUEST_COOKIE).is_some_and(|value| value != form.request_handle)
        || !matches!(form.decision.as_str(), "approve" | "deny")
    {
        return StatusCode::FORBIDDEN.into_response();
    }

    let Some(handle) = cookie(&headers, SESSION_COOKIE) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    if !verify_consent_tag(
        state.auth.secrets.encryption_key(),
        &handle,
        &form.request_handle,
        &form.csrf,
        &form.session_tag,
    ) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let info = match session(Arc::clone(&state.auth), &headers, Some(form.csrf.clone())).await {
        Ok(Some(info)) => info,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(status) => return status.into_response(),
    };

    let issuer = state.auth.origin.clone();

    let completed = db(move || {
        state.store.complete_authorization(
            &form.request_handle,
            state.auth.secrets.encryption_key(),
            &info.user.id,
            &handle,
            &form.csrf,
            &state.auth.config.client_id,
            form.decision == "approve",
        )
    })
    .await;

    let (request, code) = match completed {
        Ok(value) => value,
        Err(error) => return error_response(error),
    };

    let Ok(mut redirect) = Url::parse(&request.redirect_uri) else {
        return error_response(OAuthError::InvalidRequest);
    };

    {
        let mut query = redirect.query_pairs_mut();

        query.append_pair("state", &request.state);
        query.append_pair("iss", &issuer);

        match code {
            Some(code) => {
                query.append_pair("code", &code);
            }
            None => {
                query.append_pair("error", "access_denied");
            }
        }
    }

    let mut response = Redirect::to(redirect.as_str()).into_response();

    set_cookie(&mut response, MCP_REQUEST_COOKIE, "", 0, true);

    response
}

fn client_credentials(
    headers: &HeaderMap,
    id: Option<String>,
    secret: Option<String>,
) -> Result<(String, String), OAuthError> {
    if headers.contains_key(header::AUTHORIZATION) {
        if id.is_some() || secret.is_some() {
            return Err(OAuthError::InvalidClient);
        }

        let value = single_header(headers, "authorization").ok_or(OAuthError::InvalidClient)?;

        let (scheme, encoded) = value.split_once(' ').ok_or(OAuthError::InvalidClient)?;

        if !scheme.eq_ignore_ascii_case("basic") || encoded.len() > 512 {
            return Err(OAuthError::InvalidClient);
        }

        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| OAuthError::InvalidClient)?;

        let decoded = String::from_utf8(bytes).map_err(|_| OAuthError::InvalidClient)?;

        let (id, secret) = decoded.split_once(':').ok_or(OAuthError::InvalidClient)?;

        return Ok((id.to_owned(), secret.to_owned()));
    }

    match (id, secret) {
        (Some(id), Some(secret)) => Ok((id, secret)),
        _ => Err(OAuthError::InvalidClient),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenForm {
    grant_type: String,
    client_id: Option<String>,
    client_secret: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: Option<String>,
    scope: Option<String>,
}

fn token_response(pair: TokenPair) -> Response {
    Json(json!({
        "access_token": pair.access_token,
        "refresh_token": pair.refresh_token,
        "token_type": "Bearer",
        "expires_in": pair.expires_in,
        "scope": SCOPE,
    }))
    .into_response()
}

async fn token(
    State(state): State<Arc<OAuthHttp>>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> Response {
    let (id, secret) = match client_credentials(&headers, form.client_id, form.client_secret) {
        Ok(credentials) => credentials,
        Err(error) => return error_response(error),
    };

    if form
        .resource
        .as_deref()
        .is_some_and(|value| value != state.store.resource())
        || form.scope.as_deref().is_some_and(|value| value != SCOPE)
    {
        return error_response(OAuthError::InvalidRequest);
    }

    let result = match form.grant_type.as_str() {
        "authorization_code" => {
            if form.resource.as_deref() != Some(state.store.resource())
                || form.refresh_token.is_some()
            {
                return error_response(OAuthError::InvalidRequest);
            }

            let (Some(code), Some(redirect), Some(verifier)) =
                (form.code, form.redirect_uri, form.code_verifier)
            else {
                return error_response(OAuthError::InvalidRequest);
            };

            db(move || {
                state
                    .store
                    .exchange_code(&id, &secret, &code, &redirect, &verifier)
            })
            .await
        }

        "refresh_token" => {
            if form.code.is_some() || form.redirect_uri.is_some() || form.code_verifier.is_some() {
                return error_response(OAuthError::InvalidRequest);
            }

            let Some(refresh) = form.refresh_token else {
                return error_response(OAuthError::InvalidRequest);
            };

            db(move || state.store.rotate_refresh(&id, &secret, &refresh)).await
        }

        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "unsupported_grant_type"})),
            )
                .into_response();
        }
    };

    match result {
        Ok(pair) => token_response(pair),
        Err(error) => error_response(error),
    }
}

#[derive(Deserialize)]
struct RevokeForm {
    client_id: Option<String>,
    client_secret: Option<String>,
    token: String,
}

async fn revoke(
    State(state): State<Arc<OAuthHttp>>,
    headers: HeaderMap,
    Form(form): Form<RevokeForm>,
) -> Response {
    let (id, secret) = match client_credentials(&headers, form.client_id, form.client_secret) {
        Ok(credentials) => credentials,
        Err(error) => return error_response(error),
    };

    match db(move || state.store.revoke(&id, &secret, &form.token)).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(error) => error_response(error),
    }
}

pub(crate) fn router(auth: Arc<AuthState>, store: Arc<OAuthStore>) -> Router {
    let state = Arc::new(OAuthHttp { auth, store });

    Router::new()
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/consent", get(consent_page).post(consent_submit))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(state)
}
