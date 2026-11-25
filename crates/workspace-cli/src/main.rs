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

    /// Diagnose the local installation
    Doctor,

    /// Start the Workspace HTTP server
    Serve,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Validate the configuration file
    Check,
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

        Command::Doctor => {
            println!("{PRODUCT_NAME} {VERSION}");
            println!();
            println!("[OK] Configuration");
            println!(
                "[INFO] Platform: {}/{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            );

            if std::env::consts::OS == "windows" || std::env::consts::OS == "linux" {
                println!("[OK] Supported operating system");
            } else {
                println!("[FAIL] Unsupported operating system");
                return Ok(ExitCode::FAILURE);
            }

            match workspace_server::storage::inspect(&config.storage.data_dir)? {
                Some(storage) => {
                    println!("[OK] SQLite {}", storage.sqlite_version);
                    println!("[OK] Database integrity");
                    println!("[OK] Schema version: {}", storage.schema_version);
                    println!("[OK] Journal mode: {}", storage.journal_mode);
                    println!("[INFO] Instance ID: {}", storage.instance_id);
                }
                None => {
                    println!("[PENDING] Database is not initialized");
                }
            }

            println!("[PENDING] Docker runtime");
            println!("[PENDING] Resource isolation");
            println!("[PENDING] Storage quotas");
            println!("[PENDING] Network isolation");
            println!("[PENDING] ALXPRGS SSO");
            println!("[PENDING] MCP gateway");
            println!();
            println!("Runtime readiness: NOT READY");

            tracing::warn!("Runtime readiness has not been established");

            Ok(ExitCode::from(2))
        }
    }
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
