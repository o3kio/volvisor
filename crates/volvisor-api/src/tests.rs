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

/// Setup whose fake provider advertises only `capacity_bytes` of physical
/// capacity: a create larger than that fails deterministically with
/// `NO_SAFE_CAPACITY` (507), for failure-replay status-compatibility tests.
fn setup_with_capacity(capacity_bytes: u64) -> (SharedState, Arc<FakeProvider>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new().with_capacity_bytes(capacity_bytes));
    // The fake implements both surfaces; coerce one clone for admin routes.
    let admin: Arc<dyn volvisor_provider::AdminSurface> = provider.clone();
    let state = Arc::new(AppState::new(
        provider.clone(),
        Some(admin),
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

/// A replayed grow must return the RECORDED outcome — not a fresh
/// validation against current state. Grow is the one endpoint where a
/// pre-journal validation against current state would corrupt replay
/// semantics: after later grows (or after deletion), replaying an earlier
/// grow must still return its recorded 200 byte-identically, never a
/// shrink rejection, stale-generation 409 or 404. The provider's own
/// grow-only check runs only on real executions, under its lock.
#[tokio::test]
async fn grow_replays_the_recorded_response_across_later_grows_and_deletion() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let create = serde_json::to_value(fixture_create_request("vol-grow-replay", GIB))
        .expect("serialize create fixture");
    let (status, _) = send(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::OK);

    let grow = |operation_id: &str, new_size: u64, generation: u64| {
        serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": operation_id,
            "new_size_bytes": new_size,
            "expected_generation": generation,
        })
    };
    let grow_uri = "/v2/volumes/vol-grow-replay/grow";

    let first = grow("op-grow-1", 2 * GIB, 1);
    let (status, first_body) = send(&app, json_request(Method::POST, grow_uri, &first)).await;
    assert_eq!(status, StatusCode::OK, "first grow: {first_body}");

    let (status, _) = send(
        &app,
        json_request(Method::POST, grow_uri, &grow("op-grow-2", 4 * GIB, 2)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Replay the FIRST grow byte-identically: the volume is now 4 GiB, so
    // a validation against current state would reject 2 GiB as a shrink
    // (and the stale generation as a 409) — the journal replay must return
    // the recorded 200 instead.
    let (status, replay_body) = send(&app, json_request(Method::POST, grow_uri, &first)).await;
    assert_eq!(status, StatusCode::OK, "grow replay: {replay_body}");
    assert_eq!(first_body, replay_body, "replay must be byte-compatible");

    // Even after deletion the recorded outcome replays (never a 404).
    let delete = serde_json::to_value(fixture_delete_request("vol-grow-replay", 3))
        .expect("serialize delete fixture");
    let (status, _) = send(
        &app,
        json_request(Method::DELETE, "/v2/volumes/vol-grow-replay", &delete),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, replay_body) = send(&app, json_request(Method::POST, grow_uri, &first)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "post-delete grow replay: {replay_body}"
    );
    assert_eq!(
        first_body, replay_body,
        "post-delete replay must be byte-compatible"
    );

    // Two real executions, two replays, nothing else.
    let metrics = state.metrics.render();
    assert!(metrics.contains("operations_total{kind=\"grow_volume\",outcome=\"success\"} 2"));
    assert!(metrics.contains("operations_total{kind=\"grow_volume\",outcome=\"replayed\"} 2"));
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
// Concurrency (failure flavor): concurrent replay of a FAILED operation
// must be status-compatible, never a 200 wrapping an error body
// ---------------------------------------------------------------------------

/// N concurrent POSTs of the SAME create request whose provider call
/// deterministically fails (`NO_SAFE_CAPACITY` 507: the size exceeds the
/// fake provider's capacity). Every caller — the one that executed the
/// mutation, the ones that lost the intent-append race and re-resolved the
/// recorded outcome inside `append_intent`, and the ones that looked the
/// operation up after the outcome was journaled — must receive the SAME
/// 507 with byte-identical bodies. A 200 wrapping the recorded error body
/// (the round-2 review bug) is a regression here; an in-doubt 409 is legal
/// for callers that resolve the operation inside the executor's
/// intent-to-outcome window.
#[tokio::test]
async fn concurrent_failed_operation_replays_status_compatible() {
    const CALLERS: usize = 8;
    // 512 MiB of capacity: the 1 GiB fixture create can never succeed.
    let (state, _provider, _dir) = setup_with_capacity(GIB / 2);

    let create = serde_json::to_value(fixture_create_request("vol-no-capacity", GIB))
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

    let mut bodies: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut in_doubt = 0usize;
    for handle in handles {
        let (status, body) = handle.await.expect("task join");
        // Legal outcomes mirror the success-flavored concurrent test: the
        // recorded failure status (executor, race-replay or post-outcome
        // replay) or an in-doubt 409 for callers that resolve the operation
        // inside the executor's intent-to-outcome window.
        assert!(
            status == StatusCode::INSUFFICIENT_STORAGE || status == StatusCode::CONFLICT,
            "illegal outcome for a concurrent failed operation: {status} with body: {body}"
        );
        if status == StatusCode::CONFLICT {
            assert!(
                body.contains("OPERATION_IN_DOUBT"),
                "409 must be OPERATION_IN_DOUBT, got: {body}"
            );
            in_doubt += 1;
            continue;
        }
        assert!(
            body.contains("NO_SAFE_CAPACITY"),
            "body must be the recorded error body: {body}"
        );
        bodies.insert(body);
    }
    assert_eq!(
        bodies.len(),
        1,
        "all non-in-doubt responses must be byte-identical (executor, race-replay and \
         post-outcome replay paths)"
    );

    // Exactly one provider execution: one failure, the rest replays or
    // in-doubt resolutions, and never a success.
    let metrics = state.metrics.render();
    assert!(metrics.contains("operations_total{kind=\"create_volume\",outcome=\"failure\"} 1"));
    let replays = CALLERS - 1 - in_doubt;
    assert!(metrics.contains(&format!(
        "operations_total{{kind=\"create_volume\",outcome=\"replayed\"}} {replays}"
    )));
    assert!(!metrics.contains("operations_total{kind=\"create_volume\",outcome=\"success\"}"));

    // One intent + one outcome, nothing else was journaled.
    assert_eq!(journal_record_count(&state), 2);

    // No volume was created.
    let app = app(&state);
    let (status, listed) = send_json(&app, request(Method::GET, "/v2/volumes")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["volumes"].as_array().map(Vec::len), Some(0));
}

/// Sequential failure replay (first-lookup path): the first request fails
/// 507; a retry with the same `operation_id` after the outcome was journaled
/// returns 507 again with the byte-identical body.
#[tokio::test]
async fn sequential_failed_operation_retry_replays_status_and_body() {
    // 512 MiB of capacity: the 1 GiB fixture create can never succeed.
    let (state, _provider, _dir) = setup_with_capacity(GIB / 2);
    let app = app(&state);

    let create = serde_json::to_value(fixture_create_request("vol-no-capacity-seq", GIB))
        .expect("serialize create fixture");

    let (first_status, first_body) =
        send(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(first_status, StatusCode::INSUFFICIENT_STORAGE);
    assert!(first_body.contains("NO_SAFE_CAPACITY"));

    // Retry after the outcome was journaled: same status, same bytes, no
    // re-execution.
    let (second_status, second_body) =
        send(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(second_status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(
        second_body, first_body,
        "failure replay must be status- and byte-compatible"
    );

    let metrics = state.metrics.render();
    assert!(metrics.contains("operations_total{kind=\"create_volume\",outcome=\"failure\"} 1"));
    assert!(metrics.contains("operations_total{kind=\"create_volume\",outcome=\"replayed\"} 1"));
    assert_eq!(journal_record_count(&state), 2);
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

/// An invalid grow ENVELOPE (bad api_version, non-aligned size) is
/// rejected before anything is journaled — the operation_id stays
/// reusable for a corrected retry. Only state-dependent rejections
/// (grow-only, stale generation, missing volume) are journaled, because
/// only those can differ between the first attempt and a replay.
#[tokio::test]
async fn invalid_grow_envelope_is_rejected_without_journaling() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let create = serde_json::to_value(fixture_create_request("vol-grow-envelope", GIB))
        .expect("serialize create fixture");
    let (status, _) = send(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::OK);
    let records_before = journal_record_count(&state);

    let grow_uri = "/v2/volumes/vol-grow-envelope/grow";

    // Bad api_version: envelope rejection, nothing journaled.
    let bad_version = serde_json::json!({
        "api_version": "volvisor.volume.v1",
        "operation_id": "op-grow-envelope",
        "new_size_bytes": 2 * GIB,
        "expected_generation": 1,
    });
    let (status, body) = send(&app, json_request(Method::POST, grow_uri, &bad_version)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(journal_record_count(&state), records_before);

    // Non-512-aligned size: same.
    let unaligned = serde_json::json!({
        "api_version": "volvisor.volume.v2",
        "operation_id": "op-grow-envelope",
        "new_size_bytes": 2 * GIB + 1,
        "expected_generation": 1,
    });
    let (status, body) = send(&app, json_request(Method::POST, grow_uri, &unaligned)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(journal_record_count(&state), records_before);

    // The SAME operation_id now executes the corrected request.
    let corrected = serde_json::json!({
        "api_version": "volvisor.volume.v2",
        "operation_id": "op-grow-envelope",
        "new_size_bytes": 2 * GIB,
        "expected_generation": 1,
    });
    let (status, body) = send(&app, json_request(Method::POST, grow_uri, &corrected)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ---------------------------------------------------------------------------
// Adopt-and-promote (P4a plan §6): the nearline admin route
// ---------------------------------------------------------------------------

/// A scripted adoption surface: returns a canned classification and
/// records the calls (the classification itself is engine behavior,
/// covered by the drbd authority tests; here only the route contract
/// matters — auth, availability, validation and journal idempotency).
struct FakeAdoption {
    calls: std::sync::Mutex<Vec<(volvisor_types::VolumeId, bool)>>,
}

impl FakeAdoption {
    fn new() -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl volvisor_provider::AdoptionSurface for FakeAdoption {
    async fn adopt_volume(
        &self,
        volume_id: &volvisor_types::VolumeId,
        allow_loss: bool,
    ) -> Result<volvisor_types::AdoptVolumeResponse, ApiError> {
        self.calls
            .lock()
            .expect("calls")
            .push((volume_id.clone(), allow_loss));
        Ok(volvisor_types::AdoptVolumeResponse {
            classification: volvisor_types::PromotionClassification::PossibleLoss {
                boundary: volvisor_types::LossBoundary::Unknown,
                authorized: allow_loss,
            },
            evidence: volvisor_types::SafeCurrentEvidence::None,
            volume: None,
        })
    }
}

/// Setup with the adoption surface wired (the drbd daemon wiring, in
/// miniature): the state's provider is still the `FakeProvider`.
fn setup_with_adoption() -> (SharedState, Arc<FakeAdoption>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new());
    let adoption = Arc::new(FakeAdoption::new());
    let state = Arc::new(
        AppState::new(provider, None, journal, Some(TEST_TOKEN.to_owned()))
            .with_adoption(adoption.clone()),
    );
    (state, adoption, dir)
}

fn adopt_body(operation_id: &str, allow_loss: bool) -> Value {
    serde_json::json!({
        "api_version": "volvisor.volume.v2",
        "operation_id": operation_id,
        "allow_loss": allow_loss,
    })
}

#[tokio::test]
async fn adopt_runs_through_the_journal_and_replays() {
    let (state, adoption, _dir) = setup_with_adoption();
    let app = app(&state);
    let uri = "/v2/admin/nearline/vol-adopt-1/adopt";

    let (status, body) = send_json(
        &app,
        json_request(Method::POST, uri, &adopt_body("op-adopt-1", true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["classification"],
        json!({"possible_loss": {"boundary": "unknown", "authorized": true}})
    );
    assert_eq!(body["volume"], Value::Null, "a refusal carries no volume");
    // The surface saw exactly one execution with the authorization.
    assert_eq!(
        *adoption.calls.lock().expect("calls"),
        vec![(
            volvisor_types::VolumeId::new("vol-adopt-1").expect("id"),
            true
        )]
    );

    // The SAME operation id replays the recorded outcome byte-for-byte
    // without a second execution.
    let (status, replayed) = send_json(
        &app,
        json_request(Method::POST, uri, &adopt_body("op-adopt-1", true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(body, replayed);
    assert_eq!(
        adoption.calls.lock().expect("calls").len(),
        1,
        "replays never re-execute"
    );
}

#[tokio::test]
async fn adopt_conflicts_when_the_same_operation_changes_authorization() {
    let (state, _adoption, _dir) = setup_with_adoption();
    let app = app(&state);
    let uri = "/v2/admin/nearline/vol-adopt-2/adopt";

    let (status, _body) = send_json(
        &app,
        json_request(Method::POST, uri, &adopt_body("op-adopt-2", false)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // allow_loss is part of the request hash: a different authorization
    // under the same operation id is an idempotency conflict, never a
    // silent second attempt.
    let (status, body) = send_json(
        &app,
        json_request(Method::POST, uri, &adopt_body("op-adopt-2", true)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], json!("IDEMPOTENCY_CONFLICT"));
}

#[tokio::test]
async fn adopt_requires_the_admin_token() {
    let (state, _adoption, _dir) = setup_with_adoption();
    let app = app(&state);

    let (status, body) = send_json(
        &app,
        json_request_without_auth(
            Method::POST,
            "/v2/admin/nearline/vol-adopt-3/adopt",
            &adopt_body("op-adopt-3", false),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], json!("UNAUTHORIZED"));
}

#[tokio::test]
async fn adopt_validates_the_envelope() {
    let (state, _adoption, _dir) = setup_with_adoption();
    let app = app(&state);
    let uri = "/v2/admin/nearline/vol-adopt-4/adopt";

    // A wrong api_version is a typed invalid request (nothing journaled).
    let mut wrong_version = adopt_body("op-adopt-4", false);
    wrong_version["api_version"] = json!("volvisor.volume.v1");
    let (status, body) = send_json(&app, json_request(Method::POST, uri, &wrong_version)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["code"],
        json!("UNSUPPORTED_CLASS_OR_POLICY"),
        "a foreign api_version is the unsupported-policy refusal, before anything is journaled"
    );
    assert_eq!(journal_record_count(&state), 0, "nothing is journaled");
}

#[tokio::test]
async fn adopt_serves_the_typed_404_without_an_adoption_surface() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let (status, body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/admin/nearline/vol-adopt-5/adopt",
            &adopt_body("op-adopt-5", false),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], json!("NOT_FOUND"));
    assert_eq!(
        body["message"],
        json!("adoption surface not available for this provider")
    );
}

// ---------------------------------------------------------------------------
// Mobility surface (P4b plan §6, stage B2) — consumer routes
// ---------------------------------------------------------------------------

/// The daemon-to-daemon credential of the peer-route tests (distinct
/// from `TEST_TOKEN` by construction — the tests assert the
/// distinction).
const PEER_TOKEN: &str = "test-peer-token";

fn volume_id(raw: &str) -> volvisor_types::VolumeId {
    volvisor_types::VolumeId::new(raw).expect("valid volume id")
}

fn migration_id(raw: &str) -> volvisor_types::MigrationId {
    volvisor_types::MigrationId::new(raw).expect("valid migration id")
}

fn host_id(raw: &str) -> volvisor_types::HostId {
    volvisor_types::HostId::new(raw).expect("valid host id")
}

/// A scripted consumer-facing mobility surface: records every call and
/// answers fixed summaries (the state machine itself is the
/// `volvisor-handoff` crate's business; these tests pin the HTTP
/// surface — statuses, journaling, proof corroboration, auth).
struct FakeMigrationSurface {
    prepares: std::sync::Mutex<Vec<volvisor_handoff::MobilityRequest>>,
    transfers: std::sync::Mutex<Vec<(volvisor_types::MigrationId, Value)>>,
    aborts: std::sync::Mutex<Vec<volvisor_types::MigrationId>>,
}

impl FakeMigrationSurface {
    fn new() -> Self {
        Self {
            prepares: std::sync::Mutex::new(Vec::new()),
            transfers: std::sync::Mutex::new(Vec::new()),
            aborts: std::sync::Mutex::new(Vec::new()),
        }
    }
}

fn migration_summary(state: volvisor_handoff::HandoffState) -> volvisor_handoff::MigrationSummary {
    volvisor_handoff::MigrationSummary {
        state,
        state_history: Vec::new(),
        participants: vec![volvisor_handoff::Participant {
            volume_id: volume_id("vol-m-1"),
            expected_generation: 1,
            resource: "vol-m-1-res".to_owned(),
            minor: 7,
        }],
        in_doubt_detail: None,
    }
}

#[async_trait::async_trait]
impl volvisor_handoff::MigrationSurface for FakeMigrationSurface {
    async fn prepare(
        &self,
        request: volvisor_handoff::MobilityRequest,
    ) -> Result<volvisor_handoff::MigrationSummary, ApiError> {
        self.prepares.lock().expect("prepares").push(request);
        Ok(migration_summary(volvisor_handoff::HandoffState::Prepared))
    }

    async fn transfer(
        &self,
        migration_id: &volvisor_types::MigrationId,
        proof: Value,
    ) -> Result<volvisor_handoff::MigrationSummary, ApiError> {
        self.transfers
            .lock()
            .expect("transfers")
            .push((migration_id.clone(), proof));
        Ok(migration_summary(
            volvisor_handoff::HandoffState::BarrierDurable,
        ))
    }

    fn observe(
        &self,
        migration_id: &volvisor_types::MigrationId,
    ) -> Result<Option<volvisor_handoff::MigrationSummary>, ApiError> {
        if migration_id.as_str() == "mig-unknown" {
            return Ok(None);
        }
        Ok(Some(migration_summary(
            volvisor_handoff::HandoffState::Precopy,
        )))
    }

    async fn abort(
        &self,
        migration_id: &volvisor_types::MigrationId,
    ) -> Result<volvisor_handoff::MigrationSummary, ApiError> {
        self.aborts
            .lock()
            .expect("aborts")
            .push(migration_id.clone());
        Ok(migration_summary(volvisor_handoff::HandoffState::Aborted {
            reason: "consumer abort".to_owned(),
            at: 1,
        }))
    }
}

/// Setup with the consumer-facing mobility surface wired.
fn setup_with_migration() -> (SharedState, Arc<FakeMigrationSurface>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new());
    let migration = Arc::new(FakeMigrationSurface::new());
    let state = Arc::new(
        AppState::new(provider, None, journal, Some(TEST_TOKEN.to_owned()))
            .with_migration(migration.clone()),
    );
    (state, migration, dir)
}

fn mobility_request(migration: &str, volumes: &[&str]) -> Value {
    json!({
        "migration_id": migration,
        "vm_id": "vm-m-1",
        "target_host": "dst-host",
        "volume_ids": volumes,
        "expected_generations": vec![1; volumes.len()],
    })
}

#[tokio::test]
async fn mobility_routes_serve_the_typed_404_without_the_surfaces() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    for (method, uri, body) in [
        (
            Method::POST,
            "/v2/migrations",
            mobility_request("mig-1", &["vol-1"]),
        ),
        (
            Method::POST,
            "/v2/migrations/mig-1/transfer",
            json!({"vm_paused_and_io_drained_proof": {}}),
        ),
        (Method::GET, "/v2/migrations/mig-1", json!({})),
        (Method::POST, "/v2/migrations/mig-1/abort", json!({})),
        (
            Method::POST,
            "/v2/vms/vm-1/check-mobility",
            json!({"target_host": "dst-host"}),
        ),
    ] {
        let (status, not_enabled) = send_json(&app, json_request(method.clone(), uri, &body)).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {uri}: {not_enabled}"
        );
        assert_eq!(not_enabled["code"], json!("NOT_FOUND"), "{method} {uri}");
    }
}

#[tokio::test]
async fn prepare_migration_answers_201_and_replays_byte_compatible() {
    let (state, migration, _dir) = setup_with_migration();
    let app = app(&state);

    let (status, body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/migrations",
            &mobility_request("mig-p1", &["vol-1"]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], json!("prepared"));

    // The same request replays the recorded outcome byte-for-byte —
    // status included (201, not 200) — without a second execution.
    let (status, replayed) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/migrations",
            &mobility_request("mig-p1", &["vol-1"]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{replayed}");
    assert_eq!(body, replayed);
    assert_eq!(
        migration.prepares.lock().expect("prepares").len(),
        1,
        "replays never re-execute"
    );
}

#[tokio::test]
async fn prepare_migration_conflicts_when_the_same_migration_changes_content() {
    let (state, _migration, _dir) = setup_with_migration();
    let app = app(&state);

    let (status, _body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/migrations",
            &mobility_request("mig-p2", &["vol-1"]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // A different participant set under the same migration id (the
    // derived operation id keys on it) is the typed conflict, never a
    // silent second preparation.
    let (status, body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/migrations",
            &mobility_request("mig-p2", &["vol-1", "vol-2"]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], json!("IDEMPOTENCY_CONFLICT"));
}

#[tokio::test]
async fn transfer_records_the_proof_as_corroboration_and_answers_202() {
    let (state, migration, _dir) = setup_with_migration();
    let app = app(&state);
    let uri = "/v2/migrations/mig-t1/transfer";

    let proof = json!({"kind": "vm_paused", "observed_by": "consumer"});
    let (status, body) = send_json(
        &app,
        json_request(
            Method::POST,
            uri,
            &json!({"vm_paused_and_io_drained_proof": proof}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["state"], json!("barrier_durable"));

    // The proof was recorded verbatim as corroboration.
    assert_eq!(
        migration.transfers.lock().expect("transfers").len(),
        1,
        "the surface saw exactly one transfer"
    );
    let (recorded_id, recorded_proof) = migration.transfers.lock().expect("transfers")[0].clone();
    assert_eq!(recorded_id, migration_id("mig-t1"));
    assert_eq!(recorded_proof, proof);

    // The replay is 202 and byte-identical.
    let (status, replayed) = send_json(
        &app,
        json_request(
            Method::POST,
            uri,
            &json!({"vm_paused_and_io_drained_proof": proof}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{replayed}");
    assert_eq!(body, replayed);
    assert_eq!(migration.transfers.lock().expect("transfers").len(), 1);
}

#[tokio::test]
async fn transfer_in_flight_intent_fails_closed_as_operation_in_doubt() {
    let (state, _migration, _dir) = setup_with_migration();
    let app = app(&state);

    // The derived operation id and the canonical request hash the
    // handler computes — injected as an intent without an outcome,
    // exactly like a crash between the intent and the outcome record.
    let migration = migration_id("mig-t2");
    let operation_id = ops::mobility_operation_id(&migration, "transfer").expect("op id");
    let hash_body = json!({
        "migration_id": "mig-t2",
        "vm_paused_and_io_drained_proof": {"kind": "vm_paused"},
    });
    {
        let mut journal = state.journal.lock().expect("journal lock in test");
        journal
            .append_intent(
                operation_id,
                ops::mobility_request_hash("transfer", &hash_body),
                ops::OP_MIGRATION_TRANSFER,
                serde_json::json!({"injected": true}),
            )
            .expect("append injected intent");
    }

    // The drive task may be mid-flight right now: the retry fails
    // closed, never starts a second drive.
    let (status, in_doubt) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/migrations/mig-t2/transfer",
            &json!({"vm_paused_and_io_drained_proof": {"kind": "vm_paused"}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{in_doubt}");
    assert_eq!(in_doubt["code"], json!("OPERATION_IN_DOUBT"));
}

#[tokio::test]
async fn observe_migration_serves_the_summary_or_the_typed_404() {
    let (state, _migration, _dir) = setup_with_migration();
    let app = app(&state);

    let (status, body) = send_json(
        &app,
        json_request(Method::GET, "/v2/migrations/mig-o1", &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], json!("precopy"));
    assert!(body["participants"].is_array());

    let (status, missing) = send_json(
        &app,
        json_request(Method::GET, "/v2/migrations/mig-unknown", &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    assert_eq!(missing["code"], json!("NOT_FOUND"));
}

#[tokio::test]
async fn abort_migration_journals_and_replays() {
    let (state, migration, _dir) = setup_with_migration();
    let app = app(&state);
    let uri = "/v2/migrations/mig-a1/abort";

    let (status, body) = send_json(&app, json_request(Method::POST, uri, &json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["state"],
        json!({"aborted": {"reason": "consumer abort", "at": 1}})
    );

    let (status, replayed) = send_json(&app, json_request(Method::POST, uri, &json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, replayed);
    assert_eq!(migration.aborts.lock().expect("aborts").len(), 1);
}

#[tokio::test]
async fn mobility_mutations_require_the_admin_bearer_token() {
    let (state, _migration, _dir) = setup_with_migration();
    let app = app(&state);

    let (status, body) = send_json(
        &app,
        json_request_without_auth(
            Method::POST,
            "/v2/migrations",
            &mobility_request("mig-auth", &["vol-1"]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], json!("UNAUTHORIZED"));

    let (status, _body) = send_json(
        &app,
        json_request_without_auth(
            Method::POST,
            "/v2/vms/vm-1/check-mobility",
            &json!({"target_host": "dst-host"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// A scripted handoff surface: records promote calls (with the full
/// derived attach request) and eligibility queries; everything else
/// refuses typed (the cutover acts are the `volvisor-drbd` and daemon
/// tests' business).
struct FakeHandoffSurface {
    promotes: std::sync::Mutex<
        Vec<(
            volvisor_types::VolumeId,
            volvisor_types::MigrationId,
            volvisor_types::request::AttachVolumeRequest,
        )>,
    >,
    eligibility: std::sync::Mutex<Vec<String>>,
    cleared: std::sync::Mutex<Vec<(volvisor_types::VolumeId, bool)>>,
    /// The volumes whose target replica this surface verifies `Ok`
    /// (a volume this host tracks models an established replica);
    /// everything else is the typed no-replica refusal — the
    /// unprepared participant of row 12.
    targets: std::sync::Mutex<Vec<volvisor_types::VolumeId>>,
    /// Every replica verification the surface saw (assertion input).
    target_checks: std::sync::Mutex<Vec<volvisor_types::VolumeId>>,
}

impl FakeHandoffSurface {
    fn new() -> Self {
        Self {
            promotes: std::sync::Mutex::new(Vec::new()),
            eligibility: std::sync::Mutex::new(Vec::new()),
            cleared: std::sync::Mutex::new(Vec::new()),
            targets: std::sync::Mutex::new(Vec::new()),
            target_checks: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Register a volume as holding an established target replica.
    fn add_target(&self, volume_id: &volvisor_types::VolumeId) {
        self.targets
            .lock()
            .expect("targets")
            .push(volume_id.clone());
    }
}

/// A minimal honest inspect answer for the scripted clear-cut-marker
/// (the full field truth is the provider tests' business; the route
/// tests pin the journal behavior and the replay).
fn cleared_marker_inspect(
    volume_id: &volvisor_types::VolumeId,
) -> volvisor_types::InspectVolumeResponse {
    volvisor_types::InspectVolumeResponse {
        volume_id: volume_id.clone(),
        backend_class: volvisor_types::domain::VolumeClass::NativeLocal,
        project_id: volvisor_types::ProjectId::new("seed-project").expect("valid project id"),
        generation: 1,
        state: volvisor_types::VolumeLifecycle::Ready,
        provisioned_bytes: 1024,
        allocated_bytes: 1024,
        effective_protection: volvisor_types::EffectiveProtection::default(),
        failure_domain: volvisor_types::FailureDomain::Host,
        health: volvisor_types::Health::Unknown,
        attachment_ids: Vec::new(),
        current_writer: None,
        backend_health: volvisor_types::Health::Unknown,
        evidence_status: volvisor_types::domain::EvidenceStatus::default(),
        authority: None,
    }
}

#[async_trait::async_trait]
impl volvisor_provider::HandoffSurface for FakeHandoffSurface {
    async fn handoff_eligibility(
        &self,
        vm_id: &str,
    ) -> Result<volvisor_provider::EligibilityReport, ApiError> {
        self.eligibility
            .lock()
            .expect("eligibility")
            .push(vm_id.to_owned());
        Ok(volvisor_provider::EligibilityReport {
            vm_id: vm_id.to_owned(),
            eligible: true,
            participants: vec![volvisor_provider::EligibilityParticipant {
                volume_id: volume_id("vol-e-1"),
                eligible: true,
                reasons: Vec::new(),
            }],
        })
    }

    async fn quiesce_for_barrier(
        &self,
        _volume_id: &volvisor_types::VolumeId,
        _migration_id: &volvisor_types::MigrationId,
    ) -> Result<volvisor_provider::QuiesceProof, ApiError> {
        Err(ApiError::not_found("not scripted"))
    }

    async fn replica_caught_up(
        &self,
        _volume_id: &volvisor_types::VolumeId,
    ) -> Result<(), ApiError> {
        Err(ApiError::not_found("not scripted"))
    }

    async fn track_sync(
        &self,
        _volume_id: &volvisor_types::VolumeId,
    ) -> Result<volvisor_provider::SyncProof, ApiError> {
        Err(ApiError::not_found("not scripted"))
    }

    async fn release_source(
        &self,
        _volume_id: &volvisor_types::VolumeId,
        _migration_id: &volvisor_types::MigrationId,
    ) -> Result<(), ApiError> {
        Err(ApiError::not_found("not scripted"))
    }

    async fn abort_prepare(
        &self,
        _volume_id: &volvisor_types::VolumeId,
        _migration_id: &volvisor_types::MigrationId,
    ) -> Result<(), ApiError> {
        Err(ApiError::not_found("not scripted"))
    }

    async fn clear_cut_marker(
        &self,
        volume_id: &volvisor_types::VolumeId,
        proof: Option<&volvisor_types::FencingProof>,
    ) -> Result<volvisor_types::InspectVolumeResponse, ApiError> {
        self.cleared
            .lock()
            .expect("cleared")
            .push((volume_id.clone(), proof.is_some()));
        Ok(cleared_marker_inspect(volume_id))
    }

    async fn promote_target(
        &self,
        volume_id: &volvisor_types::VolumeId,
        migration_id: &volvisor_types::MigrationId,
        attach: &volvisor_types::request::AttachVolumeRequest,
    ) -> Result<volvisor_types::request::AttachVolumeResponse, ApiError> {
        self.promotes.lock().expect("promotes").push((
            volume_id.clone(),
            migration_id.clone(),
            attach.clone(),
        ));
        Ok(volvisor_types::request::AttachVolumeResponse {
            attachment_id: attach.attachment_id.clone(),
            attachment_generation: 1,
            volume_generation: attach.expected_volume_generation + 1,
            frontend: volvisor_types::Frontend::VirtioBlk {
                host_device_path: format!("/dev/drbd-by-res/{volume_id}"),
            },
            state: volvisor_types::AttachmentState::Prepared,
        })
    }

    async fn verify_target_replica(
        &self,
        volume_id: &volvisor_types::VolumeId,
    ) -> Result<(), ApiError> {
        self.target_checks
            .lock()
            .expect("target checks")
            .push(volume_id.clone());
        if self
            .targets
            .lock()
            .expect("targets")
            .iter()
            .any(|target| target == volume_id)
        {
            Ok(())
        } else {
            Err(ApiError::not_found(format!(
                "no target replica of {volume_id} on this host"
            )))
        }
    }

    async fn role_secondary(
        &self,
        _volume_id: &volvisor_types::VolumeId,
    ) -> Result<bool, ApiError> {
        Err(ApiError::not_found("not scripted"))
    }

    async fn fail_closed_fence(
        &self,
        _volume_id: &volvisor_types::VolumeId,
        _reason: &str,
    ) -> Result<(), ApiError> {
        Err(ApiError::not_found("not scripted"))
    }
}

#[tokio::test]
async fn check_mobility_reads_the_handoff_surface() {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new());
    let handoff = Arc::new(FakeHandoffSurface::new());
    let state = Arc::new(
        AppState::new(provider, None, journal, Some(TEST_TOKEN.to_owned()))
            .with_handoff(handoff.clone()),
    );
    let app = app(&state);

    let (status, body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/vms/vm-c-1/check-mobility",
            &json!({"target_host": "dst-host"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["eligible"], json!(true));
    assert_eq!(body["vm_id"], json!("vm-c-1"));
    assert_eq!(
        *handoff.eligibility.lock().expect("eligibility"),
        vec!["vm-c-1".to_owned()]
    );
    // Read-only: no journal record for an eligibility answer.
    assert_eq!(journal_record_count(&state), 0);
}

/// The clear-cut-marker request body (no fencing proof — sufficient
/// only for a volume that is Secondary on this host).
fn clear_cut_body(operation_id: &str) -> Value {
    serde_json::json!({
        "api_version": "volvisor.volume.v2",
        "operation_id": operation_id,
        "fencing_proof": null,
    })
}

/// The clear-cut-marker request body carrying a fencing proof.
fn clear_cut_body_with_proof(operation_id: &str) -> Value {
    serde_json::json!({
        "api_version": "volvisor.volume.v2",
        "operation_id": operation_id,
        "fencing_proof": {
            "volume_id": "vol-clear-1",
            "retired_epoch": 1,
            "commit_index": 7,
        },
    })
}

/// Setup with the handoff surface wired (the drbd daemon wiring, in
/// miniature): the state's provider is still the `FakeProvider`.
fn setup_with_handoff() -> (SharedState, Arc<FakeHandoffSurface>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new());
    let handoff = Arc::new(FakeHandoffSurface::new());
    let state = Arc::new(
        AppState::new(provider, None, journal, Some(TEST_TOKEN.to_owned()))
            .with_handoff(handoff.clone()),
    );
    (state, handoff, dir)
}

#[tokio::test]
async fn clear_cut_marker_runs_through_the_journal_and_replays() {
    let (state, handoff, _dir) = setup_with_handoff();
    let app = app(&state);
    let uri = "/v2/admin/nearline/vol-clear-1/clear-cut-marker";

    let (status, body) = send_json(
        &app,
        json_request(Method::POST, uri, &clear_cut_body("op-clear-1")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["volume_id"], json!("vol-clear-1"));
    // The surface saw exactly one execution, with no proof.
    assert_eq!(
        *handoff.cleared.lock().expect("cleared"),
        vec![(volume_id("vol-clear-1"), false)]
    );

    // The SAME operation id replays the recorded outcome byte-for-byte
    // without a second execution.
    let (status, replayed) = send_json(
        &app,
        json_request(Method::POST, uri, &clear_cut_body("op-clear-1")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(body, replayed);
    assert_eq!(
        handoff.cleared.lock().expect("cleared").len(),
        1,
        "replays never re-execute"
    );
}

#[tokio::test]
async fn clear_cut_marker_conflicts_when_the_same_operation_changes_the_proof() {
    let (state, _handoff, _dir) = setup_with_handoff();
    let app = app(&state);
    let uri = "/v2/admin/nearline/vol-clear-2/clear-cut-marker";

    let (status, _body) = send_json(
        &app,
        json_request(Method::POST, uri, &clear_cut_body("op-clear-2")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The proof is part of the request hash: a different authorization
    // under the same operation id is an idempotency conflict, never a
    // silent second clearing.
    let (status, body) = send_json(
        &app,
        json_request(Method::POST, uri, &clear_cut_body_with_proof("op-clear-2")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], json!("IDEMPOTENCY_CONFLICT"));
}

#[tokio::test]
async fn clear_cut_marker_requires_the_admin_token() {
    let (state, _handoff, _dir) = setup_with_handoff();
    let app = app(&state);

    let (status, body) = send_json(
        &app,
        json_request_without_auth(
            Method::POST,
            "/v2/admin/nearline/vol-clear-3/clear-cut-marker",
            &clear_cut_body("op-clear-3"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], json!("UNAUTHORIZED"));
}

#[tokio::test]
async fn clear_cut_marker_validates_the_envelope() {
    let (state, _handoff, _dir) = setup_with_handoff();
    let app = app(&state);
    let uri = "/v2/admin/nearline/vol-clear-4/clear-cut-marker";

    // A wrong api_version is a typed invalid request (nothing journaled).
    let mut wrong_version = clear_cut_body("op-clear-4");
    wrong_version["api_version"] = json!("volvisor.volume.v1");
    let (status, body) = send_json(&app, json_request(Method::POST, uri, &wrong_version)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["code"],
        json!("UNSUPPORTED_CLASS_OR_POLICY"),
        "a foreign api_version is the unsupported-policy refusal, before anything is journaled"
    );
    assert_eq!(journal_record_count(&state), 0, "nothing is journaled");
}

#[tokio::test]
async fn clear_cut_marker_serves_the_typed_404_without_a_handoff_surface() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let (status, body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/admin/nearline/vol-clear-5/clear-cut-marker",
            &clear_cut_body("op-clear-5"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], json!("NOT_FOUND"));
    assert_eq!(
        body["message"],
        json!("handoff surface not available for this provider")
    );
}

// ---------------------------------------------------------------------------
// Internal peer routes (destination host)
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;
use std::path::PathBuf;

use volvisor_witness::blocking::BlockingWitnessConnection;
use volvisor_witness::proto::{
    BatchGrantOutcome, GrantRequest, GrantResponse, GrantSetRequest, GrantSetResponse,
    RecordBarrierRequest, RecordBarrierResponse, RegisterRequest, RegisterResponse, RenewRequest,
    RenewResponse, RevokeRequest, RevokeResponse, RevokeSetRequest, RevokeSetResponse,
    VoidBarrierRequest, VoidBarrierResponse, WitnessError,
};

use crate::peer::{
    PeerGrantRequest, PeerRouteContext, PreparedParticipant, TargetPreparation,
    TargetPreparationStore,
};

/// One volume's scripted witness authority state.
#[derive(Clone, Debug)]
struct FakeWitnessVolume {
    epoch: u64,
    lease_id: u64,
    lease_state: volvisor_types::LeaseState,
    holder: Option<volvisor_types::HostId>,
}

/// A scripted witness for the peer routes: `inspect` answers the
/// scripted views, `grant_set` mints epochs for this host and records
/// every call (idempotent per operation id, like the real journal);
/// everything else refuses typed (unused by these routes).
struct FakePeerWitness {
    host: volvisor_types::HostId,
    volumes: std::sync::Mutex<BTreeMap<volvisor_types::VolumeId, FakeWitnessVolume>>,
    grant_set_calls: std::sync::Mutex<Vec<GrantSetRequest>>,
    recorded_grants: std::sync::Mutex<BTreeMap<volvisor_types::OperationId, GrantSetResponse>>,
}

impl FakePeerWitness {
    fn new(host: volvisor_types::HostId) -> Self {
        Self {
            host,
            volumes: std::sync::Mutex::new(BTreeMap::new()),
            grant_set_calls: std::sync::Mutex::new(Vec::new()),
            recorded_grants: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Script a live lease held by `host` at `epoch` (the grant-act
    /// inspection's proven-landed arrangement).
    fn set_live(&self, volume: &volvisor_types::VolumeId, epoch: u64, lease_id: u64) {
        self.volumes.lock().expect("volumes").insert(
            volume.clone(),
            FakeWitnessVolume {
                epoch,
                lease_id,
                lease_state: volvisor_types::LeaseState::Live,
                holder: Some(self.host.clone()),
            },
        );
    }
}

impl FakePeerWitness {
    fn view(&self, volume: &volvisor_types::VolumeId) -> volvisor_types::AuthorityView {
        let volumes = self.volumes.lock().expect("volumes");
        let entry = volumes.get(volume);
        volvisor_types::AuthorityView {
            volume_id: volume.clone(),
            current_epoch: volvisor_types::WriterEpoch(entry.map_or(0, |v| v.epoch)),
            holder: entry.and_then(|v| v.holder.clone()),
            lease_state: entry.map_or(volvisor_types::LeaseState::None, |v| v.lease_state),
            lease_id: entry.map(|v| volvisor_types::LeaseId(v.lease_id)),
            lease_remaining_secs: entry.map(|_| 30),
            commit_index: 1,
            registration: None,
            barriers: Vec::new(),
            retirements: Vec::new(),
        }
    }
}

impl BlockingWitnessConnection for FakePeerWitness {
    fn register(
        &self,
        _volume_id: &volvisor_types::VolumeId,
        _request: RegisterRequest,
    ) -> Result<RegisterResponse, WitnessError> {
        Err(WitnessError::InvalidRequest("not scripted".to_owned()))
    }

    fn grant(
        &self,
        _volume_id: &volvisor_types::VolumeId,
        _request: GrantRequest,
    ) -> Result<GrantResponse, WitnessError> {
        Err(WitnessError::InvalidRequest("not scripted".to_owned()))
    }

    fn renew(
        &self,
        _volume_id: &volvisor_types::VolumeId,
        _request: RenewRequest,
    ) -> Result<RenewResponse, WitnessError> {
        Err(WitnessError::InvalidRequest("not scripted".to_owned()))
    }

    fn revoke(
        &self,
        _volume_id: &volvisor_types::VolumeId,
        _request: RevokeRequest,
    ) -> Result<RevokeResponse, WitnessError> {
        Err(WitnessError::InvalidRequest("not scripted".to_owned()))
    }

    fn record_barrier(
        &self,
        _volume_id: &volvisor_types::VolumeId,
        _request: RecordBarrierRequest,
    ) -> Result<RecordBarrierResponse, WitnessError> {
        Err(WitnessError::InvalidRequest("not scripted".to_owned()))
    }

    fn void_barrier(
        &self,
        _volume_id: &volvisor_types::VolumeId,
        _request: VoidBarrierRequest,
    ) -> Result<VoidBarrierResponse, WitnessError> {
        Err(WitnessError::InvalidRequest("not scripted".to_owned()))
    }

    fn revoke_set(&self, _request: RevokeSetRequest) -> Result<RevokeSetResponse, WitnessError> {
        Err(WitnessError::InvalidRequest("not scripted".to_owned()))
    }

    fn grant_set(&self, request: GrantSetRequest) -> Result<GrantSetResponse, WitnessError> {
        if request.host_id != self.host {
            return Err(WitnessError::IdentityRequired);
        }
        if let Some(recorded) = self
            .recorded_grants
            .lock()
            .expect("recorded grants")
            .get(&request.operation_id)
        {
            return Ok(recorded.clone());
        }
        let mut outcomes = Vec::new();
        {
            let mut volumes = self.volumes.lock().expect("volumes");
            for member in &request.requests {
                let entry = volumes
                    .entry(member.volume_id.clone())
                    .or_insert(FakeWitnessVolume {
                        epoch: 0,
                        lease_id: 0,
                        lease_state: volvisor_types::LeaseState::None,
                        holder: None,
                    });
                entry.epoch += 1;
                entry.lease_id += 1;
                entry.lease_state = volvisor_types::LeaseState::Live;
                entry.holder = Some(self.host.clone());
                let retired = entry.epoch - 1;
                outcomes.push(BatchGrantOutcome {
                    volume_id: member.volume_id.clone(),
                    epoch: volvisor_types::WriterEpoch(entry.epoch),
                    lease_id: volvisor_types::LeaseId(entry.lease_id),
                    lease_ttl_secs: 30,
                    fencing_proof: volvisor_types::FencingProof {
                        volume_id: member.volume_id.clone(),
                        retired_epoch: volvisor_types::WriterEpoch(retired),
                        commit_index: 1,
                    },
                });
            }
        }
        let response = GrantSetResponse { grants: outcomes };
        self.grant_set_calls
            .lock()
            .expect("grant-set calls")
            .push(request.clone());
        self.recorded_grants
            .lock()
            .expect("recorded grants")
            .insert(request.operation_id, response.clone());
        Ok(response)
    }

    fn inspect(
        &self,
        volume: &volvisor_types::VolumeId,
    ) -> Result<volvisor_types::AuthorityView, WitnessError> {
        Ok(self.view(volume))
    }
}

/// A VMM controller that records every act label and delegates to the
/// fake VMM (the in-flight resolution tests prove acts were *not*
/// re-executed by the absence of their labels).
struct RecordingVmm {
    inner: Arc<volvisor_provider::FakeVmm>,
    calls: std::sync::Mutex<Vec<&'static str>>,
}

impl RecordingVmm {
    fn new(inner: Arc<volvisor_provider::FakeVmm>) -> Self {
        Self {
            inner,
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn record(&self, label: &'static str) {
        self.calls.lock().expect("calls").push(label);
    }
}

impl volvisor_provider::VmmController for RecordingVmm {
    fn pause(&self, vm_id: &str) -> Result<volvisor_provider::PauseProof, ApiError> {
        self.record("pause");
        self.inner.pause(vm_id)
    }

    fn snapshot(&self, vm_id: &str, dir: &std::path::Path) -> Result<(), ApiError> {
        self.record("snapshot");
        self.inner.snapshot(vm_id, dir)
    }

    fn destroy(&self, vm_id: &str) -> Result<(), ApiError> {
        self.record("destroy");
        self.inner.destroy(vm_id)
    }

    fn restore(
        &self,
        vm_id: &str,
        dir: &std::path::Path,
        disks: &[volvisor_provider::DiskMapping],
    ) -> Result<(), ApiError> {
        self.record("restore");
        self.inner.restore(vm_id, dir, disks)
    }

    fn resume(&self, vm_id: &str) -> Result<(), ApiError> {
        self.record("resume");
        self.inner.resume(vm_id)
    }

    fn state(&self, vm_id: &str) -> Result<volvisor_provider::VmState, ApiError> {
        self.inner.state(vm_id)
    }
}

/// The peer-route test kit: the wired state plus every double it
/// holds, so tests can arrange witness/provider/VMM state directly.
struct PeerKit {
    state: SharedState,
    witness: Arc<FakePeerWitness>,
    handoff: Arc<FakeHandoffSurface>,
    vmm: Arc<RecordingVmm>,
    fake_vmm: Arc<volvisor_provider::FakeVmm>,
    provider: Arc<FakeProvider>,
    store: TargetPreparationStore,
    host: volvisor_types::HostId,
    snapshot_root: PathBuf,
    dir: tempfile::TempDir,
}

fn setup_peer_with_root(snapshot_root: PathBuf) -> PeerKit {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new());
    let host = host_id("dst-host");
    let witness = Arc::new(FakePeerWitness::new(host.clone()));
    let handoff = Arc::new(FakeHandoffSurface::new());
    let fake_vmm = Arc::new(volvisor_provider::FakeVmm::new(&snapshot_root));
    let vmm = Arc::new(RecordingVmm::new(Arc::clone(&fake_vmm)));
    let store =
        TargetPreparationStore::open(dir.path().join("peer-preparations")).expect("store open");
    let context = Arc::new(PeerRouteContext::new(
        Arc::clone(&witness) as Arc<dyn BlockingWitnessConnection>,
        Arc::clone(&vmm) as Arc<dyn volvisor_provider::VmmController>,
        Arc::clone(&handoff) as Arc<dyn volvisor_provider::HandoffSurface>,
        Arc::clone(&provider) as Arc<dyn volvisor_provider::VolumeProvider>,
        host.clone(),
        store.clone(),
        snapshot_root,
    ));
    let state = Arc::new(
        AppState::new(provider.clone(), None, journal, Some(TEST_TOKEN.to_owned()))
            .with_peer_routes(Some(PEER_TOKEN.to_owned()), context),
    );
    PeerKit {
        state,
        witness,
        handoff,
        vmm,
        fake_vmm,
        provider,
        store,
        host,
        snapshot_root: PathBuf::new(),
        dir,
    }
}

fn setup_peer() -> PeerKit {
    let dir = tempfile::tempdir().expect("temporary snapshot root");
    let mut kit = setup_peer_with_root(dir.path().to_path_buf());
    kit.snapshot_root = dir.path().to_path_buf();
    // Hold the snapshot root tempdir for the kit's lifetime by leaking
    // it into the kit's own directory marker (tempdir cleans up on
    // drop; the journal tempdir lives in `_dir`, the snapshot root is
    // recreated per test below).
    std::mem::forget(dir);
    kit
}

/// A peer request with the peer credential.
fn peer_request(method: Method, uri: &str, body: &Value) -> Request<Body> {
    with_bearer(json_request_without_auth(method, uri, body), PEER_TOKEN)
}

fn peer_prepare_body(migration: &str, vm: &str, volumes: &[(&str, u64)]) -> Value {
    json!({
        "migration_id": migration,
        "vm_id": vm,
        "source_host": "src-host",
        "volume_ids": volumes.iter().map(|(v, _)| *v).collect::<Vec<_>>(),
        "expected_generations": volumes.iter().map(|(_, g)| *g).collect::<Vec<_>>(),
    })
}

async fn create_volume(kit: &PeerKit, raw: &str) {
    kit.provider
        .create_volume(&fixture_create_request(raw, GIB))
        .await
        .expect("create volume");
    // The fake provider's volumes model this host's established
    // replicas (the happy-path peer tests' destination); the surface's
    // replica-level gate must see them as targets.
    kit.handoff.add_target(&volume_id(raw));
}

#[tokio::test]
async fn peer_routes_fail_closed_without_the_peer_credential() {
    let kit = setup_peer();
    let app = app(&kit.state);
    let uri = "/v2/internal/peer/health";

    // No token, a wrong token, and — the credential distinction the
    // plan pins — the ADMIN token: all rejected with 401.
    let (status, body) = send_json(
        &app,
        json_request_without_auth(Method::GET, uri, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], json!("UNAUTHORIZED"));

    let (status, _body) = send_json(
        &app,
        with_bearer(
            json_request_without_auth(Method::GET, uri, &json!({})),
            "wrong-peer-token",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _body) = send_json(
        &app,
        with_bearer(
            json_request_without_auth(Method::GET, uri, &json!({})),
            TEST_TOKEN,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the admin token is not the peer credential"
    );
}

#[tokio::test]
async fn peer_routes_serve_the_typed_404_without_the_peer_context() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-x", "vm-x", &[("vol-x", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], json!("NOT_FOUND"));
}

#[tokio::test]
async fn peer_prepare_verifies_volumes_and_persists_idempotently() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-pp1").await;

    // An unknown volume is the typed refusal (nothing journaled beyond
    // the intent+failure outcome).
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-pp-unknown", "vm-pp", &[("vol-nope", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], json!("NOT_FOUND"));

    // A stale expected generation is the typed conflict.
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-pp-stale", "vm-pp", &[("vol-pp1", 99)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], json!("STALE_GENERATION"));

    // The honest preparation succeeds and persists.
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-pp1", "vm-pp", &[("vol-pp1", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["migration_id"], json!("mig-pp1"));
    assert_eq!(body["participants"][0]["volume_id"], json!("vol-pp1"));
    assert!(
        kit.store
            .load(&migration_id("mig-pp1"))
            .expect("store")
            .is_some(),
        "the preparation is durable"
    );

    // The identical request replays byte-for-byte without a second
    // verification pass (the journal replays the recorded outcome).
    let (status, replayed) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-pp1", "vm-pp", &[("vol-pp1", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, replayed);

    // Differing content for the same migration is the typed conflict.
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-pp1", "vm-other", &[("vol-pp1", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], json!("IDEMPOTENCY_CONFLICT"));
}

#[tokio::test]
async fn peer_prepare_rejects_an_unsafe_vm_id_and_an_unusable_snapshot_dir() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-pp2").await;

    // A vm_id that would escape the snapshot root is refused before
    // anything is journaled.
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-pp-esc", "../escape", &[("vol-pp2", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], json!("INVALID_REQUEST"));
    assert_eq!(journal_record_count(&kit.state), 0, "nothing is journaled");

    // An unusable snapshot root (a file where the directory should
    // be) refuses the preparation typed: the shared-path boundary is
    // proven at PREPARED, never at config time.
    let bad_root = kit.dir.path().join("not-a-dir");
    std::fs::write(&bad_root, b"file").expect("write blocker file");
    let blocked = setup_peer_with_root(bad_root);
    create_volume(&blocked, "vol-pp2").await;
    let blocked_app = router::router(blocked.state.clone(), ApiConfig::default().max_body_bytes);
    let (status, body) = send_json(
        &blocked_app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-pp-blocked", "vm-pp", &[("vol-pp2", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], json!("INVALID_STATE"));
}

#[tokio::test]
async fn peer_grant_grants_promotes_and_answers_device_paths() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-g1").await;
    create_volume(&kit, "vol-g2").await;

    let (status, _body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-g1", "vm-g", &[("vol-g1", 1), ("vol-g2", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-g1"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["migration_id"], json!("mig-g1"));
    assert_eq!(body["grants"][0]["volume_id"], json!("vol-g1"));
    assert_eq!(
        body["grants"][0]["device_path"],
        json!("/dev/drbd-by-res/vol-g1")
    );
    assert_eq!(body["grants"][0]["epoch"], json!(1));
    assert_eq!(body["grants"][0]["lease_ttl_secs"], json!(30));

    // The witness batch ran exactly once, as this host, for this
    // migration, under the deterministic batch operation id derived
    // over the ordered participant set.
    let calls = kit.witness.grant_set_calls.lock().expect("calls").clone();
    assert_eq!(calls.len(), 1, "{body}");
    assert_eq!(calls[0].host_id, kit.host);
    assert_eq!(calls[0].migration_id, Some(migration_id("mig-g1")));
    let id_participants: Vec<volvisor_handoff::Participant> = ["vol-g1", "vol-g2"]
        .iter()
        .map(|raw| volvisor_handoff::Participant {
            volume_id: volume_id(raw),
            expected_generation: 1,
            resource: String::new(),
            minor: 0,
        })
        .collect();
    let expected_op = volvisor_handoff::batch_operation_id(
        &migration_id("mig-g1"),
        volvisor_handoff::BatchStep::GrantSet,
        &id_participants,
    )
    .expect("derive op id");
    assert_eq!(calls[0].operation_id, expected_op);

    // The promote ran per participant with the derived attach
    // identity, this host, and the preparation's vm.
    let promotes = kit.handoff.promotes.lock().expect("promotes").clone();
    assert_eq!(promotes.len(), 2);
    for (volume, migration, attach) in &promotes {
        assert_eq!(migration, &migration_id("mig-g1"));
        assert_eq!(attach.vm_id, "vm-g");
        assert_eq!(attach.host_id, kit.host);
        // The deterministic per-(migration, volume) attach identity:
        // the operation id and the attachment id are the same derived
        // string (distinct namespaces, one derivation).
        assert_eq!(attach.operation_id.as_str(), attach.attachment_id.as_str());
        assert!(attach.operation_id.as_str().starts_with("mig-attach-"));
        assert!(matches!(volume.as_str(), "vol-g1" | "vol-g2"));
    }

    // The device paths are durable (the restore's verification reads
    // them).
    let preparation = kit
        .store
        .load(&migration_id("mig-g1"))
        .expect("store")
        .expect("prepared");
    for participant in &preparation.participants {
        assert_eq!(
            participant.device_path.as_deref(),
            Some(format!("/dev/drbd-by-res/{}", participant.volume_id).as_str())
        );
    }

    // The replay is byte-identical and re-executes nothing.
    let (status, replayed) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-g1"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, replayed);
    assert_eq!(
        kit.witness.grant_set_calls.lock().expect("calls").len(),
        1,
        "replays never re-execute"
    );
}

#[tokio::test]
async fn peer_grant_without_a_preparation_is_the_typed_not_found() {
    let kit = setup_peer();
    let app = app(&kit.state);

    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-absent"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], json!("NOT_FOUND"));
}

/// Inject an intent-without-outcome for a peer route's derived journal
/// operation id — exactly like a crash between the journaling of the
/// intent and the recording of the outcome.
fn inject_peer_intent(
    kit: &PeerKit,
    migration: &volvisor_types::MigrationId,
    tag: &str,
    op_kind: &'static str,
    body: &Value,
) {
    let operation_id = ops::mobility_operation_id(migration, tag).expect("derived op id");
    let hash = ops::mobility_request_hash(tag, body);
    let mut journal = kit.state.journal.lock().expect("journal lock in test");
    journal
        .append_intent(
            operation_id,
            hash,
            op_kind,
            serde_json::json!({"injected": true}),
        )
        .expect("append injected intent");
}

#[tokio::test]
async fn peer_grant_resolves_an_in_flight_intent_by_inspection() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-r1").await;

    // Arrange the proven-landed state directly: a preparation whose
    // device paths are on record, and a witness lease live and held
    // by this host on every participant.
    let migration = migration_id("mig-resolve");
    kit.store
        .install(&TargetPreparation {
            migration_id: migration.clone(),
            vm_id: "vm-r".to_owned(),
            source_host: host_id("src-host"),
            target_host: kit.host.clone(),
            participants: vec![PreparedParticipant {
                volume_id: volume_id("vol-r1"),
                expected_generation: 1,
                device_path: Some("/dev/drbd-by-res/vol-r1".to_owned()),
            }],
            created_at: 1,
        })
        .expect("install preparation");
    kit.witness.set_live(&volume_id("vol-r1"), 4, 9);

    inject_peer_intent(
        &kit,
        &migration,
        "peer-grant",
        ops::OP_PEER_GRANT,
        &serde_json::to_value(PeerGrantRequest {
            migration_id: migration.clone(),
        })
        .expect("serialize"),
    );

    // The inspection proves the act landed: the response is served
    // from the proven facts and the witness batch is NOT re-issued.
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-resolve"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["grants"][0]["device_path"],
        json!("/dev/drbd-by-res/vol-r1")
    );
    assert_eq!(body["grants"][0]["epoch"], json!(4));
    assert_eq!(
        kit.witness.grant_set_calls.lock().expect("calls").len(),
        0,
        "a proven-landed grant is resolved by inspection, never re-executed"
    );
    assert!(
        kit.handoff.promotes.lock().expect("promotes").is_empty(),
        "the promote is not re-driven either"
    );

    // A later retry now replays the journaled resolution outcome.
    let (status, replayed) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-resolve"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, replayed);
    assert_eq!(kit.witness.grant_set_calls.lock().expect("calls").len(), 0);
}

#[tokio::test]
async fn peer_grant_re_drives_an_in_flight_intent_that_did_not_land() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-r2").await;

    let migration = migration_id("mig-re-drive");
    let (status, _body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-re-drive", "vm-r2", &[("vol-r2", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // No witness lease, no device paths: the inspection proves the
    // act did NOT land, and the re-drive re-executes — the witness
    // batch under its deterministic operation id, the promote
    // re-verifying its own preconditions.
    inject_peer_intent(
        &kit,
        &migration,
        "peer-grant",
        ops::OP_PEER_GRANT,
        &serde_json::to_value(PeerGrantRequest {
            migration_id: migration.clone(),
        })
        .expect("serialize"),
    );

    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-re-drive"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["grants"][0]["device_path"],
        json!("/dev/drbd-by-res/vol-r2")
    );
    assert_eq!(
        kit.witness.grant_set_calls.lock().expect("calls").len(),
        1,
        "the re-drive re-executed the witness batch exactly once"
    );
    assert_eq!(kit.handoff.promotes.lock().expect("promotes").len(), 1);
}

/// Write a minimal snapshot directory the fake VMM's restore can
/// genuinely read (the same artifacts the verified `ch-remote
/// snapshot` surface produces).
fn write_snapshot(dir: &std::path::Path, declared_paths: &[&str]) {
    std::fs::create_dir_all(dir).expect("create snapshot dir");
    let config = json!({
        "disks": declared_paths
            .iter()
            .map(|path| json!({ "path": path }))
            .collect::<Vec<_>>(),
    });
    std::fs::write(dir.join("config.json"), config.to_string()).expect("write config");
    std::fs::write(dir.join("memory-ranges"), b"fake-memory-ranges").expect("write memory");
    std::fs::write(dir.join("state.json"), b"{\"vm_state\":\"Paused\"}").expect("write state");
}

#[tokio::test]
async fn peer_restore_vm_restores_and_resumes_under_verified_disk_mappings() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-v1").await;

    let (status, _body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-v1", "vm-v", &[("vol-v1", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, granted) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-v1"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{granted}");
    let device_path = granted["grants"][0]["device_path"]
        .as_str()
        .expect("path")
        .to_owned();

    let snapshot_dir = kit.snapshot_root.join("vm-v");
    write_snapshot(&snapshot_dir, &["/dev/source/vol-v1"]);

    // The honest restore-then-resume first: the destination VM lands
    // running with the mapped device.
    let restore_body = json!({
        "migration_id": "mig-v1",
        "snapshot_dir": snapshot_dir.display().to_string(),
        "disks": [{"declared_path": "/dev/source/vol-v1", "device_path": device_path}],
        "resume": true,
    });
    let (status, body) = send_json(
        &app,
        peer_request(Method::POST, "/v2/internal/peer/restore-vm", &restore_body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["vm_state"], json!("running"));
    assert_eq!(
        kit.fake_vmm.vm_state("vm-v").expect("state"),
        volvisor_provider::VmState::Running
    );
    assert_eq!(
        kit.fake_vmm.vm_devices("vm-v").expect("devices"),
        vec![device_path.clone()]
    );
    assert_eq!(
        *kit.vmm.calls.lock().expect("calls"),
        vec!["restore", "resume"],
        "the absent-VMM forward path is restore then resume"
    );

    // The replay re-executes nothing.
    let calls_before = kit.vmm.calls.lock().expect("calls").len();
    let (status, replayed) = send_json(
        &app,
        peer_request(Method::POST, "/v2/internal/peer/restore-vm", &restore_body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, replayed);
    assert_eq!(kit.vmm.calls.lock().expect("calls").len(), calls_before);

    // A disk mapping that does not match the promoted participant set
    // is the typed refusal, before any VMM act — proven on a second
    // migration (the first one's restore-vm operation id now carries
    // its recorded outcome, and a different body under it is the
    // journal's idempotency conflict, exactly as designed).
    let (status, _body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-v1m", "vm-v", &[("vol-v1", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-v1m"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let calls_before = kit.vmm.calls.lock().expect("calls").len();
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/restore-vm",
            &json!({
                "migration_id": "mig-v1m",
                "snapshot_dir": snapshot_dir.display().to_string(),
                "disks": [{"declared_path": "/dev/source/vol-v1", "device_path": "/dev/smuggled"}],
                "resume": true,
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], json!("INVALID_STATE"));
    assert_eq!(
        kit.vmm.calls.lock().expect("calls").len(),
        calls_before,
        "no VMM act runs for a mismatched mapping"
    );
}

#[tokio::test]
async fn peer_restore_vm_resolves_an_in_flight_intent_by_the_observed_vm_state() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-v2").await;

    let migration = migration_id("mig-v2");
    let (status, _body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-v2", "vm-v2", &[("vol-v2", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, granted) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-v2"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{granted}");
    let device_path = granted["grants"][0]["device_path"]
        .as_str()
        .expect("path")
        .to_owned();

    let snapshot_dir = kit.snapshot_root.join("vm-v2");
    write_snapshot(&snapshot_dir, &["/dev/source/vol-v2"]);
    let restore_body = json!({
        "migration_id": "mig-v2",
        "snapshot_dir": snapshot_dir.display().to_string(),
        "disks": [{"declared_path": "/dev/source/vol-v2", "device_path": device_path}],
        "resume": true,
    });

    // Arrange the proven-landed state: the destination VM is already
    // running with the mapped device (a completed prior restore).
    kit.fake_vmm
        .create("vm-v2", &[&device_path])
        .expect("create vm");
    kit.fake_vmm.start("vm-v2").expect("start vm");

    inject_peer_intent(
        &kit,
        &migration,
        "peer-restore-vm",
        ops::OP_PEER_RESTORE_VM,
        &restore_body,
    );

    // The inspection proves the act from the observed VM state: no
    // destroy, no restore, no resume is issued.
    let (status, body) = send_json(
        &app,
        peer_request(Method::POST, "/v2/internal/peer/restore-vm", &restore_body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["vm_state"], json!("running"));
    assert!(
        kit.vmm.calls.lock().expect("calls").is_empty(),
        "a proven-landed restore is resolved by inspection, never re-executed"
    );
}

#[tokio::test]
async fn peer_restore_vm_refuses_a_created_destination_vm_on_the_resume_path() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-v3").await;

    let (status, _body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-v3", "vm-v3", &[("vol-v3", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, granted) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-v3"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{granted}");
    let device_path = granted["grants"][0]["device_path"]
        .as_str()
        .expect("path")
        .to_owned();

    let snapshot_dir = kit.snapshot_root.join("vm-v3");
    write_snapshot(&snapshot_dir, &["/dev/source/vol-v3"]);

    // A defined, not-booted destination VM is a foreign shape the
    // resume path refuses typed (never destroys a VM the migration
    // did not put there).
    kit.fake_vmm
        .create("vm-v3", &[&device_path])
        .expect("create vm");
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/restore-vm",
            &json!({
                "migration_id": "mig-v3",
                "snapshot_dir": snapshot_dir.display().to_string(),
                "disks": [{"declared_path": "/dev/source/vol-v3", "device_path": device_path}],
                "resume": true,
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], json!("INVALID_STATE"));
}

#[tokio::test]
async fn peer_discard_drops_the_preparation_idempotently() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-d1").await;

    let (status, _body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-d1", "vm-d", &[("vol-d1", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/discard",
            &json!({"migration_id": "mig-d1"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["discarded"], json!(true));

    // The same request replays the recorded outcome byte-for-byte
    // (the journal's discipline — the discard's operation id is
    // derived from the migration id).
    let (status, replayed) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/discard",
            &json!({"migration_id": "mig-d1"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, replayed);

    // An absent preparation is the honest idempotent answer on a
    // fresh operation id (a discard of a migration that was never
    // prepared).
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/discard",
            &json!({"migration_id": "mig-never-prepared"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["discarded"], json!(false));

    // With the preparation gone, the grant is the typed not-found.
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-d1"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn peer_health_reports_the_honest_snapshot_dir_answer() {
    let kit = setup_peer();
    let app = app(&kit.state);

    let (status, body) = send_json(
        &app,
        peer_request(Method::GET, "/v2/internal/peer/health", &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["host_id"], json!("dst-host"));
    assert_eq!(body["snapshot_dir_readable"], json!(true));

    // A root that cannot be probed is reported false — honestly, at
    // call time.
    let bad_root = kit.dir.path().join("blocked-root");
    std::fs::write(&bad_root, b"file").expect("write blocker file");
    let blocked = setup_peer_with_root(bad_root);
    let blocked_app = router::router(blocked.state.clone(), ApiConfig::default().max_body_bytes);
    let (status, body) = send_json(
        &blocked_app,
        peer_request(Method::GET, "/v2/internal/peer/health", &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["snapshot_dir_readable"], json!(false));
}
