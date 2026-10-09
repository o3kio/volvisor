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
//!   [`CommandRunner`], with sizes verified against `lvs` after every
//!   resize (honest reporting, never pretended success);
//! - **durable JSON state** (atomic tmp-write + fsync + rename) holding the
//!   volume-to-LV cross-references, attachment records and device claims;
//! - **startup reconciliation**: state entries whose LV vanished are marked
//!   `Failed`; foreign LVs under our volume groups are reported, never
//!   touched.
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
pub mod runner;
pub mod state;

pub use admin::ReconcileReport;
pub use provider::{LvmProvider, PROVIDER_NAME};
pub use runner::{CommandOutput, CommandRunner, FakeRunner, Invocation, RealRunner};
