//! # Witness HTTP surface
//!
//! axum router exposing the registry over HTTP/JSON on the same
//! conventions as the Volume API daemon: bearer-token authentication
//! (fail-closed when no token is configured — the tokenless mode is a
//! loopback-only dev/test convenience the *binder* enforces),
//! contract-shaped error bodies, and every core operation serialized
//! through one mutex (the journal requires `&mut`).
//!
//! Time is injected: handlers pass `now_secs` from the state's clock
//! closure, so integration tests drive expiry and fence windows
//! deterministically without sleeping.
//!
//! ## Caller identity (P4b plan §4 W8)
//!
//! The presented bearer token is resolved to a [`CallerIdentity`]:
//! a match in the configured host-token map yields `Host(id)`, the
//! legacy shared token yields `Legacy`. Resolution order is
//! host-tokens first, then the admin token; no match is the same
//! fail-closed 401 as before. `Legacy` may only read (`inspect`,
//! `/healthz`); every mutating route passes the resolved identity
//! into the authority core, which refuses a caller not bound to the
//! holder the request asserts with the typed `FORBIDDEN` refusal.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use volvisor_types::HostId;
use volvisor_types::VolumeId;
use volvisor_types::error::{ApiError, ApiErrorBody};

use crate::proto::{
    CallerIdentity, GrantRequest, GrantSetRequest, RecordBarrierRequest, RegisterRequest,
    RenewRequest, RevokeRequest, RevokeSetRequest, VoidBarrierRequest, WITNESS_PROTOCOL_VERSION,
    WitnessError,
};
use crate::registry::WitnessCore;

/// Shared witness server state.
pub struct WitnessServerState {
    core: std::sync::Mutex<WitnessCore>,
    admin_token: Option<String>,
    /// Per-host credentials (W8): host-id string → token. Keys are
    /// validated as `HostId`s by [`crate::config::WitnessConfig`];
    /// the raw string form is kept so this state stays infallible to
    /// build (an unvalidated key simply never matches a resolved
    /// identity and is never consulted for `HostId` equality — the
    /// `HostId` is constructed only on a token match).
    host_tokens: BTreeMap<String, String>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl WitnessServerState {
    /// Build server state with the real system clock.
    #[must_use]
    pub fn new(
        core: WitnessCore,
        admin_token: Option<String>,
        host_tokens: BTreeMap<String, String>,
    ) -> Self {
        Self::with_clock(core, admin_token, host_tokens, Arc::new(system_now_unix))
    }

    /// Build server state with an injected clock (tests; also usable by
    /// embedders with a disciplined time source).
    #[must_use]
    pub fn with_clock(
        core: WitnessCore,
        admin_token: Option<String>,
        host_tokens: BTreeMap<String, String>,
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        Self {
            core: std::sync::Mutex::new(core),
            admin_token,
            host_tokens,
            now,
        }
    }

    /// Current witness-clock unix seconds.
    #[must_use]
    pub fn now_secs(&self) -> u64 {
        (self.now)()
    }

    /// Resolve the presented bearer token to a caller identity (W8).
    /// Host credentials are consulted first, then the legacy admin
    /// token; `None` is the fail-closed transport 401.
    #[must_use]
    fn resolve_identity(&self, presented: &str) -> Option<CallerIdentity> {
        // Host tokens first: a deployment that (mis)configures the
        // same string as a host token and the admin token resolves it
        // as the host; WitnessConfig::validate refuses that overlap.
        for (host, token) in &self.host_tokens {
            if constant_time_eq(presented.as_bytes(), token.as_bytes()) {
                return HostId::new(host).ok().map(CallerIdentity::Host);
            }
        }
        self.admin_token
            .as_deref()
            .filter(|token| constant_time_eq(presented.as_bytes(), token.as_bytes()))
            .map(|_| CallerIdentity::Legacy)
    }
}

/// Real system clock: unix seconds (0 on a pre-epoch clock, never a
/// panic).
#[must_use]
pub fn system_now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The witness router: the volume-scoped routes under
/// `/v1/volumes/{volume_id}` plus the batch routes under `/v1/batch`
/// (W10 — not volume-scoped; the member volumes travel in the body).
pub fn router(state: Arc<WitnessServerState>) -> Router {
    Router::new()
        .route("/v1/volumes/{volume_id}/register", post(register_handler))
        .route("/v1/volumes/{volume_id}/grant", post(grant_handler))
        .route("/v1/volumes/{volume_id}/renew", post(renew_handler))
        .route("/v1/volumes/{volume_id}/revoke", post(revoke_handler))
        .route(
            "/v1/volumes/{volume_id}/record-barrier",
            post(record_barrier_handler),
        )
        .route(
            "/v1/volumes/{volume_id}/void-barrier",
            post(void_barrier_handler),
        )
        .route("/v1/batch/revoke-set", post(revoke_set_handler))
        .route("/v1/batch/grant-set", post(grant_set_handler))
        .route("/v1/volumes/{volume_id}", get(inspect_handler))
        .route("/healthz", get(healthz_handler))
        // An explicit, small body limit (the witness payloads are tiny;
        // the default would silently accept megabytes).
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .with_state(state)
}

async fn healthz_handler() -> &'static str {
    "ok"
}

/// Path-volume handler plumbing shared by the mutating routes: validate
/// the id, check the protocol version, lock the registry, run the core
/// operation with the resolved caller identity, shape the response.
macro_rules! witness_handler {
    ($name:ident, $request:ty, $core_op:ident) => {
        async fn $name(
            State(state): State<Arc<WitnessServerState>>,
            Path(volume): Path<String>,
            auth: Caller,
            body: Result<Json<$request>, JsonRejection>,
        ) -> Response {
            let volume_id = match VolumeId::new(volume) {
                Ok(id) => id,
                Err(err) => return api_error_response(err),
            };
            let request = match body {
                Ok(Json(request)) => request,
                Err(rejection) => {
                    return api_error_response(ApiError::invalid_request(format!(
                        "invalid request body: {}",
                        rejection.body_text()
                    )));
                }
            };
            if request.protocol_version != WITNESS_PROTOCOL_VERSION {
                return api_error_response(ApiError::invalid_request(format!(
                    "unsupported witness protocol version {} (expected {})",
                    request.protocol_version, WITNESS_PROTOCOL_VERSION
                )));
            }
            let now = state.now_secs();
            let mut core = match lock_core(&state) {
                Ok(core) => core,
                Err(err) => return witness_error_response(&err),
            };
            match core.$core_op(&volume_id, &request, now, &auth.0) {
                Ok(response) => Json(response).into_response(),
                Err(err) => witness_error_response(&err),
            }
        }
    };
}

/// Batch handler plumbing (W10): the same discipline as
/// [`witness_handler!`] without the volume path parameter — batch
/// routes are not volume-scoped.
macro_rules! witness_batch_handler {
    ($name:ident, $request:ty, $core_op:ident) => {
        async fn $name(
            State(state): State<Arc<WitnessServerState>>,
            auth: Caller,
            body: Result<Json<$request>, JsonRejection>,
        ) -> Response {
            let request = match body {
                Ok(Json(request)) => request,
                Err(rejection) => {
                    return api_error_response(ApiError::invalid_request(format!(
                        "invalid request body: {}",
                        rejection.body_text()
                    )));
                }
            };
            if request.protocol_version != WITNESS_PROTOCOL_VERSION {
                return api_error_response(ApiError::invalid_request(format!(
                    "unsupported witness protocol version {} (expected {})",
                    request.protocol_version, WITNESS_PROTOCOL_VERSION
                )));
            }
            let now = state.now_secs();
            let mut core = match lock_core(&state) {
                Ok(core) => core,
                Err(err) => return witness_error_response(&err),
            };
            match core.$core_op(&request, now, &auth.0) {
                Ok(response) => Json(response).into_response(),
                Err(err) => witness_error_response(&err),
            }
        }
    };
}

witness_handler!(register_handler, RegisterRequest, register);
witness_handler!(grant_handler, GrantRequest, grant);
witness_handler!(renew_handler, RenewRequest, renew);
witness_handler!(revoke_handler, RevokeRequest, revoke);
witness_handler!(record_barrier_handler, RecordBarrierRequest, record_barrier);
witness_handler!(void_barrier_handler, VoidBarrierRequest, void_barrier);
witness_batch_handler!(revoke_set_handler, RevokeSetRequest, revoke_set);
witness_batch_handler!(grant_set_handler, GrantSetRequest, grant_set);

async fn inspect_handler(
    State(state): State<Arc<WitnessServerState>>,
    Path(volume): Path<String>,
    _auth: Caller,
) -> Response {
    let volume_id = match VolumeId::new(volume) {
        Ok(id) => id,
        Err(err) => return api_error_response(err),
    };
    let now = state.now_secs();
    let core = match lock_core(&state) {
        Ok(core) => core,
        Err(err) => return witness_error_response(&err),
    };
    match core.inspect(&volume_id, now) {
        Ok(view) => Json(view).into_response(),
        Err(err) => witness_error_response(&err),
    }
}

/// Lock the registry, mapping a poisoned mutex to a typed fail-closed
/// internal error (the same discipline as the Volume API daemon's
/// journal lock).
fn lock_core(
    state: &Arc<WitnessServerState>,
) -> Result<std::sync::MutexGuard<'_, WitnessCore>, WitnessError> {
    state.core.lock().map_err(|_| {
        WitnessError::Internal(
            "witness core mutex poisoned; refusing operations (fail closed)".to_owned(),
        )
    })
}

/// Extractor enforcing bearer authentication on every witness route
/// (the authority surface is host-privileged: `GET` included) and
/// carrying the **resolved caller identity** (W8) into the handler.
///
/// Fail-closed: with no configured token, requests are rejected. The
/// `401` reply uses the raw transport-level body shape
/// `{"code":"UNAUTHORIZED",...}` — deliberately outside the witness
/// error vocabulary, exactly like the Volume API daemon. An
/// authenticated-but-unauthorized *mutation* is not this extractor's
/// business: the core refuses it with the typed `FORBIDDEN` refusal so
/// the rule is unit-testable without HTTP.
struct Caller(CallerIdentity);

impl axum::extract::FromRequestParts<Arc<WitnessServerState>> for Caller {
    type Rejection = Response;

    // The trait mandates an async signature; the check itself is
    // synchronous (header comparison only).
    #[allow(clippy::unused_async_trait_impl)]
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<WitnessServerState>,
    ) -> Result<Self, Self::Rejection> {
        let presented = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        let identity = presented.and_then(|token| state.resolve_identity(token));
        match identity {
            Some(identity) => Ok(Self(identity)),
            // Fail closed: no configured token rejects everything.
            None => Err(unauthorized_response(
                "missing or invalid witness bearer token",
            )),
        }
    }
}

/// Constant-time byte comparison (no early exit on the first differing
/// byte), mirroring the Volume API daemon's token check.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (presented, expected) in a.iter().zip(b.iter()) {
        difference |= presented ^ expected;
    }
    difference == 0
}

/// The `401 UNAUTHORIZED` transport reply.
fn unauthorized_response(message: &str) -> Response {
    let body = serde_json::json!({
        "code": "UNAUTHORIZED",
        "message": message,
    });
    (
        StatusCode::UNAUTHORIZED,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap_or_else(|_| "{\"code\":\"UNAUTHORIZED\"}".to_owned()),
    )
        .into_response()
}

/// Contract-shaped error response for typed refusals.
fn witness_error_response(err: &WitnessError) -> Response {
    api_error_response(err.to_api_error())
}

/// Contract-shaped error response.
fn api_error_response(err: ApiError) -> Response {
    let status =
        StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = ApiErrorBody::from(err);
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap_or_else(|_| {
            "{\"code\":\"INTERNAL\",\"message\":\"serialization failure\"}".to_owned()
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::WitnessCoreConfig;

    #[test]
    fn constant_time_eq_behaves() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn system_now_is_sane() {
        // A 2026-era timestamp; 0 is only permitted for a broken clock.
        let now = system_now_unix();
        assert!(now == 0 || now > 1_700_000_000);
    }

    #[test]
    fn identity_resolution_prefers_host_tokens_and_fails_closed() {
        let mut host_tokens = BTreeMap::new();
        host_tokens.insert("node-a".to_owned(), "host-a-token".to_owned());
        let state = WitnessServerState {
            core: std::sync::Mutex::new(witness_core_for_tests()),
            admin_token: Some("admin-token".to_owned()),
            host_tokens,
            now: Arc::new(|| 0),
        };
        assert_eq!(
            state.resolve_identity("host-a-token"),
            Some(CallerIdentity::Host(
                HostId::new("node-a").expect("valid host id")
            ))
        );
        assert_eq!(
            state.resolve_identity("admin-token"),
            Some(CallerIdentity::Legacy)
        );
        assert_eq!(state.resolve_identity("wrong"), None);
        // A host key that is not a valid HostId never resolves (config
        // validation refuses it; the server never guesses).
        let mut bad = BTreeMap::new();
        bad.insert("not/a/host".to_owned(), "tok".to_owned());
        let state = WitnessServerState {
            core: std::sync::Mutex::new(witness_core_for_tests()),
            admin_token: None,
            host_tokens: bad,
            now: Arc::new(|| 0),
        };
        assert_eq!(state.resolve_identity("tok"), None);
    }

    /// A minimal core for state-level tests (never mutated here). The
    /// tempdir is deliberately forgotten: the core holds the journal's
    /// directory flock, and the directory only needs to outlive the
    /// state under test.
    fn witness_core_for_tests() -> WitnessCore {
        let dir = tempfile::tempdir().expect("tempdir");
        let core = WitnessCore::open(
            &dir,
            WitnessCoreConfig {
                lease_ttl_secs: 60,
                lease_grace_secs: 5,
                suspend_budget_secs: 5,
            },
        )
        .expect("witness core opens");
        std::mem::forget(dir);
        core
    }
}
