//! Request and response shapes of the Volume API v2 surface (P0 scope).
//!
//! Field names mirror the contract verbatim (`volume_id`, `size_bytes`,
//! `replication.mode`, ...). Unknown fields are rejected (`deny_unknown_fields`)
//! instead of silently ignored (fail-closed validation, contract section 1).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::authority::{AuthoritySummary, PromotionClassification, SafeCurrentEvidence};
use crate::domain::{
    EffectiveProtection, EvidenceStatus, FailureDomain, Frontend, Health, Provisioning, VolumeClass,
};
use crate::error::ApiError;
use crate::id::{AttachmentId, HostId, OperationId, ProjectId, VolumeId};
use crate::state::{MoveVolumeBackingState, VolumeLifecycle};

/// Provider-facing API version literal.
pub const PROVIDER_API_VERSION: &str = crate::API_VERSION;

/// Placement preferences for CreateVolume.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    /// Preferred serving host.
    pub preferred_host_id: Option<HostId>,
    /// Required failure-domain granularity.
    pub failure_domain: Option<FailureDomain>,
}

/// Requested local protection (contract section 1). A local mirror is a
/// separate local media-leg demand, never a remote replica.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalProtectionRequest {
    /// Protection mode.
    pub mode: LocalProtectionModeRequest,
    /// Minimum healthy local legs required to keep serving.
    pub min_healthy_legs: Option<u32>,
}

/// Requested local protection mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalProtectionModeRequest {
    /// No local mirror.
    None,
    /// Local mirror legs.
    Mirror,
    /// Backend-specific protection.
    ProviderSpecific,
}

/// Nearline replication policy (contract section 1; ADR-0007).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicationPolicyRequest {
    /// Replication engine (e.g. `drbd9`); selects a qualified backend.
    pub engine: Option<String>,
    /// `async` (DRBD A, default), `semi-sync` (B) or `sync` (C).
    pub mode: ReplicationModeRequest,
    /// Remote data copies (witness nodes do not count).
    pub remote_replicas: u32,
    /// Allow create while a replica is not yet durable.
    #[serde(default)]
    pub allow_degraded_create: bool,
}

/// Nearline replication mode (canonical values; async is not RPO=0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReplicationModeRequest {
    /// DRBD Protocol A; possible-RPO, remains the v2 default.
    #[default]
    Async,
    /// DRBD Protocol B; remote memory arrival.
    SemiSync,
    /// DRBD Protocol C; local and remote disk completion.
    Sync,
}

/// Migration policy preference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationPolicyRequest {
    /// e.g. `require_verified`.
    pub live: String,
}

/// Encryption request (separately versioned capability; not in P0).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncryptionRequest {
    /// e.g. `provider-managed`.
    pub mode: String,
    /// External secret reference; never key material inline.
    pub key_ref: String,
}

/// CreateVolume request (contract section 1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateVolumeRequest {
    /// Must equal `volvisor.volume.v2`.
    pub api_version: String,
    /// Idempotency key; reuse with a different payload is a conflict.
    pub operation_id: OperationId,
    /// Owning tenant project.
    pub project_id: ProjectId,
    /// Requested volume identity (caller-chosen, opaque).
    pub volume_id: VolumeId,
    /// Storage class.
    #[serde(rename = "class")]
    pub volume_class: VolumeClass,
    /// Logical size in bytes.
    pub size_bytes: u64,
    /// Logical block size; defaults to 4096 when omitted.
    #[serde(default)]
    pub logical_block_size: Option<u32>,
    /// Thin or thick; defaults to thick in P0.
    #[serde(default)]
    pub provisioning: Option<Provisioning>,
    /// Placement preferences.
    #[serde(default)]
    pub placement: Option<Placement>,
    /// Requested local protection.
    #[serde(default)]
    pub local_protection: Option<LocalProtectionRequest>,
    /// Requested replication policy (nearline only).
    #[serde(default)]
    pub replication: Option<ReplicationPolicyRequest>,
    /// Migration policy preference.
    #[serde(default)]
    pub migration_policy: Option<MigrationPolicyRequest>,
    /// Encryption request (unsupported in P0 -> typed rejection).
    #[serde(default)]
    pub encryption: Option<EncryptionRequest>,
}

impl CreateVolumeRequest {
    /// Validate the envelope. Unsupported policies fail closed here or at
    /// capability negotiation; nothing is silently ignored.
    pub fn validate(&self) -> Result<(), ApiError> {
        crate::validate_api_version(&self.api_version)?;
        if self.size_bytes == 0 {
            return Err(ApiError::invalid_request("size_bytes must be > 0"));
        }
        if let Some(bs) = self.logical_block_size {
            if !matches!(bs, 512 | 1024 | 2048 | 4096) {
                return Err(ApiError::invalid_request(
                    "logical_block_size must be one of 512, 1024, 2048, 4096",
                ));
            }
            if self.size_bytes % u64::from(bs) != 0 {
                return Err(ApiError::invalid_request(
                    "size_bytes must be a multiple of logical_block_size",
                ));
            }
        }
        if self.size_bytes % 512 != 0 {
            return Err(ApiError::invalid_request(
                "size_bytes must be a multiple of 512",
            ));
        }
        if let Some(repl) = &self.replication {
            if self.volume_class != VolumeClass::NearlineReplicated {
                return Err(ApiError::new(
                    crate::error::ApiErrorCode::UnsupportedClassOrPolicy,
                    "replication policy is only valid for nearline-replicated volumes",
                ));
            }
            if repl.remote_replicas == 0 {
                return Err(ApiError::invalid_request(
                    "replication.remote_replicas must be >= 1 for nearline volumes",
                ));
            }
        }
        if self.replication.is_none() && self.volume_class == VolumeClass::NearlineReplicated {
            return Err(ApiError::new(
                crate::error::ApiErrorCode::UnsupportedClassOrPolicy,
                "nearline-replicated volumes require a replication policy",
            ));
        }
        Ok(())
    }

    /// Canonical request hash for idempotency: serialization of the exact
    /// immutable request payload (deterministic field order), SHA-256.
    #[must_use]
    pub fn request_hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"volvisor.volume.v2:create:");
        hasher.update(self.volume_id.as_str().as_bytes());
        hasher.update(b":");
        let body = serde_json::to_vec(self).unwrap_or_default();
        hasher.update(&body);
        hasher.finalize().into()
    }
}

/// AttachVolume request (contract section 3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachVolumeRequest {
    /// Must equal `volvisor.volume.v2`.
    pub api_version: String,
    /// Idempotency key.
    pub operation_id: OperationId,
    /// Consuming VM identity.
    pub vm_id: String,
    /// Host the attachment is scoped to.
    pub host_id: HostId,
    /// Caller-chosen attachment identity.
    pub attachment_id: AttachmentId,
    /// Expected current volume generation (typed conflict when stale).
    pub expected_volume_generation: u64,
    /// Access mode; `single_writer` by default.
    #[serde(default)]
    pub access_mode: AccessModeRequest,
    /// Requested frontend (e.g. `virtio-blk`).
    #[serde(default)]
    pub requested_frontend: Option<String>,
    /// The VMM-side disk id the consumer configured for this
    /// attachment's frontend (e.g. Cloud Hypervisor's
    /// `--disk path=...,id=...`), when it configured one. Recorded
    /// with the attachment so a later grow of an **attached** volume
    /// can address the VMM's resize-disk API (P6-B, ADR-0006 first
    /// slice part 1). The id is the consumer's own VMM device
    /// identity — volvisor never invents one. Absent on an attached
    /// volume: the grow's notification is refused with a recorded
    /// reason (fail-closed), never a silent un-notified success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vmm_disk_id: Option<String>,
}

/// Access-mode request values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessModeRequest {
    /// Exactly one active writable attachment (default).
    #[default]
    SingleWriter,
    /// Multi-reader; requires an explicit safe multi-reader contract.
    ReadOnly,
}

impl AttachVolumeRequest {
    /// Validate the envelope.
    pub fn validate(&self) -> Result<(), ApiError> {
        crate::validate_api_version(&self.api_version)?;
        if self.vm_id.is_empty() {
            return Err(ApiError::invalid_request("vm_id must not be empty"));
        }
        if self.vmm_disk_id.as_deref() == Some("") {
            return Err(ApiError::invalid_request(
                "vmm_disk_id must not be empty when present",
            ));
        }
        Ok(())
    }

    /// Canonical request hash for idempotency.
    #[must_use]
    pub fn request_hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"volvisor.volume.v2:attach:");
        hasher.update(self.attachment_id.as_str().as_bytes());
        hasher.update(b":");
        let body = serde_json::to_vec(self).unwrap_or_default();
        hasher.update(&body);
        hasher.finalize().into()
    }
}

/// Proof that the VM stopped or its I/O drained before detach (contract
/// section 3: detach cannot release authority with in-flight writes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrainProof {
    /// The consuming VM is stopped.
    VmStopped,
    /// Guest and host frontend I/O drained.
    IoDrained,
}

/// DetachVolume request (contract section 3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetachVolumeRequest {
    /// Must equal `volvisor.volume.v2`.
    pub api_version: String,
    /// Idempotency key.
    pub operation_id: OperationId,
    /// Expected current attachment generation (typed conflict when stale).
    pub expected_attachment_generation: u64,
    /// Proof that writes cannot be in flight.
    pub vm_stopped_or_io_drained_proof: DrainProof,
}

impl DetachVolumeRequest {
    /// Validate the envelope.
    pub fn validate(&self) -> Result<(), ApiError> {
        crate::validate_api_version(&self.api_version)
    }

    /// Canonical request hash for idempotency.
    #[must_use]
    pub fn request_hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"volvisor.volume.v2:detach:");
        let body = serde_json::to_vec(self).unwrap_or_default();
        hasher.update(&body);
        hasher.finalize().into()
    }
}

/// GrowVolume request (contract section 4A: grow-only by default).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrowVolumeRequest {
    /// Must equal `volvisor.volume.v2`.
    pub api_version: String,
    /// Idempotency key.
    pub operation_id: OperationId,
    /// New logical size; must be strictly greater than current.
    pub new_size_bytes: u64,
    /// Expected current volume generation.
    pub expected_generation: u64,
}

impl GrowVolumeRequest {
    /// Validate the state-independent envelope (api version and sector
    /// alignment).
    ///
    /// This is the part of validation that can run BEFORE the journal's
    /// idempotency lookup: rejections leave no journal record and the
    /// `operation_id` stays reusable for a corrected retry. The
    /// state-dependent grow-only check needs the current size and lives in
    /// the provider (under its lock) — see [`Self::validate`].
    ///
    /// # Errors
    /// Returns [`ApiError`] when `api_version` is unsupported or
    /// `new_size_bytes` is not 512-aligned.
    pub fn validate_envelope(&self) -> Result<(), ApiError> {
        crate::validate_api_version(&self.api_version)?;
        if self.new_size_bytes % 512 != 0 {
            return Err(ApiError::invalid_request(
                "new_size_bytes must be a multiple of 512",
            ));
        }
        Ok(())
    }

    /// Validate the envelope against the current size (grow-only check
    /// included; run under the provider's lock on real executions only).
    pub fn validate(&self, current_size_bytes: u64) -> Result<(), ApiError> {
        crate::validate_api_version(&self.api_version)?;
        if self.new_size_bytes <= current_size_bytes {
            return Err(ApiError::new(
                crate::error::ApiErrorCode::UnsupportedClassOrPolicy,
                format!(
                    "grow-only: new_size_bytes {} must exceed current size {}",
                    self.new_size_bytes, current_size_bytes
                ),
            ));
        }
        self.validate_envelope()?;
        Ok(())
    }

    /// Canonical request hash for idempotency.
    #[must_use]
    pub fn request_hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"volvisor.volume.v2:grow:");
        let body = serde_json::to_vec(self).unwrap_or_default();
        hasher.update(&body);
        hasher.finalize().into()
    }
}

/// Guest notification outcome of a grow (retry, never shrink).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrowGuestNotification {
    /// Notification succeeded.
    Notified,
    /// Notification pending or failed; retry required.
    RetryRequired,
    /// No running frontend to notify.
    NotApplicable,
}

/// GrowVolume response (contract section 4A).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrowVolumeResponse {
    /// Whether the backing storage was resized.
    pub backing_resized: bool,
    /// Guest/VMM notification status.
    pub guest_notification_status: GrowGuestNotification,
    /// Actual size after the operation.
    pub effective_size_bytes: u64,
}

/// MoveVolumeBackingOnline request (contract section 4A).
///
/// `target_pool_id` names the move target. In the
/// `same_vg_extent_move` scope it is the **target PV** inside the
/// volume's own volume group — a cross-VG or cross-pool target is a
/// typed `MOVE_UNSUPPORTED_SCOPE` refusal, never a silent
/// degradation.
///
/// The response's [`MoveVolumeBackingState`] carries the contract's
/// full vocabulary; a same-VG extent move
/// passes through the honest subset `PREPARING | COPYING | COMPLETE |
/// IN_DOUBT` — `MIRROR_READY`/`PIVOTED` belong to the mirror-and-pivot
/// path (the LV's dm identity is stable across a `pvmove`, so there
/// is no pivot to observe) and a generic `FAILED` is never reported:
/// an unknown mid-move outcome is `IN_DOUBT` (the source stays intact
/// and serving) and every deterministic rejection is a typed refusal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveVolumeBackingRequest {
    /// Must equal `volvisor.volume.v2`.
    pub api_version: String,
    /// Idempotency key (the volume-op Terminal class).
    pub operation_id: OperationId,
    /// The move target: the target PV's name (e.g. `/dev/sdb`) in
    /// this scope.
    pub target_pool_id: String,
    /// Expected current volume generation.
    pub expected_generation: u64,
    /// Optional copy-rate limit. No same-VG `pvmove` implementation
    /// in this version can honor a rate limit, so a set value is a
    /// typed `UNSUPPORTED_CLASS_OR_POLICY` refusal naming this
    /// parameter — never silently ignored.
    pub max_copy_bytes_per_sec: Option<u64>,
}

impl MoveVolumeBackingRequest {
    /// Validate the state-independent envelope (api version and
    /// target shape) — the part that runs BEFORE the journal's
    /// idempotency lookup, so rejections leave no journal record and
    /// the `operation_id` stays reusable for a corrected retry.
    ///
    /// # Errors
    /// Returns [`ApiError`] when `api_version` is unsupported or
    /// `target_pool_id` is empty.
    pub fn validate_envelope(&self) -> Result<(), ApiError> {
        crate::validate_api_version(&self.api_version)?;
        if self.target_pool_id.trim().is_empty() {
            return Err(ApiError::invalid_request(
                "target_pool_id must name the target PV (non-empty)",
            ));
        }
        Ok(())
    }

    /// Canonical request hash for idempotency.
    #[must_use]
    pub fn request_hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"volvisor.volume.v2:move:");
        let body = serde_json::to_vec(self).unwrap_or_default();
        hasher.update(&body);
        hasher.finalize().into()
    }
}

/// MoveVolumeBackingOnline response (contract section 4A).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveVolumeBackingResponse {
    /// The state the move reached inside this call's supervision
    /// window. A same-VG extent move reports the honest subset
    /// `PREPARING | COPYING | COMPLETE | IN_DOUBT` (see the request
    /// documentation for why `MIRROR_READY`/`PIVOTED`/`FAILED` are
    /// never entered by this scope).
    pub state: MoveVolumeBackingState,
    /// The volume's generation after the operation (`Complete` bumps
    /// it; every other state reports the unchanged current one).
    pub generation: u64,
    /// The PV the extents were moved from (diagnostics).
    pub source_pv: String,
    /// The PV the extents were moved to.
    pub target_pv: String,
    /// Honest detail for non-complete states (the `InDoubt` reason,
    /// or the supervision note for `Copying`).
    pub detail: Option<String>,
}

/// Data-erasure policy for DeleteVolume (contract section 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErasurePolicy {
    /// Zero/discard the backing before release.
    ZeroDiscard,
    /// Cryptographic erasure (separate evidence gate).
    Cryptographic,
    /// Retain data (requires explicit operator policy).
    Retain,
}

/// DeleteVolume request (contract section 4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteVolumeRequest {
    /// Must equal `volvisor.volume.v2`.
    pub api_version: String,
    /// Idempotency key.
    pub operation_id: OperationId,
    /// Expected current volume generation.
    pub expected_generation: u64,
    /// Explicit erasure policy; never implicit.
    pub data_erasure_policy: ErasurePolicy,
}

impl DeleteVolumeRequest {
    /// Validate the envelope.
    pub fn validate(&self) -> Result<(), ApiError> {
        crate::validate_api_version(&self.api_version)
    }

    /// Canonical request hash for idempotency.
    #[must_use]
    pub fn request_hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"volvisor.volume.v2:delete:");
        let body = serde_json::to_vec(self).unwrap_or_default();
        hasher.update(&body);
        hasher.finalize().into()
    }
}

/// InspectVolume response (contract section 2 field list, verbatim).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectVolumeResponse {
    /// Opaque volume identity.
    pub volume_id: VolumeId,
    /// Backend class.
    pub backend_class: VolumeClass,
    /// Owning project.
    pub project_id: ProjectId,
    /// Current generation.
    pub generation: u64,
    /// Lifecycle state.
    pub state: VolumeLifecycle,
    /// Provisioned (logical) bytes.
    pub provisioned_bytes: u64,
    /// Physically allocated bytes.
    pub allocated_bytes: u64,
    /// Effective protection (two independent axes).
    pub effective_protection: EffectiveProtection,
    /// Failure domain.
    pub failure_domain: FailureDomain,
    /// Volume health; `unknown` when unproven.
    pub health: Health,
    /// Attachment identities.
    pub attachment_ids: Vec<AttachmentId>,
    /// Current writer attachment, if any.
    pub current_writer: Option<AttachmentId>,
    /// Backend health; `unknown` when unproven.
    pub backend_health: Health,
    /// Honest evidence status.
    pub evidence_status: EvidenceStatus,
    /// Writer-authority summary (nearline volumes with a witness; `None`
    /// for classes without remote authority — never a fabricated
    /// authority claim).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authority: Option<AuthoritySummary>,
}

/// ListVolumes response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListVolumesResponse {
    /// Volumes visible in the request scope.
    pub volumes: Vec<InspectVolumeResponse>,
}

/// Adopt-and-promote request (P4a plan §5/§6, contract section 8): the
/// operator-driven unplanned failover of a nearline volume to the
/// surviving host. A privileged mutation — journaled like every other
/// one (rule 8), with the volume id and `allow_loss` folded into the
/// request hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptVolumeRequest {
    /// Must equal `volvisor.volume.v2`.
    pub api_version: String,
    /// Idempotency key.
    pub operation_id: OperationId,
    /// The explicit loss authorization for a `possible_loss`
    /// classification (recorded with the exposure evidence). It is
    /// never sufficient for `unsafe` and never required for
    /// `safe_current`.
    pub allow_loss: bool,
}

impl AdoptVolumeRequest {
    /// Validate the envelope.
    ///
    /// # Errors
    /// `INVALID_REQUEST` when `api_version` is not the contract's.
    pub fn validate(&self) -> Result<(), ApiError> {
        crate::validate_api_version(&self.api_version)?;
        Ok(())
    }

    /// Canonical request hash for idempotency: the target volume and the
    /// request body (the operation id is the journal key, never part of
    /// the hash).
    #[must_use]
    pub fn request_hash(&self, volume_id: &VolumeId) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"volvisor.volume.v2:adopt:");
        hasher.update(volume_id.as_str().as_bytes());
        hasher.update(b":");
        let body = serde_json::to_vec(self).unwrap_or_default();
        hasher.update(&body);
        hasher.finalize().into()
    }
}

/// Adopt-and-promote response (P4a plan §5/§6): the honest promotion
/// classification — including a refused one (`unsafe`, or
/// `possible_loss` without the recorded `allow_loss` authorization) —
/// and the resulting volume state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptVolumeResponse {
    /// The classification computed from observed facts only.
    pub classification: PromotionClassification,
    /// Which evidence class justified a `SAFE_CURRENT` classification
    /// (P4b plan §7); `none` for every non-`SAFE_CURRENT` verdict.
    /// Additive: pre-P4b responses decode with `none`.
    #[serde(default)]
    pub evidence: SafeCurrentEvidence,
    /// The recorded volume state after a **successful** adoption;
    /// `None` on refusal — nothing was adopted and this host's state
    /// is unchanged (the resource keeps whatever out-of-band state it
    /// had; nothing is fabricated for a volume this host does not
    /// hold).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<InspectVolumeResponse>,
}

/// AttachVolume response: a host-scoped, ephemeral backend handle, never raw
/// secrets (contract section 3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachVolumeResponse {
    /// Attachment identity.
    pub attachment_id: AttachmentId,
    /// Attachment generation after the operation.
    pub attachment_generation: u64,
    /// Volume generation after the operation.
    pub volume_generation: u64,
    /// Frontend descriptor that the VMM should consume.
    pub frontend: Frontend,
    /// Attach evidence state (`prepared` until VMM integration exists).
    pub state: crate::domain::AttachmentState,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_req() -> CreateVolumeRequest {
        serde_json::from_value(serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-1",
            "project_id": "tenant-a",
            "volume_id": "vol-1",
            "class": "native-local",
            "size_bytes": 1_073_741_824
        }))
        .expect("valid request")
    }

    #[test]
    fn create_valid() {
        assert!(create_req().validate().is_ok());
    }

    #[test]
    fn create_rejects_wrong_api_version() {
        let mut req = create_req();
        req.api_version = "volvisor.volume.v1".to_owned();
        assert!(req.validate().is_err());
    }

    #[test]
    fn create_rejects_bad_sizes() {
        let mut req = create_req();
        req.size_bytes = 0;
        assert!(req.validate().is_err());
        req.size_bytes = 1000; // not multiple of 512
        assert!(req.validate().is_err());
        req.size_bytes = 1024;
        req.logical_block_size = Some(4096);
        assert!(req.validate().is_err()); // 1024 % 4096 != 0
    }

    #[test]
    fn create_rejects_replication_on_native() {
        let mut req = create_req();
        req.replication = Some(ReplicationPolicyRequest {
            engine: Some("drbd9".to_owned()),
            mode: ReplicationModeRequest::Async,
            remote_replicas: 1,
            allow_degraded_create: false,
        });
        assert_eq!(
            req.validate().unwrap_err().code,
            crate::error::ApiErrorCode::UnsupportedClassOrPolicy
        );
    }

    #[test]
    fn create_requires_replication_for_nearline() {
        let mut req = create_req();
        req.volume_class = VolumeClass::NearlineReplicated;
        assert_eq!(
            req.validate().unwrap_err().code,
            crate::error::ApiErrorCode::UnsupportedClassOrPolicy
        );
    }

    #[test]
    fn unknown_fields_rejected() {
        let json = serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-2",
            "project_id": "t",
            "volume_id": "v",
            "class": "native-local",
            "size_bytes": 512,
            "surprise_field": true
        });
        assert!(serde_json::from_value::<CreateVolumeRequest>(json).is_err());
    }

    #[test]
    fn request_hash_stable_and_payload_sensitive() {
        let a = create_req();
        let b = create_req();
        assert_eq!(a.request_hash(), b.request_hash());
        let mut c = create_req();
        c.size_bytes += 512;
        assert_ne!(a.request_hash(), c.request_hash());
    }

    #[test]
    fn grow_only() {
        let mut req = serde_json::from_value::<GrowVolumeRequest>(serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-3",
            "new_size_bytes": 2048,
            "expected_generation": 1
        }))
        .expect("valid");
        assert!(req.validate(1024).is_ok());
        req.new_size_bytes = 1024;
        assert_eq!(
            req.validate(1024).unwrap_err().code,
            crate::error::ApiErrorCode::UnsupportedClassOrPolicy
        );
    }

    fn move_req() -> MoveVolumeBackingRequest {
        serde_json::from_value::<MoveVolumeBackingRequest>(serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-move",
            "target_pool_id": "/dev/disk/by-id/wwn-0x5000c500target01",
            "expected_generation": 2
        }))
        .expect("valid")
    }

    #[test]
    fn move_request_envelope_and_hash() {
        let req = move_req();
        assert!(req.validate_envelope().is_ok());
        assert_eq!(req.request_hash(), move_req().request_hash());

        // Envelope rejections stay typed and pre-journal: a wrong
        // api version and an empty target are the caller's to fix,
        // and the operation_id remains reusable.
        let mut bad = move_req();
        bad.api_version = "volvisor.volume.v1".to_owned();
        assert_eq!(
            bad.validate_envelope().unwrap_err().code,
            crate::error::ApiErrorCode::UnsupportedClassOrPolicy
        );
        let mut empty = move_req();
        empty.target_pool_id = "   ".to_owned();
        assert_eq!(
            empty.validate_envelope().unwrap_err().code,
            crate::error::ApiErrorCode::InvalidRequest
        );

        // The hash is payload-sensitive: a different target or a
        // set rate limit is a different operation under the same
        // operation_id (IDEMPOTENCY_CONFLICT territory).
        let mut other = move_req();
        other.target_pool_id = "/dev/other".to_owned();
        assert_ne!(req.request_hash(), other.request_hash());
        other = move_req();
        other.max_copy_bytes_per_sec = Some(1024);
        assert_ne!(req.request_hash(), other.request_hash());
    }

    #[test]
    fn move_request_rejects_unknown_fields() {
        let json = serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-move",
            "target_pool_id": "/dev/t",
            "expected_generation": 1,
            "surprise_field": true
        });
        assert!(serde_json::from_value::<MoveVolumeBackingRequest>(json).is_err());
    }

    #[test]
    fn move_response_carries_the_contract_state_vocabulary() {
        // The wire states are the contract's SCREAMING_SNAKE spellings
        // (section 4A); the same-VG scope uses the honest subset but
        // the wire shape is the full vocabulary's.
        let response: MoveVolumeBackingResponse = serde_json::from_value(serde_json::json!({
            "state": "COPYING",
            "generation": 4,
            "source_pv": "/dev/a",
            "target_pv": "/dev/b",
            "detail": null
        }))
        .expect("valid");
        assert_eq!(response.state, MoveVolumeBackingState::Copying);
        assert_eq!(
            serde_json::to_value(&response).expect("serializes")["state"],
            "COPYING"
        );
    }
}
