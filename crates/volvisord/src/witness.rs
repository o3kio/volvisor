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
use crate::notify;
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

/// Check a witness configuration file the way `--check-config` does
/// (P7-A, ADR-0009): load and validate it, and render the concise
/// summary the operator sees. The witness journal is NOT opened —
/// this is the install smoke surface, not a startup.
///
/// # Errors
/// [`DaemonError::Config`] when the file is unreadable, malformed or
/// fails validation; the caller prints the typed error to stderr and
/// exits non-zero.
pub fn check_config(path: &Path) -> Result<String, DaemonError> {
    let config = load_config(path)?;
    let transport = if config.tls.enabled {
        "tls".to_owned()
    } else if config.allow_insecure {
        "plain-http (allow_insecure)".to_owned()
    } else {
        "plain-http loopback".to_owned()
    };
    Ok(format!(
        "config ok: listen {}, state_dir {}, transport {transport}, lease_ttl {}s, \
         grace {}s, suspend_budget {}s, host_credentials {}",
        config.listen,
        config.state_dir.display(),
        config.lease_ttl_secs,
        config.lease_grace_secs,
        config.suspend_budget_secs,
        config.host_tokens.len()
    ))
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
    let host_tokens = config.host_tokens.clone();
    let state = Arc::new(WitnessServerState::with_clock(
        core,
        token,
        host_tokens,
        Arc::new(crate::unix_now_secs),
    ));
    let app = router(state);
    let listener = std::net::TcpListener::bind(config.listen)
        .map_err(|e| DaemonError::Http(format!("bind {}: {e}", config.listen)))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| DaemonError::Http(format!("set_nonblocking {}: {e}", config.listen)))?;
    if config.allow_insecure && !config.tls.enabled && !config.listen.ip().is_loopback() {
        tracing::warn!(
            listen = %config.listen,
            "witness allow_insecure = true: bearer tokens and lease state are visible \
             on the network; use native [tls] or an external TLS terminator"
        );
    }
    tracing::info!(
        listen = %config.listen,
        tls = config.tls.enabled,
        host_credentials = config.host_tokens.len(),
        "volvisor-witnessd serving (protocol v2; prototype; not production supported)"
    );
    // P7-B (ADR-0009's service model): READY=1 only now — the
    // witness journal is open (lock + replay, above) and the
    // listener is bound, mirroring the storage daemon's readiness
    // discipline. Failure paths return before this line; a send
    // failure is logged, never fatal (`notify` module docs).
    notify::ready("volvisor-witnessd");
    if let Some(tls) = witness_tls(&config)? {
        let handle = axum_server::Handle::new();
        let shutdown = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            shutdown.graceful_shutdown(None);
        });
        axum_server::from_tcp_rustls(
            listener,
            axum_server::tls_rustls::RustlsConfig::from_config(tls),
        )
        .map_err(|e| DaemonError::Http(format!("TLS listener: {e}")))?
        .handle(handle)
        .serve(app.into_make_service())
        .await
        .map_err(|e| DaemonError::Http(format!("serve: {e}")))
    } else {
        let listener = tokio::net::TcpListener::from_std(listener)
            .map_err(|e| DaemonError::Http(format!("tokio listener: {e}")))?;
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(|e| DaemonError::Http(format!("serve: {e}")))
    }
}

/// Build the witness's native TLS server configuration, when enabled.
fn witness_tls(
    config: &WitnessConfig,
) -> Result<Option<Arc<volvisor_tls::rustls::ServerConfig>>, DaemonError> {
    if !config.tls.enabled {
        return Ok(None);
    }
    let cert_file = config.tls.cert_file.clone().ok_or_else(|| {
        DaemonError::Config("witness tls.cert_file is required when tls.enabled = true".to_owned())
    })?;
    let key_file = config.tls.key_file.clone().ok_or_else(|| {
        DaemonError::Config("witness tls.key_file is required when tls.enabled = true".to_owned())
    })?;
    volvisor_tls::ServerTls {
        cert_file,
        key_file,
        client_ca_file: config.tls.client_ca_file.clone(),
    }
    .build()
    .map(Some)
    .map_err(|error| DaemonError::Config(format!("witness TLS configuration failed: {error}")))
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

    #[test]
    fn host_credentials_load_from_the_toml_table() {
        let (_dir, path) = config_dir();
        write(
            &path,
            "listen = \"127.0.0.1:9101\"\nstate_dir = \"/tmp/opencode/witness\"\
             \nauth_token = \"shared-read\"\n\n[host_tokens]\nnode-a = \"host-a\"\n",
        );
        let config = load_config(&path).expect("load");
        assert_eq!(
            config.host_tokens.get("node-a").map(String::as_str),
            Some("host-a")
        );
    }
}
