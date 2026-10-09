//! Witness daemon entry point (`volvisor-witnessd`, P4a plan §3): the
//! third-party writer-authority service. See [`volvisord::witness`]
//! for the runtime's honesty and deployment documentation.

use std::path::PathBuf;

use clap::Parser;
use volvisord::DaemonError;
use volvisord::witness;

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(
    name = "volvisor-witnessd",
    about = "Volvisor writer-authority witness daemon (prototype)"
)]
struct Args {
    /// Path to the TOML configuration file.
    #[arg(short, long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), DaemonError> {
    let args = Args::parse();
    let config = witness::load_config(&args.config)?;
    init_logging();
    tracing::info!(
        listen = %config.listen,
        lease_ttl_secs = config.lease_ttl_secs,
        "volvisor-witnessd starting (prototype; not production supported)"
    );
    witness::serve(config).await
}

/// Initialize structured JSON logging with a conservative default
/// filter (the same convention as `volvisord`; no secret material is
/// ever logged).
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
