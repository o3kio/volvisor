//! # volvisor-provider
//!
//! Engine-neutral volume provider abstraction for the Volvisor control plane.
//!
//! The crate contains three pieces:
//!
//! - [`VolumeProvider`]: the async, object-safe trait every storage engine
//!   implements for the Volume API v2 surface (create / inspect / list /
//!   attach / detach / grow / delete), with capability negotiation and a
//!   binding semantic contract (generation fencing, single-writer
//!   attachments, fail-closed policy negotiation, host-scoped handles,
//!   honest health reporting, grow-only resize);
//! - [`FakeProvider`]: a deterministic, thread-safe, fault-injectable
//!   in-memory implementation used by the API and provider test suites;
//! - [`conformance`]: a reusable conformance kit every provider must pass
//!   (idempotent create, generation fencing, single-writer enforcement,
//!   detach drain preconditions, delete preconditions, unknown-health
//!   truthfulness), runnable via `conformance::assert_conformance` or the
//!   `provider_conformance_tests!` macro;
//! - [`handoff`]: the optional source-side coordinated-handoff surface
//!   ([`HandoffSurface`], the `AdoptionSurface` pattern) providers
//!   implement to back the migration coordinator (P4b plan §6);
//! - [`vmm`]: the VMM coordination seam of the coordinated handoff
//!   (P4b plan §5) — the engine-neutral [`vmm::VmmController`] trait,
//!   the Cloud Hypervisor `ch-remote` adapter
//!   ([`vmm::ChRemoteVmm`]) over [`CommandRunner`], and the TEST-ONLY
//!   [`vmm::FakeVmm`] harness that makes the device-open discipline
//!   provable in tests;
//! - [`vmm_http`]: the hand-rolled HTTP/1.1 `PUT` over a unix domain
//!   socket behind [`vmm::VmmController::resize_disk`] (P6-B — the
//!   REST-only resize-disk call; `ch-remote` has no such
//!   subcommand).
//!
//! Dependency direction (implementation plan section 4): `volvisor-api` and
//! `volvisor-lvm` depend on this crate; this crate depends only on
//! `volvisor-types`.

#![deny(unsafe_code)]
// Tests may use expect/unwrap for invariant assertions; production code may not.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod admin;
pub mod conformance;
pub mod fake;
pub mod handoff;
pub mod provider;
pub mod runner;
pub mod vmm;
pub mod vmm_http;

pub use admin::{AdminSurface, AdoptionSurface};
pub use fake::FakeProvider;
pub use handoff::{
    EligibilityParticipant, EligibilityReport, HandoffSurface, QuiesceProof, SyncProof,
};
pub use provider::VolumeProvider;
pub use runner::{CommandOutput, CommandRunner, FakeRunner, Invocation, RealRunner};
pub use vmm::{
    ChRemoteConfig, ChRemoteVmm, DeviceHook, DiskMapping, FakeFailKnobs, FakeResizeCall, FakeVmm,
    PauseProof, VmState, VmmController,
};
pub use vmm_http::{MAX_RESPONSE_BYTES, VmmHttpResponse};
