//! # Witness wire protocol
//!
//! Versioned request/response types for the witness HTTP surface (P4a plan
//! §3). Every request body carries `protocol_version`; a mismatch is a
//! typed invalid-request refusal, never a best-effort parse. All types
//! reject unknown fields so a foreign payload fails closed.
//!
//! Volume identity travels in the URL path, not duplicated in the body.
//! Lease deadlines travel as **durations from the response** (W5) — never
//! absolute timestamps.
//!
//! ## Error vocabulary and detail format
//!
//! Refusals use the Volume API error body shape
//! ([`volvisor_types::ApiErrorBody`]) with these codes:
//!
//! - `LEASE_HELD` — detail `witness lease held; current_epoch=N`
//! - `STALE_EPOCH` — detail `witness epoch retired; current_epoch=N`
//! - `FENCE_PENDING` — detail `witness fence window active;
//!   retry_after_secs=N`
//!
//! The `key=value` suffixes are a **stable machine-readable sub-format**
//! inside the human detail: [`WitnessError::from_wire`] parses them back
//! so clients recover the typed values without a parallel error schema.
//! Transport-level `401` replies (`{"code":"UNAUTHORIZED",...}`) never
//! carry witness state.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use volvisor_types::error::{ApiError, ApiErrorBody, ApiErrorCode};
use volvisor_types::{
    EndpointBacking, FencingProof, HostId, LeaseId, OperationId, RecordedBarrier, VolumeId,
    WriterEpoch,
};

/// Current witness protocol version.
pub const WITNESS_PROTOCOL_VERSION: u32 = 1;

/// Client-supplied registration content (the witness stamps identity and
/// time; it never invents content).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationContent {
    /// The DRBD data-generation identifiers of the volume's lineage
    /// (`drbdsetup show-gi`; verified shape: uppercase zero-padded
    /// 16-hex-digit values). Must be non-empty, sorted and deduplicated —
    /// the adopt flow compares them as a set against live facts.
    pub lineage_uuids: Vec<String>,
    /// Both endpoints' backing identities (exactly two, on different
    /// hosts — a witness for a "replica" on the same host as the primary
    /// would not be a third failure domain for the data).
    pub endpoints: Vec<EndpointBacking>,
    /// Optional operator-attested barrier. The attestation must cover the
    /// **last acknowledged boundary** property: source-committed,
    /// connection-established at the boundary, and no writes acknowledged
    /// past it (P4a plan §5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barrier: Option<RecordedBarrier>,
}

/// `register` request body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterRequest {
    /// Caller's protocol version; must equal [`WITNESS_PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Idempotency key for the registration.
    pub operation_id: OperationId,
    /// Registration content.
    pub content: RegistrationContent,
}

/// `grant` request body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantRequest {
    /// Caller's protocol version.
    pub protocol_version: u32,
    /// Idempotency key: the exactly-once grant identity (W3b).
    pub operation_id: OperationId,
    /// Host requesting writer authority.
    pub host_id: HostId,
}

/// `renew` request body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenewRequest {
    /// Caller's protocol version.
    pub protocol_version: u32,
    /// Idempotency key for this renewal.
    pub operation_id: OperationId,
    /// Host holding the lease.
    pub host_id: HostId,
    /// Epoch the writer believes it holds (W4 check).
    pub epoch: WriterEpoch,
    /// Lease being renewed (W4 check).
    pub lease_id: LeaseId,
}

/// Operator authorization for a forced revocation (W6): revoking a *live*
/// lease held by another host. Journaled verbatim — never silent, never
/// inferred.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevocationAuthorization {
    /// Operator identity taking responsibility.
    pub operator: String,
    /// Recorded reason (non-empty).
    pub reason: String,
}

/// Power-off attestation shortening the W7 fence wait (P4a plan §7):
/// requires **positive** confirmation (fence-device/BMC evidence) distinct
/// from the W6 authorization record. A false attestation re-opens the
/// dual-write window in the alive-but-partitioned-source case — the trust
/// consequence is documented, never papered over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerOffAttestation {
    /// Positive power-off evidence (non-empty).
    pub evidence: String,
}

/// `revoke` request body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeRequest {
    /// Caller's protocol version.
    pub protocol_version: u32,
    /// Idempotency key for the revocation.
    pub operation_id: OperationId,
    /// Requesting host (must be the holder for a self-release).
    pub host_id: HostId,
    /// Epoch being released or revoked (W4 check).
    pub epoch: WriterEpoch,
    /// W6 authorization; required for a forced revocation of a live
    /// lease held by another host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<RevocationAuthorization>,
    /// Optional power-off attestation shortening the fence wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_off: Option<PowerOffAttestation>,
}

/// `register` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterResponse {
    /// Registered volume.
    pub volume_id: VolumeId,
    /// Epoch after registration (always pre-authority 0: registration
    /// linearizes only *future* authority).
    pub current_epoch: WriterEpoch,
}

/// `grant` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantResponse {
    /// The granted epoch.
    pub epoch: WriterEpoch,
    /// The granted lease.
    pub lease_id: LeaseId,
    /// The lease TTL in seconds — the deadline **as a duration from this
    /// response** (W5): the writer's local deadline is
    /// `response_received_locally + lease_ttl_secs`.
    pub lease_ttl_secs: u64,
    /// Durable proof that all earlier epochs were retired by this grant
    /// (W2).
    pub fencing_proof: FencingProof,
}

/// `renew` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenewResponse {
    /// The renewed epoch (unchanged).
    pub epoch: WriterEpoch,
    /// Remaining lease seconds **as a duration from this response** (W5).
    pub remaining_secs: u64,
}

/// `revoke` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeResponse {
    /// Durable proof of the retirement.
    pub fencing_proof: FencingProof,
}

/// Typed witness outcome vocabulary: every refusal a caller can receive,
/// in one place, independent of transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WitnessError {
    /// W1: a live lease exists for the volume (detail carries the current
    /// epoch so the caller learns the authority state).
    LeaseHeld {
        /// The volume's current epoch.
        current_epoch: WriterEpoch,
    },
    /// W4: the targeted epoch is retired (or the lease expired/revoked) —
    /// the writer learns it is fenced.
    StaleEpoch {
        /// The volume's current epoch.
        current_epoch: WriterEpoch,
    },
    /// W7: the fence-wait window has not passed; retry after the carried
    /// duration.
    FencePending {
        /// Seconds until a grant may succeed.
        retry_after_secs: u64,
    },
    /// The volume is not registered with this witness.
    UnknownVolume,
    /// A different registration already exists for the volume.
    AlreadyRegistered,
    /// Request validation failure (detail explains).
    InvalidRequest(String),
    /// `operation_id` reused with a different request payload.
    IdempotencyConflict,
    /// Transport-level authentication failure (401).
    Unauthorized,
    /// The witness could not be reached / did not answer in time. Never
    /// produced by the server; produced by the client transport.
    Unreachable(String),
    /// Internal error; no state claims are made.
    Internal(String),
}

impl WitnessError {
    /// The Volume API error code for this refusal.
    #[must_use]
    pub fn api_code(&self) -> ApiErrorCode {
        match self {
            Self::LeaseHeld { .. } => ApiErrorCode::LeaseHeld,
            Self::StaleEpoch { .. } => ApiErrorCode::StaleEpoch,
            Self::FencePending { .. } => ApiErrorCode::FencePending,
            Self::UnknownVolume => ApiErrorCode::NotFound,
            Self::AlreadyRegistered => ApiErrorCode::InvalidState,
            Self::InvalidRequest(_) => ApiErrorCode::InvalidRequest,
            Self::IdempotencyConflict => ApiErrorCode::IdempotencyConflict,
            // The server produces the raw UNAUTHORIZED transport body
            // itself; this mapping exists so a leaked conversion stays
            // honest (internal-class, no state claim).
            Self::Unauthorized | Self::Unreachable(_) | Self::Internal(_) => ApiErrorCode::Internal,
        }
    }

    /// Human-readable detail. For `LEASE_HELD`, `STALE_EPOCH` and
    /// `FENCE_PENDING` the trailing `key=value` suffix is the stable
    /// machine-readable sub-format parsed by [`Self::from_wire`].
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::LeaseHeld { current_epoch } => {
                format!("witness lease held; current_epoch={}", current_epoch.0)
            }
            Self::StaleEpoch { current_epoch } => {
                format!("witness epoch retired; current_epoch={}", current_epoch.0)
            }
            Self::FencePending { retry_after_secs } => {
                format!("witness fence window active; retry_after_secs={retry_after_secs}")
            }
            Self::UnknownVolume => "volume is not registered with this witness".to_owned(),
            Self::AlreadyRegistered => {
                "a different registration already exists for this volume".to_owned()
            }
            Self::InvalidRequest(detail) => format!("invalid witness request: {detail}"),
            Self::IdempotencyConflict => {
                "operation_id reused with a different request payload".to_owned()
            }
            Self::Unauthorized => "witness authentication failed".to_owned(),
            Self::Unreachable(detail) => format!("witness unreachable: {detail}"),
            Self::Internal(detail) => format!("witness internal error: {detail}"),
        }
    }

    /// Convert to the Volume API error shape (used by the server).
    #[must_use]
    pub fn to_api_error(&self) -> ApiError {
        ApiError::new(self.api_code(), self.detail())
    }

    /// Recover a typed error from a wire error body (used by the client).
    ///
    /// `unauthorized` marks a transport-level 401 (whose body uses the
    /// raw `UNAUTHORIZED` shape, not the witness vocabulary).
    #[must_use]
    pub fn from_wire(unauthorized: bool, body: &ApiErrorBody) -> Self {
        if unauthorized {
            return Self::Unauthorized;
        }
        match body.code.as_str() {
            "LEASE_HELD" => Self::LeaseHeld {
                current_epoch: WriterEpoch(parse_u64_suffix(&body.message, "current_epoch")),
            },
            "STALE_EPOCH" => Self::StaleEpoch {
                current_epoch: WriterEpoch(parse_u64_suffix(&body.message, "current_epoch")),
            },
            "FENCE_PENDING" => Self::FencePending {
                retry_after_secs: parse_u64_suffix(&body.message, "retry_after_secs"),
            },
            "NOT_FOUND" => Self::UnknownVolume,
            "INVALID_STATE" => Self::AlreadyRegistered,
            "INVALID_REQUEST" => Self::InvalidRequest(body.message.clone()),
            "IDEMPOTENCY_CONFLICT" => Self::IdempotencyConflict,
            _ => Self::Internal(format!("unexpected witness error body: {}", body.code)),
        }
    }
}

impl From<ApiError> for WitnessError {
    fn from(err: ApiError) -> Self {
        match err.code {
            ApiErrorCode::IdempotencyConflict => Self::IdempotencyConflict,
            _ => Self::Internal(err.detail),
        }
    }
}

/// Parse a `key=`-prefixed decimal suffix out of a detail string
/// (the stable sub-format documented on [`WitnessError::detail`]).
fn parse_u64_suffix(detail: &str, key: &str) -> u64 {
    let marker = format!("{key}=");
    detail
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix(&marker))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Compute the idempotency request hash for a witness operation.
///
/// Domain-separated SHA-256 over the operation kind, the operation id and
/// the canonical JSON of the request body (the same discipline as the
/// Volume API's request hashes). Retries must send byte-identical request
/// bodies; a reused `operation_id` with a different body is a typed
/// conflict.
pub(crate) fn request_hash(
    op: &str,
    operation_id: &OperationId,
    body: &impl Serialize,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"volvisor.witness.v1:");
    hasher.update(op.as_bytes());
    hasher.update(b":");
    hasher.update(operation_id.as_str().as_bytes());
    hasher.update(b":");
    let body = serde_json::to_vec(body).unwrap_or_default();
    hasher.update(&body);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_subformat_round_trips() {
        let err = WitnessError::LeaseHeld {
            current_epoch: WriterEpoch(7),
        };
        let body = ApiErrorBody::from(err.to_api_error());
        assert_eq!(
            WitnessError::from_wire(false, &body),
            WitnessError::LeaseHeld {
                current_epoch: WriterEpoch(7)
            }
        );

        let err = WitnessError::FencePending {
            retry_after_secs: 42,
        };
        let body = ApiErrorBody::from(err.to_api_error());
        assert_eq!(
            WitnessError::from_wire(false, &body),
            WitnessError::FencePending {
                retry_after_secs: 42
            }
        );

        // A detail without the suffix parses as 0 (never panics, never
        // guesses a different variant).
        let body = ApiErrorBody {
            code: "LEASE_HELD".to_owned(),
            message: "witness lease held".to_owned(),
        };
        assert_eq!(
            WitnessError::from_wire(false, &body),
            WitnessError::LeaseHeld {
                current_epoch: WriterEpoch(0)
            }
        );
    }

    #[test]
    fn unknown_wire_code_is_internal_not_silent() {
        let body = ApiErrorBody {
            code: "SOMETHING_ELSE".to_owned(),
            message: "boom".to_owned(),
        };
        assert!(matches!(
            WitnessError::from_wire(false, &body),
            WitnessError::Internal(_)
        ));
    }

    #[test]
    fn request_hash_is_deterministic_and_discriminating() {
        let op = OperationId::new("op-1").expect("valid id");
        let req = GrantRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op.clone(),
            host_id: HostId::new("node-a").expect("valid host"),
        };
        let first = request_hash("grant", &op, &req);
        assert_eq!(first, request_hash("grant", &op, &req));
        let other_host = GrantRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op.clone(),
            host_id: HostId::new("node-b").expect("valid host"),
        };
        assert_ne!(first, request_hash("grant", &op, &other_host));
    }

    #[test]
    fn requests_reject_unknown_fields() {
        let json = serde_json::json!({
            "protocol_version": 1,
            "operation_id": "op-1",
            "host_id": "node-a",
            "mystery_field": true,
        });
        assert!(serde_json::from_value::<GrantRequest>(json).is_err());
    }
}
