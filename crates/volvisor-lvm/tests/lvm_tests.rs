//! LVM-specific provider tests beyond the generic conformance kit.
//!
//! Covers the semantics unique to the native-local backend: device
//! claiming under a destructive-authorization token, foreign-PV refusal,
//! honest size verification on create/grow, erasure-policy handling on
//! delete, startup reconciliation, and crash-replay attach idempotency.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::fs;

use common::{fixture, unclaimed_fixture};
use volvisor_lvm::provider::lv_name_for;
use volvisor_lvm::state::LvmState;
use volvisor_provider::VolumeProvider;
use volvisor_provider::conformance::{
    fixture_attach_request, fixture_create_request, fixture_delete_request, fixture_detach_request,
    fixture_grow_request, fixture_report_request,
};
use volvisor_types::request::ErasurePolicy;
use volvisor_types::{
    ApiErrorCode, AttachmentEvidence, AttachmentState, DeviceId, VolumeId, VolumeLifecycle,
};

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

fn volume_id(raw: &str) -> VolumeId {
    VolumeId::new(raw).expect("valid fixture volume id")
}

/// Make the next state save fail before its rename while leaving the
/// existing state file readable: a directory at the sibling `.tmp` path
/// makes `create_owner_only` fail with `EISDIR`.
fn block_next_state_save(state_path: &std::path::Path) -> std::path::PathBuf {
    let mut tmp_name = state_path.to_path_buf().into_os_string();
    tmp_name.push(".tmp");
    let tmp_path = std::path::PathBuf::from(tmp_name);
    fs::create_dir(&tmp_path).expect("blocking temp directory");
    tmp_path
}

// ---------------------------------------------------------------------------
// Device claiming / release
// ---------------------------------------------------------------------------

#[test]
fn claim_requires_the_destructive_authorization_token() {
    let fixture = unclaimed_fixture();
    let devices = fixture.provider.discover().expect("discover").devices;
    let device = devices.first().expect("a discovered disk");
    let err = fixture
        .provider
        .claim_device(&device.id, &common::claim_request("wrong-token"))
        .expect_err("claim with a bad token");
    assert_eq!(err.code, ApiErrorCode::UnsupportedClassOrPolicy);
    // The token value never appears in the error detail.
    assert!(!err.detail.contains("wrong-token"));
    assert!(!err.detail.contains(common::AUTH_TOKEN));

    // Nothing was mutated.
    let report = fixture.provider.reconcile_report().expect("report");
    assert!(report.missing_volumes.is_empty() && report.foreign_lvs.is_empty());
}

#[test]
fn claim_rejects_a_device_hosting_a_foreign_pv() {
    let fixture = unclaimed_fixture();
    let devices = fixture.provider.discover().expect("discover").devices;
    let device = devices.first().expect("a discovered disk").clone();

    // Foreign LVM state already sits on the discovered path.
    fixture
        .world
        .lock()
        .expect("world")
        .pvs
        .push("/dev/sda".to_owned());

    let err = fixture
        .provider
        .claim_device(&device.id, &common::claim_request(common::AUTH_TOKEN))
        .expect_err("claim over a foreign PV");
    assert_eq!(err.code, ApiErrorCode::ForeignDeviceState);
    assert!(!err.detail.contains("adopted-quietly"));
    // The detail points at interrupted-claim recovery.
    assert!(err.detail.contains("pvremove"), "{err}");

    // No pvcreate/vgcreate was ever attempted.
    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .pvs
            .iter()
            .any(|pv| pv.contains("by-id"))
    );
}

#[tokio::test]
async fn claim_creates_pool_and_release_requires_empty_vg() {
    let fixture = unclaimed_fixture();
    let devices = fixture.provider.discover().expect("discover").devices;
    let device = devices.first().expect("a discovered disk").clone();

    let pool = fixture
        .provider
        .claim_device(&device.id, &common::claim_request(common::AUTH_TOKEN))
        .expect("claim");
    assert!(pool.id.as_str().starts_with("pool-"));
    assert_eq!(
        pool.backend_class,
        volvisor_types::domain::VolumeClass::NativeLocal
    );
    assert_eq!(pool.device_ids, vec![device.id.clone()]);
    assert_eq!(pool.host_or_ceph_cluster, "local");
    assert_eq!(pool.health, volvisor_types::domain::Health::Unknown);
    assert!(
        pool.allocatable_bytes < pool.capacity_bytes,
        "headroom is withheld from allocatable capacity"
    );

    // The claim is durable: a restarted provider over the same state and
    // world sees the device and no missing volumes.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let report = restarted.reconcile_report().expect("report");
    assert!(report.missing_volumes.is_empty() && report.foreign_lvs.is_empty());

    // Re-claiming an already-claimed device is rejected.
    let err = fixture
        .provider
        .claim_device(&device.id, &common::claim_request(common::AUTH_TOKEN))
        .expect_err("double claim");
    assert_eq!(err.code, ApiErrorCode::InvalidState);

    // A volume in the VG blocks release.
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("claim-vol", GIB))
        .await
        .expect("create");
    let err = fixture
        .provider
        .release_device(&device.id, &common::release_request(common::AUTH_TOKEN))
        .expect_err("release with volumes present");
    assert_eq!(err.code, ApiErrorCode::InvalidState);

    // After deleting the volume, release succeeds with the right token.
    fixture
        .provider
        .delete_volume(
            &created.volume_id,
            &fixture_delete_request("claim-vol", created.generation),
        )
        .await
        .expect("delete");
    fixture
        .provider
        .release_device(&device.id, &common::release_request(common::AUTH_TOKEN))
        .expect("release");
}

#[test]
fn release_requires_the_destructive_authorization_token() {
    let fixture = fixture();
    let device = DeviceId::new(common::CLAIMED_DEVICE).expect("device id");
    let err = fixture
        .provider
        .release_device(&device, &common::release_request("wrong-token"))
        .expect_err("release with a bad token");
    assert_eq!(err.code, ApiErrorCode::UnsupportedClassOrPolicy);
    assert!(!err.detail.contains("wrong-token"));
    assert!(!err.detail.contains(common::AUTH_TOKEN));
}

#[test]
fn claim_of_an_undiscovered_device_is_not_found() {
    let fixture = unclaimed_fixture();
    let unknown = DeviceId::new("dev-ffffffffffffffffffffffffffffffff").expect("valid id shape");
    let err = fixture
        .provider
        .claim_device(&unknown, &common::claim_request(common::AUTH_TOKEN))
        .expect_err("claim of unknown device");
    assert_eq!(err.code, ApiErrorCode::NotFound);
}

// ---------------------------------------------------------------------------
// Volume lifecycle specifics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_without_a_claimed_pool_is_no_safe_capacity() {
    let fixture = unclaimed_fixture();
    let err = fixture
        .provider
        .create_volume(&fixture_create_request("nopool-vol", GIB))
        .await
        .expect_err("create without a pool");
    assert_eq!(err.code, ApiErrorCode::NoSafeCapacity);
}

#[tokio::test]
async fn create_beyond_free_space_is_no_safe_capacity() {
    let fixture = fixture();
    let err = fixture
        .provider
        .create_volume(&fixture_create_request("toobig-vol", common::POOL_BYTES))
        .await
        .expect_err("create beyond free space");
    assert_eq!(err.code, ApiErrorCode::NoSafeCapacity);
}

#[tokio::test]
async fn create_verifies_the_lv_against_lvs_and_fails_honestly() {
    let fixture = fixture();
    // lvcreate "succeeds" but the LV never shows up in lvs.
    fixture.world.lock().expect("world").lvcreate_silent = true;
    let err = fixture
        .provider
        .create_volume(&fixture_create_request("silent-vol", GIB))
        .await
        .expect_err("unverifiable create");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("lvs does not list"), "{err}");

    // Nothing was persisted.
    let listed = fixture.provider.list_volumes(None).await.expect("list");
    assert_eq!(listed, []);
}

#[tokio::test]
async fn create_save_failure_rolls_back_lv_and_memory_and_converges_across_restart() {
    let fixture = fixture();
    let id = volume_id("savefail-vol");
    let lv_name = lv_name_for(&id);
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name);

    // A durable volume that is NOT the one under test: the compensation
    // must check THIS volume's record, not merely "the state file has a
    // volume". A predicate that keeps the new volume whenever any volume
    // is durable would resurrect #58 on every real host.
    let unrelated = fixture
        .provider
        .create_volume(&fixture_create_request("savefail-unrelated", GIB))
        .await
        .expect("seed an unrelated durable volume");
    let unrelated_id = unrelated.volume_id.clone();
    let unrelated_lv = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&unrelated_id));
    let snapshot = fs::read(&fixture.state_path).expect("state snapshot");

    // The next save fails before its rename, but the existing state file
    // stays readable (so the carve-out probe sees the unrelated volume).
    let tmp_path = block_next_state_save(&fixture.state_path);

    let err = fixture
        .provider
        .create_volume(&fixture_create_request("savefail-vol", GIB))
        .await
        .expect_err("create whose state save fails");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    // The original save failure is surfaced, not replaced by a generic one.
    assert!(err.detail.contains("failed to create"), "{err}");

    // No 200-until-restart: the in-memory entry was rolled back too, and
    // the unrelated volume is untouched.
    let inspected = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect_err("the failed create must not be visible");
    assert_eq!(inspected.code, ApiErrorCode::NotFound);
    let listed = fixture.provider.list_volumes(None).await.expect("list");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].volume_id, unrelated_id);

    // The compensating lvremove actually ran, removed only the failed
    // LV, and left the unrelated volume alone.
    {
        let world = fixture.world.lock().expect("world");
        assert!(!world.lvs.contains_key(&lv_path));
        assert!(world.lvs.contains_key(&unrelated_lv), "{:?}", world.lvs);
    }
    let invocations = fixture.runner.invocations();
    let create_at = invocations
        .iter()
        .position(|invocation| {
            invocation.program == "lvcreate"
                && invocation.args.iter().any(|arg| arg.as_str() == lv_name)
        })
        .expect("the failed create's lvcreate ran");
    let remove_at = invocations
        .iter()
        .position(|invocation| {
            invocation.program == "lvremove" && invocation.args.iter().any(|arg| arg == &lv_path)
        })
        .expect("the compensation lvremove ran");
    assert!(remove_at > create_at, "lvremove must follow lvcreate");
    assert!(
        !invocations.iter().any(|invocation| {
            invocation.program == "lvremove"
                && invocation.args.iter().any(|arg| arg == &unrelated_lv)
        }),
        "the unrelated volume must never be compensated"
    );

    // The old durable file survived byte-for-byte: the failed volume is
    // absent, the unrelated volume and the claim are untouched.
    fs::remove_dir(&tmp_path).expect("unblock the next save");
    assert_eq!(
        fs::read(&fixture.state_path).expect("state after failure"),
        snapshot
    );
    let durable = LvmState::load(&fixture.state_path).expect("load durable state");
    assert!(durable.volume(&id).is_none());
    assert!(durable.volume(&unrelated_id).is_some());
    assert!(
        durable
            .device(&DeviceId::new(common::CLAIMED_DEVICE).expect("device id"))
            .is_some(),
        "the claim is untouched"
    );

    // Restart converges: the failed volume is absent, the unrelated one
    // survives, and there is no missing/foreign LV.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let inspected = restarted
        .inspect_volume(&id)
        .await
        .expect_err("the rolled-back create must not reappear");
    assert_eq!(inspected.code, ApiErrorCode::NotFound);
    let listed = restarted.list_volumes(None).await.expect("list");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].volume_id, unrelated_id);
    let report = restarted.reconcile_report().expect("report");
    assert!(report.missing_volumes.is_empty(), "{report:?}");
    assert!(report.foreign_lvs.is_empty(), "{report:?}");

    // The same create retries cleanly on the restarted provider: no
    // orphan name collision and no wedged state.
    let created = restarted
        .create_volume(&fixture_create_request("savefail-vol", GIB))
        .await
        .expect("retry after compensation");
    assert_eq!(created.volume_id, id);
    assert_eq!(created.state, VolumeLifecycle::Ready);
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path)
    );
    let durable = LvmState::load(&fixture.state_path).expect("state after retry");
    assert!(durable.volume(&id).is_some());
    assert!(durable.volume(&unrelated_id).is_some());
}

#[tokio::test]
async fn create_save_failure_with_a_failing_lvremove_surfaces_the_orphan() {
    let fixture = fixture();
    let id = volume_id("savefail-orphan");
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    fixture.world.lock().expect("world").fail_lvremove_for = vec![lv_path.clone()];
    let snapshot = fs::read(&fixture.state_path).expect("state snapshot");
    let tmp_path = block_next_state_save(&fixture.state_path);

    let err = fixture
        .provider
        .create_volume(&fixture_create_request("savefail-orphan", GIB))
        .await
        .expect_err("create whose save and cleanup fail");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    // The original save failure survives, and the cleanup failure is never
    // masked: the exact remediation and the operator-visible state are named.
    assert!(err.detail.contains("failed to create"), "{err}");
    assert!(err.detail.contains("manual lvremove"), "{err}");
    assert!(err.detail.contains(&lv_path), "{err}");
    assert!(err.detail.contains("absent from provider state"), "{err}");

    // Memory is rolled back even though the LV could not be removed.
    let inspected = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect_err("the failed create must not be visible");
    assert_eq!(inspected.code, ApiErrorCode::NotFound);
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path)
    );

    // The durable file is unchanged: no volume was recorded.
    fs::remove_dir(&tmp_path).expect("unblock the next save");
    assert_eq!(
        fs::read(&fixture.state_path).expect("state after failure"),
        snapshot
    );
    assert!(
        LvmState::load(&fixture.state_path)
            .expect("load durable state")
            .volume(&id)
            .is_none()
    );

    // A restart still does not adopt the orphan; the library's reconcile
    // report surfaces it as foreign and nothing is missing from state.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let inspected = restarted
        .inspect_volume(&id)
        .await
        .expect_err("the orphan is not adopted");
    assert_eq!(inspected.code, ApiErrorCode::NotFound);
    let report = restarted.reconcile_report().expect("report");
    assert!(report.foreign_lvs.contains(&lv_path), "{report:?}");
    assert!(report.missing_volumes.is_empty(), "{report:?}");
}

#[tokio::test]
async fn create_save_failure_after_rename_is_honored_not_compensated() {
    let fixture = fixture();
    let id = volume_id("saverename-vol");
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    // Seed the durable file with the volume while the provider's
    // in-memory state still predates it (the fixture was built first).
    // The seed's payload differs from this create's canonical payload;
    // that is deliberate: this test pins the branch decision (the record
    // for THIS volume exists), not the post-restart replay of a record the
    // failed save wrote. The real carve-out writes this create's own
    // record, which is exactly what the same-incarnation retry below sees.
    common::seed_volume(
        &fixture.state_path,
        "saverename-vol",
        common::CLAIMED_VG,
        GIB,
    );

    // Force the next save to fail before its rename while the durable
    // file stays present: a directory at the sibling temp path makes
    // create_owner_only fail with EISDIR.
    let tmp_path = block_next_state_save(&fixture.state_path);

    let invocations_before = fixture.runner.invocations().len();
    let err = fixture
        .provider
        .create_volume(&fixture_create_request("saverename-vol", GIB))
        .await
        .expect_err("create whose save fails after the durable record exists");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("nonetheless reached"), "{err}");

    // The durable file already contained the volume: keep the LV, do not
    // compensate, and keep serving the in-memory entry.
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path)
    );
    assert!(
        !fixture.runner.invocations()[invocations_before..]
            .iter()
            .any(|invocation| invocation.program == "lvremove"),
        "a committed record must not be compensated"
    );
    let inspected = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect("the in-memory entry is kept");
    assert_eq!(inspected.volume_id, id);

    // The error's retry instruction is true in this incarnation: the same
    // payload (with a fresh operation at the API layer) returns the kept
    // volume rather than creating a second one.
    let retried = fixture
        .provider
        .create_volume(&fixture_create_request("saverename-vol", GIB))
        .await
        .expect("same-payload retry returns the kept volume");
    assert_eq!(retried.volume_id, id);
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path)
    );

    fs::remove_dir(&tmp_path).expect("clean up the blocking temp directory");
}

#[tokio::test]
async fn create_save_failure_with_an_unreadable_durable_file_still_compensates() {
    let fixture = fixture();
    let id = volume_id("savefail-unreadable");
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    let snapshot = fs::read(&fixture.state_path).expect("state snapshot");

    // The durable file exists but cannot be re-read at compensation time.
    // The fail-closed direction treats that as "not committed": remove the
    // freshly created (empty) LV and the in-memory entry.
    fs::write(&fixture.state_path, b"{ not valid provider state").expect("corrupt the state file");
    let tmp_path = block_next_state_save(&fixture.state_path);

    let err = fixture
        .provider
        .create_volume(&fixture_create_request("savefail-unreadable", GIB))
        .await
        .expect_err("create whose state save fails and durable re-read is unreadable");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    // The save failure is the returned error; the failed re-read is only
    // the decision input and never replaces it.
    assert!(err.detail.contains("failed to create"), "{err}");

    let inspected = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect_err("the failed create must not be visible");
    assert_eq!(inspected.code, ApiErrorCode::NotFound);
    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path)
    );

    fs::remove_dir(&tmp_path).expect("unblock the next save");
    fs::write(&fixture.state_path, &snapshot).expect("restore the durable state");

    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let inspected = restarted
        .inspect_volume(&id)
        .await
        .expect_err("the rolled-back create must not reappear");
    assert_eq!(inspected.code, ApiErrorCode::NotFound);
    let report = restarted.reconcile_report().expect("report");
    assert!(report.missing_volumes.is_empty(), "{report:?}");
    assert!(report.foreign_lvs.is_empty(), "{report:?}");
}

#[tokio::test]
async fn grow_verifies_the_effective_size_from_lvm() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("grow-verify", GIB))
        .await
        .expect("create");

    // lvextend "succeeds" but lvs still reports the old size.
    fixture.world.lock().expect("world").lvextend_silent = true;
    let err = fixture
        .provider
        .grow_volume(
            &created.volume_id,
            &fixture_grow_request("grow-verify", 2 * GIB, created.generation),
        )
        .await
        .expect_err("unverifiable grow");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("LVM reports"), "{err}");

    // State is unchanged: size and generation stayed put.
    let inspected = fixture
        .provider
        .inspect_volume(&created.volume_id)
        .await
        .expect("inspect");
    assert_eq!(inspected.generation, created.generation);
    assert_eq!(inspected.provisioned_bytes, GIB);
}

#[tokio::test]
async fn grow_reports_retry_required_while_attached() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("grow-attached", GIB))
        .await
        .expect("create");
    let attached = fixture
        .provider
        .attach_volume(
            &created.volume_id,
            &fixture_attach_request("grow-attached", "grow-attached-att", 1),
        )
        .await
        .expect("attach");
    let grown = fixture
        .provider
        .grow_volume(
            &created.volume_id,
            &fixture_grow_request("grow-attached", 2 * GIB, attached.volume_generation),
        )
        .await
        .expect("grow while attached");
    assert!(grown.backing_resized);
    assert_eq!(grown.effective_size_bytes, 2 * GIB);
    // No VMM integration exists: the notification honestly stays pending.
    assert_eq!(
        grown.guest_notification_status,
        volvisor_types::request::GrowGuestNotification::RetryRequired
    );
}

#[tokio::test]
async fn delete_with_an_attachment_is_invalid_state() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("del-attached", GIB))
        .await
        .expect("create");
    let attached = fixture
        .provider
        .attach_volume(
            &created.volume_id,
            &fixture_attach_request("del-attached", "del-attached-att", 1),
        )
        .await
        .expect("attach");

    let err = fixture
        .provider
        .delete_volume(
            &created.volume_id,
            &fixture_delete_request("del-attached", attached.volume_generation),
        )
        .await
        .expect_err("delete while attached");
    assert_eq!(err.code, ApiErrorCode::InvalidState);

    // The volume and its LV both survive.
    fixture
        .provider
        .inspect_volume(&created.volume_id)
        .await
        .expect("volume survives");
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&format!(
                "{}/{}",
                common::CLAIMED_VG,
                lv_name_for(&volume_id("del-attached"))
            ))
    );
}

#[tokio::test]
async fn delete_with_zero_discard_stops_before_lvremove_on_failure() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("discard-vol", GIB))
        .await
        .expect("create");

    // blkdiscard fails; the provider must not fall through to lvremove
    // and pretend success.
    fixture.world.lock().expect("world").fail_blkdiscard = true;
    let mut request = fixture_delete_request("discard-vol", created.generation);
    request.data_erasure_policy = ErasurePolicy::ZeroDiscard;
    let err = fixture
        .provider
        .delete_volume(&created.volume_id, &request)
        .await
        .expect_err("failed discard must surface");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("blkdiscard"), "{err}");

    // The LV was NOT removed after the failed discard.
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&format!(
                "{}/{}",
                common::CLAIMED_VG,
                lv_name_for(&volume_id("discard-vol"))
            ))
    );
    fixture
        .provider
        .inspect_volume(&created.volume_id)
        .await
        .expect("volume persists");
}

#[tokio::test]
async fn delete_with_zero_discard_discards_then_removes() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("discard-ok", GIB))
        .await
        .expect("create");
    let mut request = fixture_delete_request("discard-ok", created.generation);
    request.data_erasure_policy = ErasurePolicy::ZeroDiscard;
    fixture
        .provider
        .delete_volume(&created.volume_id, &request)
        .await
        .expect("delete with discard");

    // The LV is gone from the simulated world.
    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&format!(
                "{}/{}",
                common::CLAIMED_VG,
                lv_name_for(&volume_id("discard-ok"))
            ))
    );
}

#[tokio::test]
async fn delete_with_cryptographic_erasure_fails_closed() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("crypto-vol", GIB))
        .await
        .expect("create");
    let mut request = fixture_delete_request("crypto-vol", created.generation);
    request.data_erasure_policy = ErasurePolicy::Cryptographic;
    let err = fixture
        .provider
        .delete_volume(&created.volume_id, &request)
        .await
        .expect_err("cryptographic erasure");
    assert_eq!(err.code, ApiErrorCode::UnsupportedClassOrPolicy);
    fixture
        .provider
        .inspect_volume(&created.volume_id)
        .await
        .expect("volume persists");
}

#[tokio::test]
async fn attach_replay_never_fabricates_a_second_attachment() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("replay-vol", GIB))
        .await
        .expect("create");
    let request = fixture_attach_request("replay-vol", "replay-att", 1);
    let first = fixture
        .provider
        .attach_volume(&created.volume_id, &request)
        .await
        .expect("attach");

    // Crash replay carries the original (now stale) expected generation.
    let replayed = fixture
        .provider
        .attach_volume(&created.volume_id, &request)
        .await
        .expect("idempotent attach replay");
    assert_eq!(replayed.attachment_id, first.attachment_id);
    assert_eq!(replayed.attachment_generation, first.attachment_generation);

    let inspected = fixture
        .provider
        .inspect_volume(&created.volume_id)
        .await
        .expect("inspect");
    assert_eq!(inspected.attachment_ids.len(), 1);
    assert_eq!(inspected.generation, first.volume_generation);

    // The same attachment id for a different VM is a conflict.
    let mut conflicting = fixture_attach_request("replay-vol", "replay-att", 1);
    conflicting.vm_id = "another-vm".to_owned();
    let err = fixture
        .provider
        .attach_volume(&created.volume_id, &conflicting)
        .await
        .expect_err("attachment id reuse");
    assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);
}

#[tokio::test]
async fn attach_replay_survives_a_provider_restart() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("restart-vol", GIB))
        .await
        .expect("create");
    let request = fixture_attach_request("restart-vol", "restart-att", 1);
    let first = fixture
        .provider
        .attach_volume(&created.volume_id, &request)
        .await
        .expect("attach");

    // A fresh provider over the same state file must not fabricate a
    // second attachment when the request is replayed.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let replayed = restarted
        .attach_volume(&created.volume_id, &request)
        .await
        .expect("replay across restart");
    assert_eq!(replayed.attachment_id, first.attachment_id);

    let inspected = restarted
        .inspect_volume(&created.volume_id)
        .await
        .expect("inspect");
    assert_eq!(inspected.attachment_ids.len(), 1);

    // Detach with the recorded generation still works after the restart.
    restarted
        .detach_volume(
            &created.volume_id,
            &first.attachment_id,
            &fixture_detach_request("restart-att", first.attachment_generation),
        )
        .await
        .expect("detach after restart");
}

#[tokio::test]
async fn attach_records_the_vmm_disk_id_and_reports_the_grow_facts() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("facts-vol", GIB))
        .await
        .expect("create");

    // A detached volume is absent from the enumeration.
    assert!(
        !fixture
            .provider
            .grow_attachment_facts()
            .expect("facts")
            .contains_key(&created.volume_id)
    );

    // An attachment with a VMM disk id reports addressable, with the
    // consumer's own id and the provider's current size.
    let mut request = fixture_attach_request("facts-vol", "facts-att", 1);
    request.vmm_disk_id = Some("disk-facts-vol".to_owned());
    fixture
        .provider
        .attach_volume(&created.volume_id, &request)
        .await
        .expect("attach with a disk id");
    let facts = fixture.provider.grow_attachment_facts().expect("facts");
    assert_eq!(
        facts.get(&created.volume_id),
        Some(&volvisor_provider::AttachmentForGrow::Addressable {
            vm_id: "conformance-vm".to_owned(),
            vmm_disk_id: "disk-facts-vol".to_owned(),
            current_size_bytes: GIB,
        })
    );

    // A grow moves the reported current size (the notification
    // target follows the backing).
    fixture
        .provider
        .grow_volume(
            &created.volume_id,
            &fixture_grow_request("facts-vol", 2 * GIB, 2),
        )
        .await
        .expect("grow");
    let facts = fixture.provider.grow_attachment_facts().expect("facts");
    assert_eq!(
        facts.get(&created.volume_id),
        Some(&volvisor_provider::AttachmentForGrow::Addressable {
            vm_id: "conformance-vm".to_owned(),
            vmm_disk_id: "disk-facts-vol".to_owned(),
            current_size_bytes: 2 * GIB,
        })
    );

    // An attachment WITHOUT a disk id reports unaddressable — never
    // absent (a frontend exists), never guessed.
    let mut bare = fixture_attach_request("facts-vol", "facts-att-2", 4);
    bare.vmm_disk_id = None;
    fixture
        .provider
        .detach_volume(
            &created.volume_id,
            &volvisor_types::AttachmentId::new("facts-att").expect("valid id"),
            &fixture_detach_request("facts-att", 1),
        )
        .await
        .expect("detach");
    fixture
        .provider
        .attach_volume(&created.volume_id, &bare)
        .await
        .expect("attach without a disk id");
    let facts = fixture.provider.grow_attachment_facts().expect("facts");
    assert_eq!(
        facts.get(&created.volume_id),
        Some(&volvisor_provider::AttachmentForGrow::Unaddressable)
    );
}

#[tokio::test]
async fn attach_replay_conflicts_on_a_different_vmm_disk_id() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("id-vol", GIB))
        .await
        .expect("create");
    let mut request = fixture_attach_request("id-vol", "id-att", 1);
    request.vmm_disk_id = Some("disk-one".to_owned());
    fixture
        .provider
        .attach_volume(&created.volume_id, &request)
        .await
        .expect("attach");

    // The same attachment id with a different VMM disk id is a
    // conflict — the recorded mapping is the durable truth.
    let mut conflicting = fixture_attach_request("id-vol", "id-att", 1);
    conflicting.vmm_disk_id = Some("disk-two".to_owned());
    let err = fixture
        .provider
        .attach_volume(&created.volume_id, &conflicting)
        .await
        .expect_err("disk id mismatch");
    assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);
}

#[test]
fn a_pre_p6b_attachment_record_loads_without_the_disk_id() {
    // Backward compatibility: state files written before P6-B carry
    // no `vmm_disk_id`; they load with the field absent (never
    // guessed), which the grow facts then report as unaddressable.
    let record: volvisor_lvm::state::AttachmentRecord = serde_json::from_str(
        r#"{
            "id": "att-old",
            "vm_id": "vm-old",
            "host_id": "host-old",
            "generation": 1,
            "access_mode": "single_writer"
        }"#,
    )
    .expect("a pre-P6-B record loads unchanged");
    assert_eq!(record.vmm_disk_id, None);
    // Pre-report state files also lack the evidence fields: they load as
    // the provider-recorded prepared state at revision 1, never as a
    // fabricated observation.
    assert_eq!(record.state, AttachmentState::Prepared);
    assert_eq!(record.state_revision, 1);
    assert_eq!(record.evidence, None);
    assert_eq!(record.reporter, None);
    assert_eq!(record.recorded_at_ms, None);
}

#[tokio::test]
async fn attachment_id_reuse_after_detach_is_refused() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("reuse-vol", GIB))
        .await
        .expect("create");
    let attach = fixture_attach_request("reuse-vol", "reuse-att", 1);
    let first = fixture
        .provider
        .attach_volume(&created.volume_id, &attach)
        .await
        .expect("attach");
    let detached = fixture
        .provider
        .detach_volume(
            &created.volume_id,
            &first.attachment_id,
            &fixture_detach_request("reuse-att", first.attachment_generation),
        )
        .await
        .expect("detach");

    // A report-capable provider refuses to reuse a consumed id: a delayed
    // report from the old instance must never label a new one.
    let reused = fixture_attach_request("reuse-vol", "reuse-att", detached.generation);
    let err = fixture
        .provider
        .attach_volume(&created.volume_id, &reused)
        .await
        .expect_err("consumed attachment id reuse");
    assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);

    // The tombstone is durable: a restart still refuses the reuse.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let err = restarted
        .attach_volume(&created.volume_id, &reused)
        .await
        .expect_err("consumed attachment id reuse after restart");
    assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);

    // A fresh id is admitted normally.
    let fresh = fixture_attach_request("reuse-vol", "reuse-att-2", detached.generation);
    restarted
        .attach_volume(&created.volume_id, &fresh)
        .await
        .expect("fresh attachment id");
}

#[tokio::test]
async fn report_state_survives_a_provider_restart_and_never_moves_the_generation() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("report-restart-vol", GIB))
        .await
        .expect("create");
    let attach = fixture_attach_request("report-restart-vol", "report-restart-att", 1);
    let first = fixture
        .provider
        .attach_volume(&created.volume_id, &attach)
        .await
        .expect("attach");
    let reported = fixture
        .provider
        .report_attachment_state(
            &created.volume_id,
            &fixture_report_request(
                "report-restart-att",
                first.attachment_generation,
                AttachmentState::Advertised,
                AttachmentEvidence::VmmConfigured,
            ),
        )
        .await
        .expect("report advertised");
    assert!(reported.applied);
    assert_eq!(reported.state_revision, 2);
    assert_eq!(reported.attachment_generation, first.attachment_generation);
    assert_eq!(reported.reporter.as_deref(), Some("conformance-vmm"));

    // The durable state file is the source of truth across a restart.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let inspected = restarted
        .inspect_volume(&created.volume_id)
        .await
        .expect("inspect after restart");
    let view = inspected
        .attachments
        .iter()
        .find(|view| view.attachment_id == first.attachment_id)
        .expect("the reported attachment view");
    assert_eq!(view.state, AttachmentState::Advertised);
    assert_eq!(view.state_revision, 2);
    assert_eq!(view.evidence, Some(AttachmentEvidence::VmmConfigured));
    assert_eq!(view.reporter.as_deref(), Some("conformance-vmm"));
    assert_eq!(view.recorded_at_ms, reported.recorded_at_ms);
    assert_eq!(view.attachment_generation, first.attachment_generation);

    // The revision continues from the persisted value, not from 1.
    let advanced = restarted
        .report_attachment_state(
            &created.volume_id,
            &fixture_report_request(
                "report-restart-att",
                first.attachment_generation,
                AttachmentState::Active,
                AttachmentEvidence::GuestConfirmed,
            ),
        )
        .await
        .expect("report active after restart");
    assert!(advanced.applied);
    assert_eq!(advanced.state_revision, 3);
    assert_eq!(advanced.evidence, Some(AttachmentEvidence::GuestConfirmed));

    // A provider-level attach replay answers the recorded evidence, not a
    // fresh `prepared` (the conformance fake does the same).
    let replayed = restarted
        .attach_volume(&created.volume_id, &attach)
        .await
        .expect("attach replay after a report");
    assert_eq!(replayed.state, AttachmentState::Active);

    // Reports never moved the generation: detach still fences on it.
    restarted
        .detach_volume(
            &created.volume_id,
            &first.attachment_id,
            &fixture_detach_request("report-restart-att", first.attachment_generation),
        )
        .await
        .expect("detach at the unchanged generation");
}

// ---------------------------------------------------------------------------
// Reconciliation
// ---------------------------------------------------------------------------

#[test]
fn startup_reconcile_marks_missing_lvs_failed_and_reports_foreign() {
    // State claims one volume, but the simulated LVM does not have its LV
    // and instead holds a foreign LV under our VG.
    let state_path = common::leak_tempdir().join("state.json");
    common::seed_claimed_state(&state_path);
    let world = std::sync::Arc::new(std::sync::Mutex::new(common::FakeLvm::with_claimed_pool()));
    world
        .lock()
        .expect("world")
        .lvs
        .insert(format!("{}/foreign-lv", common::CLAIMED_VG), GIB);

    // A provider start with a state volume whose LV is gone marks it
    // Failed (persisted) and reports the foreign LV without touching it.
    common::seed_volume(&state_path, "ghost-vol", common::CLAIMED_VG, GIB);
    let provider = common::provider_from(&state_path, &world);

    let report = provider.reconcile_report().expect("report");
    assert_eq!(
        report.missing_volumes,
        vec![volume_id("ghost-vol")],
        "the volume without an LV is reported missing"
    );
    assert_eq!(
        report.foreign_lvs,
        vec![format!("{}/foreign-lv", common::CLAIMED_VG)],
        "the unknown LV under our VG is reported foreign"
    );

    // The volume was honestly marked Failed in state.
    let state = volvisor_lvm::state::LvmState::load(&state_path).expect("state");
    let ghost = state.volume(&volume_id("ghost-vol")).expect("ghost entry");
    assert_eq!(ghost.runtime.state, VolumeLifecycle::Failed);

    // The foreign LV was left untouched.
    assert!(
        world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&format!("{}/foreign-lv", common::CLAIMED_VG))
    );
}

#[tokio::test]
async fn failed_volume_cannot_attach_but_can_be_deleted() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("failed-vol", GIB))
        .await
        .expect("create");

    // Make the LV vanish behind the provider's back, then restart: the
    // constructor reconciles the volume to Failed.
    fixture.world.lock().expect("world").lvs.remove(&format!(
        "{}/{}",
        common::CLAIMED_VG,
        lv_name_for(&volume_id("failed-vol"))
    ));
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let inspected = restarted
        .inspect_volume(&created.volume_id)
        .await
        .expect("inspect");
    assert_eq!(inspected.state, VolumeLifecycle::Failed);

    let err = restarted
        .attach_volume(
            &created.volume_id,
            &fixture_attach_request("failed-vol", "failed-att", inspected.generation),
        )
        .await
        .expect_err("attach to a Failed volume");
    assert_eq!(err.code, ApiErrorCode::InvalidState);

    // Delete of a Failed volume is allowed (its data is already gone).
    restarted
        .delete_volume(
            &created.volume_id,
            &fixture_delete_request("failed-vol", inspected.generation),
        )
        .await
        .expect("delete failed volume");
}

#[tokio::test]
async fn delete_of_a_failed_volume_without_an_lv_skips_lvm_entirely() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("ghost-del", GIB))
        .await
        .expect("create");

    // The LV vanishes; the restart reconciles the volume to Failed.
    fixture.world.lock().expect("world").lvs.remove(&format!(
        "{}/{}",
        common::CLAIMED_VG,
        lv_name_for(&volume_id("ghost-del"))
    ));
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let inspected = restarted
        .inspect_volume(&created.volume_id)
        .await
        .expect("inspect");
    assert_eq!(inspected.state, VolumeLifecycle::Failed);

    // blkdiscard is scripted to fail AND lvremove of a missing LV fails
    // in the realistic fake: the delete must skip both (there is nothing
    // to discard or remove) and still succeed.
    fixture.world.lock().expect("world").fail_blkdiscard = true;
    restarted
        .delete_volume(&created.volume_id, &{
            let mut request = fixture_delete_request("ghost-del", inspected.generation);
            request.data_erasure_policy = ErasurePolicy::ZeroDiscard;
            request
        })
        .await
        .expect("delete of a Failed volume whose LV is absent");

    let err = restarted
        .inspect_volume(&created.volume_id)
        .await
        .expect_err("volume is gone");
    assert_eq!(err.code, ApiErrorCode::NotFound);
}

#[tokio::test]
async fn delete_of_a_ready_volume_with_an_absent_lv_fails_loudly() {
    let fixture = fixture();
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("hidden-lv", GIB))
        .await
        .expect("create");

    // The LV disappears from `lvs` behind the provider's back WITHOUT a
    // restart: the stored state is still Ready. A delete must not
    // silently skip erasure on a (perhaps only transiently) invisible
    // LV — a ZeroDiscard delete would report success while a foreign LV
    // still holds live data — so it fails loudly instead.
    fixture.world.lock().expect("world").lvs.remove(&format!(
        "{}/{}",
        common::CLAIMED_VG,
        lv_name_for(&volume_id("hidden-lv"))
    ));
    let mut request = fixture_delete_request("hidden-lv", created.generation);
    request.data_erasure_policy = ErasurePolicy::ZeroDiscard;
    let err = fixture
        .provider
        .delete_volume(&created.volume_id, &request)
        .await
        .expect_err("absent LV on a Ready volume must not delete quietly");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("unexpectedly absent"), "{err}");
    assert!(err.detail.contains("Ready"), "{err}");

    // The volume entry survives for investigation.
    fixture
        .provider
        .inspect_volume(&created.volume_id)
        .await
        .expect("volume persists");
}

// ---------------------------------------------------------------------------
// Extent rounding (thick LVM rounds sizes up to whole physical extents)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_rounds_up_to_the_extent_and_reports_the_effective_size() {
    let fixture = fixture();
    // 1 MiB is not extent-aligned (the simulated extent is 4 MiB).
    let requested = MIB;
    let effective = 4 * MIB;

    let created = fixture
        .provider
        .create_volume(&fixture_create_request("unaligned-vol", requested))
        .await
        .expect("non-aligned create succeeds");
    assert_eq!(created.state, VolumeLifecycle::Ready);
    // The response reports the effective size honestly, not the request.
    assert_eq!(created.provisioned_bytes, effective);
    assert_eq!(created.allocated_bytes, effective);

    // The simulated LVM holds the extent-rounded LV.
    assert_eq!(
        fixture.world.lock().expect("world").lvs.get(&format!(
            "{}/{}",
            common::CLAIMED_VG,
            lv_name_for(&volume_id("unaligned-vol"))
        )),
        Some(&effective)
    );

    // State stores both the requested and the effective size.
    let state = LvmState::load(&fixture.state_path).expect("state");
    let entry = &state
        .volume(&volume_id("unaligned-vol"))
        .expect("stored volume")
        .entry;
    assert_eq!(entry.requested_size_bytes, requested);
    assert_eq!(entry.size_bytes, effective);

    // Idempotent replay of the same request returns the stored volume.
    let replayed = fixture
        .provider
        .create_volume(&fixture_create_request("unaligned-vol", requested))
        .await
        .expect("idempotent replay");
    assert_eq!(replayed.provisioned_bytes, effective);
    assert_eq!(replayed.generation, created.generation);

    // A different requested size for the same identity conflicts.
    let err = fixture
        .provider
        .create_volume(&fixture_create_request("unaligned-vol", 2 * MIB))
        .await
        .expect_err("conflicting replay");
    assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);
}

#[tokio::test]
async fn grow_rounds_up_to_the_extent_and_reports_the_effective_size() {
    let fixture = fixture();
    // Start at an extent-aligned size, then grow to a non-aligned one.
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("grow-unaligned", 4 * MIB))
        .await
        .expect("create");
    let grown = fixture
        .provider
        .grow_volume(
            &created.volume_id,
            &fixture_grow_request("grow-unaligned", 5 * MIB, created.generation),
        )
        .await
        .expect("non-aligned grow succeeds");
    assert!(grown.backing_resized);
    // 5 MiB rounds up to 8 MiB (two 4-MiB extents).
    assert_eq!(grown.effective_size_bytes, 8 * MIB);

    // The effective size is persisted and reported.
    let inspected = fixture
        .provider
        .inspect_volume(&created.volume_id)
        .await
        .expect("inspect");
    assert_eq!(inspected.provisioned_bytes, 8 * MIB);
    assert_eq!(inspected.allocated_bytes, 8 * MIB);
    let state = LvmState::load(&fixture.state_path).expect("state");
    let entry = &state
        .volume(&volume_id("grow-unaligned"))
        .expect("stored volume")
        .entry;
    assert_eq!(entry.size_bytes, 8 * MIB);
}

// ---------------------------------------------------------------------------
// Extent-rounded capacity checks (big arrays use big physical extents)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_capacity_check_is_extent_rounded() {
    let fixture = fixture();
    // 32-MiB physical extents (realistic on big arrays). 34 MiB free
    // satisfies the raw 20-MiB request plus the 4-MiB headroom, but not
    // the 32-MiB extent-rounded demand: the typed NO_SAFE_CAPACITY must
    // surface here, not a raw INTERNAL from LVM later.
    {
        let mut world = fixture.world.lock().expect("world");
        world.extent_size = 32 * MIB;
        world
            .vg_free
            .insert(common::CLAIMED_VG.to_owned(), 34 * MIB);
    }
    let err = fixture
        .provider
        .create_volume(&fixture_create_request("ext-round-create", 20 * MIB))
        .await
        .expect_err("raw-fits-but-rounded-does-not create");
    assert_eq!(err.code, ApiErrorCode::NoSafeCapacity, "{err}");

    // Nothing was created or persisted.
    let listed = fixture.provider.list_volumes(None).await.expect("list");
    assert_eq!(listed, []);
}

#[tokio::test]
async fn grow_capacity_check_is_extent_rounded() {
    let fixture = fixture();
    // Create at the default 4-MiB extent (aligned), then switch the
    // world to 32-MiB extents with free space that covers the raw grow
    // delta plus headroom but not the extent-rounded delta.
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("ext-round-grow", 4 * MIB))
        .await
        .expect("create");
    {
        let mut world = fixture.world.lock().expect("world");
        world.extent_size = 32 * MIB;
        world
            .vg_free
            .insert(common::CLAIMED_VG.to_owned(), 34 * MIB);
    }
    // 4 MiB -> 24 MiB: the raw delta is 20 MiB, the rounded delta 32 MiB.
    let err = fixture
        .provider
        .grow_volume(
            &created.volume_id,
            &fixture_grow_request("ext-round-grow", 24 * MIB, created.generation),
        )
        .await
        .expect_err("raw-fits-but-rounded-does-not grow");
    assert_eq!(err.code, ApiErrorCode::NoSafeCapacity, "{err}");

    // State is unchanged: size and generation stayed put.
    let inspected = fixture
        .provider
        .inspect_volume(&created.volume_id)
        .await
        .expect("inspect");
    assert_eq!(inspected.generation, created.generation);
    assert_eq!(inspected.provisioned_bytes, 4 * MIB);
}

// ---------------------------------------------------------------------------
// Injective LV naming
// ---------------------------------------------------------------------------

#[test]
fn lv_names_are_injective_across_the_id_charset() {
    // `vol.a`, `vol:a` and `vol-a` all sanitized to `vol-a` under the old
    // scheme; the hash suffix keeps them distinct.
    let names = [
        lv_name_for(&volume_id("vol.a")),
        lv_name_for(&volume_id("vol:a")),
        lv_name_for(&volume_id("vol-a")),
    ];
    assert_ne!(names[0], names[1]);
    assert_ne!(names[0], names[2]);
    assert_ne!(names[1], names[2]);
    for name in &names {
        assert!(name.starts_with("vol-"), "{name}: stable prefix");
        assert!(!name.starts_with('-'), "{name}: never dash-leading");
    }
    // Deterministic for the same identity.
    assert_eq!(names[0], lv_name_for(&volume_id("vol.a")));
}

#[tokio::test]
async fn volumes_with_legacy_colliding_ids_coexist() {
    let fixture = fixture();
    // Under the old sanitization both ids mapped to the LV name
    // `collide-a`; the realistic fake now refuses duplicate LV names, so
    // this only succeeds because the derived names differ.
    let first = fixture
        .provider
        .create_volume(&fixture_create_request("collide.a", MIB))
        .await
        .expect("first create");
    let second = fixture
        .provider
        .create_volume(&fixture_create_request("collide:a", MIB))
        .await
        .expect("second create with a legacy-colliding id");
    assert_ne!(first.volume_id, second.volume_id);
    let listed = fixture.provider.list_volumes(None).await.expect("list");
    assert_eq!(listed.len(), 2);
}

#[test]
fn lv_names_for_max_length_ids_fit_the_lvm_limit() {
    // A 128-byte volume id (the maximum) must still produce a name LVM
    // accepts: the sanitized segment is truncated to 100 characters, so
    // the full name is at most `vol-` + 100 + `-` + 8 hex = 113 chars
    // (LVM's own limit is ~127).
    let long_id = volume_id(&"v".repeat(128));
    let name = lv_name_for(&long_id);
    assert!(name.chars().count() <= 113, "{name} is too long");
    assert!(name.starts_with("vol-"));
    assert!(!name.starts_with('-'));

    // Two distinct 128-char ids sharing a 100-char sanitized prefix
    // still produce distinct names: uniqueness rests on the hash suffix
    // computed over the FULL volume id, not on the (truncated)
    // sanitized segment.
    let shared_prefix = "p".repeat(100);
    let first = lv_name_for(&volume_id(&format!("{shared_prefix}{}", "a".repeat(28))));
    let second = lv_name_for(&volume_id(&format!("{shared_prefix}{}", "b".repeat(28))));
    assert_ne!(first, second);
    // Their sanitized segments are identical after truncation; only the
    // hash suffix separates them.
    let (first_prefix, _) = first
        .rsplit_once('-')
        .expect("hash suffix is dash-delimited");
    let (second_prefix, _) = second
        .rsplit_once('-')
        .expect("hash suffix is dash-delimited");
    assert_eq!(
        first_prefix, second_prefix,
        "the sanitized segments are identical after truncation"
    );
}

// ---------------------------------------------------------------------------
// Claim/release crash windows
// ---------------------------------------------------------------------------

#[test]
fn claim_vgcreate_failure_undoes_the_pvcreate() {
    let fixture = unclaimed_fixture();
    let devices = fixture.provider.discover().expect("discover").devices;
    let device = devices.first().expect("a discovered disk").clone();
    fixture.world.lock().expect("world").fail_vgcreate = true;

    let err = fixture
        .provider
        .claim_device(&device.id, &common::claim_request(common::AUTH_TOKEN))
        .expect_err("claim with a failing vgcreate");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("vgcreate"), "{err}");
    // No manual remediation is demanded: the undo succeeded.
    assert!(!err.detail.contains("manual pvremove"), "{err}");

    // The pvcreate was undone: no PV remains on the device path (the
    // fixture's discovery resolves the disk to its kernel path).
    assert_eq!(
        fixture.world.lock().expect("world").pvs,
        Vec::<String>::new()
    );
    // The undo ran as a real pvremove invocation.
    assert!(
        fixture
            .runner
            .invocations()
            .into_iter()
            .any(|invocation| invocation.program == "pvremove"),
        "the claim failure must trigger a pvremove undo"
    );
    // State does not claim a device whose VG was not created.
    let state = LvmState::load(&fixture.state_path).expect("state");
    assert!(state.device(&device.id).is_none());
}

#[test]
fn claim_vgcreate_failure_with_a_failing_undo_names_the_remediation() {
    let fixture = unclaimed_fixture();
    let devices = fixture.provider.discover().expect("discover").devices;
    let device = devices.first().expect("a discovered disk").clone();
    let mut world = fixture.world.lock().expect("world");
    world.fail_vgcreate = true;
    world.fail_pvremove = true;
    drop(world);

    let err = fixture
        .provider
        .claim_device(&device.id, &common::claim_request(common::AUTH_TOKEN))
        .expect_err("claim with a failing vgcreate and undo");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("vgcreate"), "{err}");
    // The remediation hint names the exact manual pvremove required (the
    // fixture's discovery resolves the disk to its kernel path).
    assert!(err.detail.contains("manual pvremove"), "{err}");
    assert!(err.detail.contains("/dev/sda"), "{err}");

    // The leftover PV is still on the device (observed reality) and the
    // device is not claimed.
    assert_eq!(
        fixture.world.lock().expect("world").pvs,
        vec!["/dev/sda".to_owned()]
    );
    let state = LvmState::load(&fixture.state_path).expect("state");
    assert!(state.device(&device.id).is_none());
}

#[test]
fn release_pvremove_failure_keeps_the_claim_removed_with_a_remediation() {
    let fixture = fixture();
    let device = DeviceId::new(common::CLAIMED_DEVICE).expect("device id");
    fixture.world.lock().expect("world").fail_pvremove = true;

    let err = fixture
        .provider
        .release_device(&device, &common::release_request(common::AUTH_TOKEN))
        .expect_err("release with a failing pvremove");
    assert_eq!(err.code, ApiErrorCode::Internal);
    // The exact remediation is named.
    assert!(err.detail.contains("VG removed; manual pvremove"), "{err}");
    assert!(
        err.detail
            .contains("/dev/disk/by-id/wwn-0x5000c500fixt0001"),
        "{err}"
    );

    // The claim is NOT restored: the VG really is gone, so state matches
    // observed reality.
    let state = LvmState::load(&fixture.state_path).expect("state");
    assert!(state.device(&device).is_none());
    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .vg_free
            .contains_key(common::CLAIMED_VG)
    );
}

#[test]
fn release_vgremove_failure_with_the_vg_present_keeps_the_claim() {
    let fixture = fixture();
    let device = DeviceId::new(common::CLAIMED_DEVICE).expect("device id");
    fixture.world.lock().expect("world").fail_vgremove = true;

    let err = fixture
        .provider
        .release_device(&device, &common::release_request(common::AUTH_TOKEN))
        .expect_err("release with a failing vgremove and the VG present");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("vgremove"), "{err}");
    // The remediation names both steps in order: pvremove fails while
    // the VG still exists.
    assert!(
        err.detail.contains("manual remediation: vgremove") && err.detail.contains("then pvremove"),
        "{err}"
    );

    // The claim is kept (the VG still exists) and the VG is untouched:
    // state matches observed reality.
    let state = LvmState::load(&fixture.state_path).expect("state");
    assert!(state.device(&device).is_some());
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .vg_free
            .contains_key(common::CLAIMED_VG)
    );
}

#[test]
fn release_reconciles_forward_when_the_vg_is_verifiably_absent() {
    let fixture = fixture();
    let device = DeviceId::new(common::CLAIMED_DEVICE).expect("device id");

    // Crash window between vgremove success and the state save: the VG
    // (and its LVs) are gone, the PV is still on the device, and the
    // claim is still recorded. Real vgremove fails "Volume group not
    // found" forever, so a release that only trusted the vgremove exit
    // status could never free the claim.
    {
        let mut world = fixture.world.lock().expect("world");
        world.vg_free.remove(common::CLAIMED_VG);
        world.vg_size.remove(common::CLAIMED_VG);
        world
            .pvs
            .push("/dev/disk/by-id/wwn-0x5000c500fixt0001".to_owned());
    }

    fixture
        .provider
        .release_device(&device, &common::release_request(common::AUTH_TOKEN))
        .expect("release reconciles forward over the absent VG");

    // The failing vgremove was followed by an honest re-query of vgs
    // before the claim was dropped.
    let programs = fixture.runner.programs();
    let vgremove_at = programs
        .iter()
        .position(|program| program == "vgremove")
        .expect("vgremove ran");
    assert!(
        programs
            .iter()
            .skip(vgremove_at + 1)
            .any(|program| program == "vgs"),
        "the release must re-query vgs after a failed vgremove"
    );

    // The claim is gone and the leftover PV was cleaned up.
    let state = LvmState::load(&fixture.state_path).expect("state");
    assert!(state.device(&device).is_none());
    assert_eq!(
        fixture.world.lock().expect("world").pvs,
        Vec::<String>::new()
    );
}

#[test]
fn release_keeps_the_claim_when_the_vgs_requery_fails() {
    let fixture = fixture();
    let device = DeviceId::new(common::CLAIMED_DEVICE).expect("device id");
    // vgremove fails AND the honest re-query of vgs cannot run: the
    // absence of the VG is unknown, so the claim must stay and the
    // reported error is the vgremove failure (never a destructive
    // decision on missing data).
    {
        let mut world = fixture.world.lock().expect("world");
        world.fail_vgremove = true;
        world.fail_vgs = true;
    }

    let err = fixture
        .provider
        .release_device(&device, &common::release_request(common::AUTH_TOKEN))
        .expect_err("release with a failing vgremove and an unqueryable vgs");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("vgremove failed"), "{err}");

    let state = LvmState::load(&fixture.state_path).expect("state");
    assert!(state.device(&device).is_some());
}

// ---------------------------------------------------------------------------
// Startup device-claim reconciliation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn startup_reconcile_drops_a_stale_claim_whose_vg_is_gone() {
    // The VG vanished while the daemon was down (or the crash landed
    // between vgremove success and the state save): the claim is stale
    // and must be dropped so state matches observed reality.
    let state_path = common::leak_tempdir().join("state.json");
    common::seed_claimed_state(&state_path);
    // A volume that lived on the vanished VG: it must be marked Failed
    // (its LV is absent too) but never removed — the device-claim pass
    // must not touch volume entries.
    common::seed_volume(&state_path, "stale-claim-vol", common::CLAIMED_VG, GIB);
    let world = std::sync::Arc::new(std::sync::Mutex::new(common::FakeLvm::default()));

    let provider = common::provider_from(&state_path, &world);

    let state = LvmState::load(&state_path).expect("state");
    let device = DeviceId::new(common::CLAIMED_DEVICE).expect("device id");
    assert!(
        state.device(&device).is_none(),
        "a claim whose VG is verifiably absent is dropped"
    );
    let volume = state
        .volume(&volume_id("stale-claim-vol"))
        .expect("volume entry is kept");
    assert_eq!(volume.runtime.state, VolumeLifecycle::Failed);
    // The provider answers consistently from the reconciled state.
    let inspected = provider
        .inspect_volume(&volume_id("stale-claim-vol"))
        .await
        .expect("inspect");
    assert_eq!(inspected.state, VolumeLifecycle::Failed);
}

#[test]
fn startup_reconcile_keeps_claims_when_vgs_cannot_be_queried() {
    // An honest unknown (vgs fails) must never resolve into a
    // destructive decision: the claims are kept.
    let state_path = common::leak_tempdir().join("state.json");
    common::seed_claimed_state(&state_path);
    let world = std::sync::Arc::new(std::sync::Mutex::new(common::FakeLvm {
        fail_vgs: true,
        ..common::FakeLvm::default()
    }));

    let _provider = common::provider_from(&state_path, &world);

    let state = LvmState::load(&state_path).expect("state");
    let device = DeviceId::new(common::CLAIMED_DEVICE).expect("device id");
    assert!(
        state.device(&device).is_some(),
        "claims are kept when the vgs query fails"
    );
}

#[tokio::test]
async fn allocations_reduce_reported_free_and_delete_returns_it() {
    let fixture = fixture();
    {
        let mut world = fixture.world.lock().expect("world");
        world.vg_free.insert(common::CLAIMED_VG.to_owned(), 8 * MIB);
    }

    // The first create consumes its effective size from the VG's reported
    // free space (the fake derives free space from the live LV map, like
    // real LVM).
    let created = fixture
        .provider
        .create_volume(&fixture_create_request("free-derive", 4 * MIB))
        .await
        .expect("first create");

    // Baseline 8 MiB minus the allocated 4 MiB leaves 4 MiB: a second
    // 4-MiB volume needs 4 MiB plus the headroom, so the typed
    // NO_SAFE_CAPACITY must surface — proving the allocation was counted.
    let err = fixture
        .provider
        .create_volume(&fixture_create_request("free-derive-2", 4 * MIB))
        .await
        .expect_err("free space must reflect the allocated LV");
    assert_eq!(err.code, ApiErrorCode::NoSafeCapacity, "{err}");

    // Deleting the first volume returns its space.
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
        .create_volume(&fixture_create_request("free-derive-2", 4 * MIB))
        .await
        .expect("create succeeds after the space was returned");
}
