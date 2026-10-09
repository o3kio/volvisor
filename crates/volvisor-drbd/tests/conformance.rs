//! Provider conformance kit run against the DRBD9 nearline provider.
//!
//! Every check constructs a fresh provider over an isolated simulated
//! DRBD + LVM world (see [`common`]); the fixtures script `drbdadm` /
//! `drbdsetup` / `blockdev` / the LVM CLIs through the closure-mode
//! [`FakeRunner`], so the provider's read-back verification steps
//! (roles re-read from `drbdsetup status`, LV geometry from `lvs`,
//! device size from `blockdev --getsize64`) are exercised for real.
//!
//! The shared kit is hardwired to the `native-local` P0 profile, so the
//! fixtures wrap the real [`DrbdProvider`] in [`common::KitClassAdapter`],
//! which rewrites only the volume-class field, injects the `drbd9`
//! replication policy the kit never sends, and normalizes the health
//! axes back to the kit's `Unknown`-until-proven expectation — every
//! other behavior under test (state, generations, single-writer
//! fencing, capacity, ownership proofs, seeding, grow, delete) is the
//! real provider's.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::conformance_provider;
use volvisor_provider::VolumeProvider;
use volvisor_provider::conformance::assert_conformance;

fn make_provider() -> std::sync::Arc<common::KitClassAdapter> {
    conformance_provider()
}

volvisor_provider::provider_conformance_tests! { make_provider }

/// The whole suite must also pass against a single shared instance (the
/// checks use disjoint volume identities).
#[tokio::test]
async fn full_suite_on_a_shared_instance() {
    let provider = conformance_provider();
    let provider: &dyn VolumeProvider = provider.as_ref();
    let outcome = assert_conformance(provider).await;
    assert_eq!(outcome, Ok(()));
}
