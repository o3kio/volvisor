//! Shared fixtures for the `volvisor-drbd` integration tests: the
//! extracted [`volvisor_drbd_testkit`] crate (the simulated DRBD + LVM
//! world, the seeding helpers and the conformance-kit adapter), shared
//! with the daemon-level end-to-end tests.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

pub use volvisor_drbd_testkit::*;
