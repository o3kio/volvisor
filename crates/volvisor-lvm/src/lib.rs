//! # volvisor-lvm
//!
//! Native-local LVM provider (the P1 vertical slice of the P0 plan).
//!
//! The provider serves [`volvisor_types::domain::VolumeClass::NativeLocal`]
//! with thick logical volumes on explicitly claimed local disks:
//!
//! - **read-only discovery** (`lsblk` + `/dev/disk/by-id` resolution) with
//!   stable hardware identities (WWN/serial-derived), never Linux device
//!   names or PCI BDF (SPEC-0002 section 3, AGENTS rule 7);
//! - **explicit device claiming** under a scoped destructive-authorization
//!   token; foreign physical-volume state is never adopted
//!   (`FOREIGN_DEVICE_STATE`);
//! - **thick LV volumes** driven through the `lvm2` CLI via a shell-free
//!   [`CommandRunner`], with effective sizes verified against `lvs` after
//!   every create/grow (thick LVM rounds up to physical extents; the
//!   effective size is persisted and reported honestly, never a pretended
//!   success);
//! - **durable JSON state** (atomic tmp-write + fsync + rename) holding the
//!   volume-to-LV cross-references, attachment records, device claims and
//!   move records (the `MoveVolumeBackingOnline` journal);
//! - **startup reconciliation**: state entries whose LV vanished are
//!   marked `Failed`; device claims whose volume group is verifiably
//!   gone (from a successful `vgs` query — never on a failed one) are
//!   dropped; foreign LVs under our volume groups are reported, never
//!   touched; journaled moves are re-classified from the world (a live
//!   mirror rolls the record to `COPYING`, a provable relocation
//!   completes it, an ended-without-relocating move parks `IN_DOUBT`
//!   with the source intact — never a silent revert);
//! - **same-VG extent moves** (ADR-0006 first slice part 2, capability
//!   `same_vg_extent_move`): a scoped background `pvmove` evacuates a
//!   volume's extents to another PV of the same VG, supervised
//!   windowed by the caller's request. The states are the honest
//!   subset `PREPARING → COPYING → COMPLETE`; an unknown mid-move
//!   outcome parks `IN_DOUBT` (the source keeps serving; the extents
//!   are only freed after a *verified* relocation — a failed
//!   verification never frees). The kernel-side mirror and the
//!   backgrounded `pvmove` live outside the daemon: a restart
//!   re-derives everything from the durable record plus `lvs`.
//!
//! Everything is prototype evidence: health is `Unknown` until proven and
//! `evidence_status` reports `PrototypeOnly` (AGENTS rule 12).

#![deny(unsafe_code)]
// Tests may use expect/unwrap for invariant assertions; production code may not.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod admin;
pub mod discover;
pub mod provider;
pub mod report;
pub mod state;

pub use admin::ReconcileReport;
pub use provider::{LvmProvider, PROVIDER_NAME};
// The command-execution abstraction is shared with the Ceph adapter and
// lives in volvisor-provider; re-exported here for API stability.
pub use volvisor_provider::runner::{
    CommandOutput, CommandRunner, FakeRunner, Invocation, RealRunner,
};
