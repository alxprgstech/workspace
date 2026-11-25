//! Shared helpers for protocol-level acceptance tests.
//!
//! Not a test target on its own: Cargo only builds top-level `tests/*.rs`.

use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::Value;
use workspace_core::config::Config;

pub const ORIGIN: &str = "https://workspace.alxprgs.tech";

pub fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

pub fn write_config(directory: &std::path::Path, port: u16) -> Config {
    let runtime = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config.toml"),
    )
    .ok()
    .and_then(|text| {
        text.split("[runtime]")
            .nth(1)
            .map(|section| format!("[runtime]{section}"))
    })
    .unwrap_or_default();

    let data_dir = directory.to_string_lossy().replace('\\', "\\\\");

    let text = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"

[storage]
data_dir = "{data_dir}"
max_import_mib = 32

[resources]
cpu_millis = 2000
memory_mib = 4096
storage_mib = 40960
max_active_workspaces = 3
max_concurrent_jobs = 1
max_processes_per_workspace = 256
max_job_seconds = 3600

[workspace]
default_mode = "ephemeral"
idle_timeout_seconds = 300
ttl_seconds = 86400
artifact_ttl_seconds = 86400

[network]
default_mode = "none"
allowlist = []

[logging]
level = "warn"

[sso]
issuer = "https://auth.alxprgs.tech"
client_id = "client_e2e_test_1234567890"
redirect_uri = "{ORIGIN}/auth/callback"

[mcp]
issuer = "{ORIGIN}"
audience = "{ORIGIN}/chatgpt/mcp"
resource = "{ORIGIN}/chatgpt/mcp"
required_scopes = ["workspace.read"]

{runtime}
"#
    );

    let path = directory.join("config.toml");
    std::fs::write(&path, text).unwrap();

    Config::load(&path).expect("valid test configuration")
}

/// Install test secrets required by `serve`.
pub fn set_secrets() {
    // SAFETY: tests are single-threaded here and no other test reads these.
    unsafe {
        std::env::set_var("WORKSPACE_SSO_CLIENT_SECRET", "e2e-client-secret-123456");
        std::env::set_var(
            "WORKSPACE_AUTH_KEY",
            base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
        );
    }
}

/// Create a user, an OAuth client and a real access token through the local
/// authorization-code flow (exactly what the consent handler triggers).
pub fn seed_token(config: &Config, subject: &str) -> String {
    let data_dir = &config.storage.data_dir;
    let sso = config.sso.as_ref().unwrap();

    workspace_server::storage::initialize(data_dir).unwrap();

    workspace_server::users::grant_access(data_dir, sso, subject, "admin").unwrap();

    let user_id = workspace_server::users::list_users(data_dir, sso)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .id;

    let store = workspace_server::oauth_store::OAuthStore::new(data_dir, sso).unwrap();

    let client = store
        .register_client(
            "Protocol E2E",
            "https://chatgpt.com/connector_platform_oauth_redirect",
        )
        .unwrap();

    let verifier = "e2e-verifier-0123456789abcdefghijklmnopqrstuvwxyz";

    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(workspace_server::crypto::hash_handle(verifier));

    let code = store
        .issue_code(
            &client.client_id,
            "https://chatgpt.com/connector_platform_oauth_redirect",
            &challenge,
            &user_id,
        )
        .unwrap();

    store
        .exchange_code(
            &client.client_id,
            &client.client_secret,
            &code,
            "https://chatgpt.com/connector_platform_oauth_redirect",
            verifier,
        )
        .unwrap()
        .access_token
}

pub async fn wait_for_health(client: &reqwest::Client, base: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);

    while Instant::now() < deadline {
        if let Ok(response) = client.get(format!("{base}/health")).send().await
            && response.status().is_success()
        {
            return;
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    panic!("server did not become healthy");
}

/// Parse either a plain JSON response or the last SSE `data:` event.
pub fn body_of(text: &str) -> Value {
    let payload = text
        .lines()
        .rev()
        .find_map(|line| {
            let line = line.trim_start();

            if let Some(data) = line.strip_prefix("data:") {
                Some(data.trim())
            } else if line.starts_with('{') {
                Some(line)
            } else {
                None
            }
        })
        .unwrap_or(text);

    serde_json::from_str(payload).unwrap_or_else(|error| {
        panic!("response is not JSON ({error}): {text}");
    })
}

pub async fn post_mcp(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    session: &str,
    body: String,
) -> reqwest::Response {
    let mut request = client
        .post(format!("{base}/chatgpt/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {token}"));

    if !session.is_empty() {
        request = request.header("mcp-session-id", session);
    }

    request.body(body).send().await.unwrap()
}

/// Call one MCP tool and return `(is_error, parsed_text_payload)`.
pub async fn call_tool(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
    arguments: Value,
) -> (bool, Value) {
    let response = post_mcp(
        client,
        base,
        token,
        "",
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        })
        .to_string(),
    )
    .await;

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let body = body_of(&response.text().await.unwrap());
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("null");

    let payload = serde_json::from_str(text).unwrap_or(Value::Null);

    println!("{name} -> {payload}");

    (
        body["result"]["isError"].as_bool().unwrap_or(false),
        payload,
    )
}
