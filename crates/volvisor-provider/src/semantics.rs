//! Shared, engine-neutral provider semantics.
//!
//! Backends differ in *how* they realize a volume (LVM extents, RBD images,
//! DRBD resources) and in the shape of their durable records, but the rules
//! governing attachment identity, replay, generation fencing and
//! consumer-reported attachment state are the same. This module is that
//! shared core.
//!
//! It deliberately contains no backend I/O: providers keep their command
//! sequences, ownership proofs and persistence in their own crates, and
//! expose only the facts the shared decisions need through the narrow
//! [`AttachmentRecordView`] and [`AttachmentStateRecord`] traits. The
//! on-disk schemas are untouched; the traits are implemented over each
//! provider's existing records.
//!
//! # Shared vs backend-specific
//!
//! Shared by every provider:
//!
//! - expected-generation fencing, at volume and attachment scope;
//! - idempotent attach replay by attachment identity (the same id and the
//!   same payload replays the recorded instance, the same id with a
//!   different payload is `IDEMPOTENCY_CONFLICT`);
//! - single-writer admission (at most one active attachment);
//! - monotonic consumer attachment-state reports (a report never advances
//!   the attachment generation, never downgrades, and an equal/lower report
//!   is an idempotent no-op);
//! - a strictly increasing per-volume attachment generation as the
//!   detach/report fence, so per-id evidence is never misattributed to a
//!   new instance even when a consumer reuses an attachment id.
//!
//! Backend-specific and deliberately *not* generalized here:
//!
//! - device-handle acquisition and release (`rbd map`/`unmap`, DRBD
//!   promote/demote, LVM has no per-attachment device handle);
//! - ownership proofs and foreign-state classification;
//! - DRBD writer-authority leases, fencing and migration cuts;
//! - Ceph mapping/zombie reconciliation and LVM online-move records;
//! - the optional legacy tombstone layout (a bounded filter remains in the
//!   LVM v3 shape for decode compatibility) and whether a backend advertises
//!   consumer reports at all (Ceph/DRBD
//!   do not).

use volvisor_types::request::{
    AttachVolumeRequest, ReportAttachmentStateRequest, ReportAttachmentStateResponse,
};
use volvisor_types::{
    AccessMode, ApiError, ApiErrorCode, AttachmentEvidence, AttachmentId, AttachmentState, HostId,
    VolumeId,
};

/// The engine-neutral identity of an attachment request.
///
/// Every field participates in replay identity: a recorded attachment with
/// the same id but a different volume, VM, host, access mode or VMM disk id
/// is a conflicting payload, never a replay.
#[derive(Clone, Copy, Debug)]
pub struct AttachmentIdentity<'a> {
    /// Volume the attachment belongs to.
    pub volume_id: &'a VolumeId,
    /// Caller-chosen attachment identity.
    pub attachment_id: &'a AttachmentId,
    /// Consuming VM identity.
    pub vm_id: &'a str,
    /// Host the attachment is scoped to.
    pub host_id: &'a HostId,
    /// Granted access mode.
    pub access_mode: AccessMode,
    /// Consumer VMM disk identity, when one was supplied.
    pub vmm_disk_id: Option<&'a str>,
}

impl<'a> AttachmentIdentity<'a> {
    /// Build the identity of an attach request.
    #[must_use]
    pub fn from_request(
        volume_id: &'a VolumeId,
        req: &'a AttachVolumeRequest,
        access_mode: AccessMode,
    ) -> Self {
        Self {
            volume_id,
            attachment_id: &req.attachment_id,
            vm_id: &req.vm_id,
            host_id: &req.host_id,
            access_mode,
            vmm_disk_id: req.vmm_disk_id.as_deref(),
        }
    }
}

/// The narrow view of a backend attachment record the shared decisions need.
///
/// Implementations are thin accessors over each provider's existing
/// (schema-stable) record type; the shared core never mutates or serializes
/// the record through this trait.
pub trait AttachmentRecordView {
    /// Attachment identity.
    fn attachment_id(&self) -> &AttachmentId;
    /// Consuming VM identity.
    fn vm_id(&self) -> &str;
    /// Host the attachment is scoped to.
    fn host_id(&self) -> &HostId;
    /// Granted access mode.
    fn access_mode(&self) -> AccessMode;
    /// Consumer VMM disk identity, when one was supplied.
    fn vmm_disk_id(&self) -> Option<&str>;
    /// Attachment generation (the detach fence).
    fn attachment_generation(&self) -> u64;
}

/// The additional view a provider that advertises consumer attachment-state
/// reports must expose.
pub trait AttachmentStateRecord: AttachmentRecordView {
    /// Current evidence rank.
    fn attachment_state(&self) -> AttachmentState;
    /// Monotonic evidence revision.
    fn state_revision(&self) -> u64;
    /// Evidence basis of the current state.
    fn evidence(&self) -> Option<AttachmentEvidence>;
    /// Consumer that reported the current state (an untrusted label).
    fn reporter(&self) -> Option<&str>;
    /// Server receipt time of the current state, Unix milliseconds.
    fn recorded_at_ms(&self) -> Option<u64>;
    /// Apply one accepted report: set the evidence rank, advance the
    /// revision exactly once, and record provenance.
    fn record_reported_state(
        &mut self,
        state: AttachmentState,
        evidence: AttachmentEvidence,
        reporter: Option<String>,
        recorded_at_ms: u64,
    );
}

/// Fence a volume-level mutation against the caller's expected generation.
///
/// # Errors
/// `STALE_GENERATION` when `expected` does not equal `current`.
pub fn ensure_expected_generation(expected: u64, current: u64) -> Result<(), ApiError> {
    if expected == current {
        Ok(())
    } else {
        Err(ApiError::stale_generation(expected, current))
    }
}

/// Fence an attachment-level mutation (detach or a state report) against the
/// recorded attachment generation.
///
/// The attachment generation is the detach/report token. It is **strictly
/// increasing per volume**: each new instance records a generation greater
/// than every earlier instance on that volume, so a delayed detach or report
/// from a retired instance is fenced even if a consumer reuses its id. A
/// consumer report validates the token but never advances it.
///
/// # Errors
/// `STALE_GENERATION` when `expected` does not equal `current`.
pub fn ensure_expected_attachment_generation(expected: u64, current: u64) -> Result<(), ApiError> {
    ensure_expected_generation(expected, current)
}

/// Admit an attach only when no active attachment already holds the volume.
///
/// # Errors
/// `WRITER_ALREADY_ACTIVE` when an attachment is already recorded.
pub fn ensure_no_active_attachment(
    has_active_attachment: bool,
    volume_id: &VolumeId,
) -> Result<(), ApiError> {
    if has_active_attachment {
        Err(ApiError::new(
            ApiErrorCode::WriterAlreadyActive,
            format!("volume {volume_id} already has an active attachment"),
        ))
    } else {
        Ok(())
    }
}

/// Whether a recorded attachment is the exact instance an attach request
/// names (a replay), including the volume it belongs to.
#[must_use]
pub fn attachment_payload_matches<R: AttachmentRecordView>(
    recorded_volume_id: &VolumeId,
    record: &R,
    requested: &AttachmentIdentity<'_>,
) -> bool {
    recorded_volume_id == requested.volume_id
        && record.attachment_id() == requested.attachment_id
        && record.vm_id() == requested.vm_id
        && record.host_id() == requested.host_id
        && record.access_mode() == requested.access_mode
        && record.vmm_disk_id() == requested.vmm_disk_id
}

/// Resolve a matching attachment id against its recorded payload.
///
/// Returns `Ok(())` only for the same instance (the caller returns the
/// recorded response); a different payload for the same id is a typed
/// conflict, never a second attachment.
///
/// # Errors
/// `IDEMPOTENCY_CONFLICT` when the payload differs.
pub fn resolve_attachment_replay<R: AttachmentRecordView>(
    recorded_volume_id: &VolumeId,
    record: &R,
    requested: &AttachmentIdentity<'_>,
) -> Result<(), ApiError> {
    if attachment_payload_matches(recorded_volume_id, record, requested) {
        Ok(())
    } else {
        Err(ApiError::attachment_id_conflict(requested.attachment_id))
    }
}

/// Apply a consumer attachment-state report.
///
/// Fences the expected attachment generation, then advances evidence only
/// upward. A repeated or lower report is an idempotent no-op (`applied =
/// false`) carrying the current projection; the attachment generation is
/// never advanced.
///
/// # Errors
/// `STALE_GENERATION` when the report's instance generation does not match
/// the recorded attachment.
pub fn apply_attachment_report<R: AttachmentStateRecord>(
    record: &mut R,
    req: &ReportAttachmentStateRequest,
    recorded_at_ms: u64,
) -> Result<ReportAttachmentStateResponse, ApiError> {
    ensure_expected_attachment_generation(
        req.expected_attachment_generation,
        record.attachment_generation(),
    )?;
    let applied = req.state > record.attachment_state();
    if applied {
        record.record_reported_state(
            req.state,
            req.evidence,
            req.reporter.clone(),
            recorded_at_ms,
        );
    }
    Ok(ReportAttachmentStateResponse {
        attachment_id: record.attachment_id().clone(),
        state: record.attachment_state(),
        state_revision: record.state_revision(),
        evidence: record.evidence(),
        reporter: record.reporter().map(ToOwned::to_owned),
        recorded_at_ms: record.recorded_at_ms(),
        attachment_generation: record.attachment_generation(),
        applied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug)]
    struct Record {
        volume_id: VolumeId,
        id: AttachmentId,
        vm_id: String,
        host_id: HostId,
        access_mode: AccessMode,
        vmm_disk_id: Option<String>,
        generation: u64,
        state: AttachmentState,
        revision: u64,
        evidence: Option<AttachmentEvidence>,
        reporter: Option<String>,
        recorded_at_ms: Option<u64>,
    }

    impl Record {
        fn new() -> Self {
            Self {
                volume_id: VolumeId::new("vol-1").expect("valid volume id"),
                id: AttachmentId::new("att-1").expect("valid attachment id"),
                vm_id: "vm-1".to_owned(),
                host_id: HostId::new("host-1").expect("valid host id"),
                access_mode: AccessMode::SingleWriter,
                vmm_disk_id: None,
                generation: 1,
                state: AttachmentState::Prepared,
                revision: 1,
                evidence: None,
                reporter: None,
                recorded_at_ms: None,
            }
        }
    }

    impl AttachmentRecordView for Record {
        fn attachment_id(&self) -> &AttachmentId {
            &self.id
        }

        fn vm_id(&self) -> &str {
            &self.vm_id
        }

        fn host_id(&self) -> &HostId {
            &self.host_id
        }

        fn access_mode(&self) -> AccessMode {
            self.access_mode
        }

        fn vmm_disk_id(&self) -> Option<&str> {
            self.vmm_disk_id.as_deref()
        }

        fn attachment_generation(&self) -> u64 {
            self.generation
        }
    }

    impl AttachmentStateRecord for Record {
        fn attachment_state(&self) -> AttachmentState {
            self.state
        }

        fn state_revision(&self) -> u64 {
            self.revision
        }

        fn evidence(&self) -> Option<AttachmentEvidence> {
            self.evidence
        }

        fn reporter(&self) -> Option<&str> {
            self.reporter.as_deref()
        }

        fn recorded_at_ms(&self) -> Option<u64> {
            self.recorded_at_ms
        }

        fn record_reported_state(
            &mut self,
            state: AttachmentState,
            evidence: AttachmentEvidence,
            reporter: Option<String>,
            recorded_at_ms: u64,
        ) {
            self.state = state;
            self.revision += 1;
            self.evidence = Some(evidence);
            self.reporter = reporter;
            self.recorded_at_ms = Some(recorded_at_ms);
        }
    }

    fn request(
        record: &Record,
        state: AttachmentState,
        evidence: AttachmentEvidence,
        generation: u64,
    ) -> ReportAttachmentStateRequest {
        ReportAttachmentStateRequest {
            api_version: volvisor_types::API_VERSION.to_owned(),
            operation_id: volvisor_types::OperationId::new("op-report")
                .expect("valid operation id"),
            attachment_id: record.id.clone(),
            expected_attachment_generation: generation,
            state,
            evidence,
            reporter: Some("vmm".to_owned()),
            detail: None,
        }
    }

    fn attach_request(record: &Record) -> AttachVolumeRequest {
        AttachVolumeRequest {
            api_version: volvisor_types::API_VERSION.to_owned(),
            operation_id: volvisor_types::OperationId::new("op-attach")
                .expect("valid operation id"),
            vm_id: record.vm_id.clone(),
            host_id: record.host_id.clone(),
            attachment_id: record.id.clone(),
            expected_volume_generation: 1,
            access_mode: volvisor_types::request::AccessModeRequest::SingleWriter,
            requested_frontend: None,
            vmm_disk_id: record.vmm_disk_id.clone(),
        }
    }

    #[test]
    fn generation_fences_are_typed_and_separate() {
        assert_eq!(ensure_expected_generation(3, 3), Ok(()));
        let err = ensure_expected_generation(2, 3).expect_err("stale volume generation");
        assert_eq!(err.code, ApiErrorCode::StaleGeneration);
        assert_eq!(ensure_expected_attachment_generation(1, 1), Ok(()));
        let err =
            ensure_expected_attachment_generation(2, 1).expect_err("stale attachment generation");
        assert_eq!(err.code, ApiErrorCode::StaleGeneration);
    }

    #[test]
    fn attach_replay_is_identity_exact() {
        let record = Record::new();
        let req = attach_request(&record);
        let identity =
            AttachmentIdentity::from_request(&record.volume_id, &req, AccessMode::SingleWriter);
        assert!(attachment_payload_matches(
            &record.volume_id,
            &record,
            &identity
        ));
        assert_eq!(
            resolve_attachment_replay(&record.volume_id, &record, &identity),
            Ok(())
        );

        let mut other = attach_request(&record);
        other.vm_id = "vm-2".to_owned();
        let identity =
            AttachmentIdentity::from_request(&record.volume_id, &other, AccessMode::SingleWriter);
        let err = resolve_attachment_replay(&record.volume_id, &record, &identity)
            .expect_err("different payload");
        assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);
    }

    #[test]
    fn single_writer_admission_is_typed() {
        let volume_id = VolumeId::new("vol-1").expect("valid volume id");
        assert_eq!(ensure_no_active_attachment(false, &volume_id), Ok(()));
        assert_eq!(
            ensure_no_active_attachment(true, &volume_id)
                .expect_err("writer active")
                .code,
            ApiErrorCode::WriterAlreadyActive
        );
    }

    #[test]
    fn reports_advance_monotonically_without_moving_the_generation() {
        let mut record = Record::new();
        let advertised_req = request(
            &record,
            AttachmentState::Advertised,
            AttachmentEvidence::VmmConfigured,
            1,
        );
        let advertised = apply_attachment_report(&mut record, &advertised_req, 1_700_000_000_000)
            .expect("advertised report");
        assert!(advertised.applied);
        assert_eq!(advertised.state, AttachmentState::Advertised);
        assert_eq!(advertised.state_revision, 2);
        assert_eq!(advertised.attachment_generation, 1);

        let active_req = request(
            &record,
            AttachmentState::Active,
            AttachmentEvidence::GuestConfirmed,
            1,
        );
        let active = apply_attachment_report(&mut record, &active_req, 1_700_000_000_001)
            .expect("active report");
        assert!(active.applied);
        assert_eq!(active.state_revision, 3);

        let lower_req = request(
            &record,
            AttachmentState::Advertised,
            AttachmentEvidence::GuestConfirmed,
            1,
        );
        let lower = apply_attachment_report(&mut record, &lower_req, 1_700_000_000_002)
            .expect("lower report is a no-op");
        assert!(!lower.applied);
        assert_eq!(lower.state, AttachmentState::Active);
        assert_eq!(lower.state_revision, 3);
        assert_eq!(lower.recorded_at_ms, active.recorded_at_ms);

        let stale_req = request(
            &record,
            AttachmentState::Active,
            AttachmentEvidence::GuestConfirmed,
            2,
        );
        let err = apply_attachment_report(&mut record, &stale_req, 1_700_000_000_003)
            .expect_err("stale report generation");
        assert_eq!(err.code, ApiErrorCode::StaleGeneration);
    }

    #[test]
    fn identity_includes_the_volume_and_vmm_disk_id() {
        let record = Record::new();
        let req = attach_request(&record);
        let wrong_volume = VolumeId::new("vol-2").expect("valid volume id");
        let identity =
            AttachmentIdentity::from_request(&wrong_volume, &req, AccessMode::SingleWriter);
        let err = resolve_attachment_replay(&record.volume_id, &record, &identity)
            .expect_err("different volume");
        assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);

        let mut other = attach_request(&record);
        other.vmm_disk_id = Some("disk-9".to_owned());
        let identity =
            AttachmentIdentity::from_request(&record.volume_id, &other, AccessMode::SingleWriter);
        let err = resolve_attachment_replay(&record.volume_id, &record, &identity)
            .expect_err("different vmm disk id");
        assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);
    }
}
