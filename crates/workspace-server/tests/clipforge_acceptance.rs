//! ClipForge acceptance: real Rust project development inside a workspace.
//!
//! The archive is a vendored copy of ClipForge at commit
//! `91db3f530b8f2eb7a24d35c087e18e72a6507d38` with the Tauri desktop member
//! removed, so the CLI can be built offline while the runtime network policy is
//! `none`. Set `ALXPRGS_CLIPFORGE_ARCHIVE` to that zip and run:
//!
//! ```text
//! cargo test -p workspace-server --test clipforge_acceptance -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use workspace_core::config::Config;
use workspace_core::job::JobState;
use workspace_runtime::status::RuntimeMonitor;
use workspace_server::artifacts::{ArtifactSettings, ArtifactStore, Artifacts};
use workspace_server::files::Files;
use workspace_server::images::ImageBuilder;
use workspace_server::jobs::{DockerJobRuntime, ExecRequest, ExecSettings, Executor, JobStore};
use workspace_server::runtime_bridge::DockerWorkspaceRuntime;
use workspace_server::workspaces::{
    CreateRequest, Manager, ManagerSettings, Store, WorkspaceRuntime,
};

fn config() -> Config {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config.toml");

    Config::load(path).expect("config.toml with a [runtime] section is required")
}

fn archive() -> PathBuf {
    std::env::var_os("ALXPRGS_CLIPFORGE_ARCHIVE")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .expect("set ALXPRGS_CLIPFORGE_ARCHIVE to the vendored ClipForge zip")
}

async fn wait_for_prepared(manager: &Manager, owner: &str, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(2400);

    loop {
        let workspace = manager.owned(owner, id).expect("workspace");

        match workspace.state {
            workspace_core::workspace::WorkspaceState::Preparing
            | workspace_core::workspace::WorkspaceState::Starting => {}
            workspace_core::workspace::WorkspaceState::Failed => {
                panic!("workspace preparation failed: {:?}", workspace.error)
            }
            _ => return,
        }

        assert!(
            Instant::now() < deadline,
            "workspace preparation did not finish"
        );

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    owner: String,
    manager: Arc<Manager>,
    files: Arc<Files>,
    executor: Arc<Executor>,
    artifacts: Arc<Artifacts>,
    runtime: Arc<RuntimeMonitor>,
}

async fn fixture() -> Fixture {
    let config = config();
    let directory = tempfile::tempdir().unwrap();

    workspace_server::storage::initialize(directory.path()).unwrap();

    let sso = config.sso.as_ref().expect("sso");
    workspace_server::users::grant_access(
        directory.path(),
        sso,
        "550e8400-e29b-41d4-a716-446655440000",
        "admin",
    )
    .unwrap();

    let owner = workspace_server::users::list_users(directory.path(), sso)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .id;

    let runtime = Arc::new(RuntimeMonitor::new(
        config.runtime.as_ref(),
        "clipforge-instance-01",
    ));

    runtime.warm_up().await;
    let report = runtime.run_acceptance().await.expect("acceptance");
    assert!(report.passed, "runtime acceptance must pass");

    let manager_runtime: Arc<dyn WorkspaceRuntime> = Arc::new(
        DockerWorkspaceRuntime::new(Arc::clone(&runtime), "clipforge-instance-01".to_owned())
            .unwrap(),
    );

    let images = Arc::new(ImageBuilder::new(&runtime, directory.path().to_path_buf()).unwrap());

    let manager = Arc::new(
        Manager::new(
            Store::new(directory.path()),
            manager_runtime,
            ManagerSettings {
                budget: workspace_core::workspace::Limits {
                    cpu_millis: 2000,
                    memory_mib: 2048,
                    pids: 256,
                    disk_mib: 4096,
                    files: 60_000,
                    max_job_seconds: 3600,
                },
                default_ttl_seconds: 7200,
                idle_timeout_seconds: 600,
                network_mode: "none".to_owned(),
                base_image: config.runtime.as_ref().unwrap().base_image.clone(),
            },
        )
        .with_images(images),
    );

    let files = Arc::new(Files::new(&runtime, Arc::clone(&manager), 256 * 1024 * 1024).unwrap());

    let executor = Arc::new(Executor::new(
        JobStore::new(directory.path()),
        Arc::clone(&manager),
        Arc::new(DockerJobRuntime::new(&runtime, "clipforge-instance-01".to_owned()).unwrap()),
        Arc::clone(&runtime),
        ExecSettings {
            max_concurrent_jobs: 1,
            default_timeout_seconds: 1800,
            max_timeout_seconds: 3600,
        },
        "clipforge-instance-01".to_owned(),
    ));

    let artifacts = Arc::new(Artifacts::new(
        ArtifactStore::new(directory.path()),
        Arc::clone(&files),
        ArtifactSettings {
            ttl_seconds: 3600,
            link_ttl_seconds: 600,
            public_base: "https://workspace.alxprgs.tech".to_owned(),
        },
    ));

    Fixture {
        directory,
        owner,
        manager,
        files,
        executor,
        artifacts,
        runtime,
    }
}

async fn run_job(
    fixture: &Fixture,
    workspace_id: &str,
    argv: Vec<String>,
    env: Vec<(String, String)>,
) -> (String, String) {
    let described = argv.clone();

    let job = fixture
        .executor
        .submit(
            &fixture.owner,
            workspace_id,
            ExecRequest {
                argv,
                cwd: String::new(),
                env,
                timeout_seconds: Some(1800),
                idempotency_key: None,
            },
        )
        .await
        .expect("job submission");

    let deadline = Instant::now() + Duration::from_secs(1800);
    let mut state = job.state;

    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;

        let current = fixture
            .executor
            .owned_job(&fixture.owner, &job.id)
            .expect("job status");
        state = current.state;

        if state.is_terminal() {
            break;
        }
    }

    let stdout = fixture
        .executor
        .logs(&fixture.owner, &job.id, "stdout", 0, 512 * 1024)
        .expect("stdout")
        .data;

    let stderr = fixture
        .executor
        .logs(&fixture.owner, &job.id, "stderr", 0, 512 * 1024)
        .expect("stderr")
        .data;

    println!(
        "job {} state={:?} exit={:?}\nstdout tail:\n{}\nstderr tail:\n{}",
        job.id,
        state,
        fixture
            .executor
            .owned_job(&fixture.owner, &job.id)
            .unwrap()
            .exit_code,
        tail(&stdout, 2000),
        tail(&stderr, 4000)
    );

    assert!(
        state == JobState::Succeeded,
        "command {:?} ended in {state:?}",
        described
    );

    (stdout, stderr)
}

fn tail(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }

    text[text.len() - limit..].to_owned()
}

#[tokio::test]
#[ignore = "requires the dedicated runtime and ALXPRGS_CLIPFORGE_ARCHIVE"]
async fn clipforge_development_inside_a_workspace() {
    let archive = archive();
    let fixture = fixture().await;

    // ---- Create a workspace with the full Rust toolchain ------------------
    let workspace = fixture
        .manager
        .reserve(
            &fixture.owner,
            &CreateRequest {
                name: "clipforge".to_owned(),
                execution_mode: Some("ephemeral".to_owned()),
                capabilities: ["base", "rust", "cpp", "git", "ffmpeg", "python"]
                    .iter()
                    .map(|id| (*id).to_owned())
                    .collect(),
                ttl_seconds: Some(7200),
                cpu_millis: Some(2000),
                memory_mib: Some(2048),
                persistent: None,
            },
        )
        .await
        .expect("reserve");

    println!("workspace {} reserved", workspace.id);

    fixture
        .manager
        .prepare(&workspace.id)
        .await
        .expect("prepare");

    wait_for_prepared(&fixture.manager, &fixture.owner, &workspace.id).await;

    let workspace = fixture
        .manager
        .owned(&fixture.owner, &workspace.id)
        .expect("workspace");

    println!("workspace image {}", workspace.image);

    assert!(workspace.capabilities.contains(&"rust".to_owned()));

    // ---- Import the project archive --------------------------------------
    let files = Arc::clone(&fixture.files);
    let staging = files.staging_name();
    let staging_path = files.staging_path(&staging).unwrap();

    let host = fixture.runtime.host().cloned().expect("host");

    let mut command = host
        .stream_stdin_command("/usr/bin/tee", &[staging_path])
        .expect("tee");

    let mut child = command.spawn().expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");

    let mut source = tokio::fs::File::open(&archive).await.expect("archive");
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total = 0u64;

    use tokio::io::AsyncReadExt;

    loop {
        let read = source.read(&mut buffer).await.expect("read");

        if read == 0 {
            break;
        }

        total += read as u64;
        stdin.write_all(&buffer[..read]).await.expect("write");
    }

    drop(stdin);

    assert!(child.wait().await.expect("wait").success());
    println!("uploaded {total} archive bytes to staging");

    let extracted = files
        .extract_staged(
            &fixture.owner,
            &workspace.id,
            &staging,
            "",
            20_000,
            2 * 1024 * 1024 * 1024,
        )
        .await
        .expect("extract");

    println!(
        "extracted {} entries ({} bytes)",
        extracted.entries, extracted.bytes
    );

    let listing = files
        .list(&fixture.owner, &workspace.id, "", 1, 200)
        .await
        .expect("list");

    println!(
        "workspace root: {:?}",
        listing
            .entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>()
    );

    assert!(
        listing
            .entries
            .iter()
            .any(|entry| entry.name == "Cargo.toml")
    );

    // ---- Inspect sources and bump the CLI version ------------------------
    let manifest = files
        .read(
            &fixture.owner,
            &workspace.id,
            "cli/Cargo.toml",
            0,
            64 * 1024,
        )
        .await
        .expect("manifest");

    assert!(manifest.content.contains("version = \"0.3.0\""));

    let patched = files
        .patch(
            &fixture.owner,
            &workspace.id,
            "cli/Cargo.toml",
            "--- a/cli/Cargo.toml\n+++ b/cli/Cargo.toml\n@@ -1,3 +1,3 @@\n [package]\n name = \"clipforge-cli\"\n-version = \"0.3.0\"\n+version = \"0.4.0\"\n",
            Some(&manifest.sha256),
        )
        .await
        .expect("version bump");

    println!("patched cli/Cargo.toml to 0.4.0 ({})", patched.sha256);

    // ---- cargo check / test / build inside the workspace -----------------
    let (_, check_err) = run_job(
        &fixture,
        &workspace.id,
        vec![
            "cargo".to_owned(),
            "check".to_owned(),
            "--offline".to_owned(),
            "-p".to_owned(),
            "clipforge-cli".to_owned(),
        ],
        Vec::new(),
    )
    .await;

    assert!(check_err.contains("Finished") || check_err.contains("Checking"));

    let (test_out, _) = run_job(
        &fixture,
        &workspace.id,
        vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "--offline".to_owned(),
            "-p".to_owned(),
            "clipforge-cli".to_owned(),
        ],
        Vec::new(),
    )
    .await;

    assert!(test_out.contains("test result"), "{test_out}");

    let (build_out, build_err) = run_job(
        &fixture,
        &workspace.id,
        vec![
            "cargo".to_owned(),
            "build".to_owned(),
            "--offline".to_owned(),
            "--release".to_owned(),
            "-p".to_owned(),
            "clipforge-cli".to_owned(),
        ],
        vec![(
            "CARGO_TARGET_DIR".to_owned(),
            "/workspace/target".to_owned(),
        )],
    )
    .await;

    assert!(build_err.contains("Finished"), "{build_err}");
    let _ = build_out;

    // ---- The release binary must run and report 0.4.0 --------------------
    let (version_out, _) = run_job(
        &fixture,
        &workspace.id,
        vec![
            "/workspace/target/release/clipforge".to_owned(),
            "--version".to_owned(),
        ],
        Vec::new(),
    )
    .await;

    println!("clipforge --version => {version_out}");

    assert!(version_out.contains("0.4.0"), "{version_out}");

    // ---- Publish the binary and verify the download ----------------------
    let artifact = fixture
        .artifacts
        .publish(
            &fixture.owner,
            &workspace.id,
            "target/release/clipforge",
            Some("clipforge"),
        )
        .await
        .expect("publish");

    println!(
        "artifact {} size={} sha={}",
        artifact.id, artifact.size_bytes, artifact.sha256
    );

    let link = fixture
        .artifacts
        .download_link(&fixture.owner, &artifact.id, Some(600))
        .expect("link");

    let token = link.url.rsplit('/').next().unwrap();
    let (resolved, stored) = fixture.artifacts.resolve(token).expect("resolve");

    let mut downloaded = Vec::new();
    let mut offset = 0u64;

    loop {
        let chunk = fixture
            .artifacts
            .read(&resolved.id, &stored, offset, 1024 * 1024)
            .await
            .expect("download");
        if chunk.is_empty() {
            break;
        }
        offset += chunk.len() as u64;
        downloaded.extend_from_slice(&chunk);
    }

    use sha2::Digest;

    assert_eq!(
        format!("{:x}", sha2::Sha256::digest(&downloaded)),
        artifact.sha256
    );

    // ---- Cleanup leaves no workspace resources ---------------------------
    fixture
        .manager
        .delete(&fixture.owner, &workspace.id)
        .await
        .expect("delete");

    assert!(fixture.manager.list(&fixture.owner).unwrap().is_empty());
    assert!(fixture.artifacts.resolve(token).is_ok());

    let _ = fixture.directory;
}
