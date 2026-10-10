//! # The handoff vocabulary (P4b plan section 3)
//!
//! The canonical states are exactly the nearline contract's
//! (`PREPARED` … `COMPLETE`, plus the terminal observations `IN_DOUBT`
//! and `ABORTED`). The cut progress ([`CutProgress`]) is an internal
//! sub-field of [`MigrationRecord`], never a canonical state: a record
//! with an active cut is *observed* as `IN_DOUBT` with the step as
//! detail (only before `DESTINATION_AUTHORIZED` — see
//! [`MigrationRecord::observe`]).
//!
//! Wire formats are snake_case with `deny_unknown_fields` on every
//! record shape: a foreign spelling fails to decode rather than
//! silently mapping (the house `volvisor-types` discipline).

use serde::{Deserialize, Serialize};
use volvisor_types::authority::BarrierAttestation;
use volvisor_types::id::{HostId, MigrationId, VolumeId};

/// The canonical handoff states (nearline contract v2 §6; P4b plan
/// §3), plus the terminal observations.
///
/// The forward path is strictly ordered:
/// `Prepared → Precopy → Quiesced → BarrierDurable → SourceRevoked →
/// DestinationAuthorized → VmResumed → Complete`. From the first
/// irreversible act on, progress is additionally tracked by the
/// record's `cut` field (write-ahead, plan D1a) and the record is
/// forward-only.
///
/// `InDoubt` is a **terminal observation** in this vocabulary: it is
/// either the fail-closed endpoint of a pre-cut rollback whose barrier
/// void could not be confirmed (plan G5), or the observation of a
/// record stalled inside the cut window. An in-cut record's transient
/// `IN_DOUBT` is a projection ([`MigrationRecord::observe`]), not a
/// stored state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffState {
    /// Eligibility verified, migration record created, target replica
    /// verified and prepared.
    Prepared,
    /// DRBD replica catch-up observed (`TrackSync`); no memory
    /// pre-copy in v1 (plan §1 out of scope).
    Precopy,
    /// Source VM paused (verified) and every participating volume's
    /// data path suspended with a durable migration-cut marker.
    Quiesced,
    /// The replication boundary fixed by suspension, proven by a
    /// post-suspension `TrackSync`, and recorded with the witness
    /// (`RecordBarrier`, authenticated).
    BarrierDurable,
    /// The source's writer authority over every participant was
    /// released (`RevokeSet`, after every participant was proven
    /// demoted — never a subset).
    SourceRevoked,
    /// The target host was granted fresh writer authority (`GrantSet`,
    /// new epochs, lingering epochs retired).
    DestinationAuthorized,
    /// The destination VM was restored from the snapshot and resumed.
    VmResumed,
    /// Source reconciled Secondary, cut markers cleared, migration
    /// record closed.
    Complete,
    /// Terminal fail-closed observation: the record cannot proceed
    /// safely and requires the reconcile path (or an operator).
    /// `since` is the unix epoch seconds at which the doubt was
    /// recorded; `detail` is the typed reason.
    InDoubt {
        /// Unix epoch seconds at which the doubt was first recorded.
        since: u64,
        /// Stable, human-readable reason (diagnostic; the variant is
        /// the contract).
        detail: String,
    },
    /// Terminal: the migration was rolled back before the cut began.
    /// `at` is the unix epoch seconds of the abort.
    Aborted {
        /// Stable, human-readable reason.
        reason: String,
        /// Unix epoch seconds at which the abort was recorded.
        at: u64,
    },
}

impl HandoffState {
    /// The state's position on the canonical forward path
    /// (`Prepared = 0` … `Complete = 7`).
    ///
    /// `None` for the terminal observations ([`Self::InDoubt`],
    /// [`Self::Aborted`]), which are not on the forward path. The
    /// forward-only boundary of the reconcile is
    /// `rank >= SourceRevoked` (plan §3).
    #[must_use]
    pub const fn forward_rank(&self) -> Option<u8> {
        match self {
            Self::Prepared => Some(0),
            Self::Precopy => Some(1),
            Self::Quiesced => Some(2),
            Self::BarrierDurable => Some(3),
            Self::SourceRevoked => Some(4),
            Self::DestinationAuthorized => Some(5),
            Self::VmResumed => Some(6),
            Self::Complete => Some(7),
            Self::InDoubt { .. } | Self::Aborted { .. } => None,
        }
    }
}

impl std::fmt::Display for HandoffState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Prepared => "PREPARED",
            Self::Precopy => "PRECOPY",
            Self::Quiesced => "QUIESCED",
            Self::BarrierDurable => "BARRIER_DURABLE",
            Self::SourceRevoked => "SOURCE_REVOKED",
            Self::DestinationAuthorized => "DESTINATION_AUTHORIZED",
            Self::VmResumed => "VM_RESUMED",
            Self::Complete => "COMPLETE",
            Self::InDoubt { .. } => "IN_DOUBT",
            Self::Aborted { .. } => "ABORTED",
        })
    }
}

/// The durable cut progress record (plan D1a): written **before** each
/// irreversible external act, and retained through the whole
/// forward-only window — the state lands at
/// [`HandoffState::SourceRevoked`] with the last cut step still
/// recorded (the D3 crash window stays `IN_DOUBT`-observable, G1) and
/// the field is cleared only at [`HandoffState::Complete`].
///
/// A sub-field of [`MigrationRecord`], never a canonical state. The
/// names are the serde snake_case spellings; [`std::fmt::Display`]
/// renders the same words for observation details.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CutProgress {
    /// `ch-remote snapshot` of the paused source VM (the window opens;
    /// the snapshot is re-runnable — the memory is identical until the
    /// VM resumes, which it never will on the source).
    Snapshotting,
    /// `ch-remote delete` of the source VM (the device closes; there
    /// is no rollback past this line).
    DestroyingVm,
    /// `drbdadm secondary` per participant (refuses typed while the
    /// device is open — AGENTS rule 17).
    Demoting,
    /// Witness `RevokeSet` (the set-wide self-release, after every
    /// participant was proven Secondary).
    Revoking,
}

impl std::fmt::Display for CutProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Snapshotting => "snapshotting",
            Self::DestroyingVm => "destroying_vm",
            Self::Demoting => "demoting",
            Self::Revoking => "revoking",
        })
    }
}

/// One participating volume of a migration: the full writable set of
/// the VM (migration eligibility is VM-wide, AGENTS rule 6).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    /// The participating volume.
    pub volume_id: VolumeId,
    /// The volume generation the migration was prepared against
    /// (stale-generation conflicts are typed, never silent).
    pub expected_generation: u64,
    /// The provider resource name on the replication pair (opaque to
    /// the state machine; the driver interprets it).
    pub resource: String,
    /// The DRBD minor of the pair (symmetric across the hosts in the
    /// common case; verified, never assumed, by the B2 restore path).
    pub minor: u32,
}

/// The durable proof that a volume's serving boundary was recorded
/// with the witness at `BARRIER_DURABLE` (plan W9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BarrierProof {
    /// The volume the barrier attests.
    pub volume_id: VolumeId,
    /// The witness commit index of the `RecordBarrier` mutation — the
    /// ordering token placing the barrier in the journal's total
    /// order.
    pub boundary_commit_index: u64,
    /// The attested facts, recorded verbatim under the recorder's
    /// W8-bound credential.
    pub attestation: BarrierAttestation,
    /// Unix epoch seconds at recording.
    pub recorded_at: u64,
}

/// One entry of the append-only state trace `ObserveHandoff` reports.
///
/// The transition helper appends, never edits; a repeated transition
/// with an identical `(state, cut)` pair appends nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateHistoryEntry {
    /// The state after the transition.
    pub state: HandoffState,
    /// The cut progress after the transition, when inside the cut.
    pub cut: Option<CutProgress>,
    /// Unix epoch seconds at which the transition was persisted.
    pub at: u64,
    /// Optional transition detail (fold observations, fail-closed
    /// reasons); diagnostic only.
    pub detail: Option<String>,
}

/// One journaled typed refusal: the marker a parked record carries
/// so the observation, the restart reconcile and the retry pass all
/// see the same typed outcome (never a silently different one).
///
/// The v1 carrier is the barrier-time lineage re-check (P6-A F1,
/// [`MigrationRecord::barrier_lineage_refusal`]): the refusal is
/// durable state, not a log line — the drive that refused may be
/// gone (a crash, a restart), and the marker is what makes the
/// refusal re-observable without re-executing the gate first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypedRefusal {
    /// The contract-spelled error code (e.g. `FOREIGN_DEVICE_STATE`)
    /// — the same wire vocabulary [`volvisor_types::ApiErrorCode`]
    /// spells.
    pub code: String,
    /// The refusal's human-readable detail (diagnostic only).
    pub detail: String,
    /// Unix epoch seconds at which the refusal was journaled.
    pub at: u64,
}

/// The v1 abort policy: a pre-cut migration is automatically rolled
/// back by the reconcile (the source's authority is intact, so
/// rollback is safe and the consumer re-issues). There is no abort
/// after the cut begins — by construction, not by policy check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortPolicy {
    /// Abort allowed (and reconciled) only before the cut.
    #[default]
    AutoBeforeCut,
}

/// The request shape of the coordinator's `prepare` operation (the
/// journaled `PrepareNearlineHandoff` payload in stage B2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareHandoffRequest {
    /// Consumer-supplied, unique per migration (the `OperationId`
    /// validation rules).
    pub migration_id: MigrationId,
    /// The VM whose whole writable set participates.
    pub vm_id: String,
    /// The source host (the coordinator's host; the current writer).
    pub source_host: HostId,
    /// The destination host (the peer that will be granted).
    pub target_host: HostId,
    /// The participating volumes (non-empty, unique volume ids).
    pub participants: Vec<Participant>,
}

/// One migration transaction: the durable unit of the handoff state
/// machine (plan §3). Persisted as one JSON file per `migration_id`
/// with [`crate::MigrationStore`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationRecord {
    /// The migration identity (store key).
    pub migration_id: MigrationId,
    /// The migrated VM.
    pub vm_id: String,
    /// The source host (the coordinator's host).
    pub source_host: HostId,
    /// The destination host.
    pub target_host: HostId,
    /// The participating volumes, in preparation order (the order is
    /// part of the deterministic batch operation-id derivation).
    pub participants: Vec<Participant>,
    /// The canonical state.
    pub state: HandoffState,
    /// The cut progress (write-ahead), `None` outside the cut; retained
    /// through `SourceRevoked`/`DestinationAuthorized`/`VmResumed` (the
    /// D3 window stays `IN_DOUBT`-observable, G1) and cleared at
    /// `Complete`.
    pub cut: Option<CutProgress>,
    /// The append-only observable trace.
    pub state_history: Vec<StateHistoryEntry>,
    /// The per-volume barrier proofs collected at `BARRIER_DURABLE`.
    pub barrier_proofs: Vec<BarrierProof>,
    /// The abort policy (v1: `AutoBeforeCut` only).
    pub abort_policy: AbortPolicy,
    /// The consumer's `BarrierAndTransfer` proof, recorded verbatim as
    /// **corroboration** (stage B2, plan §6): volvisor performs and
    /// verifies its own pause (§5) and its own suspension proof (D2), so
    /// the parameter is recorded, never trusted — a false or absent
    /// proof changes nothing about the drive. The **first** recorded
    /// corroboration is kept; later proofs never overwrite it (the
    /// record is append-only in spirit, like the state history).
    ///
    /// Additive stage-B2 field: records written by stage B1 decode with
    /// `None` (serde default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_proof: Option<serde_json::Value>,
    /// Unix epoch seconds at `prepare`.
    pub created_at: u64,
    /// Unix epoch seconds at the last persisted transition.
    pub updated_at: u64,
    /// Unix epoch seconds when the cut began — the write-ahead's
    /// first durable step (`Snapshotting`) — the start of the measured
    /// wall-clock cut duration (plan §8 item 2: the completed
    /// migration's response carries it). Additive stage-B2 field:
    /// B1-era records decode with `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_started_at: Option<u64>,
    /// Unix epoch seconds when the state landed at `Complete` — the
    /// end of the measured cut duration. Additive stage-B2 field:
    /// B1-era records decode with `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_completed_at: Option<u64>,
    /// The barrier-time lineage re-check's typed refusal (P6-A F1,
    /// defense in depth): set when the barrier's replica-lineage
    /// re-verification refused `FOREIGN_DEVICE_STATE` — a
    /// wrong-lineage injection that landed on the target after the
    /// prepare. While the marker stands the record **parks for the
    /// operator**: the reconcile re-drives it (the re-check
    /// re-refuses — the same typed outcome every pass) instead of
    /// consuming it with the auto-before-cut rollback, which would
    /// silently convert a typed safety refusal into a plain abort.
    /// Cleared when the re-check passes again (the operator re-seeded
    /// the target; the drive then converges); retained through
    /// `Aborted` (the operator's abort is the marker's other exit).
    ///
    /// Additive field: records written before the re-check decode
    /// with `None` (serde default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barrier_lineage_refusal: Option<TypedRefusal>,
}

impl MigrationRecord {
    /// The `ObserveHandoff` projection of this record (plan §3):
    ///
    /// - a record with an active cut is observed as `IN_DOUBT` with
    ///   the cut step as detail **only before
    ///   `DESTINATION_AUTHORIZED`** (D1a);
    /// - from `DESTINATION_AUTHORIZED` on, the canonical state is
    ///   reported with a stall detail (a resolvable stall is the
    ///   canonical state plus detail; an unresolvable stall is
    ///   observed through the terminal `InDoubt` mapping, which the
    ///   reconcile records);
    /// - the terminal `InDoubt` observation surfaces its own detail.
    #[must_use]
    pub fn observe(&self) -> MigrationSummary {
        const AUTHORIZED_RANK: u8 = 5;
        const COMPLETE_RANK: u8 = 7;
        let mut in_doubt_detail = None;
        let state = if let Some(cut) = self.cut {
            // A live cut is observable only before DESTINATION_AUTHORIZED
            // (the rank guard); the write-ahead itself is retained
            // through `VmResumed` and cleared at `Complete` (plan §2).
            if self.state == HandoffState::Complete {
                // Unreachable through the coordinator (drive_complete
                // clears the cut in the same transition) — only a
                // crafted or corrupted store file can hold this
                // shape. Corruption reads as doubt, never as clean
                // (the same fail-closed direction as every other
                // observation rule).
                let detail = format!("inconsistent record: complete with a live cut ({cut})");
                in_doubt_detail = Some(detail.clone());
                HandoffState::InDoubt {
                    since: self.updated_at,
                    detail,
                }
            } else if self
                .state
                .forward_rank()
                .is_some_and(|rank| rank >= AUTHORIZED_RANK)
            {
                if self
                    .state
                    .forward_rank()
                    .is_some_and(|rank| (AUTHORIZED_RANK..COMPLETE_RANK).contains(&rank))
                {
                    in_doubt_detail = Some(format!(
                        "stalled in {}: migration not yet complete",
                        self.state
                    ));
                }
                self.state.clone()
            } else if self.state == HandoffState::SourceRevoked {
                // The D3 window: the revoke landed, the grant has not.
                // The record is forward-only and must not read as a
                // clean, progress-like canonical state (G1).
                let detail = "source revoked; destination grant not yet authorized".to_owned();
                in_doubt_detail = Some(detail.clone());
                HandoffState::InDoubt {
                    since: self.updated_at,
                    detail,
                }
            } else {
                let detail = format!("cut in progress: {cut}");
                in_doubt_detail = Some(detail.clone());
                HandoffState::InDoubt {
                    since: self.updated_at,
                    detail,
                }
            }
        } else if let HandoffState::InDoubt { detail, .. } = &self.state {
            in_doubt_detail = Some(detail.clone());
            self.state.clone()
        } else {
            if self
                .state
                .forward_rank()
                .is_some_and(|rank| (AUTHORIZED_RANK..COMPLETE_RANK).contains(&rank))
            {
                in_doubt_detail = Some(format!(
                    "stalled in {}: migration not yet complete",
                    self.state
                ));
            }
            self.state.clone()
        };
        MigrationSummary {
            state,
            state_history: self.state_history.clone(),
            participants: self.participants.clone(),
            in_doubt_detail,
            cut_duration_secs: match (self.cut_started_at, self.cut_completed_at) {
                (Some(started), Some(completed)) => Some(completed.saturating_sub(started)),
                _ => None,
            },
            barrier_lineage_refusal: self.barrier_lineage_refusal.clone(),
        }
    }
}

/// The read-only projection served by `ObserveHandoff`: the observed
/// state (with the cut/stall mapping), the append-only trace, the
/// participants and — when the observation is (or carries) an
/// `IN_DOUBT` — its detail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationSummary {
    /// The observed state (cut-progress mapped per
    /// [`MigrationRecord::observe`]).
    pub state: HandoffState,
    /// The append-only transition trace.
    pub state_history: Vec<StateHistoryEntry>,
    /// The participating volumes.
    pub participants: Vec<Participant>,
    /// The `IN_DOUBT` detail, when the observation is (or carries) an
    /// in-doubt.
    pub in_doubt_detail: Option<String>,
    /// The measured wall-clock cut duration (plan §8 item 2): seconds
    /// between the cut write-ahead's first durable step and the
    /// `Complete` transition, carried on every completed migration's
    /// observation. `None` before the cut begins and until the
    /// migration completes.
    pub cut_duration_secs: Option<u64>,
    /// The barrier-time lineage re-check's journaled typed refusal
    /// (P6-A F1), when one stands: the record parks for the operator
    /// — the canonical state stays `QUIESCED` (pre-cut, no barrier
    /// recorded) and this marker is the typed reason. Carried through
    /// `Aborted` (the operator's abort is the marker's other exit).
    pub barrier_lineage_refusal: Option<TypedRefusal>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(state: HandoffState, cut: Option<CutProgress>) -> MigrationRecord {
        MigrationRecord {
            migration_id: MigrationId::new("mig-1").expect("valid id"),
            vm_id: "vm-1".to_owned(),
            source_host: HostId::new("src").expect("valid id"),
            target_host: HostId::new("dst").expect("valid id"),
            participants: vec![Participant {
                volume_id: VolumeId::new("vol-1").expect("valid id"),
                expected_generation: 1,
                resource: "vol-1-res".to_owned(),
                minor: 7,
            }],
            state,
            cut,
            state_history: vec![],
            barrier_proofs: vec![],
            abort_policy: AbortPolicy::AutoBeforeCut,
            consumer_proof: None,
            created_at: 1,
            updated_at: 2,
            cut_started_at: None,
            cut_completed_at: None,
            barrier_lineage_refusal: None,
        }
    }

    #[test]
    fn observe_carries_the_measured_cut_duration_once_complete() {
        // Plan §8 item 2: a completed migration's observation carries
        // the measured wall-clock cut duration — the seconds between
        // the write-ahead's first durable step and `Complete`. Before
        // the cut begins, and until completion, it is absent.
        let mut in_flight = record(HandoffState::Quiesced, Some(CutProgress::Snapshotting));
        in_flight.cut_started_at = Some(100);
        assert_eq!(in_flight.observe().cut_duration_secs, None);
        let mut uncut = record(HandoffState::Complete, None);
        uncut.cut_completed_at = Some(145);
        assert_eq!(uncut.observe().cut_duration_secs, None, "no cut ever began");
        let mut complete = record(HandoffState::Complete, None);
        complete.cut_started_at = Some(100);
        complete.cut_completed_at = Some(145);
        assert_eq!(complete.observe().cut_duration_secs, Some(45));
    }

    #[test]
    fn observe_carries_a_standing_barrier_lineage_refusal() {
        // The park shape (P6-A F1): pre-cut, no barrier recorded, the
        // typed refusal journaled on the record.
        let mut parked = record(HandoffState::Quiesced, None);
        parked.barrier_lineage_refusal = Some(TypedRefusal {
            code: "FOREIGN_DEVICE_STATE".to_owned(),
            detail: "the live lineage of the target replica diverged from the \
                     source-supplied expected set"
                .to_owned(),
            at: 7,
        });
        let summary = parked.observe();
        assert_eq!(
            summary.state,
            HandoffState::Quiesced,
            "the parked record observes its canonical pre-cut state"
        );
        assert_eq!(
            summary.barrier_lineage_refusal,
            parked.barrier_lineage_refusal.clone(),
            "the observation carries the journaled typed refusal verbatim"
        );

        // The marker survives the operator's abort (the other exit).
        let mut aborted = parked;
        aborted.state = HandoffState::Aborted {
            reason: "operator abort".to_owned(),
            at: 9,
        };
        assert!(
            aborted.observe().barrier_lineage_refusal.is_some(),
            "the aborted record keeps the typed refusal of record"
        );
    }

    #[test]
    fn display_names_are_the_contract_spellings() {
        assert_eq!(HandoffState::Prepared.to_string(), "PREPARED");
        assert_eq!(HandoffState::Precopy.to_string(), "PRECOPY");
        assert_eq!(HandoffState::Quiesced.to_string(), "QUIESCED");
        assert_eq!(HandoffState::BarrierDurable.to_string(), "BARRIER_DURABLE");
        assert_eq!(HandoffState::SourceRevoked.to_string(), "SOURCE_REVOKED");
        assert_eq!(
            HandoffState::DestinationAuthorized.to_string(),
            "DESTINATION_AUTHORIZED"
        );
        assert_eq!(HandoffState::VmResumed.to_string(), "VM_RESUMED");
        assert_eq!(HandoffState::Complete.to_string(), "COMPLETE");
        assert_eq!(
            HandoffState::InDoubt {
                since: 1,
                detail: "d".to_owned()
            }
            .to_string(),
            "IN_DOUBT"
        );
        assert_eq!(
            HandoffState::Aborted {
                reason: "r".to_owned(),
                at: 1
            }
            .to_string(),
            "ABORTED"
        );
    }

    #[test]
    fn wire_formats_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&HandoffState::BarrierDurable).expect("serialize"),
            "\"barrier_durable\""
        );
        assert_eq!(
            serde_json::to_string(&HandoffState::DestinationAuthorized).expect("serialize"),
            "\"destination_authorized\""
        );
        let in_doubt = serde_json::to_string(&HandoffState::InDoubt {
            since: 9,
            detail: "void failed".to_owned(),
        })
        .expect("serialize");
        assert_eq!(
            in_doubt,
            "{\"in_doubt\":{\"since\":9,\"detail\":\"void failed\"}}"
        );
        for (cut, wire) in [
            (CutProgress::Snapshotting, "\"snapshotting\""),
            (CutProgress::DestroyingVm, "\"destroying_vm\""),
            (CutProgress::Demoting, "\"demoting\""),
            (CutProgress::Revoking, "\"revoking\""),
        ] {
            assert_eq!(
                serde_json::to_string(&cut).expect("serialize"),
                wire,
                "{cut:?}"
            );
            assert_eq!(
                serde_json::from_str::<CutProgress>(wire).expect("deserialize"),
                cut
            );
        }
        // A foreign spelling fails to decode rather than silently mapping.
        assert!(serde_json::from_str::<HandoffState>("\"PREPARED\"").is_err());
        assert!(serde_json::from_str::<CutProgress>("\"SNAPSHOTTING\"").is_err());
    }

    #[test]
    fn forward_rank_orders_the_canonical_path() {
        let path = [
            HandoffState::Prepared,
            HandoffState::Precopy,
            HandoffState::Quiesced,
            HandoffState::BarrierDurable,
            HandoffState::SourceRevoked,
            HandoffState::DestinationAuthorized,
            HandoffState::VmResumed,
            HandoffState::Complete,
        ];
        for (index, state) in path.iter().enumerate() {
            assert_eq!(state.forward_rank(), u8::try_from(index).ok(), "{state}");
        }
        assert_eq!(
            HandoffState::InDoubt {
                since: 0,
                detail: String::new()
            }
            .forward_rank(),
            None
        );
        assert_eq!(
            HandoffState::Aborted {
                reason: String::new(),
                at: 0
            }
            .forward_rank(),
            None
        );
    }

    #[test]
    fn record_denies_unknown_fields() {
        let json = serde_json::to_string(&record(HandoffState::Prepared, None)).expect("serialize");
        let foreign = json.replace("\"vm_id\"", "\"extra\": 1, \"vm_id\"");
        assert!(serde_json::from_str::<MigrationRecord>(&foreign).is_err());
        assert!(serde_json::from_str::<MigrationRecord>(&json).is_ok());
    }

    #[test]
    fn observe_maps_active_cut_to_in_doubt_before_authorization() {
        for cut in [
            CutProgress::Snapshotting,
            CutProgress::DestroyingVm,
            CutProgress::Demoting,
            CutProgress::Revoking,
        ] {
            let summary = record(HandoffState::BarrierDurable, Some(cut)).observe();
            assert!(
                matches!(summary.state, HandoffState::InDoubt { .. }),
                "{cut:?}"
            );
            assert_eq!(
                summary.in_doubt_detail.as_deref(),
                Some(format!("cut in progress: {cut}").as_str()),
                "{cut:?}"
            );
        }
    }

    #[test]
    fn observe_reports_canonical_state_with_stall_detail_from_authorization() {
        for state in [HandoffState::DestinationAuthorized, HandoffState::VmResumed] {
            // The cut write-ahead is retained through these states
            // (cleared at Complete); the canonical state still
            // reports, with the stall detail.
            for cut in [None, Some(CutProgress::Revoking)] {
                let summary = record(state.clone(), cut).observe();
                assert_eq!(summary.state, state, "cut={cut:?}");
                let detail = summary.in_doubt_detail.expect("stall detail");
                assert!(detail.contains("stalled"), "{detail}");
            }
        }
        let complete = record(HandoffState::Complete, None).observe();
        assert_eq!(complete.state, HandoffState::Complete);
        assert_eq!(complete.in_doubt_detail, None);
    }

    #[test]
    fn observe_reports_a_stalled_source_revoked_as_in_doubt() {
        // G1/D3: the cut write-ahead is retained through the D3 crash
        // window (revoke landed, grant has not). A record parked at
        // SOURCE_REVOKED must read as IN_DOUBT with a typed detail —
        // never as a clean, progress-like canonical state.
        let summary = record(HandoffState::SourceRevoked, Some(CutProgress::Revoking)).observe();
        assert!(
            matches!(summary.state, HandoffState::InDoubt { .. }),
            "a stalled SourceRevoked observes IN_DOUBT, got {:?}",
            summary.state
        );
        assert_eq!(
            summary.in_doubt_detail.as_deref(),
            Some("source revoked; destination grant not yet authorized")
        );
    }

    #[test]
    fn observe_reads_a_complete_with_live_cut_record_as_in_doubt() {
        // Unreachable through the coordinator (the cut is cleared in
        // the Complete transition); only a crafted/corrupted store
        // file can hold the shape. Corruption reads as doubt, never
        // as clean — the fail-closed direction of every observation
        // rule.
        let summary = record(HandoffState::Complete, Some(CutProgress::Revoking)).observe();
        assert!(
            matches!(summary.state, HandoffState::InDoubt { .. }),
            "complete-with-cut observes IN_DOUBT, got {:?}",
            summary.state
        );
        assert_eq!(
            summary.in_doubt_detail.as_deref(),
            Some("inconsistent record: complete with a live cut (revoking)")
        );
    }

    #[test]
    fn observe_surfaces_terminal_in_doubt_detail() {
        let terminal = record(
            HandoffState::InDoubt {
                since: 5,
                detail: "abort void failed; source fenced".to_owned(),
            },
            None,
        );
        let summary = terminal.observe();
        assert_eq!(
            summary.in_doubt_detail.as_deref(),
            Some("abort void failed; source fenced")
        );
        assert!(matches!(summary.state, HandoffState::InDoubt { .. }));
    }
}
