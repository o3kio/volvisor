//! Witness daemon entry point (`volvisor-witnessd`, P4a plan §3): the
//! third-party writer-authority service. See [`volvisord::witness`]
//! for the runtime's honesty and deployment documentation.

use std::path::{Path, PathBuf};

use clap::Parser;
use volvisord::DaemonError;
use volvisord::version::VERSION;
use volvisord::witness;

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(
    name = "volvisor-witnessd",
    about = "Volvisor writer-authority witness daemon (prototype)",
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
    let config = witness::load_config(&args.config)?;
    init_logging();
    tracing::info!(
        listen = %config.listen,
        lease_ttl_secs = config.lease_ttl_secs,
        "volvisor-witnessd starting (prototype; not production supported)"
    );
    witness::serve(config).await
}

/// The `--check-config` path: load, validate, log the summary, exit 0
/// — or print the typed error to stderr and exit non-zero. The
/// witness journal is not opened and the server never starts on this
/// path.
fn run_config_check(path: &Path) {
    let summary = match witness::check_config(path) {
        Ok(summary) => summary,
        Err(error) => {
            eprintln!("volvisor-witnessd configuration check failed: {error}");
            std::process::exit(1);
        }
    };
    init_logging();
    tracing::info!(
        summary = summary.as_str(),
        "volvisor-witnessd configuration check passed"
    );
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
