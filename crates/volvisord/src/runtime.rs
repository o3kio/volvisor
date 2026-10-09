//! Daemon runtime: composition root wiring journal, provider and API.
//!
//! Startup order is safety-relevant and mirrors the crate docs:
//! fail-closed bind/auth check, journal lock + replay, provider
//! construction (which reconciles observed backend state), then the HTTP
//! surface. A second daemon on the same journal directory fails fast on
//! the lock.

use std::sync::Arc;

use volvisor_api::{AppState, SharedState, router};
use volvisor_ceph::{CephProviderConfig, CephRbdProvider};
use volvisor_drbd::{DrbdProvider, DrbdProviderConfig};
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
/// fails here rather than mid-flight. The provider constructors run their
/// startup reconciliations (the LVM provider marks missing LVs `Failed`;
/// the Ceph provider refuses to start unless the external cluster answers
/// its fail-closed verification).
///
/// The concrete provider is constructed first and then coerced into the
/// trait-object views: `Arc<dyn VolumeProvider>` for volume operations
/// and, where the provider exposes one, `Arc<dyn AdminSurface>` for the
/// privileged `/v2/admin` device-enrollment routes (both LVM providers
/// implement both traits; the two `Arc`s share one underlying object).
/// The ceph provider is an external-cluster adapter with no local devices
/// to claim, so it contributes no admin surface and admin routes keep
/// their typed 404.
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
        ProviderKind::Ceph => {
            let provider = Arc::new(ceph_provider(config)?);
            AppState::new(provider, None, journal, config.admin_token.clone())
        }
        ProviderKind::Drbd => {
            let provider = Arc::new(drbd_provider(config)?);
            AppState::new(provider, None, journal, config.admin_token.clone())
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

/// Resolve the durable ceph provider state path (defaults to
/// `<journal_dir>/ceph-state.json`, mirroring the LVM provider).
fn ceph_state_path(config: &Config) -> std::path::PathBuf {
    config
        .ceph_state_path
        .clone()
        .unwrap_or_else(|| config.journal_dir.join("ceph-state.json"))
}

/// Construct the external-cluster Ceph RBD provider from validated
/// configuration.
///
/// The provider's constructor performs the fail-closed startup
/// verification (the cluster FSID must match `ceph_cluster_fsid`
/// exactly, the configured pool must exist, and a health query must
/// succeed); any failure refuses daemon startup. The provider's typed
/// error detail (fsid mismatch, missing pool, unqueryable health) is
/// preserved verbatim in the [`DaemonError`] message. Credential
/// resolution stays entirely with the ceph CLI (`--name` only); nothing
/// beyond field names is logged.
fn ceph_provider(config: &Config) -> Result<CephRbdProvider, DaemonError> {
    let cluster_fsid = config.ceph_cluster_fsid.clone().ok_or_else(|| {
        DaemonError::Config("ceph_cluster_fsid is required for the ceph provider".to_owned())
    })?;
    let mon_hosts = config.ceph_mon_hosts.clone().ok_or_else(|| {
        DaemonError::Config("ceph_mon_hosts is required for the ceph provider".to_owned())
    })?;
    let pool = config.ceph_pool.clone().ok_or_else(|| {
        DaemonError::Config("ceph_pool is required for the ceph provider".to_owned())
    })?;
    let provider_config = CephProviderConfig {
        cluster_fsid,
        mon_hosts,
        pool,
        user: config.ceph_user_or_default().to_owned(),
    };
    CephRbdProvider::new(
        Arc::new(RealRunner::default()),
        provider_config,
        ceph_state_path(config),
    )
    .map_err(|e| DaemonError::Config(format!("ceph provider construction failed: {e}")))
}

/// Construct the DRBD nearline provider from validated configuration.
///
/// The provider's constructor performs the fail-closed startup
/// verification (toolchain answers, kernel module present, VG exists,
/// host identity matches, secret readable, config dir usable) and
/// reconciles observed resource state; any failure refuses the daemon.
/// No `AdminSurface`: the nearline VG is operator-designated (like the
/// ceph pool), so admin routes keep their typed 404.
fn drbd_provider(config: &Config) -> Result<DrbdProvider, DaemonError> {
    let provider_config = DrbdProviderConfig {
        vg_name: config.drbd_vg_name.clone().ok_or_else(|| {
            DaemonError::Config("drbd_vg_name is required for the drbd provider".to_owned())
        })?,
        config_dir: config.drbd_config_dir_or_default().clone(),
        node_name: config.drbd_node_name.clone().ok_or_else(|| {
            DaemonError::Config("drbd_node_name is required for the drbd provider".to_owned())
        })?,
        local_address: config.drbd_local_address.clone().ok_or_else(|| {
            DaemonError::Config("drbd_local_address is required for the drbd provider".to_owned())
        })?,
        peer_name: config.drbd_peer_name.clone().ok_or_else(|| {
            DaemonError::Config("drbd_peer_name is required for the drbd provider".to_owned())
        })?,
        peer_address: config.drbd_peer_address.clone().ok_or_else(|| {
            DaemonError::Config("drbd_peer_address is required for the drbd provider".to_owned())
        })?,
        shared_secret_file: config.drbd_shared_secret_file.clone().ok_or_else(|| {
            DaemonError::Config(
                "drbd_shared_secret_file is required for the drbd provider".to_owned(),
            )
        })?,
        port_min: config.drbd_port_min,
        port_max: config.drbd_port_max,
        minor_min: config.drbd_minor_min,
        minor_max: config.drbd_minor_max,
        proc_root: config.drbd_proc_root_or_default().clone(),
    };
    DrbdProvider::new(
        Arc::new(RealRunner::default()),
        provider_config,
        drbd_state_path(config),
    )
    .map_err(|e| DaemonError::Config(format!("drbd provider construction failed: {e}")))
}

/// The durable drbd provider state path: configured, or
/// `<journal_dir>/drbd-state.json`.
fn drbd_state_path(config: &Config) -> std::path::PathBuf {
    config
        .drbd_state_path
        .clone()
        .unwrap_or_else(|| config.journal_dir.join("drbd-state.json"))
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
            ceph_cluster_fsid: None,
            ceph_mon_hosts: None,
            ceph_pool: None,
            ceph_user: None,
            ceph_state_path: None,
            sysfs_root: None,
            admin_token: admin_token.map(str::to_owned),
            drbd_vg_name: None,
            drbd_config_dir: None,
            drbd_node_name: None,
            drbd_local_address: None,
            drbd_peer_name: None,
            drbd_peer_address: None,
            drbd_shared_secret_file: None,
            drbd_port_min: 7100,
            drbd_port_max: 7199,
            drbd_minor_min: 100,
            drbd_minor_max: 999,
            drbd_proc_root: None,
            drbd_state_path: None,
            max_body_bytes: 1 << 20,
        }
    }

    /// A validated ceph-provider configuration pointing at a monitor
    /// address nothing will ever answer (the fail-closed paths are the
    /// point, not a live cluster).
    fn ceph_config(journal_dir: std::path::PathBuf) -> Config {
        Config {
            listen: "127.0.0.1:8787".parse().expect("valid listen address"),
            journal_dir,
            provider: ProviderKind::Ceph,
            lvm_vg_prefix: None,
            device_claim_token: None,
            lvm_state_path: None,
            ceph_cluster_fsid: Some("11111111-2222-3333-4444-555555555555".to_owned()),
            ceph_mon_hosts: Some(vec!["127.0.0.1:1".to_owned()]),
            ceph_pool: Some("volvisor".to_owned()),
            ceph_user: None,
            ceph_state_path: None,
            sysfs_root: None,
            admin_token: None,
            drbd_vg_name: None,
            drbd_config_dir: None,
            drbd_node_name: None,
            drbd_local_address: None,
            drbd_peer_name: None,
            drbd_peer_address: None,
            drbd_shared_secret_file: None,
            drbd_port_min: 7100,
            drbd_port_max: 7199,
            drbd_minor_min: 100,
            drbd_minor_max: 999,
            drbd_proc_root: None,
            drbd_state_path: None,
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

    #[test]
    fn ceph_state_path_defaults_into_the_journal_dir() {
        let config = ceph_config(std::path::PathBuf::from("/j"));
        assert_eq!(
            ceph_state_path(&config),
            std::path::PathBuf::from("/j/ceph-state.json"),
            "unset ceph_state_path defaults to <journal_dir>/ceph-state.json"
        );
        let config = Config {
            ceph_state_path: Some(std::path::PathBuf::from("/custom/ceph-state.json")),
            ..config
        };
        assert_eq!(
            ceph_state_path(&config),
            std::path::PathBuf::from("/custom/ceph-state.json")
        );
    }

    #[test]
    fn ceph_provider_missing_required_field_maps_to_a_config_error() {
        // Config::validate would reject this earlier in a real load; the
        // runtime must still map a missing field to a typed error rather
        // than panic or silently default.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            ceph_pool: None,
            ..ceph_config(dir.path().join("journal"))
        };
        let error = build_state(&config)
            .err()
            .expect("missing ceph_pool must refuse daemon startup");
        assert!(matches!(error, DaemonError::Config(_)), "error: {error}");
        assert!(
            error.to_string().contains("ceph_pool is required"),
            "error names the missing field: {error}"
        );
    }

    #[test]
    fn ceph_provider_construction_fails_closed_without_a_cluster() {
        // A live cluster does not exist in this environment, which is the
        // honest test condition: `CephRbdProvider::new` must refuse to
        // construct (fail-closed startup verification) and the daemon must
        // surface the failure as a typed DaemonError. The ceph binary is
        // absent here, so the very first `ceph fsid` query fails to
        // execute (instantly — no watchdog involvement); on a host with
        // the toolchain the unreachable monitor list produces the typed
        // unqueryable/unhealthy error instead. Either way: no start.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ceph_config(dir.path().join("journal"));
        let error = build_state(&config)
            .err()
            .expect("unverified cluster must refuse daemon startup");
        assert!(matches!(error, DaemonError::Config(_)), "error: {error}");
        let message = error.to_string();
        assert!(
            message.contains("ceph provider construction failed"),
            "error must surface the provider construction failure: {message}"
        );
        assert!(
            message.contains("failed to execute ceph")
                || message.contains("ceph fsid")
                || message.contains("CEPH_CLUSTER_UNHEALTHY"),
            "error must preserve the verification failure detail: {message}"
        );
    }

    fn drbd_config(journal_dir: std::path::PathBuf) -> Config {
        Config {
            drbd_vg_name: Some("volvisor-nearline".to_owned()),
            drbd_node_name: Some("host-a".to_owned()),
            drbd_local_address: Some("10.0.0.1".to_owned()),
            drbd_peer_name: Some("host-b".to_owned()),
            drbd_peer_address: Some("10.0.0.2:7100".to_owned()),
            drbd_shared_secret_file: Some(std::path::PathBuf::from("/nonexistent/secret")),
            ..drbd_base_config(journal_dir)
        }
    }

    fn drbd_base_config(journal_dir: std::path::PathBuf) -> Config {
        Config {
            listen: "127.0.0.1:8787".parse().expect("valid listen address"),
            journal_dir,
            provider: ProviderKind::Drbd,
            lvm_vg_prefix: None,
            device_claim_token: None,
            lvm_state_path: None,
            ceph_cluster_fsid: None,
            ceph_mon_hosts: None,
            ceph_pool: None,
            ceph_user: None,
            ceph_state_path: None,
            sysfs_root: None,
            admin_token: None,
            max_body_bytes: 1 << 20,
            drbd_vg_name: None,
            drbd_config_dir: None,
            drbd_node_name: None,
            drbd_local_address: None,
            drbd_peer_name: None,
            drbd_peer_address: None,
            drbd_shared_secret_file: None,
            drbd_port_min: 7100,
            drbd_port_max: 7199,
            drbd_minor_min: 100,
            drbd_minor_max: 999,
            drbd_proc_root: None,
            drbd_state_path: None,
        }
    }

    #[test]
    fn drbd_state_path_defaults_into_the_journal_dir() {
        let config = drbd_config(std::path::PathBuf::from("/j"));
        assert_eq!(
            drbd_state_path(&config),
            std::path::PathBuf::from("/j/drbd-state.json"),
            "unset drbd_state_path defaults to <journal_dir>/drbd-state.json"
        );
        let config = Config {
            drbd_state_path: Some(std::path::PathBuf::from("/custom/drbd-state.json")),
            ..config
        };
        assert_eq!(
            drbd_state_path(&config),
            std::path::PathBuf::from("/custom/drbd-state.json")
        );
    }

    #[test]
    fn drbd_provider_missing_required_field_maps_to_a_config_error() {
        // Config::validate would reject this earlier in a real load; the
        // runtime must still map a missing field to a typed error rather
        // than panic or silently default.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            drbd_vg_name: None,
            ..drbd_config(dir.path().join("journal"))
        };
        let error = build_state(&config)
            .err()
            .expect("missing drbd_vg_name must refuse daemon startup");
        assert!(matches!(error, DaemonError::Config(_)), "error: {error}");
        assert!(
            error.to_string().contains("drbd_vg_name is required"),
            "error names the missing field: {error}"
        );
    }

    #[test]
    fn drbd_provider_construction_fails_closed_without_drbd() {
        // No DRBD toolchain, kernel module or nearline VG exists in this
        // environment, which is the honest test condition: the provider's
        // fail-closed startup verification must refuse construction and
        // the daemon must surface a typed DaemonError. The drbdadm binary
        // is absent here, so the toolchain probe fails to execute; on a
        // host with the toolchain the missing module/VG/secret produces
        // the corresponding typed error instead. Either way: no start.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            // A missing secret file alone must also refuse startup, even
            // before any toolchain probe could matter.
            drbd_shared_secret_file: Some(std::path::PathBuf::from(
                "/nonexistent/drbd-peer-secret",
            )),
            ..drbd_config(dir.path().join("journal"))
        };
        let error = build_state(&config)
            .err()
            .expect("unverified DRBD host must refuse daemon startup");
        assert!(matches!(error, DaemonError::Config(_)), "error: {error}");
        let message = error.to_string();
        assert!(
            message.contains("drbd provider construction failed"),
            "error must surface the provider construction failure: {message}"
        );
    }
}
