//! `/readyz` integration test (issue #44 evidence).
//!
//! Uses the real seams the API crate exposes: a real `Journal` on a temporary
//! directory, a constructed provider, the real router, and a real loopback
//! TCP listener as the witness probe target. Dropping the listener is an
//! actual dependency loss; the readiness probe is the same bounded transport
//! connect the daemon wires from `witness_url`.

// Integration-test code: invariant assertions may use expect/unwrap.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;
use volvisor_api::{ApiConfig, AppState, SharedState, router_with_config};
use volvisor_journal::Journal;
use volvisor_provider::FakeProvider;

fn request(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value, String) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    (status, json, text)
}

fn state_with_journal(dir: &tempfile::TempDir, witness: Option<SocketAddr>) -> SharedState {
    let journal = Journal::open(dir.path().join("journal")).expect("journal");
    let state = AppState::new(Arc::new(FakeProvider::new()), None, journal, None);
    let state = match witness {
        Some(address) => state.with_witness_probe(address),
        None => state,
    };
    Arc::new(state)
}

#[tokio::test]
async fn readiness_flips_on_startup_and_real_witness_loss() {
    let dir = tempfile::tempdir().expect("tempdir");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("witness listener");
    let witness_addr = listener.local_addr().expect("witness address");

    let state = state_with_journal(&dir, Some(witness_addr));
    let app = router_with_config(Arc::clone(&state), &ApiConfig::default());

    // Liveness is independent of readiness and stays 200 even before startup.
    let (status, _, body) = send(&app, request(Method::GET, "/healthz")).await;
    assert_eq!(status, StatusCode::OK, "healthz: {body}");
    assert!(body.contains("\"status\":\"ok\""), "healthz: {body}");

    // Startup seam: journal open + provider constructed, but the daemon has
    // not reached its serving path yet.
    let (status, ready, body) = send(&app, request(Method::GET, "/readyz")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "readyz: {body}");
    assert_eq!(ready["status"], serde_json::json!("not_ready"));
    assert_eq!(ready["startup_complete"], serde_json::json!(false));
    assert_eq!(ready["checks"]["journal"], serde_json::json!("open"));
    assert_eq!(
        ready["checks"]["provider"],
        serde_json::json!("pending_startup")
    );
    assert_eq!(ready["checks"]["witness"], serde_json::json!("reachable"));

    // `serve()` marks readiness only after journal replay, provider
    // reconcile and the listener bind.
    state.mark_ready();
    let (status, ready, body) = send(&app, request(Method::GET, "/readyz")).await;
    assert_eq!(status, StatusCode::OK, "readyz: {body}");
    assert_eq!(ready["status"], serde_json::json!("ready"));
    assert_eq!(ready["startup_complete"], serde_json::json!(true));
    assert_eq!(ready["checks"]["provider"], serde_json::json!("reconciled"));
    assert_eq!(ready["checks"]["witness"], serde_json::json!("reachable"));

    // Dependency loss: closing the real listener makes the bounded transport
    // probe fail, and readiness must follow while liveness does not. The
    // probe result is deliberately cached for one second to bound public
    // `/readyz` traffic, so wait past that window before asserting the
    // dependency loss is observed.
    drop(listener);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (status, ready, body) = send(&app, request(Method::GET, "/readyz")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "readyz: {body}");
    assert_eq!(ready["status"], serde_json::json!("not_ready"));
    assert_eq!(ready["checks"]["witness"], serde_json::json!("unreachable"));
    let (status, _, body) = send(&app, request(Method::GET, "/healthz")).await;
    assert_eq!(status, StatusCode::OK, "liveness must not fail: {body}");

    // The same probe is exposed as a gauge on /metrics.
    let (status, _, metrics) = send(&app, request(Method::GET, "/metrics")).await;
    assert_eq!(status, StatusCode::OK, "metrics: {metrics}");
    assert!(
        metrics.contains("witness_reachable 0"),
        "witness gauge must record the failed probe: {metrics}"
    );
}

#[tokio::test]
async fn readiness_without_a_probe_reports_not_probed_and_stays_ready() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = state_with_journal(&dir, None);
    state.mark_ready();
    let app = router_with_config(state, &ApiConfig::default());

    let (status, ready, body) = send(&app, request(Method::GET, "/readyz")).await;
    assert_eq!(status, StatusCode::OK, "readyz: {body}");
    assert_eq!(ready["status"], serde_json::json!("ready"));
    assert_eq!(
        ready["checks"]["witness"],
        serde_json::json!("not_probed"),
        "no configured probe must never invent reachability: {body}"
    );

    // `/v2/readyz` is the same readiness endpoint.
    let (status, ready, body) = send(&app, request(Method::GET, "/v2/readyz")).await;
    assert_eq!(status, StatusCode::OK, "readyz: {body}");
    assert_eq!(ready["status"], serde_json::json!("ready"));
}

#[tokio::test]
async fn probes_answer_repeatedly_with_a_one_permit_concurrency_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = state_with_journal(&dir, None);
    state.mark_ready();
    let config = ApiConfig {
        max_concurrent_requests: 1,
        admission_wait_timeout: std::time::Duration::from_millis(1),
        ..ApiConfig::default()
    };
    let app = router_with_config(state, &config);

    // The probes must answer even with the cap configured to one permit:
    // they never acquire it in the first place.
    for _ in 0..5 {
        let (status, _, body) = send(&app, request(Method::GET, "/healthz")).await;
        assert_eq!(status, StatusCode::OK, "healthz: {body}");
        let (status, _, body) = send(&app, request(Method::GET, "/readyz")).await;
        assert_eq!(status, StatusCode::OK, "readyz: {body}");
    }
}
