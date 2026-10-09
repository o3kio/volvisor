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
//! - `FORBIDDEN` — W8 identity refusal (403; detail is prose)
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
    BarrierAttestation, EndpointBacking, FencingProof, HostId, LeaseId, MigrationId, OperationId,
    RecordedBarrier, RecordedMigrationBarrier, VolumeId, WriterEpoch,
};

/// Current witness protocol version.
///
/// Version 2 (P4b plan §4) adds the W8 caller-identity binding (host
/// credentials for every mutation; the legacy shared token is
/// read-only), the W9 barrier routes and the W10 batch mutations. A
/// version-1 peer cannot mutate a version-2 witness: the authn change
/// is the point of the bump, deployed with the migration feature that
/// cannot function without it.
pub const WITNESS_PROTOCOL_VERSION: u32 = 2;

/// The identity a witness call is authenticated as (P4b plan §4 W8).
///
/// Resolved **server-side** from the presented bearer token: a match
/// against the configured per-host credential map yields
/// [`CallerIdentity::Host`], the legacy shared token yields
/// [`CallerIdentity::Legacy`], and anything else fails the transport
/// authentication (401) before a core call is made. The identity is
/// what the authority core checks holder assertions against — a
/// mutation is accepted only when a `Host` identity matches the host
/// the request asserts; `Legacy` may only read (inspect/health).
///
/// The witness records the bound identity implicitly: every journaled
/// mutation passed a `require_holder` check against this identity, so
/// the recorder of a barrier or the self-releaser of a lease is
/// accountable to the credential, not to a shared secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallerIdentity {
    /// The legacy shared admin token: read-only on a v2 witness.
    Legacy,
    /// The named host's credential from the witness's host-token map.
    Host(HostId),
}

impl CallerIdentity {
    /// W8 enforcement for every mutating core call: the caller must be
    /// the host the request asserts. A `Legacy` caller (shared token)
    /// and a `Host` caller asserting a different host are both refused
    /// with the typed [`WitnessError::IdentityRequired`] — there is no
    /// shared-token path that could forge a holder assertion.
    ///
    /// # Errors
    /// [`WitnessError::IdentityRequired`] unless this identity is
    /// `Host(asserted)`.
    pub fn require_holder(&self, asserted: &HostId) -> Result<(), WitnessError> {
        match self {
            Self::Host(host) if host == asserted => Ok(()),
            Self::Host(_) | Self::Legacy => Err(WitnessError::IdentityRequired),
        }
    }
}

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

/// `record-barrier` request body (P4b plan §4 W9).
///
/// The witness stamps the boundary commit index and the recording
/// time; it never invents attestation content. The attestation's truth
/// lives on the recording host — the witness records it verbatim under
/// the W8-bound credential of the epoch's holder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordBarrierRequest {
    /// Caller's protocol version; must equal [`WITNESS_PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Idempotency key for the barrier recording.
    pub operation_id: OperationId,
    /// The holder recording the barrier (W8: must match the caller's
    /// credential and the current lease's holder).
    pub host_id: HostId,
    /// The writer epoch whose serving boundary the barrier attests
    /// (must be the volume's current epoch, held live by `host_id`).
    pub epoch: WriterEpoch,
    /// The attested facts, recorded verbatim.
    pub attestation: BarrierAttestation,
    /// The migration transaction this barrier belongs to, when it was
    /// recorded by a coordinated handoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_id: Option<MigrationId>,
}

/// `record-barrier` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordBarrierResponse {
    /// The recorded barrier, with the witness-stamped boundary commit
    /// index (an ordering token in the journal's total order) and
    /// recording time.
    pub barrier: RecordedMigrationBarrier,
}

/// `void-barrier` request body (P4b plan §4 W9): the abort path's
/// evidence-hygiene step. Only the recording holder, only before the
/// epoch retires — a voided barrier can never surface as
/// `SAFE_CURRENT` evidence for a later retirement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoidBarrierRequest {
    /// Caller's protocol version; must equal [`WITNESS_PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Idempotency key for the void.
    pub operation_id: OperationId,
    /// The holder that recorded the barrier (W8: must match the
    /// caller's credential).
    pub host_id: HostId,
    /// The epoch whose barrier is being voided.
    pub epoch: WriterEpoch,
    /// The migration transaction the barrier belongs to; when present,
    /// only that migration's barrier is voided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_id: Option<MigrationId>,
}

/// `void-barrier` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoidBarrierResponse {
    /// The voided barrier entry (with `voided: true`).
    pub barrier: RecordedMigrationBarrier,
}

/// One member of a batch self-release (P4b plan §4 W10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRelease {
    /// The volume being released.
    pub volume_id: VolumeId,
    /// The epoch being released (W4 check against the volume's
    /// current epoch).
    pub epoch: WriterEpoch,
}

/// `revoke-set` request body: a batch of **self-releases** by one host
/// (the migration source), journaled as one mutation so the set is
/// atomic — a single member's refusal rejects the whole batch and
/// nothing is journaled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeSetRequest {
    /// Caller's protocol version; must equal [`WITNESS_PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Idempotency key for the batch.
    pub operation_id: OperationId,
    /// The releasing host (W8: must match the caller's credential and
    /// every member lease's holder).
    pub host_id: HostId,
    /// The migration transaction this batch belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_id: Option<MigrationId>,
    /// The member releases; non-empty, no duplicate volumes.
    pub releases: Vec<BatchRelease>,
}

/// One member outcome of a `revoke-set`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRevokeOutcome {
    /// The released volume.
    pub volume_id: VolumeId,
    /// Durable proof of the member's retirement.
    pub fencing_proof: FencingProof,
}

/// `revoke-set` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeSetResponse {
    /// One outcome per requested release, in request order.
    pub releases: Vec<BatchRevokeOutcome>,
}

/// One member of a batch grant (P4b plan §4 W10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchGrantVolume {
    /// The volume to grant writer authority for.
    pub volume_id: VolumeId,
}

/// `grant-set` request body: a batch of grants by one host (the
/// migration destination), journaled as one mutation — every member
/// passes the single-grant W1/W7 checks or the whole batch is refused
/// and nothing is journaled. Each member mints its own epoch; all
/// proofs share the batch's commit index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantSetRequest {
    /// Caller's protocol version; must equal [`WITNESS_PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Idempotency key for the batch.
    pub operation_id: OperationId,
    /// The acquiring host (W8: must match the caller's credential).
    pub host_id: HostId,
    /// The migration transaction this batch belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_id: Option<MigrationId>,
    /// The member volumes; non-empty, no duplicates.
    pub requests: Vec<BatchGrantVolume>,
}

/// One member outcome of a `grant-set`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchGrantOutcome {
    /// The granted volume.
    pub volume_id: VolumeId,
    /// The granted epoch (the member's current epoch + 1).
    pub epoch: WriterEpoch,
    /// The granted lease.
    pub lease_id: LeaseId,
    /// The lease TTL in seconds — a duration from the response (W5).
    pub lease_ttl_secs: u64,
    /// Durable proof that the member's previous epoch was retired by
    /// this grant (W2), at the batch's shared commit index.
    pub fencing_proof: FencingProof,
}

/// `grant-set` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantSetResponse {
    /// One outcome per requested volume, in request order.
    pub grants: Vec<BatchGrantOutcome>,
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
    /// W8: the caller's identity is not permitted for this mutation —
    /// a legacy (shared-token) caller attempted a mutating call, or a
    /// host credential asserted a holder it is not bound to.
    IdentityRequired,
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
            Self::IdentityRequired => ApiErrorCode::Forbidden,
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
            Self::IdentityRequired => {
                "witness mutation requires the host credential bound to the asserted \
                 holder (the legacy shared token is read-only)"
                    .to_owned()
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
            "FORBIDDEN" => Self::IdentityRequired,
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
/// Domain-separated SHA-256 over the operation kind, the target
/// volume, the operation id and the canonical JSON of the request
/// body (the same discipline as the Volume API's request hashes). The
/// volume is folded in because it travels in the URL path, not the
/// body — without it, the same `operation_id` and body replayed
/// against a different volume would collide. Retries must send
/// byte-identical request bodies (and target the same volume); a
/// reused `operation_id` with a different body is a typed conflict.
pub(crate) fn request_hash(
    op: &str,
    volume_id: &VolumeId,
    operation_id: &OperationId,
    body: &impl Serialize,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"volvisor.witness.v1:");
    hasher.update(op.as_bytes());
    hasher.update(b":");
    hasher.update(volume_id.as_str().as_bytes());
    hasher.update(b":");
    hasher.update(operation_id.as_str().as_bytes());
    hasher.update(b":");
    let body = serde_json::to_vec(body).unwrap_or_default();
    hasher.update(&body);
    hasher.finalize().into()
}

/// Compute the idempotency request hash for a **batch** witness
/// operation (W10: `revoke-set`/`grant-set`).
///
/// The same domain-separated SHA-256 discipline as [`request_hash`],
/// with its own separator and **without the volume component** — batch
/// routes are not volume-scoped (there is no volume in the URL path;
/// the member volumes are part of the hashed body, so a batch replay
/// with a different member set is still a typed conflict). Retries
/// must send byte-identical request bodies; a reused `operation_id`
/// with a different body is a typed conflict.
pub(crate) fn request_hash_batch(
    op: &str,
    operation_id: &OperationId,
    body: &impl Serialize,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"volvisor.witness.v1.batch:");
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
    fn identity_required_round_trips_as_forbidden() {
        let body = ApiErrorBody::from(WitnessError::IdentityRequired.to_api_error());
        assert_eq!(body.code, "FORBIDDEN");
        assert_eq!(
            WitnessError::from_wire(false, &body),
            WitnessError::IdentityRequired
        );
        assert_eq!(
            WitnessError::IdentityRequired.to_api_error().http_status(),
            403
        );
    }

    #[test]
    fn caller_identity_binds_only_the_asserted_host() {
        let host = HostId::new("node-a").expect("valid host");
        let other = HostId::new("node-b").expect("valid host");
        assert!(
            CallerIdentity::Host(host.clone())
                .require_holder(&host)
                .is_ok()
        );
        assert_eq!(
            CallerIdentity::Host(host.clone()).require_holder(&other),
            Err(WitnessError::IdentityRequired)
        );
        assert_eq!(
            CallerIdentity::Legacy.require_holder(&host),
            Err(WitnessError::IdentityRequired)
        );
    }

    #[test]
    fn batch_request_hash_is_deterministic_and_discriminating() {
        let op = OperationId::new("op-1").expect("valid id");
        let host = HostId::new("node-a").expect("valid host");
        let request = RevokeSetRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op.clone(),
            host_id: host,
            migration_id: None,
            releases: vec![BatchRelease {
                volume_id: VolumeId::new("vol-1").expect("valid id"),
                epoch: WriterEpoch(1),
            }],
        };
        let first = request_hash_batch("revoke-set", &op, &request);
        assert_eq!(first, request_hash_batch("revoke-set", &op, &request));
        // A different member set under the same operation id is a
        // conflict, never a replay of the recorded batch outcome.
        let mut diverging = request.clone();
        diverging.releases.push(BatchRelease {
            volume_id: VolumeId::new("vol-2").expect("valid id"),
            epoch: WriterEpoch(1),
        });
        assert_ne!(first, request_hash_batch("revoke-set", &op, &diverging));
        // The batch domain is distinct from the volume-scoped one.
        let volume = VolumeId::new("vol-1").expect("valid id");
        assert_ne!(first, request_hash("revoke-set", &volume, &op, &request));
    }

    #[test]
    fn request_hash_is_deterministic_and_discriminating() {
        let vol = VolumeId::new("vol-1").expect("valid id");
        let op = OperationId::new("op-1").expect("valid id");
        let req = GrantRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op.clone(),
            host_id: HostId::new("node-a").expect("valid host"),
        };
        let first = request_hash("grant", &vol, &op, &req);
        assert_eq!(first, request_hash("grant", &vol, &op, &req));
        let other_host = GrantRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op.clone(),
            host_id: HostId::new("node-b").expect("valid host"),
        };
        assert_ne!(first, request_hash("grant", &vol, &op, &other_host));
        // The volume travels in the URL path, not the body: the same
        // operation id and body against a DIFFERENT volume must not
        // collide (cross-volume replay would return the wrong lease).
        let other_vol = VolumeId::new("vol-2").expect("valid id");
        assert_ne!(first, request_hash("grant", &other_vol, &op, &req));
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
