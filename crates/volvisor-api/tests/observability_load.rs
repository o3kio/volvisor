//! Ignored observability load test (issue #44 evidence).
//!
//! It drives the real router over `tower::oneshot`, with a provider wrapper
//! that parks `vol-slow-*` inspections until released, so the in-flight gauge
//! can be observed mid-request rather than only after the fact.
//!
//! Run explicitly:
//!
//! ```text
//! cargo test -p volvisor-api --test observability_load -- --ignored --nocapture
//! ```
//!
//! The assertions cover: HTTP latency/counter growth, provider-call latency,
//! the in-flight gauge rising and returning to its scrape-only baseline, the
//! journal record/size gauges, and bounded label cardinality under many
//! unique request paths and ids.

// Integration-test code: invariant assertions may use expect/unwrap. The load
// test stays one sequential narrative because each phase observes the state
// the previous phase produced.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::too_many_lines)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use tokio::sync::Semaphore;
use tower::ServiceExt;
use volvisor_api::{ApiConfig, AppState, SharedState, router_with_config};
use volvisor_journal::Journal;
use volvisor_provider::FakeProvider;
use volvisor_provider::VolumeProvider;
use volvisor_types::domain::VolumeClass;
use volvisor_types::request::{
    AttachVolumeRequest, AttachVolumeResponse, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, GrowVolumeRequest, GrowVolumeResponse, InspectVolumeResponse,
    ReportAttachmentStateRequest, ReportAttachmentStateResponse,
};
use volvisor_types::{ApiError, AttachmentId, CapabilitySet, ProjectId, VolumeId};

const ADMIN_TOKEN: &str = "load-admin-token";

/// Delegating provider whose `vol-slow-*` inspections park until the test
/// releases the semaphore. Every other call goes straight to the fake.
struct SlowProvider {
    inner: Arc<FakeProvider>,
    entered: Arc<AtomicUsize>,
    release: Arc<Semaphore>,
}

#[async_trait]
impl VolumeProvider for SlowProvider {
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
        self.inner.create_volume(req).await
    }

    async fn inspect_volume(&self, id: &VolumeId) -> Result<InspectVolumeResponse, ApiError> {
        if id.as_str().starts_with("vol-slow-") {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let permit = self
                .release
                .acquire()
                .await
                .expect("release semaphore open");
            drop(permit);
        }
        self.inner.inspect_volume(id).await
    }

    async fn list_volumes(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<InspectVolumeResponse>, ApiError> {
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
        attachment_id: &AttachmentId,
        req: &DetachVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError> {
        self.inner
            .detach_volume(volume_id, attachment_id, req)
            .await
    }

    async fn report_attachment_state(
        &self,
        volume_id: &VolumeId,
        req: &ReportAttachmentStateRequest,
    ) -> Result<ReportAttachmentStateResponse, ApiError> {
        self.inner.report_attachment_state(volume_id, req).await
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

fn request(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

fn authorized(method: Method, uri: &str, body: String) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("request")
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn scrape(app: &Router) -> String {
    let (status, body) = send(app, request(Method::GET, "/metrics")).await;
    assert_eq!(status, StatusCode::OK, "metrics: {body}");
    body
}

fn gauge_value(metrics: &str, name: &str) -> Option<u64> {
    metrics.lines().find_map(|line| {
        let rest = line.strip_prefix(&format!("{name} "))?;
        rest.parse().ok()
    })
}

fn route_labels(metrics: &str) -> BTreeSet<String> {
    metrics
        .lines()
        .filter(|line| line.starts_with("http_requests_total{"))
        .filter_map(|line| {
            let start = line.find("route=\"")? + "route=\"".len();
            let end = line[start..].find('"')? + start;
            Some(line[start..end].to_owned())
        })
        .collect()
}

#[tokio::test]
#[ignore = "observability load test; run explicitly for issue #44 evidence"]
async fn histograms_gauges_move_and_cardinality_stays_bounded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = Journal::open(dir.path().join("journal")).expect("journal");
    let fake = Arc::new(FakeProvider::new());
    let entered = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let provider = Arc::new(SlowProvider {
        inner: Arc::clone(&fake),
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let state: SharedState = Arc::new(AppState::new(
        provider,
        None,
        journal,
        Some(ADMIN_TOKEN.to_owned()),
    ));
    state.mark_ready();
    let app = router_with_config(Arc::clone(&state), &ApiConfig::default());

    // One real mutation: provider latency histogram + journal gauges.
    let create = serde_json::json!({
        "api_version": "volvisor.volume.v2",
        "operation_id": "op-load-create",
        "project_id": "tenant-load",
        "volume_id": "vol-load-created",
        "class": "native-local",
        "size_bytes": 1_073_741_824_u64,
    });
    let (status, body) = send(
        &app,
        authorized(
            Method::POST,
            "/v2/volumes",
            serde_json::to_string(&create).expect("create"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create: {body}");

    // Many unique ids on a matched route, and many unique raw paths on the
    // fallback: neither may become a label value.
    for index in 0..200 {
        let (status, _) = send(
            &app,
            request(Method::GET, &format!("/v2/volumes/vol-load-{index}")),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    for index in 0..50 {
        let (status, _) = send(
            &app,
            request(Method::GET, &format!("/unmatched-load-{index}")),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // Park slow requests and observe the in-flight gauge while they wait.
    let mut tasks = Vec::new();
    for index in 0..8 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let (status, _) = send(
                &app,
                request(Method::GET, &format!("/v2/volumes/vol-slow-{index}")),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }));
    }
    for _ in 0..400 {
        if entered.load(Ordering::SeqCst) >= 8 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        entered.load(Ordering::SeqCst),
        8,
        "all slow requests must reach the parked provider"
    );
    let during = scrape(&app).await;
    let in_flight = gauge_value(&during, "http_requests_in_flight").expect("in-flight gauge");
    assert!(
        in_flight >= 8,
        "in-flight gauge must include the parked requests plus the scrape: {in_flight}"
    );
    release.add_permits(8);
    for task in tasks {
        task.await.expect("slow request task");
    }

    // After release the gauge returns to its scrape-only baseline (the
    // `/metrics` request itself is in flight while it renders).
    let after = scrape(&app).await;
    assert_eq!(
        gauge_value(&after, "http_requests_in_flight"),
        Some(1),
        "in-flight gauge must return to the scrape-only baseline"
    );

    // HTTP histogram: 200 matched-route 404s and 50 fallback 404s.
    assert!(
        after.contains(
            "http_request_duration_seconds_count{class=\"4xx\",\
             route=\"/v2/volumes/{volume_id}\"} 208"
        ),
        "matched-route histogram must count 200 load reads + 8 parked reads: {after}"
    );
    assert!(
        after.contains(
            "http_request_duration_seconds_count{class=\"4xx\",route=\"<unmatched>\"} 50"
        ),
        "fallback histogram must use the fixed <unmatched> label: {after}"
    );
    assert!(
        after.contains("http_requests_total{code=\"404\",route=\"<unmatched>\"} 50"),
        "fallback counter must use the fixed <unmatched> label: {after}"
    );

    // Provider-call histogram: exactly one executed create.
    assert!(
        after.contains(
            "provider_operation_duration_seconds_count{kind=\"create_volume\",\
             outcome=\"success\"} 1"
        ),
        "provider histogram must observe the executed create: {after}"
    );

    // Journal gauges are sampled from the real journal (intent + outcome).
    assert_eq!(
        gauge_value(&after, "journal_records"),
        Some(2),
        "one create journals exactly an intent and an outcome: {after}"
    );
    assert!(
        after.contains("journal_open 1"),
        "journal must be open: {after}"
    );
    let log_bytes = gauge_value(&after, "journal_log_bytes").expect("journal log bytes");
    assert!(log_bytes > 0, "journal log must have bytes: {log_bytes}");

    // Cardinality: a fixed small route set, no raw path/id labels.
    let routes = route_labels(&after);
    assert!(
        routes.len() <= 40,
        "route label cardinality must stay bounded: {routes:?}"
    );
    assert!(routes.contains("/v2/volumes/{volume_id}"));
    assert!(routes.contains("<unmatched>"));
    assert!(
        !after.contains("vol-load-0") && !after.contains("unmatched-load-0"),
        "raw paths/ids must never appear in metric labels"
    );

    // Readiness is served by the same router and reflects startup.
    let (status, body) = send(&app, request(Method::GET, "/readyz")).await;
    assert_eq!(status, StatusCode::OK, "readyz: {body}");
    assert!(body.contains("\"status\":\"ready\""), "readyz: {body}");
    assert!(
        body.contains("\"witness\":\"not_probed\""),
        "no witness configured must be reported honestly: {body}"
    );
}
