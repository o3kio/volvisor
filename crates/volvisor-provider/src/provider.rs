//! Engine-neutral volume provider trait (Volume API v2, P0 surface).

use async_trait::async_trait;
use volvisor_types::domain::VolumeClass;
use volvisor_types::request::{
    AttachVolumeRequest, AttachVolumeResponse, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, GrowVolumeRequest, GrowVolumeResponse, InspectVolumeResponse,
    MoveVolumeBackingRequest, MoveVolumeBackingResponse,
};
use volvisor_types::{ApiError, ApiErrorCode, AttachmentId, CapabilitySet, ProjectId, VolumeId};

/// An engine-neutral storage backend driving the Volume API v2 surface.
///
/// The API server holds this trait as `Arc<dyn VolumeProvider>`; engines
/// (the in-memory fake, the native-local LVM provider, later Ceph/DRBD
/// adapters) implement it behind capability negotiation.
///
/// # Semantic contract (binding for every implementation)
///
/// 1. **Generation fencing.** Every mutation validates the caller's expected
///    generation against the current one; a mismatch fails with
///    [`ApiError::stale_generation`] (typed conflict, never success).
/// 2. **Single writer.** At most one active writable attachment may exist per
///    volume at any time; a competing writable attach fails with
///    `WRITER_ALREADY_ACTIVE`. Readers are only admitted under an explicit
///    safe multi-reader contract. A crash-replayed provider must never
///    fabricate a second attachment (Volume API v2 section 3).
/// 3. **Fail-closed negotiation.** An unsupported class, policy or field
///    combination is rejected with `UNSUPPORTED_CLASS_OR_POLICY` — it is
///    never silently degraded or ignored.
/// 4. **Host-scoped handles.** Attach returns a host-scoped, ephemeral
///    backend handle (a `Frontend` descriptor), never secrets or raw
///    backend credentials.
/// 5. **Delete preconditions.** Delete requires a fully detached volume, no
///    dependents and the correct expected generation. Foreign or ambiguous
///    backend state quarantines the volume and fails with
///    `FOREIGN_DEVICE_STATE`; it is never auto-adopted or destroyed.
/// 6. **Honest health.** Inspect reports health `Unknown` — never `Healthy` —
///    while a condition is unproven, and reports `evidence_status` honestly
///    (no production-support claims from prototype behavior).
/// 7. **Grow-only resize.** Grow strictly increases the logical size,
///    preserving block alignment; shrink is rejected.
/// 8. **Idempotent mutations.** Replaying a mutation with the same identity
///    and payload either replays the recorded outcome or fails closed with a
///    typed conflict (`IDEMPOTENCY_CONFLICT`); it never applies twice.
#[async_trait]
pub trait VolumeProvider: Send + Sync {
    /// Stable provider name for diagnostics (never a secret).
    fn name(&self) -> &str;

    /// Capabilities advertised by this provider instance, tied to its
    /// implementation version and evidence (Volume API v2 section 8).
    /// Absent capabilities cause fail-closed rejections downstream.
    fn capabilities(&self) -> CapabilitySet;

    /// Volume classes served by this provider instance. A create request for
    /// any other class is rejected with `UNSUPPORTED_CLASS_OR_POLICY`.
    fn supported_classes(&self) -> &[VolumeClass];

    /// Create a volume, or replay an identical prior creation of the same
    /// `volume_id` (same payload) by returning the existing volume.
    ///
    /// The same `volume_id` with a different payload is an
    /// `IDEMPOTENCY_CONFLICT`. A fresh volume starts at generation 1.
    async fn create_volume(
        &self,
        req: &CreateVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError>;

    /// Inspect a volume by identity; `NOT_FOUND` when absent.
    async fn inspect_volume(&self, id: &VolumeId) -> Result<InspectVolumeResponse, ApiError>;

    /// List volumes, optionally filtered to one project.
    async fn list_volumes(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<InspectVolumeResponse>, ApiError>;

    /// Attach a volume to a VM on a host.
    ///
    /// Enforces the caller's `expected_volume_generation`, the volume's
    /// state and single-writer admission. Returns a host-scoped, ephemeral
    /// frontend handle (`Frontend::VirtioBlk` and similar), never secrets.
    /// Attachment evidence starts at `Prepared` and must not be reported as
    /// `Active` without VMM observation.
    async fn attach_volume(
        &self,
        volume_id: &VolumeId,
        req: &AttachVolumeRequest,
    ) -> Result<AttachVolumeResponse, ApiError>;

    /// Detach an attachment.
    ///
    /// Requires the correct `expected_attachment_generation` and a drain
    /// proof (`vm_stopped_or_io_drained_proof`); authority is never released
    /// while writes could still be in flight.
    async fn detach_volume(
        &self,
        volume_id: &VolumeId,
        attachment_id: &AttachmentId,
        req: &DetachVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError>;

    /// Grow a volume (grow-only).
    ///
    /// Requires the `resize` capability and the correct expected generation.
    /// The response reports the effective size and the honest guest
    /// notification status (a failed notification is retried; the backing is
    /// never shrunk back).
    async fn grow_volume(
        &self,
        volume_id: &VolumeId,
        req: &GrowVolumeRequest,
    ) -> Result<GrowVolumeResponse, ApiError>;

    /// Delete a volume.
    ///
    /// Requires a fully detached volume, no dependents, the correct expected
    /// generation and an explicit data-erasure policy. Foreign backend state
    /// quarantines the volume and fails with `FOREIGN_DEVICE_STATE`.
    async fn delete_volume(
        &self,
        volume_id: &VolumeId,
        req: &DeleteVolumeRequest,
    ) -> Result<(), ApiError>;

    /// Move a volume's backing extents online (contract section 4A:
    /// `MoveVolumeBackingOnline`).
    ///
    /// **Binding semantics for every implementation:**
    ///
    /// 1. **Capability-gated scope.** Only a provider advertising
    ///    [`SameVgExtentMove`](volvisor_types::Capability::SameVgExtentMove)
    ///    may serve the move; the
    ///    default implementation refuses every request with
    ///    `MOVE_UNSUPPORTED_SCOPE`. Cross-VG, cross-pool and
    ///    cross-class targets are typed refusals of the same code —
    ///    never silent degradations, and the current extents are
    ///    never touched by a refusal.
    /// 2. **Generation fencing** applies exactly as on every other
    ///    mutation; a completed move bumps the volume's generation
    ///    (the relocation is a fenced mutation of the volume's
    ///    placement, while its dm identity, LV path and data are
    ///    unchanged).
    /// 3. **Source-extent freedom is a post-condition, not an
    ///    assumption.** The extents on the source PV are declared
    ///    freed only after the move's completion is *verified* by
    ///    observation (the LV's device list no longer references the
    ///    source PV). A failed verification never frees and reports
    ///    [`InDoubt`](volvisor_types::MoveVolumeBackingState::InDoubt).
    /// 4. **Never a generic `FAILED`.** An unknown mid-move outcome
    ///    (observation failure, or the move ending without relocating
    ///    the extents) reads `IN_DOUBT` with the source intact and
    ///    serving; deterministic rejections are typed errors, not
    ///    states. `failed_reportable` remains the vocabulary's own
    ///    rule.
    /// 5. **`max_copy_bytes_per_sec` is honored or refused.** A
    ///    provider that cannot rate-limit the copy (no same-VG
    ///    `pvmove` implementation can) refuses a set value with
    ///    `UNSUPPORTED_CLASS_OR_POLICY` naming the parameter — the
    ///    fail-closed field-negotiation rule, never a silent ignore.
    ///
    /// The default implementation is the fail-closed refusal: a
    /// provider that has not qualified the move scope answers every
    /// request with `MOVE_UNSUPPORTED_SCOPE`.
    async fn move_volume_backing(
        &self,
        volume_id: &VolumeId,
        req: &MoveVolumeBackingRequest,
    ) -> Result<MoveVolumeBackingResponse, ApiError> {
        Err(ApiError::new(
            ApiErrorCode::MoveUnsupportedScope,
            format!(
                "provider {} does not advertise same_vg_extent_move; moving {} to \
                 {} is outside every qualified move scope",
                self.name(),
                volume_id.as_str(),
                req.target_pool_id
            ),
        ))
    }
}
