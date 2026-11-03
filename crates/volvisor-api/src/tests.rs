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

use std::sync::{Arc, Mutex, Once, OnceLock};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use volvisor_journal::Journal;
use volvisor_provider::VolumeProvider;
use volvisor_provider::conformance::{
    fixture_attach_request, fixture_create_request, fixture_create_request_in_project,
    fixture_delete_request, fixture_detach_request, fixture_grow_request, fixture_report_request,
};
use volvisor_provider::fake::FakeProvider;
use volvisor_types::{ApiError, ApiErrorBody, ApiErrorCode, AttachmentEvidence, AttachmentState};

use crate::auth::{Credential, Role};
use crate::{ApiConfig, AppState, SharedState, ops, router, router_with_config};

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

const ADMIN_TOKEN: &str = "scoped-admin-token";
const OPERATOR_TOKEN: &str = "scoped-operator-token";
const READ_ONLY_TOKEN: &str = "scoped-read-only-token";

fn scoped_credentials() -> Vec<Credential> {
    vec![
        Credential {
            id: "admin-a".to_owned(),
            token: ADMIN_TOKEN.to_owned(),
            role: Role::Admin,
        },
        Credential {
            id: "operator-a".to_owned(),
            token: OPERATOR_TOKEN.to_owned(),
            role: Role::Operator,
        },
        Credential {
            id: "reader-a".to_owned(),
            token: READ_ONLY_TOKEN.to_owned(),
            role: Role::ReadOnly,
        },
    ]
}

fn setup_scoped_with(
    credentials: Vec<Credential>,
) -> (SharedState, Arc<FakeProvider>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let (state, provider) = setup_scoped_in(&dir, credentials);
    (state, provider, dir)
}

fn setup_scoped_in(
    dir: &tempfile::TempDir,
    credentials: Vec<Credential>,
) -> (SharedState, Arc<FakeProvider>) {
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new());
    let admin: Arc<dyn volvisor_provider::AdminSurface> = provider.clone();
    let state = Arc::new(
        AppState::new(provider.clone(), Some(admin), journal, None).with_credentials(credentials),
    );
    (state, provider)
}

fn setup_scoped() -> (SharedState, Arc<FakeProvider>, tempfile::TempDir) {
    setup_scoped_with(scoped_credentials())
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

/// State with the lending routes wired over the plain fake: the
/// lending ops journal their intents through the shared pipeline,
/// then the provider (which has no durable device world) answers its
/// typed 404 — the shape the journal-redaction e2e needs, since the
/// payload is journaled whatever the provider afterwards answers.
fn setup_with_lending() -> (SharedState, Arc<FakeProvider>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new());
    let admin: Arc<dyn volvisor_provider::AdminSurface> = provider.clone();
    let lending: Arc<dyn volvisor_provider::LendingSurface> = provider.clone();
    let state = Arc::new(
        AppState::new(
            provider.clone(),
            Some(admin),
            journal,
            Some(TEST_TOKEN.to_owned()),
        )
        .with_lending(lending),
    );
    (state, provider, dir)
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

#[tokio::test]
async fn attachment_state_report_is_monotonic_and_never_moves_the_generation() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let create = serde_json::to_value(fixture_create_request("vol-report", GIB))
        .expect("serialize create fixture");
    let (status, _) = send_json(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::OK);
    let attach = serde_json::to_value(fixture_attach_request("vol-report", "att-report-1", 1))
        .expect("serialize attach fixture");
    let (status, _) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes/vol-report/attach", &attach),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Inspect exposes the full per-attachment projection at prepared/1.
    let (status, inspected) = send_json(&app, request(Method::GET, "/v2/volumes/vol-report")).await;
    assert_eq!(status, StatusCode::OK);
    let views = inspected["attachments"]
        .as_array()
        .expect("attachments array");
    assert_eq!(views.len(), 1);
    assert_eq!(views[0]["attachment_id"], json!("att-report-1"));
    assert_eq!(views[0]["state"], json!("prepared"));
    assert_eq!(views[0]["state_revision"], json!(1));
    assert_eq!(views[0]["attachment_generation"], json!(1));
    assert!(views[0].get("evidence").is_none());

    // advertised with vmm_configured: applied, revision 2, generation fixed.
    let advertised = serde_json::to_value(fixture_report_request(
        "att-report-1",
        1,
        AttachmentState::Advertised,
        AttachmentEvidence::VmmConfigured,
    ))
    .expect("serialize report fixture");
    let (status, reported) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-report/attachment-state",
            &advertised,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reported["applied"], json!(true));
    assert_eq!(reported["state"], json!("advertised"));
    assert_eq!(reported["state_revision"], json!(2));
    assert_eq!(reported["attachment_generation"], json!(1));
    assert_eq!(reported["evidence"], json!("vmm_configured"));
    assert_eq!(reported["reporter"], json!("conformance-vmm"));
    assert!(
        reported["recorded_at_ms"].is_u64(),
        "an applied report carries the server receipt time"
    );

    // active requires guest_confirmed: a vmm_configured active report is a
    // typed pre-journal refusal (no record, the operation_id stays reusable).
    let mut invalid_active = advertised.clone();
    invalid_active["state"] = json!("active");
    invalid_active["operation_id"] = json!("op-report-invalid-active");
    let (status, rejected) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-report/attachment-state",
            &invalid_active,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(rejected["code"], json!("INVALID_REQUEST"));

    // active with guest_confirmed: applied at revision 3.
    let active = serde_json::to_value(fixture_report_request(
        "att-report-1",
        1,
        AttachmentState::Active,
        AttachmentEvidence::GuestConfirmed,
    ))
    .expect("serialize report fixture");
    let (status, active_body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-report/attachment-state",
            &active,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(active_body["applied"], json!(true));
    assert_eq!(active_body["state"], json!("active"));
    assert_eq!(active_body["state_revision"], json!(3));
    assert_eq!(active_body["attachment_generation"], json!(1));
    assert_eq!(active_body["evidence"], json!("guest_confirmed"));
    assert_eq!(active_body["reporter"], json!("conformance-vmm"));
    assert!(active_body["recorded_at_ms"].is_u64());

    // An equal report (fresh operation id) is the same no-op as a lower
    // one: applied=false, no revision bump, the applied projection kept.
    let mut equal = active.clone();
    equal["operation_id"] = json!("op-report-equal-active");
    let (status, equal_body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-report/attachment-state",
            &equal,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(equal_body["applied"], json!(false));
    assert_eq!(equal_body["state"], json!("active"));
    assert_eq!(equal_body["state_revision"], json!(3));
    assert_eq!(equal_body["recorded_at_ms"], active_body["recorded_at_ms"]);

    // Inspect re-projects the applied evidence verbatim.
    let (status, after_report) =
        send_json(&app, request(Method::GET, "/v2/volumes/vol-report")).await;
    assert_eq!(status, StatusCode::OK);
    let view = &after_report["attachments"][0];
    assert_eq!(view["state"], json!("active"));
    assert_eq!(view["state_revision"], json!(3));
    assert_eq!(view["evidence"], json!("guest_confirmed"));
    assert_eq!(view["reporter"], json!("conformance-vmm"));
    assert_eq!(view["recorded_at_ms"], active_body["recorded_at_ms"]);
    assert_eq!(view["attachment_generation"], json!(1));

    // Reusing the active operation_id with a different payload is the
    // standard journal conflict, independent of the no-op rules.
    let mut conflicting = active.clone();
    conflicting["state"] = json!("advertised");
    let (status, conflict_body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-report/attachment-state",
            &conflicting,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict_body["code"], json!("IDEMPOTENCY_CONFLICT"));

    // A lower report is an idempotent no-op: no downgrade, no revision bump.
    let lower = serde_json::to_value(fixture_report_request(
        "att-report-1",
        1,
        AttachmentState::Advertised,
        AttachmentEvidence::GuestConfirmed,
    ))
    .expect("serialize report fixture");
    let (status, lower_body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-report/attachment-state",
            &lower,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(lower_body["applied"], json!(false));
    assert_eq!(lower_body["state"], json!("active"));
    assert_eq!(lower_body["state_revision"], json!(3));

    // A stale attachment instance is fenced.
    let stale_report = serde_json::to_value(fixture_report_request(
        "att-report-1",
        2,
        AttachmentState::Active,
        AttachmentEvidence::GuestConfirmed,
    ))
    .expect("serialize report fixture");
    let (status, stale_body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-report/attachment-state",
            &stale_report,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stale_body["code"], json!("STALE_GENERATION"));

    // The refusal is a journaled terminal outcome: the same operation_id
    // replays it verbatim and adds no record.
    let records_after_stale = journal_record_count(&state);
    let (status, replayed_stale) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-report/attachment-state",
            &stale_report,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(replayed_stale, stale_body);
    assert_eq!(journal_record_count(&state), records_after_stale);

    // Reports never moved the generation: detach fences on 1.
    let mut detach = serde_json::to_value(fixture_detach_request("att-report-1", 1))
        .expect("serialize detach fixture");
    detach["attachment_id"] = json!("att-report-1");
    let (status, detached) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes/vol-report/detach", &detach),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detached["attachments"], json!([]));

    // Replaying the recorded active report returns its recorded outcome even
    // after detach (the journal outranks the provider's current NOT_FOUND).
    let (status, replayed) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-report/attachment-state",
            &active,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replayed, active_body);

    // A new report against the detached instance is NOT_FOUND.
    let mut fresh = lower.clone();
    fresh["operation_id"] = json!("op-report-after-detach");
    let (status, gone) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-report/attachment-state",
            &fresh,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(gone["code"], json!("NOT_FOUND"));

    // A consumed attachment id cannot be re-attached under a new operation:
    // per-id evidence must never be misattributed to a new instance.
    let mut reattach =
        serde_json::to_value(fixture_attach_request("vol-report", "att-report-1", 3))
            .expect("serialize attach fixture");
    reattach["operation_id"] = json!("op-reattach-consumed-id");
    let (status, reuse_body) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes/vol-report/attach", &reattach),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(reuse_body["code"], json!("IDEMPOTENCY_CONFLICT"));
}

/// Report envelope rejections happen before the journal: no record, and
/// the `operation_id` stays reusable for a corrected request.
#[tokio::test]
async fn report_envelope_rejections_leave_no_journal_record() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let create = serde_json::to_value(fixture_create_request("vol-report-envelope", GIB))
        .expect("serialize create fixture");
    let (status, _) = send_json(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::OK);
    let attach = serde_json::to_value(fixture_attach_request(
        "vol-report-envelope",
        "att-report-envelope",
        1,
    ))
    .expect("serialize attach fixture");
    let (status, _) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-report-envelope/attach",
            &attach,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let records_after_attach = journal_record_count(&state);

    // `active` with only `vmm_configured` is an invalid envelope pairing.
    let mut malformed = serde_json::to_value(fixture_report_request(
        "att-report-envelope",
        1,
        AttachmentState::Advertised,
        AttachmentEvidence::VmmConfigured,
    ))
    .expect("serialize report fixture");
    malformed["operation_id"] = json!("op-report-envelope");
    malformed["state"] = json!("active");
    let (status, body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-report-envelope/attachment-state",
            &malformed,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "envelope: {body}");
    assert_eq!(body["code"], json!("INVALID_REQUEST"));
    assert_eq!(journal_record_count(&state), records_after_attach);

    // A control-bearing reporter is also a pre-journal envelope refusal.
    let mut bad_reporter = serde_json::to_value(fixture_report_request(
        "att-report-envelope",
        1,
        AttachmentState::Advertised,
        AttachmentEvidence::VmmConfigured,
    ))
    .expect("serialize report fixture");
    bad_reporter["operation_id"] = json!("op-report-bad-reporter");
    bad_reporter["reporter"] = json!("chv\nagent");
    let (status, body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-report-envelope/attachment-state",
            &bad_reporter,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "envelope: {body}");
    assert_eq!(body["code"], json!("INVALID_REQUEST"));
    assert_eq!(journal_record_count(&state), records_after_attach);

    // The same operation_id, now with a well-formed envelope, applies —
    // the rejected attempt left no journal record behind.
    let mut corrected = serde_json::to_value(fixture_report_request(
        "att-report-envelope",
        1,
        AttachmentState::Advertised,
        AttachmentEvidence::VmmConfigured,
    ))
    .expect("serialize report fixture");
    corrected["operation_id"] = json!("op-report-envelope");
    let (status, body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-report-envelope/attachment-state",
            &corrected,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "corrected envelope: {body}");
    assert_eq!(body["applied"], json!(true));
    assert_eq!(body["state"], json!("advertised"));
}

/// A provider that does not advertise `attachment_state_report` answers the
/// typed NOT_FOUND, and that refusal is a journaled terminal outcome: the
/// recorded bytes replay under the same operation id.
#[tokio::test]
async fn report_without_the_capability_is_a_journaled_not_found_that_replays() {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new().with_capabilities(
        volvisor_types::CapabilitySet::of([
            volvisor_types::Capability::Create,
            volvisor_types::Capability::Attach,
            volvisor_types::Capability::Resize,
        ]),
    ));
    let admin: Arc<dyn volvisor_provider::AdminSurface> = provider.clone();
    let state = Arc::new(AppState::new(
        provider,
        Some(admin),
        journal,
        Some(TEST_TOKEN.to_owned()),
    ));
    let app = app(&state);

    let create = serde_json::to_value(fixture_create_request("vol-no-cap-report", GIB))
        .expect("serialize create fixture");
    let (status, _) = send_json(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::OK);
    let attach = serde_json::to_value(fixture_attach_request(
        "vol-no-cap-report",
        "att-no-cap-report",
        1,
    ))
    .expect("serialize attach fixture");
    let (status, _) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-no-cap-report/attach",
            &attach,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let report = serde_json::to_value(fixture_report_request(
        "att-no-cap-report",
        1,
        AttachmentState::Advertised,
        AttachmentEvidence::VmmConfigured,
    ))
    .expect("serialize report fixture");
    let (status, body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-no-cap-report/attachment-state",
            &report,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], json!("NOT_FOUND"));
    let records = journal_record_count(&state);

    let (status, replayed) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-no-cap-report/attachment-state",
            &report,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(replayed, body, "a journaled refusal replays verbatim");
    assert_eq!(journal_record_count(&state), records);
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

/// A stub grow notifier: records the calls, returns one scripted
/// status (P6-B — the compose seam's focused test; the real engine's
/// state machine lives in `volvisor-provider`'s `grow_tests`, and the
/// daemon-level e2e composes the real engine).
struct StubGrowNotifier {
    calls: std::sync::Mutex<Vec<(volvisor_types::VolumeId, u64)>>,
    status: volvisor_types::request::GrowGuestNotification,
}

impl volvisor_provider::GrowNotifier for StubGrowNotifier {
    fn notify_grow(
        &self,
        volume_id: &volvisor_types::VolumeId,
        effective_size_bytes: u64,
    ) -> volvisor_types::request::GrowGuestNotification {
        self.calls
            .lock()
            .expect("stub notifier lock")
            .push((volume_id.clone(), effective_size_bytes));
        self.status
    }
}

#[tokio::test]
async fn grow_composes_the_notifier_status_inside_the_journaled_outcome() {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new());
    let stub = Arc::new(StubGrowNotifier {
        calls: std::sync::Mutex::new(Vec::new()),
        status: volvisor_types::request::GrowGuestNotification::Notified,
    });
    let state = Arc::new(
        AppState::new(provider.clone(), None, journal, Some(TEST_TOKEN.to_owned()))
            .with_grow_notifier(Arc::clone(&stub) as Arc<dyn volvisor_provider::GrowNotifier>),
    );
    let app = app(&state);

    // Create and attach: an attached volume's provider placeholder is
    // retry_required (the honest no-VMM answer), so a `notified`
    // response can only come from the composed notifier.
    let create = serde_json::to_value(fixture_create_request("vol-compose", GIB))
        .expect("serialize create fixture");
    let (status, _) = send_json(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::OK);
    let attach = serde_json::to_value(fixture_attach_request("vol-compose", "att-compose", 1))
        .expect("serialize attach fixture");
    let (status, _) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes/vol-compose/attach", &attach),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let grow = serde_json::to_value(fixture_grow_request("vol-compose", 2 * GIB, 2))
        .expect("serialize grow fixture");
    let (status, body) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes/vol-compose/grow", &grow),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "grow: {body}");
    assert_eq!(
        body["guest_notification_status"],
        json!("notified"),
        "the composed status overrides the provider's retry_required placeholder"
    );
    assert_eq!(body["effective_size_bytes"], json!(2 * GIB));
    // The notifier ran inside the execute closure, after the
    // provider resized, and saw the effective size.
    assert_eq!(
        stub.calls.lock().expect("stub notifier lock").as_slice(),
        &[(
            volvisor_types::VolumeId::new("vol-compose").expect("valid volume id"),
            2 * GIB
        )]
    );

    // Replay byte-identically with the same operation id: the
    // journaled outcome already carries the composed status, so the
    // closure (and the notifier inside it) never re-runs.
    let (status, replay) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes/vol-compose/grow", &grow),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "replay: {replay}");
    assert_eq!(replay, body, "the replay is byte-identical");
    assert_eq!(
        stub.calls.lock().expect("stub notifier lock").len(),
        1,
        "the replay does not re-run the notifier"
    );
}

// ---------------------------------------------------------------------------
// The reserved operation-id namespace (F5 of PR #23's review): derived
// ids and consumer ids share the journal's id space, so the derived ids
// render in a reserved `derived:` prefix every consumer route refuses —
// a crafted pre-claim of a would-be derived id can never land, and the
// migration's own act can never be wedged by `IDEMPOTENCY_CONFLICT`.
// ---------------------------------------------------------------------------

/// State with every consumer route's surface wired (volume ops, admin
/// claim/release, lending, adoption, clear-cut-marker) — the table
/// test needs all thirteen consumer-supplied-id routes reachable.
fn setup_all_consumer_surfaces() -> (SharedState, Arc<FakeProvider>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new());
    let admin: Arc<dyn volvisor_provider::AdminSurface> = provider.clone();
    let lending: Arc<dyn volvisor_provider::LendingSurface> = provider.clone();
    let adoption = Arc::new(FakeAdoption::new());
    let handoff = Arc::new(FakeHandoffSurface::new());
    let state = Arc::new(
        AppState::new(
            provider.clone(),
            Some(admin),
            journal,
            Some(TEST_TOKEN.to_owned()),
        )
        .with_lending(lending)
        .with_adoption(adoption)
        .with_handoff(handoff),
    );
    (state, provider, dir)
}

/// The derivation is a pure function of the migration identity and the
/// act tag, and every derived id carries the reserved prefix — the
/// direction that makes the namespace unreachable, not just guarded.
#[test]
fn derived_operation_ids_render_in_the_reserved_namespace() {
    let tags = [
        "prepare",
        "transfer",
        "abort",
        "peer-prepare",
        "peer-grant",
        "peer-restore-vm",
        "peer-discard",
    ];
    for tag in tags {
        let id = ops::mobility_operation_id(&migration_id("mig-ns"), tag).expect("derived op id");
        let raw = id.as_str();
        assert!(raw.starts_with("derived:"), "reserved prefix: {raw}");
        assert!(raw.starts_with("derived:mig-api-"), "shaped body: {raw}");
        assert!(ops::is_derived_id(raw));
        // Determinism: the same inputs re-derive the same id — the
        // re-issue machinery's premise (a post-crash re-drive replays
        // the recorded outcome under the same id).
        let again = ops::mobility_operation_id(&migration_id("mig-ns"), tag).expect("re-derive");
        assert_eq!(again, id, "the derivation is pure");
    }
    // Different inputs derive different ids (a `prepare` and a
    // `transfer` of one migration are different operations).
    let prepare =
        ops::mobility_operation_id(&migration_id("mig-ns"), "prepare").expect("prepare id");
    let transfer =
        ops::mobility_operation_id(&migration_id("mig-ns"), "transfer").expect("transfer id");
    let other_migration = ops::mobility_operation_id(&migration_id("mig-other"), "prepare")
        .expect("other migration id");
    assert_ne!(prepare, transfer);
    assert_ne!(prepare, other_migration);
    // Ordinary consumer ids are not in the reserved namespace.
    assert!(!ops::is_derived_id("op-1"));
    assert!(!ops::is_derived_id("mig-api-prepare-deadbeefdeadbeef"));
}

/// Every consumer route that journals a consumer-supplied operation id
/// refuses a reserved-prefixed id typed, before any journal write —
/// and a later legitimate operation is not blocked by the refusal.
#[tokio::test]
async fn consumer_routes_refuse_reserved_operation_ids() {
    let (state, _provider, _dir) = setup_all_consumer_surfaces();
    let app = app(&state);
    let evil =
        ops::mobility_operation_id(&migration_id("mig-ns"), "peer-grant").expect("derived op id");
    assert!(ops::is_derived_id(evil.as_str()));

    // (uri, body) per consumer route, each body shape-valid so the
    // reserved-namespace guard is what fires — not request validation.
    let mut create =
        serde_json::to_value(fixture_create_request("vol-ns", GIB)).expect("create fixture");
    create["operation_id"] = json!(evil.as_str());
    let mut attach = serde_json::to_value(fixture_attach_request("vol-ns", "att-ns", 1))
        .expect("attach fixture");
    attach["operation_id"] = json!(evil.as_str());
    let mut detach =
        serde_json::to_value(fixture_detach_request("att-ns", 1)).expect("detach fixture");
    detach["operation_id"] = json!(evil.as_str());
    detach["attachment_id"] = json!("att-ns");
    let mut grow =
        serde_json::to_value(fixture_grow_request("vol-ns", 2 * GIB, 1)).expect("grow fixture");
    grow["operation_id"] = json!(evil.as_str());

    let routes: Vec<(Method, String, Value)> = vec![
        (Method::POST, "/v2/volumes".to_owned(), create),
        (Method::POST, "/v2/volumes/vol-ns/attach".to_owned(), attach),
        (Method::POST, "/v2/volumes/vol-ns/detach".to_owned(), detach),
        (Method::POST, "/v2/volumes/vol-ns/grow".to_owned(), grow),
        (
            Method::POST,
            "/v2/volumes/vol-ns/move-backing".to_owned(),
            move_request(evil.as_str(), "/dev/disk/by-id/wwn-0x5000c500target01", 1),
        ),
        (
            Method::DELETE,
            "/v2/volumes/vol-ns".to_owned(),
            serde_json::to_value(fixture_delete_request("vol-ns", 1)).expect("delete fixture"),
        ),
        (
            Method::POST,
            format!("/v2/admin/devices/{FAKE_DEVICE}/claim"),
            admin_request(evil.as_str(), "scoped"),
        ),
        (
            Method::POST,
            format!("/v2/admin/devices/{FAKE_DEVICE}/release"),
            admin_request(evil.as_str(), "scoped"),
        ),
        (
            Method::POST,
            format!("/v2/admin/devices/{FAKE_DEVICE}/lend"),
            json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": evil.as_str(),
                "borrower_cell_id": "cell-ns",
                "expected_claim_generation": 1,
            }),
        ),
        (
            Method::POST,
            format!("/v2/admin/devices/{FAKE_DEVICE}/reclaim"),
            json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": evil.as_str(),
                "expected_lend_generation": 1,
                "force": null,
            }),
        ),
        (
            Method::POST,
            format!("/v2/admin/devices/{FAKE_DEVICE}/resolve-quarantine"),
            json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": evil.as_str(),
                "operator_authorization": "operator-token",
            }),
        ),
        (
            Method::POST,
            "/v2/admin/nearline/vol-ns/adopt".to_owned(),
            adopt_body(evil.as_str(), false),
        ),
        (
            Method::POST,
            "/v2/admin/nearline/vol-ns/clear-cut-marker".to_owned(),
            clear_cut_body(evil.as_str()),
        ),
    ];
    for (method, uri, mut body) in routes {
        // The delete fixture has no operation_id field of its own to
        // override per-route; set it uniformly here.
        body["operation_id"] = json!(evil.as_str());
        let (status, refused) = send_json(&app, json_request(method, &uri, &body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {refused}");
        assert_eq!(refused["code"], json!("INVALID_REQUEST"), "{uri}");
        let message = refused["message"].as_str().unwrap_or_default();
        assert!(message.contains("reserved namespace"), "{uri}: {message}");
        assert!(message.contains("derived:"), "{uri}: {message}");
        // Nothing was journaled under the reserved id — the refusal
        // is pre-pipeline, so it cannot become the pre-claim record
        // the wedge needed.
        assert!(
            state
                .journal
                .lock()
                .expect("journal lock")
                .lookup(&evil)
                .is_none(),
            "{uri}: the refusal wrote nothing"
        );
    }

    // The attach route refuses a reserved attachment id too (the
    // promote attach's derived attachment identity lives in the
    // namespace; the provider keys attach idempotency by it).
    let mut attach_identity = serde_json::to_value(fixture_attach_request("vol-ns", "att-ns", 1))
        .expect("attach fixture");
    attach_identity["operation_id"] = json!("op-attach-ns");
    attach_identity["attachment_id"] = json!("derived:mig-attach-0123456789abcdef");
    let (status, refused) = send_json(
        &app,
        json_request(Method::POST, "/v2/volumes/vol-ns/attach", &attach_identity),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "attach identity: {refused}"
    );
    assert_eq!(refused["code"], json!("INVALID_REQUEST"));
    assert!(
        refused["message"]
            .as_str()
            .unwrap_or_default()
            .contains("reserved namespace"),
        "{refused}"
    );

    // A later legitimate operation sails through: nothing the
    // refusals did blocks the surface.
    let (status, created) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes",
            &serde_json::to_value(fixture_create_request("vol-after-ns", GIB))
                .expect("create fixture"),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "legitimate op after refusals: {created}"
    );
    assert_eq!(created["volume_id"], json!("vol-after-ns"));
}

/// The finding's exact repro, fixed: a consumer crafts the id a
/// migration's peer act will derive and tries to pre-register it with
/// a differing payload. The pre-claim cannot land (typed refusal,
/// nothing journaled), and the peer act itself succeeds — no
/// `IDEMPOTENCY_CONFLICT` is reachable.
#[tokio::test]
async fn a_crafted_pre_claim_cannot_wedge_a_peer_act() {
    let kit = setup_peer();
    let app = app(&kit.state);

    // The derived id of this migration's peer-grant act — exactly the
    // string an authenticated consumer would craft (the derivation is
    // deterministic from the consumer-chosen migration id).
    let evil =
        ops::mobility_operation_id(&migration_id("mig-f5"), "peer-grant").expect("derived op id");
    assert!(ops::is_derived_id(evil.as_str()));

    // The pre-claim attempt: a consumer op under the derived id is
    // refused typed before any journal write.
    let mut crafted =
        serde_json::to_value(fixture_create_request("vol-f5-claim", GIB)).expect("create fixture");
    crafted["operation_id"] = json!(evil.as_str());
    let (status, refused) =
        send_json(&app, json_request(Method::POST, "/v2/volumes", &crafted)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["code"], json!("INVALID_REQUEST"));
    assert!(
        kit.state
            .journal
            .lock()
            .expect("journal lock")
            .lookup(&evil)
            .is_none()
    );

    // The unprefixed twin is an ordinary consumer id: journaling it
    // proves the separation is the prefix, not the body — a consumer
    // id that merely looks like the derived body cannot collide.
    let twin = evil
        .as_str()
        .strip_prefix(ops::DERIVED_ID_PREFIX)
        .expect("the derived id carries the prefix");
    let mut twin_body =
        serde_json::to_value(fixture_create_request("vol-f5-twin", GIB)).expect("create fixture");
    twin_body["operation_id"] = json!(twin);
    let (status, created) =
        send_json(&app, json_request(Method::POST, "/v2/volumes", &twin_body)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the unprefixed twin is a consumer id: {created}"
    );
    assert_eq!(created["volume_id"], json!("vol-f5-twin"));

    // The peer act under its derived id succeeds — prepare, then the
    // grant that the pre-claim tried to wedge.
    create_volume(&kit, "vol-f5").await;
    let (status, _body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-f5", "vm-f5", &[("vol-f5", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-f5"}),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the peer act is unwedgeable: {body}"
    );
    assert_eq!(body["migration_id"], json!("mig-f5"));
    assert_eq!(body["grants"][0]["volume_id"], json!("vol-f5"));
    assert_eq!(
        body["grants"][0]["device_path"],
        json!("/dev/drbd-by-res/vol-f5")
    );
}

// ---------------------------------------------------------------------------
// MoveVolumeBackingOnline (contract section 4A): the route layer over the
// fail-closed trait default. The FakeProvider does not advertise
// `same_vg_extent_move`, so these pin the unqualified-provider refusal and
// the journal discipline around it; the qualified provider's own move
// semantics (the pvmove drive, its state machine and its fault rows) live
// in volvisor-lvm and the volvisord move e2e.
// ---------------------------------------------------------------------------

fn move_request(operation_id: &str, target: &str, generation: u64) -> Value {
    json!({
        "api_version": "volvisor.volume.v2",
        "operation_id": operation_id,
        "target_pool_id": target,
        "expected_generation": generation,
    })
}

/// An unqualified provider answers `MOVE_UNSUPPORTED_SCOPE` — the
/// typed scope refusal, journaled and replayed verbatim like every
/// Terminal-class volume-op failure, with the volume untouched.
#[tokio::test]
async fn move_refuses_unqualified_providers_typed() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let create = serde_json::to_value(fixture_create_request("vol-move-refuse", GIB))
        .expect("serialize create fixture");
    let (status, _) = send(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::OK);

    let move_req = move_request(
        "op-move-refuse",
        "/dev/disk/by-id/wwn-0x5000c500target01",
        1,
    );
    let (status, body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-move-refuse/move-backing",
            &move_req,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "move: {body}");
    assert_eq!(body["code"], json!("MOVE_UNSUPPORTED_SCOPE"));
    assert!(
        body["message"]
            .as_str()
            .expect("message")
            .contains("same_vg_extent_move"),
        "the refusal names the unadvertised capability: {body}"
    );

    // The refusal is journaled and replays byte-identically (the
    // Terminal failure-replay class), with no provider re-execution.
    let records_after_refusal = journal_record_count(&state);
    let (status, replay) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-move-refuse/move-backing",
            &move_req,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(replay, body, "the refusal replays verbatim");
    assert_eq!(journal_record_count(&state), records_after_refusal);

    // The volume is untouched by the refusal: same generation, no
    // state change (the extents were never a party to it).
    let (status, inspected) =
        send_json(&app, request(Method::GET, "/v2/volumes/vol-move-refuse")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inspected["generation"], json!(1));
}

/// Envelope rejections happen before the journal: no record, and the
/// `operation_id` stays reusable for a corrected request.
#[tokio::test]
async fn move_envelope_rejections_leave_no_journal_record() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let create = serde_json::to_value(fixture_create_request("vol-move-envelope", GIB))
        .expect("serialize create fixture");
    let (status, _) = send(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::OK);
    let records_after_create = journal_record_count(&state);

    let malformed = move_request("op-move-envelope", "   ", 1);
    let (status, body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-move-envelope/move-backing",
            &malformed,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "envelope: {body}");
    assert_eq!(body["code"], json!("INVALID_REQUEST"));
    assert_eq!(journal_record_count(&state), records_after_create);

    // The same operation_id, now with a well-formed envelope,
    // proceeds to the provider (and its typed refusal) — the
    // rejected attempt left nothing behind.
    let corrected = move_request(
        "op-move-envelope",
        "/dev/disk/by-id/wwn-0x5000c500target01",
        1,
    );
    let (status, body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-move-envelope/move-backing",
            &corrected,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], json!("MOVE_UNSUPPORTED_SCOPE"));
}

/// The same `operation_id` with a different payload is the immutable
/// wire-request conflict, exactly as on every volume op.
#[tokio::test]
async fn move_operation_id_reuse_with_a_different_payload_conflicts() {
    let (state, _provider, _dir) = setup();
    let app = app(&state);

    let create = serde_json::to_value(fixture_create_request("vol-move-conflict", GIB))
        .expect("serialize create fixture");
    let (status, _) = send(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::OK);

    let first = move_request(
        "op-move-conflict",
        "/dev/disk/by-id/wwn-0x5000c500target01",
        1,
    );
    let (status, _) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-move-conflict/move-backing",
            &first,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST); // the recorded refusal

    let second = move_request(
        "op-move-conflict",
        "/dev/disk/by-id/wwn-0x5000c500other02",
        1,
    );
    let (status, body) = send_json(
        &app,
        json_request(
            Method::POST,
            "/v2/volumes/vol-move-conflict/move-backing",
            &second,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "conflict: {body}");
    assert_eq!(body["code"], json!("IDEMPOTENCY_CONFLICT"));
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
// Scoped roles, read protection and rotation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scoped_credentials_enforce_the_role_matrix_and_protect_reads() {
    let (state, _provider, _dir) = setup_scoped();
    let app = app(&state);

    // Reads are closed to anonymous callers once credentials are scoped.
    for uri in ["/v2/volumes", "/v2/capabilities"] {
        let (status, unauthorized) = send_json(&app, request(Method::GET, uri)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "anonymous {uri}");
        assert_eq!(unauthorized["code"], json!("UNAUTHORIZED"));
    }
    let (status, _) = send(&app, request(Method::GET, "/metrics")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Read-only, operator and admin can all read.
    for token in [READ_ONLY_TOKEN, OPERATOR_TOKEN, ADMIN_TOKEN] {
        let (status, _) = send_json(
            &app,
            with_bearer(request(Method::GET, "/v2/volumes"), token),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "read with {token}");
        let (status, _) = send_json(
            &app,
            with_bearer(request(Method::GET, "/v2/capabilities"), token),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "capabilities with {token}");
        let (status, _) = send(&app, with_bearer(request(Method::GET, "/metrics"), token)).await;
        assert_eq!(status, StatusCode::OK, "metrics with {token}");
    }

    // Liveness stays open for probes.
    let (status, health) = send_json(&app, request(Method::GET, "/healthz")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health, json!({ "status": "ok" }));

    let create = serde_json::to_value(fixture_create_request("vol-role", GIB))
        .expect("serialize create fixture");

    // Read-only cannot mutate; the denial is a typed 403 and never journals.
    let (status, forbidden) = send_json(
        &app,
        with_bearer(
            json_request_without_auth(Method::POST, "/v2/volumes", &create),
            READ_ONLY_TOKEN,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(forbidden["code"], json!("FORBIDDEN"));
    assert_eq!(journal_record_count(&state), 0);

    // Operator can mutate consumer volumes.
    let (status, created) = send_json(
        &app,
        with_bearer(
            json_request_without_auth(Method::POST, "/v2/volumes", &create),
            OPERATOR_TOKEN,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["volume_id"], json!("vol-role"));

    // Operator is denied the admin surface; admin is allowed.
    let (status, forbidden) = send_json(
        &app,
        with_bearer(request(Method::GET, "/v2/admin/devices"), OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(forbidden["code"], json!("FORBIDDEN"));
    let (status, _) = send_json(
        &app,
        with_bearer(request(Method::GET, "/v2/admin/devices"), ADMIN_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The admin-only mobility eligibility check is denied to operator
    // principals before the handler runs (the handler itself serves 404
    // without a handoff surface).
    let (status, _) = send_json(
        &app,
        with_bearer(
            json_request_without_auth(
                Method::POST,
                "/v2/vms/vm-role/check-mobility",
                &json!({"api_version": "volvisor.volume.v2", "operation_id": "op-mobility"}),
            ),
            OPERATOR_TOKEN,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rotated_token_is_rejected_and_the_new_token_is_accepted() {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let old = vec![Credential {
        id: "operator-a".to_owned(),
        token: "old-rotation-token".to_owned(),
        role: Role::Operator,
    }];
    {
        let (state, _provider) = setup_scoped_in(&dir, old);
        let app = app(&state);
        let (status, _) = send_json(
            &app,
            with_bearer(request(Method::GET, "/v2/volumes"), "old-rotation-token"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send_json(
            &app,
            with_bearer(request(Method::GET, "/v2/volumes"), "new-rotation-token"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // Restart over the same journal directory with the rotated credential.
    let (state, _provider) = setup_scoped_in(
        &dir,
        vec![Credential {
            id: "operator-a".to_owned(),
            token: "new-rotation-token".to_owned(),
            role: Role::Operator,
        }],
    );
    let app = app(&state);
    let (status, _) = send_json(
        &app,
        with_bearer(request(Method::GET, "/v2/volumes"), "old-rotation-token"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send_json(
        &app,
        with_bearer(request(Method::GET, "/v2/volumes"), "new-rotation-token"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[derive(Clone, Default)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl CaptureWriter {
    fn snapshot(&self) -> String {
        let bytes = self.0.lock().expect("capture lock").clone();
        String::from_utf8(bytes).expect("captured logs are UTF-8")
    }

    fn clear(&self) {
        self.0.lock().expect("capture lock").clear();
    }
}

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Install one process-wide capture subscriber for the audit test. A
/// thread-local subscriber is unreliable here: the test harness runs other
/// tests concurrently and `tracing`'s callsite interest is process-wide.
fn captured_audit_log() -> CaptureWriter {
    static CAPTURE: OnceLock<CaptureWriter> = OnceLock::new();
    static INSTALL: Once = Once::new();
    let writer = CAPTURE.get_or_init(CaptureWriter::default).clone();
    INSTALL.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(writer.clone())
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
    writer
}

/// The audit event is the security record: it must name the resolved
/// principal/role, method, matched route, outcome and journaled operation id,
/// and must never contain bearer tokens.
#[tokio::test]
async fn audit_events_name_the_principal_route_and_operation_without_tokens() {
    let writer = captured_audit_log();
    writer.clear();

    let (state, _provider, _dir) = setup_scoped();
    let app = app(&state);
    let mut create = serde_json::to_value(fixture_create_request("vol-audit", GIB))
        .expect("serialize create fixture");
    create["operation_id"] = json!("op-audit-create");
    let (status, _) = send_json(
        &app,
        with_bearer(
            json_request_without_auth(Method::POST, "/v2/volumes", &create),
            OPERATOR_TOKEN,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A denied mutation is audited too, with the read-only principal.
    let (status, _) = send_json(
        &app,
        with_bearer(
            json_request_without_auth(Method::POST, "/v2/volumes", &create),
            READ_ONLY_TOKEN,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let logs = writer.snapshot();
    let events: Vec<Value> = logs
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["fields"]["kind"] == json!("audit"))
        .filter(|event| {
            event["fields"]["operation_id"] == json!("op-audit-create")
                || event["fields"]["principal"] == json!("reader-a")
        })
        .collect();
    assert_eq!(events.len(), 2, "one event per request: {logs}");

    let success = events
        .iter()
        .find(|event| event["fields"]["outcome"] == json!("success"))
        .expect("successful mutation audit");
    assert_eq!(success["fields"]["principal"], json!("operator-a"));
    assert_eq!(success["fields"]["role"], json!("operator"));
    assert_eq!(success["fields"]["method"], json!("POST"));
    assert_eq!(success["fields"]["route"], json!("/v2/volumes"));
    assert_eq!(success["fields"]["status"], json!(200));
    assert_eq!(success["fields"]["operation_id"], json!("op-audit-create"));

    let denied = events
        .iter()
        .find(|event| event["fields"]["outcome"] == json!("denied"))
        .expect("denied mutation audit");
    assert_eq!(denied["fields"]["principal"], json!("reader-a"));
    assert_eq!(denied["fields"]["role"], json!("read-only"));
    assert_eq!(denied["fields"]["method"], json!("POST"));
    assert_eq!(denied["fields"]["route"], json!("/v2/volumes"));
    assert_eq!(denied["fields"]["status"], json!(403));

    for token in [ADMIN_TOKEN, OPERATOR_TOKEN, READ_ONLY_TOKEN] {
        assert!(
            !logs.contains(token),
            "audit logs must never contain bearer token {token}"
        );
    }
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
/// file; the redaction marker does. Covers all three credential families —
/// the volume surface's `key_ref`, the admin surface's
/// `authorization_token`, and the lending surface's (force-reclaim)
/// `operator_authorization`, nested inside the force directive.
#[tokio::test]
async fn journaled_intents_redact_secrets() {
    let (state, _provider, dir) = setup_with_lending();
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

    // A lending op carrying the scoped operator token, nested inside
    // the force directive (the redaction walk must reach it at
    // depth). This fixture's provider has no durable device world,
    // so the surface answers its typed 404 — but the intent is
    // journaled *before* execution, which is what the redaction must
    // hold for.
    let force_reclaim = json!({
        "api_version": "volvisor.volume.v2",
        "operation_id": "op-force-reclaim-redact",
        "expected_lend_generation": 1,
        "force": {
            "operator_authorization": "super-secret-operator-token",
            "residue_check": "inspect_and_record",
        },
    });
    let (status, _) = send_json(
        &app,
        json_request(
            Method::POST,
            &format!("/v2/admin/devices/{FAKE_DEVICE}/reclaim"),
            &force_reclaim,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "no lending surface on this fixture"
    );

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
        !log.contains("super-secret-operator-token"),
        "operator_authorization leaked into the journal"
    );
    assert!(
        log.contains("[redacted]"),
        "redaction marker must be journaled"
    );
    assert!(
        log.contains("vol-redact-e2e"),
        "non-secret identity is preserved"
    );
    assert!(
        log.contains("op-force-reclaim-redact"),
        "the lending op's identity is preserved"
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
        cut_duration_secs: None,
        barrier_lineage_refusal: None,
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

    /// Every replica verification the surface saw, in order (the
    /// assertion input for the lineage re-verification route).
    fn target_check_list(&self) -> Vec<String> {
        self.target_checks
            .lock()
            .expect("target checks")
            .iter()
            .map(|volume| volume.as_str().to_owned())
            .collect()
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
        attachments: Vec::new(),
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
        _expected_lineage: &[String],
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

    async fn source_lineage(
        &self,
        volume_id: &volvisor_types::VolumeId,
    ) -> Result<Vec<String>, ApiError> {
        Ok(vec![format!("fake-lineage-{}", volume_id.as_str())])
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
    PeerGrantRequest, PeerRouteContext, PreparedParticipant, TARGET_PREPARATION_SCHEMA_VERSION,
    TargetPreparation, TargetPreparationStore,
};

/// One volume's scripted witness authority state.
#[derive(Clone, Debug)]
struct FakeWitnessVolume {
    epoch: u64,
    lease_id: u64,
    lease_state: volvisor_types::LeaseState,
    holder: Option<volvisor_types::HostId>,
    /// The scripted `lease_remaining_secs` the inspect view reports
    /// (the grant-hold renewal's due rule reads it).
    remaining_secs: u64,
}

/// A scripted witness for the peer routes: `inspect` answers the
/// scripted views, `grant_set` mints epochs for this host and records
/// every call (idempotent per operation id, like the real journal);
/// `renew` records every call and answers the scripted remaining
/// duration (the grant-hold renewal pass); everything else refuses
/// typed (unused by these routes).
struct FakePeerWitness {
    host: volvisor_types::HostId,
    volumes: std::sync::Mutex<BTreeMap<volvisor_types::VolumeId, FakeWitnessVolume>>,
    registrations:
        std::sync::Mutex<BTreeMap<volvisor_types::VolumeId, volvisor_types::VolumeRegistration>>,
    grant_set_calls: std::sync::Mutex<Vec<GrantSetRequest>>,
    recorded_grants: std::sync::Mutex<BTreeMap<volvisor_types::OperationId, GrantSetResponse>>,
    grant_set_refusals: std::sync::Mutex<std::collections::VecDeque<WitnessError>>,
    renew_calls: std::sync::Mutex<
        Vec<(
            volvisor_types::VolumeId,
            volvisor_witness::proto::RenewRequest,
        )>,
    >,
    renew_refusals: std::sync::Mutex<std::collections::VecDeque<WitnessError>>,
    renew_remaining_secs: std::sync::Mutex<u64>,
    barriers: std::sync::Mutex<
        BTreeMap<volvisor_types::VolumeId, Vec<volvisor_types::RecordedMigrationBarrier>>,
    >,
    retirements:
        std::sync::Mutex<BTreeMap<volvisor_types::VolumeId, Vec<volvisor_types::EpochRetirement>>>,
}

/// A plausible registration for a scripted volume: one lineage UUID
/// and one endpoint per host pair. The peer routes only read the
/// lineage set (the prepare gate's expected input); the fake handoff
/// surface ignores it, so a stable synthetic value suffices.
fn synthetic_registration(
    volume: &volvisor_types::VolumeId,
    host: &volvisor_types::HostId,
) -> volvisor_types::VolumeRegistration {
    volvisor_types::VolumeRegistration {
        volume_id: volume.clone(),
        lineage_uuids: vec![format!("fake-lineage-{}", volume.as_str())],
        endpoints: vec![volvisor_types::EndpointBacking {
            host_id: host.clone(),
            backing: format!("fake-backing-{}", volume.as_str()),
            volvisor_created: false,
        }],
        barrier: None,
        registered_at: 0,
    }
}

impl FakePeerWitness {
    fn new(host: volvisor_types::HostId) -> Self {
        Self {
            host,
            volumes: std::sync::Mutex::new(BTreeMap::new()),
            registrations: std::sync::Mutex::new(BTreeMap::new()),
            grant_set_calls: std::sync::Mutex::new(Vec::new()),
            recorded_grants: std::sync::Mutex::new(BTreeMap::new()),
            grant_set_refusals: std::sync::Mutex::new(std::collections::VecDeque::new()),
            renew_calls: std::sync::Mutex::new(Vec::new()),
            renew_refusals: std::sync::Mutex::new(std::collections::VecDeque::new()),
            renew_remaining_secs: std::sync::Mutex::new(100),
            barriers: std::sync::Mutex::new(BTreeMap::new()),
            retirements: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Queue one refusal the next `grant_set` call serves and spends
    /// (the wedge model: the transport death B's grant act sees when
    /// the witness is killed inside the grant commit — the failure
    /// B's route then journals).
    fn refuse_next_grant_set(&self, error: WitnessError) {
        self.grant_set_refusals
            .lock()
            .expect("grant-set refusals")
            .push_back(error);
    }

    /// Script a registration without a lease (the pre-grant shape:
    /// the prepare gate reads the registration's lineage; the grant
    /// route's inspection later needs `set_live`).
    fn set_registered(&self, volume: &volvisor_types::VolumeId) {
        self.registrations
            .lock()
            .expect("registrations")
            .insert(volume.clone(), synthetic_registration(volume, &self.host));
    }

    /// Script a live lease held by `host` at `epoch` (the grant-act
    /// inspection's proven-landed arrangement). The volume also
    /// becomes REGISTERED (the prepare act's lineage gate reads the
    /// registration; the fake handoff surface ignores its content).
    fn set_live(&self, volume: &volvisor_types::VolumeId, epoch: u64, lease_id: u64) {
        self.set_volume(
            volume,
            epoch,
            lease_id,
            volvisor_types::LeaseState::Live,
            30,
        );
    }

    /// Script a live lease with an explicit remaining duration (the
    /// grant-hold renewal's due rule reads it: a lease within the
    /// renewal interval of its end is due).
    fn set_live_with_remaining(
        &self,
        volume: &volvisor_types::VolumeId,
        epoch: u64,
        lease_id: u64,
        remaining_secs: u64,
    ) {
        self.set_volume(
            volume,
            epoch,
            lease_id,
            volvisor_types::LeaseState::Live,
            remaining_secs,
        );
    }

    /// Script the beyond-TTL shape: a lease this host held that
    /// lapsed at the witness (state `Expired`, holder unchanged) —
    /// the fresh-grant gates' input.
    fn set_expired(&self, volume: &volvisor_types::VolumeId, epoch: u64, lease_id: u64) {
        self.set_volume(
            volume,
            epoch,
            lease_id,
            volvisor_types::LeaseState::Expired,
            0,
        );
    }

    /// Script the volume's lease as revoked while still recorded
    /// held by this host (a deliberate end — an abort's fail-closed
    /// release, row 12b's shape; gate (a)'s revoked arm's input).
    fn set_revoked(&self, volume: &volvisor_types::VolumeId, epoch: u64, lease_id: u64) {
        self.set_volume(
            volume,
            epoch,
            lease_id,
            volvisor_types::LeaseState::Revoked,
            0,
        );
    }

    /// Script the volume state (the shared tail of the setters above).
    fn set_volume(
        &self,
        volume: &volvisor_types::VolumeId,
        epoch: u64,
        lease_id: u64,
        lease_state: volvisor_types::LeaseState,
        remaining_secs: u64,
    ) {
        self.volumes.lock().expect("volumes").insert(
            volume.clone(),
            FakeWitnessVolume {
                epoch,
                lease_id,
                lease_state,
                holder: Some(self.host.clone()),
                remaining_secs,
            },
        );
        self.registrations
            .lock()
            .expect("registrations")
            .entry(volume.clone())
            .or_insert_with(|| synthetic_registration(volume, &self.host));
    }

    /// Script one non-voided migration barrier at `epoch` naming
    /// `migration` (the cut's fence evidence — gate (c)'s barrier
    /// arm; the attestation is all-true, the recorded shape).
    fn set_barrier(
        &self,
        volume: &volvisor_types::VolumeId,
        migration: &volvisor_types::MigrationId,
        epoch: u64,
    ) {
        self.barriers
            .lock()
            .expect("barriers")
            .entry(volume.clone())
            .or_default()
            .push(volvisor_types::RecordedMigrationBarrier {
                holder: host_id("src-host"),
                epoch: volvisor_types::WriterEpoch(epoch),
                boundary_commit_index: 1,
                attestation: volvisor_types::BarrierAttestation {
                    vm_paused_and_drained: true,
                    data_path_suspended: true,
                    peer_up_to_date: true,
                },
                migration_id: Some(migration.clone()),
                recorded_at: 1,
                voided: false,
            });
    }

    /// Script one durably retired epoch (gate (c)'s retirement arm —
    /// the shape a `revoke_set` leaves).
    fn set_retired(&self, volume: &volvisor_types::VolumeId, epoch: u64) {
        self.retirements
            .lock()
            .expect("retirements")
            .entry(volume.clone())
            .or_default()
            .push(volvisor_types::EpochRetirement {
                epoch: volvisor_types::WriterEpoch(epoch),
                commit_index: 1,
            });
    }

    /// Queue one refusal the next `renew` call serves and spends
    /// (the renewal pass's witness-unreachable deferral).
    fn refuse_next_renew(&self, error: WitnessError) {
        self.renew_refusals
            .lock()
            .expect("renew refusals")
            .push_back(error);
    }

    /// Script the remaining duration a scripted `renew` answers with.
    fn set_renew_remaining(&self, remaining_secs: u64) {
        *self.renew_remaining_secs.lock().expect("renew remaining") = remaining_secs;
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
            lease_remaining_secs: entry.map(|v| v.remaining_secs),
            commit_index: 1,
            registration: self
                .registrations
                .lock()
                .expect("registrations")
                .get(volume)
                .cloned(),
            barriers: self
                .barriers
                .lock()
                .expect("barriers")
                .get(volume)
                .cloned()
                .unwrap_or_default(),
            retirements: self
                .retirements
                .lock()
                .expect("retirements")
                .get(volume)
                .cloned()
                .unwrap_or_default(),
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
        volume_id: &volvisor_types::VolumeId,
        request: RenewRequest,
    ) -> Result<RenewResponse, WitnessError> {
        // The scripted-refusal knob (the witness-unreachable
        // deferral): a queued WitnessError is served and spent before
        // the scripted-success path.
        if let Some(error) = self
            .renew_refusals
            .lock()
            .expect("renew refusals")
            .pop_front()
        {
            return Err(error);
        }
        let epoch = volvisor_types::WriterEpoch(
            self.volumes
                .lock()
                .expect("volumes")
                .get(volume_id)
                .map_or(0, |volume| volume.epoch),
        );
        self.renew_calls
            .lock()
            .expect("renew calls")
            .push((volume_id.clone(), request));
        Ok(RenewResponse {
            epoch,
            remaining_secs: *self.renew_remaining_secs.lock().expect("renew remaining"),
        })
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
        // The scripted-refusal knob (the wedge model): a queued
        // WitnessError is served and spent before the recorded-grant
        // path, modeling the transport death the destination's grant
        // act sees when the witness is killed inside the grant commit.
        if let Some(error) = self
            .grant_set_refusals
            .lock()
            .expect("grant-set refusals")
            .pop_front()
        {
            self.grant_set_calls
                .lock()
                .expect("grant-set calls")
                .push(request);
            return Err(error);
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
                        remaining_secs: 30,
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
        // The real registry refuses an unknown volume typed; the fake
        // scripts the same refusal so the routes' error mapping (and
        // the prepare gate's registration read) behaves identically.
        let volumes = self.volumes.lock().expect("volumes");
        let registrations = self.registrations.lock().expect("registrations");
        if !volumes.contains_key(volume) && !registrations.contains_key(volume) {
            return Err(WitnessError::UnknownVolume);
        }
        drop(registrations);
        drop(volumes);
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

    fn resize_disk(&self, vm_id: &str, disk_id: &str, new_size_bytes: u64) -> Result<(), ApiError> {
        self.record("resize_disk");
        self.inner.resize_disk(vm_id, disk_id, new_size_bytes)
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
    context: Arc<PeerRouteContext>,
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
            .with_peer_routes(Some(PEER_TOKEN.to_owned()), context.clone()),
    );
    PeerKit {
        state,
        witness,
        handoff,
        vmm,
        fake_vmm,
        provider,
        store,
        context,
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

#[tokio::test]
async fn peer_and_anonymous_traffic_have_separate_rate_limit_buckets() {
    // The internal peer routes are authorized by `RequirePeer`, not an
    // `AuthContext`. Without the peer-principal marker, peer traffic would
    // share the anonymous bucket with unauthenticated callers and an
    // attacker could drain the source daemon's migration calls.
    let kit = setup_peer();
    let config = ApiConfig {
        rate_limit_per_second: 1,
        rate_limit_burst: 1,
        ..ApiConfig::default()
    };
    let app = router_with_config(kit.state.clone(), &config);
    let uri = "/v2/internal/peer/health";

    let (ok, body) = send_json(&app, peer_request(Method::GET, uri, &json!({}))).await;
    assert_eq!(ok, StatusCode::OK, "{body}");
    let (limited, limited_body) = send_json(&app, peer_request(Method::GET, uri, &json!({}))).await;
    assert_eq!(limited, StatusCode::TOO_MANY_REQUESTS, "{limited_body}");
    assert_eq!(limited_body["code"], json!("RATE_LIMITED"));

    // An unauthenticated peer call is charged to its own anonymous bucket,
    // not to the peer bucket: the first is a 401 from `RequirePeer`, the
    // second is shed before the handler.
    let bad_peer = |token: &str| {
        with_bearer(
            json_request_without_auth(Method::GET, uri, &json!({})),
            token,
        )
    };
    let (unauthorized, unauthorized_body) = send_json(&app, bad_peer("wrong-peer-token")).await;
    assert_eq!(
        unauthorized,
        StatusCode::UNAUTHORIZED,
        "{unauthorized_body}"
    );
    let (shed, shed_body) = send_json(&app, bad_peer("wrong-peer-token")).await;
    assert_eq!(shed, StatusCode::TOO_MANY_REQUESTS, "{shed_body}");
    assert_eq!(shed_body["code"], json!("RATE_LIMITED"));

    // The legacy admin identity still has an independent bucket.
    let (admin, admin_body) = send_json(
        &app,
        with_bearer(
            json_request_without_auth(Method::GET, "/v2/volumes", &json!({})),
            TEST_TOKEN,
        ),
    )
    .await;
    assert_eq!(admin, StatusCode::OK, "{admin_body}");
}

fn peer_prepare_body(migration: &str, vm: &str, volumes: &[(&str, u64)]) -> Value {
    json!({
        "migration_id": migration,
        "vm_id": vm,
        "source_host": "src-host",
        "volume_ids": volumes.iter().map(|(v, _)| *v).collect::<Vec<_>>(),
        "expected_generations": volumes.iter().map(|(_, g)| *g).collect::<Vec<_>>(),
        // The source's live lineage per participant (P5 §5.2): a
        // stable synthetic set — the fake handoff surface ignores the
        // content; the shape (non-empty, aligned) is what the
        // validation pins.
        "expected_lineages": volumes
            .iter()
            .map(|(v, _)| vec![format!("fake-lineage-{v}")])
            .collect::<Vec<_>>(),
    })
}

async fn create_volume(kit: &PeerKit, raw: &str) {
    kit.provider
        .create_volume(&fixture_create_request(raw, GIB))
        .await
        .expect("create volume");
    // The fake provider's volumes model this host's established
    // replicas (the happy-path peer tests' destination); the surface's
    // replica-level gate must see them as targets, and the witness
    // must hold their registration (the prepare gate's lineage input).
    kit.handoff.add_target(&volume_id(raw));
    kit.witness.set_registered(&volume_id(raw));
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
async fn peer_prepare_refuses_a_non_empty_destination_vmm() {
    // The restore act's destroy-first re-drive (row 18) is only sound
    // for a socket proven empty at PREPARED: a VM squatting the
    // destination socket is refused typed here — before the source's
    // cut — and never destroyed.
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-p9").await;

    kit.fake_vmm
        .create("vm-p9", &["/dev/fake/vol-p9"])
        .expect("create the squatted VM");
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-p9", "vm-p9", &[("vol-p9", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], json!("INVALID_STATE"));
    assert!(
        body["message"]
            .as_str()
            .expect("message")
            .contains("not empty"),
        "the error names the rule: {body}"
    );
    // The squatted VM is untouched.
    assert_eq!(
        kit.fake_vmm.vm_state("vm-p9").expect("state"),
        volvisor_provider::VmState::Created
    );
}

#[tokio::test]
async fn peer_verify_lineage_re_runs_the_replica_gate_read_only() {
    // P6-A F1 (the barrier-time lineage re-check): the route re-runs
    // exactly the replica-level gate prepare ran, over the source's
    // freshly-read expected set — an observation, never journaled,
    // mutating nothing (no preparation record, no idempotency
    // machinery).
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-vl1").await;
    create_volume(&kit, "vol-vl2").await;
    let before = journal_record_count(&kit.state);

    // Misaligned lists are the typed validation refusal.
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/verify-lineage",
            &json!({
                "migration_id": "mig-vl",
                "volume_ids": ["vol-vl1", "vol-vl2"],
                "expected_lineages": [["fake-lineage-vol-vl1"]],
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], json!("INVALID_REQUEST"));

    // A volume this host holds no replica of is the gate's typed
    // refusal (first failure refuses the whole set).
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/verify-lineage",
            &json!({
                "migration_id": "mig-vl",
                "volume_ids": ["vol-vl1", "vol-nope"],
                "expected_lineages": [
                    ["fake-lineage-vol-vl1"],
                    ["fake-lineage-vol-nope"],
                ],
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], json!("NOT_FOUND"));

    // The honest re-verification passes and echoes the verified set
    // in preparation order.
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/verify-lineage",
            &json!({
                "migration_id": "mig-vl",
                "volume_ids": ["vol-vl1", "vol-vl2"],
                "expected_lineages": [
                    ["fake-lineage-vol-vl1"],
                    ["fake-lineage-vol-vl2"],
                ],
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["migration_id"], json!("mig-vl"));
    assert_eq!(
        body["volume_ids"],
        json!(["vol-vl1", "vol-vl2"]),
        "the verified set is echoed in preparation order"
    );

    // The gate really re-ran over the participants (the surface saw
    // every check, refused ones included) — and the route journaled
    // nothing (an observation, never a mutation).
    let checks = kit.handoff.target_check_list();
    assert_eq!(
        checks,
        ["vol-vl1", "vol-nope", "vol-vl1", "vol-vl2"],
        "each attempt re-runs the gate per participant, first failure refuses"
    );
    assert_eq!(
        journal_record_count(&kit.state),
        before,
        "the observation journals nothing"
    );
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
        // string (distinct namespaces, one derivation), in the
        // reserved namespace the consumer attach route refuses.
        assert_eq!(attach.operation_id.as_str(), attach.attachment_id.as_str());
        assert!(
            attach
                .operation_id
                .as_str()
                .starts_with("derived:mig-attach-")
        );
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

/// Inject an intent-with-a-recorded-**failure** for a peer route's
/// derived journal operation id — exactly like the recorded
/// `grant_set` wedge (P6-A part 3): the act ran, its witness call
/// died with the transport class, and the pipeline's failure tail
/// journaled the error body under the derived id. The distinctive
/// detail lets tests prove the stale failure is never re-served.
fn inject_peer_failure(
    kit: &PeerKit,
    migration: &volvisor_types::MigrationId,
    tag: &str,
    op_kind: &'static str,
    body: &Value,
) {
    inject_peer_intent(kit, migration, tag, op_kind, body);
    let operation_id = ops::mobility_operation_id(migration, tag).expect("derived op id");
    let error = ApiError::new(
        ApiErrorCode::Internal,
        "witness unreachable: the scripted transport death (the recorded wedge failure)",
    );
    let mut journal = kit.state.journal.lock().expect("journal lock in test");
    journal
        .append_outcome(
            operation_id,
            false,
            serde_json::to_value(ApiErrorBody::from(error)).expect("serialize error body"),
        )
        .expect("append injected failure outcome");
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
            schema_version: TARGET_PREPARATION_SCHEMA_VERSION,
            migration_id: migration.clone(),
            vm_id: "vm-r".to_owned(),
            source_host: host_id("src-host"),
            target_host: kit.host.clone(),
            participants: vec![PreparedParticipant {
                volume_id: volume_id("vol-r1"),
                expected_generation: 1,
                device_path: Some("/dev/drbd-by-res/vol-r1".to_owned()),
            }],
            grant_attempt: 0,
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

/// The `grant_set` wedge fix (P6-A part 3), re-execution flavor: a
/// recorded failure of a re-issuable peer act is never a terminal
/// replay. The first grant call dies at the witness batch (the
/// transport class the real wedge journals under the derived id —
/// the act died before any promote or device-path recording). The
/// re-issue — the shape of every re-drive — re-evaluates through the
/// inspection: nothing is provable (no lease, no promoted device
/// paths), so the act re-executes under its idempotency discipline
/// and succeeds; the success supersedes the recorded failure, and a
/// later retry replays it byte-for-byte without executing.
#[tokio::test]
async fn peer_grant_reissues_a_recorded_failure_when_the_act_did_not_land() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-w1").await;

    // The preparation must exist (the grant act loads it; the first
    // call's failure is the scripted witness death, not a missing
    // preparation).
    let (status, _body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-wedge1", "vm-w", &[("vol-w1", 1)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The witness dies inside the grant commit: the first call fails
    // at the witness batch (the transport class), and the pipeline
    // journals the failure under the derived operation id.
    kit.witness.refuse_next_grant_set(WitnessError::Unreachable(
        "the witness was killed inside the grant commit (scripted)".to_owned(),
    ));
    let (status, first_body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-wedge1"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{first_body}");
    assert_eq!(first_body["code"], json!("INTERNAL"));
    assert_eq!(
        kit.handoff.promotes.lock().expect("promotes").len(),
        0,
        "the act died at the witness batch, before any promote"
    );

    // The re-issue: the inspection proves nothing landed, the act
    // re-executes — the witness batch again, the promote, the device
    // paths recorded — and its success supersedes the failure.
    let (status, second_body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-wedge1"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second_body}");
    assert_eq!(
        second_body["grants"][0]["device_path"],
        json!("/dev/drbd-by-res/vol-w1")
    );
    assert_eq!(
        kit.witness.grant_set_calls.lock().expect("calls").len(),
        2,
        "the re-issue re-executed the witness batch — the recorded \
         failure is not a terminal replay"
    );
    assert_eq!(kit.handoff.promotes.lock().expect("promotes").len(), 1);

    // The superseding success is the recorded outcome now: a later
    // retry replays it byte-for-byte without executing.
    let (status, third_body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-wedge1"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(third_body, second_body);
    assert_eq!(
        kit.witness.grant_set_calls.lock().expect("calls").len(),
        2,
        "the recorded success replays without re-executing"
    );
}

/// The `grant_set` wedge fix, proven-landed flavor: when the world
/// converged past the recorded failure (the witness's own replay
/// landed the grant — a live lease held by this host — and the
/// promoted device paths are on record), the re-issue serves the
/// proven outcome and never re-calls the witness batch. The
/// resolution supersedes the failure.
#[tokio::test]
async fn peer_grant_reissues_a_recorded_failure_when_the_act_provably_landed() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-w2").await;

    // The proven-landed arrangement (the world the witness's own
    // journal replay produces): the lease is live at this host and the
    // promoted device path is durably recorded in the preparation.
    let migration = migration_id("mig-wedge2");
    kit.store
        .install(&TargetPreparation {
            schema_version: TARGET_PREPARATION_SCHEMA_VERSION,
            migration_id: migration.clone(),
            vm_id: "vm-w2".to_owned(),
            source_host: host_id("src-host"),
            target_host: kit.host.clone(),
            participants: vec![PreparedParticipant {
                volume_id: volume_id("vol-w2"),
                expected_generation: 1,
                device_path: Some("/dev/drbd-by-res/vol-w2".to_owned()),
            }],
            grant_attempt: 0,
            created_at: 1,
        })
        .expect("install preparation");
    kit.witness.set_live(&volume_id("vol-w2"), 2, 5);
    inject_peer_failure(
        &kit,
        &migration,
        "peer-grant",
        ops::OP_PEER_GRANT,
        &serde_json::to_value(PeerGrantRequest {
            migration_id: migration.clone(),
        })
        .expect("serialize"),
    );

    // The re-issue resolves by inspection: the proven outcome is
    // served (and journaled, superseding the failure) without the
    // witness batch ever being re-called.
    let (status, first_body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-wedge2"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first_body}");
    assert_eq!(
        first_body["grants"][0]["device_path"],
        json!("/dev/drbd-by-res/vol-w2")
    );
    assert_eq!(
        kit.witness.grant_set_calls.lock().expect("calls").len(),
        0,
        "the inspection proved the act landed; the witness batch was \
         never re-called"
    );
    assert_eq!(kit.handoff.promotes.lock().expect("promotes").len(), 0);

    // The resolution is the recorded outcome now: a later retry
    // replays it byte-for-byte.
    let (status, second_body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-wedge2"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second_body, first_body);
    assert_eq!(
        kit.witness.grant_set_calls.lock().expect("calls").len(),
        0,
        "the resolution replays without re-executing"
    );
}

/// The `grant_set` wedge fix, fail-closed flavor: when the inspection
/// itself errors typed (the preparation is absent), the re-issue
/// surfaces that typed error — the stale recorded failure is never
/// re-served as a terminal answer.
#[tokio::test]
async fn peer_grant_surfaces_the_inspection_error_not_the_stale_recorded_failure() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-w3").await;

    let migration = migration_id("mig-wedge3");
    inject_peer_failure(
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
            &json!({"migration_id": "mig-wedge3"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], json!("NOT_FOUND"));
    let message = body["message"].as_str().expect("message");
    assert!(
        message.contains("no target preparation"),
        "the inspection's typed error is served: {message}"
    );
    assert!(
        !message.contains("the recorded wedge failure"),
        "the stale recorded failure is never re-served: {message}"
    );
}

// ---------------------------------------------------------------------------
// The convergence horizon: the gated fresh-grant re-mint and the
// grant-hold renewal (the diagnosis doc's horizon section)
// ---------------------------------------------------------------------------

/// Arrange the beyond-TTL park directly (the world a witness outage
/// past the lease TTL leaves): a pre-promote preparation whose
/// recorded grant lapsed at the witness — state `Expired`, still held
/// by this host — with the cut's fence evidence intact below the
/// granted epoch.
async fn arrange_lapsed_park(kit: &PeerKit, raw_volume: &str, migration_raw: &str) {
    create_volume(kit, raw_volume).await;
    let volume = volume_id(raw_volume);
    let migration = migration_id(migration_raw);
    kit.store
        .install(&TargetPreparation {
            schema_version: TARGET_PREPARATION_SCHEMA_VERSION,
            migration_id: migration.clone(),
            vm_id: "vm-h".to_owned(),
            source_host: host_id("src-host"),
            target_host: kit.host.clone(),
            participants: vec![PreparedParticipant {
                volume_id: volume.clone(),
                expected_generation: 1,
                device_path: None,
            }],
            grant_attempt: 0,
            created_at: 1,
        })
        .expect("install preparation");
    // The recorded grant: epoch 2 minted for this host over the
    // source's retired epoch 1 (the barrier names the migration — the
    // cut's fence evidence).
    kit.witness.set_expired(&volume, 2, 1);
    kit.witness.set_barrier(&volume, &migration, 1);
    kit.witness.set_retired(&volume, 1);
}

/// The horizon remedy's fresh grant: when the recorded grant's lease
/// is provably dead (expired at the witness, still ours) and the park
/// is provably pre-promote with its fence evidence intact, the grant
/// act re-issues under a NEW derived operation id — the journaled
/// attempt counter folded into the reserved namespace — and the
/// re-minted grant drives the promotes to completion.
#[tokio::test]
async fn peer_grant_re_mints_under_a_new_derived_id_when_the_grant_lease_lapsed() {
    let kit = setup_peer();
    let app = app(&kit.state);
    arrange_lapsed_park(&kit, "vol-h1", "mig-h1").await;

    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-h1"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["grants"][0]["device_path"],
        json!("/dev/drbd-by-res/vol-h1"),
        "the re-minted grant drove the promote"
    );

    // The witness batch ran exactly once, under the attempt-1
    // reserved-namespace id — never the original bare id (its
    // journal entry is the expired grant the re-mint exists to
    // leave behind).
    let calls = kit.witness.grant_set_calls.lock().expect("calls").clone();
    assert_eq!(calls.len(), 1);
    let raw = calls[0].operation_id.as_str();
    assert!(
        raw.starts_with("derived:mig-grant-set-r1-"),
        "the re-mint mints under the attempt-folded derived id: {raw}"
    );
    assert!(!raw.starts_with("mig-grant-set-"));

    // The attempt counter is journaled on the preparation (durable
    // before the witness call — a re-drive re-derives this id).
    let preparation = kit
        .store
        .load(&migration_id("mig-h1"))
        .expect("load")
        .expect("installed");
    assert_eq!(preparation.grant_attempt, 1);
    assert_eq!(kit.handoff.promotes.lock().expect("promotes").len(), 1);
}

/// The re-mint's determinism across a crash: the attempt counter was
/// bumped durably and the fresh witness call landed, but the route's
/// outcome did not (the in-flight intent shape) — the re-drive
/// re-derives the SAME attempt-1 id and the witness journal replays
/// the recorded grant under it, never minting a second one.
#[tokio::test]
async fn peer_grant_re_mint_id_is_deterministic_across_the_in_flight_re_drive() {
    let kit = setup_peer();
    let app = app(&kit.state);
    arrange_lapsed_park(&kit, "vol-h2", "mig-h2").await;
    let volume = volume_id("vol-h2");
    let migration = migration_id("mig-h2");

    // The world after the fresh grant landed but before the route's
    // outcome journaled: the attempt counter is already 1, the fresh
    // lease is live at epoch 3, and the witness journal holds the
    // grant under the attempt-1 id.
    kit.store
        .begin_grant_attempt(&migration)
        .expect("bump attempt");
    kit.witness.set_live(&volume, 3, 2);
    let attempt_id = volvisor_handoff::grant_set_reissue_operation_id(
        &migration,
        &[volvisor_handoff::Participant {
            volume_id: volume.clone(),
            expected_generation: 1,
            resource: String::new(),
            minor: 0,
        }],
        1,
    )
    .expect("derive the expected attempt-1 id");
    kit.witness
        .recorded_grants
        .lock()
        .expect("recorded grants")
        .insert(
            attempt_id.clone(),
            volvisor_witness::proto::GrantSetResponse {
                grants: vec![volvisor_witness::proto::BatchGrantOutcome {
                    volume_id: volume.clone(),
                    epoch: volvisor_types::WriterEpoch(3),
                    lease_id: volvisor_types::LeaseId(2),
                    lease_ttl_secs: 30,
                    fencing_proof: volvisor_types::FencingProof {
                        volume_id: volume.clone(),
                        retired_epoch: volvisor_types::WriterEpoch(2),
                        commit_index: 1,
                    },
                }],
            },
        );
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

    // The re-drive: the inspection cannot prove the act landed (no
    // device paths), so the act re-executes — and re-derives the
    // attempt-1 id, so the witness REPLAYS the recorded grant instead
    // of minting a second one.
    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-h2"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["grants"][0]["device_path"],
        json!("/dev/drbd-by-res/vol-h2")
    );
    assert_eq!(
        kit.witness.grant_set_calls.lock().expect("calls").len(),
        0,
        "the re-drive re-derived the attempt-1 id and replayed the \
         recorded grant — a different id would have minted here"
    );
    assert_eq!(kit.handoff.promotes.lock().expect("promotes").len(), 1);
    // The counter was not bumped again: the lease is live, so the
    // re-drive took the serviceable path, not a second re-mint.
    assert_eq!(
        kit.store
            .load(&migration)
            .expect("load")
            .expect("installed")
            .grant_attempt,
        1
    );
}

/// Gate (b), the negative: the record is at or after promote (one
/// participant's device path is durably recorded while another still
/// needs the lease — the mixed park a mid-promote-loop crash leaves)
/// — the re-mint is refused typed (a re-mint would retire the epoch a
/// completed promote's authority is anchored to) and the park keeps
/// its honest spin: no witness call, no counter bump.
#[tokio::test]
async fn peer_grant_refuses_the_re_mint_when_the_record_is_at_or_after_promote() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-h3").await;
    create_volume(&kit, "vol-h3b").await;
    let migration = migration_id("mig-h3");
    for raw in ["vol-h3", "vol-h3b"] {
        let volume = volume_id(raw);
        kit.witness.set_expired(&volume, 2, 1);
        kit.witness.set_barrier(&volume, &migration, 1);
        kit.witness.set_retired(&volume, 1);
    }
    kit.store
        .install(&TargetPreparation {
            schema_version: TARGET_PREPARATION_SCHEMA_VERSION,
            migration_id: migration.clone(),
            vm_id: "vm-h3".to_owned(),
            source_host: host_id("src-host"),
            target_host: kit.host.clone(),
            participants: vec![
                PreparedParticipant {
                    volume_id: volume_id("vol-h3"),
                    expected_generation: 1,
                    device_path: Some("/dev/drbd-by-res/vol-h3".to_owned()),
                },
                PreparedParticipant {
                    volume_id: volume_id("vol-h3b"),
                    expected_generation: 1,
                    device_path: None,
                },
            ],
            grant_attempt: 0,
            created_at: 1,
        })
        .expect("install preparation");

    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-h3"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], json!("INVALID_STATE"));
    let message = body["message"].as_str().expect("message");
    assert!(
        message.contains("at or after promote"),
        "the refusal names the gate: {message}"
    );
    assert!(
        message.contains("vol-h3"),
        "the refusal names the promoted participant: {message}"
    );
    assert!(
        message.contains("pre-promote"),
        "the refusal states the remedy's scope: {message}"
    );
    assert_eq!(
        kit.witness.grant_set_calls.lock().expect("calls").len(),
        0,
        "no witness call — the park keeps its honest spin"
    );
    assert_eq!(kit.handoff.promotes.lock().expect("promotes").len(), 0);
    assert_eq!(
        kit.store
            .load(&migration)
            .expect("load")
            .expect("installed")
            .grant_attempt,
        0,
        "the attempt counter is never bumped on a refusal"
    );
}

/// Gate (c), the negative: the witness's fence evidence for the
/// participant is not the shape the grant left (no barrier naming the
/// migration, no retired source epoch below the granted one) — the
/// re-mint is refused typed: a fresh grant must never mint while any
/// writer could be live.
#[tokio::test]
async fn peer_grant_refuses_the_re_mint_when_the_fence_evidence_moved() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-h4").await;
    let volume = volume_id("vol-h4");
    let migration = migration_id("mig-h4");
    kit.store
        .install(&TargetPreparation {
            schema_version: TARGET_PREPARATION_SCHEMA_VERSION,
            migration_id: migration.clone(),
            vm_id: "vm-h4".to_owned(),
            source_host: host_id("src-host"),
            target_host: kit.host.clone(),
            participants: vec![PreparedParticipant {
                volume_id: volume.clone(),
                expected_generation: 1,
                device_path: None,
            }],
            grant_attempt: 0,
            created_at: 1,
        })
        .expect("install preparation");
    // The lapsed lease, but the fence evidence is GONE: no barrier
    // naming the migration, no retirement below the granted epoch.
    kit.witness.set_expired(&volume, 2, 1);

    let (status, body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-h4"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], json!("INVALID_STATE"));
    let message = body["message"].as_str().expect("message");
    assert!(
        message.contains("fence"),
        "the refusal names the gate: {message}"
    );
    assert!(
        message.contains("never mint while any writer could be live"),
        "the refusal states the safety argument: {message}"
    );
    assert_eq!(
        kit.witness.grant_set_calls.lock().expect("calls").len(),
        0,
        "no witness call — the park keeps its honest spin"
    );
    assert_eq!(
        kit.store
            .load(&migration)
            .expect("load")
            .expect("installed")
            .grant_attempt,
        0
    );
}

/// Gate (a)'s revoked arm, the negative: the witness reports the
/// participants' leases as `Revoked` while still recorded held by
/// this host — a deliberate end (an abort's fail-closed release,
/// row 12b's shape). The re-mint refuses typed BEFORE any witness
/// grant call (never the replay's implicit promote-refusal, never
/// the fresh-execution arm that would mint over the deliberate
/// end): the module docs' "a revoked or foreign lease never
/// re-mints", pinned — and the refusal reproduces identically on
/// the re-drive (the park keeps its typed spin).
#[tokio::test]
async fn peer_grant_refuses_the_re_mint_when_the_lease_was_revoked() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-h5").await;
    create_volume(&kit, "vol-h5b").await;
    let migration = migration_id("mig-h5");
    for raw in ["vol-h5", "vol-h5b"] {
        let volume = volume_id(raw);
        kit.witness.set_revoked(&volume, 2, 1);
        kit.witness.set_barrier(&volume, &migration, 1);
        kit.witness.set_retired(&volume, 1);
    }
    kit.store
        .install(&TargetPreparation {
            schema_version: TARGET_PREPARATION_SCHEMA_VERSION,
            migration_id: migration.clone(),
            vm_id: "vm-h5".to_owned(),
            source_host: host_id("src-host"),
            target_host: kit.host.clone(),
            participants: vec![
                PreparedParticipant {
                    volume_id: volume_id("vol-h5"),
                    expected_generation: 1,
                    device_path: None,
                },
                PreparedParticipant {
                    volume_id: volume_id("vol-h5b"),
                    expected_generation: 1,
                    device_path: None,
                },
            ],
            grant_attempt: 0,
            created_at: 1,
        })
        .expect("install preparation");

    // The act refuses typed — twice (the re-drive's re-execution
    // reproduces the refusal identically: the spin is typed, never
    // silent).
    for redrive in 0..2 {
        let (status, body) = send_json(
            &app,
            peer_request(
                Method::POST,
                "/v2/internal/peer/grant",
                &json!({"migration_id": "mig-h5"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "re-drive {redrive}: {body}");
        assert_eq!(body["code"], json!("INVALID_STATE"), "re-drive {redrive}");
        let message = body["message"].as_str().expect("message");
        assert!(
            message.contains("revoked"),
            "the refusal names the deliberate end: {message}"
        );
        assert!(
            message.contains("deliberate end"),
            "the refusal states the lease's meaning: {message}"
        );
        assert!(
            message.contains("no path converts a revoked lease"),
            "the refusal states the guarantee: {message}"
        );
        assert!(
            message.contains("operator territory"),
            "the refusal states the park's disposition: {message}"
        );
    }
    assert_eq!(
        kit.witness.grant_set_calls.lock().expect("calls").len(),
        0,
        "no witness grant call — no path mints over the revoked lease"
    );
    assert_eq!(kit.handoff.promotes.lock().expect("promotes").len(), 0);
    assert_eq!(
        kit.store
            .load(&migration)
            .expect("load")
            .expect("installed")
            .grant_attempt,
        0,
        "the attempt counter is never bumped on a refusal"
    );
    // The leases stay revoked — the scripted deliberate end is
    // intact, no path converted it.
    {
        let volumes = kit.witness.volumes.lock().expect("volumes");
        for raw in ["vol-h5", "vol-h5b"] {
            let entry = volumes.get(&volume_id(raw)).expect("the scripted lease");
            assert_eq!(
                entry.lease_state,
                volvisor_types::LeaseState::Revoked,
                "{raw}: the lease stays revoked"
            );
            assert_eq!(
                entry.epoch, 2,
                "{raw}: the granted epoch never moved (no mint over the deliberate end)"
            );
        }
    }
}

/// The still-live negative: while any participant's lease is still
/// live, the re-mint never fires (renewal's job instead) — the act
/// takes today's path under the CURRENT attempt's id, here the
/// original bare attempt-0 id (the witness's own live-lease refusal
/// governs the batch in production).
#[tokio::test]
async fn peer_grant_uses_the_original_id_while_a_member_is_still_live() {
    let kit = setup_peer();
    let app = app(&kit.state);
    create_volume(&kit, "vol-m1").await;
    create_volume(&kit, "vol-m2").await;
    let migration = migration_id("mig-mixed");
    kit.store
        .install(&TargetPreparation {
            schema_version: TARGET_PREPARATION_SCHEMA_VERSION,
            migration_id: migration.clone(),
            vm_id: "vm-m".to_owned(),
            source_host: host_id("src-host"),
            target_host: kit.host.clone(),
            participants: vec![
                PreparedParticipant {
                    volume_id: volume_id("vol-m1"),
                    expected_generation: 1,
                    device_path: None,
                },
                PreparedParticipant {
                    volume_id: volume_id("vol-m2"),
                    expected_generation: 1,
                    device_path: None,
                },
            ],
            grant_attempt: 0,
            created_at: 1,
        })
        .expect("install preparation");
    // The mixed shape: one member's lease still live (renewal's job),
    // the other lapsed.
    kit.witness.set_live(&volume_id("vol-m1"), 2, 1);
    kit.witness.set_expired(&volume_id("vol-m2"), 2, 1);

    let (status, _body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/grant",
            &json!({"migration_id": "mig-mixed"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let calls = kit.witness.grant_set_calls.lock().expect("calls").clone();
    assert_eq!(calls.len(), 1);
    let raw = calls[0].operation_id.as_str();
    assert!(
        raw.starts_with("mig-grant-set-") && !raw.starts_with("derived:"),
        "the mixed shape takes the original-id path, never a re-mint: {raw}"
    );
    assert_eq!(
        kit.store
            .load(&migration)
            .expect("load")
            .expect("installed")
            .grant_attempt,
        0,
        "no attempt was spent while a member is still live"
    );
}

/// The grant-hold renewal pass: a pre-promote preparation whose
/// leases are uniformly live gets its due members renewed (the
/// `remaining ≤ interval` rule — a lease with headroom is left
/// alone), under this host's credential with the view's epoch and
/// lease identity.
#[tokio::test]
async fn renew_grant_hold_leases_renews_due_pre_promote_leases() {
    let kit = setup_peer();
    create_volume(&kit, "vol-r1").await;
    create_volume(&kit, "vol-r2").await;
    let migration = migration_id("mig-renew1");
    kit.store
        .install(&TargetPreparation {
            schema_version: TARGET_PREPARATION_SCHEMA_VERSION,
            migration_id: migration.clone(),
            vm_id: "vm-r1".to_owned(),
            source_host: host_id("src-host"),
            target_host: kit.host.clone(),
            participants: vec![
                PreparedParticipant {
                    volume_id: volume_id("vol-r1"),
                    expected_generation: 1,
                    device_path: None,
                },
                PreparedParticipant {
                    volume_id: volume_id("vol-r2"),
                    expected_generation: 1,
                    device_path: None,
                },
            ],
            grant_attempt: 0,
            created_at: 1,
        })
        .expect("install preparation");
    // vol-r1 is due (within the interval of its end); vol-r2 has
    // headroom (not due this pass).
    kit.witness
        .set_live_with_remaining(&volume_id("vol-r1"), 2, 1, 10);
    kit.witness
        .set_live_with_remaining(&volume_id("vol-r2"), 2, 2, 90);

    let report = kit
        .context
        .renew_grant_hold_leases(20)
        .await
        .expect("renewal pass");
    assert_eq!(report.renewed, vec![volume_id("vol-r1")]);
    assert!(
        report.lapsed.is_empty(),
        "the preparation is uniformly live"
    );
    assert!(
        report.deferred.is_empty(),
        "nothing deferred on the happy pass"
    );

    let calls = kit.witness.renew_calls.lock().expect("renew calls").clone();
    assert_eq!(calls.len(), 1, "only the due member was renewed");
    assert_eq!(calls[0].0, volume_id("vol-r1"));
    assert_eq!(calls[0].1.epoch, volvisor_types::WriterEpoch(2));
    assert_eq!(calls[0].1.lease_id, volvisor_types::LeaseId(1));
    assert_eq!(calls[0].1.host_id, kit.host);
    assert!(
        calls[0]
            .1
            .operation_id
            .as_str()
            .starts_with("grant-hold-renew-vol-r1-"),
        "the renewal ids carry their volume and are process-unique"
    );
}

/// The uniform-liveness coupling: a preparation with one dead member
/// is skipped whole — the surviving member is deliberately allowed to
/// lapse toward the fresh-grant gate (renewing it would block that
/// gate forever), reported in `lapsed`.
#[tokio::test]
async fn renew_grant_hold_leases_skips_a_preparation_that_is_not_uniformly_live() {
    let kit = setup_peer();
    create_volume(&kit, "vol-s1").await;
    create_volume(&kit, "vol-s2").await;
    let migration = migration_id("mig-renew2");
    kit.store
        .install(&TargetPreparation {
            schema_version: TARGET_PREPARATION_SCHEMA_VERSION,
            migration_id: migration.clone(),
            vm_id: "vm-r2".to_owned(),
            source_host: host_id("src-host"),
            target_host: kit.host.clone(),
            participants: vec![
                PreparedParticipant {
                    volume_id: volume_id("vol-s1"),
                    expected_generation: 1,
                    device_path: None,
                },
                PreparedParticipant {
                    volume_id: volume_id("vol-s2"),
                    expected_generation: 1,
                    device_path: None,
                },
            ],
            grant_attempt: 0,
            created_at: 1,
        })
        .expect("install preparation");
    // One member lapsed: the preparation is not uniformly live.
    kit.witness
        .set_live_with_remaining(&volume_id("vol-s1"), 2, 1, 10);
    kit.witness.set_expired(&volume_id("vol-s2"), 2, 2);

    let report = kit
        .context
        .renew_grant_hold_leases(20)
        .await
        .expect("renewal pass");
    assert!(
        report.renewed.is_empty(),
        "the lapsed preparation is skipped whole"
    );
    assert_eq!(report.lapsed, vec![migration]);
    assert!(
        report.deferred.is_empty(),
        "the lapsed shape defers nothing"
    );
    assert_eq!(
        kit.witness.renew_calls.lock().expect("renew calls").len(),
        0,
        "the surviving member is NOT renewed — it must lapse toward \
         the fresh-grant gate"
    );
}

/// The terminal-shape stop: a preparation whose participants are all
/// promoted (device paths recorded) is not the grant-hold pass's
/// business — its leases belong to the volumes' own authority blocks
/// now. No lease outlives its migration record through this pass.
#[tokio::test]
async fn renew_grant_hold_leases_stops_at_promoted_participants() {
    let kit = setup_peer();
    create_volume(&kit, "vol-p1").await;
    let migration = migration_id("mig-renew3");
    kit.store
        .install(&TargetPreparation {
            schema_version: TARGET_PREPARATION_SCHEMA_VERSION,
            migration_id: migration,
            vm_id: "vm-r3".to_owned(),
            source_host: host_id("src-host"),
            target_host: kit.host.clone(),
            participants: vec![PreparedParticipant {
                volume_id: volume_id("vol-p1"),
                expected_generation: 1,
                device_path: Some("/dev/drbd-by-res/vol-p1".to_owned()),
            }],
            grant_attempt: 0,
            created_at: 1,
        })
        .expect("install preparation");
    kit.witness
        .set_live_with_remaining(&volume_id("vol-p1"), 2, 1, 10);

    let report = kit
        .context
        .renew_grant_hold_leases(20)
        .await
        .expect("renewal pass");
    assert!(
        report.renewed.is_empty(),
        "the terminal shape renews nothing"
    );
    assert!(
        report.lapsed.is_empty(),
        "the terminal shape is not reported lapsed"
    );
    assert!(
        report.deferred.is_empty(),
        "the terminal shape defers nothing"
    );
    assert_eq!(
        kit.witness.renew_calls.lock().expect("renew calls").len(),
        0,
        "a promoted participant's lease is never renewed by the \
         grant-hold pass"
    );
}

/// The honest deferral: a witness unreachable at renewal time (or a
/// renewal that cannot cover the interval) fails into `deferred` with
/// its reason — the pass never crashes, the park's semantics are
/// unchanged, the next pass retries.
#[tokio::test]
async fn renew_grant_hold_leases_defers_honestly_on_witness_failure() {
    let kit = setup_peer();
    create_volume(&kit, "vol-d1").await;
    let migration = migration_id("mig-renew4");
    kit.store
        .install(&TargetPreparation {
            schema_version: TARGET_PREPARATION_SCHEMA_VERSION,
            migration_id: migration.clone(),
            vm_id: "vm-r4".to_owned(),
            source_host: host_id("src-host"),
            target_host: kit.host.clone(),
            participants: vec![PreparedParticipant {
                volume_id: volume_id("vol-d1"),
                expected_generation: 1,
                device_path: None,
            }],
            grant_attempt: 0,
            created_at: 1,
        })
        .expect("install preparation");
    kit.witness
        .set_live_with_remaining(&volume_id("vol-d1"), 2, 1, 10);

    // The witness is unreachable: the renewal defers with the honest
    // reason, and the preparation is NOT marked lapsed (the lease's
    // state is unknown, not dead).
    kit.witness.refuse_next_renew(WitnessError::Unreachable(
        "the witness is down (scripted)".to_owned(),
    ));
    let report = kit
        .context
        .renew_grant_hold_leases(20)
        .await
        .expect("renewal pass");
    assert!(
        report.renewed.is_empty(),
        "the witness outage deferred the renewal"
    );
    assert!(
        report.lapsed.is_empty(),
        "an unreachable witness is not a dead lease"
    );
    assert_eq!(report.deferred.len(), 1);
    assert_eq!(report.deferred[0].0, volume_id("vol-d1"));
    assert!(
        report.deferred[0].1.contains("the witness is down"),
        "the deferral carries the honest reason: {}",
        report.deferred[0].1
    );

    // The interval guard (the authority's shrink discipline): a
    // renewal response that cannot cover the interval is refused and
    // deferred, never recorded as a renewal.
    kit.witness.set_renew_remaining(5);
    let report = kit
        .context
        .renew_grant_hold_leases(20)
        .await
        .expect("renewal pass");
    assert!(
        report.renewed.is_empty(),
        "the shrink-guarded renewal was refused"
    );
    assert!(
        report.lapsed.is_empty(),
        "the shrink guard defers, never lapses"
    );
    assert_eq!(report.deferred.len(), 1);
    assert!(
        report.deferred[0].1.contains("cannot cover"),
        "the shrink guard's deferral names the discipline: {}",
        report.deferred[0].1
    );
}

/// The attempt counter's durability: `begin_grant_attempt` bumps and
/// persists atomically (a re-opened store re-reads it), refuses
/// typed on an absent preparation, and preparations recorded before
/// the remedy (no `grant_attempt` field on disk) load at attempt 0.
#[test]
fn target_preparation_store_journals_the_grant_attempt_durably() {
    let dir = tempfile::tempdir().expect("journal dir");
    let store = TargetPreparationStore::open(dir.path().join("preparations")).expect("store open");
    let migration = migration_id("mig-attempt");
    store
        .install(&TargetPreparation {
            schema_version: TARGET_PREPARATION_SCHEMA_VERSION,
            migration_id: migration.clone(),
            vm_id: "vm-a".to_owned(),
            source_host: host_id("src-host"),
            target_host: host_id("dst-host"),
            participants: vec![PreparedParticipant {
                volume_id: volume_id("vol-a1"),
                expected_generation: 1,
                device_path: None,
            }],
            grant_attempt: 0,
            created_at: 1,
        })
        .expect("install");

    // The bump is durable: a re-opened store (the crash-recovery
    // view) re-reads the counter.
    assert_eq!(store.begin_grant_attempt(&migration).expect("bump"), 1);
    assert_eq!(store.begin_grant_attempt(&migration).expect("bump"), 2);
    let reopened = TargetPreparationStore::open(dir.path().join("preparations")).expect("reopen");
    assert_eq!(
        reopened
            .load(&migration)
            .expect("load")
            .expect("installed")
            .grant_attempt,
        2,
        "the attempt counter survives a re-open (the crash-recovery view)"
    );

    // An absent preparation is the typed NOT_FOUND, never a bump.
    let error = store
        .begin_grant_attempt(&migration_id("mig-absent"))
        .expect_err("absent");
    assert_eq!(error.code, ApiErrorCode::NotFound);

    // A pre-remedy record file (no grant_attempt field) loads at
    // attempt 0 — in-flight journals keep replaying the original id.
    let old_raw = serde_json::json!({
        "migration_id": "mig-old",
        "vm_id": "vm-o",
        "source_host": "src-host",
        "target_host": "dst-host",
        "participants": [{
            "volume_id": "vol-o1",
            "expected_generation": 1,
            "device_path": null,
        }],
        "created_at": 1,
    });
    std::fs::write(
        dir.path().join("preparations/mig-old.json"),
        serde_json::to_vec_pretty(&old_raw).expect("serialize"),
    )
    .expect("write the pre-remedy record");
    let with_old = TargetPreparationStore::open(dir.path().join("preparations")).expect("open");
    assert_eq!(
        with_old
            .load(&migration_id("mig-old"))
            .expect("load")
            .expect("installed")
            .grant_attempt,
        0,
        "a pre-remedy record keeps attempt 0 — the original id"
    );

    // load_all: every preparation, ordered by migration id.
    let all = with_old.load_all().expect("load all");
    assert_eq!(
        all.iter()
            .map(|record| record.migration_id.as_str().to_owned())
            .collect::<Vec<_>>(),
        vec!["mig-attempt".to_owned(), "mig-old".to_owned()],
        "load_all is ordered by migration id"
    );
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
    // migration with a FRESH destination VM id (the first one's
    // restore-vm operation id now carries its recorded outcome, and a
    // different body under it is the journal's idempotency conflict,
    // exactly as designed; the fresh id also honors prepare's
    // emptiness verification — a destination socket is empty at
    // prepare in every production shape, since the VM can only arrive
    // through this migration's own restore).
    let (status, _body) = send_json(
        &app,
        peer_request(
            Method::POST,
            "/v2/internal/peer/prepare",
            &peer_prepare_body("mig-v1m", "vm-v1m", &[("vol-v1", 1)]),
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
async fn peer_restore_vm_re_drives_a_created_destination_vm_on_the_resume_path() {
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

    // A defined, not-booted destination VM — the exact shape a crash
    // between define and boot leaves — is this migration's own
    // half-restore (prepare verified the socket empty before the
    // source's cut): the resume path destroys it first and re-drives
    // (plan §3's re-drive rule, row 18) instead of stalling on it.
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
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["vm_state"], json!("running"));
    // The re-drive order is visible in the recording wrapper's call
    // log: the destroy of the half-restored VM precedes the restore.
    let calls = kit.vmm.calls.lock().expect("calls");
    let destroyed = calls.iter().position(|label| *label == "destroy");
    let restored = calls.iter().position(|label| *label == "restore");
    assert!(
        destroyed.is_some_and(|at| restored.is_some_and(|after| at < after)),
        "the half-restored VM is destroyed before the restore: {calls:?}"
    );
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

// --------------------------------------------------------- crash hook

/// The crash hook's doc gate (P5 plan §3.1): the armed table defaults
/// to inert — no consult fires anything until the constructing rig
/// arms an exact `(operation kind, point)` pair, an unarmed point is
/// a no-op even with a kill switch registered, a firing is one-shot,
/// and a state built without `with_crash_hooks` carries no hooks at
/// all (no route, config or input path can reach the table).
#[test]
fn crash_hooks_default_to_inert_and_fire_only_when_armed() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::crash::{CRASH_PANIC_PREFIX, CrashHooks, CrashPoint};
    use crate::op_kinds::OP_CREATE_VOLUME;

    let hooks = CrashHooks::new();
    let fired = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&fired);
    hooks.set_kill_switch(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));

    // Inert by default: every point of every operation kind is a
    // no-op, and the kill switch never fires.
    hooks.consult(OP_CREATE_VOLUME, CrashPoint::AfterIntent);
    hooks.consult(OP_CREATE_VOLUME, CrashPoint::BeforeOutcome);
    hooks.consult(OP_CREATE_VOLUME, CrashPoint::AfterOutcome);
    assert_eq!(
        fired.load(Ordering::SeqCst),
        0,
        "unarmed consults fire nothing"
    );

    // A built state carries no hooks unless the constructing rig
    // attaches them.
    let (state, _provider, _dir) = setup_with_token(Some(TEST_TOKEN));
    assert!(
        state.crash.is_none(),
        "AppState::new leaves the crash hook unset (inert)"
    );

    // Arming one point of one operation fires exactly there: the
    // kill switch runs once and the firing consult panics with the
    // campaign payload (the in-band process death).
    hooks.arm(OP_CREATE_VOLUME, CrashPoint::AfterOutcome);
    let silence = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let fired_panic = catch_unwind(AssertUnwindSafe(|| {
        hooks.consult(OP_CREATE_VOLUME, CrashPoint::AfterOutcome);
    }));
    std::panic::set_hook(silence);
    let payload = fired_panic
        .expect_err("the armed point terminates the request")
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_default();
    assert!(
        payload.starts_with(CRASH_PANIC_PREFIX),
        "the panic payload is the campaign crash marker: {payload}"
    );
    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "the kill switch fired once"
    );

    // One shot: the consumed entry cannot fire again, and a
    // differently-pointed entry stays armed for its own point.
    hooks.consult(OP_CREATE_VOLUME, CrashPoint::AfterOutcome);
    assert_eq!(fired.load(Ordering::SeqCst), 1, "a fired entry is consumed");
    hooks.arm(OP_CREATE_VOLUME, CrashPoint::BeforeOutcome);
    hooks.consult(OP_CREATE_VOLUME, CrashPoint::AfterIntent);
    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "only the exact point fires"
    );

    // `clear` disarms without firing.
    hooks.clear();
    hooks.consult(OP_CREATE_VOLUME, CrashPoint::BeforeOutcome);
    assert_eq!(fired.load(Ordering::SeqCst), 1, "cleared entries are inert");
}
