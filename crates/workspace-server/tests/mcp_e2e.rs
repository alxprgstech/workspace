//! Protocol-level MCP acceptance through a real HTTP listener.
//!
//! Starts the actual server binary code (`serve`) on a loopback port with a
//! temporary database, mints a real OAuth access token through the local
//! authorization-code flow (as the consent handler does after approval) and
//! exercises discovery, authentication, `initialize`, `tools/list` and
//! `tools/call` with HTTP requests.
//!
//! ```text
//! cargo test -p workspace-server --test mcp_e2e -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};
use workspace_core::config::Config;

const ORIGIN: &str = "https://workspace.alxprgs.tech";

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn write_config(directory: &std::path::Path, port: u16) -> Config {
    let runtime = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config.toml"),
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

fn seed_token(config: &Config) -> (String, String) {
    let data_dir = &config.storage.data_dir;
    let sso = config.sso.as_ref().unwrap();

    workspace_server::storage::initialize(data_dir).unwrap();

    assert_eq!(
        std::env::var("WORKSPACE_AUTH_KEY").ok(),
        Some(base64::engine::general_purpose::STANDARD.encode([7u8; 32]))
    );

    workspace_server::users::grant_access(
        data_dir,
        sso,
        "550e8400-e29b-41d4-a716-446655440000",
        "admin",
    )
    .unwrap();

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

    let pair = store
        .exchange_code(
            &client.client_id,
            &client.client_secret,
            &code,
            "https://chatgpt.com/connector_platform_oauth_redirect",
            verifier,
        )
        .unwrap();

    (pair.access_token, pair.scope)
}

async fn wait_for_health(client: &reqwest::Client, base: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);

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

fn body_of(text: &str) -> Value {
    // Streamable HTTP may answer as JSON or as one or more SSE events.
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

async fn post_mcp(
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

#[tokio::test]
#[ignore = "binds a loopback HTTP server and seeds a real OAuth token"]
async fn mcp_protocol_end_to_end() {
    let directory = tempfile::tempdir().unwrap();
    let port = free_port();
    let config = write_config(directory.path(), port);

    // SAFETY: the test process is single-threaded at this point and no other
    // test reads these variables.
    unsafe {
        std::env::set_var("WORKSPACE_SSO_CLIENT_SECRET", "e2e-client-secret-123456");
        std::env::set_var(
            "WORKSPACE_AUTH_KEY",
            base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
        );
    }

    let (token, scope) = seed_token(&config);

    println!("access token scope: {scope}");
    assert!(scope.contains("workspace:read"));
    assert!(scope.contains("workspace:write"));

    let server = tokio::spawn(async move { workspace_server::serve(config).await });

    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    wait_for_health(&client, &base).await;

    // ---- Discovery -------------------------------------------------------
    let metadata: Value = client
        .get(format!("{base}/.well-known/oauth-protected-resource"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    println!("protected resource metadata: {metadata}");

    assert_eq!(metadata["resource"], format!("{ORIGIN}/chatgpt/mcp"));
    assert_eq!(
        metadata["authorization_servers"][0],
        Value::String(ORIGIN.to_owned())
    );
    assert_eq!(metadata["scopes_supported"][0], "workspace.read");

    let authorization: Value = client
        .get(format!("{base}/.well-known/oauth-authorization-server"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert!(authorization["authorization_endpoint"].is_string());
    assert!(authorization["token_endpoint"].is_string());

    // ---- The endpoint must reject anonymous calls -----------------------
    let anonymous = client
        .post(format!("{base}/chatgpt/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);

    let challenge = anonymous
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();

    println!("challenge: {challenge}");
    assert!(challenge.contains("resource_metadata="));

    // ---- initialize ------------------------------------------------------
    let initialize = post_mcp(
        &client,
        &base,
        &token,
        "",
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "protocol-e2e", "version": "1"}
            }
        })
        .to_string(),
    )
    .await;

    assert_eq!(initialize.status(), reqwest::StatusCode::OK);

    let session = initialize
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();

    println!(
        "session: {}",
        if session.is_empty() {
            "(stateless)"
        } else {
            &session
        }
    );

    let initialize_body = body_of(&initialize.text().await.unwrap());
    assert_eq!(
        initialize_body["result"]["serverInfo"]["name"],
        "ALXPRGS Workspace"
    );

    // ---- tools/list ------------------------------------------------------
    let list = post_mcp(
        &client,
        &base,
        &token,
        &session,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#.to_owned(),
    )
    .await;

    assert_eq!(list.status(), reqwest::StatusCode::OK);

    let listed = body_of(&list.text().await.unwrap());
    let tools = listed["result"]["tools"].as_array().expect("tools array");

    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();

    println!("tools: {names:?}");
    assert_eq!(names.len(), 21, "{names:?}");

    for required in [
        "server_info",
        "capabilities_list",
        "workspace_create",
        "exec",
        "job_logs",
        "file_write",
        "file_import",
        "url_import",
        "artifact_publish",
        "artifact_download",
    ] {
        assert!(names.contains(&required), "missing tool {required}");
    }

    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);

        if tool["name"] == "file_import" {
            assert_eq!(tool["_meta"]["openai/fileParams"][0], "file");
        }
    }

    // ---- tools/call ------------------------------------------------------
    let call = post_mcp(
        &client,
        &base,
        &token,
        &session,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "server_info", "arguments": {}}
        })
        .to_string(),
    )
    .await;

    assert_eq!(call.status(), reqwest::StatusCode::OK);

    let called = body_of(&call.text().await.unwrap());
    let text = called["result"]["content"][0]["text"].as_str().unwrap();
    let info: Value = serde_json::from_str(text).unwrap();

    println!("server_info: {info}");

    assert_eq!(info["product"], "ALXPRGS Workspace");
    assert_eq!(info["schema_version"], 9);
    assert!(info["runtime"].is_object());
    assert_eq!(called["result"]["isError"], false);

    // A tool-level error must be structured, not a transport failure.
    let error_call = post_mcp(
        &client,
        &base,
        &token,
        &session,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "workspace_info",
                "arguments": {"workspace_id": "00000000-0000-0000-0000-000000000000"}
            }
        })
        .to_string(),
    )
    .await;

    let errored = body_of(&error_call.text().await.unwrap());
    let payload: Value =
        serde_json::from_str(errored["result"]["content"][0]["text"].as_str().unwrap()).unwrap();

    println!("structured error: {payload}");

    assert_eq!(errored["result"]["isError"], true);
    assert_eq!(payload["error"], "workspace_not_found");

    server.abort();
}
