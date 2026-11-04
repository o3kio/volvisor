//! Cloud Hypervisor (CHV) consumer conformance (issue #48, Phase B slice B3).
//!
//! This test drives the exact consumer sequence a CHV-side integration uses,
//! through the real router, a real on-disk journal and the conformance
//! [`FakeProvider`]:
//!
//! 1. negotiate `/v2/capabilities` and require `attachment_state_report`;
//! 2. create a volume and attach it (`prepared`, provider-recorded);
//! 3. report `advertised`/`vmm_configured` (CHV's ceiling) and re-inspect;
//! 4. refuse `active` without `guest_confirmed` evidence (typed 400);
//! 5. fence a stale attachment generation (typed 409) without moving state;
//! 6. detach, then show an old-instance report is typed `NOT_FOUND`;
//! 7. replay the exact report after dropping/rebuilding `AppState` over the
//!    same journal, byte-identically;
//! 8. repeat the whole capability-gated failure against a provider that does
//!    not advertise the token: typed `NOT_FOUND`, replayed verbatim.
//!
//! The test is deliberately normal (not `#[ignore]`): it is CI-safe, uses no
//! loop devices, no real VMM and no network beyond the in-process router.
//!
//! Boundaries: everything asserted here is Volvisor-side contract behavior.
//! Whether CHV actually installs the frontend, reports truthfully, or ever
//! obtains positive guest evidence is CHV-side work. CHV v1 may not claim
//! `active`, and no CHV/Volvisor pair is pinned (see
//! `docs/integrations/chv.md`).

// Integration-test code: invariant assertions may use expect/unwrap.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::too_many_lines)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use volvisor_api::{ApiConfig, AppState, SharedState, router_with_config};
use volvisor_journal::Journal;
use volvisor_provider::FakeProvider;
use volvisor_provider::conformance::{
    fixture_attach_request, fixture_create_request, fixture_detach_request, fixture_report_request,
};
use volvisor_types::{AttachmentEvidence, AttachmentState, Capability, CapabilitySet};

const TOKEN: &str = "chv-consumer-test-token";
const REPORTER: &str = "chv";
const GIB: u64 = 1 << 30;
const VOLUME_ID: &str = "vol-chv-1";
const ATTACHMENT_ID: &str = "att-chv-1";

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
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("build JSON request")
}

/// Drive the router and keep the raw response text so a replay can be
/// asserted byte-for-byte, not just semantically.
async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Value, String) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let json = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, json, text)
}

fn state_over(journal_path: PathBuf, provider: Arc<FakeProvider>) -> SharedState {
    let journal = Journal::open(journal_path).expect("journal open");
    Arc::new(AppState::new(
        provider,
        None,
        journal,
        Some(TOKEN.to_owned()),
    ))
}

fn has_capability(capabilities: &Value, token: &str) -> bool {
    capabilities
        .as_array()
        .expect("capabilities array")
        .iter()
        .any(|capability| capability.as_str() == Some(token))
}

#[tokio::test]
async fn chv_consumer_sequence_is_capability_gated_and_replay_safe() {
    let dir = tempfile::tempdir().expect("tempdir");
    let journal_path = dir.path().join("journal");
    let provider = Arc::new(FakeProvider::new());
    let state = state_over(journal_path.clone(), provider);
    let app = router_with_config(Arc::clone(&state), &ApiConfig::default());

    // --- CHV side: capability negotiation happens before any report. ---
    let (status, capabilities, body) = send(&app, request(Method::GET, "/v2/capabilities")).await;
    assert_eq!(status, StatusCode::OK, "capabilities: {body}");
    assert!(
        has_capability(&capabilities["capabilities"], "attachment_state_report"),
        "CHV must negotiate the exact attachment_state_report token: {body}"
    );
    assert_eq!(
        capabilities["supported_classes"],
        json!(["native-local"]),
        "the fake serves the native-local P0 profile: {body}"
    );

    // --- Volvisor side: create and attach record `prepared`. ---
    let create = serde_json::to_value(fixture_create_request(VOLUME_ID, GIB))
        .expect("serialize create fixture");
    let (status, _, body) = send(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::OK, "create: {body}");

    let attach = serde_json::to_value(fixture_attach_request(VOLUME_ID, ATTACHMENT_ID, 1))
        .expect("serialize attach fixture");
    let attach_uri = format!("/v2/volumes/{VOLUME_ID}/attach");
    let (status, attached, body) =
        send(&app, json_request(Method::POST, &attach_uri, &attach)).await;
    assert_eq!(status, StatusCode::OK, "attach: {body}");
    assert_eq!(
        attached["state"],
        json!("prepared"),
        "attach is provider-recorded prepared, never a consumer claim: {body}"
    );
    let attachment_generation = attached["attachment_generation"]
        .as_u64()
        .expect("attachment generation");

    let inspect_uri = format!("/v2/volumes/{VOLUME_ID}");
    let (status, inspected, body) = send(&app, request(Method::GET, &inspect_uri)).await;
    assert_eq!(status, StatusCode::OK, "inspect: {body}");
    let view = &inspected["attachments"][0];
    assert_eq!(view["attachment_id"], json!(ATTACHMENT_ID));
    assert_eq!(view["state"], json!("prepared"));
    assert_eq!(view["state_revision"], json!(1));
    assert_eq!(
        view["attachment_generation"],
        json!(attachment_generation),
        "inspect exposes the detach-fencing generation"
    );
    assert!(
        view.get("evidence").is_none(),
        "prepared must not fabricate consumer evidence: {body}"
    );

    // --- CHV side: report the frontend as configured (`advertised`). ---
    // This is CHV v1's ceiling: it configured the returned virtio-blk
    // frontend into its VMM, but has no positive guest observation.
    let mut advertised_report = serde_json::to_value(fixture_report_request(
        ATTACHMENT_ID,
        attachment_generation,
        AttachmentState::Advertised,
        AttachmentEvidence::VmmConfigured,
    ))
    .expect("serialize report fixture");
    advertised_report["operation_id"] = json!("chv-op-advertised-1");
    advertised_report["reporter"] = json!(REPORTER);
    advertised_report["detail"] = json!("virtio-blk frontend configured in CHV");

    let report_uri = format!("/v2/volumes/{VOLUME_ID}/attachment-state");
    let (status, advertised_json, advertised_raw) = send(
        &app,
        json_request(Method::POST, &report_uri, &advertised_report),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "advertised report: {advertised_raw}"
    );
    assert_eq!(advertised_json["applied"], json!(true));
    assert_eq!(advertised_json["state"], json!("advertised"));
    assert_eq!(advertised_json["evidence"], json!("vmm_configured"));
    assert_eq!(advertised_json["reporter"], json!(REPORTER));
    assert_eq!(
        advertised_json["attachment_generation"],
        json!(attachment_generation),
        "a report must never advance the detach fence"
    );
    assert_eq!(
        advertised_json["state_revision"],
        json!(2),
        "an applied report advances state_revision by one"
    );
    assert!(
        advertised_json["recorded_at_ms"].is_u64(),
        "an applied report carries the server receipt time: {advertised_raw}"
    );

    // Re-observe instead of replaying: inspect projects the applied evidence.
    let (status, inspected, body) = send(&app, request(Method::GET, &inspect_uri)).await;
    assert_eq!(status, StatusCode::OK, "inspect after report: {body}");
    let view = &inspected["attachments"][0];
    assert_eq!(view["state"], json!("advertised"));
    assert_eq!(view["state_revision"], json!(2));
    assert_eq!(view["evidence"], json!("vmm_configured"));
    assert_eq!(view["reporter"], json!(REPORTER));
    assert_eq!(view["attachment_generation"], json!(attachment_generation));
    assert_eq!(view["recorded_at_ms"], advertised_json["recorded_at_ms"]);

    // --- CHV v1 must never claim `active` without guest evidence. ---
    // `vmm_configured` + `active` is an unsupported pairing; Volvisor refuses
    // it pre-journal with a typed INVALID_REQUEST and changes no state.
    let mut active_without_guest = advertised_report.clone();
    active_without_guest["operation_id"] = json!("chv-op-active-without-guest");
    active_without_guest["state"] = json!("active");
    let (status, rejected, raw) = send(
        &app,
        json_request(Method::POST, &report_uri, &active_without_guest),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "CHV v1 must never claim active without guest_confirmed evidence: {raw}"
    );
    assert_eq!(rejected["code"], json!("INVALID_REQUEST"));

    let (status, inspected, body) = send(&app, request(Method::GET, &inspect_uri)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "inspect after active refusal: {body}"
    );
    assert_eq!(
        inspected["attachments"][0]["state"],
        json!("advertised"),
        "a refused report must not move the state"
    );
    assert_eq!(inspected["attachments"][0]["state_revision"], json!(2));

    // --- Stale attachment instance: typed conflict, no state movement. ---
    // A delayed report from an old instance carries an expected generation
    // that no longer matches; it is fenced exactly like a stale detach.
    let mut stale_report = advertised_report.clone();
    stale_report["operation_id"] = json!("chv-op-stale-generation");
    stale_report["expected_attachment_generation"] = json!(attachment_generation + 1);
    let (status, stale_json, raw) =
        send(&app, json_request(Method::POST, &report_uri, &stale_report)).await;
    assert_eq!(status, StatusCode::CONFLICT, "stale report: {raw}");
    assert_eq!(stale_json["code"], json!("STALE_GENERATION"));

    let (status, inspected, body) = send(&app, request(Method::GET, &inspect_uri)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "inspect after stale refusal: {body}"
    );
    assert_eq!(
        inspected["attachments"][0]["state"],
        json!("advertised"),
        "a stale-generation refusal must not move the state"
    );
    assert_eq!(inspected["attachments"][0]["state_revision"], json!(2));

    // --- Detach fences the consumption instance; reports do not move it. ---
    let mut detach =
        serde_json::to_value(fixture_detach_request(ATTACHMENT_ID, attachment_generation))
            .expect("serialize detach fixture");
    detach["attachment_id"] = json!(ATTACHMENT_ID);
    let detach_uri = format!("/v2/volumes/{VOLUME_ID}/detach");
    let (status, detached, raw) =
        send(&app, json_request(Method::POST, &detach_uri, &detach)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "detach at the report-unchanged generation: {raw}"
    );
    assert_eq!(detached["attachments"], json!([]));

    // A *new* report against the detached instance is NOT_FOUND; the old
    // instance has no state left to move.
    let mut after_detach = advertised_report.clone();
    after_detach["operation_id"] = json!("chv-op-after-detach");
    let (status, gone, raw) =
        send(&app, json_request(Method::POST, &report_uri, &after_detach)).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a report from a detached instance: {raw}"
    );
    assert_eq!(gone["code"], json!("NOT_FOUND"));

    let (status, inspected, body) = send(&app, request(Method::GET, &inspect_uri)).await;
    assert_eq!(status, StatusCode::OK, "inspect after detach: {body}");
    assert_eq!(
        inspected["attachments"],
        json!([]),
        "a detached instance has no projection to move"
    );

    // --- Restart boundary: the journal replays the recorded report. ---
    // The FakeProvider's world is in-memory, so this restart deliberately
    // does not reconstruct volumes/attachments. The replay below is served by
    // the journal's recorded outcome (the contract's replay rule), not by
    // provider state; a production daemon rebuilds provider state by
    // reconcile. This test asserts the journal-replay boundary only.
    drop(app);
    drop(state);
    let restarted_provider = Arc::new(FakeProvider::new());
    let restarted_state = state_over(journal_path, restarted_provider);
    let restarted_app = router_with_config(restarted_state, &ApiConfig::default());

    let (status, replayed, replayed_raw) = send(
        &restarted_app,
        json_request(Method::POST, &report_uri, &advertised_report),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "replayed report after restart: {replayed_raw}"
    );
    assert_eq!(
        replayed_raw, advertised_raw,
        "the same operation_id and payload must replay byte-identically across restart"
    );
    assert_eq!(replayed, advertised_json);

    // The fresh provider has no rebuilt in-memory attachment: a *different*
    // operation id is NOT_FOUND. This keeps the replay assertion above
    // honest about what the restart did (and did not) reconstruct.
    let mut post_restart_fresh = advertised_report.clone();
    post_restart_fresh["operation_id"] = json!("chv-op-post-restart-fresh");
    let (status, missing, raw) = send(
        &restarted_app,
        json_request(Method::POST, &report_uri, &post_restart_fresh),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "no provider reconcile in this test: {raw}"
    );
    assert_eq!(missing["code"], json!("NOT_FOUND"));
}

#[tokio::test]
async fn chv_consumer_without_the_capability_gets_a_replayed_typed_404() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(FakeProvider::new().with_capabilities(CapabilitySet::of([
        Capability::Create,
        Capability::Attach,
        Capability::Resize,
    ])));
    let state = state_over(dir.path().join("journal"), provider);
    let app = router_with_config(state, &ApiConfig::default());

    // Capability negotiation is the consumer's first step: the token is
    // absent, so CHV must treat attachment reporting as unsupported rather
    // than as a crash/retry loop.
    let (status, capabilities, body) = send(&app, request(Method::GET, "/v2/capabilities")).await;
    assert_eq!(status, StatusCode::OK, "capabilities: {body}");
    assert!(
        !has_capability(&capabilities["capabilities"], "attachment_state_report"),
        "the restricted provider must not advertise the token: {body}"
    );

    let create = serde_json::to_value(fixture_create_request(VOLUME_ID, GIB))
        .expect("serialize create fixture");
    let (status, _, body) = send(&app, json_request(Method::POST, "/v2/volumes", &create)).await;
    assert_eq!(status, StatusCode::OK, "create: {body}");

    let attach = serde_json::to_value(fixture_attach_request(VOLUME_ID, ATTACHMENT_ID, 1))
        .expect("serialize attach fixture");
    let attach_uri = format!("/v2/volumes/{VOLUME_ID}/attach");
    let (status, attached, body) =
        send(&app, json_request(Method::POST, &attach_uri, &attach)).await;
    assert_eq!(status, StatusCode::OK, "attach: {body}");
    let attachment_generation = attached["attachment_generation"]
        .as_u64()
        .expect("attachment generation");

    // Volvisor's default provider behavior for an absent capability is a
    // typed NOT_FOUND, journaled like any other terminal outcome.
    let mut report = serde_json::to_value(fixture_report_request(
        ATTACHMENT_ID,
        attachment_generation,
        AttachmentState::Advertised,
        AttachmentEvidence::VmmConfigured,
    ))
    .expect("serialize report fixture");
    report["operation_id"] = json!("chv-op-no-capability");
    report["reporter"] = json!(REPORTER);

    let report_uri = format!("/v2/volumes/{VOLUME_ID}/attachment-state");
    let (status, first, first_raw) =
        send(&app, json_request(Method::POST, &report_uri, &report)).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a provider without attachment_state_report must answer the typed 404: {first_raw}"
    );
    assert_eq!(first["code"], json!("NOT_FOUND"));

    // Replaying the same operation id is byte-identical from CHV's view:
    // the refusal is a recorded outcome, not a fresh provider decision.
    let (status, replayed, replayed_raw) =
        send(&app, json_request(Method::POST, &report_uri, &report)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        replayed_raw, first_raw,
        "the typed 404 must replay verbatim"
    );
    assert_eq!(replayed, first);

    // Capability absence is not a state transition: the attachment stays
    // provider-recorded `prepared` at revision 1.
    let inspect_uri = format!("/v2/volumes/{VOLUME_ID}");
    let (status, inspected, body) = send(&app, request(Method::GET, &inspect_uri)).await;
    assert_eq!(status, StatusCode::OK, "inspect: {body}");
    let view = &inspected["attachments"][0];
    assert_eq!(view["state"], json!("prepared"));
    assert_eq!(view["state_revision"], json!(1));
    assert_eq!(view["attachment_generation"], json!(attachment_generation));
}
