//! # volvisor-handoff
//!
//! The coordinated VM/storage migration transaction — stage B1 of the
//! [P4b plan](../../docs/plans/2026-10-09-p4b-vmm-storage-handoff.md)
//! (section 3): the canonical handoff state machine, its durable
//! per-migration store, and the external-facts-first `IN_DOUBT`
//! reconcile.
//!
//! This crate is deliberately **engine- and transport-neutral**: it
//! consumes [`volvisor_types`] only and drives every external effect
//! through the [`coordinator::HandoffDriver`] seam.
//! Stage B2 implements that seam over the real surfaces (VMM
//! controller, peer daemon API, witness client, provider handoff
//! surface); stage B1 tests it with a scripted fake. There is no
//! dependency on `volvisor-drbd` or `volvisor-witness`.
//!
//! The vocabulary ([`types`]) is exactly the nearline contract's
//! canonical states. Two properties are structural, not conventional:
//!
//! - **The cut is a durable, forward-only progress record** (plan
//!   D1a). Every cut step is persisted *before* its external act
//!   (write-ahead, the P4a marker-before-demote discipline), and every
//!   state at or past `cut=snapshotting` has **no abort handler** —
//!   reconcile can only drive such a record forward. A record with an
//!   active cut is observed as `IN_DOUBT` (with the step as detail)
//!   until `DESTINATION_AUTHORIZED`.
//! - **An unvoided recorded barrier is a hard gate on any source
//!   resume** (plan G5). The pre-cut rollback voids every recorded
//!   barrier first and only resumes once the void is *confirmed*; a
//!   void that cannot be journaled — for any reason, including witness
//!   unreachability — fails the whole rollback into `self_fence`
//!   (`fence_source` per participant) with the record left in terminal
//!   `InDoubt`. Never a silent resume, never an unmarked suspension.
//!
//! Durability follows the house `DrbdState` discipline: one JSON file
//! per migration, written tmp + fsync + rename + directory-fsync
//! ([`store`]); corrupt record files are a typed startup error, never
//! silently dropped.

#![deny(unsafe_code)]
// Tests may use expect/unwrap for invariant assertions; production code may not.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod coordinator;
pub mod store;
pub mod types;

pub use coordinator::{
    BatchStep, Clock, HandoffDriver, MigrationCoordinator, barrier_operation_id, batch_operation_id,
};
pub use store::MigrationStore;
pub use types::{
    AbortPolicy, BarrierProof, CutProgress, HandoffState, MigrationRecord, MigrationSummary,
    Participant, PrepareHandoffRequest, StateHistoryEntry,
};
