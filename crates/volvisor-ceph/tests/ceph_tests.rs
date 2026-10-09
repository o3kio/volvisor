//! Ceph-specific provider tests beyond the generic conformance kit.
//!
//! Covers the semantics unique to the external-cluster RBD backend: the
//! fail-closed startup verification, ownership metadata proofs, stale
//! `rbd map` mappings, trash-verified deletes, honest capacity against
//! `ceph df`, crash-window recovery (create reclaim, unrecorded-grow
//! healing), honest unknowns on transient metadata failures, startup
//! reconciliation (missing/mismatched/foreign/stale state) and the argv
//! contract (`-m`/`--name` on every invocation).
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::{Arc, Mutex};

use common::{
    FSID, FakeCeph, MON_HOSTS, POOL_MAX_AVAIL, USER, config, fixture, leak_tempdir, provider_from,
    seed_foreign_image, seed_mapping, seed_volume,
};
use volvisor_ceph::provider::{CEPH_HEADROOM_BYTES, CephRbdProvider, image_name_for};
use volvisor_ceph::state::{CephState, ClearedAttachment, ClearedAttachmentReason as ClearReason};
use volvisor_provider::VolumeProvider;
use volvisor_provider::conformance::{
    fixture_attach_request, fixture_create_request, fixture_delete_request, fixture_detach_request,
    fixture_grow_request,
};
use volvisor_types::domain::VolumeClass;
use volvisor_types::request::{
    AccessModeRequest, CreateVolumeRequest, ErasurePolicy, GrowGuestNotification,
};
use volvisor_types::{ApiErrorCode, AttachmentState, Frontend, VolumeId, VolumeLifecycle};

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

fn volume_id(raw: &str) -> VolumeId {
    VolumeId::new(raw).expect("valid fixture volume id")
}

/// A valid `ceph-rbd` create request (the shared kit fixture is
/// hardwired to `native-local`; only the class field differs here).
fn create_request(id: &str, size_bytes: u64) -> CreateVolumeRequest {
    let mut request = fixture_create_request(id, size_bytes);
    request.volume_class = VolumeClass::CephRbd;
    request
}

/// Attempt a provider construction over a fresh state file, asserting
/// that nothing was persisted; success collapses to `()`.
fn try_new(
    world: &Arc<Mutex<FakeCeph>>,
    config: volvisor_ceph::provider::CephProviderConfig,
) -> Result<(), volvisor_types::ApiError> {
    let state_path = leak_tempdir().join("state.json");
    let runner = FakeCeph::runner(world);
    let result = CephRbdProvider::new(runner, config, state_path.clone());
    assert!(
        !state_path.exists(),
        "a refused startup must never write state"
    );
    result.map(|_| ())
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_lifecycle_maps_unmaps_and_trashes() {
    let fixture = fixture();
    let id = volume_id("life-vol");

    // Create: generation 1, Ready, byte-exact size (no extent rounding).
    let created = fixture
        .provider
        .create_volume(&create_request("life-vol", GIB))
        .await
        .expect("create");
    assert_eq!(created.generation, 1);
    assert_eq!(created.state, VolumeLifecycle::Ready);
    assert_eq!(created.provisioned_bytes, GIB);
    assert_eq!(created.allocated_bytes, GIB);
    assert_eq!(created.backend_class, VolumeClass::CephRbd);
    let image_name = image_name_for(&id);
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .images
            .get(&image_name)
            .expect("image exists")
            .size,
        GIB
    );

    // Attach: single-writer, the /dev/rbd* device is the frontend handle.
    let attached = fixture
        .provider
        .attach_volume(&id, &fixture_attach_request("life-vol", "life-att", 1))
        .await
        .expect("attach");
    assert_eq!(attached.attachment_generation, 1);
    assert_eq!(attached.volume_generation, 2);
    assert_eq!(
        attached.frontend,
        Frontend::VirtioBlk {
            host_device_path: "/dev/rbd0".to_owned()
        }
    );
    assert_eq!(attached.state, AttachmentState::Prepared);
    let device = "/dev/rbd0";
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .mappings
            .get(device)
            .map(String::as_str),
        Some(image_name.as_str())
    );

    // Grow while attached: the guest notification honestly stays pending.
    let grown = fixture
        .provider
        .grow_volume(&id, &fixture_grow_request("life-vol", 2 * GIB, 2))
        .await
        .expect("grow");
    assert!(grown.backing_resized);
    assert_eq!(grown.effective_size_bytes, 2 * GIB);
    assert_eq!(
        grown.guest_notification_status,
        GrowGuestNotification::RetryRequired
    );

    // Detach: authority released, the mapping is really gone.
    let detached = fixture
        .provider
        .detach_volume(
            &id,
            &attached.attachment_id,
            &fixture_detach_request("life-att", attached.attachment_generation),
        )
        .await
        .expect("detach");
    assert_eq!(detached.state, VolumeLifecycle::Ready);
    // Attach, grow and detach each advanced the generation once.
    assert_eq!(detached.generation, 4);
    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .mappings
            .contains_key(device)
    );

    // Delete with Retain: the image lands in the RBD trash (recoverable).
    fixture
        .provider
        .delete_volume(
            &id,
            &fixture_delete_request("life-vol", detached.generation),
        )
        .await
        .expect("delete");
    {
        let world = fixture.world.lock().expect("world");
        assert!(!world.images.contains_key(&image_name));
        assert!(world.trash.contains(&image_name), "image is in the trash");
    }

    let err = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect_err("deleted volume is gone");
    assert_eq!(err.code, ApiErrorCode::NotFound);
}

#[tokio::test]
async fn create_replays_idempotently_and_conflicts_on_a_different_size() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&create_request("replay-vol", MIB))
        .await
        .expect("create");

    // The same request replays to the stored volume.
    let replayed = fixture
        .provider
        .create_volume(&create_request("replay-vol", MIB))
        .await
        .expect("idempotent replay");
    assert_eq!(replayed.generation, created.generation);
    assert_eq!(replayed.provisioned_bytes, MIB);

    // A different size for the same identity is a typed conflict.
    let err = fixture
        .provider
        .create_volume(&create_request("replay-vol", 2 * MIB))
        .await
        .expect_err("conflicting replay");
    assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);

    // Exactly one image exists for the volume.
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .images
            .keys()
            .filter(|name| name.starts_with("vol-replay-vol"))
            .count(),
        1
    );
}

#[tokio::test]
async fn create_replay_reflects_a_vanished_backing_instead_of_reporting_ready() {
    let fixture = fixture();
    let id = volume_id("replay-gone");
    fixture
        .provider
        .create_volume(&create_request("replay-gone", MIB))
        .await
        .expect("create");

    // The image is manually removed behind the provider's back; the
    // replayed create must route through the same backing verification
    // as a fresh inspect and reflect the observed failure, not echo a
    // stale Ready.
    fixture
        .world
        .lock()
        .expect("world")
        .images
        .remove(&image_name_for(&id));
    let replayed = fixture
        .provider
        .create_volume(&create_request("replay-gone", MIB))
        .await
        .expect("the replay itself still succeeds idempotently");
    assert_eq!(replayed.state, VolumeLifecycle::Failed, "{replayed:?}");
}

#[tokio::test]
async fn create_rejects_a_non_512_aligned_size() {
    let fixture = fixture();
    let err = fixture
        .provider
        .create_volume(&create_request("unaligned-vol", GIB + 1))
        .await
        .expect_err("non-512-aligned size");
    assert_eq!(err.code, ApiErrorCode::InvalidRequest);
    let listed = fixture.provider.list_volumes(None).await.expect("list");
    assert_eq!(listed, []);
}

#[tokio::test]
async fn create_rejects_unsupported_classes_and_policies() {
    let fixture = fixture();

    // The provider serves only ceph-rbd: fail-closed class negotiation.
    let err = fixture
        .provider
        .create_volume(&fixture_create_request("class-vol", MIB))
        .await
        .expect_err("native-local is not served here");
    assert_eq!(err.code, ApiErrorCode::UnsupportedClassOrPolicy);

    // Thick preallocation is not implemented (RBD images here are thin).
    let mut thick = create_request("thick-vol", MIB);
    thick.provisioning = Some(volvisor_types::domain::Provisioning::Thick);
    let err = fixture
        .provider
        .create_volume(&thick)
        .await
        .expect_err("thick provisioning");
    assert_eq!(err.code, ApiErrorCode::UnsupportedClassOrPolicy);

    // Encryption is a typed rejection, never silently ignored.
    let mut encrypted = create_request("enc-vol", MIB);
    encrypted.encryption = Some(volvisor_types::request::EncryptionRequest {
        mode: "provider-managed".to_owned(),
        key_ref: "ref-1".to_owned(),
    });
    let err = fixture
        .provider
        .create_volume(&encrypted)
        .await
        .expect_err("encryption");
    assert_eq!(err.code, ApiErrorCode::UnsupportedClassOrPolicy);

    let listed = fixture.provider.list_volumes(None).await.expect("list");
    assert_eq!(listed, []);
}

#[tokio::test]
async fn create_beyond_safe_capacity_is_typed() {
    let fixture = fixture();
    // The request plus the documented headroom no longer fits.
    fixture.world.lock().expect("world").pool_max_avail = GIB + CEPH_HEADROOM_BYTES - 512;
    let err = fixture
        .provider
        .create_volume(&create_request("toobig-vol", GIB))
        .await
        .expect_err("create beyond safe capacity");
    assert_eq!(err.code, ApiErrorCode::NoSafeCapacity, "{err}");

    // Nothing was created or persisted.
    let listed = fixture.provider.list_volumes(None).await.expect("list");
    assert_eq!(listed, []);
    assert!(fixture.world.lock().expect("world").images.is_empty());
}

#[tokio::test]
async fn allocations_reduce_reported_free_and_delete_returns_it() {
    let fixture = fixture();
    // Baseline: enough for one image plus headroom, but not for two.
    fixture.world.lock().expect("world").pool_max_avail = 2 * MIB + CEPH_HEADROOM_BYTES - 512;

    let created = fixture
        .provider
        .create_volume(&create_request("free-derive", MIB))
        .await
        .expect("first create");

    // The allocated image consumed the pool's reported max_avail (the
    // fake derives it from the live image map, like real ceph df), so a
    // second equal create no longer fits with headroom.
    let err = fixture
        .provider
        .create_volume(&create_request("free-derive-2", MIB))
        .await
        .expect_err("free space must reflect the allocated image");
    assert_eq!(err.code, ApiErrorCode::NoSafeCapacity, "{err}");

    // Deleting (Retain → trash) returns the space.
    fixture
        .provider
        .delete_volume(
            &created.volume_id,
            &fixture_delete_request("free-derive", created.generation),
        )
        .await
        .expect("delete returns the space");
    fixture
        .provider
        .create_volume(&create_request("free-derive-2", MIB))
        .await
        .expect("create succeeds after the space was returned");
}

#[tokio::test]
async fn create_failure_cleans_up_and_persists_nothing() {
    let fixture = fixture();
    // rbd create "succeeds" but the image never shows up: the create must
    // fail honestly and remove the half-created image best-effort.
    fixture.world.lock().expect("world").create_silent = true;
    let err = fixture
        .provider
        .create_volume(&create_request("silent-vol", MIB))
        .await
        .expect_err("unverifiable create");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");

    // Nothing was persisted and no image remains.
    let listed = fixture.provider.list_volumes(None).await.expect("list");
    assert_eq!(listed, []);
    assert!(fixture.world.lock().expect("world").images.is_empty());

    // The best-effort cleanup really ran as an rbd rm invocation (the
    // subcommand follows the fixed -m/--name argv prefix).
    assert!(
        fixture.runner.invocations().into_iter().any(|invocation| {
            invocation.program == "rbd" && invocation.args.get(4).map(String::as_str) == Some("rm")
        }),
        "the create failure must trigger a best-effort rbd rm"
    );
}

// ---------------------------------------------------------------------------
// Crash-window recovery on create
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_reclaims_our_half_created_image_after_a_crash_window() {
    let fixture = fixture();
    let id = volume_id("reclaim-vol");
    let image_name = image_name_for(&id);
    // Crash window: a previous create stamped the image (with our
    // ownership record) but died before the state save, so a retry hits
    // "rbd create: already exists".
    fixture.world.lock().expect("world").images.insert(
        image_name.clone(),
        common::FakeImage::owned(2 * MIB, "reclaim-vol"),
    );

    let created = fixture
        .provider
        .create_volume(&create_request("reclaim-vol", MIB))
        .await
        .expect("the orphaned image is reclaimed, not wedged forever");
    assert_eq!(created.state, VolumeLifecycle::Ready);
    // The actual (larger) size is adopted as the effective size.
    assert_eq!(created.provisioned_bytes, 2 * MIB);
    assert_eq!(created.allocated_bytes, 2 * MIB);

    // Exactly one image exists, still carrying the ownership record, and
    // the state entry matches the reclaimed image.
    let world = fixture.world.lock().expect("world");
    assert_eq!(
        world
            .images
            .get(&image_name)
            .expect("reclaimed image")
            .meta
            .get(common::OWNER_META_KEY),
        Some(&"reclaim-vol".to_owned())
    );
    let state = CephState::load(&fixture.state_path).expect("state");
    let stored = state.volume(&id).expect("state entry persisted");
    assert_eq!(stored.entry.size_bytes, 2 * MIB);
    assert_eq!(stored.entry.requested_size_bytes, MIB);
}

#[tokio::test]
async fn create_reclaim_restamps_a_missing_generation_record() {
    let fixture = fixture();
    let id = volume_id("regen-vol");
    let image_name = image_name_for(&id);
    // The crash closed before the generation record was stamped: only
    // the owner metadata proves the image is ours.
    let mut image = common::FakeImage::owned(MIB, "regen-vol");
    image.meta.remove(common::GENERATION_META_KEY);
    fixture
        .world
        .lock()
        .expect("world")
        .images
        .insert(image_name, image);

    fixture
        .provider
        .create_volume(&create_request("regen-vol", MIB))
        .await
        .expect("reclaim with a re-stamped generation record");
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .images
            .get(&image_name_for(&id))
            .expect("image")
            .meta
            .get(common::GENERATION_META_KEY),
        Some(&"1".to_owned()),
        "the missing generation record is re-stamped"
    );
}

#[tokio::test]
async fn create_never_adopts_an_existing_image_without_our_ownership_record() {
    let fixture = fixture();

    // A different owner's record: typed conflict, never adopted.
    let foreign_id = volume_id("clash-vol");
    let mut foreign = common::FakeImage::owned(MIB, "someone-elses-volume");
    foreign.meta.insert(
        common::OWNER_META_KEY.to_owned(),
        "someone-elses-volume".to_owned(),
    );
    fixture
        .world
        .lock()
        .expect("world")
        .images
        .insert(image_name_for(&foreign_id), foreign);
    let err = fixture
        .provider
        .create_volume(&create_request("clash-vol", MIB))
        .await
        .expect_err("an image owned by someone else is never adopted");
    assert_eq!(err.code, ApiErrorCode::ForeignDeviceState, "{err}");
    assert!(err.detail.contains("never adopted"), "{err}");

    // No metadata at all: equally foreign, equally refused.
    let bare_id = volume_id("bare-vol");
    fixture
        .world
        .lock()
        .expect("world")
        .images
        .insert(image_name_for(&bare_id), common::FakeImage::foreign(MIB));
    let err = fixture
        .provider
        .create_volume(&create_request("bare-vol", MIB))
        .await
        .expect_err("an image without ownership metadata is never adopted");
    assert_eq!(err.code, ApiErrorCode::ForeignDeviceState, "{err}");

    // Nothing was persisted and both images were left untouched.
    let listed = fixture.provider.list_volumes(None).await.expect("list");
    assert_eq!(listed, []);
    let world = fixture.world.lock().expect("world");
    assert!(world.images.contains_key(&image_name_for(&foreign_id)));
    assert!(world.images.contains_key(&image_name_for(&bare_id)));
    assert_eq!(world.trash, Vec::<String>::new());
}

#[tokio::test]
async fn create_reclaim_refuses_an_orphan_smaller_than_the_request() {
    let fixture = fixture();
    let id = volume_id("small-vol");
    // Our own orphan, but half a mebibyte: the request cannot be
    // satisfied by adopting it.
    fixture.world.lock().expect("world").images.insert(
        image_name_for(&id),
        common::FakeImage::owned(MIB / 2, "small-vol"),
    );
    let err = fixture
        .provider
        .create_volume(&create_request("small-vol", MIB))
        .await
        .expect_err("a smaller orphan is never adopted as-is");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("smaller than the request"), "{err}");
    // The orphan survives for investigation (never destroyed by us).
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .images
            .contains_key(&image_name_for(&id))
    );
    let listed = fixture.provider.list_volumes(None).await.expect("list");
    assert_eq!(listed, []);
}

// ---------------------------------------------------------------------------
// Grow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grow_is_grow_only_and_generation_fenced() {
    let fixture = fixture();
    let id = volume_id("grow-fence");
    let created = fixture
        .provider
        .create_volume(&create_request("grow-fence", GIB))
        .await
        .expect("create");

    // A stale expected generation is a typed conflict.
    let err = fixture
        .provider
        .grow_volume(&id, &fixture_grow_request("grow-fence", 2 * GIB, 99))
        .await
        .expect_err("stale generation");
    assert_eq!(err.code, ApiErrorCode::StaleGeneration);

    // Shrinking is refused fail-closed.
    let err = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request("grow-fence", GIB / 2, created.generation),
        )
        .await
        .expect_err("grow-only");
    assert_eq!(err.code, ApiErrorCode::UnsupportedClassOrPolicy);

    // State is unchanged.
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.generation, created.generation);
    assert_eq!(inspected.provisioned_bytes, GIB);
}

#[tokio::test]
async fn grow_verifies_the_effective_size_from_rbd_info() {
    let fixture = fixture();
    let id = volume_id("grow-verify");
    let created = fixture
        .provider
        .create_volume(&create_request("grow-verify", GIB))
        .await
        .expect("create");

    // rbd resize "succeeds" but rbd info still reports the old size.
    fixture.world.lock().expect("world").resize_silent = true;
    let err = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request("grow-verify", 2 * GIB, created.generation),
        )
        .await
        .expect_err("unverifiable grow");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("rbd info"), "{err}");

    // State is unchanged: size and generation stayed put.
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.generation, created.generation);
    assert_eq!(inspected.provisioned_bytes, GIB);
}

#[tokio::test]
async fn grow_of_a_detached_volume_has_nobody_to_notify() {
    let fixture = fixture();
    let id = volume_id("grow-detached");
    let created = fixture
        .provider
        .create_volume(&create_request("grow-detached", GIB))
        .await
        .expect("create");
    let grown = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request("grow-detached", 2 * GIB, created.generation),
        )
        .await
        .expect("grow detached");
    assert!(grown.backing_resized);
    assert_eq!(grown.effective_size_bytes, 2 * GIB);
    assert_eq!(
        grown.guest_notification_status,
        GrowGuestNotification::NotApplicable
    );

    // The effective size is persisted.
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.provisioned_bytes, 2 * GIB);
    assert_eq!(inspected.generation, created.generation + 1);
}

#[tokio::test]
async fn grow_capacity_check_is_typed() {
    let fixture = fixture();
    let id = volume_id("grow-cap");
    let created = fixture
        .provider
        .create_volume(&create_request("grow-cap", GIB))
        .await
        .expect("create");

    // Shrink the baseline so the grow delta (1 GiB) plus headroom no
    // longer fits into the derived max_avail (baseline - allocated).
    fixture.world.lock().expect("world").pool_max_avail = 2 * GIB + CEPH_HEADROOM_BYTES - 512;
    let err = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request("grow-cap", 2 * GIB, created.generation),
        )
        .await
        .expect_err("grow beyond safe capacity");
    assert_eq!(err.code, ApiErrorCode::NoSafeCapacity, "{err}");

    // State is unchanged: the image was never resized.
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.generation, created.generation);
    assert_eq!(inspected.provisioned_bytes, GIB);
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .images
            .get(&image_name_for(&id))
            .expect("image")
            .size,
        GIB
    );
}

#[tokio::test]
async fn an_unrecorded_grow_is_healed_not_wedged() {
    let fixture = fixture();
    let id = volume_id("heal-vol");
    fixture
        .provider
        .create_volume(&create_request("heal-vol", GIB))
        .await
        .expect("create");

    // Crash window after `rbd resize` but before the state save: the
    // image is 2 GiB while state still records 1 GiB. This must not be a
    // one-way door.
    fixture
        .world
        .lock()
        .expect("world")
        .images
        .get_mut(&image_name_for(&id))
        .expect("image")
        .size = 2 * GIB;

    // Inspect reflects the observed reality (the actual size), with the
    // persisted heal left to reconcile.
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.provisioned_bytes, 2 * GIB);
    assert_eq!(inspected.state, VolumeLifecycle::Ready);

    // Reconcile heals the recorded size and reports it.
    let report = fixture.provider.reconcile().expect("reconcile");
    assert_eq!(report.healed_grown, vec![id.clone()], "the heal is counted");
    let state = CephState::load(&fixture.state_path).expect("state");
    assert_eq!(
        state.volume(&id).expect("entry").entry.size_bytes,
        2 * GIB,
        "the recorded size is healed to the actual"
    );

    // A subsequent grow proceeds from the healed size and succeeds.
    let grown = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request("heal-vol", 3 * GIB, inspected.generation),
        )
        .await
        .expect("grow after the heal");
    assert!(grown.backing_resized);
    assert_eq!(grown.effective_size_bytes, 3 * GIB);
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .images
            .get(&image_name_for(&id))
            .expect("image")
            .size,
        3 * GIB
    );
}

#[tokio::test]
async fn grow_proceeds_from_the_actual_size_without_a_reconcile_first() {
    let fixture = fixture();
    let id = volume_id("heal-direct");
    let created = fixture
        .provider
        .create_volume(&create_request("heal-direct", GIB))
        .await
        .expect("create");

    // Same crash window, but the operator grows immediately: the grow
    // path itself must proceed from the image's actual size (2 GiB), so
    // only the 1 GiB delta is checked against capacity and resized.
    fixture
        .world
        .lock()
        .expect("world")
        .images
        .get_mut(&image_name_for(&id))
        .expect("image")
        .size = 2 * GIB;
    let grown = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request("heal-direct", 3 * GIB, created.generation),
        )
        .await
        .expect("grow from the actual size");
    assert_eq!(grown.effective_size_bytes, 3 * GIB);

    let state = CephState::load(&fixture.state_path).expect("state");
    assert_eq!(state.volume(&id).expect("entry").entry.size_bytes, 3 * GIB);
}

#[tokio::test]
async fn grow_to_an_already_met_target_reports_no_backing_resize() {
    let fixture = fixture();
    let id = volume_id("met-target");
    let created = fixture
        .provider
        .create_volume(&create_request("met-target", GIB))
        .await
        .expect("create");

    // Crash window after `rbd resize`: the image is already at the 2
    // GiB the operator will ask for, while state still records 1 GiB.
    fixture
        .world
        .lock()
        .expect("world")
        .images
        .get_mut(&image_name_for(&id))
        .expect("image")
        .size = 2 * GIB;

    // Growing to the already-met target runs NO resize, so the response
    // must not claim one (`backing_resized` is true only when a resize
    // actually ran).
    let grown = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request("met-target", 2 * GIB, created.generation),
        )
        .await
        .expect("grow to the already-met target");
    assert!(!grown.backing_resized, "no rbd resize ran: {grown:?}");
    assert_eq!(grown.effective_size_bytes, 2 * GIB);
    assert_eq!(
        grown.guest_notification_status,
        GrowGuestNotification::NotApplicable
    );

    // The record is still healed to the honest effective size.
    let state = CephState::load(&fixture.state_path).expect("state");
    let stored = state.volume(&id).expect("entry");
    assert_eq!(stored.entry.size_bytes, 2 * GIB);
    assert_eq!(stored.entry.generation, created.generation + 1);

    // A grow beyond the target still reports a real resize.
    let grown = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request("met-target", 3 * GIB, created.generation + 1),
        )
        .await
        .expect("grow beyond the target");
    assert!(grown.backing_resized);
    assert_eq!(grown.effective_size_bytes, 3 * GIB);
}

#[tokio::test]
async fn a_shrunk_image_is_failed_and_never_healed_downward() {
    let fixture = fixture();
    let id = volume_id("shrink-vol");
    fixture
        .provider
        .create_volume(&create_request("shrink-vol", GIB))
        .await
        .expect("create");

    // The image shrank outside volvisor: a violation, never healed.
    fixture
        .world
        .lock()
        .expect("world")
        .images
        .get_mut(&image_name_for(&id))
        .expect("image")
        .size = GIB / 2;

    // Inspect (before reconcile owns the transition) reports Failed
    // while carrying the RECORDED size: the recorded size is the last
    // volvisor-provisioned truth, and the shrunk actual is a violation
    // being surfaced — not a new size to adopt.
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.state, VolumeLifecycle::Failed);
    assert_eq!(inspected.provisioned_bytes, GIB);
    assert_eq!(inspected.allocated_bytes, GIB);

    let report = fixture.provider.reconcile().expect("reconcile");
    assert_eq!(report.shrunk_volumes, vec![id.clone()]);
    assert_eq!(report.healed_grown, [], "a shrink is never healed");

    let state = CephState::load(&fixture.state_path).expect("state");
    let stored = state.volume(&id).expect("entry kept");
    assert_eq!(stored.runtime.state, VolumeLifecycle::Failed);
    assert_eq!(
        stored.entry.size_bytes, GIB,
        "the recorded size is never healed downward"
    );

    // A grow of the Failed volume is refused.
    let err = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request("shrink-vol", 2 * GIB, stored.entry.generation),
        )
        .await
        .expect_err("grow requires a healthy volume");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
}

// ---------------------------------------------------------------------------
// Attach / detach
// ---------------------------------------------------------------------------

#[tokio::test]
async fn double_attach_is_rejected_single_writer() {
    let fixture = fixture();
    let id = volume_id("double-att");
    fixture
        .provider
        .create_volume(&create_request("double-att", MIB))
        .await
        .expect("create");
    let first = fixture
        .provider
        .attach_volume(
            &id,
            &fixture_attach_request("double-att", "double-att-1", 1),
        )
        .await
        .expect("first attach");

    // A second writer is fenced out.
    let err = fixture
        .provider
        .attach_volume(
            &id,
            &fixture_attach_request("double-att", "double-att-2", 2),
        )
        .await
        .expect_err("second writer must be rejected");
    assert_eq!(err.code, ApiErrorCode::WriterAlreadyActive);

    // The first attachment is untouched.
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.attachment_ids, vec![first.attachment_id.clone()]);
    assert_eq!(inspected.current_writer, Some(first.attachment_id));
}

#[tokio::test]
async fn attach_replay_is_idempotent_and_conflicts_are_typed() {
    let fixture = fixture();
    let id = volume_id("replay-att");
    let created = fixture
        .provider
        .create_volume(&create_request("replay-att", MIB))
        .await
        .expect("create");
    let request = fixture_attach_request("replay-att", "replay-att-1", 1);
    let first = fixture
        .provider
        .attach_volume(&id, &request)
        .await
        .expect("attach");

    // Crash replay with the original (now stale) generation replays the
    // recorded response and never fabricates a second attachment.
    let replayed = fixture
        .provider
        .attach_volume(&id, &request)
        .await
        .expect("idempotent attach replay");
    assert_eq!(replayed.attachment_id, first.attachment_id);
    assert_eq!(replayed.attachment_generation, first.attachment_generation);
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.attachment_ids.len(), 1);
    assert_eq!(inspected.generation, first.volume_generation);

    // The same attachment id for a different VM is a conflict.
    let mut conflicting = fixture_attach_request("replay-att", "replay-att-1", 1);
    conflicting.vm_id = "another-vm".to_owned();
    let err = fixture
        .provider
        .attach_volume(&id, &conflicting)
        .await
        .expect_err("attachment id reuse");
    assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);

    // A fresh attach with a stale volume generation is fenced.
    let err = fixture
        .provider
        .attach_volume(
            &id,
            &fixture_attach_request("replay-att", "replay-att-2", 99),
        )
        .await
        .expect_err("stale generation");
    assert_eq!(err.code, ApiErrorCode::StaleGeneration);

    // None of the rejections moved the create-time generation baseline.
    assert_eq!(created.generation, 1);
}

#[tokio::test]
async fn attach_replay_survives_a_provider_restart() {
    let fixture = fixture();
    let id = volume_id("restart-att");
    fixture
        .provider
        .create_volume(&create_request("restart-att", MIB))
        .await
        .expect("create");
    let request = fixture_attach_request("restart-att", "restart-att-1", 1);
    let first = fixture
        .provider
        .attach_volume(&id, &request)
        .await
        .expect("attach");

    // A fresh provider over the same state file and cluster must not
    // fabricate a second attachment when the request is replayed.
    let restarted = provider_from(&fixture.state_path, &fixture.world);
    let replayed = restarted
        .attach_volume(&id, &request)
        .await
        .expect("replay across restart");
    assert_eq!(replayed.attachment_id, first.attachment_id);

    let inspected = restarted.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.attachment_ids.len(), 1);

    // Detach with the recorded generation still works after the restart.
    restarted
        .detach_volume(
            &id,
            &first.attachment_id,
            &fixture_detach_request("restart-att-1", first.attachment_generation),
        )
        .await
        .expect("detach after restart");
}

#[tokio::test]
async fn attach_with_a_stale_pre_existing_mapping_fails_closed() {
    let fixture = fixture();
    let id = volume_id("stale-map");
    fixture
        .provider
        .create_volume(&create_request("stale-map", MIB))
        .await
        .expect("create");

    // A crash leftover mapping exists on this host (a possible foreign
    // writer): attach must refuse, never adopt and never auto-unmap.
    let device = seed_mapping(&fixture.world, &image_name_for(&id));
    let err = fixture
        .provider
        .attach_volume(
            &id,
            &fixture_attach_request("stale-map", "stale-map-att", 1),
        )
        .await
        .expect_err("stale pre-existing mapping must fail closed");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(err.detail.contains("stale mapping"), "{err}");

    // No attachment was recorded and the mapping was left in place.
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.attachment_ids, []);
    assert_eq!(inspected.state, VolumeLifecycle::Ready);
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .mappings
            .get(&device)
            .map(String::as_str),
        Some(image_name_for(&id).as_str())
    );
}

#[tokio::test]
async fn map_failure_records_no_attachment() {
    let fixture = fixture();
    let id = volume_id("map-fail");
    fixture
        .provider
        .create_volume(&create_request("map-fail", MIB))
        .await
        .expect("create");

    fixture.world.lock().expect("world").fail_map = true;
    let err = fixture
        .provider
        .attach_volume(&id, &fixture_attach_request("map-fail", "map-fail-att", 1))
        .await
        .expect_err("rbd map failure must surface");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("rbd map"), "{err}");

    // No attachment was recorded and no mapping exists.
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.attachment_ids, []);
    assert_eq!(inspected.state, VolumeLifecycle::Ready);
    assert_eq!(inspected.generation, 1);
    assert!(fixture.world.lock().expect("world").mappings.is_empty());
}

#[tokio::test]
async fn read_only_attach_is_a_typed_rejection_before_any_mutation() {
    let fixture = fixture();
    let id = volume_id("ro-att");
    let created = fixture
        .provider
        .create_volume(&create_request("ro-att", MIB))
        .await
        .expect("create");

    // ReadOnly (shared-reader) is not a contract this prototype has
    // qualified, and the only mapping it could make is writable and
    // exclusive-lock-owning: typed rejection BEFORE any mutation.
    let mut read_only = fixture_attach_request("ro-att", "ro-att-1", created.generation);
    read_only.access_mode = AccessModeRequest::ReadOnly;
    let err = fixture
        .provider
        .attach_volume(&id, &read_only)
        .await
        .expect_err("read-only attach is not qualified");
    assert_eq!(err.code, ApiErrorCode::UnsupportedClassOrPolicy, "{err}");
    assert!(err.detail.contains("read-only"), "{err}");
    assert!(err.detail.contains("multi-reader"), "{err}");

    // No mapping was created, no attachment recorded, no state moved.
    assert!(fixture.world.lock().expect("world").mappings.is_empty());
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.state, VolumeLifecycle::Ready);
    assert_eq!(inspected.attachment_ids, []);
    assert_eq!(inspected.current_writer, None);
    assert_eq!(inspected.generation, created.generation);

    // Read-write single-writer attach still works on the same volume.
    let attached = fixture
        .provider
        .attach_volume(
            &id,
            &fixture_attach_request("ro-att", "rw-att-1", created.generation),
        )
        .await
        .expect("read-write attach still works");
    assert_eq!(
        attached.frontend,
        Frontend::VirtioBlk {
            host_device_path: "/dev/rbd0".to_owned()
        }
    );
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.current_writer, Some(attached.attachment_id));
}

#[tokio::test]
async fn detach_with_a_wrong_generation_is_stale() {
    let fixture = fixture();
    let id = volume_id("detach-stale");
    fixture
        .provider
        .create_volume(&create_request("detach-stale", MIB))
        .await
        .expect("create");
    let attached = fixture
        .provider
        .attach_volume(
            &id,
            &fixture_attach_request("detach-stale", "detach-stale-att", 1),
        )
        .await
        .expect("attach");

    let err = fixture
        .provider
        .detach_volume(
            &id,
            &attached.attachment_id,
            &fixture_detach_request("detach-stale-att", 99),
        )
        .await
        .expect_err("stale attachment generation");
    assert_eq!(err.code, ApiErrorCode::StaleGeneration);

    // The attachment and the mapping both survive.
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.attachment_ids.len(), 1);
    assert!(!fixture.world.lock().expect("world").mappings.is_empty());
}

#[tokio::test]
async fn detach_of_an_already_unmapped_device_fails_loudly() {
    let fixture = fixture();
    let id = volume_id("detach-absent");
    fixture
        .provider
        .create_volume(&create_request("detach-absent", MIB))
        .await
        .expect("create");
    let attached = fixture
        .provider
        .attach_volume(
            &id,
            &fixture_attach_request("detach-absent", "detach-absent-att", 1),
        )
        .await
        .expect("attach");

    // The device vanished from showmapped behind the provider's back
    // without a state update: authority is never released on a guess.
    fixture
        .world
        .lock()
        .expect("world")
        .mappings
        .remove("/dev/rbd0");
    let err = fixture
        .provider
        .detach_volume(
            &id,
            &attached.attachment_id,
            &fixture_detach_request("detach-absent-att", attached.attachment_generation),
        )
        .await
        .expect_err("absent device must not detach quietly");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(err.detail.contains("showmapped"), "{err}");

    // The attachment record survives for investigation.
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    assert_eq!(inspected.attachment_ids.len(), 1);
}

#[tokio::test]
async fn failed_volume_cannot_attach_but_can_be_deleted() {
    let fixture = fixture();
    let id = volume_id("failed-vol");
    fixture
        .provider
        .create_volume(&create_request("failed-vol", MIB))
        .await
        .expect("create");

    // The image vanishes behind the provider's back, then a restart: the
    // constructor reconciles the volume to Failed.
    fixture
        .world
        .lock()
        .expect("world")
        .images
        .remove(&image_name_for(&id));
    let restarted = provider_from(&fixture.state_path, &fixture.world);
    let inspected = restarted
        .inspect_volume(&id)
        .await
        .expect("inspect after restart");
    assert_eq!(inspected.state, VolumeLifecycle::Failed);

    let err = restarted
        .attach_volume(
            &id,
            &fixture_attach_request("failed-vol", "failed-att", inspected.generation),
        )
        .await
        .expect_err("attach to a Failed volume");
    assert_eq!(err.code, ApiErrorCode::InvalidState);

    // Delete of a Failed volume whose image is absent succeeds (there is
    // nothing to trash) and does not pin the state forever.
    restarted
        .delete_volume(
            &id,
            &fixture_delete_request("failed-vol", inspected.generation),
        )
        .await
        .expect("delete failed volume");
    let err = restarted
        .inspect_volume(&id)
        .await
        .expect_err("volume is gone");
    assert_eq!(err.code, ApiErrorCode::NotFound);
}

// ---------------------------------------------------------------------------
// Delete / erasure policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_with_zero_discard_is_a_typed_rejection() {
    let fixture = fixture();
    let id = volume_id("discard-vol");
    let created = fixture
        .provider
        .create_volume(&create_request("discard-vol", MIB))
        .await
        .expect("create");

    // Ceph reclaim does not guarantee block zeroing: fail-closed policy
    // negotiation instead of a false erasure claim.
    let mut request = fixture_delete_request("discard-vol", created.generation);
    request.data_erasure_policy = ErasurePolicy::ZeroDiscard;
    let err = fixture
        .provider
        .delete_volume(&id, &request)
        .await
        .expect_err("zero-discard must be refused");
    assert_eq!(err.code, ApiErrorCode::UnsupportedClassOrPolicy);

    // The volume and its image both survive.
    fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect("persists");
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .images
            .contains_key(&image_name_for(&id))
    );
}

#[tokio::test]
async fn delete_with_an_attachment_is_invalid_state() {
    let fixture = fixture();
    let id = volume_id("del-attached");
    fixture
        .provider
        .create_volume(&create_request("del-attached", MIB))
        .await
        .expect("create");
    let attached = fixture
        .provider
        .attach_volume(
            &id,
            &fixture_attach_request("del-attached", "del-attached-att", 1),
        )
        .await
        .expect("attach");

    let err = fixture
        .provider
        .delete_volume(
            &id,
            &fixture_delete_request("del-attached", attached.volume_generation),
        )
        .await
        .expect_err("delete while attached");
    assert_eq!(err.code, ApiErrorCode::InvalidState);

    // The volume, its image and its mapping all survive.
    fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect("persists");
    let world = fixture.world.lock().expect("world");
    assert!(world.images.contains_key(&image_name_for(&id)));
    assert!(!world.mappings.is_empty());
}

#[tokio::test]
async fn delete_of_a_ready_volume_with_an_absent_image_fails_loudly() {
    let fixture = fixture();
    let id = volume_id("hidden-img");
    let created = fixture
        .provider
        .create_volume(&create_request("hidden-img", MIB))
        .await
        .expect("create");

    // The image disappears from rbd ls behind the provider's back
    // WITHOUT a restart: the stored state is still Ready. A delete must
    // not silently skip erasure on a (perhaps only transiently)
    // invisible image — it fails loudly instead.
    fixture
        .world
        .lock()
        .expect("world")
        .images
        .remove(&image_name_for(&id));
    let err = fixture
        .provider
        .delete_volume(
            &id,
            &fixture_delete_request("hidden-img", created.generation),
        )
        .await
        .expect_err("absent image on a Ready volume must not delete quietly");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("unexpectedly absent"), "{err}");
    assert!(err.detail.contains("Ready"), "{err}");

    // The volume entry survives for investigation.
    fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect("persists");
}

// ---------------------------------------------------------------------------
// Fail-closed startup verification
// ---------------------------------------------------------------------------

#[test]
fn constructor_refuses_a_foreign_cluster_and_writes_no_state() {
    let world = Arc::new(Mutex::new(FakeCeph::default()));
    world.lock().expect("world").fsid = "00000000-0000-0000-0000-00000000dead".to_owned();
    let err = try_new(&world, config()).expect_err("fsid mismatch must refuse startup");
    assert_eq!(err.code, ApiErrorCode::ForeignDeviceState);
    assert!(err.detail.contains("refusing to adopt"), "{err}");
}

#[test]
fn constructor_refuses_a_missing_pool() {
    let world = Arc::new(Mutex::new(FakeCeph::default()));
    let mut config = config();
    config.pool = "does-not-exist".to_owned();
    let err = try_new(&world, config).expect_err("missing pool must refuse startup");
    assert_eq!(err.code, ApiErrorCode::NotFound);
}

#[test]
fn constructor_refuses_an_unusable_cluster_query() {
    // The fsid query itself fails.
    let world = Arc::new(Mutex::new(FakeCeph::default()));
    world.lock().expect("world").fail_fsid = true;
    let err = try_new(&world, config()).expect_err("fsid query failure must refuse startup");
    assert_eq!(err.code, ApiErrorCode::CephClusterUnhealthy);

    // The health query fails.
    let world = Arc::new(Mutex::new(FakeCeph::default()));
    world.lock().expect("world").fail_health = true;
    let err = try_new(&world, config()).expect_err("health query failure must refuse startup");
    assert_eq!(err.code, ApiErrorCode::CephClusterUnhealthy);
}

#[test]
fn constructor_rejects_a_malformed_config_without_touching_the_cluster() {
    let world = Arc::new(Mutex::new(FakeCeph::default()));
    let mut config = config();
    config.mon_hosts = Vec::new();
    let err = try_new(&world, config).expect_err("malformed config");
    assert_eq!(err.code, ApiErrorCode::InvalidRequest);
    // No cluster command was ever executed.
    assert_eq!(FakeCeph::runner(&world).invocations(), []);
}

// ---------------------------------------------------------------------------
// Startup reconciliation
// ---------------------------------------------------------------------------

#[test]
fn startup_reconcile_marks_missing_images_failed() {
    let state_path = leak_tempdir().join("state.json");
    let world = Arc::new(Mutex::new(FakeCeph::default()));
    seed_volume(&state_path, &world, "ghost-vol", GIB);
    // The image vanished (a crash or an operator intervention).
    world
        .lock()
        .expect("world")
        .images
        .remove(&image_name_for(&volume_id("ghost-vol")));

    let provider = provider_from(&state_path, &world);
    let report = provider.reconcile().expect("reconcile report");
    assert_eq!(
        report.missing_volumes,
        vec![volume_id("ghost-vol")],
        "the volume without an image is reported missing"
    );

    // The volume was honestly marked Failed in persisted state.
    let state = CephState::load(&state_path).expect("state");
    let ghost = state.volume(&volume_id("ghost-vol")).expect("entry kept");
    assert_eq!(ghost.runtime.state, VolumeLifecycle::Failed);
    // The entry is kept for investigation, never dropped.
    assert!(!state.volumes().is_empty());
}

#[tokio::test]
async fn startup_reconcile_marks_mismatched_ownership_failed_and_delete_refuses() {
    let state_path = leak_tempdir().join("state.json");
    let world = Arc::new(Mutex::new(FakeCeph::default()));
    seed_volume(&state_path, &world, "mismatch-vol", GIB);
    // The image exists but its ownership record names someone else.
    world
        .lock()
        .expect("world")
        .images
        .get_mut(&image_name_for(&volume_id("mismatch-vol")))
        .expect("image")
        .meta
        .insert(
            common::OWNER_META_KEY.to_owned(),
            "someone-elses-volume".to_owned(),
        );

    let provider = provider_from(&state_path, &world);
    let report = provider.reconcile().expect("reconcile report");
    assert_eq!(
        report.mismatched_volumes,
        vec![volume_id("mismatch-vol")],
        "the volume whose image is not owned by it is reported mismatched"
    );
    let state = CephState::load(&state_path).expect("state");
    assert_eq!(
        state
            .volume(&volume_id("mismatch-vol"))
            .expect("entry")
            .runtime
            .state,
        VolumeLifecycle::Failed
    );

    // A delete of the mismatched volume refuses to destroy the image.
    let generation = state
        .volume(&volume_id("mismatch-vol"))
        .expect("entry")
        .entry
        .generation;
    let err = provider
        .delete_volume(
            &volume_id("mismatch-vol"),
            &fixture_delete_request("mismatch-vol", generation),
        )
        .await
        .expect_err("a foreign image is never destroyed");
    assert_eq!(err.code, ApiErrorCode::ForeignDeviceState);
    assert!(
        world
            .lock()
            .expect("world")
            .images
            .contains_key(&image_name_for(&volume_id("mismatch-vol")))
    );
}

#[test]
fn startup_reconcile_reports_foreign_and_untracked_images_without_touching_them() {
    let state_path = leak_tempdir().join("state.json");
    let world = Arc::new(Mutex::new(FakeCeph::default()));
    seed_volume(&state_path, &world, "owned-vol", GIB);
    // A foreign image (no volvisor metadata) and an owned image with no
    // state entry (a crash between rbd create and the state save).
    seed_foreign_image(&world, "foreign-image", MIB);
    world.lock().expect("world").images.insert(
        "vol-untracked-abcdef01".to_owned(),
        common::FakeImage::owned(MIB, "untracked-vol"),
    );

    let provider = provider_from(&state_path, &world);
    let report = provider.reconcile().expect("reconcile report");
    assert_eq!(report.foreign_images, vec!["foreign-image".to_owned()]);
    assert_eq!(
        report.untracked_owned_images,
        vec!["vol-untracked-abcdef01".to_owned()]
    );
    // The tracked volume is healthy: no false reports.
    assert_eq!(report.missing_volumes, []);
    assert_eq!(report.mismatched_volumes, []);
    assert_eq!(report.stale_mappings, []);

    // Both images were left untouched (never adopted, never destroyed).
    let world = world.lock().expect("world");
    assert!(world.images.contains_key("foreign-image"));
    assert!(world.images.contains_key("vol-untracked-abcdef01"));
    assert_eq!(world.trash, Vec::<String>::new());
}

#[tokio::test]
async fn startup_reconcile_never_unmaps_a_stale_mapping() {
    let state_path = leak_tempdir().join("state.json");
    let world = Arc::new(Mutex::new(FakeCeph::default()));
    seed_volume(&state_path, &world, "stale-vol", GIB);
    // The image is mapped on this host but no attachment record exists:
    // a stale mapping from a previous incarnation.
    let device = seed_mapping(&world, &image_name_for(&volume_id("stale-vol")));

    let provider = provider_from(&state_path, &world);
    let report = provider.reconcile().expect("reconcile report");
    assert_eq!(
        report.stale_mappings,
        vec![volume_id("stale-vol")],
        "the stale mapping is reported"
    );

    // The volume is marked Failed (visible, never silently served) and
    // the mapping is still in place: unmap stays a manual decision.
    let state = CephState::load(&state_path).expect("state");
    assert_eq!(
        state
            .volume(&volume_id("stale-vol"))
            .expect("entry")
            .runtime
            .state,
        VolumeLifecycle::Failed
    );
    assert_eq!(
        world
            .lock()
            .expect("world")
            .mappings
            .get(&device)
            .map(String::as_str),
        Some(image_name_for(&volume_id("stale-vol")).as_str())
    );

    // Attach to the Failed volume is refused.
    let err = provider
        .attach_volume(
            &volume_id("stale-vol"),
            &fixture_attach_request("stale-vol", "stale-att", 1),
        )
        .await
        .expect_err("attach to a Failed volume");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
}

#[tokio::test]
async fn startup_reconcile_clears_an_interrupted_detach() {
    let fixture = fixture();
    let id = volume_id("interrupted-detach");
    fixture
        .provider
        .create_volume(&create_request("interrupted-detach", MIB))
        .await
        .expect("create");
    fixture
        .provider
        .attach_volume(
            &id,
            &fixture_attach_request("interrupted-detach", "id-att", 1),
        )
        .await
        .expect("attach");

    // Crash window between a successful `rbd unmap` and the state save:
    // the mapping is gone but the attachment record remains.
    fixture
        .world
        .lock()
        .expect("world")
        .mappings
        .remove("/dev/rbd0");
    let restarted = provider_from(&fixture.state_path, &fixture.world);

    // Reconcile cleared the record (state matches observed reality) and
    // the volume is Ready for a fresh attach.
    let inspected = restarted
        .inspect_volume(&id)
        .await
        .expect("inspect after restart");
    assert_eq!(inspected.state, VolumeLifecycle::Ready);
    assert_eq!(inspected.attachment_ids, []);

    // The volume is attachable again on a fresh device.
    let reattached = restarted
        .attach_volume(
            &id,
            &fixture_attach_request("interrupted-detach", "id-att-2", inspected.generation),
        )
        .await
        .expect("re-attach after the cleared record");
    assert_eq!(
        reattached.frontend,
        Frontend::VirtioBlk {
            host_device_path: "/dev/rbd1".to_owned()
        }
    );
}

#[tokio::test]
async fn a_vanished_image_with_a_live_attachment_is_unwedged_by_reconcile() {
    let fixture = fixture();
    let id = volume_id("wedge-gone");
    let image_name = image_name_for(&id);
    fixture
        .provider
        .create_volume(&create_request("wedge-gone", MIB))
        .await
        .expect("create");
    fixture
        .provider
        .attach_volume(
            &id,
            &fixture_attach_request("wedge-gone", "wedge-gone-att", 1),
        )
        .await
        .expect("attach");

    // The image vanishes behind the provider's back while the
    // attachment record (and its device mapping) is still live.
    fixture
        .world
        .lock()
        .expect("world")
        .images
        .remove(&image_name);

    // Restart (the remedy the detach error message documents): reconcile
    // marks the volume Failed and drops the stale attachment record —
    // the backing it referenced no longer exists.
    let restarted = provider_from(&fixture.state_path, &fixture.world);
    // The STARTUP pass cleared the record; its audit trail is retained
    // on the provider (construction discards the return value).
    let startup = restarted
        .last_reconcile_report()
        .expect("report slot")
        .expect("the startup reconcile report is retained");
    assert_eq!(
        startup.missing_volumes,
        vec![id.clone()],
        "the volume without an image is reported missing by the startup pass"
    );

    // The cleared record is preserved in the report as the audit
    // trail: the device it named, and the live zombie mapping over the
    // gone backing (left for an operator, never auto-unmapped).
    assert_eq!(
        startup.cleared_attachments,
        vec![ClearedAttachment {
            volume_id: id.clone(),
            device: "/dev/rbd0".to_owned(),
            zombie_mapping: Some(true),
            reason: ClearReason::VanishedImage,
        }],
        "the cleared attachment record is reported with its device and \
         the zombie mapping over the vanished backing"
    );

    // A later explicit reconcile still reports the (still absent)
    // volume; the record itself is long gone, so nothing is cleared
    // twice.
    let report = restarted.reconcile().expect("reconcile report");
    assert_eq!(
        report.missing_volumes,
        vec![id.clone()],
        "the volume without an image is reported missing"
    );
    assert_eq!(
        report.cleared_attachments,
        [],
        "the record was cleared once, by the startup pass"
    );

    // Failed, detached, no writer — in the response AND in state.
    let inspected = restarted
        .inspect_volume(&id)
        .await
        .expect("inspect after restart");
    assert_eq!(inspected.state, VolumeLifecycle::Failed);
    assert_eq!(inspected.attachment_ids, []);
    assert_eq!(inspected.current_writer, None);
    let state = CephState::load(&fixture.state_path).expect("state");
    assert!(
        state
            .volume(&id)
            .expect("entry kept")
            .runtime
            .attachment
            .is_none(),
        "the stale attachment record is cleared and persisted"
    );

    // The zombie mapping itself is never auto-unmapped (destructive):
    // only the RECORD was dropped, the device is left to an operator.
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .mappings
            .get("/dev/rbd0")
            .map(String::as_str),
        Some(image_name.as_str())
    );

    // Delete now succeeds (Failed + detached) instead of refusing
    // forever on "must be fully detached".
    restarted
        .delete_volume(
            &id,
            &fixture_delete_request("wedge-gone", inspected.generation),
        )
        .await
        .expect("delete after the restart remedy");
    let err = restarted
        .inspect_volume(&id)
        .await
        .expect_err("volume is gone");
    assert_eq!(err.code, ApiErrorCode::NotFound);
}

#[tokio::test]
async fn a_mismatched_image_with_a_live_attachment_is_unwedged_by_reconcile() {
    let fixture = fixture();
    let id = volume_id("wedge-mismatch");
    let image_name = image_name_for(&id);
    fixture
        .provider
        .create_volume(&create_request("wedge-mismatch", MIB))
        .await
        .expect("create");
    fixture
        .provider
        .attach_volume(
            &id,
            &fixture_attach_request("wedge-mismatch", "wedge-mismatch-att", 1),
        )
        .await
        .expect("attach");

    // The image's ownership record is rewritten to name a foreign
    // owner: the backing is no longer provably ours.
    fixture
        .world
        .lock()
        .expect("world")
        .images
        .get_mut(&image_name)
        .expect("image")
        .meta
        .insert(
            common::OWNER_META_KEY.to_owned(),
            "someone-elses-volume".to_owned(),
        );

    // Restart: reconcile marks the volume Failed, clears the attachment
    // record (its authority claim is void) and never adopts the image.
    // The STARTUP pass cleared the record; its audit trail is retained
    // on the provider.
    let restarted = provider_from(&fixture.state_path, &fixture.world);
    let startup = restarted
        .last_reconcile_report()
        .expect("report slot")
        .expect("the startup reconcile report is retained");
    assert_eq!(
        startup.mismatched_volumes,
        vec![id.clone()],
        "the volume whose image is not owned by it is reported mismatched"
    );

    // The cleared record is preserved in the report as the audit
    // trail: the device it named, and the live mapping over the (still
    // existing) foreign-owned backing — a zombie from volvisor's point
    // of view, left for an operator.
    assert_eq!(
        startup.cleared_attachments,
        vec![ClearedAttachment {
            volume_id: id.clone(),
            device: "/dev/rbd0".to_owned(),
            zombie_mapping: Some(true),
            reason: ClearReason::OwnershipMismatch,
        }],
        "the cleared attachment record is reported with its device and \
         the live mapping over the no-longer-owned backing"
    );
    let inspected = restarted
        .inspect_volume(&id)
        .await
        .expect("inspect after restart");
    assert_eq!(inspected.state, VolumeLifecycle::Failed);
    assert_eq!(inspected.attachment_ids, []);
    assert_eq!(inspected.current_writer, None);
    let state = CephState::load(&fixture.state_path).expect("state");
    assert!(
        state
            .volume(&id)
            .expect("entry kept")
            .runtime
            .attachment
            .is_none(),
        "the stale attachment record is cleared and persisted"
    );

    // The image survives untouched under its foreign owner record.
    {
        let world = fixture.world.lock().expect("world");
        assert_eq!(
            world
                .images
                .get(&image_name)
                .expect("image kept")
                .meta
                .get(common::OWNER_META_KEY),
            Some(&"someone-elses-volume".to_owned())
        );
        assert_eq!(world.trash, Vec::<String>::new());
    }

    // The volume is no longer wedged on the stale attachment record:
    // delete now reaches the honest ownership refusal (a foreign image
    // is never destroyed, AGENTS rule 7) instead of "must be fully
    // detached".
    let err = restarted
        .delete_volume(
            &id,
            &fixture_delete_request("wedge-mismatch", inspected.generation),
        )
        .await
        .expect_err("a foreign image is never destroyed");
    assert_eq!(err.code, ApiErrorCode::ForeignDeviceState);
    assert!(err.detail.contains("never destroyed"), "{err}");
}

#[test]
fn reconcile_treats_a_transient_metadata_failure_as_an_honest_unknown() {
    let state_path = leak_tempdir().join("state.json");
    let world = Arc::new(Mutex::new(FakeCeph::default()));
    seed_volume(&state_path, &world, "transient-vol", GIB);
    // A transient metadata-read failure (mon timeout, NON-ENOENT): the
    // volume's lifecycle must stay untouched — never Failed from an
    // outage — and the unknown must be counted, not swallowed.
    world.lock().expect("world").fail_meta_get_transient = true;

    // The constructor's reconcile already ran with the failure present.
    let provider = provider_from(&state_path, &world);
    let state = CephState::load(&state_path).expect("state");
    assert_eq!(
        state
            .volume(&volume_id("transient-vol"))
            .expect("entry kept")
            .runtime
            .state,
        VolumeLifecycle::Ready,
        "a transient failure must not flip the lifecycle"
    );

    let report = provider.reconcile().expect("reconcile report");
    assert_eq!(report.unverifiable_volumes.len(), 1, "{report:?}");
    assert_eq!(
        report.unverifiable_volumes[0].volume_id,
        volume_id("transient-vol")
    );
    assert!(
        report.unverifiable_volumes[0]
            .detail
            .contains("rbd image-meta get"),
        "the error is summarized: {report:?}"
    );
    assert_eq!(report.mismatched_volumes, [], "no mismatch was verified");
    assert_eq!(report.missing_volumes, [], "the image still exists");
    assert_eq!(report.healed_grown, []);

    // Once the outage passes, the very same volume verifies clean.
    world.lock().expect("world").fail_meta_get_transient = false;
    let report = provider.reconcile().expect("reconcile report");
    assert_eq!(report.unverifiable_volumes, []);
    assert_eq!(report.mismatched_volumes, []);
    let state = CephState::load(&state_path).expect("state");
    assert_eq!(
        state
            .volume(&volume_id("transient-vol"))
            .expect("entry")
            .runtime
            .state,
        VolumeLifecycle::Ready
    );
}

#[test]
fn reconcile_marks_a_volume_failed_when_ownership_metadata_is_absent() {
    let state_path = leak_tempdir().join("state.json");
    let world = Arc::new(Mutex::new(FakeCeph::default()));
    seed_volume(&state_path, &world, "no-meta-vol", GIB);
    // Genuinely absent ownership metadata (an ENOENT-class read): a
    // verified mismatch, Failed — this is NOT a transient failure.
    world
        .lock()
        .expect("world")
        .images
        .get_mut(&image_name_for(&volume_id("no-meta-vol")))
        .expect("image")
        .meta
        .remove(common::OWNER_META_KEY);

    let provider = provider_from(&state_path, &world);
    let report = provider.reconcile().expect("reconcile report");
    assert_eq!(
        report.mismatched_volumes,
        vec![volume_id("no-meta-vol")],
        "key absence is a verified mismatch"
    );
    assert_eq!(report.unverifiable_volumes, []);
    let state = CephState::load(&state_path).expect("state");
    assert_eq!(
        state
            .volume(&volume_id("no-meta-vol"))
            .expect("entry")
            .runtime
            .state,
        VolumeLifecycle::Failed
    );
}

#[test]
fn reconcile_does_not_classify_an_unverifiable_image_as_foreign() {
    let state_path = leak_tempdir().join("state.json");
    let world = Arc::new(Mutex::new(FakeCeph::default()));
    // An image with no state entry whose ownership cannot be read: it
    // must be counted as unverifiable, not labeled foreign.
    seed_foreign_image(&world, "unreadable-image", MIB);
    world.lock().expect("world").fail_meta_get_transient = true;

    let provider = provider_from(&state_path, &world);
    let report = provider.reconcile().expect("reconcile report");
    assert_eq!(
        report.foreign_images,
        Vec::<String>::new(),
        "an unknown is not a foreign fact"
    );
    assert_eq!(report.untracked_owned_images, Vec::<String>::new());
    assert_eq!(report.unverifiable_images.len(), 1);
    assert_eq!(report.unverifiable_images[0].image_name, "unreadable-image");

    // The image was left untouched.
    assert!(
        world
            .lock()
            .expect("world")
            .images
            .contains_key("unreadable-image")
    );
}

// ---------------------------------------------------------------------------
// Read-only pool discovery
// ---------------------------------------------------------------------------

#[test]
fn discover_pools_reports_the_configured_pool_honestly() {
    let fixture = fixture();
    let pools = fixture.provider.discover_pools().expect("discover pools");
    assert_eq!(pools.len(), 1);
    let pool = &pools[0];
    assert!(pool.id.as_str().starts_with(&format!("ceph-{FSID}")));
    assert_eq!(pool.backend_class, VolumeClass::CephRbd);
    assert_eq!(pool.device_ids, []);
    assert_eq!(pool.host_or_ceph_cluster, FSID);
    assert_eq!(pool.health, volvisor_types::domain::Health::Healthy);
    // Capacity from ceph df: nothing allocated in the fresh fixture.
    assert_eq!(pool.capacity_bytes, POOL_MAX_AVAIL);
    assert_eq!(
        pool.allocatable_bytes,
        POOL_MAX_AVAIL - CEPH_HEADROOM_BYTES,
        "headroom is withheld from allocatable capacity"
    );
    // Never a local mirror beneath Ceph; replication is reported from the
    // pool's own policy query (`ceph osd pool get <pool> size`, default
    // size 3 in the fake) — never inferred from capacity output.
    assert!(!pool.protection.local_mirror);
    assert!(pool.protection.remote_replication);

    // A size-1 pool keeps no remote copy: not established.
    {
        let mut world = fixture.world.lock().expect("world");
        world.pool_size = 1;
        world.pool_min_size = 1;
    }
    let pools = fixture.provider.discover_pools().expect("discover pools");
    assert!(!pools[0].protection.remote_replication);

    // A failing policy query is a typed error, never a guessed fact.
    fixture.world.lock().expect("world").fail_pool_get = true;
    let err = fixture
        .provider
        .discover_pools()
        .expect_err("policy query failure must surface");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("ceph osd pool get"), "{err}");

    // A failing health query degrades to Unknown, never a fabricated fact.
    {
        let mut world = fixture.world.lock().expect("world");
        world.fail_pool_get = false;
        world.fail_health = true;
    }
    let pools = fixture.provider.discover_pools().expect("discover pools");
    assert_eq!(pools[0].health, volvisor_types::domain::Health::Unknown);
}

#[tokio::test]
async fn inspect_does_not_claim_an_unproven_remote_protection_axis() {
    let fixture = fixture();
    let id = volume_id("protect-vol");
    fixture
        .provider
        .create_volume(&create_request("protect-vol", MIB))
        .await
        .expect("create");
    let inspected = fixture.provider.inspect_volume(&id).await.expect("inspect");
    // The per-volume inspect path makes no policy queries (it must stay
    // cheap), so the remote axis is honestly NOT established — never
    // asserted from a heuristic. The authoritative replication facts
    // live on the pool discovery surface (see the discover_pools test).
    assert_eq!(
        inspected.effective_protection.remote,
        volvisor_types::domain::RemoteProtectionAxis::None
    );
    assert_eq!(
        inspected.effective_protection.local,
        volvisor_types::domain::LocalProtectionAxis::None
    );
}

// ---------------------------------------------------------------------------
// Durable state and the argv contract
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn state_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = fixture();
    fixture
        .provider
        .create_volume(&create_request("perm-vol", MIB))
        .await
        .expect("create");
    let mode = std::fs::metadata(&fixture.state_path)
        .expect("state file exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "state file must be owner-only");
}

#[tokio::test]
async fn every_invocation_carries_the_monitor_and_user_flags() {
    let fixture = fixture();
    let id = volume_id("argv-vol");
    fixture
        .provider
        .create_volume(&create_request("argv-vol", MIB))
        .await
        .expect("create");
    let attached = fixture
        .provider
        .attach_volume(&id, &fixture_attach_request("argv-vol", "argv-att", 1))
        .await
        .expect("attach");
    fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request("argv-vol", 2 * MIB, attached.volume_generation),
        )
        .await
        .expect("grow");
    fixture
        .provider
        .detach_volume(
            &id,
            &attached.attachment_id,
            &fixture_detach_request("argv-att", attached.attachment_generation),
        )
        .await
        .expect("detach");
    fixture
        .provider
        .delete_volume(&id, &fixture_delete_request("argv-vol", 4))
        .await
        .expect("delete");

    let mons = MON_HOSTS.join(",");
    let invocations = fixture.runner.invocations();
    assert!(
        invocations.len() >= 10,
        "the lifecycle must have exercised the CLIs"
    );
    for invocation in &invocations {
        assert!(
            invocation.program == "ceph" || invocation.program == "rbd",
            "unexpected program {:?}",
            invocation.program
        );
        assert_eq!(
            invocation.args.first().map(String::as_str),
            Some("-m"),
            "{:?}: every invocation starts with -m",
            invocation.program
        );
        assert_eq!(
            invocation.args.get(1).map(String::as_str),
            Some(mons.as_str())
        );
        // The user is the FULL entity name and must travel via --name:
        // --id takes a bare id and would double-prefix into the
        // nonexistent client.client.volvisor.
        assert_eq!(invocation.args.get(2).map(String::as_str), Some("--name"));
        assert_eq!(invocation.args.get(3).map(String::as_str), Some(USER));
    }
}

#[tokio::test]
async fn volumes_with_legacy_colliding_ids_coexist() {
    let fixture = fixture();
    // Under plain sanitization both ids map to the image-name segment
    // `collide-a`; the fake (like real rbd) refuses duplicate image
    // names, so this only succeeds because the hash suffix differs.
    let first = fixture
        .provider
        .create_volume(&create_request("collide.a", MIB))
        .await
        .expect("first create");
    let second = fixture
        .provider
        .create_volume(&create_request("collide:a", MIB))
        .await
        .expect("second create with a legacy-colliding id");
    assert_ne!(first.volume_id, second.volume_id);
    let listed = fixture.provider.list_volumes(None).await.expect("list");
    assert_eq!(listed.len(), 2);
}
