use super::*;
use crate::oauth_store::{
    OAuthError, OAuthStore, SCOPES, TokenPair, browser::AuthorizationRequest, parse_scopes,
};
use axum::{
    Form,
    extract::{DefaultBodyLimit, Query, RawQuery},
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
        "scopes_supported": SCOPES
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
    raw_query: RawQuery,
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
    let language = consent_language(raw_query.0.as_deref(), &headers);
    let tag = consent_tag(
        state.auth.secrets.encryption_key(),
        &session_handle,
        &handle,
        &csrf,
    );

    let page = render_consent_page(
        language,
        &pending.client_name,
        username,
        &crate::oauth_store::browser::requested_scopes(&pending.request),
        state.store.resource(),
        &csrf,
        &handle,
        &tag,
    );

    let mut response = Html(page).into_response();

    let Ok(callback) = Url::parse(&pending.request.redirect_uri) else {
        return error_response(OAuthError::InvalidRequest);
    };

    let form_action = format!(
        "form-action 'self' {}",
        callback.origin().ascii_serialization(),
    );

    let csp = [
        "default-src 'none'",
        "style-src 'unsafe-inline'",
        form_action.as_str(),
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

/// Official ALXPRGS mark, inlined because the consent page's CSP allows no
/// external images. The file is the same asset the SPA serves from `/brand`.
const BRAND_LOGO: &str = include_str!("../../../web/public/brand/logo-mark.svg");

/// Server-rendered consent page. Placeholders are replaced after HTML escaping;
/// `format!` is deliberately not used so the inline CSS needs no brace escaping.
const CONSENT_TEMPLATE: &str = r#"<!doctype html>
<html lang="%%LANG%%">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>ALXPRGS Workspace — %%TITLE%%</title>
<style>
:root{color-scheme:dark;--canvas:#090a0c;--surface:#0f1115;--surface-2:#14171d;--border:#2a303a;--text:#f5f7fa;--secondary:#b6bec9;--tertiary:#929caa;--brand:#3b82f6;--brand-hover:#5594f8;--on-brand:#090a0c;--focus:#60a5fa;--info-surface:#11213a}
@media (prefers-color-scheme: light){:root{color-scheme:light;--canvas:#f7f8fa;--surface:#fff;--surface-2:#f1f3f5;--border:#cbd2da;--text:#111318;--secondary:#4b5563;--tertiary:#606b78;--brand:#2563eb;--brand-hover:#1d4ed8;--on-brand:#fff;--focus:#2563eb;--info-surface:#eaf1ff}}
*,*::before,*::after{box-sizing:border-box}
body{margin:0;min-height:100vh;display:grid;place-items:center;padding:24px;background:radial-gradient(520px 320px at 15% 0%,color-mix(in srgb,var(--brand) 14%,transparent),transparent 70%),var(--canvas);color:var(--text);font:14px/1.5 system-ui,-apple-system,"Segoe UI",Roboto,Arial,sans-serif;-webkit-font-smoothing:antialiased}
.card{width:min(560px,100%);padding:28px;border:1px solid var(--border);border-radius:18px;background:var(--surface);box-shadow:0 20px 60px rgb(0 0 0 / .25)}
.brand{display:flex;align-items:center;gap:12px;margin-bottom:18px}
.brand svg{width:38px;height:38px}
.brand small{color:var(--tertiary);font-size:11px;font-weight:600;letter-spacing:.08em}
h1{margin:0 0 8px;font-size:24px;line-height:32px;letter-spacing:-.01em}
.lead{margin:0 0 6px;color:var(--secondary)}
.signed{margin:0 0 18px;color:var(--tertiary);font-size:13px}
h2{margin:18px 0 10px;font-size:15px;line-height:22px}
.hint{margin:0 0 10px;color:var(--tertiary);font-size:12.5px}
ul.scopes{list-style:none;margin:0;padding:0;border:1px solid var(--border);border-radius:10px;overflow:hidden;background:var(--surface-2)}
ul.scopes li{display:flex;gap:10px;align-items:baseline;padding:10px 12px;border-bottom:1px solid var(--border);flex-wrap:wrap}
ul.scopes li:last-child{border-bottom:0}
ul.scopes code{font-family:"Cascadia Code",Consolas,monospace;font-size:12px;color:var(--text);background:var(--surface);border:1px solid var(--border);border-radius:6px;padding:1px 6px}
ul.scopes span{color:var(--secondary);font-size:12.5px}
.resource{margin:14px 0 0;color:var(--tertiary);font-size:12px;overflow-wrap:anywhere}
.resource code{font-family:"Cascadia Code",Consolas,monospace}
form{margin:0}
.actions{display:flex;gap:10px;flex-wrap:wrap;margin-top:20px}
button{min-height:44px;padding:0 18px;border-radius:8px;font:inherit;font-weight:500;cursor:pointer;transition:background-color 160ms cubic-bezier(.2,.8,.2,1),border-color 160ms cubic-bezier(.2,.8,.2,1)}
button:focus-visible,a:focus-visible{outline:2px solid var(--focus);outline-offset:2px}
.allow{background:var(--brand);border:1px solid var(--brand);color:var(--on-brand);font-weight:600}
.allow:hover{background:var(--brand-hover);border-color:var(--brand-hover)}
.deny{background:transparent;border:1px solid var(--border);color:var(--text)}
.deny:hover{background:var(--surface-2)}
.languages{display:flex;gap:14px;margin-top:18px;font-size:12px}
.languages a{color:var(--secondary);text-decoration:none}
.languages a[aria-current="true"]{color:var(--text);font-weight:600}
.languages a:hover{color:var(--text)}
.note{margin:16px 0 0;color:var(--tertiary);font-size:12px}
@media (max-width:480px){body{padding:14px}.card{padding:20px}.actions button{width:100%}}
@media (prefers-reduced-motion: reduce){button{transition:none}}
</style>
</head>
<body>
<main class="card">
<div class="brand">%%LOGO%%<small>%%BRAND%%</small></div>
<h1>%%TITLE%%</h1>
<p class="lead">%%LEAD%%</p>
<p class="signed">%%SIGNED_IN%%</p>
<h2>%%PERMISSIONS%%</h2>
<p class="hint">%%SCOPES_HINT%%</p>
<ul class="scopes">%%SCOPES%%</ul>
<p class="resource">%%RESOURCE_LABEL%%: <code>%%RESOURCE%%</code></p>
<form method="post" action="/oauth/consent">
<input type="hidden" name="csrf" value="%%CSRF%%">
<input type="hidden" name="request_handle" value="%%HANDLE%%">
<input type="hidden" name="session_tag" value="%%TAG%%">
<div class="actions">
<button class="allow" name="decision" value="approve">%%ALLOW%%</button>
<button class="deny" name="decision" value="deny">%%DENY%%</button>
</div>
</form>
<p class="note">%%NOTE%%</p>
%%LANGUAGE_SWITCH%%
</main>
</body>
</html>"#;

struct ConsentCopy {
    title: &'static str,
    lead: &'static str,
    signed_in: &'static str,
    permissions: &'static str,
    scopes_hint: &'static str,
    resource: &'static str,
    allow: &'static str,
    deny: &'static str,
    note: &'static str,
}

const CONSENT_EN: ConsentCopy = ConsentCopy {
    title: "Authorize access",
    lead: "%CLIENT% requests access to ALXPRGS Workspace.",
    signed_in: "Signed in as %USERNAME%.",
    permissions: "Requested permissions",
    scopes_hint: "The connector receives only the permissions listed below.",
    resource: "Resource",
    allow: "Allow access",
    deny: "Deny",
    note: "Workspace keeps only an encrypted server-side session. Access can be revoked later.",
};

const CONSENT_RU: ConsentCopy = ConsentCopy {
    title: "Разрешить доступ",
    lead: "%CLIENT% запрашивает доступ к ALXPRGS Workspace.",
    signed_in: "Вы вошли как %USERNAME%.",
    permissions: "Запрашиваемые разрешения",
    scopes_hint: "Коннектор получит только перечисленные ниже разрешения.",
    resource: "Ресурс",
    allow: "Разрешить доступ",
    deny: "Отклонить",
    note: "Workspace хранит только зашифрованную серверную сессию. Доступ можно отозвать позже.",
};

fn consent_copy(language: &str) -> &'static ConsentCopy {
    if language == "ru" {
        &CONSENT_RU
    } else {
        &CONSENT_EN
    }
}

/// Render the consent page. Kept separate from the handler so the escaping,
/// localization and placeholder contract can be tested without a session.
#[allow(clippy::too_many_arguments)]
fn render_consent_page(
    language: &str,
    client: &str,
    username: &str,
    scopes: &[String],
    resource: &str,
    csrf: &str,
    handle: &str,
    tag: &str,
) -> String {
    let copy = consent_copy(language);

    let scope_items = scopes
        .iter()
        .map(|scope| {
            let description = scope_description(scope, language);

            if description.is_empty() {
                format!("<li><code>{}</code></li>", escape(scope))
            } else {
                format!(
                    "<li><code>{}</code><span>{}</span></li>",
                    escape(scope),
                    escape(description)
                )
            }
        })
        .collect::<String>();

    let language_switch = format!(
        r#"<nav class="languages" aria-label="{label}"><a href="?lang=ru"{ru}>Русский</a><a href="?lang=en"{en}>English</a></nav>"#,
        label = if language == "ru" {
            "Язык страницы"
        } else {
            "Page language"
        },
        ru = current_attribute(language == "ru"),
        en = current_attribute(language == "en"),
    );

    CONSENT_TEMPLATE
        .replace("%%LANG%%", language)
        .replace("%%TITLE%%", copy.title)
        .replace("%%LANGUAGE_SWITCH%%", &language_switch)
        .replace("%%LOGO%%", BRAND_LOGO)
        .replace("%%BRAND%%", "ALXPRGS WORKSPACE")
        .replace(
            "%%LEAD%%",
            &copy
                .lead
                .replace("%CLIENT%", &format!("<strong>{}</strong>", escape(client))),
        )
        .replace(
            "%%SIGNED_IN%%",
            &copy.signed_in.replace(
                "%USERNAME%",
                &format!("<strong>{}</strong>", escape(username)),
            ),
        )
        .replace("%%PERMISSIONS%%", copy.permissions)
        .replace("%%SCOPES_HINT%%", copy.scopes_hint)
        .replace("%%SCOPES%%", &scope_items)
        .replace("%%RESOURCE_LABEL%%", copy.resource)
        .replace("%%RESOURCE%%", &escape(resource))
        .replace("%%ALLOW%%", copy.allow)
        .replace("%%DENY%%", copy.deny)
        .replace("%%NOTE%%", copy.note)
        .replace("%%CSRF%%", &escape(csrf))
        .replace("%%HANDLE%%", &escape(handle))
        .replace("%%TAG%%", &escape(tag))
}

fn current_attribute(active: bool) -> &'static str {
    if active { " aria-current=\"true\"" } else { "" }
}

/// Explicit `?lang=` wins; otherwise the browser's Accept-Language decides,
/// with English as the safe fallback for unknown languages.
fn consent_language(raw_query: Option<&str>, headers: &HeaderMap) -> &'static str {
    if let Some(query) = raw_query {
        for pair in query.split('&') {
            let mut parts = pair.splitn(2, '=');

            if parts.next() != Some("lang") {
                continue;
            }

            match parts.next() {
                Some("ru") => return "ru",
                Some("en") => return "en",
                _ => {}
            }
        }
    }

    if let Some(value) = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok())
    {
        for part in value.split(',') {
            let tag = part
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();

            if tag == "ru" || tag.starts_with("ru-") {
                return "ru";
            }

            if tag == "en" || tag.starts_with("en-") {
                return "en";
            }
        }
    }

    "en"
}

/// Human-readable descriptions for the known scopes. The exact scope code is
/// always shown as well; an unknown scope simply has no description.
fn scope_description(scope: &str, language: &str) -> &'static str {
    let russian = language == "ru";

    match scope {
        "workspace:read" => {
            if russian {
                "Чтение метаданных рабочих пространств"
            } else {
                "Read workspace metadata"
            }
        }
        "workspace:write" => {
            if russian {
                "Создание и изменение рабочих пространств"
            } else {
                "Create and change workspaces"
            }
        }
        "workspace:delete" => {
            if russian {
                "Удаление рабочих пространств"
            } else {
                "Delete workspaces"
            }
        }
        "execution:run" => {
            if russian {
                "Запуск команд и заданий"
            } else {
                "Run commands and jobs"
            }
        }
        "files:read" => {
            if russian {
                "Чтение файлов"
            } else {
                "Read workspace files"
            }
        }
        "files:write" => {
            if russian {
                "Запись и редактирование файлов"
            } else {
                "Write and edit files"
            }
        }
        "files:import" => {
            if russian {
                "Импорт файлов и ссылок"
            } else {
                "Import files and URLs"
            }
        }
        "artifacts:read" => {
            if russian {
                "Чтение и скачивание артефактов"
            } else {
                "Read and download artifacts"
            }
        }
        "artifacts:publish" => {
            if russian {
                "Публикация артефактов"
            } else {
                "Publish artifacts"
            }
        }
        _ => "",
    }
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
        "scope": pair.scope,
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
        || form
            .scope
            .as_deref()
            .is_some_and(|value| parse_scopes(value).is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_language_wins_and_accept_language_is_a_fallback() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT_LANGUAGE,
            "ru-RU,ru;q=0.9,en;q=0.8".parse().unwrap(),
        );

        assert_eq!(consent_language(Some("lang=en"), &headers), "en");
        assert_eq!(consent_language(Some("lang=ru"), &headers), "ru");
        assert_eq!(consent_language(Some("lang=de"), &headers), "ru");
        assert_eq!(consent_language(Some("other=1"), &headers), "ru");

        headers.insert(header::ACCEPT_LANGUAGE, "de-DE,de;q=0.9".parse().unwrap());

        assert_eq!(consent_language(None, &headers), "en");
        assert_eq!(consent_language(None, &HeaderMap::new()), "en");
    }

    #[test]
    fn consent_copy_and_scope_descriptions_are_localized() {
        assert_eq!(consent_copy("ru").allow, "Разрешить доступ");
        assert_eq!(consent_copy("en").allow, "Allow access");
        assert!(scope_description("execution:run", "ru").contains("Запуск"));
        assert_eq!(
            scope_description("execution:run", "en"),
            "Run commands and jobs"
        );
        assert_eq!(scope_description("workspace:unknown", "en"), "");
    }

    #[test]
    fn consent_template_is_self_contained_and_csp_safe() {
        for marker in [
            "%%LANG%%",
            "%%TITLE%%",
            "%%LEAD%%",
            "%%SCOPES%%",
            "%%CSRF%%",
            "%%TAG%%",
        ] {
            assert!(CONSENT_TEMPLATE.contains(marker), "{marker}");
        }

        // The page's CSP allows inline styles only: no scripts, no remote
        // images, and the official mark is inlined instead of referenced.
        assert!(CONSENT_TEMPLATE.contains("<style>"));
        assert!(!CONSENT_TEMPLATE.contains("<script"));
        assert!(!CONSENT_TEMPLATE.contains("<img"));
        assert!(BRAND_LOGO.contains("<svg"));
    }

    #[test]
    fn rendered_consent_page_is_localized_escaped_and_complete() {
        let scopes = vec!["execution:run".to_owned(), "workspace:read".to_owned()];

        let russian = render_consent_page(
            "ru",
            "Demo <script>",
            "оператор",
            &scopes,
            "https://workspace.example.test/chatgpt/mcp",
            "csrf-token",
            "request-handle",
            "tag",
        );

        assert!(russian.contains(r#"<html lang="ru">"#));
        assert!(russian.contains("Разрешить доступ"));
        assert!(russian.contains("Запуск команд и заданий"));
        assert!(russian.contains("Demo &lt;script&gt;"));
        assert!(!russian.contains("<script>"));
        assert!(!russian.contains("%%"));
        assert!(russian.contains(r#"href="?lang=en""#));

        let english = render_consent_page(
            "en",
            "Demo",
            "operator",
            &scopes,
            "https://workspace.example.test/chatgpt/mcp",
            "csrf-token",
            "request-handle",
            "tag",
        );

        assert!(english.contains(r#"<html lang="en">"#));
        assert!(english.contains("Allow access"));
        assert!(english.contains("Run commands and jobs"));
        assert!(!english.contains("%%"));
    }
}
