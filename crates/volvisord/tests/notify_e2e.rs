//! P7-B readiness placement, proven through the real `serve()` paths
//! of both daemons (ADR-0009's service model): `READY=1` arrives
//! exactly once startup completes, and a startup that fails — during
//! reconciliation, or at the bind after a completed reconciliation —
//! never reports ready.
//!
//! The e2e rigs elsewhere build the daemon composition by hand
//! (AppState + router + listener); these tests deliberately drive
//! [`volvisord::runtime::serve`] and [`volvisord::witness::serve`]
//! because the notify call lives in that startup sequence.
//! `NOTIFY_SOCKET` is process-global and tests run in parallel: each
//! test holds `NOTIFY_ENV` for its whole body — the daemon reads the
//! variable at its notify point, inside the spawned serve task.
//!
//! The bind ordering is pinned from both sides rather than by a TCP
//! connect on a pre-picked port (a flake, not a proof): a daemon
//! whose listener cannot bind (the port is held) completes
//! reconciliation but never reports ready, and a daemon whose
//! provider reconciliation fails never reaches the bind — only the
//! fully-started daemon's datagram exists. The success case binds an
//! ephemeral port; the datagram's arrival itself proves the bind
//! preceded the notify (the call site is after the bind).

// Integration-test code: invariant assertions may use expect/unwrap.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::ffi::OsStr;
use std::os::unix::net::UnixDatagram;
use std::time::{Duration, Instant};

use volvisor_witness::WitnessConfig;
use volvisord::config::{Config, MigrationConfig, ProviderKind, VmmConfig};
use volvisord::{runtime, witness};

/// Serializes `NOTIFY_SOCKET` access across the parallel tests. The
/// guard is async because it is held across the serve awaits.
static NOTIFY_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Set `NOTIFY_SOCKET` for the current test.
#[allow(unsafe_code)]
fn set_notify_socket(spec: &OsStr) {
    // SAFETY: process-global environment mutation; every caller
    // holds NOTIFY_ENV for the duration, and only these tests (and
    // the daemons they spawn, under the same lock) read
    // NOTIFY_SOCKET.
    unsafe { std::env::set_var("NOTIFY_SOCKET", spec) };
}

/// A notify socket the test owns, with the environment pointing the
/// daemon at it — what a service manager hands the unit.
fn arm_notify_socket() -> (UnixDatagram, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("notify tempdir");
    let path = dir.path().join("notify.sock");
    let socket = UnixDatagram::bind(&path).expect("bind the notify socket");
    set_notify_socket(path.as_os_str());
    (socket, dir)
}

/// Poll the notify socket for one datagram, bounded: readiness
/// either arrives or provably does not. Non-blocking polls with
/// async sleeps — the spawned serve task must keep making progress
/// on the (current-thread) test runtime.
async fn recv_datagram(socket: &UnixDatagram, wait: Duration) -> Option<Vec<u8>> {
    socket
        .set_nonblocking(true)
        .expect("nonblocking notify socket");
    let deadline = Instant::now() + wait;
    loop {
        let mut buf = [0u8; 128];
        match socket.recv_from(&mut buf) {
            Ok((n, _)) => return Some(buf[..n].to_vec()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => unreachable!("notify socket failed: {error}"),
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A minimal valid fake-provider configuration (the e2e convention).
fn fake_config(journal_dir: std::path::PathBuf, listen: std::net::SocketAddr) -> Config {
    Config {
        listen,
        journal_dir,
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
        admin_token: Some("notify-e2e-admin-token".to_owned()),
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
        witness_url: None,
        witness_token: None,
        witness_host_token: None,
        migration: MigrationConfig::default(),
        vmm: VmmConfig::default(),
        witness_renewal_interval_secs: None,
        credentials: volvisord::config::CredentialsConfig::default(),
        tls: volvisord::config::TlsConfig::default(),
        allow_insecure: false,
        admission: volvisord::config::AdmissionConfig::default(),
        journal: volvisord::config::JournalConfig::default(),
    }
}

/// The ceph-provider configuration of the runtime tests: a monitor
/// nothing answers (loopback port 1), so provider construction — the
/// startup reconcile — fails closed, fast. The journal opens first
/// (`build_state` order), so the failure lands inside the reconcile
/// phase, after the journal lock.
fn ceph_config(journal_dir: std::path::PathBuf, listen: std::net::SocketAddr) -> Config {
    Config {
        provider: ProviderKind::Ceph,
        ceph_cluster_fsid: Some("11111111-2222-3333-4444-555555555555".to_owned()),
        ceph_mon_hosts: Some(vec!["127.0.0.1:1".to_owned()]),
        ceph_pool: Some("volvisor".to_owned()),
        ..fake_config(journal_dir, listen)
    }
}

/// A minimal witness configuration bound to `state_dir`.
fn witness_config(state_dir: std::path::PathBuf) -> WitnessConfig {
    WitnessConfig {
        listen: "127.0.0.1:0".parse().expect("listen address"),
        state_dir,
        auth_token: None,
        host_tokens: std::collections::BTreeMap::new(),
        lease_ttl_secs: 60,
        lease_grace_secs: 5,
        suspend_budget_secs: 5,
        tls: volvisor_witness::config::WitnessTlsConfig::default(),
        allow_insecure: false,
    }
}

#[tokio::test]
async fn volvisord_reports_ready_once_reconciled_and_bound() {
    let _env = NOTIFY_ENV.lock().await;
    let journal = tempfile::tempdir().expect("journal dir");
    let (notify_socket, _notify_dir) = arm_notify_socket();
    let config = fake_config(
        journal.path().join("journal"),
        "127.0.0.1:0".parse().expect("listen address"),
    );
    let serve = tokio::spawn(async move { runtime::serve(config).await });
    let datagram = recv_datagram(&notify_socket, Duration::from_secs(30)).await;
    assert_eq!(
        datagram.as_deref(),
        Some(b"READY=1\n".as_slice()),
        "the fully-started daemon sends exactly READY=1"
    );
    serve.abort();
    let _ = serve.await;
}

#[tokio::test]
async fn volvisord_reconciled_but_unbound_never_reports_ready() {
    let _env = NOTIFY_ENV.lock().await;
    // Hold the port: serve's bind fails AFTER `build_state` (journal
    // lock + replay, fake provider reconcile) — reconciled, not
    // serving, therefore not ready.
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("hold the port");
    let port = held.local_addr().expect("held port").port();
    let journal = tempfile::tempdir().expect("journal dir");
    let (notify_socket, _notify_dir) = arm_notify_socket();
    let config = fake_config(
        journal.path().join("journal"),
        format!("127.0.0.1:{port}").parse().expect("listen address"),
    );
    let result = runtime::serve(config).await;
    assert!(result.is_err(), "the bind must fail while the port is held");
    // serve has returned: nothing can send anymore. The bounded
    // absence is the pin — reconciliation alone is not readiness.
    assert_eq!(
        recv_datagram(&notify_socket, Duration::from_secs(1)).await,
        None,
        "no READY without a bound listener"
    );
}

#[tokio::test]
async fn volvisord_failing_reconciliation_never_reports_ready() {
    let _env = NOTIFY_ENV.lock().await;
    let journal = tempfile::tempdir().expect("journal dir");
    let (notify_socket, _notify_dir) = arm_notify_socket();
    let config = ceph_config(
        journal.path().join("journal"),
        "127.0.0.1:0".parse().expect("listen address"),
    );
    let result = runtime::serve(config).await;
    assert!(
        result.is_err(),
        "a daemon whose provider reconcile fails closed never starts"
    );
    assert_eq!(
        recv_datagram(&notify_socket, Duration::from_secs(1)).await,
        None,
        "a daemon that fails reconciliation never reports ready"
    );
}

#[tokio::test]
async fn witnessd_reports_ready_once_open_and_bound() {
    let _env = NOTIFY_ENV.lock().await;
    let state = tempfile::tempdir().expect("witness state dir");
    let (notify_socket, _notify_dir) = arm_notify_socket();
    let config = witness_config(state.path().to_path_buf());
    let serve = tokio::spawn(async move { witness::serve(config).await });
    let datagram = recv_datagram(&notify_socket, Duration::from_secs(30)).await;
    assert_eq!(
        datagram.as_deref(),
        Some(b"READY=1\n".as_slice()),
        "the witness sends exactly READY=1 after journal open and bind"
    );
    serve.abort();
    let _ = serve.await;
}

#[tokio::test]
async fn witnessd_failing_journal_open_never_reports_ready() {
    let _env = NOTIFY_ENV.lock().await;
    let dir = tempfile::tempdir().expect("tempdir");
    // state_dir points at a regular file: the witness journal (and
    // its lock) cannot be opened — the witness startup's first step.
    let not_a_dir = dir.path().join("not-a-dir");
    std::fs::write(&not_a_dir, b"file").expect("write the blocker");
    let (notify_socket, _notify_dir) = arm_notify_socket();
    let config = witness_config(not_a_dir);
    let result = witness::serve(config).await;
    assert!(
        result.is_err(),
        "the witness journal cannot open on a file path"
    );
    assert_eq!(
        recv_datagram(&notify_socket, Duration::from_secs(1)).await,
        None,
        "a witness that fails its journal open never reports ready"
    );
}
