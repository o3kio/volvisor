//! # volvisor-drbd
//!
//! The DRBD9 nearline-replicated provider (ADR-0007, the P3 baseline):
//! host-kernel DRBD over an operator-designated LVM volume group on the
//! local host, driven through the unmodified `drbdadm` / `drbdsetup` /
//! `blockdev` / LVM command-line tools via the shell-free
//! [`CommandRunner`](volvisor_provider::runner) shared with the LVM and
//! Ceph adapters. **No new replication engine exists here** (AGENTS
//! rule 13): the DRBD9 kernel module and userland are used exactly as
//! shipped.
//!
//! Volvisor owns exactly the **local end** of every resource —
//!
//! - the local backing logical volume, created in the configured
//!   operator-designated VG with the immutable `volvisor.owner` /
//!   `volvisor.generation` LVM tags (verified read-back before state is
//!   persisted and re-verified before every mutation; a foreign LV is
//!   never adopted or touched, rule 7);
//! - the generated single-resource `.res` file
//!   (`<config_dir>/volvisor-<resource>.res`, owner-only `0600`, atomic
//!   tmp-write + rename), carrying the operator-declared peer address,
//!   the shared secret read from an operator-provisioned 0600 file, and
//!   the protocol letter fixed at create;
//! - the resource lifecycle (`create-md`, `up`, `down`) and the local
//!   role (`primary`/`secondary`), every `drbdadm` invocation scoped
//!   through `-c <our own file>` so a foreign resource can never be
//!   named;
//!
//! — while the **peer** is operator-provisioned out of band with the
//! identical definition; volvisor never writes to it and never adopts
//! peer state beyond what `drbdsetup status` observes.
//!
//! Honesty rules that shape the implementation (see
//! //! [`provider`] for the full list): single-writer maps
//! to DRBD single-primary with role re-verified from status after every
//! promotion/demotion (a successful exit status is never evidence);
//! seeding (`primary --force`) runs only over a provably fresh resource
//! and a peer observed holding data is never overwritten; the Protocol
//! A remote-protection axis states only that a remote replica is
//! currently established and observed `UpToDate`, never a durability
//! claim (rule 16); grow-only resize fails closed with the honest
//! boundary when the device does not reach the request; and
//! `evidence_status` stays `PrototypeOnly` (rule 12).
//!
//! Durable provider state (the volume→resource cross-references, the
//! monotonic minor/port allocation counters, attachment records) lives
//! in [`state`] as owner-only atomic JSON; reconciliation
//! findings live in [`report`].

#![deny(unsafe_code)]
// Tests may use expect/unwrap for invariant assertions; production code may not.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod authority;
pub mod provider;
pub mod report;
pub mod resgen;
pub mod state;

pub use authority::AuthorityContext;
pub use provider::{DrbdProvider, DrbdProviderConfig, PROVIDER_NAME, resource_name_for};
pub use state::{
    ClearedAttachment, ClearedAttachmentReason, ReconcileReport, ReplicationMode,
    VolumeAuthorityBlock,
};
// The command-execution abstraction is shared with the LVM and Ceph
// adapters and lives in volvisor-provider; re-exported here for API
// stability.
pub use volvisor_provider::runner::{
    CommandOutput, CommandRunner, FakeRunner, Invocation, RealRunner,
};
