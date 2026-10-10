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
    /// The caller's identity is not permitted to perform this
    /// operation (witness invariant W8: a mutating witness call requires
    /// the host credential bound to the holder it asserts; the legacy
    /// shared token is read-only).
    Forbidden,
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
            Self::Forbidden => "FORBIDDEN",
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
            Self::Forbidden => 403,
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

/// The single source of the peer-daemon transport class's detail
/// prefix: [`ApiError::peer_unreachable`] builds it and
/// [`ApiError::is_peer_unreachable`] matches it — one definition, so
/// the constructor and the discriminator cannot drift apart (a drift
/// would stop the class riding out a peer outage and surface the
/// error immediately — the fail-safe direction, never a false pass).
const PEER_UNREACHABLE_PREFIX: &str = "peer daemon unreachable";

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

    /// Convenience constructor for the peer-daemon transport class:
    /// the source daemon's handoff driver cannot reach the
    /// destination daemon (connect, timeout, torn body). `INTERNAL`
    /// by design — no state claim is made about the peer — and the
    /// detail carries the `PEER_UNREACHABLE_PREFIX` stamp so
    /// [`ApiError::is_peer_unreachable`] (the same source) can tell
    /// this class apart from every other internal failure: the two
    /// are one definition, never two string copies that can drift.
    #[must_use]
    pub fn peer_unreachable(detail: impl fmt::Display) -> Self {
        Self::new(
            ApiErrorCode::Internal,
            format!("{PEER_UNREACHABLE_PREFIX}: {detail}"),
        )
    }

    /// Whether this error is the peer-daemon transport class built by
    /// [`ApiError::peer_unreachable`] (connect/timeout/torn-body —
    /// the destination was never asked, so nothing it owns was
    /// journaled and the act is freely re-drivable). Every other
    /// `INTERNAL` failure — including the same client's local
    /// serialization failures — is deliberately **not** this class:
    /// callers use the discriminator to retry riding out a peer
    /// outage, and a wrong positive would retry a bug; a wrong
    /// negative (the fail-safe direction) surfaces immediately.
    #[must_use]
    pub fn is_peer_unreachable(&self) -> bool {
        self.code == ApiErrorCode::Internal && self.detail.starts_with(PEER_UNREACHABLE_PREFIX)
    }

    /// HTTP status for this error.
    #[must_use]
    pub fn http_status(&self) -> u16 {
        self.code.http_status()
    }

    /// Decode a wire error body back into a typed error (the client
    /// mirror of [`From<ApiError> for ApiErrorBody`], stage B2: the
    /// source daemon's peer client recovers the destination's typed
    /// refusals instead of flattening them).
    ///
    /// A known code string recovers its variant with the message
    /// verbatim; an unknown code degrades to `INTERNAL` with the
    /// original code preserved in the detail — never a guess, never a
    /// dropped error.
    #[must_use]
    pub fn from_wire(body: &ApiErrorBody) -> Self {
        let code = match body.code.as_str() {
            "UNSUPPORTED_CLASS_OR_POLICY" => ApiErrorCode::UnsupportedClassOrPolicy,
            "INSUFFICIENT_FAILURE_DOMAINS" => ApiErrorCode::InsufficientFailureDomains,
            "NO_SAFE_CAPACITY" => ApiErrorCode::NoSafeCapacity,
            "THIN_METADATA_EXHAUSTED" => ApiErrorCode::ThinMetadataExhausted,
            "FOREIGN_DEVICE_STATE" => ApiErrorCode::ForeignDeviceState,
            "STALE_GENERATION" => ApiErrorCode::StaleGeneration,
            "WRITER_ALREADY_ACTIVE" => ApiErrorCode::WriterAlreadyActive,
            "LEASE_HELD" => ApiErrorCode::LeaseHeld,
            "STALE_EPOCH" => ApiErrorCode::StaleEpoch,
            "FENCE_PENDING" => ApiErrorCode::FencePending,
            "UNKNOWN_FENCING_AUTHORITY" => ApiErrorCode::UnknownFencingAuthority,
            "REPLICA_NOT_DURABLE" => ApiErrorCode::ReplicaNotDurable,
            "MIGRATION_UNSUPPORTED_LOCAL_STORAGE" => ApiErrorCode::MigrationUnsupportedLocalStorage,
            "VMM_HANDOFF_UNSUPPORTED" => ApiErrorCode::VmmHandoffUnsupported,
            "OPERATION_IN_DOUBT" => ApiErrorCode::OperationInDoubt,
            "UNSAFE_DATA_LOSS" => ApiErrorCode::UnsafeDataLoss,
            "CEPH_CLUSTER_UNHEALTHY" => ApiErrorCode::CephClusterUnhealthy,
            "INVALID_REQUEST" => ApiErrorCode::InvalidRequest,
            "NOT_FOUND" => ApiErrorCode::NotFound,
            "INVALID_STATE" => ApiErrorCode::InvalidState,
            "IDEMPOTENCY_CONFLICT" => ApiErrorCode::IdempotencyConflict,
            "FORBIDDEN" => ApiErrorCode::Forbidden,
            "INTERNAL" => ApiErrorCode::Internal,
            other => {
                return Self::new(
                    ApiErrorCode::Internal,
                    format!(
                        "unrecognized error code {other} (message: {})",
                        body.message
                    ),
                );
            }
        };
        Self::new(code, body.message.clone())
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
            ApiErrorCode::Forbidden,
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

    #[test]
    fn the_peer_unreachable_class_round_trips_and_excludes_every_other_internal() {
        // The transport class: built by the constructor, recognized by
        // the discriminator — the same source, so the round trip is
        // total by construction (this pins it).
        let err = ApiError::peer_unreachable("connect refused (test)");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert_eq!(err.http_status(), 500);
        assert!(err.is_peer_unreachable(), "the constructor's own class");

        // The exclusions (the fail-safe direction — each of these
        // must surface immediately, never ride out a retry bound):
        // any other internal detail, and the same words in a
        // different code (a typed refusal is never the transport
        // class, whatever its detail says).
        let local_bug = ApiError::new(ApiErrorCode::Internal, "peer request serialization failure");
        assert!(!local_bug.is_peer_unreachable());
        let elsewhere = ApiError::new(
            ApiErrorCode::Internal,
            "the witness reported: peer daemon unreachable downstream",
        );
        assert!(
            !elsewhere.is_peer_unreachable(),
            "an internal detail merely containing the words is not the class: {elsewhere}"
        );
        let typed = ApiError::new(ApiErrorCode::ForeignDeviceState, "peer daemon unreachable");
        assert!(
            !typed.is_peer_unreachable(),
            "a typed refusal is never the transport class: {typed}"
        );
    }

    #[test]
    fn from_wire_round_trips_every_code_and_refuses_unknown_ones() {
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
            ApiErrorCode::InvalidRequest,
            ApiErrorCode::NotFound,
            ApiErrorCode::InvalidState,
            ApiErrorCode::IdempotencyConflict,
            ApiErrorCode::Forbidden,
            ApiErrorCode::Internal,
        ];
        for code in codes {
            let body = ApiErrorBody::from(ApiError::new(code, "detail"));
            let recovered = ApiError::from_wire(&body);
            assert_eq!(recovered.code, code, "code {code:?} must round-trip");
            assert_eq!(recovered.detail, "detail");
        }
        // An unknown code never maps to a guess: it degrades typed to
        // INTERNAL with the original code preserved in the detail.
        let foreign = ApiErrorBody {
            code: "SOME_FUTURE_CODE".to_owned(),
            message: "from a newer daemon".to_owned(),
        };
        let recovered = ApiError::from_wire(&foreign);
        assert_eq!(recovered.code, ApiErrorCode::Internal);
        assert!(recovered.detail.contains("SOME_FUTURE_CODE"));
        assert!(recovered.detail.contains("from a newer daemon"));
    }
}
