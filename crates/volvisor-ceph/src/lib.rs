//! # volvisor-ceph
//!
//! External-cluster Ceph RBD provider (the P2 vertical slice of the
//! [P2 plan](../../../docs/plans/2026-10-09-p2-ceph-rbd-adapter.md),
//! ADR-0005 Phase A).
//!
//! The provider serves [`volvisor_types::domain::VolumeClass::CephRbd`]
//! with RBD images on an **existing, externally operated** Ceph cluster:
//!
//! - **fail-closed startup verification**: the cluster FSID must match the
//!   configuration exactly (a mis-pointed cluster is never adopted), the
//!   configured pool must exist, and a health query must succeed;
//! - **injective image names** (`vol-<sanitized>-<hash8>`, mirroring the
//!   LVM provider's scheme) plus an immutable `volvisor.owner` /
//!   `volvisor.generation` ownership record in RBD image metadata, set on
//!   create, verified read-back before state is persisted, and re-verified
//!   before every mutation — foreign images are never adopted or touched
//!   (AGENTS rule 7);
//! - **CLI-driven** through the shell-free
//!   [`CommandRunner`](volvisor_provider::runner) shared with the LVM
//!   adapter (watchdog-bounded, argv arrays, never a shell); every
//!   invocation carries `-m <mons>` and `--id <user>` so no environment
//!   or config-file side channel is involved (the ceph CLI resolves
//!   credentials itself; this provider never reads key material);
//! - **single-writer attach** via the RBD `exclusive-lock` image feature:
//!   attach maps the image (`rbd map`) and returns the `/dev/rbd*` device
//!   as the host-scoped ephemeral handle; a pre-existing (crash-leftover)
//!   mapping is detected through `rbd showmapped` and rejected
//!   fail-closed, never silently adopted;
//! - **honest capacity** (`ceph df` pool statistics against a documented
//!   headroom) and **honest health reflection** (`ceph health`:
//!   OK→Healthy, WARN→Degraded, ERR→Unhealthy, query failure→Unknown) on
//!   the read-only pool discovery surface — never converted into a
//!   Volvisor-made durability guarantee (ADR-0005);
//! - **grow-only resize** (`rbd resize --allow-shrink=false`) with the
//!   effective size verified from `rbd info` afterwards;
//! - **erasure policy**: `Retain` moves the image to the RBD trash
//!   (recoverable, verified via `rbd trash ls`); `ZeroDiscard` is a typed
//!   `UNSUPPORTED_CLASS_OR_POLICY` rejection (Ceph reclaim does not
//!   guarantee block-level zeroing; fail-closed policy negotiation,
//!   Volume API v2 section 4D);
//! - **durable JSON state** (owner-only `0600`, atomic tmp-write + fsync +
//!   rename) holding the volume-to-image cross-references and attachment
//!   records; **startup reconciliation** marks volumes whose image
//!   vanished or whose ownership metadata mismatched as `Failed` and
//!   reports foreign images without touching them.
//!
//! Everything is prototype evidence: volume health axes report `Unknown`
//! until proven and `evidence_status` reports `PrototypeOnly` (AGENTS rule
//! 12). No `AdminSurface` exists here (external cluster; no device
//! claiming — the daemon routes admin calls to the existing typed 404).

#![deny(unsafe_code)]
// Tests may use expect/unwrap for invariant assertions; production code may not.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod provider;
pub mod report;
pub mod state;

pub use provider::{CephProviderConfig, CephRbdProvider, PROVIDER_NAME};
pub use state::ReconcileReport;
// The command-execution abstraction is shared with the LVM adapter and
// lives in volvisor-provider; re-exported here for API stability.
pub use volvisor_provider::runner::{
    CommandOutput, CommandRunner, FakeRunner, Invocation, RealRunner,
};
