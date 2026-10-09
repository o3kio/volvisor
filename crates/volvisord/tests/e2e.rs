//! End-to-end daemon test: config -> journal -> provider -> router -> real
//! HTTP/1.1 over TCP, using the in-memory fake provider.

// Integration-test code: invariant assertions may use expect.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Hand-rolled minimal HTTP/1.1 client (no client dependency by design).
async fn http_request(
    stream: &mut tokio::net::TcpStream,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, String) {
    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
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

#[tokio::test]
async fn daemon_end_to_end_fake_provider() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = volvisord::Config {
        listen: "127.0.0.1:0".parse().expect("addr"),
        journal_dir: dir.path().join("journal"),
        provider: volvisord::config::ProviderKind::Fake,
        lvm_vg_prefix: None,
        device_claim_token: None,
        lvm_state_path: None,
        sysfs_root: None,
        admin_token: None,
        max_body_bytes: 1 << 20,
    };
    let state = volvisord::runtime::build_state(&config).expect("build state");
    let app = volvisor_api::router(state, config.max_body_bytes);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server must serve");
    });

    // Create a volume over real HTTP.
    let create_body = r#"{
        "api_version": "volvisor.volume.v2",
        "operation_id": "op-e2e-1",
        "project_id": "tenant-e2e",
        "volume_id": "vol-e2e-1",
        "class": "native-local",
        "size_bytes": 1048576
    }"#;
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (status, body) = http_request(&mut stream, "POST", "/v2/volumes", Some(create_body)).await;
    assert_eq!(status, 200, "create body: {body}");
    assert!(body.contains("\"state\":\"Ready\""), "body: {body}");
    assert!(
        body.contains("\"health\":\"Unknown\""),
        "health must be honestly Unknown: {body}"
    );

    // Inspect it.
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (status, body) = http_request(&mut stream, "GET", "/v2/volumes/vol-e2e-1", None).await;
    assert_eq!(status, 200, "inspect body: {body}");
    assert!(body.contains("\"volume_id\":\"vol-e2e-1\""), "body: {body}");

    // Idempotent replay over HTTP: same operation, byte-compatible response,
    // served from the journal.
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (status, replay_body) =
        http_request(&mut stream, "POST", "/v2/volumes", Some(create_body)).await;
    assert_eq!(status, 200, "replay body: {replay_body}");
    assert_eq!(body, replay_body, "replay must be byte-compatible");

    // Healthz is liveness only.
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (status, body) = http_request(&mut stream, "GET", "/healthz", None).await;
    assert_eq!(status, 200, "healthz body: {body}");
    assert!(body.contains("ok"), "body: {body}");

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn daemon_journal_lock_fails_fast_for_second_instance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = volvisord::Config {
        listen: "127.0.0.1:0".parse().expect("addr"),
        journal_dir: dir.path().join("journal"),
        provider: volvisord::config::ProviderKind::Fake,
        lvm_vg_prefix: None,
        device_claim_token: None,
        lvm_state_path: None,
        sysfs_root: None,
        admin_token: None,
        max_body_bytes: 1 << 20,
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
