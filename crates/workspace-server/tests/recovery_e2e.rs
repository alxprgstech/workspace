//! Restart/recovery acceptance through the real MCP protocol.
//!
//! Starts the real server on a loopback port, creates a workspace, launches a
//! long-running job, kills the server (simulated crash) and starts it again on
//! the same database. The job must become `interrupted`, the workspace must
//! survive, and deleting it afterwards must release everything.
//!
//! ```text
//! cargo test -p workspace-server --test recovery_e2e -- --ignored --nocapture
//! ```

mod support;

use std::time::{Duration, Instant};

use serde_json::json;
use tokio::time::sleep;

use support::{call_tool, free_port, seed_token, set_secrets, wait_for_health, write_config};

async fn wait_for_state(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    workspace_id: &str,
    wanted: &[&str],
    timeout: Duration,
) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    let mut detail = serde_json::Value::Null;

    while Instant::now() < deadline {
        let (error, payload) = call_tool(
            client,
            base,
            token,
            "workspace_info",
            json!({"workspace_id": workspace_id}),
        )
        .await;

        assert!(!error, "workspace_info failed: {payload}");

        let state = payload["workspace"]["state"].as_str().unwrap_or_default();

        if wanted.contains(&state) {
            return payload;
        }

        detail = payload;

        sleep(Duration::from_secs(2)).await;
    }

    panic!("workspace did not reach {wanted:?}: {detail}");
}

#[tokio::test]
#[ignore = "starts the dedicated runtime, builds an image and restarts the server"]
async fn restart_interrupts_the_job_and_keeps_the_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let port = free_port();
    let config = write_config(directory.path(), port);

    set_secrets();

    let token = seed_token(&config, "550e8400-e29b-41d4-a716-446655440002");
    let base = format!("http://127.0.0.1:{port}");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();

    let server = tokio::spawn(async move { workspace_server::serve(config).await });

    wait_for_health(&client, &base).await;

    // Execution becomes ready only after the background isolation acceptance.
    let deadline = Instant::now() + Duration::from_secs(240);

    loop {
        let (_, info) = call_tool(&client, &base, &token, "server_info", json!({})).await;

        if info["execution_ready"] == true {
            break;
        }

        assert!(
            Instant::now() < deadline,
            "execution runtime never became ready: {info}"
        );

        sleep(Duration::from_secs(3)).await;
    }

    println!("execution runtime ready");

    let (error, created) = call_tool(
        &client,
        &base,
        &token,
        "workspace_create",
        json!({
            "name": "recovery",
            "execution_mode": "ephemeral",
            "capabilities": []
        }),
    )
    .await;

    assert!(!error, "workspace_create failed: {created}");

    let workspace_id = created["id"].as_str().unwrap().to_owned();

    println!("workspace {workspace_id}");

    let detail = wait_for_state(
        &client,
        &base,
        &token,
        &workspace_id,
        &["created", "idle"],
        Duration::from_secs(300),
    )
    .await;

    println!(
        "prepared: state={} image={}",
        detail["workspace"]["state"], detail["workspace"]["image"]
    );

    let (error, job) = call_tool(
        &client,
        &base,
        &token,
        "exec",
        json!({
            "workspace_id": workspace_id,
            "argv": ["/bin/sh", "-c", "sleep 120; echo finished"],
            "timeout_seconds": 300,
            "idempotency_key": "recovery-1"
        }),
    )
    .await;

    assert!(!error, "exec failed: {job}");

    let job_id = job["id"].as_str().unwrap().to_owned();

    println!("job {job_id}");

    // Wait until the job is really running before the crash.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut running = false;
    let mut last = serde_json::Value::Null;

    while Instant::now() < deadline {
        let (_, status) = call_tool(
            &client,
            &base,
            &token,
            "job_status",
            json!({"job_id": job_id}),
        )
        .await;

        last = status.clone();

        if status["job"]["state"] == "running" {
            running = true;
            break;
        }

        sleep(Duration::from_millis(500)).await;
    }

    assert!(running, "job never entered the running state: {last}");

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Simulated crash: drop the server without a graceful shutdown.
    server.abort();
    let _ = server.await;

    for _ in 0..40 {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            break;
        }

        sleep(Duration::from_millis(250)).await;
    }

    // Restart on the same database and port.
    let config = write_config(directory.path(), port);
    let restarted = tokio::spawn(async move { workspace_server::serve(config).await });

    wait_for_health(&client, &base).await;

    println!("server restarted");

    let (error, status) = call_tool(
        &client,
        &base,
        &token,
        "job_status",
        json!({"job_id": job_id}),
    )
    .await;

    assert!(!error, "job_status failed: {status}");

    println!("recovered job state={}", status["job"]["state"]);

    assert_eq!(
        status["job"]["state"], "interrupted",
        "a job that was running during the crash must be interrupted: {status}"
    );

    // The workspace itself must still exist and be usable for cleanup.
    let (error, listed) = call_tool(&client, &base, &token, "workspace_list", json!({})).await;

    assert!(!error);

    let ids: Vec<&str> = listed["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|workspace| workspace["id"].as_str())
        .collect();

    assert!(
        ids.contains(&workspace_id.as_str()),
        "workspace lost: {listed}"
    );

    let (error, deleted) = call_tool(
        &client,
        &base,
        &token,
        "workspace_delete",
        json!({"workspace_id": workspace_id}),
    )
    .await;

    assert!(!error, "workspace_delete failed: {deleted}");

    let (_, after) = call_tool(&client, &base, &token, "workspace_list", json!({})).await;

    assert!(
        after["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .all(|workspace| workspace["id"] != workspace_id.as_str()),
        "workspace was not removed: {after}"
    );

    // The server stays healthy after recovery and deletion.
    let health = client.get(format!("{base}/health")).send().await.unwrap();

    assert!(health.status().is_success());

    restarted.abort();
}
