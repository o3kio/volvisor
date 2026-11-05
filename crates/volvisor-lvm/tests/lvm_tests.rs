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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use common::{fixture, unclaimed_fixture};
use volvisor_lvm::provider::{LvmProvider, lv_name_for};
use volvisor_lvm::state::LvmState;
use volvisor_lvm::{CommandRunner, FakeRunner};
use volvisor_provider::VolumeProvider;
use volvisor_provider::conformance::{
    fixture_attach_request, fixture_create_request, fixture_delete_request, fixture_detach_request,
    fixture_grow_request, fixture_report_request,
};
use volvisor_types::crash::{STORE_LVM_STATE, StoreSavePoint};
use volvisor_types::request::{DeleteVolumeRequest, ErasurePolicy, MoveVolumeBackingRequest};
use volvisor_types::{
    ApiErrorCode, AttachmentEvidence, AttachmentState, DeviceId, MoveVolumeBackingState,
    OperationId, VolumeId, VolumeLifecycle,
};

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

fn volume_id(raw: &str) -> VolumeId {
    VolumeId::new(raw).expect("valid fixture volume id")
}

/// A delete request with an explicit operation id and erasure policy
/// (the durable-intent tests need distinct retry identities and
/// `ZeroDiscard` for the strongest untouched-LV assertion).
fn delete_request_with(
    operation_id: &str,
    expected_generation: u64,
    policy: ErasurePolicy,
) -> DeleteVolumeRequest {
    DeleteVolumeRequest {
        api_version: volvisor_types::API_VERSION.to_owned(),
        operation_id: OperationId::new(operation_id).expect("valid fixture operation id"),
        expected_generation,
        data_erasure_policy: policy,
    }
}

/// A seed-only startup world: the claimed pool plus a state file with
/// the claim, ready for intent/volume seeding.
fn seeded_world() -> (std::path::PathBuf, Arc<Mutex<common::FakeLvm>>) {
    let state_path = common::leak_tempdir().join("state.json");
    common::seed_claimed_state(&state_path);
    let world = Arc::new(Mutex::new(common::FakeLvm::with_claimed_pool()));
    (state_path, world)
}

/// Construct a provider that must refuse startup, returning the typed
/// error (the `Ok` provider is not `Debug`, so `expect_err` cannot be
/// used directly; an unexpected success is surfaced as a sentinel error
/// the caller's assertions reject).
fn startup_error(
    state_path: &std::path::Path,
    world: &Arc<Mutex<common::FakeLvm>>,
) -> volvisor_types::ApiError {
    match common::try_provider_from(state_path, world) {
        Err(error) => error,
        Ok(_) => volvisor_types::ApiError::new(
            ApiErrorCode::Internal,
            "provider startup unexpectedly succeeded",
        ),
    }
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
#[allow(clippy::too_many_lines)] // one end-to-end failure/restart/retry narrative
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

    // The create's FIRST save (the durable intent) succeeds; the FINAL
    // save (volume record + intent removal) fails before its rename, so
    // the existing state file stays readable (the carve-out probe sees
    // the unrelated volume). `skip = 1` passes the intent save's consult.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterTmpWrite,
        1,
    );

    let err = fixture
        .provider
        .create_volume(&fixture_create_request("savefail-vol", GIB))
        .await
        .expect_err("create whose state save fails");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    // The original save failure is surfaced, not replaced by a generic one.
    assert!(err.detail.contains("injected state save failure"), "{err}");

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

    // The compensating clear-save restored the durable state
    // byte-for-byte: the failed volume and its intent are absent, the
    // unrelated volume and the claim are untouched.
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
    // Pass the durable-intent save; fail the final save before its
    // rename so the compensation path runs.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterTmpWrite,
        1,
    );

    let err = fixture
        .provider
        .create_volume(&fixture_create_request("savefail-orphan", GIB))
        .await
        .expect_err("create whose save and cleanup fail");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    // The original save failure survives, and the cleanup failure is never
    // masked: the exact remediation and the operator-visible state are named.
    assert!(err.detail.contains("injected state save failure"), "{err}");
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

    // The compensating clear-save restored the durable file: no volume
    // and no create intent were recorded.
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

    // The REAL post-rename window: the final save's rename lands, then
    // the directory-fsync boundary returns an injected error (#70's
    // non-kill seam). The durable file therefore contains this create's
    // own volume record when compensation classifies it. `skip = 1`
    // passes the preceding durable-intent save.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterRenameBeforeDirFsync,
        1,
    );

    let invocations_before = fixture.runner.invocations().len();
    let err = fixture
        .provider
        .create_volume(&fixture_create_request("saverename-vol", GIB))
        .await
        .expect_err("create whose final save fails after the rename");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("injected state save failure"), "{err}");
    assert!(err.detail.contains("AfterRenameBeforeDirFsync"), "{err}");
    assert!(
        err.detail.contains("durable state already contains"),
        "{err}"
    );

    // The rename landed, so the durable file contains the volume record
    // (and no create intent): keep the LV, do not compensate, and keep
    // serving the in-memory entry.
    let durable = LvmState::load(&fixture.state_path).expect("load durable state");
    assert!(durable.volume(&id).is_some(), "the rename committed");
    assert!(
        durable.create_intent(&id).is_none(),
        "the final save commits the record and drops the intent together"
    );
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
}

#[tokio::test]
async fn create_save_failure_with_an_unreadable_durable_file_still_compensates() {
    let fixture = fixture();
    let id = volume_id("savefail-unreadable");
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    let snapshot = fs::read(&fixture.state_path).expect("state snapshot");

    // The durable-intent save succeeds; `lvcreate` then leaves the
    // durable file unreadable and the test's persistent failure arm makes
    // the final save and the compensation's clear-save fail too (unique
    // temp names mean there is no fixed `.tmp` path to block). The
    // fail-closed re-read treats the unreadable durable state as "not
    // committed": remove the freshly created (empty) LV and the in-memory
    // entry, and leave the unreadable file for the operator.
    fixture
        .provider
        .store_crash_hooks()
        .arm_save_failure_until_cleared(STORE_LVM_STATE, StoreSavePoint::AfterTmpWrite, 1);
    fixture.world.lock().expect("world").break_state_on_lvcreate = Some(fixture.state_path.clone());

    let err = fixture
        .provider
        .create_volume(&fixture_create_request("savefail-unreadable", GIB))
        .await
        .expect_err("create whose state save fails and durable re-read is unreadable");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    // The save failure is the returned error; the failed re-read is only
    // the decision input and never replaces it. The failed clear-save is
    // reported as an appended note, not masked.
    assert!(err.detail.contains("injected state save failure"), "{err}");
    // The re-read failed, so the clear's outcome cannot be known: the
    // error says the intent is possibly durable (fail-closed), never
    // that it was definitely cleared or definitely remains.
    assert!(err.detail.contains("could not be verified"), "{err}");
    assert!(err.detail.contains("possibly durable"), "{err}");

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

    // Neither the failed save nor the compensation may rewrite or delete the
    // unreadable durable file: doing so would silently drop the device claim
    // and make the restart assertions below pass vacuously.
    assert_eq!(
        fs::read(&fixture.state_path).expect("durable file survived untouched"),
        b"{ not valid provider state".to_vec(),
        "the unreadable durable file must be left for the operator"
    );

    fixture.provider.store_crash_hooks().clear();
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
async fn startup_sweeps_only_this_state_paths_stale_temps() {
    // Mirrors the bounded temp namespace in `volvisor_lvm::state`: the
    // hash is stable FNV-1a 64 over the state path's identity bytes.
    fn fnv1a64(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3_u64);
        }
        hash
    }
    fn hashed_temp_path(
        state_path: &std::path::Path,
        pid: u64,
        sequence: u64,
    ) -> std::path::PathBuf {
        state_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."))
            .join(format!(
                ".volvisor-{:016x}.{pid}.{sequence}.tmp",
                state_path_hash(state_path)
            ))
    }
    fn state_path_hash(state_path: &std::path::Path) -> u64 {
        // Mirrors `state::canonical_state_identity` exactly, including the
        // `.` parent fallback; the production helper is not visible to an
        // integration test.
        let identity = match state_path.file_name() {
            Some(file_name) => {
                let parent = state_path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| std::path::Path::new("."));
                parent.canonicalize().map_or_else(
                    |_| state_path.to_path_buf(),
                    |parent| parent.join(file_name),
                )
            }
            None => state_path.to_path_buf(),
        };
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            fnv1a64(identity.as_os_str().as_bytes())
        }
        #[cfg(not(unix))]
        {
            fnv1a64(identity.to_string_lossy().as_bytes())
        }
    }

    let fixture = fixture();
    let parent = fixture.state_path.parent().expect("state parent");
    let stale = hashed_temp_path(&fixture.state_path, u64::from(std::process::id()), 1);
    fs::write(&stale, b"stale temp from a crashed save").expect("write stale temp");
    let mut legacy_name = fixture.state_path.clone().into_os_string();
    legacy_name.push(".tmp");
    let legacy = std::path::PathBuf::from(legacy_name);
    fs::write(&legacy, b"legacy fixed temp").expect("write legacy temp");
    // The legacy fixed name has no pid component, so the sweep treats it
    // like a foreign writer and only removes it after the one-hour grace.
    let old = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(2 * 60 * 60))
        .expect("test age fits the platform clock");
    fs::OpenOptions::new()
        .write(true)
        .open(&legacy)
        .expect("open legacy temp")
        .set_times(std::fs::FileTimes::new().set_modified(old))
        .expect("age the legacy temp");

    // Files that are not provably this state path's temp namespace.
    let foreign = parent.join("other-state.json.123.456.tmp");
    fs::write(&foreign, b"another state file's temp").expect("write foreign file");
    let foreign_hash = parent.join(format!(
        ".volvisor-{:016x}.123.456.tmp",
        state_path_hash(&fixture.state_path) ^ 1
    ));
    fs::write(&foreign_hash, b"another hash's temp").expect("write foreign hash file");
    let malformed = parent.join("state.json.notnumeric.tmp");
    fs::write(&malformed, b"not a provider temp").expect("write malformed file");

    let _restarted = common::provider_from(&fixture.state_path, &fixture.world);

    assert!(!stale.exists(), "the stale hashed temp is swept at startup");
    assert!(
        !legacy.exists(),
        "the legacy fixed temp is swept at startup"
    );
    assert!(foreign.exists(), "another state file's temp survives");
    assert!(foreign_hash.exists(), "another state path's hash survives");
    assert!(malformed.exists(), "a non-numeric middle survives");
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

/// A one-shot gate that holds `lvextend` inside the fake runner, so a
/// test can observe what other admissions do while a grow is in flight.
#[derive(Default)]
struct GrowGate {
    state: Mutex<(bool, bool)>,
    wake: Condvar,
}

impl GrowGate {
    fn entered(&self) {
        let mut state = self.state.lock().expect("gate lock");
        state.0 = true;
        self.wake.notify_all();
    }

    fn has_entered(&self) -> bool {
        self.state.lock().expect("gate lock").0
    }

    fn wait_release(&self) {
        let mut state = self.state.lock().expect("gate lock");
        while !state.1 {
            state = self.wake.wait(state).expect("gate wait");
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("gate lock");
        state.1 = true;
        self.wake.notify_all();
    }
}

/// Release the fake `lvextend` even when an assertion panics, so a failed
/// test cannot leave the grow task blocked at runtime shutdown.
struct GateRelease(Arc<GrowGate>);

impl Drop for GateRelease {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// A provider whose fake `lvextend` blocks on the returned gate until the
/// test releases it.
fn gated_provider() -> (Arc<LvmProvider>, Arc<GrowGate>) {
    let state_path = common::leak_tempdir().join("state.json");
    common::seed_claimed_state(&state_path);
    let world = Arc::new(Mutex::new(common::FakeLvm::with_claimed_pool()));
    let gate = Arc::new(GrowGate::default());
    let runner = {
        let world = Arc::clone(&world);
        let gate = Arc::clone(&gate);
        Arc::new(FakeRunner::with_closure(move |program, args| {
            if program == "lvextend" {
                gate.entered();
                gate.wait_release();
            }
            let mut world = world.lock().ok()?;
            common::script(&mut world, program, args)
        }))
    };
    let provider = Arc::new(
        LvmProvider::new(
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
            state_path,
            common::leak_tempdir(),
            common::VG_PREFIX.to_owned(),
            common::AUTH_TOKEN.to_owned(),
        )
        .expect("provider construction"),
    );
    (provider, gate)
}

/// A one-shot gate on the **first** armed invocation of one LVM program.
///
/// Regression tests for the capacity domain need to hold a grow inside its
/// `vgs` sample while letting a concurrent create's own `vgs` through. A
/// persistent gate (like [`GrowGate`]) would also stop the create, hiding
/// the property under test; this gate fires once and then stays open.
#[derive(Default)]
struct OneShotProgramGate {
    armed: AtomicBool,
    fired: AtomicBool,
    state: Mutex<(bool, bool)>,
    wake: Condvar,
}

impl OneShotProgramGate {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Record entry and return `true` exactly for the first armed call.
    fn try_fire(&self) -> bool {
        self.armed.load(Ordering::SeqCst) && !self.fired.swap(true, Ordering::SeqCst)
    }

    fn entered(&self) {
        let mut state = self.state.lock().expect("gate lock");
        state.0 = true;
        self.wake.notify_all();
    }

    fn has_entered(&self) -> bool {
        self.state.lock().expect("gate lock").0
    }

    fn wait_release(&self) {
        let mut state = self.state.lock().expect("gate lock");
        while !state.1 {
            state = self.wake.wait(state).expect("gate wait");
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("gate lock");
        state.1 = true;
        self.wake.notify_all();
    }
}

/// Release a one-shot gate even when an assertion panics, so a failed test
/// cannot leave a blocking LVM call parked at runtime shutdown.
struct OneShotGateRelease(Arc<OneShotProgramGate>);

impl Drop for OneShotGateRelease {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// A provider whose fake `vgs` can be held on demand (see
/// [`OneShotProgramGate`]), plus the runner for command-log assertions.
fn gated_capacity_provider() -> (Arc<LvmProvider>, Arc<OneShotProgramGate>, Arc<FakeRunner>) {
    gated_capacity_provider_inner(false)
}

/// Like [`gated_capacity_provider`], but the parked first `vgs` computes
/// its report *before* blocking. The caller's sample is therefore the
/// pre-park world, which is what the epoch tests need (a plain gate would
/// hand back a post-create snapshot and never exercise staleness).
fn gated_capacity_provider_snapshot() -> (Arc<LvmProvider>, Arc<OneShotProgramGate>, Arc<FakeRunner>)
{
    gated_capacity_provider_inner(true)
}

fn gated_capacity_provider_inner(
    snapshot_at_entry: bool,
) -> (Arc<LvmProvider>, Arc<OneShotProgramGate>, Arc<FakeRunner>) {
    let (provider, gate, runner, _world) = gated_capacity_provider_with_world(snapshot_at_entry);
    (provider, gate, runner)
}

/// Like [`gated_capacity_provider_snapshot`], but also returns the fake
/// LVM world so a test can inject a command failure while a grow parks.
fn gated_capacity_provider_snapshot_with_world() -> (
    Arc<LvmProvider>,
    Arc<OneShotProgramGate>,
    Arc<FakeRunner>,
    Arc<Mutex<common::FakeLvm>>,
) {
    gated_capacity_provider_with_world(true)
}

fn gated_capacity_provider_with_world(
    snapshot_at_entry: bool,
) -> (
    Arc<LvmProvider>,
    Arc<OneShotProgramGate>,
    Arc<FakeRunner>,
    Arc<Mutex<common::FakeLvm>>,
) {
    let state_path = common::leak_tempdir().join("state.json");
    common::seed_claimed_state(&state_path);
    let world = Arc::new(Mutex::new(common::FakeLvm::with_claimed_pool()));
    let gate = Arc::new(OneShotProgramGate::default());
    let runner = {
        let world = Arc::clone(&world);
        let gate = Arc::clone(&gate);
        Arc::new(FakeRunner::with_closure(move |program, args| {
            if program == "vgs" && gate.try_fire() {
                let output = if snapshot_at_entry {
                    let mut world = world.lock().ok()?;
                    common::script(&mut world, program, args)
                } else {
                    None
                };
                gate.entered();
                gate.wait_release();
                if snapshot_at_entry {
                    return output;
                }
            }
            let mut world = world.lock().ok()?;
            common::script(&mut world, program, args)
        }))
    };
    let provider = Arc::new(
        LvmProvider::new(
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
            state_path,
            common::leak_tempdir(),
            common::VG_PREFIX.to_owned(),
            common::AUTH_TOKEN.to_owned(),
        )
        .expect("provider construction"),
    );
    (provider, gate, runner, world)
}

/// A gate for `lvextend` that counts how many calls are inside at once.
///
/// Used to hold a grow inside its resize while a concurrent create samples
/// capacity and publishes its admission.
#[derive(Default)]
struct LvextendGate {
    state: Mutex<LvextendGateState>,
    wake: Condvar,
}

#[derive(Default)]
struct LvextendGateState {
    inside: usize,
    released: bool,
}

impl LvextendGate {
    fn enter_and_wait(&self) {
        let mut state = self.state.lock().expect("lvextend gate lock");
        state.inside += 1;
        self.wake.notify_all();
        while !state.released {
            state = self.wake.wait(state).expect("lvextend gate wait");
        }
        state.inside -= 1;
    }

    fn inside(&self) -> usize {
        self.state.lock().expect("lvextend gate lock").inside
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("lvextend gate lock");
        state.released = true;
        self.wake.notify_all();
    }
}

/// Release the lvextend gate even when an assertion panics, so a failed
/// test cannot leave blocking LVM calls parked at runtime shutdown.
struct LvextendGateRelease(Arc<LvextendGate>);

impl Drop for LvextendGateRelease {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// A provider whose `vgs` can be held on its first armed call (with the
/// report computed before parking) and whose `lvextend` parks at a
/// multi-entrant gate.
fn gated_capacity_and_extend_provider() -> (
    Arc<LvmProvider>,
    Arc<OneShotProgramGate>,
    Arc<LvextendGate>,
    Arc<FakeRunner>,
) {
    let state_path = common::leak_tempdir().join("state.json");
    common::seed_claimed_state(&state_path);
    let world = Arc::new(Mutex::new(common::FakeLvm::with_claimed_pool()));
    let vgs_gate = Arc::new(OneShotProgramGate::default());
    let extend_gate = Arc::new(LvextendGate::default());
    let runner = {
        let world = Arc::clone(&world);
        let vgs_gate = Arc::clone(&vgs_gate);
        let extend_gate = Arc::clone(&extend_gate);
        Arc::new(FakeRunner::with_closure(move |program, args| {
            if program == "vgs" && vgs_gate.try_fire() {
                let output = {
                    let mut world = world.lock().ok()?;
                    common::script(&mut world, program, args)
                };
                vgs_gate.entered();
                vgs_gate.wait_release();
                return output;
            }
            if program == "lvextend" {
                extend_gate.enter_and_wait();
            }
            let mut world = world.lock().ok()?;
            common::script(&mut world, program, args)
        }))
    };
    let provider = Arc::new(
        LvmProvider::new(
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
            state_path,
            common::leak_tempdir(),
            common::VG_PREFIX.to_owned(),
            common::AUTH_TOKEN.to_owned(),
        )
        .expect("provider construction"),
    );
    (provider, vgs_gate, extend_gate, runner)
}

/// Wait until the gated `vgs` has started.
async fn wait_for_vgs(gate: &Arc<OneShotProgramGate>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !gate.has_entered() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the grow's vgs sample must start"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Count recorded invocations of one LVM program.
fn count_program(runner: &Arc<FakeRunner>, program: &str) -> usize {
    runner
        .programs()
        .iter()
        .filter(|name| name.as_str() == program)
        .count()
}

/// Wait until the runner has recorded at least `expected` invocations of
/// one LVM program. Used to prove a concurrent call *started* while
/// another is parked (the invocation is recorded before the gate blocks).
async fn wait_for_program_count(runner: &Arc<FakeRunner>, program: &str, expected: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while count_program(runner, program) < expected {
        assert!(
            tokio::time::Instant::now() < deadline,
            "a concurrent {program} invocation must start while the first is parked"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Wait until the fake `lvextend` has started (and therefore the grow has
/// released the provider state lock, when the fix is present).
async fn wait_for_lvextend(gate: &Arc<GrowGate>) {
    // Poll from an async task, never from a blocking task: if the grow
    // fails before `lvextend` the polling loop simply times out instead
    // of leaving a `spawn_blocking` thread parked on the condvar forever
    // (which would hang the test binary at runtime shutdown).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !gate.has_entered() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "lvextend must start"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The timeout used for calls that must answer while a grow is in flight.
/// Pre-fix they block on the provider state lock; the calls run in their
/// own tasks so the timeout is driven by another runtime worker.
const IN_FLIGHT_TIMEOUT: Duration = Duration::from_secs(10);

/// A fake world with the move's two PVs, both contributing free space to
/// the claimed VG (the shape `move_fixture` builds).
fn gated_move_world() -> Arc<Mutex<common::FakeLvm>> {
    let world = Arc::new(Mutex::new(common::FakeLvm::with_claimed_pool()));
    {
        let mut world = world.lock().expect("world");
        world.add_pv_to_vg(
            common::MOVE_SOURCE_PV,
            common::CLAIMED_VG,
            common::POOL_BYTES,
        );
        world.add_pv_to_vg(
            common::MOVE_TARGET_PV,
            common::CLAIMED_VG,
            common::POOL_BYTES,
        );
        world
            .vg_free
            .insert(common::CLAIMED_VG.to_owned(), 2 * common::POOL_BYTES);
        world
            .vg_size
            .insert(common::CLAIMED_VG.to_owned(), 2 * common::POOL_BYTES);
    }
    world
}

/// Regression for the measured c32/c128 write-tail: a grow must release
/// the provider state lock across `lvextend`.
///
/// The fake `lvextend` blocks until this test releases it. While it is
/// blocked, an attach on a *different* volume must complete (the state
/// lock is not held) and structural mutations of the *growing* volume
/// must be refused typed (the durable intent is the recovery anchor and
/// its generation must not move under the resize).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn grow_releases_the_state_lock_across_lvextend() {
    let (provider, gate) = gated_provider();
    let _release = GateRelease(Arc::clone(&gate));

    let growing = provider
        .create_volume(&fixture_create_request("grow-lock", GIB))
        .await
        .expect("create growing volume");
    let other = provider
        .create_volume(&fixture_create_request("grow-other", GIB))
        .await
        .expect("create other volume");

    let grow = {
        let provider = Arc::clone(&provider);
        let volume_id = growing.volume_id.clone();
        let request = fixture_grow_request("grow-lock", 2 * GIB, growing.generation);
        tokio::spawn(async move { provider.grow_volume(&volume_id, &request).await })
    };
    wait_for_lvextend(&gate).await;

    // The growing volume's structural mutations are refused while the
    // intent is in flight, so the recovery anchor's generation cannot
    // move under `lvextend`.
    let refusal = {
        let provider = Arc::clone(&provider);
        let volume_id = growing.volume_id.clone();
        tokio::spawn(async move {
            provider
                .attach_volume(
                    &volume_id,
                    &fixture_attach_request("grow-lock", "grow-lock-att", 1),
                )
                .await
        })
    };
    let err = tokio::time::timeout(IN_FLIGHT_TIMEOUT, refusal)
        .await
        .expect("attach during a grow must not block on the state lock")
        .expect("attach task join")
        .expect_err("attach during a grow must be refused");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(err.detail.contains("grow in progress"), "{err}");

    let second_grow = {
        let provider = Arc::clone(&provider);
        let volume_id = growing.volume_id.clone();
        tokio::spawn(async move {
            provider
                .grow_volume(
                    &volume_id,
                    &fixture_grow_request("grow-lock-second", 3 * GIB, 1),
                )
                .await
        })
    };
    let err = tokio::time::timeout(IN_FLIGHT_TIMEOUT, second_grow)
        .await
        .expect("a second grow must not block on the state lock")
        .expect("grow task join")
        .expect_err("a second grow of the same volume must be refused");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(
        err.detail.contains("already has a grow in progress"),
        "{err}"
    );

    // A different volume's write must not queue behind this volume's
    // ~200 ms LVM CLI call: the state lock is released for the command.
    let attach_other = {
        let provider = Arc::clone(&provider);
        let volume_id = other.volume_id.clone();
        tokio::spawn(async move {
            provider
                .attach_volume(
                    &volume_id,
                    &fixture_attach_request("grow-other", "grow-other-att", 1),
                )
                .await
        })
    };
    let attached = tokio::time::timeout(IN_FLIGHT_TIMEOUT, attach_other)
        .await
        .expect("another volume's attach must not wait behind lvextend")
        .expect("attach task join")
        .expect("attach other volume");
    assert_eq!(attached.volume_generation, 2);

    gate.release();
    let grown = grow.await.expect("grow task").expect("grow succeeds");
    assert!(grown.backing_resized);
    assert_eq!(grown.effective_size_bytes, 2 * GIB);
    let inspected = provider
        .inspect_volume(&growing.volume_id)
        .await
        .expect("inspect grown volume");
    assert_eq!(inspected.provisioned_bytes, 2 * GIB);
    assert_eq!(inspected.generation, 2);
}

/// A report only advances the attachment record's own state; it does not
/// move the volume generation, so it must stay admitted while a grow of the
/// same volume is in flight (an idempotent report retry must not be turned
/// into a typed failure by the grow's recovery reservation).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn report_stays_admitted_during_a_grow() {
    let (provider, gate) = gated_provider();
    let _release = GateRelease(Arc::clone(&gate));

    let volume = provider
        .create_volume(&fixture_create_request("grow-report", GIB))
        .await
        .expect("create volume");
    let attached = provider
        .attach_volume(
            &volume.volume_id,
            &fixture_attach_request("grow-report", "grow-report-att", volume.generation),
        )
        .await
        .expect("attach");

    let grow = {
        let provider = Arc::clone(&provider);
        let volume_id = volume.volume_id.clone();
        let request = fixture_grow_request("grow-report", 2 * GIB, attached.volume_generation);
        tokio::spawn(async move { provider.grow_volume(&volume_id, &request).await })
    };
    wait_for_lvextend(&gate).await;

    // Detach bumps the volume generation and is refused.
    let detach = {
        let provider = Arc::clone(&provider);
        let volume_id = volume.volume_id.clone();
        let attachment_id = attached.attachment_id.clone();
        let request = fixture_detach_request("grow-report-att", attached.attachment_generation);
        tokio::spawn(async move {
            provider
                .detach_volume(&volume_id, &attachment_id, &request)
                .await
        })
    };
    let err = tokio::time::timeout(IN_FLIGHT_TIMEOUT, detach)
        .await
        .expect("detach during a grow must not block on the state lock")
        .expect("detach task join")
        .expect_err("detach during a grow must be refused");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(err.detail.contains("grow in progress"), "{err}");

    // The report is admitted and advances the attachment record.
    let report = {
        let provider = Arc::clone(&provider);
        let volume_id = volume.volume_id.clone();
        let request = fixture_report_request(
            "grow-report-att",
            attached.attachment_generation,
            AttachmentState::Active,
            AttachmentEvidence::GuestConfirmed,
        );
        tokio::spawn(async move { provider.report_attachment_state(&volume_id, &request).await })
    };
    let reported = tokio::time::timeout(IN_FLIGHT_TIMEOUT, report)
        .await
        .expect("a report must not wait behind lvextend")
        .expect("report task join")
        .expect("report during a grow");
    assert!(reported.applied);
    assert_eq!(reported.state, AttachmentState::Active);

    gate.release();
    let grown = grow.await.expect("grow task").expect("grow succeeds");
    assert_eq!(grown.effective_size_bytes, 2 * GIB);
    let inspected = provider
        .inspect_volume(&volume.volume_id)
        .await
        .expect("inspect grown volume");
    assert_eq!(inspected.generation, attached.volume_generation + 1);
}

/// The released-lock grow must still reserve its extents: a concurrent
/// create, or another volume's grow, that takes the state lock while
/// `lvextend` runs must not spend the same free space.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_grow_reserves_capacity_against_concurrent_admissions() {
    let (provider, gate) = gated_provider();
    let _release = GateRelease(Arc::clone(&gate));

    let first = provider
        .create_volume(&fixture_create_request("cap-first", GIB))
        .await
        .expect("create first volume");
    let second = provider
        .create_volume(&fixture_create_request("cap-second", GIB))
        .await
        .expect("create second volume");

    // 599 GiB demand against a 1 TiB pool: admissible alone, but not
    // twice. The grow is held inside `lvextend`, so its bytes are only
    // promised (not yet consumed).
    let grown_size = 600 * GIB;
    let grow = {
        let provider = Arc::clone(&provider);
        let volume_id = first.volume_id.clone();
        let request = fixture_grow_request("cap-first", grown_size, first.generation);
        tokio::spawn(async move { provider.grow_volume(&volume_id, &request).await })
    };
    wait_for_lvextend(&gate).await;

    let other_grow = {
        let provider = Arc::clone(&provider);
        let volume_id = second.volume_id.clone();
        let request = fixture_grow_request("cap-second", grown_size, second.generation);
        tokio::spawn(async move { provider.grow_volume(&volume_id, &request).await })
    };
    let err = tokio::time::timeout(IN_FLIGHT_TIMEOUT, other_grow)
        .await
        .expect("a second volume's grow must not block on the state lock")
        .expect("grow task join")
        .expect_err("the reserved bytes must not be promised twice");
    assert_eq!(err.code, ApiErrorCode::NoSafeCapacity);
    assert!(
        err.detail.contains("reserved for in-flight allocations"),
        "{err}"
    );

    let create = {
        let provider = Arc::clone(&provider);
        tokio::spawn(async move {
            provider
                .create_volume(&fixture_create_request("cap-create", grown_size))
                .await
        })
    };
    let err = tokio::time::timeout(IN_FLIGHT_TIMEOUT, create)
        .await
        .expect("a create must not block on the state lock")
        .expect("create task join")
        .expect_err("a create must not spend the reserved bytes");
    assert_eq!(err.code, ApiErrorCode::NoSafeCapacity);

    gate.release();
    let grown = grow.await.expect("grow task").expect("grow succeeds");
    assert_eq!(grown.effective_size_bytes, grown_size);
}

/// Regression for the lock-scope property implicated by (not proven by)
/// the residual c128 tail: the grow's `vgs` capacity sample must run
/// without the provider state lock.
///
/// The fake `vgs` is held on its first armed call. While it is blocked, an
/// attach on a *different* volume must complete (the state lock is free).
/// Pre-fix the sample ran inside `prepare_grow` under the state guard, so
/// the attach timed out behind the LVM CLI. The test proves lock scope
/// only; it says nothing about which cost produced the measured tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn grow_samples_capacity_without_holding_the_state_lock() {
    let (provider, gate, _runner) = gated_capacity_provider();
    let _release = OneShotGateRelease(Arc::clone(&gate));

    let growing = provider
        .create_volume(&fixture_create_request("cap-lock-grow", GIB))
        .await
        .expect("create growing volume");
    let other = provider
        .create_volume(&fixture_create_request("cap-lock-other", GIB))
        .await
        .expect("create other volume");

    gate.arm();
    let grow = {
        let provider = Arc::clone(&provider);
        let volume_id = growing.volume_id.clone();
        let request = fixture_grow_request("cap-lock-grow", 2 * GIB, growing.generation);
        tokio::spawn(async move { provider.grow_volume(&volume_id, &request).await })
    };
    wait_for_vgs(&gate).await;

    // A different volume's write must not queue behind the `vgs` sample.
    let attach_other = {
        let provider = Arc::clone(&provider);
        let volume_id = other.volume_id.clone();
        tokio::spawn(async move {
            provider
                .attach_volume(
                    &volume_id,
                    &fixture_attach_request("cap-lock-other", "cap-lock-other-att", 1),
                )
                .await
        })
    };
    let attached = tokio::time::timeout(IN_FLIGHT_TIMEOUT, attach_other)
        .await
        .expect("another volume's attach must not wait behind the grow's vgs")
        .expect("attach task join")
        .expect("attach other volume while the grow samples capacity");
    assert_eq!(attached.volume_generation, other.generation + 1);

    gate.release();
    let grown = grow.await.expect("grow task").expect("grow succeeds");
    assert!(grown.backing_resized);
    assert_eq!(grown.effective_size_bytes, 2 * GIB);
}

/// Regression for grow-grow admission concurrency: two grows must be
/// able to sample `vgs` at the same time. A diagnostic build with a
/// global capacity mutex (non-evidence, dirty tree) measured the c128
/// collapse with grows on (about 395 ops/s versus about 2,460 grows-off
/// on that build); that convoy is the suspected mechanism, not an
/// attributed cause of the published residual tail.
///
/// The first grow's `vgs` is a snapshot taken before it parks. A second
/// volume's grow must reach its own `vgs` while the first is still
/// parked (two recorded samples), finish, and bump the epoch; the first
/// then re-samples (three samples). A state-locked sample cannot start
/// the second `vgs`, and an admitted stale sample never re-samples, so
/// both the wait and the final count fail without the respective
/// property.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_grows_sample_capacity_in_parallel() {
    let (provider, gate, runner) = gated_capacity_provider_snapshot();
    let _release = OneShotGateRelease(Arc::clone(&gate));

    let first = provider
        .create_volume(&fixture_create_request("cap-par-grow-a", GIB))
        .await
        .expect("create first grow volume");
    let second = provider
        .create_volume(&fixture_create_request("cap-par-grow-b", GIB))
        .await
        .expect("create second grow volume");
    let vgs_before = count_program(&runner, "vgs");

    gate.arm();
    let first_grow = {
        let provider = Arc::clone(&provider);
        let volume_id = first.volume_id.clone();
        let request = fixture_grow_request("cap-par-grow-a", 2 * GIB, first.generation);
        tokio::spawn(async move { provider.grow_volume(&volume_id, &request).await })
    };
    wait_for_vgs(&gate).await;

    let second_grow = {
        let provider = Arc::clone(&provider);
        let volume_id = second.volume_id.clone();
        let request = fixture_grow_request("cap-par-grow-b", 2 * GIB, second.generation);
        tokio::spawn(async move { provider.grow_volume(&volume_id, &request).await })
    };
    // The evidence: B's own `vgs` starts while A's first `vgs` is held.
    // A's invocation was already recorded before it parked, so two
    // recorded samples means B reached the fake CLI too.
    wait_for_program_count(&runner, "vgs", vgs_before + 2).await;

    let second_result = tokio::time::timeout(IN_FLIGHT_TIMEOUT, second_grow)
        .await
        .expect("the second grow must not wait for the first grow's sample")
        .expect("second grow task join")
        .expect("second grow succeeds while the first samples capacity");
    assert_eq!(second_result.effective_size_bytes, 2 * GIB);

    gate.release();
    let first_result = first_grow
        .await
        .expect("first grow task join")
        .expect("first grow succeeds after re-sampling");
    assert_eq!(first_result.effective_size_bytes, 2 * GIB);
    // A's first sample was a pre-B snapshot, so A must re-sample rather
    // than reserving against stale free space: A, B, and A's re-sample.
    assert!(
        count_program(&runner, "vgs") >= vgs_before + 3,
        "the first grow must re-sample after the second grow's allocation"
    );
}

/// Regression for the allocation epoch: a create that completes between a
/// grow's `vgs` sample and its reservation must force the grow to
/// re-sample. Without the epoch check the grow would reserve (and extend)
/// against free space the create already consumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_create_during_a_grow_sample_forces_a_resample() {
    let (provider, gate, runner) = gated_capacity_provider_snapshot();
    let _release = OneShotGateRelease(Arc::clone(&gate));

    let growing = provider
        .create_volume(&fixture_create_request("cap-epoch-grow", GIB))
        .await
        .expect("create growing volume");
    let lvextends_before = count_program(&runner, "lvextend");
    let vgs_before = count_program(&runner, "vgs");

    gate.arm();
    let grow = {
        let provider = Arc::clone(&provider);
        let volume_id = growing.volume_id.clone();
        let request = fixture_grow_request("cap-epoch-grow", 2 * GIB, growing.generation);
        tokio::spawn(async move { provider.grow_volume(&volume_id, &request).await })
    };
    wait_for_vgs(&gate).await;

    // Consume all but the growing volume's own 1 GiB while the grow's
    // sample is parked. The create bumps the epoch under the state lock.
    let hog = common::POOL_BYTES - 2 * GIB;
    let created = provider
        .create_volume(&fixture_create_request("cap-epoch-hog", hog))
        .await
        .expect("create the consuming volume");
    assert!(created.provisioned_bytes >= hog);

    gate.release();
    let err = grow
        .await
        .expect("grow task join")
        .expect_err("the grow must not promise space the create consumed");
    assert_eq!(err.code, ApiErrorCode::NoSafeCapacity, "{err}");
    assert!(
        err.detail.contains("reserved for in-flight allocations") || err.detail.contains("free"),
        "{err}"
    );
    assert_eq!(
        count_program(&runner, "lvextend"),
        lvextends_before,
        "no stale capacity sample may reach lvextend"
    );
    // Initial sample + post-epoch re-sample.
    assert!(
        count_program(&runner, "vgs") >= vgs_before + 2,
        "the grow must re-sample capacity after the create"
    );
}

/// Regression for a create attempt whose `lvcreate` outcome is unknown:
/// the command may have spent capacity even though the create returns an
/// error. The attempt must bump the allocation epoch so a grow that
/// sampled before it cannot reserve against the pre-attempt figure.
///
/// The gate returns the pre-create `vgs` snapshot. Without the bump the
/// grow never re-samples (two `vgs` calls: its sample and the create's);
/// with the bump it re-samples before admitting (three calls). The create
/// fails without touching the fake world's free space, so this isolates
/// the epoch invalidation from an actual capacity change.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_create_with_an_unknown_outcome_invalidates_a_grows_capacity_sample() {
    let (provider, gate, runner, world) = gated_capacity_provider_snapshot_with_world();
    let _release = OneShotGateRelease(Arc::clone(&gate));

    let growing = provider
        .create_volume(&fixture_create_request("cap-unknown-grow", GIB))
        .await
        .expect("create growing volume");

    let vgs_before = count_program(&runner, "vgs");
    gate.arm();
    let grow = {
        let provider = Arc::clone(&provider);
        let volume_id = growing.volume_id.clone();
        let request = fixture_grow_request("cap-unknown-grow", 2 * GIB, growing.generation);
        tokio::spawn(async move { provider.grow_volume(&volume_id, &request).await })
    };
    wait_for_vgs(&gate).await;

    // The create's `lvcreate` returns a runner error (no exit status):
    // the outcome is unknown and the durable intent remains.
    world
        .lock()
        .expect("world")
        .fail_runner_for
        .push("lvcreate".to_owned());
    let err = provider
        .create_volume(&fixture_create_request("cap-unknown-create", GIB))
        .await
        .expect_err("the lvcreate outcome is unknown");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("outcome is unknown"), "{err}");
    world.lock().expect("world").fail_runner_for.clear();

    gate.release();
    let grown = tokio::time::timeout(IN_FLIGHT_TIMEOUT, grow)
        .await
        .expect("the grow must finish")
        .expect("grow task join")
        .expect("grow succeeds after re-sampling");
    assert!(grown.backing_resized);
    assert_eq!(grown.effective_size_bytes, 2 * GIB);
    // Initial sample + the create attempt + the grow's post-epoch
    // re-sample. A missing bump leaves the grow at two samples.
    assert!(
        count_program(&runner, "vgs") >= vgs_before + 3,
        "an unknown lvcreate outcome must invalidate a grow's in-flight sample"
    );
}

/// Regression for a create whose `lvcreate` exits non-zero: the clear
/// path does not prove the LV absent, so the attempt must still bump the
/// allocation epoch before returning. Same gate and sample-count shape as
/// the unknown-outcome test.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_create_with_a_known_failed_start_invalidates_a_grows_capacity_sample() {
    let (provider, gate, runner, world) = gated_capacity_provider_snapshot_with_world();
    let _release = OneShotGateRelease(Arc::clone(&gate));

    let growing = provider
        .create_volume(&fixture_create_request("cap-failed-grow", GIB))
        .await
        .expect("create growing volume");

    let vgs_before = count_program(&runner, "vgs");
    gate.arm();
    let grow = {
        let provider = Arc::clone(&provider);
        let volume_id = growing.volume_id.clone();
        let request = fixture_grow_request("cap-failed-grow", 2 * GIB, growing.generation);
        tokio::spawn(async move { provider.grow_volume(&volume_id, &request).await })
    };
    wait_for_vgs(&gate).await;

    world.lock().expect("world").fail_lvcreate = true;
    let err = provider
        .create_volume(&fixture_create_request("cap-failed-create", GIB))
        .await
        .expect_err("lvcreate exits non-zero");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    world.lock().expect("world").fail_lvcreate = false;

    gate.release();
    let grown = tokio::time::timeout(IN_FLIGHT_TIMEOUT, grow)
        .await
        .expect("the grow must finish")
        .expect("grow task join")
        .expect("grow succeeds after re-sampling");
    assert!(grown.backing_resized);
    assert!(
        count_program(&runner, "vgs") >= vgs_before + 3,
        "a failed lvcreate start must invalidate a grow's in-flight sample"
    );
}

/// Regression for adopting a verified create intent: the retry returns
/// early and never reaches the fresh-create bump, but the earlier attempt
/// may already have spent capacity. Finding an intent must bump the epoch
/// before the adoption returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_create_intent_adoption_invalidates_a_grows_capacity_sample() {
    let (provider, gate, runner, world) = gated_capacity_provider_snapshot_with_world();
    let _release = OneShotGateRelease(Arc::clone(&gate));

    let growing = provider
        .create_volume(&fixture_create_request("cap-adopt-grow", GIB))
        .await
        .expect("create growing volume");

    // Leave a durable intent through an unknown lvcreate outcome, then
    // make the intent-owned LV appear so the retry adopts it.
    let adopted_id = volume_id("cap-adopt-create");
    let request = fixture_create_request(adopted_id.as_str(), GIB);
    world
        .lock()
        .expect("world")
        .fail_runner_for
        .push("lvcreate".to_owned());
    provider
        .create_volume(&request)
        .await
        .expect_err("the lvcreate outcome is unknown");
    world.lock().expect("world").fail_runner_for.clear();
    common::add_lv(&world, common::CLAIMED_VG, &lv_name_for(&adopted_id), GIB);

    let vgs_before = count_program(&runner, "vgs");
    gate.arm();
    let grow = {
        let provider = Arc::clone(&provider);
        let volume_id = growing.volume_id.clone();
        let request = fixture_grow_request("cap-adopt-grow", 2 * GIB, growing.generation);
        tokio::spawn(async move { provider.grow_volume(&volume_id, &request).await })
    };
    wait_for_vgs(&gate).await;

    // The retry adopts the verified LV and returns before the fresh-create
    // bump; the fix bumps on finding the intent.
    let adopted = provider
        .create_volume(&request)
        .await
        .expect("the verified intent-owned LV is adopted");
    assert_eq!(adopted.state, VolumeLifecycle::Ready);
    assert_eq!(adopted.provisioned_bytes, GIB);

    gate.release();
    let grown = tokio::time::timeout(IN_FLIGHT_TIMEOUT, grow)
        .await
        .expect("the grow must finish")
        .expect("grow task join")
        .expect("grow succeeds after re-sampling");
    assert!(grown.backing_resized);
    assert!(
        count_program(&runner, "vgs") >= vgs_before + 2,
        "an adopted create intent must invalidate a grow's in-flight sample"
    );
}

/// Regression for publishing a completed grow allocation under the state
/// lock: a create samples `vgs` and reads the grow reservations while
/// holding that lock, so a grow must not drop its reservation (and land
/// its epoch bump) between those two reads. Without the publish lock the
/// create admits against pre-allocation free space and then consumes
/// bytes the grow already spent.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_create_cannot_spend_a_grows_completed_allocation() {
    let (provider, vgs_gate, extend_gate, _runner) = gated_capacity_and_extend_provider();
    let _vgs_release = OneShotGateRelease(Arc::clone(&vgs_gate));
    let _extend_release = LvextendGateRelease(Arc::clone(&extend_gate));

    let growing = provider
        .create_volume(&fixture_create_request("cap-publish-grow", GIB))
        .await
        .expect("create growing volume");

    // The grow samples, reserves 1 GiB, and parks inside `lvextend`.
    let grow = {
        let provider = Arc::clone(&provider);
        let volume_id = growing.volume_id.clone();
        let request = fixture_grow_request("cap-publish-grow", 2 * GIB, growing.generation);
        tokio::spawn(async move { provider.grow_volume(&volume_id, &request).await })
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while extend_gate.inside() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the grow must reach lvextend"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // The create's `vgs` snapshots the pre-lvextend free space and parks,
    // holding the state lock across the sample.
    vgs_gate.arm();
    let hog = common::POOL_BYTES - 2 * GIB + MIB;
    let create = {
        let provider = Arc::clone(&provider);
        tokio::spawn(async move {
            provider
                .create_volume(&fixture_create_request("cap-publish-hog", hog))
                .await
        })
    };
    wait_for_vgs(&vgs_gate).await;

    // Complete the grow while the create is parked. It must not be able
    // to drop the reservation before the create reads it.
    extend_gate.release();
    tokio::time::sleep(Duration::from_millis(150)).await;
    vgs_gate.release();

    let err = create
        .await
        .expect("create task join")
        .expect_err("the create must account for the grow's completed allocation");
    assert_eq!(err.code, ApiErrorCode::NoSafeCapacity, "{err}");

    let grown = tokio::time::timeout(IN_FLIGHT_TIMEOUT, grow)
        .await
        .expect("the grow must finish after the create releases the state lock")
        .expect("grow task join")
        .expect("grow succeeds");
    assert_eq!(grown.effective_size_bytes, 2 * GIB);
}

/// A move completion bumps the volume generation without touching the
/// grow intent, so it must defer to a live grow; otherwise a crash would
/// make the durable intent look stale to recovery and it would be dropped
/// while the LV is already extended. The deferral must skip only that
/// record: the rest of the pass still classifies other volumes.
#[allow(clippy::too_many_lines)] // one grow/deferral/two-volume-pass narrative
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn move_completion_defers_while_a_grow_is_in_flight() {
    let state_path = common::leak_tempdir().join("state.json");
    common::seed_claimed_state(&state_path);
    let world = gated_move_world();
    let gate = Arc::new(GrowGate::default());
    let runner = {
        let world = Arc::clone(&world);
        let gate = Arc::clone(&gate);
        Arc::new(FakeRunner::with_closure(move |program, args| {
            if program == "lvextend" {
                gate.entered();
                gate.wait_release();
            }
            let mut world = world.lock().ok()?;
            common::script(&mut world, program, args)
        }))
    };
    let build = |runner: Arc<FakeRunner>| {
        Arc::new(
            LvmProvider::new(
                runner as Arc<dyn CommandRunner>,
                state_path.clone(),
                common::leak_tempdir(),
                common::VG_PREFIX.to_owned(),
                common::AUTH_TOKEN.to_owned(),
            )
            .expect("provider construction"),
        )
    };
    let _release = GateRelease(Arc::clone(&gate));

    let (growing, other) = {
        let provider = build(Arc::clone(&runner));
        let growing = provider
            .create_volume(&fixture_create_request("move-grow", GIB))
            .await
            .expect("create growing volume");
        let other = provider
            .create_volume(&fixture_create_request("move-later", GIB))
            .await
            .expect("create later volume");
        (growing, other)
    };
    // Seed a settled FAILED record for both volumes and restart so they
    // are part of the provider's in-memory state. The placements are
    // still on the source PV here, so the constructor's reconcile pass
    // cannot complete either record and bump a generation behind this
    // test's back.
    for volume_id in ["move-grow", "move-later"] {
        common::seed_move(
            &state_path,
            volume_id,
            common::MOVE_SOURCE_PV,
            common::MOVE_TARGET_PV,
            MoveVolumeBackingState::Failed,
        );
    }
    let provider = build(runner);
    for volume in [&growing, &other] {
        let inspected = provider
            .inspect_volume(&volume.volume_id)
            .await
            .expect("inspect restarted");
        assert_eq!(inspected.generation, volume.generation);
    }
    // Now move both LVs' placements to the records' target so the world
    // proves the relocations the next reconcile pass will try to record.
    {
        let mut world = world.lock().expect("world");
        for volume in [&growing, &other] {
            world.lv_devices.insert(
                format!("{}/{}", common::CLAIMED_VG, lv_name_for(&volume.volume_id)),
                vec![common::MOVE_TARGET_PV.to_owned()],
            );
        }
    }

    let grow = {
        let provider = Arc::clone(&provider);
        let volume_id = growing.volume_id.clone();
        let request = fixture_grow_request("move-grow", 2 * GIB, growing.generation);
        tokio::spawn(async move { provider.grow_volume(&volume_id, &request).await })
    };
    wait_for_lvextend(&gate).await;

    // The growing volume's completion is deferred; the later volume's
    // completion still lands in the same pass (no pass-wide abort).
    let report = tokio::time::timeout(
        IN_FLIGHT_TIMEOUT,
        tokio::task::spawn_blocking({
            let provider = Arc::clone(&provider);
            move || provider.move_reconcile_pass()
        }),
    )
    .await
    .expect("the move pass must not block on the state lock")
    .expect("move pass join")
    .expect("a deferral must not abort the pass");
    assert_eq!(
        report.deferred_while_growing,
        vec![growing.volume_id.clone()]
    );
    assert_eq!(report.completed, vec![other.volume_id.clone()]);
    let inspected = provider
        .inspect_volume(&other.volume_id)
        .await
        .expect("inspect later volume");
    assert_eq!(inspected.generation, other.generation + 1);

    gate.release();
    let grown = grow.await.expect("grow task").expect("grow succeeds");
    assert_eq!(grown.effective_size_bytes, 2 * GIB);

    // With the grow committed, the next pass completes the deferred move
    // and bumps that volume's generation exactly once.
    let report = provider
        .move_reconcile_pass()
        .expect("the retry pass completes the deferred move");
    assert_eq!(report.completed, vec![growing.volume_id.clone()]);
    assert_eq!(report.deferred_while_growing, Vec::<VolumeId>::new());
    let inspected = provider
        .inspect_volume(&growing.volume_id)
        .await
        .expect("inspect volume");
    assert_eq!(inspected.generation, growing.generation + 2);
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
async fn reattaching_a_consumed_id_fences_the_retired_instance_by_generation() {
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

    // Reusing an id is a new instance with a strictly greater generation.
    // It is not refused, so a churned volume stays usable (issue #63).
    let reused = fixture_attach_request("reuse-vol", "reuse-att", detached.generation);
    let reattached = fixture
        .provider
        .attach_volume(&created.volume_id, &reused)
        .await
        .expect("re-attach with a consumed id is a new instance");
    assert!(
        reattached.attachment_generation > first.attachment_generation,
        "the new instance must fence the retired one by generation"
    );

    // A delayed report from the retired instance carries the old
    // generation and is refused; it never labels the new instance.
    let delayed = fixture_report_request(
        "reuse-att",
        first.attachment_generation,
        AttachmentState::Active,
        AttachmentEvidence::GuestConfirmed,
    );
    let err = fixture
        .provider
        .report_attachment_state(&created.volume_id, &delayed)
        .await
        .expect_err("a delayed report from the retired generation");
    assert_eq!(err.code, ApiErrorCode::StaleGeneration);

    // The fence is durable: a restart still refuses the retired generation.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let err = restarted
        .report_attachment_state(&created.volume_id, &delayed)
        .await
        .expect_err("the retired generation after restart");
    assert_eq!(err.code, ApiErrorCode::StaleGeneration);

    // A fresh id is admitted normally after the new instance detaches.
    let detached2 = restarted
        .detach_volume(
            &created.volume_id,
            &reattached.attachment_id,
            &fixture_detach_request("reuse-att", reattached.attachment_generation),
        )
        .await
        .expect("detach the new instance");
    let fresh = fixture_attach_request("reuse-vol", "reuse-att-2", detached2.generation);
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

// ---------------------------------------------------------------------------
// Issue #70: durable create/grow/delete intents across crash windows
// ---------------------------------------------------------------------------

// -- Create startup recovery ------------------------------------------------

#[tokio::test]
async fn create_intent_adopts_a_verified_lv_left_by_a_kill_window() {
    let (state_path, world) = seeded_world();
    let id = volume_id("intent-adopt");
    common::seed_create_intent(
        &state_path,
        id.as_str(),
        common::CLAIMED_VG,
        GIB,
        "seed-create",
    );
    common::add_lv(&world, common::CLAIMED_VG, &lv_name_for(&id), GIB);

    // The fixture retains the runner, so the construction's commands are
    // assertable (the adoption path must be read-only verification).
    let fixture = common::fixture_from_state(state_path, world);

    let inspected = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect("the intent-owned LV is adopted");
    assert_eq!(inspected.state, VolumeLifecycle::Ready);
    assert_eq!(inspected.generation, 1);
    assert_eq!(inspected.provisioned_bytes, GIB);

    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_some(), "the volume record committed");
    assert!(
        durable.create_intent(&id).is_none(),
        "adoption drops the create intent"
    );

    let destructive = fixture
        .runner
        .invocations()
        .into_iter()
        .filter(|invocation| invocation.program == "lvcreate" || invocation.program == "lvremove")
        .count();
    assert_eq!(
        destructive, 0,
        "startup adoption runs no destructive command"
    );

    let report = fixture.provider.reconcile_report().expect("report");
    assert!(report.missing_volumes.is_empty(), "{report:?}");
    assert!(report.foreign_lvs.is_empty(), "{report:?}");
}

#[tokio::test]
async fn create_intent_is_dropped_when_the_intent_owned_lv_is_absent() {
    let (state_path, world) = seeded_world();
    let id = volume_id("intent-absent");
    common::seed_create_intent(
        &state_path,
        id.as_str(),
        common::CLAIMED_VG,
        GIB,
        "seed-create",
    );

    let provider = common::provider_from(&state_path, &world);
    let err = provider
        .inspect_volume(&id)
        .await
        .expect_err("a dropped intent is not a volume");
    assert_eq!(err.code, ApiErrorCode::NotFound);

    let durable = LvmState::load(&state_path).expect("durable state");
    assert!(durable.create_intent(&id).is_none());
    assert!(durable.volume(&id).is_none());

    // The dropped intent does not wedge a fresh create with the same id.
    let created = provider
        .create_volume(&fixture_create_request("intent-absent", GIB))
        .await
        .expect("fresh create after the intent was dropped");
    assert_eq!(created.state, VolumeLifecycle::Ready);
    assert!(world.lock().expect("world").lvs.contains_key(&format!(
        "{}/{}",
        common::CLAIMED_VG,
        lv_name_for(&id)
    )));
}

#[test]
fn create_intent_with_an_undersized_lv_fails_startup() {
    let (state_path, world) = seeded_world();
    let id = volume_id("intent-undersized");
    common::seed_create_intent(
        &state_path,
        id.as_str(),
        common::CLAIMED_VG,
        GIB,
        "seed-create",
    );
    common::add_lv(&world, common::CLAIMED_VG, &lv_name_for(&id), 64 * MIB);

    let err = startup_error(&state_path, &world);
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("smaller than the requested"), "{err}");

    // An intent that does not own its id-derived LV is refused typed too,
    // before any geometry is trusted.
    let (state_path, world) = seeded_world();
    let id = volume_id("intent-wrong-name");
    common::seed_create_intent(
        &state_path,
        id.as_str(),
        common::CLAIMED_VG,
        GIB,
        "seed-create",
    );
    {
        let mut state = LvmState::load(&state_path).expect("load state");
        let mut intent = state
            .create_intent(&id)
            .cloned()
            .expect("the seeded create intent");
        intent.lv_name = "lv-not-derived-from-the-id".to_owned();
        state.insert_create_intent(id.clone(), intent);
        state.save(&state_path).expect("save the mismatched intent");
    }
    let err = startup_error(&state_path, &world);
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("does not own its LV"), "{err}");
}

/// The create-intent save fails before its rename (`AfterTmpWrite`):
/// the durable file is byte-for-byte unchanged, no `lvcreate` runs, and
/// the retry succeeds once the seam is consumed.
#[tokio::test]
async fn create_intent_save_failure_runs_no_lvcreate() {
    let fixture = fixture();
    let volume = "create-intent-nowrite";
    let id = volume_id(volume);
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    let state_before = fs::read(&fixture.state_path).expect("state snapshot");
    let invocations_before = fixture.runner.invocations().len();

    // The create's first save is the durable intent; failing before the
    // rename leaves the pre-create durable state in place.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterTmpWrite,
        0,
    );
    let err = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect_err("the create-intent save fails");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("injected state save failure"), "{err}");

    assert!(
        !fixture.runner.invocations()[invocations_before..]
            .iter()
            .any(|invocation| invocation.program == "lvcreate"),
        "a failed intent save must run no lvcreate"
    );
    assert_eq!(
        fs::read(&fixture.state_path).expect("state after the failed save"),
        state_before,
        "the failed save must not alter the durable file"
    );
    let inspected = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect_err("no volume is visible");
    assert_eq!(inspected.code, ApiErrorCode::NotFound);
    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path),
        "the fake world has no LV"
    );

    // The arm was consumed: the retry creates normally.
    let created = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("the retry creates");
    assert_eq!(created.state, VolumeLifecycle::Ready);
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path)
    );
}

/// The create-intent save's rename lands before the injected failure:
/// the durable intent is present even though no LV exists. Startup
/// observes the absent intent-owned LV and drops it; a fresh create
/// with the same id and payload then succeeds.
#[tokio::test]
async fn create_intent_save_failure_after_rename_is_dropped_on_restart() {
    let fixture = fixture();
    let volume = "create-intent-rename";
    let id = volume_id(volume);
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    let invocations_before = fixture.runner.invocations().len();

    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterRenameBeforeDirFsync,
        0,
    );
    let err = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect_err("the create-intent save fails after its rename");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("injected state save failure"), "{err}");
    assert!(
        err.detail
            .contains("durable state did record the create intent"),
        "the error names the durable intent left behind: {err}"
    );
    assert!(
        !fixture.runner.invocations()[invocations_before..]
            .iter()
            .any(|invocation| invocation.program == "lvcreate"),
        "the durable intent save precedes lvcreate"
    );

    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(
        durable.create_intent(&id).is_some(),
        "the rename made the create intent durable"
    );
    assert!(durable.volume(&id).is_none());

    // Startup observes no intent-owned LV: it drops the stale intent and
    // never fabricates a volume.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let inspected = restarted
        .inspect_volume(&id)
        .await
        .expect_err("the absent intent-owned LV is not adopted");
    assert_eq!(inspected.code, ApiErrorCode::NotFound);
    let durable = LvmState::load(&fixture.state_path).expect("durable after restart");
    assert!(durable.create_intents().is_empty(), "{durable:?}");
    assert!(durable.volume(&id).is_none());

    // The dropped intent does not wedge the same create.
    let created = restarted
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("a fresh create with the same id and payload succeeds");
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
}

/// A runner-level `lvcreate` failure has no exit status: the outcome is
/// unknown, so the durable intent must be kept (never cleared) for
/// startup recovery to classify the observed LV.
#[tokio::test]
async fn a_runner_error_keeps_the_create_intent_for_startup_recovery() {
    let fixture = fixture();
    let volume = "create-runner-error";
    let id = volume_id(volume);
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    let invocations_before = fixture.runner.invocations().len();
    fixture
        .world
        .lock()
        .expect("world")
        .fail_runner_for
        .push("lvcreate".to_owned());

    let err = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect_err("the runner fails without an exit status");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("outcome is unknown"), "{err}");
    assert!(
        err.detail.contains("durable create intent remains"),
        "the error says the unknown outcome keeps the intent: {err}"
    );

    assert!(
        fixture.runner.invocations()[invocations_before..]
            .iter()
            .any(|invocation| invocation.program == "lvcreate"),
        "the command was attempted"
    );
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(
        durable.create_intent(&id).is_some(),
        "an unknown command outcome never clears the durable intent"
    );
    assert!(durable.volume(&id).is_none());
    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path)
    );

    // Startup observes no intent-owned LV and drops the intent.
    fixture.world.lock().expect("world").fail_runner_for.clear();
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let durable = LvmState::load(&fixture.state_path).expect("durable after restart");
    assert!(durable.create_intents().is_empty(), "{durable:?}");
    let inspected = restarted
        .inspect_volume(&id)
        .await
        .expect_err("no volume was fabricated");
    assert_eq!(inspected.code, ApiErrorCode::NotFound);
}

/// A known `lvcreate` failure clears the intent, but when that clear
/// save fails before its rename the durable intent survives. Memory
/// must be put back to match the durable file so a later successful
/// save cannot erase an authorization startup still has to classify.
#[tokio::test]
async fn a_create_clear_save_failure_keeps_the_intent_in_memory() {
    let fixture = fixture();
    let volume = "create-clear-save-fails";
    let id = volume_id(volume);
    fixture.world.lock().expect("world").fail_lvcreate = true;
    // Save 0 is the durable intent; save 1 is the clear after the known
    // command failure. Failing save 1 before its rename leaves the
    // intent in the old durable file.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterTmpWrite,
        1,
    );

    let err = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect_err("lvcreate reports a known failure");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("lvcreate"), "{err}");
    assert!(
        err.detail.contains("could not be cleared durably"),
        "the failed clear is reported honestly: {err}"
    );

    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(
        durable.create_intent(&id).is_some(),
        "the pre-rename clear failure left the intent durable"
    );

    // A later successful mutation from the same in-memory provider must
    // not erase the re-inserted intent.
    fixture.world.lock().expect("world").fail_lvcreate = false;
    fixture
        .provider
        .create_volume(&fixture_create_request("create-clear-save-witness", GIB))
        .await
        .expect("a second create saves the in-memory state");
    let durable = LvmState::load(&fixture.state_path).expect("durable after the next save");
    assert!(
        durable.create_intent(&id).is_some(),
        "the re-inserted intent survived a later save"
    );

    // Startup drops it: no intent-owned LV exists.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let durable = LvmState::load(&fixture.state_path).expect("durable after restart");
    assert!(durable.create_intents().is_empty(), "{durable:?}");
    let err = restarted
        .inspect_volume(&id)
        .await
        .expect_err("no volume was adopted");
    assert_eq!(err.code, ApiErrorCode::NotFound);
}

/// A create whose final save fails runs the checked compensation; when
/// that compensation's clear save also fails **before its rename**, the
/// durable file still holds the create intent. Memory must put it back,
/// or the next successful save would erase the anchor and orphan an LV
/// whose `lvremove` also failed.
#[tokio::test]
async fn a_create_compensation_clear_failure_keeps_the_intent_in_memory() {
    let fixture = fixture();
    let volume = "create-compensation-clear-fails";
    let id = volume_id(volume);
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    // `lvcreate` succeeds, then a persistent failure arm makes the final
    // save fail pre-rename (the valid durable file still holds the
    // intent) and the compensating clear save fail the same way.
    fixture
        .provider
        .store_crash_hooks()
        .arm_save_failure_until_cleared(STORE_LVM_STATE, StoreSavePoint::AfterTmpWrite, 1);

    let err = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect_err("the final save and its compensating clear both fail");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(
        err.detail
            .contains("durable create intent could not be cleared"),
        "the failed clear is reported honestly: {err}"
    );
    assert!(
        err.detail.contains("cleanup save failed"),
        "the compensation save failure is reported: {err}"
    );

    // The checked compensation removed the LV and the durable file still
    // holds the intent (the clear's rename did not land).
    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path),
        "the compensating lvremove removed the LV"
    );
    let durable = LvmState::load(&fixture.state_path).expect("durable state is still readable");
    assert!(
        durable.create_intent(&id).is_some(),
        "the pre-rename clear failure left the intent durable"
    );

    // Unblock saves and drive a later successful mutation from the same
    // in-memory provider: the re-inserted intent must survive it.
    fixture.provider.store_crash_hooks().clear();
    fixture
        .provider
        .create_volume(&fixture_create_request("create-compensation-witness", GIB))
        .await
        .expect("a second create saves the in-memory state");
    let durable = LvmState::load(&fixture.state_path).expect("durable after the next save");
    assert!(
        durable.create_intent(&id).is_some(),
        "the re-inserted create intent survived a later save"
    );

    // Startup observes no intent-owned LV and drops the intent.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let durable = LvmState::load(&fixture.state_path).expect("durable after restart");
    assert!(durable.create_intents().is_empty(), "{durable:?}");
    let err = restarted
        .inspect_volume(&id)
        .await
        .expect_err("no volume was fabricated");
    assert_eq!(err.code, ApiErrorCode::NotFound);
}

// -- Grow -------------------------------------------------------------------

#[tokio::test]
async fn grow_intent_save_failure_means_no_lvextend() {
    let fixture = fixture();
    let volume = "grow-intent-fail";
    let created = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("create");
    let id = created.volume_id.clone();
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    let invocations_before = fixture.runner.invocations().len();

    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterTmpWrite,
        0,
    );
    let err = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request(volume, 2 * GIB, created.generation),
        )
        .await
        .expect_err("the grow intent save fails");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("injected state save failure"), "{err}");
    assert!(err.detail.contains("no grow was attempted"), "{err}");

    assert!(
        !fixture.runner.invocations()[invocations_before..]
            .iter()
            .any(|invocation| invocation.program == "lvextend"),
        "a failed intent save must not reach lvextend"
    );
    assert_eq!(
        fixture.world.lock().expect("world").lvs.get(&lv_path),
        Some(&GIB),
        "the LV was not extended"
    );

    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    let stored = durable.volume(&id).expect("the volume record");
    assert_eq!(stored.entry.size_bytes, GIB);
    assert_eq!(stored.entry.generation, 1);
    assert!(durable.grow_intent(&id).is_none(), "no intent survived");

    let inspected = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect("inspect unchanged");
    assert_eq!(inspected.provisioned_bytes, GIB);
    assert_eq!(inspected.generation, 1);

    // The retry runs a fresh durable-first grow.
    let grown = fixture
        .provider
        .grow_volume(&id, &fixture_grow_request(volume, 2 * GIB, 1))
        .await
        .expect("retry the grow");
    assert_eq!(grown.effective_size_bytes, 2 * GIB);
    let inspected = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect("inspect after retry");
    assert_eq!(inspected.provisioned_bytes, 2 * GIB);
    assert_eq!(inspected.generation, 2);
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.grow_intent(&id).is_none());
}

/// The grow-intent save's rename lands before the injected failure: the
/// durable intent is present even though no `lvextend` ran. Startup sees
/// the LV still at its from-size and drops the intent without changing
/// the volume.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one crash-window/restart narrative
async fn grow_intent_save_failure_after_rename_is_dropped_on_restart() {
    let fixture = fixture();
    let volume = "grow-intent-rename";
    let created = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("create");
    let id = created.volume_id.clone();
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    let invocations_before = fixture.runner.invocations().len();

    // The grow's first save is the durable intent; fail it after the
    // rename landed. No destructive command has run yet.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterRenameBeforeDirFsync,
        0,
    );
    let err = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request(volume, 2 * GIB, created.generation),
        )
        .await
        .expect_err("the grow-intent save fails after its rename");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("injected state save failure"), "{err}");
    assert!(
        err.detail
            .contains("durable state did record the grow intent"),
        "the error names the durable intent left behind: {err}"
    );

    assert!(
        !fixture.runner.invocations()[invocations_before..]
            .iter()
            .any(|invocation| invocation.program == "lvextend"),
        "a failed intent save must not reach lvextend"
    );
    assert_eq!(
        fixture.world.lock().expect("world").lvs.get(&lv_path),
        Some(&GIB),
        "the LV is still at the from-size"
    );
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(
        durable.grow_intent(&id).is_some(),
        "the rename made the grow intent durable"
    );
    let stored = durable.volume(&id).expect("the volume record");
    assert_eq!(stored.entry.size_bytes, GIB);
    assert_eq!(stored.entry.generation, created.generation);

    // Startup observes the LV at its from-size and drops the intent; no
    // size or generation change is invented.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let durable = LvmState::load(&fixture.state_path).expect("durable after restart");
    assert!(durable.grow_intents().is_empty(), "{durable:?}");
    let inspected = restarted
        .inspect_volume(&id)
        .await
        .expect("the unchanged volume is served");
    assert_eq!(inspected.provisioned_bytes, GIB);
    assert_eq!(inspected.generation, created.generation);
}

/// A runner-level `lvextend` failure has no exit status: the outcome is
/// unknown, so the durable grow intent must be kept for startup
/// recovery to classify the observed size.
#[tokio::test]
async fn a_runner_error_keeps_the_grow_intent_for_startup_recovery() {
    let fixture = fixture();
    let volume = "grow-runner-error";
    let created = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("create");
    let id = created.volume_id.clone();
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    fixture
        .world
        .lock()
        .expect("world")
        .fail_runner_for
        .push("lvextend".to_owned());

    let err = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request(volume, 2 * GIB, created.generation),
        )
        .await
        .expect_err("the runner fails without an exit status");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("outcome is unknown"), "{err}");
    assert!(
        err.detail.contains("durable grow intent remains"),
        "the error says the unknown outcome keeps the intent: {err}"
    );

    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(
        durable.grow_intent(&id).is_some(),
        "an unknown command outcome never clears the durable intent"
    );
    let stored = durable.volume(&id).expect("the volume record");
    assert_eq!(stored.entry.size_bytes, GIB);

    // Startup observes the LV at its from-size and drops the intent.
    fixture.world.lock().expect("world").fail_runner_for.clear();
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let durable = LvmState::load(&fixture.state_path).expect("durable after restart");
    assert!(durable.grow_intents().is_empty(), "{durable:?}");
    let inspected = restarted
        .inspect_volume(&id)
        .await
        .expect("the unchanged volume is served");
    assert_eq!(inspected.provisioned_bytes, GIB);
    assert_eq!(inspected.generation, created.generation);
    assert_eq!(
        fixture.world.lock().expect("world").lvs.get(&lv_path),
        Some(&GIB),
        "the LV was not extended"
    );
}

/// A known `lvextend` failure clears the intent, but when that clear
/// save fails before its rename the durable intent survives. Memory
/// must be put back to match the durable file so a later successful
/// save cannot erase an authorization startup still has to classify.
#[tokio::test]
async fn a_grow_clear_save_failure_keeps_the_intent_in_memory() {
    let fixture = fixture();
    let volume = "grow-clear-save-fails";
    let created = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("create");
    let id = created.volume_id.clone();
    fixture.world.lock().expect("world").fail_lvextend = true;
    // Save 0 is the durable intent; save 1 is the clear after the known
    // command failure. Failing save 1 before its rename leaves the
    // intent in the old durable file.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterTmpWrite,
        1,
    );

    let err = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request(volume, 2 * GIB, created.generation),
        )
        .await
        .expect_err("lvextend reports a known failure");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("lvextend"), "{err}");
    assert!(
        err.detail.contains("could not be cleared durably"),
        "the failed clear is reported honestly: {err}"
    );

    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(
        durable.grow_intent(&id).is_some(),
        "the pre-rename clear failure left the intent durable"
    );

    // A later successful mutation from the same in-memory provider must
    // not erase the re-inserted intent.
    fixture.world.lock().expect("world").fail_lvextend = false;
    fixture
        .provider
        .create_volume(&fixture_create_request("grow-clear-save-witness", GIB))
        .await
        .expect("a second create saves the in-memory state");
    let durable = LvmState::load(&fixture.state_path).expect("durable after the next save");
    assert!(
        durable.grow_intent(&id).is_some(),
        "the re-inserted grow intent survived a later save"
    );

    // Startup sees the LV at its from-size and drops the intent.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let durable = LvmState::load(&fixture.state_path).expect("durable after restart");
    assert!(durable.grow_intents().is_empty(), "{durable:?}");
    let inspected = restarted
        .inspect_volume(&id)
        .await
        .expect("the unchanged volume is served");
    assert_eq!(inspected.provisioned_bytes, GIB);
    assert_eq!(inspected.generation, created.generation);
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one crash-window/restart/roll-forward narrative
async fn grow_final_save_failure_rolls_forward_on_restart() {
    let fixture = fixture();
    let volume = "grow-final-fail";
    let created = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("create");
    let id = created.volume_id.clone();
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));

    // `skip = 1` passes the grow-intent save and fails the final
    // record save before its rename: the old durable state (volume at
    // its from-size + the grow intent) survives.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterTmpWrite,
        1,
    );
    let err = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request(volume, 2 * GIB, created.generation),
        )
        .await
        .expect_err("the final grow save fails");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("injected state save failure"), "{err}");
    assert!(err.detail.contains("was extended"), "{err}");
    assert!(err.detail.contains("grow intent"), "{err}");

    // The world really grew; the durable record did not.
    assert_eq!(
        fixture.world.lock().expect("world").lvs.get(&lv_path),
        Some(&(2 * GIB))
    );
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    let stored = durable.volume(&id).expect("the durable volume");
    assert_eq!(stored.entry.size_bytes, GIB);
    assert_eq!(stored.entry.generation, 1);
    let intent = durable.grow_intent(&id).expect("the durable grow intent");
    assert_eq!(intent.from_size_bytes, GIB);
    assert_eq!(intent.requested_size_bytes, 2 * GIB);
    assert_eq!(intent.expected_generation, 1);

    // The contract keeps the observed shape in memory: same-incarnation
    // inspect reports the extended LV, never the stale durable size.
    let observed = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect("same-incarnation inspect");
    assert_eq!(observed.provisioned_bytes, 2 * GIB);
    assert_eq!(observed.generation, 2);

    // Restart rolls the grow forward exactly once.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let inspected = restarted
        .inspect_volume(&id)
        .await
        .expect("inspect after restart");
    assert_eq!(inspected.provisioned_bytes, 2 * GIB);
    assert_eq!(inspected.generation, 2);
    let durable = LvmState::load(&fixture.state_path).expect("durable after restart");
    assert!(durable.grow_intent(&id).is_none(), "the intent is consumed");
    assert_eq!(
        durable.volume(&id).expect("volume").entry.generation,
        2,
        "the restart bumped the generation exactly once"
    );
    let report = restarted.reconcile_report().expect("report");
    assert!(report.missing_volumes.is_empty(), "{report:?}");
    assert!(report.foreign_lvs.is_empty(), "{report:?}");

    // The next grow fences on the rolled-forward generation.
    let grown = restarted
        .grow_volume(&id, &fixture_grow_request(volume, 3 * GIB, 2))
        .await
        .expect("grow to 3 GiB at generation 2");
    assert_eq!(grown.effective_size_bytes, 3 * GIB);
}

/// The FINAL grow save's rename lands before the injected failure (the
/// documented carve-out): the durable state already contains the grow,
/// so memory keeps the observed completion and a restart neither bumps
/// the generation again nor shrinks the LV.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one crash-window/restart narrative
async fn grow_final_save_failure_after_rename_is_the_carve_out() {
    let fixture = fixture();
    let volume = "grow-final-rename";
    let created = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("create");
    let id = created.volume_id.clone();
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));

    // Grow saves: (1) the durable intent, (2) the final record + intent
    // removal. Skip the first; fail the second after its rename.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterRenameBeforeDirFsync,
        1,
    );
    let err = fixture
        .provider
        .grow_volume(
            &id,
            &fixture_grow_request(volume, 2 * GIB, created.generation),
        )
        .await
        .expect_err("the final grow save fails after its rename");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(
        err.detail
            .contains("the durable state already contains the grow"),
        "the carve-out names the committed grow: {err}"
    );

    // The rename landed: the durable record carries the new size and
    // the bumped generation, with no grow intent left.
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    let stored = durable.volume(&id).expect("the durable volume");
    assert_eq!(stored.entry.size_bytes, 2 * GIB);
    assert_eq!(stored.entry.generation, created.generation + 1);
    assert!(durable.grow_intent(&id).is_none(), "the intent is consumed");

    // Memory keeps the observed completion (the LV was really extended).
    let inspected = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect("same-incarnation inspect");
    assert_eq!(inspected.provisioned_bytes, 2 * GIB);
    assert_eq!(inspected.generation, created.generation + 1);

    // A restart agrees and performs no further roll-forward.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let inspected = restarted
        .inspect_volume(&id)
        .await
        .expect("inspect after restart");
    assert_eq!(inspected.provisioned_bytes, 2 * GIB);
    assert_eq!(inspected.generation, created.generation + 1);
    let durable = LvmState::load(&fixture.state_path).expect("durable after restart");
    let stored = durable.volume(&id).expect("the durable volume");
    assert_eq!(stored.entry.size_bytes, 2 * GIB);
    assert_eq!(stored.entry.generation, created.generation + 1);
    assert!(durable.grow_intent(&id).is_none());
    assert_eq!(
        fixture.world.lock().expect("world").lvs.get(&lv_path),
        Some(&(2 * GIB)),
        "the LV is never shrunk"
    );
}

#[tokio::test]
async fn grow_intent_roll_forward_at_startup() {
    let (state_path, world) = seeded_world();
    let id = volume_id("grow-roll-forward");
    common::seed_volume(&state_path, id.as_str(), common::CLAIMED_VG, GIB);
    common::add_lv(&world, common::CLAIMED_VG, &lv_name_for(&id), 2 * GIB);
    common::seed_grow_intent(&state_path, id.as_str(), "op-seeded-grow", GIB, 2 * GIB, 1);

    let provider = common::provider_from(&state_path, &world);
    let inspected = provider
        .inspect_volume(&id)
        .await
        .expect("the volume rolled forward");
    assert_eq!(inspected.provisioned_bytes, 2 * GIB);
    assert_eq!(inspected.generation, 2);

    let durable = LvmState::load(&state_path).expect("durable state");
    assert!(durable.grow_intent(&id).is_none(), "the intent is consumed");
}

#[tokio::test]
async fn grow_intent_is_dropped_when_the_lv_did_not_grow() {
    let (state_path, world) = seeded_world();
    let id = volume_id("grow-not-landed");
    common::seed_volume(&state_path, id.as_str(), common::CLAIMED_VG, GIB);
    common::add_lv(&world, common::CLAIMED_VG, &lv_name_for(&id), GIB);
    common::seed_grow_intent(&state_path, id.as_str(), "op-seeded-grow", GIB, 2 * GIB, 1);

    let provider = common::provider_from(&state_path, &world);
    let inspected = provider
        .inspect_volume(&id)
        .await
        .expect("the volume is unchanged");
    assert_eq!(inspected.provisioned_bytes, GIB);
    assert_eq!(inspected.generation, 1);

    let durable = LvmState::load(&state_path).expect("durable state");
    assert!(
        durable.grow_intent(&id).is_none(),
        "an unlanded grow drops the intent"
    );
    assert_eq!(durable.volume(&id).expect("volume").entry.size_bytes, GIB);
}

#[test]
fn grow_intent_with_ambiguous_geometry_fails_startup() {
    let (state_path, world) = seeded_world();
    let id = volume_id("grow-ambiguous");
    common::seed_volume(&state_path, id.as_str(), common::CLAIMED_VG, GIB);
    // Strictly between the from-size and the requested size.
    common::add_lv(&world, common::CLAIMED_VG, &lv_name_for(&id), 3 * GIB / 2);
    common::seed_grow_intent(&state_path, id.as_str(), "op-seeded-grow", GIB, 2 * GIB, 1);

    let err = startup_error(&state_path, &world);
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("ambiguous"), "{err}");
}

#[tokio::test]
async fn grow_intent_with_a_stale_generation_is_dropped_at_startup() {
    let (state_path, world) = seeded_world();
    let id = volume_id("grow-stale-generation");
    common::seed_volume(&state_path, id.as_str(), common::CLAIMED_VG, GIB);
    // The LV was extended, but the intent's authorization (generation 9)
    // is stale: it must be dropped, never rolled forward.
    common::add_lv(&world, common::CLAIMED_VG, &lv_name_for(&id), 2 * GIB);
    common::seed_grow_intent(&state_path, id.as_str(), "op-seeded-grow", GIB, 2 * GIB, 9);

    let provider = common::provider_from(&state_path, &world);
    let inspected = provider
        .inspect_volume(&id)
        .await
        .expect("the volume record is kept");
    assert_eq!(inspected.provisioned_bytes, GIB);
    assert_eq!(inspected.generation, 1);

    let durable = LvmState::load(&state_path).expect("durable state");
    assert!(durable.grow_intent(&id).is_none(), "the intent is dropped");
    // The LV is never shrunk as compensation.
    assert_eq!(
        world.lock().expect("world").lvs.get(&format!(
            "{}/{}",
            common::CLAIMED_VG,
            lv_name_for(&id)
        )),
        Some(&(2 * GIB))
    );
}

/// A durable delete authorized at the volume's current generation is
/// the later authority. Startup must not roll an older grow intent
/// forward (which would bump the generation and silently void the
/// delete, potentially presenting ZeroDiscard-erased data as live).
#[tokio::test]
async fn startup_delete_wins_over_a_prior_grow_intent() {
    let (state_path, world) = seeded_world();
    let id = volume_id("grow-then-delete");
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    common::seed_volume(&state_path, id.as_str(), common::CLAIMED_VG, GIB);
    // The dead incarnation extended the LV after journaling the grow but
    // before saving the volume record...
    common::add_lv(&world, common::CLAIMED_VG, &lv_name_for(&id), 2 * GIB);
    common::seed_grow_intent(&state_path, id.as_str(), "op-seeded-grow", GIB, 2 * GIB, 1);
    // ...and a delete was later authorized at the still-current
    // generation.
    common::seed_delete_intent(
        &state_path,
        id.as_str(),
        "op-seeded-delete",
        1,
        ErasurePolicy::ZeroDiscard,
    );

    let fixture = common::fixture_from_state(state_path, world);

    // The delete completed at generation 1: the grow never bumped it.
    let err = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect_err("the delete removed the volume");
    assert_eq!(err.code, ApiErrorCode::NotFound);
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(
        durable.volume(&id).is_none(),
        "the delete removed the record"
    );
    assert!(
        durable.grow_intent(&id).is_none(),
        "the superseded grow intent is dropped"
    );
    assert!(durable.delete_intent(&id).is_none());
    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path),
        "startup ran the recorded delete"
    );
    let programs = fixture.runner.programs();
    assert!(
        programs.iter().any(|program| program == "blkdiscard"),
        "{programs:?}"
    );
    assert!(
        programs.iter().any(|program| program == "lvremove"),
        "{programs:?}"
    );
}

// -- Delete -----------------------------------------------------------------

#[tokio::test]
async fn delete_intent_save_failure_leaves_the_volume_untouched() {
    let fixture = fixture();
    let volume = "delete-intent-fail";
    let created = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("create");
    let id = created.volume_id.clone();
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    let invocations_before = fixture.runner.invocations().len();

    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterTmpWrite,
        0,
    );
    let err = fixture
        .provider
        .delete_volume(
            &id,
            &delete_request_with(
                "op-delete-intent-fail",
                created.generation,
                ErasurePolicy::ZeroDiscard,
            ),
        )
        .await
        .expect_err("the delete intent save fails");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("injected state save failure"), "{err}");

    assert!(
        !fixture.runner.invocations()[invocations_before..]
            .iter()
            .any(|invocation| {
                invocation.program == "blkdiscard" || invocation.program == "lvremove"
            }),
        "a failed intent save must run no destructive command"
    );
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path),
        "the LV is untouched"
    );
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_some());
    assert!(durable.delete_intent(&id).is_none());

    let inspected = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect("the volume is still served");
    assert_eq!(inspected.state, VolumeLifecycle::Ready);
    assert_eq!(inspected.generation, 1);

    // The retry completes with a fresh operation id.
    fixture
        .provider
        .delete_volume(
            &id,
            &delete_request_with(
                "op-delete-intent-fail-retry",
                created.generation,
                ErasurePolicy::ZeroDiscard,
            ),
        )
        .await
        .expect("the retry completes");
    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path)
    );
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_none());
    assert!(durable.delete_intent(&id).is_none());
    let err = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect_err("the volume is gone");
    assert_eq!(err.code, ApiErrorCode::NotFound);
}

/// A failed destructive delete command must not discard the durable
/// authorization: the intent keeps fencing attach/create-replay/grow/
/// move, and a retry or restart completes it.
///
/// The crash window here is a command failure (a failed `blkdiscard`)
/// with a healthy durable intent save: the step may have partially
/// erased the LV, so reporting the volume as fully usable would be
/// dishonest.
#[tokio::test]
async fn delete_command_failure_keeps_the_pending_authorization_and_fences_new_authority() {
    let fixture = fixture();
    let volume = "delete-command-fail";
    let created = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("create");
    let id = created.volume_id.clone();
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    let invocations_before = fixture.runner.invocations().len();

    fixture.world.lock().expect("world").fail_blkdiscard = true;
    let err = fixture
        .provider
        .delete_volume(
            &id,
            &delete_request_with(
                "op-delete-command-fail",
                created.generation,
                ErasurePolicy::ZeroDiscard,
            ),
        )
        .await
        .expect_err("the failed discard surfaces");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(
        err.detail.contains("remains authorized and pending"),
        "the failed command keeps the authorization: {err}"
    );

    // The durable file still holds both the volume and the matching
    // authorization; the destructive step never reached lvremove.
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_some());
    let intent = durable
        .delete_intent(&id)
        .expect("the pending delete stays authorized");
    assert_eq!(intent.expected_generation, created.generation);
    assert_eq!(intent.erasure_policy, ErasurePolicy::ZeroDiscard);
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path),
        "a failed discard must not remove the LV"
    );
    assert!(
        !fixture.runner.invocations()[invocations_before..]
            .iter()
            .any(|invocation| invocation.program == "lvremove"),
        "the failed discard never proceeds to lvremove"
    );

    // The pending delete is a fence, not a bookkeeping note: a new
    // attachment is refused until the delete is completed.
    let attach_err = fixture
        .provider
        .attach_volume(
            &id,
            &fixture_attach_request(volume, "att-delete-command-fail", created.generation),
        )
        .await
        .expect_err("attach during the pending delete");
    assert_eq!(attach_err.code, ApiErrorCode::InvalidState, "{attach_err}");
    assert!(
        attach_err.detail.contains("durable pending delete"),
        "{attach_err}"
    );

    // A retry with a fresh operation id (the same generation/policy) is
    // the completion path.
    fixture.world.lock().expect("world").fail_blkdiscard = false;
    fixture
        .provider
        .delete_volume(
            &id,
            &delete_request_with(
                "op-delete-command-fail-retry",
                created.generation,
                ErasurePolicy::ZeroDiscard,
            ),
        )
        .await
        .expect("the retry completes the authorized delete");
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_none());
    assert!(durable.delete_intent(&id).is_none());
    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path)
    );
}

/// A delete-intent save failure whose rename landed: the durable intent
/// is present even though this call ran no destructive command. Memory
/// must be re-inserted to match it (the fence survives), and the
/// re-insertion must survive a *later* unrelated save rather than being
/// silently overwritten.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one crash-window/fence/later-save/restart narrative
async fn delete_intent_save_failure_after_rename_keeps_the_fence_and_completes_on_restart() {
    let fixture = fixture();
    let volume = "delete-intent-rename";
    let created = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("create");
    let id = created.volume_id.clone();
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    let invocations_before = fixture.runner.invocations().len();

    // The delete's FIRST save is the durable intent. The rename lands
    // before the directory-fsync boundary fails, so the durable file
    // authorizes a delete that this call never started.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterRenameBeforeDirFsync,
        0,
    );
    let err = fixture
        .provider
        .delete_volume(
            &id,
            &delete_request_with(
                "op-delete-intent-rename",
                created.generation,
                ErasurePolicy::ZeroDiscard,
            ),
        )
        .await
        .expect_err("the intent save fails after its rename");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(
        err.detail.contains("pending delete still fences"),
        "the error names the surviving fence: {err}"
    );
    assert!(
        !fixture.runner.invocations()[invocations_before..]
            .iter()
            .any(|invocation| {
                invocation.program == "blkdiscard" || invocation.program == "lvremove"
            }),
        "a failed intent save runs no destructive command"
    );

    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_some());
    let intent = durable
        .delete_intent(&id)
        .expect("the landed rename made the intent durable");
    assert_eq!(intent.expected_generation, created.generation);
    assert_eq!(intent.erasure_policy, ErasurePolicy::ZeroDiscard);
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path),
        "the LV is untouched"
    );

    let attach_err = fixture
        .provider
        .attach_volume(
            &id,
            &fixture_attach_request(volume, "att-delete-intent-rename", created.generation),
        )
        .await
        .expect_err("attach during the re-inserted pending delete");
    assert_eq!(attach_err.code, ApiErrorCode::InvalidState, "{attach_err}");
    assert!(
        attach_err.detail.contains("durable pending delete"),
        "{attach_err}"
    );

    // A later, unrelated create performs a REAL successful save. If the
    // in-memory re-insertion were missing, that save would overwrite the
    // durable file and erase the pending authorization.
    let second = fixture
        .provider
        .create_volume(&fixture_create_request("delete-intent-rename-second", GIB))
        .await
        .expect("an unrelated create still saves");
    let durable = LvmState::load(&fixture.state_path).expect("durable state after the save");
    assert!(
        durable.delete_intent(&id).is_some(),
        "the in-memory re-inserted intent survived a later save"
    );
    assert!(durable.volume(&id).is_some());
    assert!(durable.volume(&second.volume_id).is_some());

    // Restart observes the durable authorization and completes it; the
    // unrelated volume is left intact.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let err = restarted
        .inspect_volume(&id)
        .await
        .expect_err("startup completed the authorized delete");
    assert_eq!(err.code, ApiErrorCode::NotFound);
    let durable = LvmState::load(&fixture.state_path).expect("durable after restart");
    assert!(durable.volume(&id).is_none());
    assert!(durable.delete_intent(&id).is_none());
    assert!(durable.volume(&second.volume_id).is_some());
    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path)
    );
    restarted
        .inspect_volume(&second.volume_id)
        .await
        .expect("the unrelated volume is intact");
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one crash-window/authority-fence/retry narrative
async fn delete_final_save_failure_blocks_new_authority_and_completes_on_retry() {
    let fixture = fixture();
    let volume = "delete-final-fail";
    let created = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("create");
    let id = created.volume_id.clone();
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));

    // `skip = 1` passes the delete-intent save and fails the final
    // removal save before its rename: the durable intent survives while
    // the LV may already be gone.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterTmpWrite,
        1,
    );
    let err = fixture
        .provider
        .delete_volume(
            &id,
            &delete_request_with(
                "op-delete-final-fail",
                created.generation,
                ErasurePolicy::Retain,
            ),
        )
        .await
        .expect_err("the final delete save fails");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("injected state save failure"), "{err}");
    assert!(err.detail.contains("durable delete intent"), "{err}");

    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path),
        "the LV was already removed"
    );
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_some());
    let intent = durable
        .delete_intent(&id)
        .expect("the durable delete intent");
    assert_eq!(intent.expected_generation, 1);
    assert_eq!(intent.erasure_policy, ErasurePolicy::Retain);

    // The contract restores the durable shape in memory: inspect still
    // answers, while new authority is refused.
    fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect("the restored durable shape is served");

    let attach_err = fixture
        .provider
        .attach_volume(&id, &fixture_attach_request(volume, "att-pending", 1))
        .await
        .expect_err("attach during a pending delete");
    assert_eq!(attach_err.code, ApiErrorCode::InvalidState, "{attach_err}");
    assert!(
        attach_err.detail.contains("durable pending delete"),
        "{attach_err}"
    );

    let grow_err = fixture
        .provider
        .grow_volume(&id, &fixture_grow_request(volume, 2 * GIB, 1))
        .await
        .expect_err("grow during a pending delete");
    assert_eq!(grow_err.code, ApiErrorCode::InvalidState, "{grow_err}");
    assert!(
        grow_err.detail.contains("durable pending delete"),
        "{grow_err}"
    );

    // An idempotent create replay must not report the doomed volume as
    // live either: it is the same authority family as attach/grow/move.
    let create_err = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect_err("create replay during a pending delete");
    assert_eq!(create_err.code, ApiErrorCode::InvalidState, "{create_err}");
    assert!(
        create_err.detail.contains("durable pending delete"),
        "{create_err}"
    );

    let move_req = MoveVolumeBackingRequest {
        api_version: volvisor_types::API_VERSION.to_owned(),
        operation_id: OperationId::new("op-move-pending-delete").expect("valid operation id"),
        target_pool_id: "/dev/pv-b".to_owned(),
        expected_generation: 1,
        max_copy_bytes_per_sec: None,
    };
    let move_err = fixture
        .provider
        .move_volume_backing(&id, &move_req)
        .await
        .expect_err("move during a pending delete");
    assert_eq!(move_err.code, ApiErrorCode::InvalidState, "{move_err}");
    assert!(
        move_err.detail.contains("durable pending delete"),
        "{move_err}"
    );

    // The retry is the completion path (the delete is allowed to retry).
    fixture
        .provider
        .delete_volume(
            &id,
            &delete_request_with(
                "op-delete-final-fail-retry",
                created.generation,
                ErasurePolicy::Retain,
            ),
        )
        .await
        .expect("the retry completes the delete");
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_none());
    assert!(durable.delete_intent(&id).is_none());
    let err = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect_err("the deleted volume is gone");
    assert_eq!(err.code, ApiErrorCode::NotFound);
}

#[tokio::test]
async fn delete_final_save_failure_is_completed_at_startup() {
    let fixture = fixture();
    let volume = "delete-startup-complete";
    let created = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("create");
    let id = created.volume_id.clone();

    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterTmpWrite,
        1,
    );
    fixture
        .provider
        .delete_volume(
            &id,
            &delete_request_with(
                "op-delete-startup-complete",
                created.generation,
                ErasurePolicy::Retain,
            ),
        )
        .await
        .expect_err("the final delete save fails");

    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let err = restarted
        .inspect_volume(&id)
        .await
        .expect_err("recovery completed the delete");
    assert_eq!(err.code, ApiErrorCode::NotFound);

    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_none(), "the record is removed");
    assert!(durable.delete_intent(&id).is_none());
    assert!(
        !durable
            .volumes()
            .values()
            .any(|stored| stored.runtime.state == VolumeLifecycle::Failed),
        "recovery removes the record; it never marks it Failed"
    );
    let report = restarted.reconcile_report().expect("report");
    assert!(report.missing_volumes.is_empty(), "{report:?}");
}

#[tokio::test]
async fn delete_startup_completion_runs_the_recorded_policy() {
    let (state_path, world) = seeded_world();
    let id = volume_id("delete-startup-policy");
    common::seed_volume(&state_path, id.as_str(), common::CLAIMED_VG, GIB);
    common::add_lv(&world, common::CLAIMED_VG, &lv_name_for(&id), GIB);
    common::seed_delete_intent(
        &state_path,
        id.as_str(),
        "op-seeded-delete",
        1,
        ErasurePolicy::ZeroDiscard,
    );

    let fixture = common::fixture_from_state(state_path, world);
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));

    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path),
        "startup ran the recorded lvremove"
    );
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_none());
    assert!(durable.delete_intent(&id).is_none());

    let invocations = fixture.runner.invocations();
    let device = format!("/dev/{lv_path}");
    let discard_at = invocations
        .iter()
        .position(|invocation| {
            invocation.program == "blkdiscard" && invocation.args.iter().any(|arg| arg == &device)
        })
        .expect("the recorded ZeroDiscard blkdiscard ran");
    let remove_at = invocations
        .iter()
        .position(|invocation| {
            invocation.program == "lvremove" && invocation.args.iter().any(|arg| arg == &lv_path)
        })
        .expect("the recorded lvremove ran");
    assert!(
        discard_at < remove_at,
        "the recorded policy discards before removing"
    );
}

/// A destructive command that fails during startup completion fails the
/// startup and leaves both the LV and the durable authorization in
/// place. Startup never serves a volume whose authorized delete is
/// pending, and never rewrites the state as though the delete landed.
#[test]
fn startup_delete_command_failure_fails_startup_and_leaves_the_lv() {
    let (state_path, world) = seeded_world();
    let id = volume_id("delete-startup-command-fails");
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));
    common::seed_volume(&state_path, id.as_str(), common::CLAIMED_VG, GIB);
    common::add_lv(&world, common::CLAIMED_VG, &lv_name_for(&id), GIB);
    common::seed_delete_intent(
        &state_path,
        id.as_str(),
        "op-seeded-delete",
        1,
        ErasurePolicy::Retain,
    );
    world
        .lock()
        .expect("world")
        .fail_lvremove_for
        .push(lv_path.clone());
    let state_before = fs::read(&state_path).expect("state snapshot");

    let err = startup_error(&state_path, &world);
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(err.detail.contains("could not complete"), "{err}");
    assert!(
        err.detail.contains("authorized delete is pending"),
        "the failure names the pending authorization: {err}"
    );

    // The failed delete leaves the LV and the durable change untouched.
    assert!(
        world.lock().expect("world").lvs.contains_key(&lv_path),
        "the failed delete did not remove the LV"
    );
    assert_eq!(
        fs::read(&state_path).expect("state after the failed startup"),
        state_before,
        "startup does not rewrite state when the authorized delete fails"
    );
    let durable = LvmState::load(&state_path).expect("durable state");
    assert!(durable.volume(&id).is_some());
    assert!(
        durable.delete_intent(&id).is_some(),
        "the authorization survives for the next retry/restart"
    );
}

#[tokio::test]
async fn delete_intent_with_a_stale_generation_is_dropped_without_deleting() {
    let (state_path, world) = seeded_world();
    let id = volume_id("delete-stale-generation");
    common::seed_volume(&state_path, id.as_str(), common::CLAIMED_VG, GIB);
    common::add_lv(&world, common::CLAIMED_VG, &lv_name_for(&id), GIB);
    common::seed_delete_intent(
        &state_path,
        id.as_str(),
        "op-seeded-delete",
        9,
        ErasurePolicy::ZeroDiscard,
    );

    let fixture = common::fixture_from_state(state_path, world);
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));

    let inspected = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect("the stale-authorization volume is kept");
    assert_eq!(inspected.generation, 1);
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path),
        "a stale generation never deletes"
    );
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_some());
    assert!(
        durable.delete_intent(&id).is_none(),
        "the intent is dropped"
    );
    assert!(
        !fixture
            .runner
            .invocations()
            .iter()
            .any(|invocation| invocation.program == "lvremove"),
        "no lvremove ran for a stale authorization"
    );
}

#[tokio::test]
async fn delete_intent_with_an_attached_volume_fails_startup() {
    let fixture = fixture();
    let volume = "delete-attached-guard";
    let created = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("create");
    let attached = fixture
        .provider
        .attach_volume(
            &created.volume_id,
            &fixture_attach_request(volume, "att-delete-guard", 1),
        )
        .await
        .expect("attach");

    // A delete intent must never coexist with an attachment; insert one
    // directly at the current generation to pin the startup guard.
    {
        let mut state = LvmState::load(&fixture.state_path).expect("load state");
        state.insert_delete_intent(
            created.volume_id.clone(),
            volvisor_lvm::state::DeleteIntent {
                operation_id: OperationId::new("op-seeded-attached-delete")
                    .expect("valid operation id"),
                expected_generation: attached.volume_generation,
                erasure_policy: ErasurePolicy::Retain,
            },
        );
        state.save(&fixture.state_path).expect("save the intent");
    }

    let err = startup_error(&fixture.state_path, &fixture.world);
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(
        err.detail.contains("attachment") || err.detail.contains("attached"),
        "{err}"
    );
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&format!(
                "{}/{}",
                common::CLAIMED_VG,
                lv_name_for(&created.volume_id)
            )),
        "startup refused instead of removing the attached LV"
    );
}

#[tokio::test]
async fn delete_post_rename_failure_is_the_carve_out() {
    let fixture = fixture();
    let volume = "delete-post-rename";
    let created = fixture
        .provider
        .create_volume(&fixture_create_request(volume, GIB))
        .await
        .expect("create");
    let id = created.volume_id.clone();
    let lv_path = format!("{}/{}", common::CLAIMED_VG, lv_name_for(&id));

    // `skip = 1` passes the delete-intent save; the final save's rename
    // lands before the directory-fsync boundary fails, so the NEW
    // durable state (no volume, no intent) is the commit.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterRenameBeforeDirFsync,
        1,
    );
    let err = fixture
        .provider
        .delete_volume(
            &id,
            &delete_request_with(
                "op-delete-post-rename",
                created.generation,
                ErasurePolicy::Retain,
            ),
        )
        .await
        .expect_err("the directory-fsync boundary fails");
    assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    assert!(
        err.detail
            .contains("durable state already records the deletion"),
        "{err}"
    );

    assert!(
        !fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&lv_path)
    );
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_none());
    assert!(durable.delete_intent(&id).is_none());
    let err = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect_err("the in-memory removal was kept");
    assert_eq!(err.code, ApiErrorCode::NotFound);

    // A restart is consistent with the committed deletion.
    let restarted = common::provider_from(&fixture.state_path, &fixture.world);
    let err = restarted
        .inspect_volume(&id)
        .await
        .expect_err("the restart agrees the volume is gone");
    assert_eq!(err.code, ApiErrorCode::NotFound);
    let report = restarted.reconcile_report().expect("report");
    assert!(report.missing_volumes.is_empty(), "{report:?}");
    assert!(report.foreign_lvs.is_empty(), "{report:?}");
}

// ---------------------------------------------------------------------------
// Phase E E1: delete releases the provider state lock
// ---------------------------------------------------------------------------

/// Wait until a one-shot program gate has recorded entry.
async fn wait_for_gate_entered(gate: &Arc<OneShotProgramGate>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !gate.has_entered() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the gated LVM command must start"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A provider whose fake `lvremove` parks on the returned gate until the
/// test releases it, plus the durable state path for post-conditions.
fn gated_delete_provider() -> (
    Arc<LvmProvider>,
    Arc<OneShotProgramGate>,
    Arc<FakeRunner>,
    std::path::PathBuf,
) {
    let state_path = common::leak_tempdir().join("state.json");
    common::seed_claimed_state(&state_path);
    let world = Arc::new(Mutex::new(common::FakeLvm::with_claimed_pool()));
    let gate = Arc::new(OneShotProgramGate::default());
    let runner = {
        let world = Arc::clone(&world);
        let gate = Arc::clone(&gate);
        Arc::new(FakeRunner::with_closure(move |program, args| {
            if program == "lvremove" && gate.try_fire() {
                gate.entered();
                gate.wait_release();
            }
            let mut world = world.lock().ok()?;
            common::script(&mut world, program, args)
        }))
    };
    let provider = Arc::new(
        LvmProvider::new(
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
            state_path.clone(),
            common::leak_tempdir(),
            common::VG_PREFIX.to_owned(),
            common::AUTH_TOKEN.to_owned(),
        )
        .expect("provider construction"),
    );
    (provider, gate, runner, state_path)
}

/// Phase E E1 (#62): a delete must release the provider state lock across
/// `blkdiscard`/`lvremove`.
///
/// While `lvremove` is parked, another volume's attach must complete, the
/// deleting volume must stay fenced by its durable intent, and a second
/// delete of the same volume must be refused by the in-memory drive guard
/// rather than racing the destructive body.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn delete_releases_the_state_lock_across_lvremove() {
    let (provider, gate, _runner, state_path) = gated_delete_provider();
    let _release = OneShotGateRelease(Arc::clone(&gate));

    let doomed = provider
        .create_volume(&fixture_create_request("delete-lock", GIB))
        .await
        .expect("create doomed volume");
    let other = provider
        .create_volume(&fixture_create_request("delete-other", GIB))
        .await
        .expect("create other volume");

    gate.arm();
    let delete_request =
        delete_request_with("delete-lock-op", doomed.generation, ErasurePolicy::Retain);
    let delete = {
        let provider = Arc::clone(&provider);
        let volume_id = doomed.volume_id.clone();
        tokio::spawn(async move { provider.delete_volume(&volume_id, &delete_request).await })
    };
    wait_for_gate_entered(&gate).await;

    // A different volume's write must not queue behind this delete's
    // ~LVM CLI call: the state lock is released for the command body.
    let attach_other = {
        let provider = Arc::clone(&provider);
        let volume_id = other.volume_id.clone();
        tokio::spawn(async move {
            provider
                .attach_volume(
                    &volume_id,
                    &fixture_attach_request("delete-other", "delete-other-att", 1),
                )
                .await
        })
    };
    let attached = tokio::time::timeout(IN_FLIGHT_TIMEOUT, attach_other)
        .await
        .expect("another volume's attach must not wait behind lvremove")
        .expect("attach task join")
        .expect("attach other volume");
    assert_eq!(attached.volume_generation, 2);

    // The deleting volume stays fenced by its durable delete intent.
    let attach_doomed = {
        let provider = Arc::clone(&provider);
        let volume_id = doomed.volume_id.clone();
        tokio::spawn(async move {
            provider
                .attach_volume(
                    &volume_id,
                    &fixture_attach_request("delete-lock", "delete-lock-att", 1),
                )
                .await
        })
    };
    let err = tokio::time::timeout(IN_FLIGHT_TIMEOUT, attach_doomed)
        .await
        .expect("attach during a delete must not block on the state lock")
        .expect("attach task join")
        .expect_err("attach during a delete must be refused");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(err.detail.contains("durable pending delete"), "{err}");

    // A second delete of the same volume is refused immediately, before it
    // can run a second destructive body against the same intent.
    let second = delete_request_with(
        "delete-lock-op-second",
        doomed.generation,
        ErasurePolicy::Retain,
    );
    let err = tokio::time::timeout(
        IN_FLIGHT_TIMEOUT,
        provider.delete_volume(&doomed.volume_id, &second),
    )
    .await
    .expect("a second delete must not block on the state lock")
    .expect_err("a second delete of the same volume must be refused");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(
        err.detail.contains("already has a delete in progress"),
        "{err}"
    );

    gate.release();
    delete
        .await
        .expect("delete task join")
        .expect("delete completes after the gate releases");

    // The final atomic save removed the volume and every stale intent.
    let durable = LvmState::load(&state_path).expect("durable state after delete");
    assert!(durable.volume(&doomed.volume_id).is_none());
    assert!(durable.delete_intent(&doomed.volume_id).is_none());
    assert!(durable.volume(&other.volume_id).is_some());
}
