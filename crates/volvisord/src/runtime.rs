//! Daemon runtime: composition root wiring journal, provider and API.
//!
//! Startup order is safety-relevant and mirrors the crate docs:
//! fail-closed bind/auth check, journal lock + replay, provider
//! construction (which reconciles observed backend state), then the HTTP
//! surface. A second daemon on the same journal directory fails fast on
//! the lock.

use std::sync::Arc;

use volvisor_api::{AppState, SharedState, router};
use volvisor_journal::Journal;
use volvisor_lvm::{LvmProvider, RealRunner};
use volvisor_provider::AdminSurface;
use volvisor_types::ApiError;

use crate::DaemonError;
use crate::config::{Config, ProviderKind};

/// Build the shared server state: open (and replay) the journal, construct
/// the configured provider and reconcile it.
///
/// Opening the journal acquires the single-writer lock; a competing daemon
/// fails here rather than mid-flight. The LVM provider's constructor runs
/// its startup reconciliation (missing LVs are marked `Failed`, foreign LVs
/// are reported, never adopted).
///
/// The concrete provider is constructed first and then coerced into *both*
/// trait-object views: `Arc<dyn VolumeProvider>` for volume operations and
/// `Arc<dyn AdminSurface>` for the privileged `/v2/admin` device-enrollment
/// routes (both providers implement both traits; the two `Arc`s share one
/// underlying object).
///
/// # Errors
/// Returns [`DaemonError`] when the journal cannot be locked/opened or the
/// provider cannot be constructed or reconciled.
pub fn build_state(config: &Config) -> Result<SharedState, DaemonError> {
    let journal = Journal::open(&config.journal_dir).map_err(journal_err("open journal"))?;
    let state = match config.provider {
        ProviderKind::Fake => {
            let provider = Arc::new(volvisor_provider::FakeProvider::new());
            let admin: Arc<dyn AdminSurface> = provider.clone();
            AppState::new(provider, Some(admin), journal, config.admin_token.clone())
        }
        ProviderKind::Lvm => {
            let provider = Arc::new(lvm_provider(config)?);
            let admin: Arc<dyn AdminSurface> = provider.clone();
            AppState::new(provider, Some(admin), journal, config.admin_token.clone())
        }
    };
    Ok(Arc::new(state))
}

/// Construct the native-local LVM provider from validated configuration.
fn lvm_provider(config: &Config) -> Result<LvmProvider, DaemonError> {
    let vg_prefix = config
        .lvm_vg_prefix
        .clone()
        .ok_or_else(|| DaemonError::Config("lvm_vg_prefix missing".to_owned()))?;
    let claim_token = config.device_claim_token.clone().ok_or_else(|| {
        DaemonError::Config(
            "device_claim_token is required for the lvm provider (scoped destructive \
             authorization)"
                .to_owned(),
        )
    })?;
    let state_path = config
        .lvm_state_path
        .clone()
        .unwrap_or_else(|| config.journal_dir.join("lvm-state.json"));
    let sysfs_root = config.sysfs_root.clone().unwrap_or_else(|| "/".into());
    LvmProvider::new(
        Arc::new(RealRunner::default()),
        state_path,
        sysfs_root,
        vg_prefix,
        claim_token,
    )
    .map_err(|e| DaemonError::Config(format!("lvm provider construction failed: {e}")))
}

/// Serve the Volume API v2 surface until a shutdown signal arrives, then
/// drain in-flight requests gracefully.
///
/// # Errors
/// Returns [`DaemonError::Config`] when the fail-closed bind/auth check
/// rejects the configuration (tokenless daemon on a non-loopback bind), and
/// [`DaemonError::Http`] when binding or serving fails.
pub async fn serve(config: Config) -> Result<(), DaemonError> {
    ensure_tokened_or_loopback(&config)?;
    let state = build_state(&config)?;
    let app = router(state, config.max_body_bytes);
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .map_err(|e| DaemonError::Http(format!("bind {}: {e}", config.listen)))?;
    tracing::info!(listen = %config.listen, "volvisor API serving");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| DaemonError::Http(format!("serve: {e}")))
}

/// Fail-closed bind check: a daemon without an `admin_token` may only bind
/// a loopback address.
///
/// Without a token, every mutating endpoint and the whole `/v2/admin`
/// surface reject all requests (fail closed, see the API crate), so a
/// tokenless daemon is only usable as a loopback-bound dev/test instance.
/// Binding a non-loopback address in that state would silently expose a
/// read-only control plane to the network; refuse to start instead.
fn ensure_tokened_or_loopback(config: &Config) -> Result<(), DaemonError> {
    if config.admin_token.is_none() && !config.listen.ip().is_loopback() {
        return Err(DaemonError::Config(format!(
            "admin_token is not configured and the bind address {} is not loopback; \
             refusing to start (fail closed). Configure admin_token, or bind a \
             loopback address for the tokenless dev/test mode",
            config.listen
        )));
    }
    Ok(())
}

/// Resolve on SIGTERM (systemd/K8s) or Ctrl-C.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

/// Map a typed API error to a daemon error with context.
fn journal_err(context: &'static str) -> impl Fn(ApiError) -> DaemonError {
    move |e| DaemonError::Config(format!("{context}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_config(listen: &str, admin_token: Option<&str>) -> Config {
        Config {
            listen: listen.parse().expect("valid listen address"),
            journal_dir: std::env::temp_dir().join("volvisord-runtime-test-journal"),
            provider: ProviderKind::Fake,
            lvm_vg_prefix: None,
            device_claim_token: None,
            lvm_state_path: None,
            sysfs_root: None,
            admin_token: admin_token.map(str::to_owned),
            max_body_bytes: 1 << 20,
        }
    }

    #[test]
    fn tokenless_non_loopback_bind_is_refused() {
        let config = fake_config("0.0.0.0:8787", None);
        let error = ensure_tokened_or_loopback(&config).expect_err("must refuse");
        assert!(
            error.to_string().contains("fail closed"),
            "error must explain the fail-closed behavior: {error}"
        );
    }

    #[test]
    fn tokenless_loopback_bind_is_the_dev_test_mode() {
        for listen in ["127.0.0.1:8787", "[::1]:8787"] {
            let config = fake_config(listen, None);
            assert!(
                ensure_tokened_or_loopback(&config).is_ok(),
                "loopback bind without a token is the documented dev/test mode ({listen})"
            );
        }
    }

    #[test]
    fn tokened_non_loopback_bind_is_allowed() {
        let config = fake_config("0.0.0.0:8787", Some("admin-token"));
        assert!(ensure_tokened_or_loopback(&config).is_ok());
    }

    #[tokio::test]
    async fn serve_refuses_tokenless_non_loopback_before_binding() {
        let config = fake_config("192.0.2.10:8787", None);
        let error = serve(config).await.expect_err("must refuse to serve");
        assert!(matches!(error, DaemonError::Config(_)), "error: {error}");
    }
}
