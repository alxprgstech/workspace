#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use std::{error::Error, path::PathBuf, process::ExitCode};
use tracing::level_filters::LevelFilter;
use workspace_core::{
    PRODUCT_NAME, VERSION,
    config::{Config, LogLevel},
};

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Parser)]
#[command(
    name = "workspace",
    version,
    about = "ALXPRGS Workspace execution platform"
)]
struct Cli {
    #[arg(long, global = true, default_value = "config.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Display product version
    Version,

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
    if matches!(&cli.command, Command::Version) {
        println!("{PRODUCT_NAME} {VERSION}");
        return Ok(ExitCode::SUCCESS);
    }

    let config = Config::load(&cli.config)?;
    init_logging(config.logging.level)?;

    tracing::debug!(
        config = %cli.config.display(),
        "Configuration loaded"
    );

    match cli.command {
        Command::Version => unreachable!(),

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

            runtime.block_on(workspace_server::serve(config))?;

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
                            "{}  enabled={}  name={:?}  redirect={:?}",
                            client.client_id, client.is_enabled, client.name, client.redirect_uri,
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
            workspace_server::secrets::AuthSecrets::from_env()?;

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

                    for directory in ["workspaces", "artifacts", "staging", "logs"] {
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

            if storage.schema_version == workspace_server::storage::SCHEMA_VERSION {
                println!("[OK] Schema version: {}", storage.schema_version);
            } else {
                println!(
                    "[PENDING] Schema version: {} (migrates to {} on server start)",
                    storage.schema_version,
                    workspace_server::storage::SCHEMA_VERSION
                );
            }

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
        assert!(matches!(cli.command, Command::Version));
    }

    #[test]
    fn parses_config_check() {
        let cli = Cli::try_parse_from(["workspace", "--config", "example.toml", "config", "check"])
            .unwrap();

        assert_eq!(cli.config, PathBuf::from("example.toml"));
        assert!(matches!(
            cli.command,
            Command::Config {
                command: ConfigCommand::Check
            }
        ));
    }

    #[test]
    fn parses_serve_command() {
        let cli = Cli::try_parse_from(["workspace", "serve"]).unwrap();
        assert!(matches!(cli.command, Command::Serve));
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
            Command::Users {
                command: UsersCommand::Grant { .. }
            }
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
            Command::McpClients {
                command: McpClientsCommand::Register { .. }
            }
        ));
    }
}
