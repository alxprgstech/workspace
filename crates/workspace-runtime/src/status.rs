//! One readiness model for every surface.
//!
//! `doctor`, `/ready`, the HTTP server info, `server_info`, `capabilities_list`
//! and the web panel all read [`RuntimeStatus`]. Nothing else is allowed to make
//! its own readiness claim, which is what keeps the surfaces consistent.
//!
//! Two different questions are answered separately:
//!
//! * **infrastructure readiness** — the dedicated engine, quota filesystem and
//!   helper exist and answer;
//! * **execution readiness** — the isolation policy is proven by actually
//!   running a hardened probe container and a real quota exhaustion test.

use serde::Serialize;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::docker::{ContainerSpec, Docker, EngineInfo, INSTANCE_LABEL, KIND_LABEL};
use crate::helper::{Helper, HelperInfo};
use crate::host::Host;
use crate::storage::{self, FilesystemStatus, WslConfiguration};
use crate::{RuntimeError, RuntimeSettings, container_name, quota};

/// How long a cheap probe result is reused by `/ready`.
const STATUS_CACHE: Duration = Duration::from_secs(10);

/// How long a full acceptance result stays authoritative.
pub const ACCEPTANCE_TTL: Duration = Duration::from_secs(600);

/// Project id reserved for the quota acceptance probe.
pub const PROBE_PROJECT_ID: u32 = 1011;

/// One check, reported with the same identifier on every surface.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StatusCheck {
    pub id: String,
    pub required: bool,
    pub ok: bool,
    pub detail: String,
}

impl StatusCheck {
    fn new(id: &str, required: bool, ok: bool, detail: impl Into<String>) -> Self {
        Self {
            id: id.to_owned(),
            required,
            ok,
            detail: detail.into(),
        }
    }
}

/// Result of the practical runtime acceptance.
#[derive(Debug, Clone, Serialize)]
pub struct AcceptanceReport {
    pub passed: bool,
    pub finished_at_unix: i64,
    pub duration_ms: u64,
    pub checks: Vec<StatusCheck>,
}

impl AcceptanceReport {
    fn blocking(&self) -> Vec<String> {
        self.checks
            .iter()
            .filter(|check| check.required && !check.ok)
            .map(|check| format!("{}: {}", check.id, check.detail))
            .collect()
    }
}

/// Aggregated runtime state.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeStatus {
    pub configured: bool,
    pub backend: Option<String>,
    pub platform: String,
    pub infrastructure_ready: bool,
    pub execution_ready: bool,
    pub engine: Option<EngineInfo>,
    pub storage: Option<FilesystemStatus>,
    pub helper: Option<HelperInfo>,
    pub wsl: Option<WslConfiguration>,
    pub kernel: Option<String>,
    pub acceptance: Option<AcceptanceReport>,
    pub acceptance_age_seconds: Option<u64>,
    pub checks: Vec<StatusCheck>,
    pub blocking: Vec<String>,
    pub checked_at_unix: i64,
}

impl RuntimeStatus {
    /// Status for a deployment that does not configure execution at all.
    pub fn unconfigured(reason: &str) -> Self {
        Self {
            configured: false,
            backend: None,
            platform: std::env::consts::OS.to_owned(),
            infrastructure_ready: false,
            execution_ready: false,
            engine: None,
            storage: None,
            helper: None,
            wsl: None,
            kernel: None,
            acceptance: None,
            acceptance_age_seconds: None,
            checks: vec![StatusCheck::new("runtime.configured", true, false, reason)],
            blocking: vec![format!("runtime.configured: {reason}")],
            checked_at_unix: unix_now(),
        }
    }
}

/// Probe output shared by the cheap status and the practical acceptance.
struct Probe {
    checks: Vec<StatusCheck>,
    engine: Option<EngineInfo>,
    storage: Option<FilesystemStatus>,
    helper: Option<HelperInfo>,
    wsl: Option<WslConfiguration>,
    kernel: Option<String>,
    infrastructure_ready: bool,
}

/// Runtime state machine with cached diagnostics.
pub struct RuntimeMonitor {
    settings: Option<RuntimeSettings>,
    host: Option<Host>,
    docker: Option<Docker>,
    helper: Option<Helper>,
    instance_id: String,
    configuration_error: Option<String>,
    status_cache: Mutex<Option<(Instant, RuntimeStatus)>>,
    acceptance: Mutex<Option<(Instant, AcceptanceReport)>>,
    probe_lock: Mutex<()>,
}

impl RuntimeMonitor {
    /// Build the monitor. A malformed runtime section fails closed: the reason
    /// is reported by every surface instead of being silently ignored.
    pub fn new(config: Option<&workspace_core::config::RuntimeConfig>, instance_id: &str) -> Self {
        let instance_id = instance_id.to_owned();

        let Some(config) = config else {
            return Self {
                settings: None,
                host: None,
                docker: None,
                helper: None,
                instance_id,
                configuration_error: Some("execution runtime is not configured".to_owned()),
                status_cache: Mutex::new(None),
                acceptance: Mutex::new(None),
                probe_lock: Mutex::new(()),
            };
        };

        if let Err(reason) = config.validate() {
            return Self {
                settings: None,
                host: None,
                docker: None,
                helper: None,
                instance_id,
                configuration_error: Some(reason.to_owned()),
                status_cache: Mutex::new(None),
                acceptance: Mutex::new(None),
                probe_lock: Mutex::new(()),
            };
        }

        let (host, settings) = match Host::from_config(config) {
            Ok((host, settings)) => (host, settings),
            Err(_) => {
                return Self {
                    settings: None,
                    host: None,
                    docker: None,
                    helper: None,
                    instance_id,
                    configuration_error: Some(
                        "the configured backend cannot run on this platform".to_owned(),
                    ),
                    status_cache: Mutex::new(None),
                    acceptance: Mutex::new(None),
                    probe_lock: Mutex::new(()),
                };
            }
        };

        let docker = Docker::new(host.clone(), &settings);
        let helper = Helper::new(host.clone(), &settings);

        Self {
            settings: Some(settings),
            host: Some(host),
            docker: Some(docker),
            helper: Some(helper),
            instance_id,
            configuration_error: None,
            status_cache: Mutex::new(None),
            acceptance: Mutex::new(None),
            probe_lock: Mutex::new(()),
        }
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn configured(&self) -> bool {
        self.settings.is_some()
    }

    pub fn settings(&self) -> Option<&RuntimeSettings> {
        self.settings.as_ref()
    }

    pub fn host(&self) -> Option<&Host> {
        self.host.as_ref()
    }

    pub fn docker(&self) -> Option<&Docker> {
        self.docker.as_ref()
    }

    pub fn helper(&self) -> Option<&Helper> {
        self.helper.as_ref()
    }

    pub fn configuration_error(&self) -> Option<&str> {
        self.configuration_error.as_deref()
    }

    /// Cached status, refreshed at most once per [`STATUS_CACHE`].
    pub async fn status(&self) -> RuntimeStatus {
        if let Some((at, status)) = self.status_cache.lock().await.as_ref()
            && at.elapsed() < STATUS_CACHE
        {
            return status.clone();
        }

        let status = self.refresh().await;

        *self.status_cache.lock().await = Some((Instant::now(), status.clone()));

        status
    }

    /// Wake a stopped distribution and wait for the dedicated engine.
    ///
    /// Cold starts are expected: the distribution exits when idle, and the
    /// rootless engine needs a few seconds after boot. This call is bounded by
    /// the configured startup timeout and never starts a workload.
    pub async fn warm_up(&self) {
        if let Some(docker) = self.docker.as_ref() {
            let _ = docker.wait_until_ready().await;
        }
    }

    /// Drop cached diagnostics, for example after an operator action.
    pub async fn invalidate(&self) {
        *self.status_cache.lock().await = None;
        *self.acceptance.lock().await = None;
    }

    /// Re-probe without consulting the cache.
    pub async fn refresh(&self) -> RuntimeStatus {
        let Some(settings) = self.settings.as_ref() else {
            return RuntimeStatus::unconfigured(
                self.configuration_error
                    .as_deref()
                    .unwrap_or("execution runtime is not configured"),
            );
        };

        let probe = self.probe().await;

        let acceptance = self.acceptance.lock().await.clone();

        let acceptance_ok = acceptance
            .as_ref()
            .is_some_and(|(at, report)| at.elapsed() < ACCEPTANCE_TTL && report.passed);

        let mut checks = probe.checks.clone();
        let mut blocking = Vec::new();

        for check in &checks {
            if check.required && !check.ok {
                blocking.push(format!("{}: {}", check.id, check.detail));
            }
        }

        checks.push(StatusCheck::new(
            "runtime.acceptance",
            true,
            acceptance_ok,
            match acceptance.as_ref() {
                Some((at, report)) if at.elapsed() < ACCEPTANCE_TTL => {
                    if report.passed {
                        format!(
                            "isolation acceptance passed {}s ago",
                            at.elapsed().as_secs()
                        )
                    } else {
                        format!(
                            "isolation acceptance failed: {}",
                            report.blocking().join("; ")
                        )
                    }
                }
                Some(_) => "isolation acceptance is stale".to_owned(),
                None => "isolation acceptance has not run".to_owned(),
            },
        ));

        if !acceptance_ok {
            let detail = checks
                .last()
                .map(|check| check.detail.clone())
                .unwrap_or_default();
            blocking.push(format!("runtime.acceptance: {detail}"));
        }

        RuntimeStatus {
            configured: true,
            backend: Some(settings.backend.name().to_owned()),
            platform: std::env::consts::OS.to_owned(),
            infrastructure_ready: probe.infrastructure_ready,
            execution_ready: probe.infrastructure_ready && acceptance_ok,
            engine: probe.engine,
            storage: probe.storage,
            helper: probe.helper,
            wsl: probe.wsl,
            kernel: probe.kernel,
            acceptance_age_seconds: acceptance.as_ref().map(|(at, _)| at.elapsed().as_secs()),
            acceptance: acceptance.map(|(_, report)| report),
            checks,
            blocking,
            checked_at_unix: unix_now(),
        }
    }

    /// Cheap, non-destructive probe of engine, storage, helper and WSL policy.
    async fn probe(&self) -> Probe {
        let _guard = self.probe_lock.lock().await;

        let Some(settings) = self.settings.as_ref() else {
            return Probe {
                checks: vec![StatusCheck::new(
                    "runtime.configured",
                    true,
                    false,
                    self.configuration_error
                        .as_deref()
                        .unwrap_or("not configured"),
                )],
                engine: None,
                storage: None,
                helper: None,
                wsl: None,
                kernel: None,
                infrastructure_ready: false,
            };
        };

        let mut checks = Vec::new();

        let engine = match self.docker.as_ref() {
            Some(docker) => match docker.info().await {
                Ok(info) => {
                    checks.push(StatusCheck::new(
                        "engine.reachable",
                        true,
                        true,
                        format!("Docker {} at {}", info.server_version, info.name),
                    ));

                    checks.push(StatusCheck::new(
                        "engine.dedicated",
                        true,
                        info.name == settings.engine_name,
                        format!(
                            "expected engine {:?}, found {:?}",
                            settings.engine_name, info.name
                        ),
                    ));

                    checks.push(StatusCheck::new(
                        "engine.rootless",
                        true,
                        info.rootless,
                        if info.rootless {
                            "engine reports rootless mode".to_owned()
                        } else {
                            "engine is not rootless; isolation is unproven".to_owned()
                        },
                    ));

                    checks.push(StatusCheck::new(
                        "engine.cgroup_v2",
                        true,
                        info.cgroup_version == "2" && info.cgroup_driver == "systemd",
                        format!(
                            "cgroup version {} with {} driver",
                            info.cgroup_version, info.cgroup_driver
                        ),
                    ));

                    checks.push(StatusCheck::new(
                        "engine.seccomp",
                        true,
                        info.seccomp,
                        format!("security options: {:?}", info.security_options),
                    ));

                    checks.push(StatusCheck::new(
                        "engine.linux",
                        true,
                        info.ostype == "linux",
                        format!("engine operating system: {}", info.ostype),
                    ));

                    checks.push(StatusCheck::new(
                        "engine.unprivileged_data_root",
                        true,
                        info.root_dir.starts_with("/home/"),
                        format!("engine data root: {}", info.root_dir),
                    ));

                    Some(info)
                }
                Err(error) => {
                    checks.push(StatusCheck::new(
                        "engine.reachable",
                        true,
                        false,
                        format!("dedicated engine did not answer: {error}"),
                    ));

                    None
                }
            },
            None => {
                checks.push(StatusCheck::new(
                    "engine.reachable",
                    true,
                    false,
                    "runtime is not configured",
                ));

                None
            }
        };

        let (wsl, kernel) = match settings.backend {
            crate::RuntimeBackend::Wsl2 { .. } => {
                let kernel = match self.host.as_ref() {
                    Some(host) => match storage::kernel_release(host).await {
                        Ok(kernel) => {
                            checks.push(StatusCheck::new(
                                "wsl2.kernel",
                                true,
                                kernel.to_ascii_lowercase().contains("microsoft-standard"),
                                format!("kernel release: {kernel}"),
                            ));

                            Some(kernel)
                        }
                        Err(error) => {
                            checks.push(StatusCheck::new(
                                "wsl2.kernel",
                                true,
                                false,
                                format!("kernel release is unavailable: {error}"),
                            ));

                            None
                        }
                    },
                    None => {
                        checks.push(StatusCheck::new(
                            "wsl2.kernel",
                            true,
                            false,
                            "runtime host is unavailable",
                        ));

                        None
                    }
                };

                let wsl_result = match self.host.as_ref() {
                    Some(host) => Some(storage::wsl_configuration(host).await),
                    None => None,
                };

                let wsl = match wsl_result {
                    Some(Ok(configuration)) => {
                        for (id, actual, wanted) in [
                            (
                                "wsl2.automount_disabled",
                                configuration.automount_enabled,
                                false,
                            ),
                            (
                                "wsl2.interop_disabled",
                                configuration.interop_enabled,
                                false,
                            ),
                            (
                                "wsl2.append_windows_path_disabled",
                                configuration.append_windows_path,
                                false,
                            ),
                        ] {
                            checks.push(StatusCheck::new(
                                id,
                                true,
                                actual == Some(wanted),
                                format!("configured value: {actual:?}"),
                            ));
                        }

                        Some(configuration)
                    }
                    _ => {
                        checks.push(StatusCheck::new(
                            "wsl2.automount_disabled",
                            true,
                            false,
                            "/etc/wsl.conf is unavailable",
                        ));

                        None
                    }
                };

                (wsl, kernel)
            }
            crate::RuntimeBackend::RootlessDocker => (None, None),
        };

        let filesystem_result = match self.host.as_ref() {
            Some(host) => Some(storage::filesystem(host, &settings.data_root).await),
            None => None,
        };

        let storage_status = match filesystem_result {
            Some(Ok(filesystem)) => {
                checks.push(StatusCheck::new(
                    "storage.mounted",
                    true,
                    filesystem.fs_type == "ext4"
                        && filesystem.options.iter().any(|option| option == "nosuid")
                        && filesystem.options.iter().any(|option| option == "nodev"),
                    format!(
                        "{} mounted with {:?}",
                        filesystem.fs_type, filesystem.options
                    ),
                ));

                checks.push(StatusCheck::new(
                    "storage.project_quota",
                    true,
                    filesystem.quota_enabled,
                    if filesystem.quota_enabled {
                        "prjquota is active".to_owned()
                    } else {
                        "prjquota is not active".to_owned()
                    },
                ));

                let minimum = settings.minimum_free_mib * 1024 * 1024;

                checks.push(StatusCheck::new(
                    "storage.free_space",
                    true,
                    filesystem.available_bytes >= minimum,
                    format!(
                        "{} bytes available, {} bytes required",
                        filesystem.available_bytes, minimum
                    ),
                ));

                Some(filesystem)
            }
            _ => {
                checks.push(StatusCheck::new(
                    "storage.mounted",
                    true,
                    false,
                    format!("{} is not mounted", settings.data_root),
                ));

                None
            }
        };

        let helper_result = match self.helper.as_ref() {
            Some(helper) => Some(helper.ping().await),
            None => None,
        };

        let helper = match helper_result {
            Some(Ok(info)) => {
                let unprivileged = info.uid.is_some_and(|uid| uid != 0);
                let protocol_ok = info.protocol == workspace_core::helper::PROTOCOL_VERSION;
                let version_ok = info.version == workspace_core::VERSION;

                checks.push(StatusCheck::new(
                    "helper.available",
                    true,
                    protocol_ok && version_ok && unprivileged,
                    format!(
                        "version {}, protocol {}, uid {:?}",
                        info.version, info.protocol, info.uid
                    ),
                ));

                Some(info)
            }
            _ => {
                checks.push(StatusCheck::new(
                    "helper.available",
                    true,
                    false,
                    "the trusted Linux helper did not answer",
                ));

                None
            }
        };

        let infrastructure_ready = checks.iter().all(|check| !check.required || check.ok);

        Probe {
            checks,
            engine,
            storage: storage_status,
            helper,
            wsl,
            kernel,
            infrastructure_ready,
        }
    }

    /// Practical acceptance: prove isolation and hard quotas on the real host.
    pub async fn run_acceptance(&self) -> Result<AcceptanceReport, RuntimeError> {
        let started = Instant::now();

        let Some(settings) = self.settings.as_ref() else {
            return Err(RuntimeError::NotConfigured);
        };

        let probe = self.probe().await;
        let mut checks = probe.checks.clone();

        if probe.infrastructure_ready {
            checks.extend(self.acceptance_checks(settings).await);
        }

        let passed = checks.iter().all(|check| !check.required || check.ok);

        let report = AcceptanceReport {
            passed,
            finished_at_unix: unix_now(),
            duration_ms: started.elapsed().as_millis() as u64,
            checks,
        };

        *self.acceptance.lock().await = Some((Instant::now(), report.clone()));
        *self.status_cache.lock().await = None;

        Ok(report)
    }

    /// Cached acceptance report, if any.
    pub async fn acceptance(&self) -> Option<AcceptanceReport> {
        self.acceptance
            .lock()
            .await
            .as_ref()
            .map(|(_, report)| report.clone())
    }

    /// Isolation and quota checks that require actually starting containers.
    async fn acceptance_checks(&self, settings: &RuntimeSettings) -> Vec<StatusCheck> {
        let mut checks = Vec::new();

        let (Some(docker), Some(helper), Some(host)) = (
            self.docker.as_ref(),
            self.helper.as_ref(),
            self.host.as_ref(),
        ) else {
            return vec![StatusCheck::new(
                "acceptance.dependencies",
                true,
                false,
                "runtime components are unavailable",
            )];
        };

        checks.push(self.image_check(docker, settings).await);

        checks.push(self.quota_probe(host, helper, settings).await);

        checks.extend(self.container_probe(docker, helper, settings).await);

        checks
    }

    async fn image_check(&self, docker: &Docker, settings: &RuntimeSettings) -> StatusCheck {
        match docker.image_present(&settings.base_image).await {
            Ok(true) => StatusCheck::new(
                "image.base",
                true,
                true,
                format!("{} is present", settings.base_image),
            ),
            Ok(false) => match docker.pull(&settings.base_image).await {
                Ok(()) => StatusCheck::new(
                    "image.base",
                    true,
                    true,
                    format!("{} pulled", settings.base_image),
                ),
                Err(error) => StatusCheck::new(
                    "image.base",
                    true,
                    false,
                    format!(
                        "{} is missing and could not be pulled: {error}",
                        settings.base_image
                    ),
                ),
            },
            Err(error) => StatusCheck::new(
                "image.base",
                true,
                false,
                format!("cannot inspect images: {error}"),
            ),
        }
    }

    /// Write until the hard quota refuses, then verify the inode limit.
    async fn quota_probe(
        &self,
        host: &Host,
        helper: &Helper,
        settings: &RuntimeSettings,
    ) -> StatusCheck {
        let id = Uuid::new_v4().to_string();
        let workspaces = format!("{}/workspaces", settings.data_root);

        let cleanup = async {
            let _ = quota::clear(host, PROBE_PROJECT_ID).await;
            let _ = helper
                .call(
                    workspace_core::helper::Operation::Remove {
                        root: workspaces.clone(),
                        path: id.clone(),
                        recursive: true,
                    },
                    &[],
                )
                .await;
        };

        let result = self.quota_probe_steps(host, helper, &workspaces, &id).await;

        cleanup.await;

        match result {
            Ok(detail) => StatusCheck::new("quota.enforcement", true, true, detail),
            Err(error) => StatusCheck::new(
                "quota.enforcement",
                true,
                false,
                format!("hard quota is not enforced: {error}"),
            ),
        }
    }

    async fn quota_probe_steps(
        &self,
        host: &Host,
        helper: &Helper,
        workspaces: &str,
        id: &str,
    ) -> Result<String, RuntimeError> {
        helper
            .call(
                workspace_core::helper::Operation::Mkdir {
                    root: workspaces.to_owned(),
                    path: id.to_owned(),
                    parents: true,
                },
                &[],
            )
            .await?;

        quota::assign(host, PROBE_PROJECT_ID, id).await?;
        quota::set_limits(host, PROBE_PROJECT_ID, 1024 * 1024, 16).await?;

        // A 2 MiB write into a 1 MiB quota must be refused by the filesystem.
        let oversized = vec![0u8; 2 * 1024 * 1024];

        let blocked = helper
            .call(
                workspace_core::helper::Operation::Write {
                    root: format!("{workspaces}/{id}"),
                    path: "blocked.bin".into(),
                    create_parents: false,
                    replace: true,
                    expected_sha256: None,
                },
                &oversized,
            )
            .await;

        let block_limit_enforced = match &blocked {
            Err(RuntimeError::Helper { message, .. }) => is_quota_error(message),
            _ => false,
        };

        if !block_limit_enforced {
            return Err(RuntimeError::Quota(format!(
                "a 2 MiB write into a 1 MiB quota was not refused: {blocked:?}"
            )));
        }

        // The inode quota must refuse further files too.
        let root = format!("{workspaces}/{id}");
        let mut refused = false;

        for index in 0..32 {
            let outcome = helper
                .call(
                    workspace_core::helper::Operation::Write {
                        root: root.clone(),
                        path: format!("probe-{index}.txt"),
                        create_parents: false,
                        replace: true,
                        expected_sha256: None,
                    },
                    b"x",
                )
                .await;

            if let Err(RuntimeError::Helper { message, .. }) = outcome {
                if is_quota_error(&message) {
                    refused = true;
                    break;
                }

                return Err(RuntimeError::Quota(format!(
                    "unexpected helper failure during the inode probe: {message}"
                )));
            }
        }

        if !refused {
            return Err(RuntimeError::Quota(
                "the inode quota did not refuse new files".into(),
            ));
        }

        let usage = quota::show(host, PROBE_PROJECT_ID)
            .await?
            .ok_or_else(|| RuntimeError::Quota("quota report disappeared".into()))?;

        Ok(format!(
            "block quota and inode quota refused writes as expected ({} bytes used)",
            usage.used_bytes
        ))
    }

    async fn container_probe(
        &self,
        docker: &Docker,
        helper: &Helper,
        settings: &RuntimeSettings,
    ) -> Vec<StatusCheck> {
        let id = Uuid::new_v4().to_string();
        let staging = format!("{}/probe-{id}", settings.data_root);

        let _ = helper
            .call(
                workspace_core::helper::Operation::Mkdir {
                    root: settings.data_root.clone(),
                    path: format!("probe-{id}"),
                    parents: true,
                },
                &[],
            )
            .await;

        let name = match container_name(&self.instance_id, "probe", &id) {
            Ok(name) => name,
            Err(error) => {
                return vec![StatusCheck::new(
                    "container.probe",
                    true,
                    false,
                    error.to_string(),
                )];
            }
        };

        let spec = ContainerSpec {
            name: name.clone(),
            image: settings.base_image.clone(),
            labels: vec![
                (INSTANCE_LABEL.to_owned(), self.instance_id.clone()),
                (KIND_LABEL.to_owned(), "probe".to_owned()),
            ],
            workspace_dir: staging.clone(),
            cache_dir: None,
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                PROBE_SCRIPT.to_owned(),
            ],
            env: vec![
                ("HOME".to_owned(), "/tmp".to_owned()),
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ],
            cpu_millis: PROBE_CPU_MILLIS,
            memory_mib: PROBE_MEMORY_MIB,
            pids: PROBE_PIDS,
            tmpfs_mib: 16,
            network: false,
        };

        let mut checks = Vec::new();

        let run = docker.run_attached(&spec, Duration::from_secs(90)).await;

        let _ = docker.remove(&name).await;

        let _ = helper
            .call(
                workspace_core::helper::Operation::Remove {
                    root: settings.data_root.clone(),
                    path: format!("probe-{id}"),
                    recursive: true,
                },
                &[],
            )
            .await;

        match run {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let fields = parse_probe_output(&stdout);

                checks.push(StatusCheck::new(
                    "container.started",
                    true,
                    output.exit_code == 0,
                    format!(
                        "probe exited with {} stderr={:?}",
                        output.exit_code,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                ));

                checks.push(expect(
                    "container.capabilities_dropped",
                    &fields,
                    "capeff",
                    |value| value == "0000000000000000",
                    "no effective capabilities remain",
                ));

                checks.push(expect(
                    "container.no_new_privileges",
                    &fields,
                    "nonewprivs",
                    |value| value == "1",
                    "NoNewPrivs is set",
                ));

                checks.push(expect(
                    "container.seccomp_filter",
                    &fields,
                    "seccomp",
                    |value| value == "2",
                    "seccomp filter mode is active",
                ));

                checks.push(expect(
                    "container.read_only_root",
                    &fields,
                    "rootfs",
                    |value| value == "readonly",
                    "the root filesystem is read-only",
                ));

                checks.push(expect(
                    "container.no_docker_socket",
                    &fields,
                    "dockersock",
                    |value| value == "absent",
                    "no Docker socket is mounted",
                ));

                checks.push(expect(
                    "container.network_isolated",
                    &fields,
                    "interfaces",
                    |value| value == "lo,",
                    "only the loopback interface exists",
                ));

                checks.push(expect(
                    "container.no_default_route",
                    &fields,
                    "routes",
                    |value| value.parse::<u32>().is_ok_and(|count| count <= 1),
                    "no default route exists",
                ));

                checks.push(expect(
                    "container.cgroup_controllers",
                    &fields,
                    "controllers",
                    |value| {
                        value
                            .split_whitespace()
                            .filter(|name| matches!(*name, "memory" | "pids" | "cpu" | "cpuset"))
                            .count()
                            == 4
                    },
                    "memory, pids, cpu and cpuset controllers are available",
                ));

                checks.push(expect(
                    "container.memory_limit",
                    &fields,
                    "memory_max",
                    |value| value == (PROBE_MEMORY_MIB * 1024 * 1024).to_string(),
                    "the memory limit is applied",
                ));

                checks.push(expect(
                    "container.swap_disabled",
                    &fields,
                    "swap_max",
                    |value| value == "0",
                    "swap is disabled",
                ));

                checks.push(expect(
                    "container.pid_limit",
                    &fields,
                    "pids_max",
                    |value| value == PROBE_PIDS.to_string(),
                    "the pid limit is applied",
                ));

                checks.push(expect(
                    "container.cpu_limit",
                    &fields,
                    "cpu_max",
                    |value| {
                        let mut parts = value.split_whitespace();
                        let quota: f64 = parts.next().unwrap_or("0").parse().unwrap_or(0.0);
                        let period: f64 = parts.next().unwrap_or("0").parse().unwrap_or(1.0);

                        period > 0.0
                            && ((quota / period) - f64::from(PROBE_CPU_MILLIS) / 1000.0).abs()
                                < 0.05
                    },
                    "the CPU quota is applied",
                ));

                checks.push(expect(
                    "container.user_namespace",
                    &fields,
                    "uid_map",
                    |value| {
                        let fields: Vec<&str> = value.split_whitespace().collect();

                        fields.len() >= 3
                            && fields[0] == "0"
                            && fields[1] != "0"
                            && fields[2] == "1"
                    },
                    "container root maps to one unprivileged host uid",
                ));
            }
            Err(error) => {
                checks.push(StatusCheck::new(
                    "container.started",
                    true,
                    false,
                    format!("probe container did not run: {error}"),
                ));
            }
        }

        checks
    }
}

const PROBE_CPU_MILLIS: u32 = 500;
const PROBE_MEMORY_MIB: u64 = 256;
const PROBE_PIDS: u32 = 64;

/// Fixed probe program. It contains no user input and only reads its own state.
const PROBE_SCRIPT: &str = r#"set -u
echo "uid=$(id -u)"
grep -E '^(CapEff|NoNewPrivs|Seccomp):' /proc/self/status
if touch /.alxprgs-readonly-probe 2>/dev/null; then echo "rootfs=writable"; rm -f /.alxprgs-readonly-probe; else echo "rootfs=readonly"; fi
if [ -e /var/run/docker.sock ]; then echo "dockersock=present"; else echo "dockersock=absent"; fi
echo "interfaces=$(ls /sys/class/net | sort | tr '\n' ',')"
echo "routes=$(grep -c '^' /proc/net/route)"
echo "controllers=$(cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null || echo none)"
echo "memory_max=$(cat /sys/fs/cgroup/memory.max 2>/dev/null || echo none)"
echo "swap_max=$(cat /sys/fs/cgroup/memory.swap.max 2>/dev/null || echo none)"
echo "pids_max=$(cat /sys/fs/cgroup/pids.max 2>/dev/null || echo none)"
echo "cpu_max=$(cat /sys/fs/cgroup/cpu.max 2>/dev/null || echo none)"
echo "uid_map=$(head -1 /proc/self/uid_map | tr -s ' ' ' ')"
"#;

/// Whether a helper failure is the filesystem refusing a write because of the
/// hard quota (ENOSPC or EDQUOT).
fn is_quota_error(message: &str) -> bool {
    [
        "No space left",
        "Disk quota",
        "Quota exceeded",
        "os error 122",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

/// Parse `key=value` lines plus `/proc/self/status` style `Key:\tvalue` lines.
fn parse_probe_output(raw: &str) -> std::collections::BTreeMap<String, String> {
    let mut fields = std::collections::BTreeMap::new();

    for line in raw.lines() {
        let line = line.trim();

        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.trim().to_ascii_lowercase(), value.trim().to_owned());
            continue;
        }

        if let Some((key, value)) = line.split_once(':') {
            fields.insert(key.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }

    fields
}

fn expect(
    id: &str,
    fields: &std::collections::BTreeMap<String, String>,
    field: &str,
    predicate: impl Fn(&str) -> bool,
    description: &str,
) -> StatusCheck {
    match fields.get(field) {
        Some(value) if predicate(value) => {
            StatusCheck::new(id, true, true, format!("{description} ({value})"))
        }
        Some(value) => StatusCheck::new(id, true, false, format!("{description}: got {value:?}")),
        None => StatusCheck::new(
            id,
            true,
            false,
            format!("{description}: {field} is missing"),
        ),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_output_parses_both_formats() {
        let raw =
            "uid=0\nCapEff:\t0000000000000000\nNoNewPrivs:\t1\ninterfaces=lo,\nrootfs=readonly\n";
        let fields = parse_probe_output(raw);

        assert_eq!(fields.get("uid").map(String::as_str), Some("0"));
        assert_eq!(
            fields.get("capeff").map(String::as_str),
            Some("0000000000000000")
        );
        assert_eq!(fields.get("nonewprivs").map(String::as_str), Some("1"));
        assert_eq!(fields.get("interfaces").map(String::as_str), Some("lo,"));
    }

    #[test]
    fn expect_reports_missing_fields() {
        let fields = std::collections::BTreeMap::new();
        let check = expect("id", &fields, "missing", |_| true, "should exist");

        assert!(!check.ok);
        assert!(check.detail.contains("missing"));
    }

    #[test]
    fn unconfigured_status_is_fail_closed() {
        let status = RuntimeStatus::unconfigured("not configured");

        assert!(!status.configured);
        assert!(!status.infrastructure_ready);
        assert!(!status.execution_ready);
        assert!(!status.blocking.is_empty());
    }

    #[test]
    fn acceptance_failures_are_reported_as_blocking() {
        let report = AcceptanceReport {
            passed: false,
            finished_at_unix: 0,
            duration_ms: 1,
            checks: vec![
                StatusCheck::new("a", true, true, "ok"),
                StatusCheck::new("b", true, false, "broken"),
            ],
        };

        assert_eq!(report.blocking(), vec!["b: broken".to_owned()]);
    }

    #[test]
    fn monitor_without_configuration_never_claims_readiness() {
        let monitor = RuntimeMonitor::new(None, "f32ac08f-5f8c-41df-8a72-820d9ff123d8");

        assert!(!monitor.configured());
        assert!(monitor.configuration_error().is_some());
    }
}
