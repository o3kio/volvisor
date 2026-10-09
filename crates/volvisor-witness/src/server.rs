//! # Witness HTTP surface
//!
//! axum router exposing the registry over HTTP/JSON on the same
//! conventions as the Volume API daemon: bearer-token admin
//! authentication (fail-closed when no token is configured — the
//! tokenless mode is a loopback-only dev/test convenience the *binder*
//! enforces), contract-shaped error bodies, and every core operation
//! serialized through one mutex (the journal requires `&mut`).
//!
//! Time is injected: handlers pass `now_secs` from the state's clock
//! closure, so integration tests drive expiry and fence windows
//! deterministically without sleeping.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use volvisor_types::VolumeId;
use volvisor_types::error::{ApiError, ApiErrorBody};

use crate::proto::{
    GrantRequest, RegisterRequest, RenewRequest, RevokeRequest, WITNESS_PROTOCOL_VERSION,
    WitnessError,
};
use crate::registry::WitnessCore;

/// Shared witness server state.
pub struct WitnessServerState {
    core: std::sync::Mutex<WitnessCore>,
    admin_token: Option<String>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl WitnessServerState {
    /// Build server state with the real system clock.
    #[must_use]
    pub fn new(core: WitnessCore, admin_token: Option<String>) -> Self {
        Self::with_clock(core, admin_token, Arc::new(system_now_unix))
    }

    /// Build server state with an injected clock (tests; also usable by
    /// embedders with a disciplined time source).
    #[must_use]
    pub fn with_clock(
        core: WitnessCore,
        admin_token: Option<String>,
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        Self {
            core: std::sync::Mutex::new(core),
            admin_token,
            now,
        }
    }

    /// Current witness-clock unix seconds.
    #[must_use]
    pub fn now_secs(&self) -> u64 {
        (self.now)()
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

/// The witness router: five routes under `/v1/volumes/{volume_id}`.
pub fn router(state: Arc<WitnessServerState>) -> Router {
    Router::new()
        .route("/v1/volumes/{volume_id}/register", post(register_handler))
        .route("/v1/volumes/{volume_id}/grant", post(grant_handler))
        .route("/v1/volumes/{volume_id}/renew", post(renew_handler))
        .route("/v1/volumes/{volume_id}/revoke", post(revoke_handler))
        .route("/v1/volumes/{volume_id}", get(inspect_handler))
        .route("/healthz", get(healthz_handler))
        .with_state(state)
}

async fn healthz_handler() -> &'static str {
    "ok"
}

/// Path-volume handler plumbing shared by the mutating routes: validate
/// the id, check the protocol version, lock the registry, run the core
/// operation, shape the response.
macro_rules! witness_handler {
    ($name:ident, $request:ty, $core_op:ident) => {
        async fn $name(
            State(state): State<Arc<WitnessServerState>>,
            Path(volume): Path<String>,
            _auth: RequireWitnessAuth,
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
            match core.$core_op(&volume_id, &request, now) {
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

async fn inspect_handler(
    State(state): State<Arc<WitnessServerState>>,
    Path(volume): Path<String>,
    _auth: RequireWitnessAuth,
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

/// Extractor enforcing admin bearer authentication on every witness
/// route (the authority surface is host-privileged: `GET` included).
///
/// Fail-closed: with no `admin_token` configured, requests are rejected.
/// The `401` reply uses the raw transport-level body shape
/// `{"code":"UNAUTHORIZED",...}` — deliberately outside the witness
/// error vocabulary, exactly like the Volume API daemon.
struct RequireWitnessAuth;

impl axum::extract::FromRequestParts<Arc<WitnessServerState>> for RequireWitnessAuth {
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
        let authorized = match (presented, state.admin_token.as_deref()) {
            (Some(presented), Some(expected)) => {
                constant_time_eq(presented.as_bytes(), expected.as_bytes())
            }
            // Fail closed: no configured token rejects everything.
            _ => false,
        };
        if authorized {
            Ok(Self)
        } else {
            Err(unauthorized_response(
                "missing or invalid witness bearer token",
            ))
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
}
