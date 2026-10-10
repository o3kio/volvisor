//! Custom extractors: fail-closed JSON bodies and admin bearer auth.

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use serde::de::DeserializeOwned;
use volvisor_types::ApiError;

use crate::error::{ApiErrorReply, json_response};
use crate::state::SharedState;

/// JSON body extractor that maps every rejection onto the typed
/// `INVALID_REQUEST` error shape.
///
/// axum's default `Json` rejection body (which echoes framework internals)
/// never reaches the client: the rejection's reason is folded into the
/// contract's `{"code","message"}` body instead.
pub(crate) struct ValidJson<T>(pub(crate) T);

impl<T, S> FromRequest<S> for ValidJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiErrorReply;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let json = axum::extract::Json::<T>::from_request(req, state)
            .await
            .map_err(|rejection| {
                ApiErrorReply(ApiError::invalid_request(format!(
                    "malformed request body: {rejection}"
                )))
            })?;
        Ok(ValidJson(json.0))
    }
}

/// Extractor enforcing admin bearer authentication on privileged endpoints.
///
/// Requests must present `Authorization: Bearer <token>` matching the
/// configured [`crate::AppState`] `admin_token`; a missing or mismatching
/// token fails with `401` and body `{"code":"UNAUTHORIZED", ...}`.
/// `UNAUTHORIZED` is a transport-level code: it is deliberately not part of
/// the volume error taxonomy in `volvisor-types` (which models *storage*
/// failures).
///
/// **Fail closed**: when no `admin_token` is configured, privileged requests
/// are *rejected*, never permitted. Mutating endpoints cannot be exposed
/// without authentication; the tokenless mode is a loopback-only dev/test
/// convenience (the daemon refuses non-loopback binds without a token).
///
/// Two route groups use this extractor:
///
/// - every mutating volume endpoint (`POST`/`DELETE` under `/v2/volumes`);
/// - the whole `/v2/admin` surface, `GET` included, since device inventory
///   is host-privileged information.
///
/// The remaining read-only `GET` routes (`/v2/volumes`, `/v2/capabilities`,
/// `/healthz`, `/metrics`) stay open in P0 because the daemon binds
/// host-local and they expose nothing a local process could not already
/// observe. **This changes with multi-tenant exposure** — every route will
/// require authorization then.
///
/// The token comparison walks all bytes without an early exit (length is
/// compared separately), avoiding the obvious timing oracle without pulling
/// in a constant-time crate.
pub(crate) struct RequireAdmin;

impl FromRequestParts<SharedState> for RequireAdmin {
    type Rejection = Response;

    // The trait mandates an async signature; the check itself is synchronous
    // (header comparison only).
    #[allow(clippy::unused_async_trait_impl)]
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let Some(expected) = state.admin_token.as_deref() else {
            // Fail closed: without a configured token there is nothing to
            // authenticate against, so privileged requests are disabled.
            return Err(unauthorized_response(
                "admin_token is not configured; mutating endpoints are disabled (fail closed)",
            ));
        };
        let presented = bearer_token(&parts.headers);
        if presented.is_some_and(|token| fixed_time_eq(token, expected)) {
            Ok(Self)
        } else {
            Err(unauthorized_response(
                "mutating operations require a valid admin bearer token",
            ))
        }
    }
}

/// Extract the bearer token from an `Authorization` header, if present and
/// well-formed.
pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ")
}

/// Length-checked, no-early-exit byte comparison.
pub(crate) fn fixed_time_eq(presented: &str, expected: &str) -> bool {
    if presented.len() != expected.len() {
        return false;
    }
    let mut difference = 0u8;
    for (presented_byte, expected_byte) in presented.bytes().zip(expected.bytes()) {
        difference |= presented_byte ^ expected_byte;
    }
    difference == 0
}

/// The `401 UNAUTHORIZED` reply (transport-level code, contract error shape).
fn unauthorized_response(message: &str) -> Response {
    let body = serde_json::json!({
        "code": "UNAUTHORIZED",
        "message": message,
    });
    json_response(StatusCode::UNAUTHORIZED, &body)
}
