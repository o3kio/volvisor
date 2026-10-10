//! Daemon entry point.

use std::path::{Path, PathBuf};

use clap::Parser;
use volvisord::DaemonError;
use volvisord::config::Config;
use volvisord::runtime;
use volvisord::version::VERSION;

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(
    name = "volvisord",
    about = "Volvisor volume virtualization daemon (prototype)",
    version = VERSION
)]
struct Args {
    /// Path to the TOML configuration file.
    #[arg(short, long)]
    config: PathBuf,
    /// Load and validate the configuration, log the concise summary,
    /// and exit without starting the server (the install smoke
    /// surface, P7-A per ADR-0009). An invalid configuration prints
    /// the typed error to stderr and exits non-zero.
    #[arg(long)]
    check_config: bool,
}

#[tokio::main]
async fn main() -> Result<(), DaemonError> {
    let args = Args::parse();
    if args.check_config {
        run_config_check(&args.config);
        return Ok(());
    }
    let config = Config::load(&args.config)?;
    init_logging();
    tracing::info!(
        listen = %config.listen,
        provider = ?config.provider,
        "volvisord starting (prototype; not production supported)"
    );
    // Startup order: journal lock + replay, provider reconcile, then serve.
    // A second daemon on the same journal directory fails fast here.
    runtime::serve(config).await
}

/// The `--check-config` path: load, validate, log the summary, exit 0
/// — or print the typed error to stderr and exit non-zero. The server
/// never starts on this path.
fn run_config_check(path: &Path) {
    let summary = match volvisord::config::check_config(path) {
        Ok(summary) => summary,
        Err(error) => {
            eprintln!("volvisord configuration check failed: {error}");
            std::process::exit(1);
        }
    };
    init_logging();
    tracing::info!(
        summary = summary.as_str(),
        "volvisord configuration check passed"
    );
}

/// Initialize structured JSON logging with a conservative default filter.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
