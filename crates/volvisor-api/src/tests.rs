//! Integration-style tests over the full HTTP surface.
//!
//! These live in-crate (not in `tests/`) so they can reach `AppState`
//! internals the task requires: the journal (for in-flight intent injection,
//! record counts and fault injection), metrics (for provider-call counting)
//! and the `ops` redaction helper. Each test drives the real router via
//! `tower::ServiceExt::oneshot` with a real `Journal` on a `tempfile`
//! directory and the `FakeProvider`.
//!
//! Mutations in these tests authenticate with the bearer token configured
//! by [`setup`] (fail-closed auth, see the `extract` module); tokenless
//! behavior is covered explicitly by dedicated tests.

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

/// Admin bearer token configured by the default test setup.
const TEST_TOKEN: &str = "test-admin-token";

/// The single device exposed by `FakeProvider`'s admin surface (must match
/// `volvisor_provider::admin::FAKE_DEVICE_ID`, which is crate-private).
const FAKE_DEVICE: &str = "dev-fake-1";

fn setup_with_token(
    admin_token: Option<&str>,
) -> (SharedState, Arc<FakeProvider>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new());
    // The fake implements both surfaces; coerce one clone for admin routes.
    let admin: Arc<dyn volvisor_provider::AdminSurface> = provider.clone();
    let state = Arc::new(AppState::new(
        provider.clone(),
        Some(admin),
        journal,
        admin_token.map(str::to_owned),
    ));
    (state, provider, dir)
}

fn setup() -> (SharedState, Arc<FakeProvider>, tempfile::TempDir) {
    setup_with_token(Some(TEST_TOKEN))
}

/// Tokenless state: the fail-closed auth mode (loopback-only dev/test).
fn setup_tokenless() -> (SharedState, Arc<FakeProvider>, tempfile::TempDir) {
    setup_with_token(None)
}

/// State without an admin surface: `/v2/admin` routes must 404.
fn setup_without_admin_surface() -> (SharedState, Arc<FakeProvider>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new());
    let state = Arc::new(AppState::new(
        provider.clone(),
        None,
        journal,
        Some(TEST_TOKEN.to_owned()),
    ));
    (state, provider, dir)
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

/// JSON request authenticating as the configured test admin (mutations in
/// these tests are intended to succeed unless stated otherwise).
fn json_request(method: Method, uri: &str, body: &Value) -> Request<Body> {
    with_bearer(json_request_without_auth(method, uri, body), TEST_TOKEN)
}

/// JSON request without an `Authorization` header (auth-failure tests).
fn json_request_without_auth(method: Method, uri: &str, body: &Value) -> Request<Body> {
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

/// Raw broken-JSON request, authenticated as the test admin (auth must not
/// mask body-level rejections in the malformed-body tests).
fn broken_json_request(uri: &str, body: &'static str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_TOKEN}"))
        .body(Body::from(body))
        .expect("build request")
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

/// A valid admin claim/release request body for the fake device.
fn admin_request(operation_id: &str, authorization_token: &str) -> Value {
    json!({
        "api_version": "volvisor.volume.v2",
        "operation_id": operation_id,
        "authorization_token": authorization_token,
    })
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
// Concurrency (F6): same operation_id from many callers at once
// ---------------------------------------------------------------------------

/// Legal outcomes of N concurrent identical requests for one `operation_id`:
///
/// - `200` with byte-identical bodies — the caller whose intent append won
///   the race executed the mutation (exactly one such execution), and any
///   caller arriving after the outcome was journaled replays the recorded
///   body;
/// - `409 OPERATION_IN_DOUBT` — a caller that resolved the intent while the
///   operation was still in flight (no outcome yet); fail closed, never
///   re-executed.
///
/// Anything else (a second execution, divergent bodies, a success without a
/// 200) is a bug.
#[tokio::test]
async fn concurrent_same_operation_id_creates_exactly_once() {
    const CALLERS: usize = 8;
    let (state, _provider, _dir) = setup();

    let create = serde_json::to_value(fixture_create_request("vol-concurrent", GIB))
        .expect("serialize create fixture");

    let mut handles = Vec::new();
    for _ in 0..CALLERS {
        let app = app(&state);
        let body = create.clone();
        handles.push(tokio::spawn(async move {
            let response = app
                .oneshot(json_request(Method::POST, "/v2/volumes", &body))
                .await
                .expect("router call");
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read response body");
            (
                status,
                String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
            )
        }));
    }

    let mut ok_bodies: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut ok_responses = 0usize;
    let mut in_doubt = 0usize;
    for handle in handles {
        let (status, body) = handle.await.expect("task join");
        assert!(
            status == StatusCode::OK || status == StatusCode::CONFLICT,
            "illegal outcome for concurrent replay: {status} {body}"
        );
        if status == StatusCode::OK {
            ok_responses += 1;
            ok_bodies.insert(body);
        } else {
            assert!(
                body.contains("OPERATION_IN_DOUBT"),
                "409 must be OPERATION_IN_DOUBT, got: {body}"
            );
            in_doubt += 1;
        }
    }
    assert!(
        ok_responses > 0,
        "the winning caller must have received the success response"
    );
    assert_eq!(
        ok_bodies.len(),
        1,
        "all 200 bodies must be byte-identical (fresh or replayed)"
    );
    assert_eq!(
        ok_responses + in_doubt,
        CALLERS,
        "every response must a 200 or an in-doubt 409"
    );

    // Exactly one provider execution happened.
    let metrics = state.metrics.render();
    assert!(metrics.contains("operations_total{kind=\"create_volume\",outcome=\"success\"} 1"));

    // The final state shows exactly one volume.
    let app = app(&state);
    let (status, listed) = send_json(&app, request(Method::GET, "/v2/volumes")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["volumes"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["volumes"][0]["volume_id"], json!("vol-concurrent"));
}

// ---------------------------------------------------------------------------
// Journal fault injection (F6): intent survives, outcome append fails
// ---------------------------------------------------------------------------

/// The intent append succeeds, the outcome append fails: the caller still
/// learns the truthful success, and any retry fails closed as in-doubt.
#[tokio::test]
async fn outcome_append_failure_returns_truthful_success_then_fails_closed() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    {
        let journal = state.journal.lock().expect("journal lock in test");
        journal.inject_append_failures_after(1);
    }

    let create = serde_json::to_value(fixture_create_request("vol-fault-outcome", GIB))
        .expect("serialize create fixture");
    let (status, body) = send(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "mutation succeeded; caller must learn it"
    );
    assert!(body.contains("vol-fault-outcome"));

    // The volume really exists (truthful success).
    let (status, _) = send_json(&app, request(Method::GET, "/v2/volumes/vol-fault-outcome")).await;
    assert_eq!(status, StatusCode::OK);

    // A retry of the same operation cannot know the outcome: fail closed.
    let (status, in_doubt) =
        send_json(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(in_doubt["code"], json!("OPERATION_IN_DOUBT"));
}

/// The intent append itself fails: a typed internal error, and the provider
/// mutation never ran (journal-before-mutate).
#[tokio::test]
async fn intent_append_failure_prevents_the_provider_mutation() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    {
        let journal = state.journal.lock().expect("journal lock in test");
        journal.inject_append_failures_after(0);
    }

    let create = serde_json::to_value(fixture_create_request("vol-fault-intent", GIB))
        .expect("serialize create fixture");
    let (status, error) = send_json(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error["code"], json!("INTERNAL"));

    // No provider mutation happened and nothing was journaled.
    let (status, missing) =
        send_json(&app, request(Method::GET, "/v2/volumes/vol-fault-intent")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["code"], json!("NOT_FOUND"));
    assert_eq!(journal_record_count(&state), 0);
}

// ---------------------------------------------------------------------------
// Auth (F4): fail closed without a configured token
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mutations_require_the_admin_bearer_token_when_configured() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let create = serde_json::to_value(fixture_create_request("vol-auth", GIB))
        .expect("serialize create fixture");
    let bare_request = || json_request_without_auth(Method::POST, "/v2/volumes", &create);

    // Missing token.
    let (status, unauthorized) = send_json(&app, bare_request()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(unauthorized["code"], json!("UNAUTHORIZED"));

    // Wrong token.
    let (status, _) = send_json(&app, with_bearer(bare_request(), "wrong-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Correct token.
    let (status, created) =
        send_json(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
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

/// No token configured: mutations (and the admin surface) fail closed; a
/// presented bearer cannot help because there is nothing to match against.
#[tokio::test]
async fn tokenless_server_fails_closed_for_mutations() {
    let (state, _provider, _dir) = setup_tokenless();
    let app = app(&state);

    let create = serde_json::to_value(fixture_create_request("vol-tokenless", GIB))
        .expect("serialize create fixture");
    let bare_request = || json_request_without_auth(Method::POST, "/v2/volumes", &create);

    // Tokenless mutation: 401 with the fail-closed message.
    let (status, unauthorized) = send_json(&app, bare_request()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(unauthorized["code"], json!("UNAUTHORIZED"));
    assert_eq!(
        unauthorized["message"],
        json!("admin_token is not configured; mutating endpoints are disabled (fail closed)")
    );

    // A bearer token cannot unlock a tokenless server.
    let (status, _) = send_json(&app, with_bearer(bare_request(), "any-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The admin surface fails closed too, GET included.
    let (status, devices) = send_json(&app, request(Method::GET, "/v2/admin/devices")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(devices["code"], json!("UNAUTHORIZED"));

    // Nothing was journaled or executed.
    assert_eq!(journal_record_count(&state), 0);

    // Read-only GETs still work tokenlessly (loopback-only mode).
    let (status, listed) = send_json(&app, request(Method::GET, "/v2/volumes")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["volumes"].as_array().map(Vec::len), Some(0));
    let (status, _) = send_json(&app, request(Method::GET, "/v2/capabilities")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, health) = send_json(&app, request(Method::GET, "/healthz")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health, json!({ "status": "ok" }));
}

// ---------------------------------------------------------------------------
// Journal payload redaction (F5)
// ---------------------------------------------------------------------------

/// Direct unit test of the redaction walk: `key_ref` and
/// `authorization_token` string values are replaced at any depth, including
/// inside arrays; everything else (ids, sizes, non-string values) is intact.
#[test]
fn redaction_replaces_credential_material_at_any_depth() {
    let mut payload = json!({
        "volume_id": "vol-redact",
        "size_bytes": 1024,
        "request": {
            "encryption": {
                "mode": "provider-managed",
                "key_ref": "secret-key-ref-value",
            },
            "authorization_token": "scoped-destructive-token",
            "nested": [
                { "key_ref": "deep-secret", "kept": "not-a-secret" },
            ],
            "key_ref_count": 7,
        },
    });
    ops::redact(&mut payload);

    assert_eq!(payload["volume_id"], json!("vol-redact"));
    assert_eq!(payload["size_bytes"], json!(1024));
    assert_eq!(
        payload["request"]["encryption"]["key_ref"],
        json!("[redacted]")
    );
    assert_eq!(
        payload["request"]["authorization_token"],
        json!("[redacted]")
    );
    assert_eq!(
        payload["request"]["nested"][0]["key_ref"],
        json!("[redacted]")
    );
    assert_eq!(
        payload["request"]["nested"][0]["kept"],
        json!("not-a-secret")
    );
    // Non-string values under a redacted key name are left as they are.
    assert_eq!(payload["request"]["key_ref_count"], json!(7));

    // No secret material survives anywhere in the serialized payload.
    let serialized = payload.to_string();
    assert!(!serialized.contains("secret-key-ref-value"));
    assert!(!serialized.contains("scoped-destructive-token"));
    assert!(!serialized.contains("deep-secret"));
}

/// End-to-end: the secrets from the wire requests never reach the journal
/// file; the redaction marker does.
#[tokio::test]
async fn journaled_intents_redact_secrets() {
    let (state, _provider, dir) = setup();
    let app = app(&state);

    // A create carrying an encryption key reference (the provider rejects
    // encryption, but the intent is journaled *before* execution).
    let mut create = fixture_create_request("vol-redact-e2e", GIB);
    create.encryption = Some(volvisor_types::request::EncryptionRequest {
        mode: "provider-managed".to_owned(),
        key_ref: "super-secret-key-ref".to_owned(),
    });
    let create_body = serde_json::to_value(create).expect("serialize create fixture");
    let (status, _) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes", &create_body),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "fake rejects encryption");

    // An admin claim carrying a scoped destructive-authorization token.
    let claim = admin_request("op-claim-redact", "super-secret-claim-token");
    let (status, _) = send_json(
        &app,
        json_request(
            Method::POST,
            &format!("/v2/admin/devices/{FAKE_DEVICE}/claim"),
            &claim,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Read the journal file back: neither secret may appear; the redaction
    // marker must. (The log is length/CRC framed, so read bytes and search
    // lossily — the JSON payload is embedded verbatim in the frames.)
    let log_path = state
        .journal
        .lock()
        .expect("journal lock in test")
        .log_path()
        .to_path_buf();
    let log_bytes = std::fs::read(&log_path).expect("read journal log");
    let log = String::from_utf8_lossy(&log_bytes);
    assert!(
        !log.contains("super-secret-key-ref"),
        "key_ref leaked into the journal"
    );
    assert!(
        !log.contains("super-secret-claim-token"),
        "authorization_token leaked into the journal"
    );
    assert!(
        log.contains("[redacted]"),
        "redaction marker must be journaled"
    );
    assert!(
        log.contains("vol-redact-e2e"),
        "non-secret identity is preserved"
    );
    drop(dir);
}

// ---------------------------------------------------------------------------
// Admin surface over HTTP (F3/N6)
// ---------------------------------------------------------------------------

/// Full admin flow: discover (unclaimed) -> claim -> typed double-claim
/// error -> release refused while volumes exist -> release after cleanup.
#[tokio::test]
async fn admin_flow_over_http() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);
    let claim_uri = format!("/v2/admin/devices/{FAKE_DEVICE}/claim");
    let release_uri = format!("/v2/admin/devices/{FAKE_DEVICE}/release");

    // Discovery lists one unclaimed device.
    let (status, devices) = send_json(
        &app,
        with_bearer(request(Method::GET, "/v2/admin/devices"), TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let devices = devices["devices"]
        .as_array()
        .expect("devices array")
        .clone();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["id"], json!(FAKE_DEVICE));
    assert_eq!(
        devices[0]["owner_role"],
        Value::Null,
        "device starts unclaimed"
    );

    // Claim: 200 with the resulting Pool.
    let (status, pool) = send_json(
        &app,
        json_request(
            Method::POST,
            &claim_uri,
            &admin_request("op-claim-flow-1", "scoped"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pool["id"], json!("pool-fake-1"));
    assert_eq!(pool["backend_class"], json!("native-local"));

    // Discovery now shows the claimed role.
    let (status, devices) = send_json(
        &app,
        with_bearer(request(Method::GET, "/v2/admin/devices"), TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(devices["devices"][0]["owner_role"], json!("native_pool"));

    // A *different* operation attempting the same claim is a typed error.
    let (status, double) = send_json(
        &app,
        json_request(
            Method::POST,
            &claim_uri,
            &admin_request("op-claim-flow-2", "scoped"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(double["code"], json!("INVALID_STATE"));

    // A volume on the pool blocks release.
    let create = serde_json::to_value(fixture_create_request("vol-on-pool", GIB))
        .expect("serialize create fixture");
    let (status, _) = send_json(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, blocked) = send_json(
        &app,
        json_request(
            Method::POST,
            &release_uri,
            &admin_request("op-release-flow-1", "scoped"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(blocked["code"], json!("INVALID_STATE"));

    // After deleting the volume, release succeeds.
    let delete = serde_json::to_value(fixture_delete_request("vol-on-pool", 1))
        .expect("serialize delete fixture");
    let (status, _) = send_json(
        &app,
        json_request(Method::DELETE, "/v2/volumes/vol-on-pool", &delete),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, released) = send_json(
        &app,
        json_request(
            Method::POST,
            &release_uri,
            &admin_request("op-release-flow-2", "scoped"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(released, json!({ "released": FAKE_DEVICE }));

    // Discovery shows the device unclaimed again.
    let (status, devices) = send_json(
        &app,
        with_bearer(request(Method::GET, "/v2/admin/devices"), TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(devices["devices"][0]["owner_role"], Value::Null);
}

/// Claim idempotency over HTTP: the same operation id replays the recorded
/// Pool response byte-for-byte — including after token rotation, because
/// the request hash excludes the token by design.
#[tokio::test]
async fn admin_claim_replays_from_the_journal_across_token_rotation() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);
    let claim_uri = format!("/v2/admin/devices/{FAKE_DEVICE}/claim");

    let (first_status, first_body) = send(
        &app,
        json_request(
            Method::POST,
            &claim_uri,
            &admin_request("op-claim-replay", "token-before-rotation"),
        ),
    )
    .await;
    assert_eq!(first_status, StatusCode::OK);

    // Identical retry: byte-compatible replay.
    let (status, replay_body) = send(
        &app,
        json_request(
            Method::POST,
            &claim_uri,
            &admin_request("op-claim-replay", "token-before-rotation"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay_body, first_body, "replay must be byte-compatible");

    // Rotated token, same operation: still the recorded outcome (the hash
    // excludes the token; the journaled payload is redacted).
    let (status, rotated_body) = send(
        &app,
        json_request(
            Method::POST,
            &claim_uri,
            &admin_request("op-claim-replay", "token-after-rotation"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rotated_body, first_body,
        "token rotation must not break replay"
    );

    let metrics = state.metrics.render();
    assert!(metrics.contains("operations_total{kind=\"claim_device\",outcome=\"success\"} 1"));
    assert!(metrics.contains("operations_total{kind=\"claim_device\",outcome=\"replayed\"} 2"));
}

/// Admin routes are privileged: the token is required for discovery (GET)
/// and for claim/release, wrong tokens included.
#[tokio::test]
async fn admin_routes_require_the_admin_token() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);
    let claim_uri = format!("/v2/admin/devices/{FAKE_DEVICE}/claim");

    // Tokenless GET.
    let (status, unauthorized) = send_json(&app, request(Method::GET, "/v2/admin/devices")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(unauthorized["code"], json!("UNAUTHORIZED"));

    // Wrong token GET.
    let wrong = with_bearer(request(Method::GET, "/v2/admin/devices"), "wrong-token");
    let (status, _) = send_json(&app, wrong).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Tokenless claim: the auth extractor runs before any body handling, so
    // the rejection is the 401, never a body-level error.
    let (status, _) = send_json(
        &app,
        json_request_without_auth(
            Method::POST,
            &claim_uri,
            &admin_request("op-claim-noauth", "scoped"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Providers without an admin surface serve a typed 404 on the admin routes.
#[tokio::test]
async fn admin_routes_404_without_an_admin_surface() {
    let (state, _provider, _dir) = setup_without_admin_surface();
    let app = app(&state);

    let (status, missing) = send_json(
        &app,
        with_bearer(request(Method::GET, "/v2/admin/devices"), TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["code"], json!("NOT_FOUND"));
    assert_eq!(
        missing["message"],
        json!("admin surface not available for this provider")
    );

    for (method, uri) in [
        (
            Method::POST,
            format!("/v2/admin/devices/{FAKE_DEVICE}/claim"),
        ),
        (
            Method::POST,
            format!("/v2/admin/devices/{FAKE_DEVICE}/release"),
        ),
    ] {
        let (status, missing) = send_json(
            &app,
            json_request(method, &uri, &admin_request("op-admin-404", "scoped")),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(missing["code"], json!("NOT_FOUND"));
        assert_eq!(
            missing["message"],
            json!("admin surface not available for this provider")
        );
    }
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

    // Syntactically broken JSON (authenticated: the body, not the auth, is
    // the rejection under test).
    let (status, body) = send_json(&app, broken_json_request("/v2/volumes", "{not json")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], json!("INVALID_REQUEST"));

    // A missing JSON content type never reaches serde.
    let no_content_type = Request::builder()
        .method(Method::POST)
        .uri("/v2/volumes")
        .header(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {TEST_TOKEN}")).expect("header value"),
        )
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
