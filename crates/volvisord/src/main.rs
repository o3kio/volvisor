//! Daemon entry point.

use std::path::PathBuf;

use clap::Parser;
use volvisord::DaemonError;
use volvisord::config::Config;
use volvisord::runtime;

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(
    name = "volvisord",
    about = "Volvisor volume virtualization daemon (prototype)"
)]
struct Args {
    /// Path to the TOML configuration file.
    #[arg(short, long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), DaemonError> {
    let args = Args::parse();
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
