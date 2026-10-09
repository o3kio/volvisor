//! Integration-style tests over the full HTTP surface.
//!
//! These live in-crate (not in `tests/`) so they can reach `AppState`
//! internals the task requires: the journal (for in-flight intent injection
//! and record counts) and metrics (for provider-call counting). Each test
//! drives the real router via `tower::ServiceExt::oneshot` with a real
//! `Journal` on a `tempfile` directory and the `FakeProvider`.

// Test bodies favor linear readability over clippy's size heuristics.
#![allow(clippy::too_many_lines)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use volvisor_journal::Journal;
use volvisor_provider::VolumeProvider;
use volvisor_provider::conformance::{
    fixture_attach_request, fixture_create_request, fixture_create_request_in_project,
    fixture_delete_request, fixture_detach_request, fixture_grow_request,
};
use volvisor_provider::fake::FakeProvider;
use volvisor_types::{ApiError, ApiErrorCode};

use crate::{ApiConfig, AppState, SharedState, ops, router};

/// Fixture size used throughout (1 GiB, 512-aligned).
const GIB: u64 = 1 << 30;

fn setup_with_token(
    admin_token: Option<&str>,
) -> (SharedState, Arc<FakeProvider>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new());
    let state = Arc::new(AppState::new(
        provider.clone(),
        journal,
        admin_token.map(str::to_owned),
    ));
    (state, provider, dir)
}

fn setup() -> (SharedState, Arc<FakeProvider>, tempfile::TempDir) {
    setup_with_token(None)
}

fn app(state: &SharedState) -> Router {
    router(state.clone(), ApiConfig::default().max_body_bytes)
}

fn request(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("build request")
}

fn json_request(method: Method, uri: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request")
}

fn with_bearer(mut request: Request<Body>, token: &str) -> Request<Body> {
    let value = HeaderValue::from_str(&format!("Bearer {token}")).expect("header value");
    request.headers_mut().insert(header::AUTHORIZATION, value);
    request
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.clone().oneshot(request).await.expect("router call");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read response body");
    (
        status,
        String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    )
}

async fn send_json(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let (status, body) = send(app, request).await;
    let value = serde_json::from_str(&body).expect("JSON response body");
    (status, value)
}

fn journal_record_count(state: &SharedState) -> u64 {
    state
        .journal
        .lock()
        .expect("journal lock in test")
        .record_count()
}

// ---------------------------------------------------------------------------
// Full lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_lifecycle_over_http() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    // Create: 200, generation 1, Ready, honest Unknown health.
    let create = serde_json::to_value(fixture_create_request("vol-life", GIB))
        .expect("serialize create fixture");
    let (status, created) =
        send_json(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["volume_id"], json!("vol-life"));
    assert_eq!(created["generation"], json!(1));
    assert_eq!(created["state"], json!("Ready"));
    assert_eq!(created["health"], json!("Unknown"));
    assert_eq!(created["backend_health"], json!("Unknown"));
    assert_eq!(created["current_writer"], Value::Null);

    // Inspect round-trips the create response.
    let (status, inspected) = send_json(&app, request(Method::GET, "/v2/volumes/vol-life")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inspected, created);

    // A second volume in another project for the listing checks.
    let other = serde_json::to_value(fixture_create_request_in_project(
        "vol-life-b",
        GIB,
        "project-b",
    ))
    .expect("serialize create fixture");
    let (status, _) = send_json(&app, json_request(Method::POST, "/v2/volumes", &other)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, listed_all) = send_json(&app, request(Method::GET, "/v2/volumes")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed_all["volumes"].as_array().map(Vec::len), Some(2));

    let (status, listed_b) = send_json(
        &app,
        request(Method::GET, "/v2/volumes?project_id=project-b"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let volumes_b = listed_b["volumes"].as_array().expect("volumes array");
    assert_eq!(
        volumes_b.len(),
        1,
        "project filter must not leak other projects"
    );
    assert_eq!(volumes_b[0]["volume_id"], json!("vol-life-b"));

    let (status, listed_a) = send_json(
        &app,
        request(Method::GET, "/v2/volumes?project_id=conformance-project"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed_a["volumes"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed_a["volumes"][0]["volume_id"], json!("vol-life"));

    // Attach (single writer): 200, prepared evidence, host-scoped frontend.
    let attach = serde_json::to_value(fixture_attach_request("vol-life", "att-life-1", 1))
        .expect("serialize attach fixture");
    let (status, attached) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes/vol-life/attach", &attach),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(attached["attachment_id"], json!("att-life-1"));
    assert_eq!(attached["attachment_generation"], json!(1));
    assert_eq!(attached["volume_generation"], json!(2));
    assert_eq!(attached["state"], json!("prepared"));
    assert_eq!(
        attached["frontend"]["virtio_blk"]["host_device_path"],
        json!("/dev/volvisor-fake/vol-life")
    );

    // A second writable attachment is rejected (single-writer, API v2 s3).
    let second_attach = serde_json::to_value(fixture_attach_request("vol-life", "att-life-2", 2))
        .expect("serialize attach fixture");
    let (status, rejected) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes/vol-life/attach", &second_attach),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(rejected["code"], json!("WRITER_ALREADY_ACTIVE"));

    // A stale expected generation is a typed conflict, not success.
    let stale_attach =
        serde_json::to_value(fixture_attach_request("vol-life", "att-life-stale", 999))
            .expect("serialize attach fixture");
    let (status, stale_rejected) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes/vol-life/attach", &stale_attach),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stale_rejected["code"], json!("STALE_GENERATION"));

    // Detach (body carries the attachment_id).
    let mut detach = serde_json::to_value(fixture_detach_request("att-life-1", 1))
        .expect("serialize detach fixture");
    detach["attachment_id"] = json!("att-life-1");
    let (status, detached) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes/vol-life/detach", &detach),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detached["state"], json!("Ready"));
    assert_eq!(detached["generation"], json!(3));
    assert_eq!(detached["current_writer"], Value::Null);
    assert_eq!(detached["attachment_ids"], json!([]));

    // Grow (grow-only, generation-fenced).
    let grow = serde_json::to_value(fixture_grow_request("vol-life", 2 * GIB, 3))
        .expect("serialize grow fixture");
    let (status, grown) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes/vol-life/grow", &grow),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(grown["backing_resized"], json!(true));
    assert_eq!(grown["effective_size_bytes"], json!(2 * GIB));
    assert_eq!(grown["guest_notification_status"], json!("not_applicable"));

    // Delete.
    let delete = serde_json::to_value(fixture_delete_request("vol-life", 4))
        .expect("serialize delete fixture");
    let (status, deleted) = send_json(
        &app,
        json_request(Method::DELETE, "/v2/volumes/vol-life", &delete),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted, json!({ "deleted": "vol-life" }));

    // Inspect after delete: typed NOT_FOUND.
    let (status, missing) = send_json(&app, request(Method::GET, "/v2/volumes/vol-life")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["code"], json!("NOT_FOUND"));
}

// ---------------------------------------------------------------------------
// Idempotency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn idempotent_create_replays_the_recorded_response() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let create = serde_json::to_value(fixture_create_request("vol-replay", GIB))
        .expect("serialize create fixture");

    let (first_status, first_body) =
        send(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(first_status, StatusCode::OK);
    let records_after_first = journal_record_count(&state);

    // Same operation_id + same payload: identical bytes, no re-execution.
    let (second_status, second_body) =
        send(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(second_status, StatusCode::OK);
    assert_eq!(first_body, second_body, "replay must be byte-compatible");

    // The replay added no journal records (intent + outcome only, once).
    assert_eq!(journal_record_count(&state), records_after_first);

    // The provider executed create exactly once; the second response came
    // from the journal replay.
    let metrics = state.metrics.render();
    assert!(metrics.contains("operations_total{kind=\"create_volume\",outcome=\"success\"} 1"));
    assert!(metrics.contains("operations_total{kind=\"create_volume\",outcome=\"replayed\"} 1"));
}

#[tokio::test]
async fn same_operation_id_with_different_payload_is_a_conflict() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let original = serde_json::to_value(fixture_create_request("vol-conflict", GIB))
        .expect("serialize create fixture");
    let (status, _) = send_json(&app, json_request(Method::POST, "/v2/volumes", &original)).await;
    assert_eq!(status, StatusCode::OK);

    // Same operation_id (derived from the fixture's project + volume id),
    // different size.
    let conflicting = serde_json::to_value(fixture_create_request("vol-conflict", 2 * GIB))
        .expect("serialize create fixture");
    let (status, conflict) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes", &conflicting),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["code"], json!("IDEMPOTENCY_CONFLICT"));
}

#[tokio::test]
async fn attach_reused_across_volume_targets_is_a_conflict() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    for volume in ["vol-target-a", "vol-target-b"] {
        let create = serde_json::to_value(fixture_create_request(volume, GIB))
            .expect("serialize create fixture");
        let (status, _) = send_json(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
        assert_eq!(status, StatusCode::OK);
    }

    // The same operation_id and attach body sent to a different volume path
    // is a different immutable wire request: it must fail closed instead of
    // replaying the first volume's response (see the target-folded request
    // hashes in `ops`).
    let attach = serde_json::to_value(fixture_attach_request("vol-target-a", "att-shared", 1))
        .expect("serialize attach fixture");
    let (status, _) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes/vol-target-a/attach", &attach),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, conflict) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes/vol-target-b/attach", &attach),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["code"], json!("IDEMPOTENCY_CONFLICT"));
}

#[tokio::test]
async fn in_flight_intent_fails_closed_as_operation_in_doubt() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    // Inject an intent without an outcome directly into the journal, exactly
    // like a crash between journaling the intent and recording the outcome.
    let create = fixture_create_request("vol-indoubt", GIB);
    {
        let mut journal = state.journal.lock().expect("journal lock in test");
        journal
            .append_intent(
                create.operation_id.clone(),
                ops::create_hash(&create),
                "create_volume",
                serde_json::json!({"injected": true}),
            )
            .expect("append injected intent");
    }

    let body = serde_json::to_value(create).expect("serialize create fixture");
    let (status, in_doubt) =
        send_json(&app, json_request(Method::POST, "/v2/volumes", &body)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(in_doubt["code"], json!("OPERATION_IN_DOUBT"));
}

#[tokio::test]
async fn recorded_failure_outcome_is_replayed_without_reexecution() {
    let (state, provider, _dir) = setup();
    let app = app(&state);

    // Fail the first create attempt (models a crash mid-mutation).
    provider
        .set_next_failure(Some(ApiError::new(
            ApiErrorCode::OperationInDoubt,
            "injected in-flight failure",
        )))
        .await;
    let create = serde_json::to_value(fixture_create_request("vol-failreplay", GIB))
        .expect("serialize create fixture");
    let (first_status, first_body) =
        send(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(first_status, StatusCode::CONFLICT);
    assert!(first_body.contains("OPERATION_IN_DOUBT"));

    // The fault cleared; a replay of the same request must still return the
    // recorded failure (never re-execute), byte- and status-compatible.
    let (second_status, second_body) =
        send(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(second_status, first_status);
    assert_eq!(second_body, first_body);

    let metrics = state.metrics.render();
    // No successful create ever executed (the series is absent entirely, not
    // zero), the injected failure was recorded once, and the retry replayed
    // it from the journal.
    assert!(!metrics.contains("operations_total{kind=\"create_volume\",outcome=\"success\"}"));
    assert!(metrics.contains("operations_total{kind=\"create_volume\",outcome=\"failure\"} 1"));
    assert!(metrics.contains("operations_total{kind=\"create_volume\",outcome=\"replayed\"} 1"));
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mutations_require_the_admin_bearer_token_when_configured() {
    let (state, _provider, _dir) = setup_with_token(Some("secret-token"));
    let app = app(&state);

    let create = serde_json::to_value(fixture_create_request("vol-auth", GIB))
        .expect("serialize create fixture");
    let create_request = || json_request(Method::POST, "/v2/volumes", &create);

    // Missing token.
    let (status, unauthorized) = send_json(&app, create_request()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(unauthorized["code"], json!("UNAUTHORIZED"));

    // Wrong token.
    let (status, _) = send_json(&app, with_bearer(create_request(), "wrong-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Correct token.
    let (status, created) = send_json(&app, with_bearer(create_request(), "secret-token")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["volume_id"], json!("vol-auth"));

    // Read-only routes stay open in P0.
    let (status, listed) = send_json(&app, request(Method::GET, "/v2/volumes")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["volumes"].as_array().map(Vec::len), Some(1));
    let (status, health) = send_json(&app, request(Method::GET, "/healthz")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health, json!({ "status": "ok" }));
}

// ---------------------------------------------------------------------------
// Error shapes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_route_returns_the_contract_not_found_shape() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let (status, body) = send_json(&app, request(Method::GET, "/v2/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], json!("NOT_FOUND"));
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "error body must carry a message: {body}"
    );
}

#[tokio::test]
async fn invalid_volume_id_in_path_is_an_invalid_request() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    // '!' is outside the ID charset.
    let (status, body) = send_json(&app, request(Method::GET, "/v2/volumes/bad!id")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], json!("INVALID_REQUEST"));
}

#[tokio::test]
async fn malformed_bodies_are_typed_invalid_requests() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    // Unknown fields are rejected (fail-closed), never ignored.
    let mut surprise = serde_json::to_value(fixture_create_request("vol-unknown-field", GIB))
        .expect("serialize create fixture");
    surprise["surprise_field"] = json!(true);
    let (status, body) =
        send_json(&app, json_request(Method::POST, "/v2/volumes", &surprise)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], json!("INVALID_REQUEST"));

    // Syntactically broken JSON.
    let broken = Request::builder()
        .method(Method::POST)
        .uri("/v2/volumes")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{not json"))
        .expect("build request");
    let (status, body) = send_json(&app, broken).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], json!("INVALID_REQUEST"));

    // A missing JSON content type never reaches serde.
    let no_content_type = Request::builder()
        .method(Method::POST)
        .uri("/v2/volumes")
        .body(Body::from("{}"))
        .expect("build request");
    let (status, body) = send_json(&app, no_content_type).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], json!("INVALID_REQUEST"));

    // An invalid project_id query value is rejected, not ignored.
    let (status, body) =
        send_json(&app, request(Method::GET, "/v2/volumes?project_id=bad!id")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], json!("INVALID_REQUEST"));
}

// ---------------------------------------------------------------------------
// Capabilities, healthz, metrics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn capabilities_expose_the_provider_advertisement() {
    let (state, provider, _dir) = setup();
    let app = app(&state);

    let (status, body) = send_json(&app, request(Method::GET, "/v2/capabilities")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["provider"], json!(provider.name()));
    let advertised: Vec<String> = provider
        .capabilities()
        .iter()
        .map(|capability| capability.wire_name().to_owned())
        .collect();
    assert_eq!(body["capabilities"], json!(advertised));
    assert_eq!(body["supported_classes"], json!(["native-local"]));
}

#[tokio::test]
async fn healthz_reports_liveness_only() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    for uri in ["/healthz", "/v2/healthz"] {
        let (status, body) = send_json(&app, request(Method::GET, uri)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "status": "ok" }));
    }
}

#[tokio::test]
async fn metrics_serve_prometheus_text() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let (status, _) = send_json(&app, request(Method::GET, "/healthz")).await;
    assert_eq!(status, StatusCode::OK);

    // A param route must be labeled by its pattern, not the concrete path.
    let create = serde_json::to_value(fixture_create_request("vol-metrics", GIB))
        .expect("serialize create fixture");
    let (status, _) = send_json(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send_json(&app, request(Method::GET, "/v2/volumes/vol-metrics")).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(&app, request(Method::GET, "/metrics")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("# TYPE http_requests_total counter"));
    assert!(body.contains("# TYPE operations_total counter"));
    assert!(body.contains("http_requests_total{code=\"200\",route=\"/healthz\"} 1"));
    assert!(body.contains("http_requests_total{code=\"200\",route=\"/v2/volumes/{volume_id}\"} 1"));
    assert!(body.contains("operations_total{kind=\"create_volume\",outcome=\"success\"} 1"));
}
