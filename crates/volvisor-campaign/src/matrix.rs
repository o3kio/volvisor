//! The generated kill matrix (P5 plan §3.2): the enumeration of
//! journaled operations × deterministic kill points the campaign
//! executes, and the coverage accounting that keeps the
//! enumeration honest — every operation kind the router exposes is
//! either driven by a matrix cell or recorded as a gap with its
//! reason, never silently cut (§3.2's bounds clause).
//!
//! The matrix is **generated, not hand-enumerated**: [`cells`] is
//! the cross product of each family's operation list with its
//! family's kill-point list, so a newly added operation kind or
//! crash point widens the matrix mechanically instead of by
//! copy-paste. The executors live in `tests/rows_4_7.rs` (one per
//! family, §9 rows 4–7); this module owns only the *shape*.
//!
//! # Sizes and bounds (§3.2's honesty spine)
//!
//! Each family's executor must finish inside the plan's per-family
//! budget (≤5 s wall) and record its duration in the evidence;
//! the whole campaign stays inside the suite's ~60 s budget. Cells
//! within a family run concurrently (each in its own rig), which
//! is what keeps the volume-mutation family's 30 cells inside the
//! family bound.

use volvisor_api::crash::CrashPoint;
use volvisor_api::op_kinds;
use volvisor_types::crash::StoreSavePoint;

/// One deterministic kill point (§3.1) expressed against the seam
/// that implements it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hook {
    /// The journal-append seam (`volvisor-api`): one journaled
    /// operation kind dies after its intent write, before its
    /// outcome write, or after it.
    Journal(CrashPoint),
    /// The DRBD provider state's atomic file save
    /// (`STORE_DRBD_STATE`).
    StateSave(StoreSavePoint),
    /// The migration record store's atomic file save
    /// (`STORE_MIGRATION_RECORDS`).
    RecordSave(StoreSavePoint),
    /// The witness journal's own commit, keyed by mutation kind
    /// (the mid-save variant on the witness's durable surface).
    WitnessCommit {
        /// The serde tag of the witness mutation the arm targets.
        mutation: &'static str,
        /// The mid-commit window.
        point: StoreSavePoint,
    },
    /// The witness's outage window: no armed seam — the listener
    /// goes away (the rig's stop) while the cut needs it, and the
    /// fail-closed behavior during the outage is the assertion.
    WitnessOutage,
}

impl Hook {
    /// The evidence record's fault-location string (§6: the fault
    /// kind plus the exact point).
    #[must_use]
    pub fn location(&self) -> String {
        match self {
            Hook::Journal(point) => format!("journal/{point:?}"),
            Hook::StateSave(point) => format!("drbd_state-save/{point:?}"),
            Hook::RecordSave(point) => format!("migration_records-save/{point:?}"),
            Hook::WitnessCommit { mutation, point } => {
                format!("witness_commit:{mutation}/{point:?}")
            }
            Hook::WitnessOutage => "witness-outage/stop-restart".to_owned(),
        }
    }

    /// The evidence record's fault kind (§6).
    #[must_use]
    pub fn fault_kind(&self) -> &'static str {
        match self {
            Hook::Journal(point) => match point {
                CrashPoint::AfterIntent | CrashPoint::AfterOutcome => "crash-after-journal-write",
                CrashPoint::BeforeOutcome => "crash-before-journal-write",
            },
            Hook::StateSave(_) | Hook::RecordSave(_) => "crash-inside-store-save",
            Hook::WitnessCommit { .. } => "crash-inside-witness-commit",
            Hook::WitnessOutage => "witness-outage",
        }
    }
}

/// One matrix family (§9 rows 4–7 name them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// Row 4: consumer volume mutations through the volume routes.
    VolumeMutations,
    /// Row 5: consumer mobility routes (prepare/transfer/abort).
    ConsumerMobility,
    /// Row 6: the destination's internal peer routes.
    PeerRoutes,
    /// Row 7: the witness journal (stop/restart + mid-save).
    WitnessJournal,
}

impl Family {
    /// The family's name (evidence scenario nesting, §9's row
    /// label).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Family::VolumeMutations => "volume-mutations",
            Family::ConsumerMobility => "consumer-mobility",
            Family::PeerRoutes => "peer-routes",
            Family::WitnessJournal => "witness-journal",
        }
    }
}

/// One generated cell: one operation of one family, killed at one
/// deterministic point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The family the cell belongs to (its executor, §9's row).
    pub family: Family,
    /// The journaled operation kind the cell kills (an
    /// [`op_kinds::ALL_OP_KINDS`] entry).
    pub op: &'static str,
    /// The kill point.
    pub hook: Hook,
}

impl Cell {
    /// The evidence scenario name (§6 nesting:
    /// `matrix/<family>/<op>/<point>`).
    #[must_use]
    pub fn scenario(&self) -> String {
        format!(
            "matrix/{}/{}/{}",
            self.family.name(),
            self.op,
            self.hook.location()
        )
    }
}

/// The volume-mutation family's operation list (§9 row 4: the
/// plan's letter is create/attach/detach/resize; delete is the
/// same pipeline and the same state-save seam, so it is included —
/// the gaps table carries only the device/adoption surface).
const VOLUME_OPS: [&str; 5] = [
    op_kinds::OP_CREATE_VOLUME,
    op_kinds::OP_ATTACH_VOLUME,
    op_kinds::OP_DETACH_VOLUME,
    op_kinds::OP_GROW_VOLUME,
    op_kinds::OP_DELETE_VOLUME,
];

/// The consumer-mobility family's operation list (§9 row 5).
const MOBILITY_OPS: [&str; 3] = [
    op_kinds::OP_MIGRATION_PREPARE,
    op_kinds::OP_MIGRATION_TRANSFER,
    op_kinds::OP_MIGRATION_ABORT,
];

/// The peer-route family's operation list (§9 row 6 — armed on the
/// destination daemon).
const PEER_OPS: [&str; 4] = [
    op_kinds::OP_PEER_PREPARE,
    op_kinds::OP_PEER_GRANT,
    op_kinds::OP_PEER_RESTORE_VM,
    op_kinds::OP_PEER_DISCARD,
];

/// The witness-journal family's mutation list (§9 row 7): the
/// barrier record (the cut's own witness mutation) and the grant
/// set (the destination's promote batch) — the two mutations whose
/// mid-commit windows the cut crosses.
const WITNESS_MUTATIONS: [&str; 2] = ["record_barrier", "grant_set"];

/// The journal-append kill points every journaled op exposes
/// (§3.1).
const JOURNAL_POINTS: [CrashPoint; 3] = [
    CrashPoint::AfterIntent,
    CrashPoint::BeforeOutcome,
    CrashPoint::AfterOutcome,
];

/// The atomic-file-save kill points (§3.1): the tmp/fsync/rename
/// splits. The recovery distinguishes exactly these: the first two
/// leave the OLD durable state, the third the NEW one.
const FILE_SAVE_POINTS: [StoreSavePoint; 3] = [
    StoreSavePoint::AfterTmpWrite,
    StoreSavePoint::AfterFsyncBeforeRename,
    StoreSavePoint::AfterRename,
];

/// The witness mid-commit windows (§3.1's witness variant).
const WITNESS_POINTS: [StoreSavePoint; 2] = [
    StoreSavePoint::WitnessAfterIntentAppend,
    StoreSavePoint::WitnessAfterApplyBeforeOutcome,
];

/// The generated matrix (§3.2): the full cross product, family by
/// family. Size and shape:
///
/// - volume mutations: 5 ops × (3 journal points + 3 state-save
///   splits) = 30 cells;
/// - consumer mobility: prepare × (3 journal + 3 record-save) plus
///   transfer × 3 journal plus abort × 3 journal = 12 cells;
/// - peer routes: 4 ops × 3 journal points = 12 cells;
/// - witness journal: 2 mutations × 2 mid-commit windows + the
///   outage = 5 cells.
///
/// 59 cells total. The transfer and peer-grant journal cells
/// intentionally regenerate stage A's K1–K5 shapes: the matrix is
/// the campaign's single enumeration, and regenerating them keeps
/// it mechanically complete rather than hand-curated.
#[must_use]
pub fn cells() -> Vec<Cell> {
    let mut matrix = Vec::new();
    for op in VOLUME_OPS {
        for point in JOURNAL_POINTS {
            matrix.push(Cell {
                family: Family::VolumeMutations,
                op,
                hook: Hook::Journal(point),
            });
        }
        for point in FILE_SAVE_POINTS {
            matrix.push(Cell {
                family: Family::VolumeMutations,
                op,
                hook: Hook::StateSave(point),
            });
        }
    }
    for op in MOBILITY_OPS {
        // Only the prepare's record save is a store-split cell: the
        // transfer's and the abort's record saves belong to the
        // drive/rollback (background paths whose kill points are
        // the peer-route and witness families' cells); the prepare
        // is the consumer-driven act whose save the rig can target
        // deterministically.
        let store_split = op == op_kinds::OP_MIGRATION_PREPARE;
        for point in JOURNAL_POINTS {
            matrix.push(Cell {
                family: Family::ConsumerMobility,
                op,
                hook: Hook::Journal(point),
            });
        }
        if store_split {
            for point in FILE_SAVE_POINTS {
                matrix.push(Cell {
                    family: Family::ConsumerMobility,
                    op,
                    hook: Hook::RecordSave(point),
                });
            }
        }
    }
    for op in PEER_OPS {
        for point in JOURNAL_POINTS {
            matrix.push(Cell {
                family: Family::PeerRoutes,
                op,
                hook: Hook::Journal(point),
            });
        }
    }
    for mutation in WITNESS_MUTATIONS {
        for point in WITNESS_POINTS {
            matrix.push(Cell {
                family: Family::WitnessJournal,
                op: op_kinds::OP_MIGRATION_TRANSFER,
                hook: Hook::WitnessCommit { mutation, point },
            });
        }
    }
    matrix.push(Cell {
        family: Family::WitnessJournal,
        op: op_kinds::OP_MIGRATION_TRANSFER,
        hook: Hook::WitnessOutage,
    });
    matrix
}

/// One family's cells, in generation order.
#[must_use]
pub fn family_cells(family: Family) -> Vec<Cell> {
    cells()
        .into_iter()
        .filter(|cell| cell.family == family)
        .collect()
}

/// A recorded coverage gap (§3.2: "cluster the kill points... and
/// record what was not covered — never silently cut"): an
/// operation kind the matrix does not drive, with the reason.
pub struct Gap {
    /// The un-driven operation kind.
    pub op: &'static str,
    /// Why it is not driven (recorded, not asserted away).
    pub reason: &'static str,
}

/// The operation kinds no matrix cell drives, with reasons. Four
/// are the admin device/adoption surface: they share the identical
/// journaled pipeline (`ops::execute` → the strict in-doubt rule)
/// and the identical `DrbdState` save seam the five driven volume
/// kinds prove, but they are not on the nearline handoff path the
/// campaign targets (§1's scope), and driving them would need
/// adoption fixtures (a foreign resource to adopt) the rig does not
/// model. The fifth is the P6-C same-VG move: the rig's daemon is
/// DRBD-backed (the move route's provider refuses
/// `MOVE_UNSUPPORTED_SCOPE` there), so the move's own durable
/// boundaries — the `LvmState` move-record saves — are carried by
/// §9 row 16 (this crate's `move_rows` binary, the LVM sibling
/// surface) instead of a matrix cell.
#[must_use]
pub fn recorded_gaps() -> Vec<Gap> {
    vec![
        Gap {
            op: op_kinds::OP_CLAIM_DEVICE,
            reason: "admin device-enrollment surface: off the nearline handoff path (plan \
                     §1 scope); same journaled pipeline and state-save seam the five driven \
                     volume kinds prove",
        },
        Gap {
            op: op_kinds::OP_RELEASE_DEVICE,
            reason: "admin device-enrollment surface: off the nearline handoff path (plan \
                     §1 scope); same journaled pipeline and state-save seam the five driven \
                     volume kinds prove",
        },
        Gap {
            op: op_kinds::OP_ADOPT_VOLUME,
            reason: "adoption surface: needs a foreign-resource fixture the rig does not \
                     model (plan §1: no destructive adoption in scope); same journaled \
                     pipeline and state-save seam the five driven volume kinds prove",
        },
        Gap {
            op: op_kinds::OP_CLEAR_CUT_MARKER,
            reason: "cut-marker hygiene route: exercised indirectly by every rollback \
                     recovery cell (the abort path clears the markers); not separately \
                     driven to keep the family budgets",
        },
        Gap {
            op: op_kinds::OP_MOVE_VOLUME_BACKING,
            reason: "P6-C same-VG extent move: the rig's daemon is DRBD-backed and refuses \
                     the route typed, so no matrix cell can drive its provider drive; the \
                     route's journal pipeline is the same ops::execute the volume-mutation \
                     family proves, and the move's own durable boundaries (the LvmState \
                     move-record saves, pvmove start, verified completion) are carried by \
                     §9 row 16 (move_rows, the LVM sibling fault rows)",
        },
    ]
}

/// The coverage accounting (§3.2's never-silently-cut rule, made
/// checkable): every kind in [`op_kinds::ALL_OP_KINDS`] is either
/// driven by at least one matrix cell or recorded as a gap. Panics
/// (it is a test-support crate's assertion) on any kind that is
/// neither — a new route without a matrix row fails here rather
/// than vanishing from the campaign.
///
/// # Panics
///
/// When an operation kind is neither driven nor recorded as a gap.
pub fn assert_full_coverage() {
    for kind in op_kinds::ALL_OP_KINDS {
        let driven = cells().iter().any(|cell| cell.op == kind);
        let gapped = recorded_gaps().iter().any(|gap| gap.op == kind);
        assert!(
            driven || gapped,
            "operation kind {kind} is neither driven by a matrix cell nor recorded as a gap \
             (plan §3.2: never silently cut)"
        );
    }
    // And the converse: a gap entry that names a driven kind (or no
    // kind at all) is a stale record.
    for gap in recorded_gaps() {
        assert!(
            op_kinds::ALL_OP_KINDS.contains(&gap.op),
            "gap entry names an unknown kind: {}",
            gap.op
        );
        assert!(
            !cells().iter().any(|cell| cell.op == gap.op),
            "gap entry {} names a kind the matrix drives (drop the gap)",
            gap.op
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The matrix's shape (§3.2's stated sizes — a change in the
    /// enumeration is a change in the campaign's claim surface, so
    /// it must be deliberate).
    #[test]
    fn the_generated_shape_is_the_stated_one() {
        let all = cells();
        assert_eq!(all.len(), 59, "the generated matrix is 59 cells");
        assert_eq!(family_cells(Family::VolumeMutations).len(), 30);
        assert_eq!(family_cells(Family::ConsumerMobility).len(), 12);
        assert_eq!(family_cells(Family::PeerRoutes).len(), 12);
        assert_eq!(family_cells(Family::WitnessJournal).len(), 5);
        // No duplicate cell (the cross product is a set).
        let mut seen = std::collections::BTreeSet::new();
        for cell in &all {
            assert!(
                seen.insert(cell.scenario()),
                "duplicate cell {}",
                cell.scenario()
            );
        }
    }

    /// The never-silently-cut rule holds (§3.2).
    #[test]
    fn every_journaled_kind_is_driven_or_recorded() {
        assert_full_coverage();
    }
}
