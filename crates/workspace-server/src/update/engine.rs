//! Steps of an update and of a rollback.
//!
//! Every step that changes the installation is preceded by a complete backup
//! and recorded in the journal. On any failure after the first change the
//! engine restores the Windows binary, the replaced WSL components and the
//! database, restarts the previous server and verifies it. A failed rollback is
//! reported as `rollback_failed`; it is never reported as a success.

use super::{
    RollbackOptions, UpdateError, UpdateOptions,
    journal::{Journal, JournalPaths, UpdateStatus, save_journal, unix_now},
    manifest::{FileRole, VerifiedPackage},
    platform::{self, BackupManifest, FileRoleRef, InstalledInfo, Layout, Wsl},
};
use crate::{secrets::AuthSecrets, storage};
use std::{
    fs,
    path::{Path, PathBuf},
};
use workspace_core::PRODUCT_NAME;

struct Progress {
    server_stopped: bool,
    runtime_installed: bool,
    migrated: bool,
    windows_installed: bool,
    new_server_started: bool,
    pre_runtime_ready: bool,
}

fn begin(
    paths: &JournalPaths,
    journal: &mut Journal,
    step: &str,
    progress: u8,
    detail: Option<String>,
) {
    journal.begin_step(step, progress, detail);
    let _ = save_journal(paths, journal);
}

fn complete(paths: &JournalPaths, journal: &mut Journal, step: &str, detail: Option<String>) {
    journal.complete_step(step, detail);
    let _ = save_journal(paths, journal);
}

fn fail(paths: &JournalPaths, journal: &mut Journal, step: &str, error: &UpdateError) {
    journal.fail_step(step, error.to_string());
    journal.error = Some(error.to_string());
    let _ = save_journal(paths, journal);
}

fn message(paths: &JournalPaths, journal: &mut Journal, text: String) {
    journal.message = Some(text);
    journal.updated_at_unix = unix_now();
    let _ = save_journal(paths, journal);
}

fn release_lock(paths: &JournalPaths, token: &str) {
    super::journal::release_lock(paths, token);
}

fn database_hash(data_dir: &Path) -> Option<String> {
    let path = storage::database_path(data_dir);

    super::manifest::sha256_file(&path).ok()
}

/// Run one update. The caller owns the journal and the lock.
pub async fn run_update(
    layout: &Layout,
    config: &workspace_core::config::Config,
    paths: &JournalPaths,
    journal: &mut Journal,
    package: &VerifiedPackage,
    secrets: &AuthSecrets,
    options: &UpdateOptions,
) -> Result<(), UpdateError> {
    let backup_dir = journal
        .backup_dir
        .as_ref()
        .map(PathBuf::from)
        .ok_or_else(|| UpdateError::Internal("journal has no backup directory".into()))?;

    let pre_runtime_ready = if layout.runtime.is_some() && cfg!(windows) {
        platform::self_check(layout, &["runtime", "status"])
            .map(|(ok, _)| ok)
            .unwrap_or(false)
    } else {
        false
    };

    let mut progress = Progress {
        server_stopped: false,
        runtime_installed: false,
        migrated: false,
        windows_installed: false,
        new_server_started: false,
        pre_runtime_ready,
    };

    journal.status = UpdateStatus::Running;
    journal.stage = "drain".into();
    journal.progress = 5;
    journal.message = Some("waiting for active jobs".into());
    save_journal(paths, journal)?;

    let drain_timeout = options
        .drain_timeout
        .unwrap_or_else(|| config.update.drain_timeout());

    let outcome = async {
        begin(paths, journal, "drain", 5, None);

        platform::wait_for_drain(&layout.data_dir, drain_timeout, |active| {
            journal.message = Some(format!("waiting for {active} active job(s)"));
            journal.updated_at_unix = unix_now();
            let _ = save_journal(paths, journal);
        })?;

        complete(paths, journal, "drain", Some("no active jobs".into()));

        // Backup before stopping the server: the web interface can still read
        // the journal during this stage. `VACUUM INTO` produces a consistent
        // SQLite snapshot while the server is running, and copying a running
        // executable is a read-only operation.
        begin(paths, journal, "backup", 20, None);

        let backup = backup_installation(layout, &backup_dir, journal, &package.manifest)?;

        complete(
            paths,
            journal,
            "backup",
            Some(format!(
                "binary {}, database {}, {} runtime component(s)",
                backup
                    .windows_binary_sha256
                    .as_deref()
                    .map(|sha| &sha[..12.min(sha.len())])
                    .unwrap_or("none"),
                &backup.database_sha256[..12.min(backup.database_sha256.len())],
                backup
                    .wsl
                    .as_ref()
                    .map(|wsl| wsl.components.len())
                    .unwrap_or(0)
            )),
        );

        begin(paths, journal, "stop", 30, None);

        progress.server_stopped = platform::stop_server(layout)?;

        // On Windows the launcher may still hold its own executable. The
        // detached updater waits until that process (the launcher, or the
        // server it asked to stop) is gone before replacing files.
        if options.wait_for_invoker {
            super::wait_for_invoker_exit(std::time::Duration::from_secs(180)).await?;
        }

        complete(
            paths,
            journal,
            "stop",
            Some(if progress.server_stopped {
                "server stopped".into()
            } else {
                "server was not running".into()
            }),
        );

        if layout.wsl_required() {
            begin(paths, journal, "install_runtime", 40, None);

            let runtime = layout.runtime.as_ref().expect("checked");
            let wsl = Wsl::new(runtime)?;

            let components = vec![
                (
                    FileRoleRef::Helper,
                    package
                        .component_bytes(FileRole::RuntimeHelper)
                        .map_err(UpdateError::Manifest)?,
                ),
                (
                    FileRoleRef::Egress,
                    package
                        .component_bytes(FileRole::RuntimeEgress)
                        .map_err(UpdateError::Manifest)?,
                ),
                (
                    FileRoleRef::Quota,
                    package
                        .component_bytes(FileRole::QuotaHelper)
                        .map_err(UpdateError::Manifest)?,
                ),
            ];

            let installed = platform::install_wsl_components(layout, &wsl, &components)?;

            progress.runtime_installed = true;

            complete(
                paths,
                journal,
                "install_runtime",
                Some(format!("installed: {}", installed.join(", "))),
            );
        }

        begin(paths, journal, "migrate", 55, None);

        let before = storage::inspect(&layout.data_dir)
            .map_err(|error| UpdateError::Platform(platform::PlatformError::Database(error)))?;
        let instance_id = before
            .as_ref()
            .map(|info| info.instance_id.clone())
            .unwrap_or_default();

        let info = storage::initialize(&layout.data_dir)
            .map_err(|error| UpdateError::Platform(platform::PlatformError::Database(error)))?;

        // From here on the database has been rewritten (or migrated inside a
        // transaction) and must be restored if a later step fails.
        progress.migrated = true;

        if !instance_id.is_empty() && info.instance_id != instance_id {
            return Err(UpdateError::Internal(
                "database instance id changed during migration".into(),
            ));
        }

        if info.schema_version != package.manifest.schema_version {
            return Err(UpdateError::Incompatible(format!(
                "migrated database schema {} does not match the release schema {}",
                info.schema_version, package.manifest.schema_version
            )));
        }

        progress.migrated = true;

        complete(
            paths,
            journal,
            "migrate",
            Some(format!(
                "schema {} -> {} (instance preserved)",
                journal.from_schema, info.schema_version
            )),
        );

        begin(paths, journal, "install_windows", 70, None);

        let binary = package
            .manifest
            .file(FileRole::WindowsBinary)
            .ok_or_else(|| {
                UpdateError::Manifest(super::manifest::ManifestError::Invalid(
                    "release has no Windows binary".into(),
                ))
            })?;

        // Mark the step as "may have changed the binary" before touching it:
        // a failure after the rename still has to restore the backup.
        progress.windows_installed = true;

        platform::install_windows_binary(layout, &package.root.join(&binary.path), &binary.sha256)?;

        progress.windows_installed = true;

        let deploy = package
            .manifest
            .files
            .iter()
            .filter(|file| file.role == FileRole::DeployScript)
            .filter_map(|file| {
                file.path
                    .rsplit_once('/')
                    .map(|(_, name)| (file.path.clone(), name.to_owned()))
                    .or_else(|| Some((file.path.clone(), file.path.clone())))
            })
            .collect::<Vec<_>>();

        platform::install_deploy_scripts(layout, &package.root, &deploy, &backup_dir)?;

        complete(
            paths,
            journal,
            "install_windows",
            Some(format!("sha256 {}", &binary.sha256[..12])),
        );

        if config.update.restart() {
            begin(paths, journal, "start", 82, None);

            let pid = platform::start_server(
                layout,
                secrets.client_secret_value(),
                &secrets.auth_key_base64(),
            )?;

            progress.new_server_started = true;

            message(
                paths,
                journal,
                format!("new server started (pid {pid}); waiting for health"),
            );

            let health = platform::wait_healthy(
                layout,
                &package.manifest.version,
                Some(&package.manifest.git_sha),
                Some(package.manifest.schema_version),
                config.update.health_timeout(),
            )
            .await?;

            complete(
                paths,
                journal,
                "start",
                Some(format!("healthy, build {}", health.build_sha)),
            );

            begin(paths, journal, "verify", 92, None);

            let verification = platform::verify_installation(
                layout,
                &package.manifest.version,
                &package.manifest.git_sha,
                Some(package.manifest.schema_version),
                progress.pre_runtime_ready,
            )
            .await
            .inspect_err(|error| {
                journal
                    .warnings
                    .push(format!("verification failed: {error}"));
            })?;

            if !verification.healthy {
                return Err(UpdateError::Platform(platform::PlatformError::Health(
                    verification.detail,
                )));
            }

            complete(paths, journal, "verify", Some(verification.detail));
        } else {
            begin(paths, journal, "verify", 92, None);
            complete(
                paths,
                journal,
                "verify",
                Some("restart disabled by configuration".into()),
            );
        }

        Ok::<(), UpdateError>(())
    }
    .await;

    match outcome {
        Ok(()) => {
            let installed = InstalledInfo {
                product: PRODUCT_NAME.into(),
                version: package.manifest.version.clone(),
                git_sha: package.manifest.git_sha.clone(),
                build_time: format!("unix:{}", package.manifest.build_time_unix),
                installed_at_unix: Some(unix_now()),
                schema_version: package.manifest.schema_version,
                package_sha256: Some(package.manifest_sha256.clone()),
                previous_version: Some(journal.from_version.clone()),
                backup: Some(backup_dir.display().to_string()),
                database_sha256: database_hash(&layout.data_dir),
                managed: true,
                install_dir: layout.install_dir.display().to_string(),
            };

            installed.write(layout)?;

            journal.status = UpdateStatus::Succeeded;
            journal.stage = "succeeded".into();
            journal.progress = 100;
            journal.message = Some(format!(
                "{} -> {}",
                journal.from_version, package.manifest.version
            ));
            journal.updated_at_unix = unix_now();
            save_journal(paths, journal)?;

            super::write_audit(
                layout,
                "update.finished",
                "updater",
                &format!("version {}", package.manifest.version),
            );

            release_lock(paths, &journal.token);

            // The staged package is no longer needed; the verified manifest
            // remains in the journal and the backup.
            cleanup_staging(layout, &journal.id);

            Ok(())
        }
        Err(error) => {
            journal.error = Some(error.to_string());
            journal.updated_at_unix = unix_now();

            // The step that failed must not stay "running" in the journal.
            let failed_stage = journal.stage.clone();
            journal.fail_step(&failed_stage, error.to_string());
            let _ = save_journal(paths, journal);

            let changed = progress.server_stopped
                || progress.runtime_installed
                || progress.migrated
                || progress.windows_installed
                || progress.new_server_started;

            if !changed {
                // Nothing was replaced. The only side effect may be a stopped
                // server, so bring it back exactly as it was.
                if progress.server_stopped && journal.server_was_running {
                    let _ = platform::start_server(
                        layout,
                        secrets.client_secret_value(),
                        &secrets.auth_key_base64(),
                    );

                    let _ = platform::wait_and_verify_previous(
                        layout,
                        &journal.from_version,
                        Some(&journal.from_git_sha),
                        config.update.health_timeout(),
                    )
                    .await;
                }

                journal.status = UpdateStatus::Failed;
                journal.stage = "failed".into();
                journal.error = Some(error.to_string());
                let _ = save_journal(paths, journal);
                release_lock(paths, &journal.token);

                super::write_audit(layout, "update.failed", "updater", &error.to_string());

                return Err(error);
            }

            journal.stage = "rollback".into();
            journal.progress = 90;
            journal.message = Some("restoring the previous installation".into());
            let _ = save_journal(paths, journal);

            let restore_result = restore_previous(
                layout,
                config,
                paths,
                journal,
                secrets,
                &backup_dir,
                &progress,
            )
            .await;

            match restore_result {
                Ok(()) => {
                    journal.status = UpdateStatus::RolledBack;
                    journal.stage = "rolled_back".into();
                    journal.progress = 100;
                    journal.message = Some(format!(
                        "update failed and was rolled back to {}",
                        journal.from_version
                    ));
                    journal.warnings.push(format!("original failure: {error}"));
                    journal.error = Some(error.to_string());
                    let _ = save_journal(paths, journal);

                    super::write_audit(
                        layout,
                        "update.rolled_back",
                        "updater",
                        &format!("{}: {error}", journal.from_version),
                    );

                    release_lock(paths, &journal.token);

                    Err(error)
                }
                Err(restore_error) => {
                    journal.status = UpdateStatus::RollbackFailed;
                    journal.stage = "rollback_failed".into();
                    journal.progress = 100;
                    journal.error = Some(format!("{error}; rollback failed: {restore_error}"));
                    journal.message =
                        Some("the previous installation could not be restored".into());
                    let _ = save_journal(paths, journal);

                    super::write_audit(
                        layout,
                        "update.rollback_failed",
                        "updater",
                        &restore_error.to_string(),
                    );

                    release_lock(paths, &journal.token);

                    Err(UpdateError::Internal(format!(
                        "update failed ({error}) and the automatic rollback also failed ({restore_error}); the installation needs manual attention"
                    )))
                }
            }
        }
    }
}

fn cleanup_staging(layout: &Layout, keep_id: &str) {
    let staging = layout.updates_dir().join("staging");

    let Ok(entries) = fs::read_dir(&staging) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();

        if name == keep_id || name == "package" {
            continue;
        }

        // Only remove directories that look like our own staging.
        if name.starts_with("source-")
            || name.starts_with("package-")
            || name.starts_with("release-")
        {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

fn backup_installation(
    layout: &Layout,
    backup_dir: &Path,
    journal: &mut Journal,
    manifest: &super::manifest::Manifest,
) -> Result<BackupManifest, UpdateError> {
    fs::create_dir_all(backup_dir.join("state"))?;

    let config_copy = backup_dir.join("config.toml");
    fs::copy(&layout.config_path, &config_copy)?;

    let binary_sha = platform::backup_windows_binary(layout, backup_dir)?;

    let database = storage::database_path(&layout.data_dir);

    let (database_sha256, database_size) = if database.is_file() {
        let target = backup_dir.join("state").join("workspace.db");
        platform::snapshot_database(&layout.data_dir, &target)?;

        let sha = super::manifest::sha256_file(&target)?;
        let size = fs::metadata(&target)?.len();

        (sha, size)
    } else {
        (String::new(), 0)
    };

    if layout.installed_file().is_file() {
        fs::copy(
            layout.installed_file(),
            backup_dir.join("state").join("installed.json"),
        )?;
    }

    let wsl_backup = if layout.wsl_required() {
        let runtime = layout.runtime.as_ref().expect("checked");
        let wsl = Wsl::new(runtime)?;

        Some(platform::backup_wsl(layout, &wsl, backup_dir)?)
    } else {
        None
    };

    let backup = BackupManifest {
        format: 1,
        id: journal.id.clone(),
        created_at_unix: unix_now(),
        action: "update".into(),
        from_version: journal.from_version.clone(),
        from_git_sha: journal.from_git_sha.clone(),
        to_version: manifest.version.clone(),
        from_schema: journal.from_schema,
        to_schema: manifest.schema_version,
        install_dir: layout.install_dir.display().to_string(),
        server_was_running: journal.server_was_running,
        windows_binary_sha256: binary_sha,
        database_sha256,
        database_size,
        wsl: wsl_backup,
        notes: Vec::new(),
    };

    platform::write_json(&backup_dir.join(platform::BACKUP_FILE), &backup)?;

    Ok(backup)
}

async fn restore_previous(
    layout: &Layout,
    config: &workspace_core::config::Config,
    paths: &JournalPaths,
    journal: &mut Journal,
    secrets: &AuthSecrets,
    backup_dir: &Path,
    progress: &Progress,
) -> Result<(), String> {
    begin(paths, journal, "rollback", 90, None);

    if progress.new_server_started
        && let Err(error) = platform::stop_server(layout)
    {
        return Err(format!("cannot stop the failed server: {error}"));
    }

    if progress.migrated {
        // Keep everything the failed server managed to write. It is not
        // silently discarded even though the pre-update snapshot wins.
        let preserved = backup_dir.join("post-update.db");

        if let Err(error) = platform::snapshot_database(&layout.data_dir, &preserved) {
            journal
                .warnings
                .push(format!("cannot preserve the post-update database: {error}"));
        } else {
            journal.warnings.push(format!(
                "database written after the update is preserved at {}",
                preserved.display()
            ));
        }

        platform::restore_database(layout, &backup_dir.join("state").join("workspace.db"))
            .map_err(|error| format!("cannot restore the database: {error}"))?;
    }

    if progress.windows_installed {
        let backup_meta: Option<BackupManifest> =
            platform::read_json(&backup_dir.join(platform::BACKUP_FILE)).ok();

        if backup_meta
            .as_ref()
            .is_some_and(|backup| backup.windows_binary_sha256.is_some())
        {
            platform::restore_windows_binary(layout, backup_dir)
                .map_err(|error| format!("cannot restore the Windows binary: {error}"))?;
            let _ = platform::restore_deploy_scripts(layout, backup_dir);
        } else {
            // A first installation had no previous binary: rolling back must
            // remove the one that was just installed.
            if layout.exe.exists() {
                fs::remove_file(&layout.exe)
                    .map_err(|error| format!("cannot remove the new binary: {error}"))?;
            }
        }
    }

    if progress.runtime_installed {
        let runtime = layout
            .runtime
            .as_ref()
            .ok_or_else(|| "runtime is not configured".to_owned())?;
        let wsl = Wsl::new(runtime).map_err(|error| error.to_string())?;

        platform::restore_wsl(layout, &wsl, backup_dir)
            .map_err(|error| format!("cannot restore the WSL components: {error}"))?;
    }

    if journal.server_was_running {
        platform::start_server(
            layout,
            secrets.client_secret_value(),
            &secrets.auth_key_base64(),
        )
        .map_err(|error| format!("cannot start the previous server: {error}"))?;

        platform::wait_and_verify_previous(
            layout,
            &journal.from_version,
            Some(&journal.from_git_sha),
            config.update.health_timeout(),
        )
        .await
        .map_err(|error| format!("the previous server is not healthy: {error}"))?;
    }

    let installed = InstalledInfo {
        product: PRODUCT_NAME.into(),
        version: journal.from_version.clone(),
        git_sha: journal.from_git_sha.clone(),
        build_time: String::new(),
        installed_at_unix: Some(unix_now()),
        schema_version: storage::inspect(&layout.data_dir)
            .ok()
            .flatten()
            .map(|info| info.schema_version)
            .unwrap_or(journal.from_schema),
        package_sha256: None,
        previous_version: Some(journal.to_version.clone()),
        backup: Some(backup_dir.display().to_string()),
        database_sha256: database_hash(&layout.data_dir),
        managed: true,
        install_dir: layout.install_dir.display().to_string(),
    };

    installed
        .write(layout)
        .map_err(|error| format!("cannot record the installed version: {error}"))?;

    complete(paths, journal, "rollback", None);

    Ok(())
}

/// Restore the last backup explicitly.
pub async fn run_rollback(
    layout: &Layout,
    config: &workspace_core::config::Config,
    paths: &JournalPaths,
    journal: &mut Journal,
    secrets: &AuthSecrets,
    options: &RollbackOptions,
) -> Result<(), UpdateError> {
    let backup_dir = options
        .backup
        .clone()
        .or_else(|| journal.backup_dir.as_ref().map(PathBuf::from))
        .ok_or_else(|| UpdateError::Config("no backup directory was selected".into()))?;

    let backup: BackupManifest = platform::read_json(&backup_dir.join(platform::BACKUP_FILE))
        .map_err(UpdateError::Platform)?;

    // Verify every backup file before touching the installation.
    if let Some(expected) = backup.windows_binary_sha256.as_deref() {
        let binary = backup_dir.join(
            layout
                .exe
                .file_name()
                .ok_or_else(|| UpdateError::Internal("executable has no name".into()))?,
        );

        let actual = super::manifest::sha256_file(&binary)?;

        if actual != expected {
            return Err(UpdateError::Incompatible(
                "the backup binary is damaged".into(),
            ));
        }
    }

    let database_backup = backup_dir.join("state").join("workspace.db");

    if database_backup.is_file() {
        let actual = super::manifest::sha256_file(&database_backup)?;

        if actual != backup.database_sha256 {
            return Err(UpdateError::Incompatible(
                "the backup database is damaged".into(),
            ));
        }
    }

    let pre_runtime_ready = if layout.runtime.is_some() && cfg!(windows) {
        platform::self_check(layout, &["runtime", "status"])
            .map(|(ok, _)| ok)
            .unwrap_or(false)
    } else {
        false
    };

    journal.status = UpdateStatus::Running;
    journal.stage = "stop".into();
    journal.progress = 10;
    journal.message = Some("stopping the server for rollback".into());
    save_journal(paths, journal)?;

    let outcome = async {
        begin(paths, journal, "stop", 10, None);

        platform::stop_server(layout)?;

        complete(paths, journal, "stop", None);

        begin(paths, journal, "safety_copy", 25, None);

        if storage::database_path(&layout.data_dir).is_file() {
            let safety = backup_dir.join(format!("rollback-{}", unix_now()));

            fs::create_dir_all(&safety)?;

            platform::snapshot_database(&layout.data_dir, &safety.join("workspace.db"))?;

            platform::write_json(
                &safety.join("README.json"),
                &serde_json::json!({
                    "kind": "pre-rollback-snapshot",
                    "created_at_unix": unix_now(),
                    "from_version": journal.from_version,
                    "to_version": journal.to_version,
                    "note": "database as it was immediately before the rollback"
                }),
            )?;

            journal.warnings.push(format!(
                "the database before this rollback is preserved at {}",
                safety.display()
            ));
        }

        complete(paths, journal, "safety_copy", None);

        begin(paths, journal, "restore_database", 40, None);

        if database_backup.is_file() {
            platform::restore_database(layout, &database_backup)?;
        }

        complete(paths, journal, "restore_database", None);

        begin(paths, journal, "restore_windows", 60, None);

        if backup.windows_binary_sha256.is_some() {
            platform::restore_windows_binary(layout, &backup_dir)?;
            platform::restore_deploy_scripts(layout, &backup_dir)?;
        }

        complete(paths, journal, "restore_windows", None);

        if layout.wsl_required() {
            begin(paths, journal, "restore_runtime", 75, None);

            let runtime = layout.runtime.as_ref().expect("checked");
            let wsl = Wsl::new(runtime)?;

            platform::restore_wsl(layout, &wsl, &backup_dir)?;

            complete(paths, journal, "restore_runtime", None);
        }

        begin(paths, journal, "start", 85, None);

        // A manual rollback restores a working installation: start it even if
        // the interrupted update had already stopped the server.
        platform::start_server(
            layout,
            secrets.client_secret_value(),
            &secrets.auth_key_base64(),
        )?;

        platform::wait_and_verify_previous(
            layout,
            &backup.from_version,
            Some(&backup.from_git_sha),
            config.update.health_timeout(),
        )
        .await?;

        complete(paths, journal, "start", None);

        begin(paths, journal, "verify", 95, None);

        // A pre-updater build has no version fields in `/health`; its version is
        // proven by executing the binary and by the restored database opening.
        let legacy = platform::probe_binary(&layout.exe)
            .map(|probe| !probe.modern)
            .unwrap_or(false);

        if legacy {
            let health = platform::health(layout).await?;

            if health.status != "ok" {
                return Err(UpdateError::Platform(platform::PlatformError::Health(
                    "the restored legacy server is not healthy".into(),
                )));
            }

            complete(
                paths,
                journal,
                "verify",
                Some("legacy build: health ok, version probed from the binary".into()),
            );
        } else {
            let verification = platform::verify_installation(
                layout,
                &backup.from_version,
                &backup.from_git_sha,
                None,
                pre_runtime_ready,
            )
            .await?;

            if !verification.healthy {
                return Err(UpdateError::Platform(platform::PlatformError::Health(
                    verification.detail,
                )));
            }

            complete(paths, journal, "verify", Some(verification.detail));
        }

        Ok::<(), UpdateError>(())
    }
    .await;

    match outcome {
        Ok(()) => {
            let installed = InstalledInfo {
                product: PRODUCT_NAME.into(),
                version: backup.from_version.clone(),
                git_sha: backup.from_git_sha.clone(),
                build_time: String::new(),
                installed_at_unix: Some(unix_now()),
                schema_version: storage::inspect(&layout.data_dir)
                    .ok()
                    .flatten()
                    .map(|info| info.schema_version)
                    .unwrap_or(backup.from_schema),
                package_sha256: None,
                previous_version: Some(backup.to_version.clone()),
                backup: Some(backup_dir.display().to_string()),
                database_sha256: database_hash(&layout.data_dir),
                managed: true,
                install_dir: layout.install_dir.display().to_string(),
            };

            installed.write(layout)?;

            journal.status = UpdateStatus::RolledBack;
            journal.stage = "rolled_back".into();
            journal.progress = 100;
            journal.message = Some(if journal.from_version == journal.to_version {
                format!("installation recovered at version {}", journal.to_version)
            } else {
                format!(
                    "rolled back from {} to {}",
                    journal.from_version, journal.to_version
                )
            });
            journal.updated_at_unix = unix_now();
            save_journal(paths, journal)?;

            super::write_audit(layout, "rollback.finished", "updater", &journal.to_version);

            release_lock(paths, &journal.token);

            Ok(())
        }
        Err(error) => {
            fail(paths, journal, "rollback", &error);
            journal.status = UpdateStatus::RollbackFailed;
            journal.stage = "rollback_failed".into();
            journal.progress = 100;
            journal.updated_at_unix = unix_now();
            let _ = save_journal(paths, journal);

            release_lock(paths, &journal.token);

            super::write_audit(layout, "rollback.failed", "updater", &error.to_string());

            Err(error)
        }
    }
}

/// Resolve the backup selected by the API by path traversal protections.
pub fn validate_backup_path(layout: &Layout, path: &Path) -> Result<PathBuf, UpdateError> {
    let candidate = fs::canonicalize(path)?;
    let root = fs::canonicalize(layout.backups_dir())?;

    if !candidate.starts_with(&root) {
        return Err(UpdateError::Config(
            "the backup must be inside the installation backup directory".into(),
        ));
    }

    Ok(candidate)
}
