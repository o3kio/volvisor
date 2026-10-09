//! Provider conformance kit run against the external Ceph RBD provider.
//!
//! Every check constructs a fresh provider over an isolated simulated
//! Ceph cluster (see [`common`]); the fixtures script `ceph`/`rbd`
//! through the closure-mode [`FakeRunner`], so the provider's
//! read-back verification steps (image listing, `rbd info` sizes,
//! `rbd showmapped` mappings, trash placement) are exercised for real.
//!
//! The shared kit is hardwired to the `native-local` P0 profile, so the
//! fixtures wrap the real [`CephRbdProvider`] in
//! [`common::KitClassAdapter`], which rewrites only the volume-class
//! field in both directions — every other behavior under test (state,
//! generations, single-writer fencing, capacity, ownership proofs,
//! trash) is the real provider's.
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
