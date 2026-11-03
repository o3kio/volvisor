//! Router construction: the Volume API v2 HTTP surface.
//!
//! All routes share one [`crate::AppState`]. The
//! [`crate::auth::authorize_and_audit`] middleware authenticates every
//! request exactly once, enforces the route's role rule and emits one
//! structured audit event; the role extractors are defense in depth. A
//! metrics middleware records `http_requests_total` (route pattern + status
//! code) for every matched route.
//!
//! Admission control (issue #45) sits around the same routes: a global
//! concurrency cap and a read-only route timeout, then a per-principal rate
//! limit after the principal is resolved. A request body limit from
//! [`crate::ApiConfig`] guards every endpoint, including streamed bodies.
//! `<healthz>` and `/metrics` bypass admission control so probes and
//! monitoring stay available while load is shed; reads are open only in the
//! documented legacy/tokenless posture (see [`crate::auth`]).

use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};

use crate::ApiConfig;
use crate::admission::{self, Admission};
use crate::auth;
use crate::handlers;
use crate::metrics::Metrics;
use crate::peer;
use crate::state::SharedState;

/// Build the Volume API v2 router with the legacy body-limit argument and
/// otherwise-default admission control.
///
/// Kept for existing callers (the campaign rig and daemon tests). New code
/// should use [`router_with_config`] so timeouts, the concurrency cap and
/// per-principal limits are explicit.
pub fn router(state: SharedState, max_body_bytes: usize) -> Router {
    let config = ApiConfig {
        max_body_bytes,
        ..ApiConfig::default()
    };
    router_with_config(state, &config)
}

/// Build the Volume API v2 router.
///
/// `config.max_body_bytes` bounds the accepted request body size (requests
/// beyond the limit — including streamed bodies without a
/// `Content-Length` — are rejected with `PAYLOAD_TOO_LARGE` before parsing).
/// `config` also supplies the read-only route timeout, the global
/// concurrency cap and the per-principal rate limit. See
/// `docs/admission-control.md` for the long-operation model: journaled
/// mutations are never wrapped in a wall-clock timeout.
#[allow(clippy::too_many_lines)] // the route table is deliberately one readable block
pub fn router_with_config(state: SharedState, config: &ApiConfig) -> Router {
    let admission = Admission::new(*config);
    Router::new()
        .route(
            "/v2/volumes",
            post(handlers::create_volume).get(handlers::list_volumes),
        )
        .route(
            "/v2/volumes/{volume_id}",
            get(handlers::inspect_volume).delete(handlers::delete_volume),
        )
        .route(
            "/v2/volumes/{volume_id}/attach",
            post(handlers::attach_volume),
        )
        .route(
            "/v2/volumes/{volume_id}/detach",
            post(handlers::detach_volume),
        )
        .route(
            "/v2/volumes/{volume_id}/attachment-state",
            post(handlers::report_attachment_state),
        )
        .route("/v2/volumes/{volume_id}/grow", post(handlers::grow_volume))
        .route(
            "/v2/volumes/{volume_id}/move-backing",
            post(handlers::move_volume_backing),
        )
        .route("/v2/capabilities", get(handlers::capabilities))
        // Admin surface (device enrollment): every route requires the admin
        // token, GET included — device inventory is privileged. Providers
        // without an admin surface serve a typed 404 on these routes.
        .route("/v2/admin/devices", get(handlers::admin_list_devices))
        .route(
            "/v2/admin/devices/{device_id}/claim",
            post(handlers::claim_device),
        )
        .route(
            "/v2/admin/devices/{device_id}/release",
            post(handlers::release_device),
        )
        // Device lending (P9-A, the rook-cell contract's lending
        // surface): the lend/reclaim/resolve mutations and the
        // read-only ownership answer, over claimed devices. Admin
        // token required like every admin route; providers without a
        // lending surface serve the typed 404.
        .route(
            "/v2/admin/devices/{device_id}/lend",
            post(handlers::lend_device),
        )
        .route(
            "/v2/admin/devices/{device_id}/reclaim",
            post(handlers::reclaim_device),
        )
        .route(
            "/v2/admin/devices/{device_id}/resolve-quarantine",
            post(handlers::resolve_quarantine),
        )
        .route(
            "/v2/admin/devices/{device_id}/ownership",
            get(handlers::device_ownership),
        )
        // Nearline unplanned failover (P4a plan §6): the adopt-and-promote
        // admin operation. Admin token required; providers without an
        // adoption surface serve the typed 404.
        .route(
            "/v2/admin/nearline/{volume_id}/adopt",
            post(handlers::adopt_volume),
        )
        // Nearline residue cleanup (P4b plan §6): clear the cut marker
        // of an interrupted handoff — the operator's typed, journaled
        // escape hatch for the Secondary-with-marker residue. Admin
        // token required; providers without a handoff surface serve
        // the typed 404.
        .route(
            "/v2/admin/nearline/{volume_id}/clear-cut-marker",
            post(handlers::clear_cut_marker),
        )
        // Consumer-facing mobility (P4b plan §6, stage B2): the four
        // journaled migration routes plus the read-only eligibility
        // check. Admin token required like every mutation; a daemon
        // without the surfaces serves the typed 404s.
        .route(
            "/v2/vms/{vm_id}/check-mobility",
            post(handlers::check_mobility),
        )
        .route("/v2/migrations", post(handlers::prepare_migration))
        .route(
            "/v2/migrations/{migration_id}",
            get(handlers::observe_migration),
        )
        .route(
            "/v2/migrations/{migration_id}/transfer",
            post(handlers::transfer_migration),
        )
        .route(
            "/v2/migrations/{migration_id}/abort",
            post(handlers::abort_migration),
        )
        // Internal peer routes (P4b plan §6, stage B2): the
        // destination-side surface, guarded by the daemon-to-daemon
        // peer credential (distinct from the admin token — see
        // `peer::RequirePeer`). A daemon without the peer context
        // serves the typed 404 on every route.
        .route("/v2/internal/peer/prepare", post(peer::prepare))
        .route(
            "/v2/internal/peer/verify-lineage",
            post(peer::verify_lineage),
        )
        .route("/v2/internal/peer/grant", post(peer::grant))
        .route("/v2/internal/peer/restore-vm", post(peer::restore_vm))
        .route("/v2/internal/peer/discard", post(peer::discard))
        .route("/v2/internal/peer/health", get(peer::health))
        // Liveness is exposed on /healthz (task requirement); the
        // implementation plan section 5.5 also lists /v2/healthz, so both
        // spellings serve the same liveness-only response.
        .route("/healthz", get(handlers::healthz))
        .route("/v2/healthz", get(handlers::healthz))
        // Readiness is distinct from liveness: it requires journal replay,
        // provider reconcile and startup completion, and (when a real probe
        // is configured) witness reachability. Both probe endpoints bypass
        // admission control exactly like `/healthz`.
        .route("/readyz", get(handlers::readyz))
        .route("/v2/readyz", get(handlers::readyz))
        .route("/metrics", get(handlers::metrics))
        .fallback(handlers::not_found)
        // Authentication/authorization/audit runs inside the metrics layer so
        // denied requests are still counted; `MatchedPath` is resolved by
        // then, which is what the route access matrix keys on.
        // Layers are applied bottom-to-top: the last call is outermost.
        // Request order is metrics -> admission guard -> auth -> rate
        // limit -> body limit -> handler.
        .layer(DefaultBodyLimit::max(config.max_body_bytes))
        .layer(middleware::from_fn_with_state(
            admission.clone(),
            admission::rate_limit,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::authorize_and_audit,
        ))
        .layer(middleware::from_fn_with_state(admission, admission::guard))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            record_http_request,
        ))
        .with_state(state)
}

/// Decrements the in-flight gauge when the request future completes or is
/// dropped, so a cancellation cannot leave the gauge permanently elevated.
struct InFlightGuard(Arc<Metrics>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.request_finished();
    }
}

/// Middleware recording `http_requests_total{route,code}` and
/// `http_request_duration_seconds{route,class}` and maintaining
/// `http_requests_in_flight`.
///
/// The route label is the matched route *pattern* (e.g.
/// `/v2/volumes/{volume_id}`) when routing resolved one. Unmatched requests
/// (the fallback) are recorded under the fixed `<unmatched>` label: the raw
/// path may contain consumer ids and must never become an unbounded label.
async fn record_http_request(
    State(state): State<SharedState>,
    req: Request,
    next: Next,
) -> Response {
    const UNMATCHED_ROUTE: &str = "<unmatched>";
    let route = req.extensions().get::<MatchedPath>().map_or_else(
        || UNMATCHED_ROUTE.to_owned(),
        |matched| matched.as_str().to_owned(),
    );
    state.metrics.request_started();
    let _guard = InFlightGuard(Arc::clone(&state.metrics));
    let started = Instant::now();
    let response = next.run(req).await;
    state
        .metrics
        .record_http(&route, response.status().as_u16(), started.elapsed());
    response
}
