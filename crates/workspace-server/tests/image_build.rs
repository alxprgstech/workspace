//! Practical checks for the trusted capability image builder.
//!
//! ```text
//! cargo test -p workspace-server --test image_build -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use workspace_core::config::Config;
use workspace_runtime::docker::ContainerSpec;
use workspace_runtime::status::RuntimeMonitor;
use workspace_server::images::ImageBuilder;

fn config() -> Config {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config.toml");

    Config::load(path).expect("config.toml with a [runtime] section is required")
}

async fn builder(directory: &std::path::Path) -> (Arc<RuntimeMonitor>, ImageBuilder) {
    let config = config();

    workspace_server::storage::initialize(directory).expect("database");

    let monitor = Arc::new(RuntimeMonitor::new(
        config.runtime.as_ref(),
        "image-test-instance",
    ));

    monitor.warm_up().await;

    let report = monitor.run_acceptance().await.expect("acceptance");

    assert!(report.passed, "runtime acceptance must pass");

    let builder = ImageBuilder::new(&monitor, directory.to_path_buf()).expect("builder");

    (monitor, builder)
}

#[tokio::test]
#[ignore = "builds real images on the dedicated runtime"]
async fn builds_and_caches_a_package_image() {
    let directory = tempfile::tempdir().unwrap();
    let (monitor, builder) = builder(directory.path()).await;

    let modules = vec!["base".to_owned(), "git".to_owned()];

    let image = builder.ensure(&modules).await.expect("build");

    println!("image {} id {}", image.reference, image.image_id);
    assert!(image.reference.contains("alxprgs-workspace/"));
    assert!(image.image_id.starts_with("sha256:"));

    // Second request must reuse the cached image.
    let again = builder.ensure(&modules).await.expect("cached build");

    assert_eq!(again.reference, image.reference);
    assert_eq!(again.image_id, image.image_id);

    // Run a job in the built image and check the toolchain.
    let docker = monitor.docker().cloned().expect("docker");
    let workspace = tempfile::tempdir().unwrap();

    let spec = ContainerSpec {
        name: "alxprgs-image-test-job-000001".into(),
        image: image.reference.clone(),
        labels: vec![],
        workspace_dir: workspace.path().to_string_lossy().to_string(),
        cache_dir: None,
        argv: vec!["git".into(), "--version".into()],
        env: vec![("HOME".into(), "/tmp".into())],
        cpu_millis: 500,
        memory_mib: 256,
        pids: 64,
        tmpfs_mib: 16,
        network: workspace_runtime::docker::Network::None,
        extra_mounts: Vec::new(),
    };

    // The workspace directory must exist inside the Linux filesystem; this
    // test only proves the image runs, so mount the data root staging area.
    let staging = format!(
        "{}/staging",
        monitor.settings().expect("settings").data_root
    );

    let mut spec = spec;
    spec.workspace_dir = staging;

    let run = docker
        .run_attached(&spec, Duration::from_secs(120))
        .await
        .expect("run");

    let _ = docker.remove(&spec.name).await;

    println!(
        "git --version => {:?}",
        String::from_utf8_lossy(&run.stdout)
    );

    assert_eq!(run.exit_code, 0);
    assert!(String::from_utf8_lossy(&run.stdout).contains("git version"));
}

#[tokio::test]
#[ignore = "builds the full Rust/C++/FFmpeg image on the dedicated runtime"]
async fn builds_the_rust_toolchain_image() {
    let directory = tempfile::tempdir().unwrap();
    let (monitor, builder) = builder(directory.path()).await;

    let modules: Vec<String> = ["base", "rust", "cpp", "git", "ffmpeg", "python"]
        .iter()
        .map(|id| (*id).to_owned())
        .collect();

    let image = builder.ensure(&modules).await.expect("build");

    println!("image {} id {}", image.reference, image.image_id);

    let docker = monitor.docker().cloned().expect("docker");
    let staging = format!(
        "{}/staging",
        monitor.settings().expect("settings").data_root
    );

    let spec = ContainerSpec {
        name: "alxprgs-image-test-rust-0001".into(),
        image: image.reference.clone(),
        labels: vec![],
        workspace_dir: staging,
        cache_dir: None,
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "cargo --version && rustc --version && cargo clippy --version && \
             g++ --version | head -1 && python3 --version && ffmpeg -version | head -1"
                .into(),
        ],
        env: vec![
            ("HOME".into(), "/tmp".into()),
            (
                "PATH".into(),
                "/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin".into(),
            ),
        ],
        cpu_millis: 1000,
        memory_mib: 1024,
        pids: 128,
        tmpfs_mib: 64,
        network: workspace_runtime::docker::Network::None,
        extra_mounts: Vec::new(),
    };

    let run = docker
        .run_attached(&spec, Duration::from_secs(300))
        .await
        .expect("run");

    let _ = docker.remove(&spec.name).await;

    let stdout = String::from_utf8_lossy(&run.stdout).into_owned();

    println!("{stdout}");

    assert_eq!(run.exit_code, 0);
    assert!(stdout.contains("cargo "), "{stdout}");
    assert!(stdout.contains("rustc 1.100"), "{stdout}");
    assert!(stdout.contains("clippy"), "{stdout}");
    assert!(stdout.contains("g++"), "{stdout}");
    assert!(stdout.contains("Python 3"), "{stdout}");
    assert!(stdout.contains("ffmpeg version"), "{stdout}");
}
