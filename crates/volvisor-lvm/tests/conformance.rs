//! Provider conformance kit run against the native-local LVM provider.
//!
//! Every check constructs a fresh provider over an isolated simulated LVM
//! (see [`common`]); the fixtures script `vgs`/`lvs`/`lvcreate`/
//! `lvextend`/`lvremove` through the closure-mode [`FakeRunner`], so the
//! provider's read-back verification steps are exercised for real.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::conformance_provider;
use volvisor_provider::VolumeProvider;
use volvisor_provider::conformance::assert_conformance;

fn make_provider() -> std::sync::Arc<volvisor_lvm::LvmProvider> {
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
