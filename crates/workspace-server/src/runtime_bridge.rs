//! Docker implementation of the workspace runtime operations.
//!
//! This is the only place that turns a workspace record into Docker resources.
//! Every name and path is derived from server-side identifiers, and every
//! operation is idempotent so retries after a crash are safe.

use async_trait::async_trait;
use std::sync::Arc;
use workspace_core::helper::Operation;
use workspace_core::workspace::Workspace;
use workspace_runtime::docker::{
    ContainerSpec, Docker, INSTANCE_LABEL, KIND_LABEL, Network, WORKSPACE_LABEL,
};
use workspace_runtime::quota;
use workspace_runtime::status::RuntimeMonitor;
use workspace_runtime::{
    RuntimeError, RuntimeSettings, container_name, egress_proxy_name, workspace_network,
};

use crate::workspaces::{QuotaSnapshot, WorkspaceError, WorkspaceRuntime};

/// Bridge between the manager and the dedicated Docker runtime.
pub struct DockerWorkspaceRuntime {
    monitor: Arc<RuntimeMonitor>,
    settings: RuntimeSettings,
    instance_id: String,
    allowlist: Vec<String>,
    egress_binary: String,
}

impl DockerWorkspaceRuntime {
    pub fn new(
        monitor: Arc<RuntimeMonitor>,
        instance_id: String,
        allowlist: Vec<String>,
        egress_binary: Option<String>,
    ) -> Result<Self, WorkspaceError> {
        let settings = monitor.settings().cloned().ok_or_else(|| {
            WorkspaceError::RuntimeUnavailable("runtime is not configured".into())
        })?;

        let egress_binary = egress_binary.unwrap_or_else(|| {
            // The helper and the egress proxy live in the same runtime
            // directory; build the Linux path with forward slashes because
            // this code runs on a Windows host.
            let directory = std::path::Path::new(&settings.helper_path)
                .parent()
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|| format!("{}/bin", settings.data_root));

            format!("{directory}/workspace-egress")
        });

        Ok(Self {
            monitor,
            settings,
            instance_id,
            allowlist,
            egress_binary,
        })
    }

    fn docker(&self) -> Result<&Docker, WorkspaceError> {
        self.monitor.docker().ok_or_else(|| {
            WorkspaceError::RuntimeUnavailable("docker client is unavailable".into())
        })
    }

    fn helper(&self) -> Result<&workspace_runtime::Helper, WorkspaceError> {
        self.monitor
            .helper()
            .ok_or_else(|| WorkspaceError::RuntimeUnavailable("helper is unavailable".into()))
    }

    fn host(&self) -> Result<&workspace_runtime::Host, WorkspaceError> {
        self.monitor
            .host()
            .ok_or_else(|| WorkspaceError::RuntimeUnavailable("runtime host is unavailable".into()))
    }

    fn name(&self, workspace: &Workspace) -> Result<String, WorkspaceError> {
        container_name(&self.instance_id, "workspace", &workspace.id).map_err(map_runtime)
    }

    fn allowlist_enabled(&self) -> bool {
        !self.allowlist.is_empty()
    }

    fn workspace_network(&self, workspace: &Workspace) -> Result<String, WorkspaceError> {
        workspace_network(&self.instance_id, &workspace.id).map_err(map_runtime)
    }

    fn proxy_name(&self) -> Result<String, WorkspaceError> {
        egress_proxy_name(&self.instance_id).map_err(map_runtime)
    }

    fn allowlist_path(&self) -> String {
        format!("{}/egress/allowlist", self.settings.data_root)
    }

    fn proxy_env(&self) -> Result<Vec<(String, String)>, WorkspaceError> {
        let proxy = self.proxy_name()?;
        let url = format!("http://{proxy}:3128");

        Ok(vec![
            ("HTTP_PROXY".to_owned(), url.clone()),
            ("HTTPS_PROXY".to_owned(), url.clone()),
            ("http_proxy".to_owned(), url.clone()),
            ("https_proxy".to_owned(), url),
            ("NO_PROXY".to_owned(), String::new()),
        ])
    }

    fn spec(&self, workspace: &Workspace, name: &str) -> Result<ContainerSpec, WorkspaceError> {
        let workspace_dir =
            crate::workspaces::workspace_data_dir(&self.settings.data_root, &workspace.id);

        let allowlisted = self.allowlist_enabled() && workspace.network_mode == "allowlist";

        let network = if allowlisted {
            Network::Named(self.workspace_network(workspace)?)
        } else {
            Network::None
        };

        let mut environment = vec![
            ("HOME".to_owned(), "/tmp".to_owned()),
            ("PATH".to_owned(), "/usr/local/bin:/usr/bin:/bin".to_owned()),
        ];

        if allowlisted {
            environment.extend(self.proxy_env()?);
        }

        Ok(ContainerSpec {
            name: name.to_owned(),
            image: workspace.image.clone(),
            labels: vec![
                (INSTANCE_LABEL.to_owned(), self.instance_id.clone()),
                (WORKSPACE_LABEL.to_owned(), workspace.id.clone()),
                (KIND_LABEL.to_owned(), "workspace".to_owned()),
            ],
            workspace_dir: format!("{workspace_dir}/files"),
            cache_dir: Some(format!("{workspace_dir}/cache")),
            // Fixed keep-alive command; no caller input is involved.
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "exec sleep infinity".to_owned(),
            ],
            env: environment,
            cpu_millis: workspace.limits.cpu_millis,
            memory_mib: workspace.limits.memory_mib,
            pids: workspace.limits.pids,
            tmpfs_mib: 64,
            network,
            extra_mounts: Vec::new(),
        })
    }

    fn proxy_spec(&self) -> Result<ContainerSpec, WorkspaceError> {
        let name = self.proxy_name()?;

        Ok(ContainerSpec {
            name,
            image: self.settings.base_image.clone(),
            labels: vec![
                (INSTANCE_LABEL.to_owned(), self.instance_id.clone()),
                (KIND_LABEL.to_owned(), "egress".to_owned()),
            ],
            workspace_dir: format!("{}/egress", self.settings.data_root),
            cache_dir: None,
            argv: vec!["/egress".to_owned()],
            env: vec![
                ("HOME".to_owned(), "/tmp".to_owned()),
                (
                    "EGRESS_ALLOWLIST".to_owned(),
                    "/etc/egress/allowlist".to_owned(),
                ),
                ("EGRESS_PORT".to_owned(), "3128".to_owned()),
            ],
            cpu_millis: 200,
            memory_mib: 128,
            pids: 64,
            tmpfs_mib: 16,
            network: Network::Named("bridge".to_owned()),
            extra_mounts: vec![
                (self.egress_binary.clone(), "/egress".to_owned()),
                (self.allowlist_path(), "/etc/egress/allowlist".to_owned()),
            ],
        })
    }

    async fn write_allowlist(&self) -> Result<(), WorkspaceError> {
        let helper = self.helper()?;

        let mut hosts = self.allowlist.clone();
        hosts.sort();
        hosts.dedup();

        let content = hosts.join("\n") + "\n";

        helper
            .call(
                Operation::Mkdir {
                    root: self.settings.data_root.clone(),
                    path: "egress".to_owned(),
                    parents: true,
                },
                &[],
            )
            .await
            .map_err(map_runtime)?;

        helper
            .call(
                Operation::Write {
                    root: self.settings.data_root.clone(),
                    path: "egress/allowlist".to_owned(),
                    create_parents: false,
                    replace: true,
                    expected_sha256: None,
                },
                content.as_bytes(),
            )
            .await
            .map_err(map_runtime)?;

        Ok(())
    }

    /// Create the internal network and the instance egress proxy.
    async fn ensure_egress(&self, workspace: &Workspace) -> Result<(), WorkspaceError> {
        if !self.allowlist_enabled() || workspace.network_mode != "allowlist" {
            return Ok(());
        }

        let docker = self.docker()?;
        let network = self.workspace_network(workspace)?;
        let proxy = self.proxy_name()?;

        self.write_allowlist().await?;

        docker
            .network_create(&network, true, &self.instance_id)
            .await
            .map_err(map_runtime)?;

        let running = docker
            .inspect(&proxy)
            .await
            .is_ok_and(|state| state.running);

        if !running {
            let _ = docker.remove(&proxy).await;

            let spec = self.proxy_spec()?;

            docker.create(&spec).await.map_err(map_runtime)?;
            docker.start(&proxy).await.map_err(map_runtime)?;
        }

        docker
            .network_connect(&network, &proxy)
            .await
            .map_err(map_runtime)?;

        Ok(())
    }

    /// Remove the workspace network; the shared proxy stays for other spaces.
    async fn remove_egress(&self, workspace: &Workspace) -> Result<(), WorkspaceError> {
        if !self.allowlist_enabled() || workspace.network_mode != "allowlist" {
            return Ok(());
        }

        let docker = self.docker()?;
        let network = self.workspace_network(workspace)?;
        let proxy = self.proxy_name()?;

        let _ = docker.network_disconnect(&network, &proxy).await;
        let _ = docker.network_remove(&network).await;

        Ok(())
    }
}

#[async_trait]
impl WorkspaceRuntime for DockerWorkspaceRuntime {
    async fn prepare_storage(&self, workspace: &Workspace) -> Result<(), WorkspaceError> {
        let helper = self.helper()?;
        let host = self.host()?;

        let directory =
            crate::workspaces::workspace_data_dir(&self.settings.data_root, &workspace.id);

        helper
            .call(
                Operation::Mkdir {
                    root: self.settings.data_root.clone(),
                    path: format!("workspaces/{}/files", workspace.id),
                    parents: true,
                },
                &[],
            )
            .await
            .map_err(map_runtime)?;

        helper
            .call(
                Operation::Mkdir {
                    root: self.settings.data_root.clone(),
                    path: format!("workspaces/{}/cache", workspace.id),
                    parents: true,
                },
                &[],
            )
            .await
            .map_err(map_runtime)?;

        let _ = directory;

        quota::assign(host, workspace.project_id, &workspace.id)
            .await
            .map_err(map_runtime)?;

        quota::set_limits(
            host,
            workspace.project_id,
            workspace.limits.disk_mib * 1024 * 1024,
            workspace.limits.files,
        )
        .await
        .map_err(map_runtime)?;

        self.ensure_egress(workspace).await?;

        Ok(())
    }

    async fn release_storage(&self, workspace: &Workspace) -> Result<(), WorkspaceError> {
        let helper = self.helper()?;
        let host = self.host()?;

        self.remove_egress(workspace).await?;

        helper
            .call(
                Operation::Remove {
                    root: self.settings.data_root.clone(),
                    path: format!("workspaces/{}", workspace.id),
                    recursive: true,
                },
                &[],
            )
            .await
            .map_err(map_runtime)?;

        quota::clear(host, workspace.project_id)
            .await
            .map_err(map_runtime)?;

        Ok(())
    }

    async fn ensure_network(&self, workspace: &Workspace) -> Result<(), WorkspaceError> {
        self.ensure_egress(workspace).await
    }

    async fn start_container(&self, workspace: &Workspace) -> Result<String, WorkspaceError> {
        let docker = self.docker()?;
        let name = self.name(workspace)?;

        self.ensure_egress(workspace).await?;

        match docker.inspect(&name).await {
            Ok(state) if state.running => return Ok(name),
            Ok(_) => {
                let _ = docker.remove(&name).await;
            }
            Err(_) => {}
        }

        let spec = self.spec(workspace, &name)?;

        docker.create(&spec).await.map_err(map_runtime)?;
        docker.start(&name).await.map_err(map_runtime)?;

        Ok(name)
    }

    async fn stop_container(&self, workspace: &Workspace) -> Result<(), WorkspaceError> {
        let docker = self.docker()?;

        let mut names = Vec::new();

        if let Some(name) = workspace.container_name.clone() {
            names.push(name);
        }

        let computed = self.name(workspace)?;

        if !names.contains(&computed) {
            names.push(computed);
        }

        for name in names {
            // Removing a container that never existed is not an error.
            docker.remove(&name).await.map_err(map_runtime)?;
        }

        Ok(())
    }

    async fn quota_usage(&self, workspace: &Workspace) -> Result<QuotaSnapshot, WorkspaceError> {
        let usage = quota::show(self.host()?, workspace.project_id)
            .await
            .map_err(map_runtime)?;

        Ok(match usage {
            Some(usage) => QuotaSnapshot {
                used_bytes: usage.used_bytes,
                used_files: usage.used_files,
                hard_bytes: usage.hard_bytes,
                hard_files: usage.hard_files,
            },
            None => QuotaSnapshot {
                hard_bytes: workspace.limits.disk_mib * 1024 * 1024,
                hard_files: workspace.limits.files,
                ..QuotaSnapshot::default()
            },
        })
    }
}

fn map_runtime(error: RuntimeError) -> WorkspaceError {
    match error {
        RuntimeError::NotConfigured | RuntimeError::Timeout(_) => {
            WorkspaceError::RuntimeUnavailable(error.to_string())
        }
        other => WorkspaceError::Storage(other.to_string()),
    }
}

/// Runtime that refuses every operation because execution is not configured.
pub struct UnavailableRuntime {
    reason: String,
}

impl UnavailableRuntime {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    fn refuse<T>(&self) -> Result<T, WorkspaceError> {
        Err(WorkspaceError::RuntimeUnavailable(self.reason.clone()))
    }
}

#[async_trait]
impl WorkspaceRuntime for UnavailableRuntime {
    async fn prepare_storage(&self, _workspace: &Workspace) -> Result<(), WorkspaceError> {
        self.refuse()
    }

    async fn release_storage(&self, _workspace: &Workspace) -> Result<(), WorkspaceError> {
        self.refuse()
    }

    async fn start_container(&self, _workspace: &Workspace) -> Result<String, WorkspaceError> {
        self.refuse()
    }

    async fn stop_container(&self, _workspace: &Workspace) -> Result<(), WorkspaceError> {
        self.refuse()
    }

    async fn quota_usage(&self, _workspace: &Workspace) -> Result<QuotaSnapshot, WorkspaceError> {
        self.refuse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workspace_core::workspace::{ExecutionMode, Limits, WorkspaceState};

    fn workspace() -> Workspace {
        Workspace {
            id: "550e8400-e29b-41d4-a716-446655440000".into(),
            owner_id: "owner".into(),
            name: "probe".into(),
            execution_mode: ExecutionMode::Persistent,
            state: WorkspaceState::Created,
            capabilities: vec!["base".into()],
            image: "debian@sha256:8aa5a7bf5a1955bddbe7d74fcdd0b91ab30e74362a8b73af672b0481b35b44a0"
                .into(),
            network_mode: "none".into(),
            limits: Limits {
                cpu_millis: 1000,
                memory_mib: 512,
                pids: 128,
                disk_mib: 1024,
                files: 1000,
                max_job_seconds: 600,
            },
            project_id: 1020,
            container_name: None,
            created_at_unix: 0,
            updated_at_unix: 0,
            last_activity_unix: 0,
            ttl_seconds: 3600,
            expires_at_unix: 3600,
            pinned: false,
            error: None,
        }
    }

    fn runtime(allowlist: Vec<String>) -> DockerWorkspaceRuntime {
        DockerWorkspaceRuntime {
            monitor: Arc::new(RuntimeMonitor::new(
                None,
                "f32ac08f-5f8c-41df-8a72-820d9ff123d8",
            )),
            settings: RuntimeSettings {
                backend: workspace_runtime::RuntimeBackend::RootlessDocker,
                socket: "unix:///run/user/1000/docker.sock".into(),
                docker_path: "/usr/bin/docker".into(),
                engine_name: "alxprgs-workspace".into(),
                data_root: "/srv/alxprgs-workspace/data".into(),
                helper_path: "/srv/alxprgs-workspace/bin/workspace-helper".into(),
                startup_timeout: std::time::Duration::from_secs(1),
                command_timeout: std::time::Duration::from_secs(1),
                workspace_storage_mib: 1024,
                workspace_files: 1000,
                minimum_free_mib: 10,
                base_image: "debian@sha256:00".into(),
            },
            instance_id: "f32ac08f-5f8c-41df-8a72-820d9ff123d8".into(),
            allowlist,
            egress_binary: "/srv/alxprgs-workspace/bin/workspace-egress".into(),
        }
    }

    #[test]
    fn container_spec_is_derived_from_server_state_only() {
        let runtime = runtime(Vec::new());
        let workspace = workspace();
        let name = runtime.name(&workspace).unwrap();
        let spec = runtime.spec(&workspace, &name).unwrap();

        assert_eq!(name, "alxprgs-f32ac08f-workspace-550e8400");
        assert!(
            spec.workspace_dir
                .ends_with("/workspaces/550e8400-e29b-41d4-a716-446655440000/files")
        );
        assert_eq!(spec.cpu_millis, 1000);
        assert_eq!(spec.memory_mib, 512);
        assert_eq!(spec.network, Network::None);

        let arguments = spec.arguments("create").unwrap().join(" ");

        assert!(arguments.contains("--cap-drop ALL"));
        assert!(arguments.contains("--network none"));
        assert!(!arguments.contains("--privileged"));
    }

    #[test]
    fn allowlisted_workspaces_use_an_internal_network_and_proxy() {
        let runtime = runtime(vec!["crates.io".to_owned()]);
        let mut workspace = workspace();
        workspace.network_mode = "allowlist".to_owned();

        let spec = runtime
            .spec(&workspace, "alxprgs-f32ac08f-workspace-550e8400")
            .unwrap();

        assert_eq!(
            spec.network,
            Network::Named("alxprgs-f32ac08f-ws-550e8400".to_owned())
        );
        assert!(spec.env.iter().any(|(key, value)| {
            key == "HTTPS_PROXY" && value.contains("alxprgs-f32ac08f-egress:3128")
        }));

        let arguments = spec.arguments("create").unwrap().join(" ");
        assert!(arguments.contains("--network alxprgs-f32ac08f-ws-550e8400"));
    }

    #[test]
    fn proxy_spec_mounts_only_server_generated_paths() {
        let runtime = runtime(vec!["crates.io".to_owned()]);
        let spec = runtime.proxy_spec().unwrap();

        assert_eq!(spec.argv, vec!["/egress"]);
        assert_eq!(spec.network, Network::Named("bridge".to_owned()));
        assert_eq!(spec.extra_mounts.len(), 2);

        let arguments = spec.arguments("create").unwrap().join(" ");
        assert!(arguments.contains("target=/egress,readonly"));
        assert!(arguments.contains("target=/etc/egress/allowlist,readonly"));
    }
}
