//! Custom extractors: fail-closed JSON bodies and admin bearer auth.

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use serde::de::DeserializeOwned;
use volvisor_types::ApiError;

use crate::auth::{AuthContext, Role};
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

/// Extractor enforcing admin-role access on privileged endpoints.
///
/// Authentication happens once, in the [`crate::auth::authorize_and_audit`]
/// middleware, which inserts an [`AuthContext`](crate::auth::AuthContext)
/// into the request extensions. This extractor re-checks the resolved role
/// (defense in depth: a route wired without the middleware fails closed
/// instead of silently skipping authorization).
///
/// Requests must present `Authorization: Bearer <token>` resolving to an
/// admin credential; a missing or insufficient credential fails with `401`
/// (unauthenticated) or `403` (authenticated but under-privileged) and the
/// contract's `{"code","message"}` body. `UNAUTHORIZED`/`FORBIDDEN` are
/// transport-level codes: deliberately not part of the volume error taxonomy
/// in `volvisor-types` (which models *storage* failures).
///
/// **Fail closed**: when no credential is configured at all, privileged
/// requests are *rejected*, never permitted. Mutating endpoints cannot be
/// exposed without authentication; the tokenless mode is a loopback-only
/// dev/test convenience (the daemon refuses non-loopback binds without a
/// credential).
///
/// The whole `/v2/admin` surface uses this extractor (`GET` included, since
/// device inventory is host-privileged information); volume and migration
/// mutations use [`RequireOperator`]. Read routes use [`RequireRead`], which
/// is open only in the documented legacy/tokenless posture.
pub(crate) struct RequireAdmin;

impl FromRequestParts<SharedState> for RequireAdmin {
    type Rejection = Response;

    // The trait mandates an async signature; the check itself is synchronous.
    #[allow(clippy::unused_async_trait_impl)]
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        require_role(parts, state, Role::Admin)?;
        Ok(Self)
    }
}

/// Extractor requiring operator-or-admin access on consumer mutations.
pub(crate) struct RequireOperator;

impl FromRequestParts<SharedState> for RequireOperator {
    type Rejection = Response;

    #[allow(clippy::unused_async_trait_impl)]
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        require_role(parts, state, Role::Operator)?;
        Ok(Self)
    }
}

/// Extractor requiring read access on metadata/capability/metrics routes.
///
/// Reads stay open when no scoped `[credentials]` are configured (the
/// documented legacy and loopback-dev posture). Once any scoped credential
/// exists, a read-only, operator or admin principal is required.
pub(crate) struct RequireRead;

impl FromRequestParts<SharedState> for RequireRead {
    type Rejection = Response;

    #[allow(clippy::unused_async_trait_impl)]
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        if !state.auth.protect_reads() {
            return Ok(Self);
        }
        require_role(parts, state, Role::ReadOnly)?;
        Ok(Self)
    }
}

/// Shared role check over the middleware-inserted context.
#[allow(clippy::result_large_err)] // axum's extractor rejection is a Response
fn require_role(
    parts: &axum::http::request::Parts,
    state: &SharedState,
    required: Role,
) -> Result<(), Response> {
    let context = parts.extensions.get::<AuthContext>();
    match context {
        Some(context) if context.role >= required => Ok(()),
        Some(context) => Err(forbidden_response(&format!(
            "the presented credential role {} is less privileged than the required {} role",
            context.role.as_str(),
            required.as_str()
        ))),
        None if !state.auth.is_configured() => Err(unauthorized_response(
            "admin_token is not configured; mutating endpoints are disabled (fail closed)",
        )),
        None => Err(unauthorized_response(&format!(
            "this endpoint requires a valid {} bearer token",
            required.as_str()
        ))),
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

/// The `403 FORBIDDEN` reply (authenticated but insufficient role).
fn forbidden_response(message: &str) -> Response {
    let body = serde_json::json!({
        "code": "FORBIDDEN",
        "message": message,
    });
    json_response(StatusCode::FORBIDDEN, &body)
}
