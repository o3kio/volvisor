//! Coordinated handoff surface (P4b plan §6, the `AdoptionSurface`
//! pattern): the engine-neutral vocabulary a provider exposes to the
//! migration coordinator for the **source side** of a coordinated
//! VMM/storage handoff.
//!
//! Like [`crate::admin::AdoptionSurface`], this is an optional trait:
//! providers that cannot support coordinated handoff (native-local,
//! unmanaged Ceph) simply do not implement it, and the API layer
//! refuses migrations for their volumes typed instead of guessing
//! (plan §9 row 20). The daemon wires the trait object behind admin
//! authentication and the journal pipeline like every other
//! privileged mutation; the implementations here must honor the same
//! fail-closed rules as the base surface.
//!
//! The semantic contract (plan §2 D2/D4/D6a):
//!
//! - [`HandoffSurface::quiesce_for_barrier`] fixes the source boundary
//!   at the kernel enforcement point and stamps the **durable
//!   migration-cut marker** first (write-ahead: the marker is
//!   persisted before the suspension command, so a crash in between
//!   leaves a volume the provider's own reconcile refuses to resume);
//! - [`HandoffSurface::track_sync`] proves replication catch-up by
//!   observation, **only after** the suspension fixed the boundary (a
//!   catch-up observation taken before the freeze proves nothing about
//!   the boundary — ADR-0004 Decision 2's exact-prefix rule);
//! - [`HandoffSurface::release_source`] is the demote-verify-clear
//!   path: the demotion observes the device closed and refuses typed
//!   otherwise, never forcing (AGENTS rule 17), and performs **no
//!   witness call** — the caller batches the set-wide revocation
//!   (W10 `RevokeSet`) so the participant set is released
//!   all-or-nothing;
//! - [`HandoffSurface::abort_prepare`] is the pre-cut rollback tail:
//!   unsuspend and clear the marker, gated on every recorded barrier
//!   of the epoch being confirmed voided (G5 — an unvoided barrier is
//!   a hard gate on any source resume);
//! - [`HandoffSurface::clear_cut_marker`] is the operator's
//!   fencing-gated resolution for a cut marker its migration no
//!   longer owns (D6a): it requires the volume to be provably
//!   not-writer first — Secondary role, or a fencing proof the
//!   witness corroborates.
//! - [`HandoffSurface::promote_target`] is the destination-side
//!   half: **promote-under-granted-lease** (plan §6). It is a sibling
//!   of the P4a adoption path, not a reuse — it shares the adoption
//!   verification core (lineage, Secondary role, the definition
//!   naming this host, ownership tag) and the entry-creation tail,
//!   with three named deviations: the authority gate is *inverted*
//!   (a live lease held by **this host at the epoch `GrantSet`
//!   minted** is required, never refused), the classification admits
//!   the protocol-independent migration-barrier evidence class
//!   (§7), and the tracked entry is created with migration
//!   provenance plus the attachment record the restore's disk-path
//!   verification needs — not adoption's `"adopted"`-project/`Ready`
//!   stamp.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use volvisor_types::request::{AttachVolumeRequest, AttachVolumeResponse};
use volvisor_types::{ApiError, FencingProof, InspectVolumeResponse, MigrationId, VolumeId};

/// One participant of a VM's handoff eligibility (plan §2, rule 6:
/// eligibility is VM-wide, across all attached volumes — one
/// unprepared participant refuses the whole migration, never a
/// subset).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EligibilityParticipant {
    /// The attached volume this participant describes.
    pub volume_id: VolumeId,
    /// Whether this participant alone would admit a handoff.
    pub eligible: bool,
    /// Stable, human-readable reasons for ineligibility (empty when
    /// eligible) — typed diagnostics, never a silent skip.
    pub reasons: Vec<String>,
}

/// The VM-wide handoff eligibility answer for one consuming VM.
///
/// Honest by construction: `eligible` is `true` only when every
/// participant of the VM this provider holds is individually
/// eligible; anything less carries the per-participant reasons.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EligibilityReport {
    /// The consuming VM the report was computed for.
    pub vm_id: String,
    /// Whether the whole participant set admits a handoff.
    pub eligible: bool,
    /// Every attached volume of this VM the provider holds, each with
    /// its own eligibility and reasons.
    pub participants: Vec<EligibilityParticipant>,
}

/// Proof that one volume's source data path is quiesced for a
/// migration barrier (plan §2 D2/D6a): the suspension was **observed**
/// (never inferred from a command's exit status) and the durable cut
/// marker is in place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuiesceProof {
    /// The quiesced volume.
    pub volume_id: VolumeId,
    /// The migration the quiesce belongs to.
    pub migration_id: MigrationId,
    /// The suspension was observed on the resource (a first-class
    /// status fact, not the suspend command's exit status).
    pub observed_suspended: bool,
    /// The durable migration-cut marker is recorded (write-ahead:
    /// persisted before the suspension command ran).
    pub cut_marker_durable: bool,
    /// Local unix time the marker stamped (the recorded cut start).
    pub suspended_at: u64,
}

/// Proof of replication catch-up through the barrier (plan §2 D2):
/// peer disk `UpToDate`, no resync in progress, connection
/// established — each an observed fact, taken **after** the
/// suspension fixed the boundary.
///
/// The Protocol A residual is recorded in the plan (§8 item 3): these
/// facts state what DRBD's own state machine reports, never a
/// zero-RPO claim (AGENTS rule 16).
// The bools are the recorded evidence, not a state machine: each
// names one independently re-checkable observed fact (the same shape
// as `BarrierAttestation`'s three).
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncProof {
    /// The volume whose peer convergence was observed.
    pub volume_id: VolumeId,
    /// The peer's disk state was observed `UpToDate`.
    pub peer_up_to_date: bool,
    /// No resync was in progress at the observation.
    pub resync_active: bool,
    /// The replication connection was established at the observation.
    pub connection_established: bool,
    /// The observation was taken after the suspension was observed
    /// (the ordering that makes the boundary exact; always `true` on
    /// a successful return — a pre-freeze observation is refused, not
    /// reported).
    pub observed_after_suspension: bool,
    /// Local unix time of the observation.
    pub observed_at: u64,
}

/// The source-side coordinated-handoff surface (P4b plan §6).
///
/// Implementations are privileged mutations from the API's
/// perspective (admin token, journaled intent); they must be
/// idempotent under the coordinator's retry discipline and fail
/// closed on every unprovable step.
#[async_trait]
pub trait HandoffSurface: Send + Sync {
    /// Compute the VM-wide handoff eligibility for `vm_id` across
    /// every volume this provider holds attached to that VM (rule 6).
    /// Read-only: no suspension, no witness mutation.
    ///
    /// # Errors
    /// Returns [`ApiError`] only for real observation failures (an
    /// unreadable state); an ineligible participant is a **result**
    /// carried in the report with typed reasons, never an error.
    async fn handoff_eligibility(&self, vm_id: &str) -> Result<EligibilityReport, ApiError>;

    /// Suspend one participant's source I/O at the kernel enforcement
    /// point and stamp the durable migration-cut marker (D6a),
    /// write-ahead: the marker is persisted **before** the suspension
    /// command, so a crash in between leaves a volume the provider's
    /// own reconcile refuses to resume. The suspension must then be
    /// observed before the proof is returned.
    ///
    /// Idempotent per `(volume, migration)`: a replay for the same
    /// migration re-observes and re-proves; a different migration id
    /// over a marked volume is a typed refusal.
    ///
    /// # Errors
    /// Returns [`ApiError`] typed: unknown volume, a non-writer
    /// (unattached or not-Primary) source, a conflicting cut marker,
    /// or a suspension that could not be executed and observed.
    async fn quiesce_for_barrier(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<QuiesceProof, ApiError>;

    /// Prove replication catch-up by observation, taken **after** the
    /// suspension fixed the boundary (D2): peer disk `UpToDate`, no
    /// resync, connection established. A single observation — the
    /// caller owns the bounded wait and retries this typed, retryable
    /// refusal while the peer lags (the fake models asynchronous peer
    /// apply so tests prove the coordinator *waits* rather than
    /// assumes).
    ///
    /// # Errors
    /// [`ApiError`] with code
    /// `volvisor_types::ApiErrorCode::ReplicaNotDurable` while the peer has not
    /// converged (retryable); typed refusals for an unquiesced source
    /// (an observation before the freeze proves nothing) or an
    /// unknown volume.
    async fn track_sync(&self, volume_id: &VolumeId) -> Result<SyncProof, ApiError>;

    /// Release one participant's source role (D4): demote, re-verify
    /// Secondary from observed status, lift this host's suspension,
    /// clear the cut marker and the attachment record. The demotion
    /// observes the device closed and refuses typed otherwise — never
    /// forced (rule 17). Performs **no witness call**: the caller
    /// batches the set-wide revocation (W10 `RevokeSet`) so one
    /// participant's refusal holds the whole set.
    ///
    /// # Errors
    /// Returns [`ApiError`] typed: unknown volume, marker mismatch,
    /// a still-open device (`INVALID_STATE`, retry after the VM
    /// destroy), or a demotion that could not be verified.
    async fn release_source(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<(), ApiError>;

    /// The pre-cut rollback tail for one participant: unsuspend the
    /// source I/O and clear the cut marker, keeping the attachment
    /// intact. Gated on every recorded barrier of the epoch being
    /// confirmed voided first (G5: an unvoided barrier is a hard gate
    /// on any source resume) — a refusal leaves the source suspended
    /// and the marker in place, never a silent resume.
    ///
    /// # Errors
    /// Returns [`ApiError`] typed: unknown volume, marker mismatch,
    /// an unvoided recorded barrier or an unreachable witness
    /// (fail-closed — the source stays suspended), or a resume that
    /// could not be executed.
    async fn abort_prepare(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<(), ApiError>;

    /// The operator's fencing-gated resolution for a cut marker its
    /// migration no longer owns (D6a): refuses while the volume is
    /// Primary/writer; clears the marker and reconciles the volume
    /// normally when the role is Secondary, or when the caller
    /// supplies a [`FencingProof`] the witness corroborates (the
    /// retirement record must match the volume's retired epoch).
    ///
    /// # Errors
    /// Returns [`ApiError`] typed: unknown volume, no marker present,
    /// a Primary/writer without a corroborated proof
    /// (`INVALID_STATE`/`UNSAFE_DATA_LOSS`), or an unreachable
    /// witness when a proof must be verified (fail-closed).
    async fn clear_cut_marker(
        &self,
        volume_id: &VolumeId,
        proof: Option<&FencingProof>,
    ) -> Result<InspectVolumeResponse, ApiError>;

    /// The destination-side half of the coordinated handoff (plan §6):
    /// **promote-under-granted-lease**. The caller (the coordinator's
    /// `DESTINATION_AUTHORIZED` step) has already run the W10
    /// `GrantSet` at the witness — this method verifies the granted
    /// lease is live and held by **this host** at the minted epoch,
    /// that the migration's source epoch is durably retired, and the
    /// adoption verification core (lineage, Secondary role, the
    /// definition naming this host, ownership tag) — then classifies
    /// (the migration-barrier evidence class of §7 applies,
    /// protocol-independent) and promotes **only** on
    /// `SAFE_CURRENT`: there is no `allow_loss` parameter on this
    /// path, a coordinated cut that lost its evidence is a typed
    /// refusal, never an authorized-loss promotion.
    ///
    /// The tracked entry is created with migration provenance
    /// (migration id, granted epoch) and the attachment record
    /// (vm id, host, device) the restore's disk-path verification
    /// needs. Idempotent per migration: a re-drive of a tracked
    /// entry whose creation payload names this migration re-verifies
    /// everything and completes the tail (the crash window between
    /// the durable record and the attachment save); a different
    /// migration over a tracked volume is a typed refusal.
    ///
    /// # Errors
    /// Returns [`ApiError`] typed: unknown/unregistered volume,
    /// verification failures (`INVALID_STATE`,
    /// `FOREIGN_DEVICE_STATE`), a lease that is not live
    /// (`INVALID_STATE`) or held by another host (`LEASE_HELD`), an
    /// unretired source epoch (`UNSAFE_DATA_LOSS`), a classification
    /// below `SAFE_CURRENT` (`UNSAFE_DATA_LOSS` with the
    /// classification rendered in the detail — the coordinator
    /// surfaces it, never authorizes loss from here), or an
    /// unreachable witness (`UNKNOWN_FENCING_AUTHORITY`).
    async fn promote_target(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
        attach: &AttachVolumeRequest,
    ) -> Result<AttachVolumeResponse, ApiError>;

    /// Whether one volume's **local** role is Secondary, from observed
    /// status (stage B2: the daemon's handoff driver feeds the
    /// coordinator's `source_secondary` observation, and the
    /// destination's peer `prepare` verifies the target replica's
    /// role the same way before the cut).
    ///
    /// Read-only: a status observation, no mutation.
    ///
    /// # Errors
    /// Returns [`ApiError`] typed: `NOT_FOUND` for an unknown volume;
    /// `INTERNAL` for a status observation that cannot be trusted. A
    /// resource that is verifiably down is a **result** (`false`), not
    /// an error: "not Secondary" is the observable fact the caller
    /// reconciles on.
    async fn role_secondary(&self, volume_id: &VolumeId) -> Result<bool, ApiError>;

    /// Fail-closed fencing for one participant (stage B2: the daemon
    /// driver's `fence_source`, the rollback tail whose barrier void
    /// could not be confirmed): durably self-fence the volume —
    /// suspend, mark, demote — so it cannot be written again until an
    /// operator resolves it. Never followed by a resume of anything
    /// (AGENTS rule 4: fencing is never weakened; rule 5: the fenced
    /// source is never blindly restarted).
    ///
    /// # Errors
    /// Returns [`ApiError`] typed: `NOT_FOUND` for an unknown volume;
    /// the fencing act's typed error otherwise (the volume stays
    /// suspended — never a silent unfenced writer).
    async fn fail_closed_fence(&self, volume_id: &VolumeId, reason: &str) -> Result<(), ApiError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(raw: &str) -> VolumeId {
        VolumeId::new(raw).expect("valid volume id")
    }

    fn migration(raw: &str) -> MigrationId {
        MigrationId::new(raw).expect("valid migration id")
    }

    #[test]
    fn proofs_round_trip_through_json() {
        let quiesce = QuiesceProof {
            volume_id: volume("vol-1"),
            migration_id: migration("mig-1"),
            observed_suspended: true,
            cut_marker_durable: true,
            suspended_at: 1_700_000_000,
        };
        let json = serde_json::to_string(&quiesce).expect("serialize");
        assert_eq!(
            serde_json::from_str::<QuiesceProof>(&json).expect("deserialize"),
            quiesce
        );
        assert!(json.contains("\"cut_marker_durable\":true"));

        let sync = SyncProof {
            volume_id: volume("vol-1"),
            peer_up_to_date: true,
            resync_active: false,
            connection_established: true,
            observed_after_suspension: true,
            observed_at: 1_700_000_001,
        };
        let json = serde_json::to_string(&sync).expect("serialize");
        assert_eq!(
            serde_json::from_str::<SyncProof>(&json).expect("deserialize"),
            sync
        );
        assert!(json.contains("\"peer_up_to_date\":true"));
    }

    #[test]
    fn eligibility_report_round_trips_and_denies_unknown_fields() {
        let report = EligibilityReport {
            vm_id: "vm-1".to_owned(),
            eligible: false,
            participants: vec![
                EligibilityParticipant {
                    volume_id: volume("vol-1"),
                    eligible: true,
                    reasons: Vec::new(),
                },
                EligibilityParticipant {
                    volume_id: volume("vol-2"),
                    eligible: false,
                    reasons: vec!["a pending self-fence".to_owned()],
                },
            ],
        };
        let json = serde_json::to_string(&report).expect("serialize");
        assert_eq!(
            serde_json::from_str::<EligibilityReport>(&json).expect("deserialize"),
            report
        );
        // Unknown fields are refused, never silently mapped.
        let extended = json.replace("\"vm_id\"", "\"extra\":1,\"vm_id\"");
        assert!(serde_json::from_str::<EligibilityReport>(&extended).is_err());
    }
}
