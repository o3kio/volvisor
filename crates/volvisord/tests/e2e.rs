//! End-to-end daemon test: config -> journal -> provider -> router -> real
//! HTTP/1.1 over TCP, using the in-memory fake provider.
//!
//! Mutations authenticate with the configured admin bearer token (fail-closed
//! auth); a tokenless mutation is asserted to be rejected with `401`, and the
//! privileged admin discovery route is exercised over real HTTP.
//!
//! There is deliberately no ceph-provider e2e here: constructing
//! `CephRbdProvider` requires a live external cluster answering its
//! fail-closed startup verification, and none exists in this environment
//! or CI. The daemon's ceph wiring (config-to-error mapping, default
//! state path, fail-closed startup refusal) is covered by the runtime
//! unit tests instead; a real-cluster path would follow the
//! `VOLVISOR_TEST_LVM` env-gate pattern.

// Integration-test code: invariant assertions may use expect/unwrap.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Admin bearer token used by the e2e daemon configuration.
const E2E_ADMIN_TOKEN: &str = "e2e-admin-token";

/// Hand-rolled minimal HTTP/1.1 client (no client dependency by design).
///
/// `authorization` carries an optional bearer token value (the header is
/// omitted entirely when `None`).
async fn http_request(
    stream: &mut tokio::net::TcpStream,
    method: &str,
    path: &str,
    body: Option<&str>,
    authorization: Option<&str>,
) -> (u16, String) {
    let body = body.unwrap_or("");
    let auth_header = authorization
        .map(|token| format!("authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n{auth_header}connection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    let text = String::from_utf8_lossy(&response).into_owned();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status line");
    let body_start = text
        .find("\r\n\r\n")
        .map(|i| text[i + 4..].to_owned())
        .unwrap_or_default();
    (status, body_start)
}

fn e2e_config(dir: &tempfile::TempDir) -> volvisord::Config {
    volvisord::Config {
        listen: "127.0.0.1:0".parse().expect("addr"),
        journal_dir: dir.path().join("journal"),
        provider: volvisord::config::ProviderKind::Fake,
        lvm_vg_prefix: None,
        device_claim_token: None,
        lvm_state_path: None,
        ceph_cluster_fsid: None,
        ceph_mon_hosts: None,
        ceph_pool: None,
        ceph_user: None,
        ceph_state_path: None,
        sysfs_root: None,
        admin_token: Some(E2E_ADMIN_TOKEN.to_owned()),
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
        witness_renewal_interval_secs: None,
    }
}

#[tokio::test]
async fn daemon_end_to_end_fake_provider() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = e2e_config(&dir);
    let state = volvisord::runtime::build_state(&config).expect("build state");
    let app = volvisor_api::router(state, config.max_body_bytes);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server must serve");
    });

    // A tokenless mutation is rejected (fail closed).
    let create_body = r#"{
        "api_version": "volvisor.volume.v2",
        "operation_id": "op-e2e-1",
        "project_id": "tenant-e2e",
        "volume_id": "vol-e2e-1",
        "class": "native-local",
        "size_bytes": 1048576
    }"#;
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (status, body) =
        http_request(&mut stream, "POST", "/v2/volumes", Some(create_body), None).await;
    assert_eq!(status, 401, "tokenless mutation must fail closed: {body}");
    assert!(body.contains("\"UNAUTHORIZED\""), "body: {body}");

    // Create a volume over real HTTP, authenticated as the admin.
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (status, body) = http_request(
        &mut stream,
        "POST",
        "/v2/volumes",
        Some(create_body),
        Some(E2E_ADMIN_TOKEN),
    )
    .await;
    assert_eq!(status, 200, "create body: {body}");
    assert!(body.contains("\"state\":\"Ready\""), "body: {body}");
    assert!(
        body.contains("\"health\":\"Unknown\""),
        "health must be honestly Unknown: {body}"
    );

    // Inspect it (read-only GETs stay open in P0).
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (status, body) =
        http_request(&mut stream, "GET", "/v2/volumes/vol-e2e-1", None, None).await;
    assert_eq!(status, 200, "inspect body: {body}");
    assert!(body.contains("\"volume_id\":\"vol-e2e-1\""), "body: {body}");

    // Idempotent replay over HTTP: same operation, byte-compatible response,
    // served from the journal.
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (status, replay_body) = http_request(
        &mut stream,
        "POST",
        "/v2/volumes",
        Some(create_body),
        Some(E2E_ADMIN_TOKEN),
    )
    .await;
    assert_eq!(status, 200, "replay body: {replay_body}");
    assert_eq!(body, replay_body, "replay must be byte-compatible");

    // The admin discovery route is privileged: tokenless is 401 ...
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (status, body) = http_request(&mut stream, "GET", "/v2/admin/devices", None, None).await;
    assert_eq!(status, 401, "admin discovery requires the token: {body}");
    // ... and the authenticated call lists the fake device.
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (status, body) = http_request(
        &mut stream,
        "GET",
        "/v2/admin/devices",
        None,
        Some(E2E_ADMIN_TOKEN),
    )
    .await;
    assert_eq!(status, 200, "admin discovery body: {body}");
    assert!(body.contains("\"id\":\"dev-fake-1\""), "body: {body}");

    // Healthz is liveness only.
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (status, body) = http_request(&mut stream, "GET", "/healthz", None, None).await;
    assert_eq!(status, 200, "healthz body: {body}");
    assert!(body.contains("ok"), "body: {body}");

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn daemon_journal_lock_fails_fast_for_second_instance() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Loopback bind without a token: the tokenless loopback dev/test mode.
    let config = volvisord::Config {
        listen: "127.0.0.1:0".parse().expect("addr"),
        journal_dir: dir.path().join("journal"),
        provider: volvisord::config::ProviderKind::Fake,
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
        witness_renewal_interval_secs: None,
    };
    // Hold one state (and thus the journal lock) ...
    let first = volvisord::runtime::build_state(&config).expect("first instance");
    // ... a second daemon on the same journal directory must fail fast.
    let second = volvisord::runtime::build_state(&config);
    assert!(second.is_err(), "second daemon must fail on the lock");
    drop(first);
    // After release, a new instance opens cleanly (crash-restart path).
    tokio::time::timeout(Duration::from_secs(10), async {
        volvisord::runtime::build_state(&config).expect("reopen after lock release");
    })
    .await
    .expect("reopen within timeout");
}
