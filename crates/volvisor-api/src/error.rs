//! Mapping typed errors onto axum replies, plus the shared JSON reply
//! helpers.
//!
//! Every rejection the server produces uses the contract's JSON error shape
//! `{"code": ..., "message": ...}` with the HTTP status derived from the
//! typed [`ApiErrorCode`]. axum's own rejection bodies (extractor defaults)
//! never reach the client; they are converted in [`crate::extract`].

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::Value;
use volvisor_types::{ApiError, ApiErrorBody, ApiErrorCode};

/// Body used when serializing a reply fails (defensive; serialization of the
/// types crate's shapes cannot fail in practice).
const SERIALIZATION_FAILURE_BODY: &[u8] =
    b"{\"code\":\"INTERNAL\",\"message\":\"response serialization failed\"}";

/// Prometheus text exposition content type (served by `/metrics`).
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Every wire code of the error taxonomy, in declaration order.
///
/// Used to reconstruct the HTTP status of a *replayed failure outcome*: the
/// journal persists the error body (not the status), and a replay must be
/// status- and byte-compatible with the first caller's response.
const ALL_CODES: [ApiErrorCode; 20] = [
    ApiErrorCode::UnsupportedClassOrPolicy,
    ApiErrorCode::InsufficientFailureDomains,
    ApiErrorCode::NoSafeCapacity,
    ApiErrorCode::ThinMetadataExhausted,
    ApiErrorCode::ForeignDeviceState,
    ApiErrorCode::StaleGeneration,
    ApiErrorCode::WriterAlreadyActive,
    ApiErrorCode::UnknownFencingAuthority,
    ApiErrorCode::ReplicaNotDurable,
    ApiErrorCode::MigrationUnsupportedLocalStorage,
    ApiErrorCode::VmmHandoffUnsupported,
    ApiErrorCode::OperationInDoubt,
    ApiErrorCode::UnsafeDataLoss,
    ApiErrorCode::CephClusterUnhealthy,
    ApiErrorCode::InvalidRequest,
    ApiErrorCode::NotFound,
    ApiErrorCode::InvalidState,
    ApiErrorCode::IdempotencyConflict,
    ApiErrorCode::Forbidden,
    ApiErrorCode::Internal,
];

/// axum reply for a typed [`ApiError`].
///
/// The orphan rule forbids implementing axum's `IntoResponse` for the foreign
/// `volvisor_types::ApiError` directly, so handlers return
/// `Result<Response, ApiErrorReply>` and every `?` on an `ApiError` converts
/// through [`From`]. There remains exactly one `IntoResponse` for API errors.
#[derive(Debug)]
pub(crate) struct ApiErrorReply(pub(crate) ApiError);

impl From<ApiError> for ApiErrorReply {
    fn from(error: ApiError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiErrorReply {
    fn into_response(self) -> Response {
        error_response(&self.0)
    }
}

/// Build the canonical JSON error reply (contract error shape, status from
/// the typed code, `application/json`).
pub(crate) fn error_response(error: &ApiError) -> Response {
    let body = serde_json::to_value(ApiErrorBody::from(error.clone()))
        .unwrap_or_else(|_| fallback_error_value());
    json_response(status_of(error.http_status()), &body)
}

/// Serialize `value` into the JSON [`Value`] used for all reply bodies.
///
/// All HTTP bodies are built through this helper (never `to_vec` on the typed
/// struct directly) so that the bytes served to the first caller of an
/// operation are exactly the bytes stored in the journal and replayed later.
pub(crate) fn to_json_value<T: Serialize + ?Sized>(value: &T) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|err| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("failed to serialize response body: {err}"),
        )
    })
}

/// Build a JSON response from an already-serialized body.
pub(crate) fn json_response(status: StatusCode, body: &Value) -> Response {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| SERIALIZATION_FAILURE_BODY.to_vec());
    build_response(status, "application/json", bytes)
}

/// Build a plain-text response (used by `/metrics`).
pub(crate) fn text_response(status: StatusCode, body: String) -> Response {
    build_response(status, PROMETHEUS_CONTENT_TYPE, body.into_bytes())
}

/// Reconstruct the HTTP status for a recorded wire error code, if the code is
/// part of the taxonomy.
pub(crate) fn status_for_wire_code(wire: &str) -> Option<u16> {
    ALL_CODES
        .iter()
        .find(|code| code.as_str() == wire)
        .map(|code| code.http_status())
}

fn status_of(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn fallback_error_value() -> Value {
    serde_json::json!({
        "code": ApiErrorCode::Internal.as_str(),
        "message": "error serialization failed",
    })
}

fn build_response(status: StatusCode, content_type: &str, bytes: Vec<u8>) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(bytes))
        .unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to construct response",
            )
                .into_response()
        })
}
