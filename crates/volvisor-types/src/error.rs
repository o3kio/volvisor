//! Typed error taxonomy (Volume API v2 section 7) with HTTP mapping.
//!
//! Errors are fail-closed and machine-readable: every rejection carries a
//! stable `code` string exactly as spelled in the contract, a human message
//! and an HTTP status. Unknown backend conditions never degrade into a
//! success or a healthy status.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The literal expected by [`crate::validate_api_version`] callers that want
/// to surface the constant in messages.
pub const INVALID_API_VERSION: &str = crate::API_VERSION;

/// Stable machine-readable error codes (Volume API v2 section 7, plus
/// transport-level codes needed by the HTTP surface).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ApiErrorCode {
    /// Requested class/policy/field combination is not supported; fail
    /// closed, never silently ignored.
    UnsupportedClassOrPolicy,
    /// Placement cannot satisfy the requested failure domains.
    InsufficientFailureDomains,
    /// No safe capacity (including metadata and mirror legs).
    NoSafeCapacity,
    /// Thin-pool metadata exhausted.
    ThinMetadataExhausted,
    /// Foreign or mismatched backend device state; quarantined.
    ForeignDeviceState,
    /// Expected generation does not match; typed conflict, not success.
    StaleGeneration,
    /// Another writable attachment is already active.
    WriterAlreadyActive,
    /// The witness holds a live lease for the volume; the competing grant
    /// is refused (witness invariant W1).
    LeaseHeld,
    /// The renewal targets a retired epoch; the writer learns it is fenced
    /// (witness invariant W4).
    StaleEpoch,
    /// A grant is inside the witness's fence-wait window; retry after the
    /// carried duration (witness invariant W7).
    FencePending,
    /// Fencing authority cannot be established.
    UnknownFencingAuthority,
    /// A replica required for the requested guarantee is not durable.
    ReplicaNotDurable,
    /// Cross-host migration unsupported for native-local storage.
    MigrationUnsupportedLocalStorage,
    /// VMM handoff path not implemented/qualified.
    VmmHandoffUnsupported,
    /// Commit outcome unknown; observable retriable state.
    OperationInDoubt,
    /// Request would risk unattended data loss.
    UnsafeDataLoss,
    /// Backing Ceph cluster is unhealthy.
    CephClusterUnhealthy,
    /// Malformed request (validation-level rejection).
    InvalidRequest,
    /// Referenced object does not exist.
    NotFound,
    /// Volume is not in a state that admits this operation.
    InvalidState,
    /// `operation_id` reused with a different request payload.
    IdempotencyConflict,
    /// Internal error; no state claims are made.
    Internal,
}

impl ApiErrorCode {
    /// Contract-spelled wire string for this code.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedClassOrPolicy => "UNSUPPORTED_CLASS_OR_POLICY",
            Self::InsufficientFailureDomains => "INSUFFICIENT_FAILURE_DOMAINS",
            Self::NoSafeCapacity => "NO_SAFE_CAPACITY",
            Self::ThinMetadataExhausted => "THIN_METADATA_EXHAUSTED",
            Self::ForeignDeviceState => "FOREIGN_DEVICE_STATE",
            Self::StaleGeneration => "STALE_GENERATION",
            Self::WriterAlreadyActive => "WRITER_ALREADY_ACTIVE",
            Self::LeaseHeld => "LEASE_HELD",
            Self::StaleEpoch => "STALE_EPOCH",
            Self::FencePending => "FENCE_PENDING",
            Self::UnknownFencingAuthority => "UNKNOWN_FENCING_AUTHORITY",
            Self::ReplicaNotDurable => "REPLICA_NOT_DURABLE",
            Self::MigrationUnsupportedLocalStorage => "MIGRATION_UNSUPPORTED_LOCAL_STORAGE",
            Self::VmmHandoffUnsupported => "VMM_HANDOFF_UNSUPPORTED",
            Self::OperationInDoubt => "OPERATION_IN_DOUBT",
            Self::UnsafeDataLoss => "UNSAFE_DATA_LOSS",
            Self::CephClusterUnhealthy => "CEPH_CLUSTER_UNHEALTHY",
            Self::InvalidRequest => "INVALID_REQUEST",
            Self::NotFound => "NOT_FOUND",
            Self::InvalidState => "INVALID_STATE",
            Self::IdempotencyConflict => "IDEMPOTENCY_CONFLICT",
            Self::Internal => "INTERNAL",
        }
    }

    /// HTTP status used by the API surface.
    #[must_use]
    pub fn http_status(self) -> u16 {
        match self {
            Self::UnsupportedClassOrPolicy
            | Self::MigrationUnsupportedLocalStorage
            | Self::UnsafeDataLoss
            | Self::InvalidRequest => 400,
            Self::NotFound => 404,
            Self::VmmHandoffUnsupported => 501,
            Self::NoSafeCapacity | Self::ThinMetadataExhausted => 507,
            Self::CephClusterUnhealthy => 503,
            Self::Internal => 500,
            Self::InsufficientFailureDomains
            | Self::ForeignDeviceState
            | Self::StaleGeneration
            | Self::WriterAlreadyActive
            | Self::LeaseHeld
            | Self::StaleEpoch
            | Self::FencePending
            | Self::UnknownFencingAuthority
            | Self::ReplicaNotDurable
            | Self::OperationInDoubt
            | Self::InvalidState
            | Self::IdempotencyConflict => 409,
        }
    }
}

impl fmt::Display for ApiErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed API error carrying a stable code and a human-readable detail.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {detail}")]
pub struct ApiError {
    /// Stable machine-readable code.
    pub code: ApiErrorCode,
    /// Human-readable detail; must never contain secret material.
    pub detail: String,
}

impl ApiError {
    /// Build an error with the given code and detail.
    #[must_use]
    pub fn new(code: ApiErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// Convenience constructor for validation rejections.
    #[must_use]
    pub fn invalid_request(detail: impl Into<String>) -> Self {
        Self::new(ApiErrorCode::InvalidRequest, detail)
    }

    /// Convenience constructor for typed not-found rejections.
    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(ApiErrorCode::NotFound, detail)
    }

    /// Convenience constructor for stale-generation conflicts.
    #[must_use]
    pub fn stale_generation(expected: u64, actual: u64) -> Self {
        Self::new(
            ApiErrorCode::StaleGeneration,
            format!("expected generation {expected}, current generation {actual}"),
        )
    }

    /// Convenience constructor for idempotency conflicts.
    #[must_use]
    pub fn idempotency_conflict(operation_id: impl fmt::Display) -> Self {
        Self::new(
            ApiErrorCode::IdempotencyConflict,
            format!("operation_id {operation_id} reused with a different request payload"),
        )
    }

    /// HTTP status for this error.
    #[must_use]
    pub fn http_status(&self) -> u16 {
        self.code.http_status()
    }
}

impl From<ApiError> for ApiErrorBody {
    fn from(value: ApiError) -> Self {
        Self {
            code: value.code.as_str().to_owned(),
            message: value.detail,
        }
    }
}

/// JSON error body returned by the HTTP surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiErrorBody {
    /// Stable machine-readable code string.
    pub code: String,
    /// Human-readable message; never contains secret material.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_contract_code_has_status_and_wire_string() {
        let codes = [
            ApiErrorCode::UnsupportedClassOrPolicy,
            ApiErrorCode::InsufficientFailureDomains,
            ApiErrorCode::NoSafeCapacity,
            ApiErrorCode::ThinMetadataExhausted,
            ApiErrorCode::ForeignDeviceState,
            ApiErrorCode::StaleGeneration,
            ApiErrorCode::WriterAlreadyActive,
            ApiErrorCode::LeaseHeld,
            ApiErrorCode::StaleEpoch,
            ApiErrorCode::FencePending,
            ApiErrorCode::UnknownFencingAuthority,
            ApiErrorCode::ReplicaNotDurable,
            ApiErrorCode::MigrationUnsupportedLocalStorage,
            ApiErrorCode::VmmHandoffUnsupported,
            ApiErrorCode::OperationInDoubt,
            ApiErrorCode::UnsafeDataLoss,
            ApiErrorCode::CephClusterUnhealthy,
        ];
        for code in codes {
            assert_eq!(code.as_str(), code.as_str().to_uppercase());
            assert!((400..=599).contains(&code.http_status()));
            let err = ApiError::new(code, "detail");
            let body = ApiErrorBody::from(err);
            let json = serde_json::to_string(&body).expect("serialize");
            assert!(json.contains(code.as_str()));
        }
    }

    #[test]
    fn stale_generation_detail() {
        let err = ApiError::stale_generation(3, 7);
        assert_eq!(err.code, ApiErrorCode::StaleGeneration);
        assert_eq!(err.http_status(), 409);
        assert!(err.detail.contains("expected generation 3"));
    }
}
