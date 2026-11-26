#![forbid(unsafe_code)]

use clap::{ArgAction, Args, Parser, Subcommand};
use std::{
    error::Error,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};
use tracing::level_filters::LevelFilter;
use workspace_core::{
    BUILD_TIME, GIT_SHA, PRODUCT_NAME, VERSION,
    config::{Config, LogLevel},
};
use workspace_server::update;

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Parser)]
#[command(
    name = "workspace",
    disable_version_flag = true,
    about = "ALXPRGS Workspace execution platform"
)]
struct Cli {
    #[arg(long, global = true, default_value = "config.toml")]
    config: PathBuf,

    /// Print version, build SHA and the installed release
    #[arg(short = 'V', long = "version", global = true, action = ArgAction::SetTrue)]
    version: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Display product version
    Version,

    /// Check for, install or roll back a release
    Update(UpdateArgs),

    /// Local release tooling (key generation, signing, packaging)
    Release {
        #[command(subcommand)]
        command: ReleaseCommand,
    },

    /// Manage and validate configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Validate ALXPRGS SSO discovery and signing keys
    SsoCheck,

    /// Validate authentication secrets without displaying them
    SsoSecretsCheck,

    /// Manage local Workspace user permissions
    Users {
        #[command(subcommand)]
        command: UsersCommand,
    },

    /// Manage registered MCP OAuth clients
    McpClients {
        #[command(subcommand)]
        command: McpClientsCommand,
    },

    /// Diagnose the local installation
    Doctor {
        /// Skip the practical isolation acceptance (fast, infrastructure only)
        #[arg(long)]
        quick: bool,
    },

    /// Inspect or accept the dedicated execution runtime
    Runtime {
        #[command(subcommand)]
        command: RuntimeCommand,
    },

    /// Start the Workspace HTTP server
    Serve,
}

#[derive(Debug, Args)]
struct UpdateArgs {
    #[command(subcommand)]
    command: Option<UpdateCommand>,

    /// Only check whether a newer verified release is available
    #[arg(long)]
    check: bool,

    /// Show the last update journal and rollback availability
    #[arg(long)]
    status: bool,

    /// Restore the previous release and its pre-update database snapshot
    #[arg(long)]
    rollback: bool,

    /// Use a specific release package (directory or zip)
    #[arg(long)]
    package: Option<PathBuf>,

    /// Installation directory (defaults to the running binary's directory)
    #[arg(long)]
    install_dir: Option<PathBuf>,

    /// Reinstall the same version
    #[arg(long)]
    force: bool,

    /// Allow installing an older version
    #[arg(long)]
    allow_downgrade: bool,

    /// Confirm that database writes after the update may be replaced on rollback
    #[arg(long)]
    allow_data_restore: bool,

    /// First installation: do not require an existing workspace.exe
    #[arg(long)]
    bootstrap: bool,

    /// Do not ask for confirmation (required for a non-interactive rollback)
    #[arg(long)]
    yes: bool,

    /// Machine-readable output
    #[arg(long)]
    json: bool,

    /// Wait this many seconds for active jobs before aborting
    #[arg(long)]
    drain_timeout: Option<u64>,
}

#[derive(Debug, Subcommand)]
enum UpdateCommand {
    /// Internal: run the detached updater for a prepared journal
    #[command(hide = true)]
    Apply {
        #[arg(long)]
        journal: PathBuf,

        #[arg(long)]
        install_dir: PathBuf,

        /// Wait for the launching process to exit before changing files
        #[arg(long)]
        wait_invoker: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ReleaseCommand {
    /// Generate an Ed25519 release key pair
    Keygen {
        #[arg(long)]
        out: PathBuf,

        #[arg(long)]
        force: bool,
    },

    /// Write a detached signature for an existing manifest
    Sign {
        manifest: PathBuf,

        #[arg(long)]
        key: PathBuf,
    },

    /// Build a signed, deterministic release zip
    Package {
        /// Directory containing workspace.exe, linux/ and deploy/
        #[arg(long)]
        files: PathBuf,

        #[arg(long)]
        output: PathBuf,

        #[arg(long = "release-version")]
        release_version: String,

        #[arg(long)]
        key: PathBuf,

        #[arg(long, default_value = "stable")]
        channel: String,

        #[arg(long)]
        git_sha: Option<String>,

        #[arg(long)]
        build_time_unix: Option<i64>,

        #[arg(long, default_value_t = 11)]
        schema_version: u32,

        #[arg(long)]
        min_schema: Option<u32>,

        #[arg(long)]
        max_schema: Option<u32>,

        #[arg(long)]
        changelog: Option<PathBuf>,
    },

    /// Verify a package with the embedded or an explicit public key
    Verify {
        #[arg(long)]
        package: PathBuf,

        #[arg(long)]
        public_key: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Validate the configuration file
    Check,
}

#[derive(Debug, Subcommand)]
enum UsersCommand {
    /// List local users
    List,

    /// Grant access to an explicitly selected SSO identity
    Grant {
        #[arg(long)]
        subject: String,

        #[arg(long, default_value = "user",
              value_parser = ["user", "admin"])]
        role: String,
    },

    /// Disable a user and revoke their local sessions
    Disable {
        #[arg(long)]
        subject: String,
    },
}

#[derive(Debug, Subcommand)]
enum RuntimeCommand {
    /// Show runtime checks without running containers
    Status,

    /// Run the practical isolation and quota acceptance
    Accept,

    /// Create the dedicated storage layout and verify quotas
    Init,
}

#[derive(Debug, Subcommand)]
enum McpClientsCommand {
    /// Register a confidential OAuth client
    Register {
        #[arg(long)]
        name: String,

        #[arg(long)]
        redirect_uri: String,
    },

    /// List OAuth clients without secrets
    List,

    /// Disable an OAuth client and revoke its grants
    Disable {
        #[arg(long)]
        client_id: String,
    },
}

fn init_logging(level: LogLevel) -> AppResult<()> {
    let filter = match level {
        LogLevel::Trace => LevelFilter::TRACE,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Error => LevelFilter::ERROR,
    };

    tracing_subscriber::fmt()
        .with_max_level(filter)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init()?;

    Ok(())
}

fn run(cli: Cli) -> AppResult<ExitCode> {
    if cli.version || matches!(cli.command, Some(Command::Version)) {
        print_version(&cli.config);
        return Ok(ExitCode::SUCCESS);
    }

    let Some(command) = cli.command else {
        eprintln!("A command is required. Run `workspace --help`.");
        return Ok(ExitCode::from(2));
    };

    // Release tooling works without a deployment configuration: it is used on a
    // build machine, not on an installed instance.
    if let Command::Release { command } = command {
        return run_release(command);
    }

    let config = Config::load(&cli.config)?;
    init_logging(config.logging.level)?;

    tracing::debug!(
        config = %cli.config.display(),
        "Configuration loaded"
    );

    match command {
        Command::Version => unreachable!(),

        Command::Update(args) => run_update(config, &cli.config, args),

        Command::Release { .. } => unreachable!(),

        Command::Config {
            command: ConfigCommand::Check,
        } => {
            println!("Configuration: OK");
            println!("Listen: {}", config.server.listen);
            println!("Storage: {}", config.storage.data_dir.display());
            println!("CPU budget: {} millicores", config.resources.cpu_millis);
            println!("Memory budget: {} MiB", config.resources.memory_mib);
            println!("Storage budget: {} MiB", config.resources.storage_mib);
            println!("Container mode: {:?}", config.workspace.default_mode);
            println!("Network mode: {:?}", config.network.default_mode);

            Ok(ExitCode::SUCCESS)
        }

        Command::Serve => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?;

            runtime.block_on(workspace_server::serve_with_config(
                config,
                cli.config.clone(),
            ))?;

            Ok(ExitCode::SUCCESS)
        }

        Command::Users { command } => {
            let sso = config.sso.as_ref().ok_or(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "SSO is not configured",
            ))?;

            let data_dir = &config.storage.data_dir;

            match command {
                UsersCommand::List => {
                    let users = workspace_server::users::list_users(data_dir, sso)?;

                    println!("Local users: {}", users.len());

                    for user in users {
                        println!(
                            "{}  role={}  enabled={}  username={:?}",
                            user.subject, user.role, user.is_enabled, user.username,
                        );
                    }
                }

                UsersCommand::Grant { subject, role } => {
                    workspace_server::users::grant_access(data_dir, sso, &subject, &role)?;

                    println!("[OK] Access granted");
                    println!("[INFO] Subject: {subject}");
                    println!("[INFO] Role: {role}");
                }

                UsersCommand::Disable { subject } => {
                    let changed = workspace_server::users::disable_access(data_dir, sso, &subject)?;

                    if !changed {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "Local user not found",
                        )
                        .into());
                    }

                    println!("[OK] Access disabled");
                    println!("[OK] Local sessions revoked");
                }
            }

            Ok(ExitCode::SUCCESS)
        }

        Command::McpClients { command } => {
            let sso = config.sso.as_ref().ok_or(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "SSO is not configured",
            ))?;

            let store =
                workspace_server::oauth_store::OAuthStore::new(&config.storage.data_dir, sso)?;

            match command {
                McpClientsCommand::Register { name, redirect_uri } => {
                    let credentials = store.register_client(&name, &redirect_uri)?;

                    println!("[OK] OAuth client registered");
                    println!("Client ID: {}", credentials.client_id);
                    println!("Client Secret: {}", credentials.client_secret);
                    println!("Redirect URI: {redirect_uri}");
                    println!("Resource: {}", store.resource());
                    println!("Scope: workspace.read");
                    println!("Save the client secret securely. It cannot be recovered.");
                }

                McpClientsCommand::List => {
                    let clients = store.list_clients()?;

                    println!("OAuth clients: {}", clients.len());

                    for client in clients {
                        println!(
                            "{}  enabled={}  name={:?}  redirect={:?}  scopes={:?}",
                            client.client_id,
                            client.is_enabled,
                            client.name,
                            client.redirect_uri,
                            client.allowed_scope,
                        );
                    }
                }

                McpClientsCommand::Disable { client_id } => {
                    if !store.disable_client(&client_id)? {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "OAuth client not found",
                        )
                        .into());
                    }

                    println!("[OK] OAuth client disabled");
                    println!("[OK] Client grants revoked");
                }
            }

            Ok(ExitCode::SUCCESS)
        }

        Command::SsoSecretsCheck => {
            workspace_server::secrets::AuthSecrets::load(&config.storage.data_dir)?;

            println!("[OK] Installation secrets available");
            println!("[OK] SSO client secret format");
            println!("[OK] AES-256 encryption key format");

            Ok(ExitCode::SUCCESS)
        }

        Command::SsoCheck => {
            let sso = config.sso.as_ref().ok_or(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "SSO is not configured",
            ))?;

            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;

            let client = runtime.block_on(workspace_server::sso::SsoClient::discover(sso))?;

            println!("[OK] SSO discovery");
            println!("[OK] Trusted HTTPS endpoints");
            println!("[OK] Authorization Code");
            println!("[OK] PKCE S256");
            println!("[OK] RS256");
            println!("[OK] JWKS: {} keys", client.signing_key_count());
            println!("[INFO] Issuer: {}", sso.issuer);
            println!("[INFO] Client ID: {}", sso.client_id);
            println!("[INFO] Redirect URI: {}", sso.redirect_uri);

            Ok(ExitCode::SUCCESS)
        }

        Command::Doctor { quick } => diagnose(&config, quick),

        Command::Runtime { command } => {
            let instance = instance_id(&config)?;
            let monitor =
                workspace_runtime::status::RuntimeMonitor::new(config.runtime.as_ref(), &instance);

            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;

            match command {
                RuntimeCommand::Status => {
                    let status = runtime.block_on(monitor.status());
                    print_runtime_status(&status);

                    Ok(if status.execution_ready {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(2)
                    })
                }

                RuntimeCommand::Accept => {
                    runtime.block_on(monitor.warm_up());

                    let status = runtime.block_on(monitor.status());

                    if !status.infrastructure_ready {
                        print_runtime_status(&status);
                        return Ok(ExitCode::from(2));
                    }

                    println!("Running practical runtime acceptance...");
                    println!();

                    let report = runtime.block_on(monitor.run_acceptance())?;

                    for check in &report.checks {
                        print_check(check);
                    }

                    println!();
                    println!(
                        "Acceptance: {} in {} ms",
                        if report.passed { "PASSED" } else { "FAILED" },
                        report.duration_ms
                    );

                    Ok(if report.passed {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(2)
                    })
                }

                RuntimeCommand::Init => {
                    let Some(settings) = monitor.settings().cloned() else {
                        println!("[FAIL] Execution runtime is not configured");
                        return Ok(ExitCode::from(2));
                    };

                    let Some(helper) = monitor.helper().cloned() else {
                        println!("[FAIL] Trusted helper is unavailable");
                        return Ok(ExitCode::from(2));
                    };

                    for directory in ["workspaces", "artifacts", "staging", "logs", "builds"] {
                        runtime.block_on(helper.call(
                            workspace_core::helper::Operation::Mkdir {
                                root: settings.data_root.clone(),
                                path: directory.to_owned(),
                                parents: true,
                            },
                            &[],
                        ))?;

                        println!("[OK] {}/{}", settings.data_root, directory);
                    }

                    let status = runtime.block_on(monitor.status());
                    print_runtime_status(&status);

                    Ok(if status.infrastructure_ready {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(2)
                    })
                }
            }
        }
    }
}

fn print_version(config_path: &Path) {
    println!("{PRODUCT_NAME} {VERSION}");
    println!("build: {GIT_SHA}");
    println!("built: {BUILD_TIME}");

    if let Ok(config) = Config::load(config_path) {
        // When the configuration's data directory is the installation (the
        // production layout), report *that* installation even if this command
        // runs from a build directory.
        let install_override = config.update.install_dir.clone().or_else(|| {
            let name = if cfg!(windows) {
                "workspace.exe"
            } else {
                "workspace"
            };

            if config.storage.data_dir.join(name).is_file() {
                Some(config.storage.data_dir.clone())
            } else {
                None
            }
        });

        if let Ok(layout) =
            update::InstallLayout::resolve(config_path, &config, install_override.as_deref())
            && let Ok(info) = update::current_installation(&layout)
        {
            println!(
                "installed: {} ({}), schema {}",
                info.version,
                if info.managed { "managed" } else { "detected" },
                info.schema_version
            );
            println!("install dir: {}", layout.install_dir.display());
            println!("config: {}", layout.config_path.display());
        }
    }
}

fn run_update(config: Config, config_path: &Path, args: UpdateArgs) -> AppResult<ExitCode> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    if let Some(UpdateCommand::Apply {
        journal,
        install_dir,
        wait_invoker,
    }) = args.command
    {
        runtime.block_on(update::apply_child(
            &config,
            config_path,
            &install_dir,
            &journal,
            wait_invoker,
        ))?;

        return Ok(ExitCode::SUCCESS);
    }

    if args.check {
        let options = update::UpdateOptions {
            package: args.package,
            install_dir: args.install_dir,
            ..update::UpdateOptions::default()
        };

        let report = runtime.block_on(update::check(&config, config_path, &options))?;

        if args.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!(
                "Installed: {} ({})",
                report.installed.version,
                if report.installed.git_sha.is_empty() {
                    "-"
                } else {
                    &report.installed.git_sha
                }
            );
            println!("Schema: {}", report.installed.schema_version);

            if let Some(available) = report.available.as_ref() {
                println!(
                    "Available: {} ({}) channel {} runtime {}",
                    available.version,
                    available.git_sha,
                    available.channel,
                    available.compatibility.runtime.join("/")
                );
                println!(
                    "Compatibility: schema {}..={}",
                    available.compatibility.min_database_schema,
                    available.compatibility.max_database_schema
                );

                if !available.changelog.is_empty() {
                    println!("Changelog:");
                    for line in &available.changelog {
                        println!("  - {line}");
                    }
                }
            }

            println!("Status: {}", report.status.to_uppercase());

            if let Some(message) = report.message.as_ref() {
                println!("{message}");
            }
        }

        return Ok(if report.status == "error" {
            ExitCode::from(2)
        } else {
            ExitCode::SUCCESS
        });
    }

    if args.status {
        let report =
            update::status_with_install_dir(&config, config_path, args.install_dir.as_deref())?;

        if args.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!(
                "Installed: {} ({}), schema {}",
                report.installed.version,
                if report.installed.git_sha.is_empty() {
                    "no build sha"
                } else {
                    &report.installed.git_sha
                },
                report.installed.schema_version
            );
            println!("Active jobs: {}", report.active_jobs);

            if let Some(journal) = report.journal.as_ref() {
                println!(
                    "Update journal: {} {} ({}%), stage {}",
                    journal.id,
                    journal.status_as_str(),
                    journal.progress,
                    journal.stage
                );

                if let Some(message) = journal.message.as_ref() {
                    println!("Message: {message}");
                }

                if let Some(error) = journal.error.as_ref() {
                    println!("Error: {error}");
                }

                println!("Log: {}", journal.log_path);
            } else {
                println!("Update journal: none");
            }

            if let Some(rollback) = report.rollback.as_ref() {
                println!(
                    "Rollback available: {} -> {} ({})",
                    rollback.to_version, rollback.from_version, rollback.backup_dir
                );

                if rollback.database_restore_requires_ack {
                    println!(
                        "Rollback restores the pre-update database; confirm with --allow-data-restore"
                    );
                }
            } else {
                println!("Rollback available: no");
            }
        }

        return Ok(ExitCode::SUCCESS);
    }

    if args.rollback {
        if !args.yes {
            eprintln!("Refusing to roll back without confirmation.");
            eprintln!("Review `workspace update --status`, then repeat with --yes.");
            eprintln!("If the database changed after the update, also pass --allow-data-restore.");

            return Ok(ExitCode::from(2));
        }

        let options = update::RollbackOptions {
            backup: None,
            install_dir: args.install_dir,
            allow_database_restore: args.allow_data_restore,
            force: args.force,
        };

        match runtime.block_on(update::start_rollback(
            &config,
            config_path,
            &options,
            "cli",
        )) {
            Ok(report) => {
                println!(
                    "Rollback journal: {} ({})",
                    report.journal.id, report.journal_path
                );

                if report.completed {
                    println!("Rollback finished. Previous release is running.");
                } else {
                    println!(
                        "Rollback started by updater pid {:?}. The server will restart.",
                        report.updater_pid
                    );
                    println!("Track progress: workspace update --status");
                }

                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                eprintln!("Rollback was not started: {error}");

                if error.code() == "incompatible" {
                    eprintln!(
                        "If the database changed after the update, repeat with --allow-data-restore."
                    );
                }

                Ok(ExitCode::from(2))
            }
        }
    } else {
        let options = update::UpdateOptions {
            package: args.package,
            install_dir: args.install_dir,
            force: args.force || args.bootstrap,
            allow_downgrade: args.allow_downgrade,
            drain_timeout: args.drain_timeout.map(Duration::from_secs),
            bootstrap: args.bootstrap,
            wait_for_invoker: true,
        };

        match runtime.block_on(update::start_update(&config, config_path, &options, "cli")) {
            Ok(report) => {
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&report.journal)?);
                } else if report.completed {
                    println!(
                        "Update finished: {} -> {}",
                        report.journal.from_version, report.journal.to_version
                    );
                    println!("Journal: {}", report.journal_path);
                } else {
                    println!(
                        "Updater started (pid {:?}). The server will restart.",
                        report.updater_pid
                    );
                    println!("Journal: {}", report.journal_path);
                    println!("Track progress: workspace update --status");
                }

                Ok(ExitCode::SUCCESS)
            }
            Err(update::UpdateError::NoUpdate) => {
                println!("No update available: the installed release is current.");

                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                eprintln!("Update was not started: {error}");
                Ok(ExitCode::from(2))
            }
        }
    }
}

fn run_release(command: ReleaseCommand) -> AppResult<ExitCode> {
    match command {
        ReleaseCommand::Keygen { out, force } => {
            let public = update::release::keygen(&out, force)?;

            println!("[OK] Key pair written to {}", out.display());
            println!("Public key (base64):");
            println!("{public}");
            println!("Keep the private key outside the repository.");
            println!("Commit the public key as deploy/release-public-key.txt.");
        }

        ReleaseCommand::Sign { manifest, key } => {
            let signature = update::release::sign_manifest(&manifest, &key)?;

            println!("[OK] Signature written to {}", signature.display());
        }

        ReleaseCommand::Package {
            files,
            output,
            release_version,
            key,
            channel,
            git_sha,
            build_time_unix,
            schema_version,
            min_schema,
            max_schema,
            changelog,
        } => {
            let options = update::release::PackageOptions {
                version: release_version,
                git_sha: git_sha.unwrap_or_else(|| GIT_SHA.to_owned()),
                build_time_unix: build_time_unix.unwrap_or_else(unix_now),
                channel,
                schema_version,
                min_schema: min_schema.unwrap_or(schema_version.min(9)),
                max_schema: max_schema.unwrap_or(schema_version),
                changelog: update::release::read_changelog(changelog.as_deref())?,
            };

            if options.min_schema > options.max_schema {
                return Err("min schema must not exceed max schema".into());
            }

            let manifest = update::release::build_package(&files, &output, &options, &key)?;

            // Verify against the key that just signed the package. The
            // deployment trust check (embedded key) is performed separately by
            // the build script and by `release verify`.
            let verified = update::release::verify_with_signing_key(&output, &key)?;
            let summary = update::release::summarize(&output, &verified)?;

            println!("[OK] Release package: {}", output.display());
            println!("Version: {}", manifest.version);
            println!("Git SHA: {}", manifest.git_sha);
            println!("Files: {}", manifest.files.len());
            println!("Manifest SHA-256: {}", summary.manifest_sha256);
            println!("Package SHA-256: {}", summary.package_sha256);
        }

        ReleaseCommand::Verify {
            package,
            public_key,
        } => {
            let verified = update::release::verify(&package, public_key.as_deref())?;

            println!("[OK] Signature and file hashes verified");
            println!("Version: {}", verified.manifest.version);
            println!("Git SHA: {}", verified.manifest.git_sha);
            println!("Schema: {}", verified.manifest.schema_version);
            println!("Files: {}", verified.manifest.files.len());
            println!("Manifest SHA-256: {}", verified.manifest_sha256);
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn instance_id(config: &Config) -> AppResult<String> {
    Ok(
        workspace_server::storage::inspect(&config.storage.data_dir)?
            .map(|info| info.instance_id)
            .unwrap_or_else(|| "unprovisioned-instance".to_owned()),
    )
}

fn print_check(check: &workspace_runtime::StatusCheck) {
    let marker = if !check.required {
        "[INFO]"
    } else if check.ok {
        "[OK]"
    } else {
        "[FAIL]"
    };

    println!("{marker} {}: {}", check.id, check.detail);
}

fn print_runtime_status(status: &workspace_runtime::RuntimeStatus) {
    println!();
    println!(
        "Runtime backend: {}",
        status.backend.as_deref().unwrap_or("not configured")
    );

    for check in &status.checks {
        print_check(check);
    }

    if let Some(storage) = status.storage.as_ref() {
        println!(
            "[INFO] Storage: {}/{} used, {} available",
            storage.used_bytes, storage.size_bytes, storage.available_bytes
        );
    }

    println!();
    println!(
        "Runtime readiness: {}",
        if status.execution_ready {
            "READY (execution)"
        } else if status.infrastructure_ready {
            "READY (infrastructure only)"
        } else {
            "NOT READY"
        }
    );

    for reason in &status.blocking {
        println!("[BLOCKING] {reason}");
    }
}

fn diagnose(config: &Config, quick: bool) -> AppResult<ExitCode> {
    println!("{PRODUCT_NAME} {VERSION}");
    println!("build: {GIT_SHA}");
    println!();
    println!("[OK] Configuration");
    println!(
        "[INFO] Platform: {}/{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    if std::env::consts::OS != "windows" && std::env::consts::OS != "linux" {
        println!("[FAIL] Unsupported operating system");
        return Ok(ExitCode::FAILURE);
    }

    println!("[OK] Supported operating system");

    match workspace_server::storage::inspect(&config.storage.data_dir)? {
        Some(storage) => {
            println!("[OK] SQLite {}", storage.sqlite_version);
            println!("[OK] Database integrity");
            println!("[INFO] Schema version: {}", storage.schema_version);
            println!("[INFO] Journal mode: {}", storage.journal_mode);
            println!("[INFO] Instance ID: {}", storage.instance_id);
        }
        None => println!("[PENDING] Database is not initialized"),
    }

    println!(
        "[{}] ALXPRGS SSO: {}",
        if config.sso.is_some() {
            "OK"
        } else {
            "PENDING"
        },
        if config.sso.is_some() {
            "configured"
        } else {
            "not configured"
        }
    );

    println!(
        "[{}] MCP gateway: {}",
        if config.mcp.is_some() {
            "OK"
        } else {
            "PENDING"
        },
        if config.mcp.is_some() {
            "configured"
        } else {
            "not configured"
        }
    );

    let instance = instance_id(config)?;
    let monitor =
        workspace_runtime::status::RuntimeMonitor::new(config.runtime.as_ref(), &instance);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    runtime.block_on(monitor.warm_up());

    let mut status = runtime.block_on(monitor.status());

    if !quick && status.infrastructure_ready {
        println!();
        println!("Running practical runtime acceptance...");

        match runtime.block_on(monitor.run_acceptance()) {
            Ok(report) => {
                for check in report
                    .checks
                    .iter()
                    .filter(|check| check.required && !check.ok)
                {
                    print_check(check);
                }

                if !report.passed {
                    println!("[FAIL] Practical runtime acceptance failed");
                }

                status = runtime.block_on(monitor.status());
            }
            Err(error) => println!("[FAIL] runtime.acceptance: {error}"),
        }
    }

    print_runtime_status(&status);

    Ok(if status.execution_ready {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    })
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_command() {
        let cli = Cli::try_parse_from(["workspace", "version"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Version)));
    }

    #[test]
    fn parses_version_flag_without_subcommand() {
        let cli = Cli::try_parse_from(["workspace", "--version"]).unwrap();
        assert!(cli.version);
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_update_flags() {
        let cli =
            Cli::try_parse_from(["workspace", "update", "--check", "--package", "C:\\release"])
                .unwrap();

        match cli.command {
            Some(Command::Update(args)) => {
                assert!(args.check);
                assert_eq!(args.package.as_deref(), Some(Path::new("C:\\release")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_apply_command() {
        let cli = Cli::try_parse_from([
            "workspace",
            "update",
            "apply",
            "--journal",
            "journal.json",
            "--install-dir",
            "C:\\install",
            "--wait-invoker",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Some(Command::Update(UpdateArgs {
                command: Some(UpdateCommand::Apply { .. }),
                ..
            }))
        ));
    }

    #[test]
    fn parses_config_check() {
        let cli = Cli::try_parse_from(["workspace", "--config", "example.toml", "config", "check"])
            .unwrap();

        assert_eq!(cli.config, PathBuf::from("example.toml"));
        assert!(matches!(
            cli.command,
            Some(Command::Config {
                command: ConfigCommand::Check
            })
        ));
    }

    #[test]
    fn parses_serve_command() {
        let cli = Cli::try_parse_from(["workspace", "serve"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Serve)));
    }

    #[test]
    fn rejects_unknown_command() {
        assert!(Cli::try_parse_from(["workspace", "unknown"]).is_err());
    }
}

#[cfg(test)]
mod users_cli_tests {
    use super::*;

    #[test]
    fn parses_admin_grant_command() {
        let cli = Cli::try_parse_from([
            "workspace",
            "users",
            "grant",
            "--subject",
            "550e8400-e29b-41d4-a716-446655440000",
            "--role",
            "admin",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Some(Command::Users {
                command: UsersCommand::Grant { .. }
            })
        ));
    }
}

#[cfg(test)]
mod mcp_cli_tests {
    use super::*;

    #[test]
    fn parses_mcp_client_registration() {
        let cli = Cli::try_parse_from([
            "workspace",
            "mcp-clients",
            "register",
            "--name",
            "ChatGPT",
            "--redirect-uri",
            "https://chatgpt.com/connector_platform_oauth_redirect",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Some(Command::McpClients {
                command: McpClientsCommand::Register { .. }
            })
        ));
    }
}
