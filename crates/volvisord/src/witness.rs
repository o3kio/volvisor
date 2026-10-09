//! # `volvisor-witnessd` runtime
//!
//! The witness daemon (P4a plan §3): the third-party writer-authority
//! service for nearline-replicated volumes. It owns the durable
//! epoch/lease registry (journal-backed, single-writer via the journal
//! lock) and serves the witness HTTP surface — grant, renew, revoke,
//! register, inspect — with the same conventions as the storage daemon
//! (axum, bearer token, fail-closed authentication, loopback-only
//! without a token).
//!
//! It is deliberately **not** a storage daemon: it holds no volume
//! provider, no device inventory and no tenant path. Its configuration
//! ([`volvisor_witness::WitnessConfig`]) is its own shape for the same
//! reason — only the TOML/validation conventions are shared with
//! `volvisord`.
//!
//! Deployment honesty (plan §3/§7): the witness is a third failure
//! domain **by deployment**; the storage daemon refuses a witness
//! endpoint colocated with either replication end, but volvisor cannot
//! verify physical placement. Witness loss blocks new grants, renewals
//! past the lease deadline (writers self-fence per policy) and
//! failover — never established guest I/O before the deadline. This is
//! the contract's conscious safety/availability tradeoff.

use std::path::Path;
use std::sync::Arc;

use volvisor_witness::WitnessConfig;
use volvisor_witness::registry::WitnessCore;
use volvisor_witness::server::{WitnessServerState, router};

use crate::DaemonError;
use crate::runtime::shutdown_signal;

/// Load and validate a witness configuration from a TOML file.
///
/// # Errors
/// [`DaemonError::Config`] when the file is unreadable, malformed or
/// fails validation (a misconfigured witness never starts half-safe).
pub fn load_config(path: &Path) -> Result<WitnessConfig, DaemonError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| DaemonError::Config(format!("cannot read {}: {e}", path.display())))?;
    let config: WitnessConfig = toml::from_str(&raw)
        .map_err(|e| DaemonError::Config(format!("cannot parse {}: {e}", path.display())))?;
    config
        .validate()
        .map_err(|e| DaemonError::Config(format!("invalid witness configuration: {e}")))?;
    Ok(config)
}

/// Serve the witness surface until a shutdown signal arrives, then
/// drain in-flight requests gracefully.
///
/// Startup order mirrors the storage daemon's: the journal lock is
/// acquired (and replayed) by [`WitnessCore::open`] — a competing
/// witness on the same state directory fails here, not mid-flight.
///
/// # Errors
/// [`DaemonError::Config`] when the journal cannot be opened or
/// replayed; [`DaemonError::Http`] when binding or serving fails.
pub async fn serve(config: WitnessConfig) -> Result<(), DaemonError> {
    let core = WitnessCore::open(&config.state_dir, config.core_config())
        .map_err(|e| DaemonError::Config(format!("witness journal open failed: {e}")))?;
    let token = config.auth_token.clone();
    let state = Arc::new(WitnessServerState::with_clock(
        core,
        token,
        Arc::new(crate::unix_now_secs),
    ));
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .map_err(|e| DaemonError::Http(format!("bind {}: {e}", config.listen)))?;
    tracing::info!(
        listen = %config.listen,
        "volvisor-witnessd serving (prototype; not production supported)"
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| DaemonError::Http(format!("serve: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_dir() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("witness.toml");
        (dir, path)
    }

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write config");
    }

    #[test]
    fn loads_a_valid_witness_configuration() {
        let (_dir, path) = config_dir();
        write(
            &path,
            "listen = \"127.0.0.1:9101\"\nstate_dir = \"/tmp/opencode/witness\"\n\
             auth_token = \"secret\"\nlease_ttl_secs = 60\n",
        );
        let config = load_config(&path).expect("load");
        assert_eq!(config.lease_ttl_secs, 60);
        assert_eq!(
            config.lease_grace_secs,
            volvisor_witness::config::DEFAULT_LEASE_GRACE_SECS
        );
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let (_dir, path) = config_dir();
        write(
            &path,
            "listen = \"127.0.0.1:9101\"\nstate_dir = \"/tmp/opencode/witness\"\nmystery = 1\n",
        );
        assert!(load_config(&path).is_err(), "deny_unknown_fields");
    }

    #[test]
    fn a_half_safe_configuration_never_loads() {
        let (_dir, path) = config_dir();
        // Non-loopback bind without a token: refused, never started.
        write(
            &path,
            "listen = \"10.0.0.3:9101\"\nstate_dir = \"/tmp/opencode/witness\"\n",
        );
        let error = load_config(&path).expect_err("must refuse");
        assert!(
            error.to_string().contains("auth_token"),
            "error names the violated rule: {error}"
        );
        // A zero TTL makes the lease immortal: refused too.
        write(
            &path,
            "listen = \"127.0.0.1:9101\"\nstate_dir = \"/tmp/opencode/witness\"\
             \nlease_ttl_secs = 0\n",
        );
        assert!(load_config(&path).is_err());
    }

    #[test]
    fn an_unreadable_configuration_is_a_typed_error() {
        let (_dir, path) = config_dir();
        let error = load_config(&path).expect_err("missing file");
        assert!(matches!(error, DaemonError::Config(_)));
    }
}
