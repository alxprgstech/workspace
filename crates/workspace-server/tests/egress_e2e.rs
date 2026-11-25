//! Real acceptance for the allowlist egress policy (stage 013).
//!
//! Creates a workspace with `network_mode = "allowlist"` through the normal
//! manager, lets it prepare its base image, internal network and proxy, then
//! runs a job that must reach an allowlisted host and fail on a denied one.
//!
//! ```text
//! cargo test -p workspace-server --test egress_e2e -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time::sleep;
use workspace_core::config::Config;
use workspace_core::job::JobState;
use workspace_core::workspace::Limits;
use workspace_runtime::status::RuntimeMonitor;
use workspace_server::images::ImageBuilder;
use workspace_server::jobs::{DockerJobRuntime, ExecRequest, ExecSettings, Executor, JobStore};
use workspace_server::runtime_bridge::DockerWorkspaceRuntime;
use workspace_server::storage;
use workspace_server::users;
use workspace_server::workspaces::{
    CreateRequest, Manager, ManagerSettings, Store, WorkspaceRuntime,
};

const INSTANCE: &str = "egress-instance-1";

fn config() -> Config {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config.toml");

    Config::load(path).expect("config.toml with a [runtime] section is required")
}

#[tokio::test]
#[ignore = "starts the dedicated runtime, builds an image and runs real containers"]
async fn allowlisted_workspace_can_only_reach_the_proxy() {
    let config = config();
    let directory = tempfile::tempdir().unwrap();

    storage::initialize(directory.path()).unwrap();

    let owner = {
        let sso = config.sso.as_ref().unwrap();

        users::grant_access(
            directory.path(),
            sso,
            "550e8400-e29b-41d4-a716-446655440001",
            "admin",
        )
        .unwrap();

        users::list_users(directory.path(), sso)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .id
    };

    let runtime = Arc::new(RuntimeMonitor::new(config.runtime.as_ref(), INSTANCE));

    runtime.warm_up().await;

    assert!(
        runtime.refresh().await.infrastructure_ready,
        "runtime must be ready before this acceptance"
    );

    let settings = ManagerSettings {
        budget: Limits {
            cpu_millis: 1000,
            memory_mib: 512,
            pids: 128,
            disk_mib: 1024,
            files: 5000,
            max_job_seconds: 600,
        },
        default_ttl_seconds: 3600,
        idle_timeout_seconds: 300,
        network_mode: "allowlist".to_owned(),
        base_image: config.runtime.as_ref().unwrap().base_image.clone(),
    };

    let bridge: Arc<dyn WorkspaceRuntime> = Arc::new(
        DockerWorkspaceRuntime::new(
            Arc::clone(&runtime),
            INSTANCE.to_owned(),
            vec!["example.com".to_owned()],
            None,
        )
        .unwrap(),
    );

    let images = Arc::new(ImageBuilder::new(&runtime, directory.path().to_path_buf()).unwrap());

    let manager =
        Arc::new(Manager::new(Store::new(directory.path()), bridge, settings).with_images(images));

    let workspace = manager
        .create(
            &owner,
            &CreateRequest {
                name: "egress".to_owned(),
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

    println!(
        "workspace {} mode={} state={:?}",
        workspace.id, workspace.network_mode, workspace.state
    );

    let network = workspace_runtime::workspace_network(INSTANCE, &workspace.id).unwrap();
    let proxy = workspace_runtime::egress_proxy_name(INSTANCE).unwrap();

    println!("network {network}, proxy {proxy}");

    // Jobs require a fresh isolation acceptance, exactly like production.
    let report = runtime.run_acceptance().await.expect("runtime acceptance");

    assert!(report.passed, "isolation acceptance must pass");

    let executor = Arc::new(Executor::new(
        JobStore::new(directory.path()),
        Arc::clone(&manager),
        Arc::new(
            DockerJobRuntime::new(
                &runtime,
                INSTANCE.to_owned(),
                vec!["example.com".to_owned()],
            )
            .unwrap(),
        ),
        Arc::clone(&runtime),
        ExecSettings {
            max_concurrent_jobs: 1,
            default_timeout_seconds: 120,
            max_timeout_seconds: 600,
        },
        INSTANCE.to_owned(),
    ));

    let job = executor
        .submit(
            &owner,
            &workspace.id,
            ExecRequest {
                argv: vec![
                    "/bin/bash".to_owned(),
                    "-c".to_owned(),
                    "set -u; \
                     printf 'allowed='; curl -sS --max-time 20 -o /dev/null -w '%{http_code}' https://example.com || echo fail; \
                     printf ' denied='; curl -sS --max-time 10 -o /dev/null -w '%{http_code}' https://example.org || echo fail; \
                     printf '\\n'"
                        .to_owned(),
                ],
                cwd: String::new(),
                env: vec![],
                timeout_seconds: Some(120),
                idempotency_key: Some("egress-1".to_owned()),
            },
        )
        .await
        .expect("job submission");

    let deadline = Instant::now() + Duration::from_secs(180);
    let mut final_job = job.clone();

    while Instant::now() < deadline {
        sleep(Duration::from_millis(500)).await;

        final_job = executor.owned_job(&owner, &job.id).expect("job status");

        if final_job.state.is_terminal() {
            break;
        }
    }

    let stdout = executor
        .logs(&owner, &job.id, "stdout", 0, 64 * 1024)
        .expect("logs");

    let stderr = executor
        .logs(&owner, &job.id, "stderr", 0, 64 * 1024)
        .expect("stderr logs");

    println!(
        "job state={:?} exit={:?}\nstdout: {}\nstderr: {}",
        final_job.state, final_job.exit_code, stdout.data, stderr.data
    );

    assert_eq!(final_job.state, JobState::Succeeded, "{}", stderr.data);
    assert!(
        stdout.data.contains("allowed=200"),
        "allowlisted host must be reachable: {}",
        stdout.data
    );
    assert!(
        !stdout.data.contains("denied=200"),
        "a host outside the allowlist must be denied: {}",
        stdout.data
    );

    // Deleting the workspace must remove its network.
    manager.delete(&owner, &workspace.id).await.expect("delete");

    let host = runtime.host().cloned().expect("host");
    let docker_path = runtime.settings().expect("settings").docker_path.clone();
    let networks = host
        .run(
            &docker_path,
            &[
                "network".to_owned(),
                "ls".to_owned(),
                "--format".to_owned(),
                "{{.Name}}".to_owned(),
            ],
        )
        .await
        .unwrap();

    println!("networks: {networks}");

    assert!(!networks.contains(&network));
}
