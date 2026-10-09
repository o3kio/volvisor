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
//!   `provider_conformance_tests!` macro.
//!
//! Dependency direction (implementation plan section 4): `volvisor-api` and
//! `volvisor-lvm` depend on this crate; this crate depends only on
//! `volvisor-types`.

#![deny(unsafe_code)]
// Tests may use expect/unwrap for invariant assertions; production code may not.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod conformance;
pub mod fake;
pub mod provider;

pub use fake::FakeProvider;
pub use provider::VolumeProvider;
