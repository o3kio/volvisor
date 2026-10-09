//! # Writer authority vocabulary
//!
//! Engine-neutral types for the nearline writer-authority layer (nearline
//! contract v2 §2, [P4a plan
//! §2](../../docs/plans/2026-10-09-p4-witness-fencing-authority.md)).
//!
//! These types are the **protocol boundary** between the witness service
//! (`volvisor-witness`), the DRBD provider's authority integration and the
//! Volume API surface — which is why they live in this crate rather than in
//! any one consumer (ADR-0007's provider-neutral boundary for the fencing
//! phase).
//!
//! Invariants (each unit-tested in the witness crate):
//!
//! - [`WriterEpoch`] is strictly increasing per volume lineage, never reused
//!   and never shrunk across witness crash — epoch 0 is pre-authority (a
//!   volume created before the witness existed; contract §2: "Epochs
//!   increase monotonically, including after crash, promotion, cancellation
//!   and failback").
//! - [`FencingProof`] is evidence **returned by the witness after the
//!   fact** (the grant of a newer epoch, or an explicit recorded
//!   revocation) — never a precondition a caller supplies or a claim
//!   accepted from the host being fenced.
//! - [`PromotionClassification`] reports unknown loss boundaries as
//!   [`LossBoundary::Unknown`] rather than pretending precise bounds
//!   (contract §8; `SAFE_CURRENT` is evidence-gated, not protocol-gated).

use serde::{Deserialize, Serialize};

use crate::id::{HostId, VolumeId};

/// Monotonic writer epoch of one volume lineage.
///
/// Strictly increasing; 0 is the pre-authority epoch of volumes created
/// before witness registration. Never reused, never decremented — including
/// across witness crash (journal replay derives it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WriterEpoch(pub u64);

impl WriterEpoch {
    /// The pre-authority epoch: volumes created before the witness existed
    /// (P3-era volumes). A holder of this epoch was never granted authority
    /// by the witness and is treated as unregistered, not as fenced.
    #[must_use]
    pub const fn pre_authority() -> Self {
        Self(0)
    }

    /// Whether this epoch predates witness authority.
    #[must_use]
    pub const fn is_pre_authority(self) -> bool {
        self.0 == 0
    }
}

/// Opaque identity of one lease grant (unique per grant, never reused).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LeaseId(pub u64);

/// Observed state of a volume's current lease at the witness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    /// A lease is live for the current epoch and holder.
    Live,
    /// The lease's recorded end has passed (witness-clock evaluation).
    Expired,
    /// The lease was explicitly revoked (forced or self-initiated).
    Revoked,
    /// No lease exists for the current epoch.
    None,
}

/// One endpoint's backing identity, recorded at registration time.
///
/// A registration captures **both** endpoints (nearline replication has
/// exactly two data ends in the P4a model), each with the backing facts the
/// adopt flow on that host must match.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointBacking {
    /// Host holding this end of the replication.
    pub host_id: HostId,
    /// Backing identity as recorded at registration (e.g. the LV
    /// `vg/lv` name and the resource-definition facts). Opaque to the
    /// witness; compared verbatim by the adopt flow.
    pub backing: String,
    /// Whether volvisor itself created this backing (it then carries the
    /// `volvisor.owner` LV tag) or it is operator-provisioned (the P3 peer
    /// side; no tag — the lineage match carries the rule-7 weight there).
    pub volvisor_created: bool,
}

/// An operator-attested barrier recorded at registration (P4a's only
/// `SAFE_CURRENT` evidence source; P4b's `BARRIER_DURABLE` automates it).
///
/// The attestation must explicitly cover the **last acknowledged boundary**
/// property: source-committed, connection-established at the boundary, and
/// **no writes acknowledged past it** — a barrier recorded mid-serving
/// proves nothing about the tail written after it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedBarrier {
    /// Opaque boundary token as attested.
    pub boundary: String,
    /// The operator attestation, recorded verbatim.
    pub attestation: String,
    /// Unix epoch seconds at which the barrier was recorded.
    pub recorded_at: u64,
}

/// The registration record a witness holds for a volume (P4a plan §3:
/// `register`), returned by `inspect` for the adopt flow to compare
/// against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeRegistration {
    /// The registered volume.
    pub volume_id: VolumeId,
    /// The DRBD data-generation UUID set (`drbdsetup show-gi`) captured at
    /// registration — the lineage proof that closes the recreated-volume
    /// hole (the name scheme alone cannot distinguish a rebuilt cluster's
    /// same-named volume). Sorted, compared as a set.
    pub lineage_uuids: Vec<String>,
    /// Both endpoints' backing identities.
    pub endpoints: Vec<EndpointBacking>,
    /// The recorded barrier, if any.
    pub barrier: Option<RecordedBarrier>,
    /// Unix epoch seconds at registration.
    pub registered_at: u64,
}

/// The witness's durable statement that an epoch of a volume was retired.
///
/// Produced by the grant of a strictly newer epoch (which retires all older
/// ones) or by an explicit recorded revocation. Irrevocable: the witness
/// will never accept a renewal for a retired epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FencingProof {
    /// The volume whose writer was fenced.
    pub volume_id: VolumeId,
    /// The epoch that can no longer admit writes.
    pub retired_epoch: WriterEpoch,
    /// The epoch whose grant retired it (strictly newer; equals
    /// `retired_epoch + 1` or more).
    pub granted_epoch: WriterEpoch,
    /// The witness commit index that durably recorded the retirement —
    /// contract §1's `authority_commit_index`.
    pub commit_index: u64,
}

/// The witness's full view of one volume's authority — what `inspect`
/// returns and what the promotion authority check reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorityView {
    /// The queried volume.
    pub volume_id: VolumeId,
    /// The current (highest ever granted) epoch.
    pub current_epoch: WriterEpoch,
    /// The current epoch's holder, if any was ever granted.
    pub holder: Option<HostId>,
    /// The current lease's state.
    pub lease_state: LeaseState,
    /// For a live lease: its remaining duration **as a
    /// duration-from-response** (plan W5 — a revived writer must never
    /// reconstruct a local deadline from an absolute timestamp).
    pub lease_remaining_secs: Option<u64>,
    /// The witness commit index that last changed this authority.
    pub commit_index: u64,
    /// The full registration record, when the volume is registered.
    pub registration: Option<VolumeRegistration>,
}

/// The tenant/consumer-facing authority summary reported by
/// `InspectVolume` for nearline volumes (observed facts only — never a
/// claim that authority is *correct*, only what the witness last said).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthoritySummary {
    /// The volume's current writer epoch (0 = pre-authority).
    pub epoch: WriterEpoch,
    /// The observed lease state.
    pub lease_state: LeaseState,
    /// The current holder, if any.
    pub holder: Option<HostId>,
    /// For a live lease: remaining seconds as of the observation.
    pub lease_remaining_secs: Option<u64>,
}

/// What is known about the acknowledged-write boundary a promotion might
/// lose (contract §8: report unknown rather than pretend precise bounds).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LossBoundary {
    /// A recorded boundary token (only with recorded barrier evidence).
    Known(String),
    /// The tail is unknowable from the surviving facts.
    Unknown,
}

/// The honest classification of an unplanned promotion (ADR-0004 Decision 5,
/// nearline contract §8). Computed from observed facts only; never from
/// claims of the dead host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionClassification {
    /// Old authority irrevocably fenced **and** the full acknowledged tail
    /// proved durably present (recorded-barrier evidence; P4a's only source
    /// is an operator-attested registration barrier).
    SafeCurrent,
    /// Old authority fenced but the acknowledged tail cannot be proved
    /// present. Requires explicit `allow_loss` authorization, recorded with
    /// the exposure evidence.
    PossibleLoss {
        /// The (usually unknown) boundary of potential loss.
        boundary: LossBoundary,
        /// Whether the explicit loss authorization was recorded.
        authorized: bool,
    },
    /// Promotion refused: old authority may still write, lineage conflicts,
    /// or integrity is unknown. Never promoted, never "partial".
    Unsafe {
        /// Stable, human-readable reasons (diagnostic; the variant is the
        /// contract).
        reasons: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epochs_order_and_pre_authority() {
        assert!(WriterEpoch::pre_authority().is_pre_authority());
        assert!(!WriterEpoch(1).is_pre_authority());
        assert!(WriterEpoch(2) > WriterEpoch(1));
        assert_eq!(WriterEpoch(7).0, 7);
    }

    #[test]
    fn authority_vocabulary_round_trips_through_json() {
        // The wire vocabularies are snake_case and deny nothing they did
        // not define; a foreign spelling must fail to decode rather than
        // silently map.
        assert_eq!(
            serde_json::to_string(&LeaseState::Live).expect("serialize"),
            "\"live\""
        );
        for (state, wire) in [
            (LeaseState::Live, "\"live\""),
            (LeaseState::Expired, "\"expired\""),
            (LeaseState::Revoked, "\"revoked\""),
            (LeaseState::None, "\"none\""),
        ] {
            assert_eq!(
                serde_json::to_string(&state).expect("serialize"),
                wire,
                "{state:?}"
            );
            assert_eq!(
                serde_json::from_str::<LeaseState>(wire).expect("deserialize"),
                state
            );
        }
        assert!(serde_json::from_str::<LeaseState>("\"LIVE\"").is_err());

        let classification = PromotionClassification::PossibleLoss {
            boundary: LossBoundary::Unknown,
            authorized: false,
        };
        let json = serde_json::to_string(&classification).expect("serialize");
        assert_eq!(
            json,
            "{\"possible_loss\":{\"boundary\":\"unknown\",\"authorized\":false}}"
        );
        let safe = PromotionClassification::SafeCurrent;
        assert_eq!(
            serde_json::to_string(&safe).expect("serialize"),
            "\"safe_current\""
        );
    }

    #[test]
    fn epochs_and_leases_serialize_transparently() {
        assert_eq!(
            serde_json::to_string(&WriterEpoch(12)).expect("serialize"),
            "12"
        );
        assert_eq!(
            serde_json::to_string(&LeaseId(99)).expect("serialize"),
            "99"
        );
        assert_eq!(
            serde_json::from_str::<WriterEpoch>("12").expect("deserialize"),
            WriterEpoch(12)
        );
    }

    #[test]
    fn authority_summary_round_trips() {
        let summary = AuthoritySummary {
            epoch: WriterEpoch(3),
            lease_state: LeaseState::Live,
            holder: Some(HostId::new("node-a").expect("host id")),
            lease_remaining_secs: Some(42),
        };
        let json = serde_json::to_string(&summary).expect("serialize");
        let back: AuthoritySummary = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, summary);
        assert!(json.contains("\"lease_remaining_secs\":42"));
    }
}
