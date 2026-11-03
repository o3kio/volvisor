//! Emits the committed attach trace example for issue #44.
//!
//! This is an evidence generator, not a behavioral test: it installs the
//! daemon's JSON tracing subscriber shape and drives one create followed by
//! one attach through the real router. Run explicitly and capture only the
//! JSON lines:
//!
//! ```text
//! cargo test -p volvisor-api --test trace_attach -- --ignored --nocapture \
//!   2>&1 | grep '^{' > docs/operations/traces/attach.jsonl
//! ```

// Integration-test code: invariant assertions may use expect/unwrap.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use tower::ServiceExt;
use volvisor_api::{ApiConfig, AppState, router_with_config};
use volvisor_journal::Journal;
use volvisor_provider::FakeProvider;
use volvisor_provider::conformance::{fixture_attach_request, fixture_create_request};

const ADMIN_TOKEN: &str = "trace-admin-token";

fn authorized_json(method: Method, uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .expect("request")
}

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
#[ignore = "evidence generator; run explicitly to refresh docs/operations/traces/attach.jsonl"]
async fn emit_attach_trace() {
    let _ = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .try_init();

    let dir = tempfile::tempdir().expect("tempdir");
    let journal = Journal::open(dir.path().join("journal")).expect("journal");
    let state = Arc::new(AppState::new(
        Arc::new(FakeProvider::new()),
        None,
        journal,
        Some(ADMIN_TOKEN.to_owned()),
    ));
    state.mark_ready();
    let app = router_with_config(state, &ApiConfig::default());

    let create = fixture_create_request("vol-trace-attach", 1_073_741_824);
    let (status, body) = send(
        &app,
        authorized_json(
            Method::POST,
            "/v2/volumes",
            &serde_json::to_string(&create).expect("create json"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create: {body}");

    let attach = fixture_attach_request("vol-trace-attach", "att-trace-1", 1);
    let (status, body) = send(
        &app,
        authorized_json(
            Method::POST,
            "/v2/volumes/vol-trace-attach/attach",
            &serde_json::to_string(&attach).expect("attach json"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "attach: {body}");
}
