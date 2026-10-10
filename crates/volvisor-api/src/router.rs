//! Router construction: the Volume API v2 HTTP surface.
//!
//! All routes share one [`crate::AppState`]. A metrics middleware records
//! `http_requests_total` (route pattern + status code) for every matched
//! route; a request body limit from [`crate::ApiConfig`] guards every
//! endpoint. Mutating endpoints and the whole `/v2/admin` surface (device
//! inventory is privileged, `GET` included) enforce admin bearer auth
//! through the [`crate::extract::RequireAdmin`] extractor (see its
//! documentation for the fail-closed tokenless behavior and why the
//! remaining `GET` routes are open in P0).

use axum::Router;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};

use crate::handlers;
use crate::peer;
use crate::state::SharedState;

/// Build the Volume API v2 router.
///
/// `max_body_bytes` bounds the accepted request body size (see
/// [`crate::ApiConfig`]; requests beyond the limit are rejected before
/// parsing).
pub fn router(state: SharedState, max_body_bytes: usize) -> Router {
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
        .route("/v2/volumes/{volume_id}/grow", post(handlers::grow_volume))
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
        .route("/v2/internal/peer/grant", post(peer::grant))
        .route("/v2/internal/peer/restore-vm", post(peer::restore_vm))
        .route("/v2/internal/peer/discard", post(peer::discard))
        .route("/v2/internal/peer/health", get(peer::health))
        // Liveness is exposed on /healthz (task requirement); the
        // implementation plan section 5.5 also lists /v2/healthz, so both
        // spellings serve the same liveness-only response.
        .route("/healthz", get(handlers::healthz))
        .route("/v2/healthz", get(handlers::healthz))
        .route("/metrics", get(handlers::metrics))
        .fallback(handlers::not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            record_http_request,
        ))
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(state)
}

/// Middleware counting served requests in `http_requests_total{route,code}`.
///
/// The label is the matched route *pattern* (e.g. `/v2/volumes/{volume_id}`)
/// when routing resolved one, falling back to the raw path.
async fn record_http_request(
    State(state): State<SharedState>,
    req: Request,
    next: Next,
) -> Response {
    let route = req.extensions().get::<MatchedPath>().map_or_else(
        || req.uri().path().to_owned(),
        |matched| matched.as_str().to_owned(),
    );
    let response = next.run(req).await;
    state
        .metrics
        .record_http(&route, response.status().as_u16());
    response
}
