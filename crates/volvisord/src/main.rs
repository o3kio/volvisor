//! Daemon entry point.

use std::path::PathBuf;

use clap::Parser;
use volvisord::config::Config;
use volvisord::DaemonError;

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(name = "volvisord", about = "Volvisor volume virtualization daemon")]
struct Args {
    /// Path to the TOML configuration file.
    #[arg(short, long)]
    config: PathBuf,
}

fn main() -> Result<(), DaemonError> {
    let args = Args::parse();
    let config = Config::load(&args.config)?;
    init_logging();
    tracing::info!(
        listen = %config.listen,
        provider = ?config.provider,
        "volvisord starting (prototype; not production supported)"
    );
    // Journal open/replay, provider construction and HTTP serving are wired
    // in by the integration milestone (M6); failing closed until then.
    Err(DaemonError::Config(
        "daemon wiring not yet enabled in this milestone".to_owned(),
    ))
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
