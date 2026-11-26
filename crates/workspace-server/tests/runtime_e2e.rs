//! Practical end-to-end checks against the dedicated runtime.
//!
//! These tests create real containers, real project quotas and real files, so
//! they only run when explicitly requested:
//!
//! ```text
//! cargo test -p workspace-server --test runtime_e2e -- --ignored --nocapture
//! ```
//!
//! They read the deployment configuration from `config.toml` in the repository
//! root, because that is the file the running instance uses.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use workspace_core::config::Config;
use workspace_core::job::JobState;
use workspace_core::workspace::ExecutionMode;
use workspace_runtime::status::RuntimeMonitor;
use workspace_server::jobs::{
    DockerJobRuntime, ExecRequest, ExecSettings, Executor, JobError, JobStore,
};
use workspace_server::runtime_bridge::DockerWorkspaceRuntime;
use workspace_server::storage;
use workspace_server::workspaces::{
    CreateRequest, Manager, ManagerSettings, Store, WorkspaceRuntime,
};

fn config() -> Config {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config.toml");

    Config::load(path).expect("config.toml with a [runtime] section is required")
}

fn insert_owner(data_dir: &std::path::Path, sso: &workspace_core::config::SsoConfig) -> String {
    workspace_server::users::grant_access(
        data_dir,
        sso,
        "550e8400-e29b-41d4-a716-446655440000",
        "admin",
    )
    .unwrap();

    workspace_server::users::list_users(data_dir, sso)
        .unwrap()
        .into_iter()
        .find(|user| user.subject == "550e8400-e29b-41d4-a716-446655440000")
        .expect("owner exists")
        .id
}

#[tokio::test]
#[ignore = "requires the dedicated WSL2/Docker runtime and its acceptance"]
async fn workspace_lifecycle_and_job_execution() {
    let config = config();

    let directory = tempfile::tempdir().unwrap();
    storage::initialize(directory.path()).unwrap();

    let owner = insert_owner(directory.path(), config.sso.as_ref().unwrap());

    let runtime = Arc::new(RuntimeMonitor::new(
        config.runtime.as_ref(),
        "e2e-instance-0001",
    ));

    runtime.warm_up().await;

    let report = runtime.run_acceptance().await.expect("acceptance");

    for check in &report.checks {
        println!(
            "[{}] {}: {}",
            if check.ok { "ok" } else { "FAIL" },
            check.id,
            check.detail
        );
    }

    assert!(
        report.passed,
        "runtime acceptance must pass before jobs run"
    );

    let settings = ManagerSettings {
        budget: workspace_core::workspace::Limits {
            cpu_millis: 2000,
            memory_mib: 4096,
            pids: 256,
            disk_mib: 2048,
            files: 20_000,
            max_job_seconds: 600,
        },
        default_ttl_seconds: 3600,
        idle_timeout_seconds: 60,
        network_mode: "none".to_owned(),
        base_image: config.runtime.as_ref().unwrap().base_image.clone(),
    };

    let workspace_runtime: Arc<dyn WorkspaceRuntime> = Arc::new(
        DockerWorkspaceRuntime::new(
            Arc::clone(&runtime),
            "e2e-instance-0001".to_owned(),
            Vec::new(),
            None,
        )
        .unwrap(),
    );

    let manager = Arc::new(Manager::new(
        Store::new(directory.path()),
        workspace_runtime,
        settings,
    ));

    let workspace = manager
        .create(
            &owner,
            &CreateRequest {
                name: "e2e".to_owned(),
                execution_mode: Some("ephemeral".to_owned()),
                capabilities: vec![],
                ttl_seconds: Some(600),
                cpu_millis: Some(500),
                memory_mib: Some(256),
                persistent: None,
            },
        )
        .await
        .expect("workspace creation");

    println!("workspace: {} state={:?}", workspace.id, workspace.state);

    assert_eq!(workspace.execution_mode, ExecutionMode::Ephemeral);

    let job_runtime =
        DockerJobRuntime::new(&runtime, "e2e-instance-0001".to_owned(), Vec::new()).unwrap();

    let executor = Executor::new(
        JobStore::new(directory.path()),
        Arc::clone(&manager),
        Arc::new(job_runtime),
        Arc::clone(&runtime),
        ExecSettings {
            max_concurrent_jobs: 1,
            default_timeout_seconds: 60,
            max_timeout_seconds: 600,
        },
        "e2e-instance-0001".to_owned(),
    );

    let job = executor
        .submit(
            &owner,
            &workspace.id,
            ExecRequest {
                argv: vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    "echo out; echo err >&2; exit 3".to_owned(),
                ],
                cwd: String::new(),
                env: vec![],
                timeout_seconds: Some(60),
                idempotency_key: Some("e2e-1".to_owned()),
            },
        )
        .await
        .expect("job submission");

    println!("job: {}", job.id);

    let deadline = Instant::now() + Duration::from_secs(120);
    let mut final_job = job.clone();

    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;

        final_job = executor.owned_job(&owner, &job.id).expect("job status");

        if final_job.state.is_terminal() {
            break;
        }
    }

    println!(
        "job state={:?} exit={:?}",
        final_job.state, final_job.exit_code
    );

    assert_eq!(final_job.state, JobState::Failed);
    assert_eq!(final_job.exit_code, Some(3));

    let stdout = executor
        .logs(&owner, &job.id, "stdout", 0, 64 * 1024)
        .expect("stdout");

    let stderr = executor
        .logs(&owner, &job.id, "stderr", 0, 64 * 1024)
        .expect("stderr");

    println!("stdout: {:?}", stdout.data);
    println!("stderr: {:?}", stderr.data);

    assert!(stdout.data.contains("out"), "{}", stdout.data);
    assert!(stderr.data.contains("err"), "{}", stderr.data);

    // Idempotency: an identical request with the same key returns the same job
    // and never starts a second execution.
    let duplicate = executor
        .submit(
            &owner,
            &workspace.id,
            ExecRequest {
                argv: vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    "echo out; echo err >&2; exit 3".to_owned(),
                ],
                cwd: String::new(),
                env: vec![],
                timeout_seconds: Some(60),
                idempotency_key: Some("e2e-1".to_owned()),
            },
        )
        .await
        .expect("idempotent submission");

    assert_eq!(duplicate.id, job.id);

    // Reusing the key with different parameters is a predictable conflict, not
    // a silent replay of the original command.
    let conflicting = executor
        .submit(
            &owner,
            &workspace.id,
            ExecRequest {
                argv: vec!["/bin/true".to_owned()],
                cwd: String::new(),
                env: vec![],
                timeout_seconds: Some(30),
                idempotency_key: Some("e2e-1".to_owned()),
            },
        )
        .await;

    assert!(
        matches!(conflicting, Err(JobError::IdempotencyConflict)),
        "unexpected result: {conflicting:?}"
    );

    // ---- Files: write, read, patch, list ---------------------------------
    let files =
        workspace_server::files::Files::new(&runtime, Arc::clone(&manager), 8 * 1024 * 1024)
            .expect("file service");

    let written = files
        .write(
            &owner,
            &workspace.id,
            "src/lib.rs",
            b"pub fn answer() -> u32 {\n    41\n}\n",
            None,
            true,
            true,
        )
        .await
        .expect("file write");

    println!("write sha={} size={}", written.sha256, written.size);

    let chunk = files
        .read(&owner, &workspace.id, "src/lib.rs", 0, 64 * 1024)
        .await
        .expect("file read");

    assert_eq!(chunk.encoding, "utf-8");
    assert!(chunk.content.contains("answer"));

    let patched = files
        .patch(
            &owner,
            &workspace.id,
            "src/lib.rs",
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn answer() -> u32 {\n-    41\n+    42\n }\n",
            Some(&chunk.sha256),
        )
        .await
        .expect("file patch");

    assert_ne!(patched.sha256, written.sha256);

    let listing = files
        .list(&owner, &workspace.id, "", 4, 100)
        .await
        .expect("file list");

    assert!(
        listing
            .entries
            .iter()
            .any(|entry| entry.path == "src/lib.rs"),
        "{:?}",
        listing.entries
    );

    // ---- Artifacts: publish, link, download, survive workspace delete ----
    let artifacts = workspace_server::artifacts::Artifacts::new(
        workspace_server::artifacts::ArtifactStore::new(directory.path()),
        Arc::new(files),
        workspace_server::artifacts::ArtifactSettings {
            ttl_seconds: 3600,
            link_ttl_seconds: 600,
            public_base: "https://workspace.example.test".to_owned(),
            ..Default::default()
        },
    );

    let artifact = artifacts
        .publish(&owner, &workspace.id, "src/lib.rs", Some("lib.rs"))
        .await
        .expect("publish");

    println!(
        "artifact {} sha={} size={}",
        artifact.id, artifact.sha256, artifact.size_bytes
    );

    let link = artifacts
        .download_link(&owner, &artifact.id, Some(600))
        .expect("link");

    assert!(
        link.url
            .starts_with("https://workspace.example.test/artifacts/")
    );

    let token = link.url.rsplit('/').next().unwrap();

    let (resolved, stored_name) = artifacts.resolve(token).expect("resolve");

    assert_eq!(resolved.sha256, artifact.sha256);

    let mut downloaded = Vec::new();
    let mut offset = 0u64;

    loop {
        let bytes = artifacts
            .read(&resolved.id, &stored_name, offset, 1024 * 1024)
            .await
            .expect("download chunk");

        if bytes.is_empty() {
            break;
        }

        offset += bytes.len() as u64;
        downloaded.extend_from_slice(&bytes);
    }

    use sha2::Digest;

    let digest = format!("{:x}", sha2::Sha256::digest(&downloaded));

    assert_eq!(digest, artifact.sha256);
    assert_eq!(downloaded.len() as u64, artifact.size_bytes);

    println!("downloaded {} bytes with verified sha256", downloaded.len());

    // Cleanup must remove the workspace files and free the project quota.
    manager.delete(&owner, &workspace.id).await.expect("delete");

    // The artifact is independent of the workspace lifetime.
    let (still_there, _) = artifacts.resolve(token).expect("artifact survives");

    assert_eq!(still_there.id, artifact.id);

    artifacts
        .store()
        .revoke(&owner, &artifact.id)
        .expect("revoke");

    assert!(artifacts.resolve(token).is_err());

    let remaining = manager.list(&owner).unwrap();

    assert!(remaining.is_empty());

    let status = runtime.status().await;

    assert!(status.execution_ready);
}
