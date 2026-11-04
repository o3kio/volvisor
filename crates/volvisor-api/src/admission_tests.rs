//! Admission-control integration-style tests (issue #45).
//!
//! Every test drives the real router with a real journal over a temporary
//! directory. The controlled provider delegates to [`FakeProvider`] and adds
//! only the latency/failure the test needs, so the exercised surface is the
//! actual middleware stack: concurrency permit, read timeout, authentication,
//! per-principal rate limit, body limit and handler.

#![allow(clippy::too_many_lines)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tower::ServiceExt;
use volvisor_journal::Journal;
use volvisor_provider::VolumeProvider;
use volvisor_provider::conformance::fixture_create_request;
use volvisor_provider::fake::FakeProvider;
use volvisor_types::domain::VolumeClass;
use volvisor_types::request::{
    AttachVolumeRequest, AttachVolumeResponse, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, GrowVolumeRequest, GrowVolumeResponse, InspectVolumeResponse,
};
use volvisor_types::{ApiError, CapabilitySet, ProjectId, VolumeId};

use crate::auth::{Credential, Role};
use crate::{ApiConfig, AppState, RouteTimeouts, SharedState, router_with_config};

const ADMIN_TOKEN: &str = "admission-admin-token";
const READER_A_TOKEN: &str = "admission-reader-a-token";
const READER_B_TOKEN: &str = "admission-reader-b-token";
const GIB: u64 = 1 << 30;

/// How the controlled provider's `list_volumes` behaves.
#[derive(Clone, Copy)]
enum ListBehavior {
    Normal,
    Hang,
    Delay(Duration),
}

struct ControlledProvider {
    inner: FakeProvider,
    list: ListBehavior,
    create_delay: Duration,
    inspect_delay: Duration,
    entered: Arc<Notify>,
}

impl ControlledProvider {
    fn new() -> Self {
        Self {
            inner: FakeProvider::new(),
            list: ListBehavior::Normal,
            create_delay: Duration::ZERO,
            inspect_delay: Duration::ZERO,
            entered: Arc::new(Notify::new()),
        }
    }

    fn hanging_list() -> Self {
        Self {
            list: ListBehavior::Hang,
            ..Self::new()
        }
    }

    fn with_list_delay(delay: Duration) -> Self {
        Self {
            list: ListBehavior::Delay(delay),
            ..Self::new()
        }
    }

    fn with_create_delay(delay: Duration) -> Self {
        Self {
            create_delay: delay,
            ..Self::new()
        }
    }

    fn with_list_and_inspect_delay(delay: Duration) -> Self {
        Self {
            list: ListBehavior::Delay(delay),
            inspect_delay: delay,
            ..Self::new()
        }
    }

    async fn seed_volume(&self, id: &str) {
        self.inner
            .create_volume(&fixture_create_request(id, GIB))
            .await
            .expect("seed volume");
    }
}

#[async_trait::async_trait]
impl VolumeProvider for ControlledProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn capabilities(&self) -> CapabilitySet {
        self.inner.capabilities()
    }

    fn supported_classes(&self) -> &[VolumeClass] {
        self.inner.supported_classes()
    }

    async fn create_volume(
        &self,
        req: &CreateVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError> {
        if !self.create_delay.is_zero() {
            tokio::time::sleep(self.create_delay).await;
        }
        self.inner.create_volume(req).await
    }

    async fn inspect_volume(&self, id: &VolumeId) -> Result<InspectVolumeResponse, ApiError> {
        if !self.inspect_delay.is_zero() {
            tokio::time::sleep(self.inspect_delay).await;
        }
        self.inner.inspect_volume(id).await
    }

    async fn list_volumes(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<InspectVolumeResponse>, ApiError> {
        self.entered.notify_waiters();
        match self.list {
            ListBehavior::Normal => {}
            ListBehavior::Hang => std::future::pending::<()>().await,
            ListBehavior::Delay(delay) => tokio::time::sleep(delay).await,
        }
        self.inner.list_volumes(project).await
    }

    async fn attach_volume(
        &self,
        volume_id: &VolumeId,
        req: &AttachVolumeRequest,
    ) -> Result<AttachVolumeResponse, ApiError> {
        self.inner.attach_volume(volume_id, req).await
    }

    async fn detach_volume(
        &self,
        volume_id: &VolumeId,
        attachment_id: &volvisor_types::AttachmentId,
        req: &DetachVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError> {
        self.inner
            .detach_volume(volume_id, attachment_id, req)
            .await
    }

    async fn grow_volume(
        &self,
        volume_id: &VolumeId,
        req: &GrowVolumeRequest,
    ) -> Result<GrowVolumeResponse, ApiError> {
        self.inner.grow_volume(volume_id, req).await
    }

    async fn delete_volume(
        &self,
        volume_id: &VolumeId,
        req: &DeleteVolumeRequest,
    ) -> Result<(), ApiError> {
        self.inner.delete_volume(volume_id, req).await
    }
}

fn setup(provider: Arc<ControlledProvider>) -> (SharedState, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider: Arc<dyn VolumeProvider> = provider;
    let state = Arc::new(AppState::new(
        provider,
        None,
        journal,
        Some(ADMIN_TOKEN.to_owned()),
    ));
    (state, dir)
}

fn setup_scoped(provider: Arc<ControlledProvider>) -> (SharedState, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider: Arc<dyn VolumeProvider> = provider;
    let credentials = vec![
        Credential {
            id: "reader-a".to_owned(),
            token: READER_A_TOKEN.to_owned(),
            role: Role::ReadOnly,
        },
        Credential {
            id: "reader-b".to_owned(),
            token: READER_B_TOKEN.to_owned(),
            role: Role::ReadOnly,
        },
    ];
    let state =
        Arc::new(AppState::new(provider, None, journal, None).with_credentials(credentials));
    (state, dir)
}

fn base_config(max_body_bytes: usize) -> ApiConfig {
    ApiConfig {
        max_body_bytes,
        ..ApiConfig::default()
    }
}

fn request(method: Method, uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("build request")
}

fn json_request(method: Method, uri: &str, token: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .expect("build request")
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let response = app.clone().oneshot(request).await.expect("router call");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read response body");
    let body = String::from_utf8(bytes.to_vec()).expect("utf-8 body");
    let value = serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }));
    (status, headers, value)
}

fn retry_after(headers: &HeaderMap) -> &str {
    headers
        .get(header::RETRY_AFTER)
        .expect("Retry-After header")
        .to_str()
        .expect("Retry-After is ASCII")
}

fn journal_record_count(state: &SharedState) -> u64 {
    state
        .journal
        .lock()
        .expect("journal lock in test")
        .record_count()
}

// ---------------------------------------------------------------------------
// Timeouts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_route_timeout_is_typed_and_does_not_pin_the_connection() {
    let provider = Arc::new(ControlledProvider::with_list_delay(Duration::from_millis(
        250,
    )));
    let (state, _dir) = setup(provider);
    let config = ApiConfig {
        read_timeout: Duration::from_millis(40),
        ..base_config(1 << 20)
    };
    let app = router_with_config(state, &config);

    let (status, _headers, body) =
        send(&app, request(Method::GET, "/v2/volumes", ADMIN_TOKEN)).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(body["code"], json!("REQUEST_TIMEOUT"));
}

#[tokio::test]
async fn head_request_is_bounded_like_get() {
    // axum routes HEAD to the GET handler; the timeout selector must include
    // it or a hung provider pins a HEAD request indefinitely.
    let provider = Arc::new(ControlledProvider::with_list_delay(Duration::from_millis(
        250,
    )));
    let (state, _dir) = setup(provider);
    let config = ApiConfig {
        read_timeout: Duration::from_millis(40),
        ..base_config(1 << 20)
    };
    let app = router_with_config(state, &config);

    let (status, _headers, _body) =
        send(&app, request(Method::HEAD, "/v2/volumes", ADMIN_TOKEN)).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
}

#[tokio::test]
async fn per_route_override_bounds_only_its_matched_path() {
    // Both read routes are deliberately slower than the short list bound.
    // The list route's override must fire while the inspect route, with a
    // longer explicit bound, completes — the override is keyed by matched
    // path, not global.
    let provider = Arc::new(ControlledProvider::with_list_and_inspect_delay(
        Duration::from_millis(250),
    ));
    provider.seed_volume("vol-bounded").await;
    let (state, _dir) = setup(provider);
    let config = ApiConfig {
        read_timeout: Duration::from_secs(30),
        route_timeouts: RouteTimeouts {
            list_volumes: Some(Duration::from_millis(40)),
            inspect_volume: Some(Duration::from_secs(5)),
            ..RouteTimeouts::default()
        },
        ..base_config(1 << 20)
    };
    let app = router_with_config(state, &config);

    let (status, _headers, body) =
        send(&app, request(Method::GET, "/v2/volumes", ADMIN_TOKEN)).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(body["code"], json!("REQUEST_TIMEOUT"));
    let message = body["message"].as_str().expect("typed message");
    assert!(
        message.contains("/v2/volumes") && message.contains("40ms"),
        "the timeout names the route and the effective limit: {message}"
    );

    let (status, _headers, body) = send(
        &app,
        request(Method::GET, "/v2/volumes/vol-bounded", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the inspect route has its own longer bound: {body}"
    );
    assert_eq!(body["volume_id"], json!("vol-bounded"));
}

#[tokio::test]
async fn a_timed_out_read_releases_its_concurrency_permit() {
    // The read timeout drops the handler future; the permit guard must drop
    // with it, or the default cap would leak one slot per timeout until every
    // later request is shed with 503.
    let provider = Arc::new(ControlledProvider::with_list_delay(Duration::from_millis(
        250,
    )));
    let (state, _dir) = setup(provider);
    let config = ApiConfig {
        read_timeout: Duration::from_millis(30),
        max_concurrent_requests: 1,
        ..base_config(1 << 20)
    };
    let app = router_with_config(state, &config);

    for attempt in 0..3 {
        let (status, _headers, body) =
            send(&app, request(Method::GET, "/v2/volumes", ADMIN_TOKEN)).await;
        assert_eq!(
            status,
            StatusCode::GATEWAY_TIMEOUT,
            "attempt {attempt} must reach the timeout, not 503: {body}"
        );
        assert_eq!(body["code"], json!("REQUEST_TIMEOUT"));
    }
}

#[tokio::test]
async fn journaled_mutation_is_not_aborted_by_the_read_timeout() {
    // The read timeout is far shorter than the provider's create delay. A
    // journaled mutation must still complete and record its outcome: the
    // timeout is a read-route bound, never a silent abort of journaled work.
    let provider = Arc::new(ControlledProvider::with_create_delay(
        Duration::from_millis(120),
    ));
    let (state, _dir) = setup(provider);
    // The tiny bound is set both as the default and as an override for the
    // very matched path (`/v2/volumes`) the POST uses: even then, a
    // mutating method must remain unbounded.
    let config = ApiConfig {
        read_timeout: Duration::from_millis(20),
        route_timeouts: RouteTimeouts {
            list_volumes: Some(Duration::from_millis(20)),
            ..RouteTimeouts::default()
        },
        ..base_config(1 << 20)
    };
    let app = router_with_config(state.clone(), &config);

    let create = serde_json::to_value(fixture_create_request("vol-slow-create", GIB))
        .expect("serialize create fixture");
    let (status, _headers, body) = send(
        &app,
        json_request(Method::POST, "/v2/volumes", ADMIN_TOKEN, &create),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "mutation must complete: {body}");
    assert_eq!(body["volume_id"], json!("vol-slow-create"));
    let records_after_first = journal_record_count(&state);
    assert!(
        records_after_first >= 2,
        "intent and outcome must both be journaled, saw {records_after_first}"
    );

    // Byte-identical replay returns the recorded outcome without re-running
    // the provider.
    let (replay_status, _headers, replay_body) = send(
        &app,
        json_request(Method::POST, "/v2/volumes", ADMIN_TOKEN, &create),
    )
    .await;
    assert_eq!(replay_status, StatusCode::OK);
    assert_eq!(replay_body, body);
    assert_eq!(journal_record_count(&state), records_after_first);
}

#[tokio::test(flavor = "current_thread")]
async fn readyz_snapshot_runs_off_the_runtime_worker() {
    // A stalled journal lock must not park the only runtime worker: while
    // `/readyz` waits for its blocking snapshot, `/healthz` must still run.
    let (state, _dir) = setup(Arc::new(ControlledProvider::new()));
    let app = router_with_config(state.clone(), &base_config(1 << 20));

    let held = Arc::clone(&state);
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = held.journal.lock().expect("journal lock");
        locked_tx.send(()).expect("signal lock");
        std::thread::sleep(Duration::from_millis(400));
    });
    locked_rx.recv().expect("journal lock acquired");

    let ready_app = app.clone();
    let ready = tokio::spawn(async move {
        send(&ready_app, request(Method::GET, "/readyz", ADMIN_TOKEN)).await
    });
    // Give the readyz task a chance to reach the (blocking) snapshot wait.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let started = Instant::now();
    let (health, _headers, body) = send(&app, request(Method::GET, "/healthz", ADMIN_TOKEN)).await;
    assert_eq!(health, StatusCode::OK, "{body}");
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "healthz waited {:?} behind the journal lock; the snapshot must run off-worker",
        started.elapsed()
    );

    let (ready_status, _headers, _body) = ready.await.expect("readyz task");
    assert!(
        matches!(
            ready_status,
            StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
        ),
        "readyz must answer, not hang: {ready_status}"
    );
    holder.join().expect("journal holder");
}

// ---------------------------------------------------------------------------
// Concurrency cap / backpressure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hung_provider_is_shed_and_cannot_exhaust_the_server() {
    let provider = Arc::new(ControlledProvider::hanging_list());
    let (state, _dir) = setup(provider.clone());
    let config = ApiConfig {
        read_timeout: Duration::from_millis(300),
        admission_wait_timeout: Duration::from_millis(50),
        max_concurrent_requests: 1,
        ..base_config(1 << 20)
    };
    let app = router_with_config(state, &config);

    // Register before spawning so the provider's entry is observable.
    let entered = provider.entered.clone();
    let arrived = entered.notified();
    let slow_app = app.clone();
    let slow = tokio::spawn(async move {
        send(&slow_app, request(Method::GET, "/v2/volumes", ADMIN_TOKEN)).await
    });
    arrived.await;

    // The single permit is held by the hung request: a second request is
    // shed promptly with a typed 503 and a Retry-After, not queued forever.
    let (status, headers, body) =
        send(&app, request(Method::GET, "/v2/volumes", ADMIN_TOKEN)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], json!("OVERLOADED"));
    assert_eq!(retry_after(&headers), "1");

    // Liveness bypasses admission control and stays available while the
    // provider is hung.
    let (health, _headers, health_body) =
        send(&app, request(Method::GET, "/healthz", ADMIN_TOKEN)).await;
    assert_eq!(health, StatusCode::OK, "healthz: {health_body}");

    // The hung call itself is bounded by the read timeout.
    let (slow_status, _headers, slow_body) = slow.await.expect("slow request task");
    assert_eq!(slow_status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(slow_body["code"], json!("REQUEST_TIMEOUT"));
}

// ---------------------------------------------------------------------------
// Per-principal rate limits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn per_principal_rate_limit_is_isolated_and_typed() {
    let provider = Arc::new(ControlledProvider::new());
    let (state, _dir) = setup_scoped(provider);
    let config = ApiConfig {
        rate_limit_per_second: 1,
        rate_limit_burst: 1,
        ..base_config(1 << 20)
    };
    let app = router_with_config(state, &config);

    let (first, _headers, _body) =
        send(&app, request(Method::GET, "/v2/volumes", READER_A_TOKEN)).await;
    assert_eq!(first, StatusCode::OK);

    let (limited, headers, body) =
        send(&app, request(Method::GET, "/v2/volumes", READER_A_TOKEN)).await;
    assert_eq!(limited, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["code"], json!("RATE_LIMITED"));
    assert!(
        retry_after(&headers)
            .parse::<u64>()
            .is_ok_and(|seconds| seconds >= 1),
        "Retry-After must carry a positive integer"
    );

    // A different principal has an independent bucket; the limit is not a
    // single global counter and never keys on a bearer token.
    let (other_principal, _headers, _body) =
        send(&app, request(Method::GET, "/v2/volumes", READER_B_TOKEN)).await;
    assert_eq!(other_principal, StatusCode::OK);

    // Health probes bypass per-principal limiting.
    let (health, _headers, _body) =
        send(&app, request(Method::GET, "/healthz", READER_A_TOKEN)).await;
    assert_eq!(health, StatusCode::OK);
}

#[tokio::test]
async fn credential_denials_are_charged_to_the_anonymous_bucket() {
    // Authentication runs before the rate-limit layer, so a denied credential
    // never reaches it. Without charging the denial here, brute force would
    // receive an unthrottled stream of 401s.
    let provider = Arc::new(ControlledProvider::new());
    let (state, _dir) = setup_scoped(provider);
    let config = ApiConfig {
        rate_limit_per_second: 1,
        rate_limit_burst: 1,
        ..base_config(1 << 20)
    };
    let app = router_with_config(state, &config);

    let (first, _headers, first_body) =
        send(&app, request(Method::GET, "/v2/volumes", "wrong-token")).await;
    assert_eq!(first, StatusCode::UNAUTHORIZED, "{first_body}");

    let (second, headers, second_body) =
        send(&app, request(Method::GET, "/v2/volumes", "wrong-token")).await;
    assert_eq!(second, StatusCode::TOO_MANY_REQUESTS, "{second_body}");
    assert_eq!(second_body["code"], json!("RATE_LIMITED"));
    assert_eq!(retry_after(&headers), "1");

    // A valid principal has an independent bucket, so the brute-force
    // attempt did not lock out legitimate traffic.
    let (legit, _headers, _body) =
        send(&app, request(Method::GET, "/v2/volumes", READER_A_TOKEN)).await;
    assert_eq!(legit, StatusCode::OK);
}

#[tokio::test]
async fn metrics_denials_are_charged_even_though_the_route_bypasses_admission() {
    // `/metrics` is exempt from the concurrency cap and the post-auth rate
    // limiter, but it still requires read auth when credentials are scoped.
    // It must not become an unthrottled token oracle: the admission handle
    // is installed before the exempt fast path so denials are charged.
    let provider = Arc::new(ControlledProvider::new());
    let (state, _dir) = setup_scoped(provider);
    let config = ApiConfig {
        rate_limit_per_second: 1,
        rate_limit_burst: 1,
        ..base_config(1 << 20)
    };
    let app = router_with_config(state, &config);

    let (first, _headers, first_body) =
        send(&app, request(Method::GET, "/metrics", "wrong-token")).await;
    assert_eq!(first, StatusCode::UNAUTHORIZED, "{first_body}");

    let (second, _headers, second_body) =
        send(&app, request(Method::GET, "/metrics", "wrong-token")).await;
    assert_eq!(
        second,
        StatusCode::TOO_MANY_REQUESTS,
        "an exempt route must still charge credential denials: {second_body}"
    );
    assert_eq!(second_body["code"], json!("RATE_LIMITED"));

    // The probes have no auth requirement, so they keep answering while the
    // anonymous bucket is exhausted (liveness is not locked out).
    let (health, _headers, _body) =
        send(&app, request(Method::GET, "/healthz", "wrong-token")).await;
    assert_eq!(health, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

async fn seed_volumes(provider: &FakeProvider, ids: &[&str]) {
    for id in ids {
        provider
            .create_volume(&fixture_create_request(id, GIB))
            .await
            .expect("seed volume");
    }
}

#[tokio::test]
async fn list_volumes_paginates_with_a_stable_opaque_cursor_and_legacy_default() {
    let provider = Arc::new(FakeProvider::new());
    seed_volumes(&provider, &["vol-c", "vol-a", "vol-b"]).await;
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let state = Arc::new(AppState::new(
        provider.clone(),
        None,
        journal,
        Some(ADMIN_TOKEN.to_owned()),
    ));
    let app = router_with_config(state, &base_config(1 << 20));

    // Legacy request without `limit`: all volumes, no truncation and no
    // cursor — an old client cannot silently lose entries.
    let (status, _headers, all) =
        send(&app, request(Method::GET, "/v2/volumes", ADMIN_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(all["volumes"].as_array().expect("volumes array").len(), 3);
    assert!(all.get("next_cursor").is_none());
    let ids: Vec<&str> = all["volumes"]
        .as_array()
        .expect("volumes")
        .iter()
        .map(|volume| volume["volume_id"].as_str().expect("volume id"))
        .collect();
    assert_eq!(ids, ["vol-a", "vol-b", "vol-c"], "stable volume_id order");

    // First bounded page.
    let (status, _headers, first) = send(
        &app,
        request(Method::GET, "/v2/volumes?limit=2", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first_ids: Vec<&str> = first["volumes"]
        .as_array()
        .expect("volumes")
        .iter()
        .map(|volume| volume["volume_id"].as_str().expect("volume id"))
        .collect();
    assert_eq!(first_ids, ["vol-a", "vol-b"]);
    let cursor = first["next_cursor"].as_str().expect("next_cursor");
    assert!(cursor.starts_with("v1."), "opaque versioned cursor");

    // Second page resumes strictly after the last served identity.
    let uri = format!("/v2/volumes?limit=2&cursor={cursor}");
    let (status, _headers, second) = send(&app, request(Method::GET, &uri, ADMIN_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    let second_ids: Vec<&str> = second["volumes"]
        .as_array()
        .expect("volumes")
        .iter()
        .map(|volume| volume["volume_id"].as_str().expect("volume id"))
        .collect();
    assert_eq!(second_ids, ["vol-c"]);
    assert!(second.get("next_cursor").is_none());

    // The cursor is a resume point, not a snapshot: adding an identity after
    // the cursor makes it visible on the next page, while one inserted before
    // the cursor is not re-served. No served identity is duplicated.
    seed_volumes(&provider, &["vol-0-before", "vol-d-after"]).await;
    let (status, _headers, mutated) = send(&app, request(Method::GET, &uri, ADMIN_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    let mutated_ids: Vec<&str> = mutated["volumes"]
        .as_array()
        .expect("volumes")
        .iter()
        .map(|volume| volume["volume_id"].as_str().expect("volume id"))
        .collect();
    assert_eq!(mutated_ids, ["vol-c", "vol-d-after"]);
    assert!(mutated.get("next_cursor").is_none());
}

#[tokio::test]
async fn cursor_without_limit_is_rejected() {
    let provider = Arc::new(FakeProvider::new());
    seed_volumes(&provider, &["vol-a", "vol-b"]).await;
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let state = Arc::new(AppState::new(
        provider,
        None,
        journal,
        Some(ADMIN_TOKEN.to_owned()),
    ));
    let app = router_with_config(state, &base_config(1 << 20));

    let (status, _headers, body) = send(
        &app,
        request(Method::GET, "/v2/volumes?cursor=v1.766f6c2d61", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], json!("INVALID_REQUEST"));
}

#[tokio::test]
async fn list_volumes_rejects_invalid_limits_and_cursors() {
    let provider = Arc::new(FakeProvider::new());
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let state = Arc::new(AppState::new(
        provider,
        None,
        journal,
        Some(ADMIN_TOKEN.to_owned()),
    ));
    let app = router_with_config(state, &base_config(1 << 20));

    for uri in [
        "/v2/volumes?limit=0",
        "/v2/volumes?limit=1001",
        "/v2/volumes?cursor=not-a-cursor",
        "/v2/volumes?cursor=v1.zz",
        "/v2/volumes?cursor=v1.20",
    ] {
        let (status, _headers, body) = send(&app, request(Method::GET, uri, ADMIN_TOKEN)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "uri {uri} must be rejected: {body}"
        );
        assert_eq!(body["code"], json!("INVALID_REQUEST"), "uri {uri}");
    }
}

// ---------------------------------------------------------------------------
// Body limits (sized and streamed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oversized_body_is_rejected_before_journaling() {
    let provider = Arc::new(FakeProvider::new());
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let state = Arc::new(AppState::new(
        provider,
        None,
        journal,
        Some(ADMIN_TOKEN.to_owned()),
    ));
    let app = router_with_config(state.clone(), &base_config(64));

    let create = serde_json::to_value(fixture_create_request("vol-too-big", GIB))
        .expect("serialize create fixture");
    assert!(create.to_string().len() > 64);
    let (status, _headers, body) = send(
        &app,
        json_request(Method::POST, "/v2/volumes", ADMIN_TOKEN, &create),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["code"], json!("PAYLOAD_TOO_LARGE"));
    assert_eq!(
        journal_record_count(&state),
        0,
        "the body limit must reject before any journal write"
    );
}

#[tokio::test]
async fn streamed_body_without_content_length_hits_the_same_limit() {
    let provider = Arc::new(FakeProvider::new());
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let state = Arc::new(AppState::new(
        provider,
        None,
        journal,
        Some(ADMIN_TOKEN.to_owned()),
    ));
    let app = router_with_config(state.clone(), &base_config(64));

    let chunks: Vec<Result<Bytes, std::io::Error>> =
        (0..8).map(|_| Ok(Bytes::from(vec![b'a'; 32]))).collect();
    let body = Body::from_stream(futures::stream::iter(chunks));
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v2/volumes")
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {ADMIN_TOKEN}")).expect("auth header"),
        )
        .body(body)
        .expect("build streamed request");

    let (status, _headers, response) = send(&app, request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(response["code"], json!("PAYLOAD_TOO_LARGE"));
    assert_eq!(journal_record_count(&state), 0);
}
