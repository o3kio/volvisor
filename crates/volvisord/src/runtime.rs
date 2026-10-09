//! Daemon runtime: composition root wiring journal, provider and API.
//!
//! Startup order is safety-relevant and mirrors the crate docs:
//! journal lock + replay first, then provider construction (which
//! reconciles observed backend state), then the HTTP surface. A second
//! daemon on the same journal directory fails fast on the lock.

use std::sync::Arc;

use volvisor_api::{AppState, SharedState, router};
use volvisor_journal::Journal;
use volvisor_lvm::{LvmProvider, RealRunner};
use volvisor_provider::VolumeProvider;
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
/// # Errors
/// Returns [`DaemonError`] when the journal cannot be locked/opened or the
/// provider cannot be constructed or reconciled.
pub fn build_state(config: &Config) -> Result<SharedState, DaemonError> {
    let journal = Journal::open(&config.journal_dir).map_err(journal_err("open journal"))?;
    let provider: Arc<dyn VolumeProvider> = match config.provider {
        ProviderKind::Fake => Arc::new(volvisor_provider::FakeProvider::new()),
        ProviderKind::Lvm => Arc::new(lvm_provider(config)?),
    };
    Ok(Arc::new(AppState::new(
        provider,
        journal,
        config.admin_token.clone(),
    )))
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
        Arc::new(RealRunner),
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
/// Returns [`DaemonError::Http`] when binding or serving fails.
pub async fn serve(config: Config) -> Result<(), DaemonError> {
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
