//! Real-LVM integration test on a loop device (feature + env gated).
//!
//! Compile gate: `--features integration-lvm`. Runtime gate: the test
//! skips itself unless `VOLVISOR_TEST_LVM=1` is set (it needs root,
//! `losetup` and the `lvm2` toolchain, and it performs real, destructive
//! LVM operations on a private loop device).
//!
//! One deliberate deviation from the unit-test fixtures: the loop device
//! is **not** claimed through `claim_device`, because loop devices are
//! excluded from discovery by design (they expose no WWN/serial, so no
//! stable hardware identity can be established — using them as claimable
//! identities would violate SPEC-0002 section 3). The test therefore seeds
//! the claim record after running `pvcreate`/`vgcreate` itself, then
//! drives the full create → attach → grow → detach → delete lifecycle
//! through the provider with [`RealRunner`].
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![cfg(feature = "integration-lvm")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use volvisor_lvm::provider::{LvmProvider, lv_name_for};
use volvisor_lvm::state::{DeviceEntry, LvmState};
use volvisor_lvm::{CommandRunner, RealRunner};
use volvisor_provider::VolumeProvider;
use volvisor_provider::conformance::{
    fixture_attach_request, fixture_create_request, fixture_delete_request, fixture_detach_request,
    fixture_grow_request,
};
use volvisor_types::request::{
    AttachVolumeRequest, AttachVolumeResponse, GrowGuestNotification, InspectVolumeResponse,
};
use volvisor_types::{
    ApiError, ApiErrorCode, AttachmentState, DeviceId, DeviceRole, Frontend, VolumeId,
    VolumeLifecycle,
};

/// An `INTERNAL` error (no `ApiError::internal` constructor exists).
fn internal(detail: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorCode::Internal, detail)
}

/// The volume identity driven through the lifecycle.
const VOLUME: &str = "itest-vol";
/// Backing-file size for the loop device.
const BACKING_BYTES: u64 = 256 << 20;
/// Volume size used in the lifecycle.
const VOLUME_BYTES: u64 = 64 << 20;
/// Grown volume size.
const GROWN_BYTES: u64 = 128 << 20;
/// A non-extent-aligned size (default LVM extent: 4 MiB).
const UNALIGNED_BYTES: u64 = 1 << 20;
/// Default LVM physical extent size.
const EXTENT_BYTES: u64 = 4 << 20;

fn volume_id() -> VolumeId {
    VolumeId::new(VOLUME).expect("valid volume id")
}

/// Removes the VG/PV and detaches the loop device on every path, including
/// assertion failures and panics.
struct LoopGuard {
    runner: RealRunner,
    loop_device: String,
    vg_name: String,
}

impl Drop for LoopGuard {
    fn drop(&mut self) {
        if let Err(e) = self
            .runner
            .run("vgremove", &["--yes", "--force", &self.vg_name])
        {
            eprintln!("cleanup: vgremove failed: {e}");
        }
        if let Err(e) = self.runner.run("pvremove", &["--yes", &self.loop_device]) {
            eprintln!("cleanup: pvremove failed: {e}");
        }
        if let Err(e) = self.runner.run("losetup", &["-d", &self.loop_device]) {
            eprintln!("cleanup: losetup -d failed: {e}");
        }
    }
}

/// Create a 256-MiB backing file and attach a loop device to it.
fn attach_loop_device(backing: &std::path::Path) -> Result<String, ApiError> {
    let file = std::fs::File::create(backing)
        .map_err(|e| internal(format!("create backing file: {e}")))?;
    file.set_len(BACKING_BYTES)
        .map_err(|e| internal(format!("truncate backing file: {e}")))?;
    drop(file);
    let output = Command::new("losetup")
        .args(["-f", "--show"])
        .arg(backing)
        .output()
        .map_err(|e| internal(format!("execute losetup: {e}")))?;
    if !output.status.success() {
        return Err(internal(format!(
            "losetup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Phase: create a volume and assert the honest post-create contract.
async fn phase_create(provider: &LvmProvider, volume_bytes: u64) -> InspectVolumeResponse {
    let created = provider
        .create_volume(&fixture_create_request(VOLUME, volume_bytes))
        .await
        .expect("create");
    assert_eq!(created.generation, 1);
    assert_eq!(created.state, VolumeLifecycle::Ready);
    assert_eq!(created.provisioned_bytes, volume_bytes);
    created
}

/// Phase: attach (single-writer) and assert the prepared frontend handle.
async fn phase_attach(provider: &LvmProvider) -> AttachVolumeResponse {
    let request: AttachVolumeRequest = fixture_attach_request(VOLUME, "itest-att", 1);
    let attached = provider
        .attach_volume(&volume_id(), &request)
        .await
        .expect("attach");
    assert_eq!(attached.attachment_generation, 1);
    assert_eq!(attached.volume_generation, 2);
    assert_eq!(
        attached.frontend,
        Frontend::VirtioBlk {
            host_device_path: format!("/dev/volvisorit-itest/{}", lv_name_for(&volume_id()))
        }
    );
    assert_eq!(attached.state, AttachmentState::Prepared);

    // A second writer is rejected (single-writer fencing).
    let err = provider
        .attach_volume(
            &volume_id(),
            &fixture_attach_request(VOLUME, "itest-att-2", 2),
        )
        .await
        .expect_err("second writer must be rejected");
    assert_eq!(err.code, ApiErrorCode::WriterAlreadyActive);
    attached
}

/// Phase: grow while attached; the notification honestly stays retryable.
async fn phase_grow(provider: &LvmProvider, grown_bytes: u64, generation: u64) {
    let grown = provider
        .grow_volume(
            &volume_id(),
            &fixture_grow_request(VOLUME, grown_bytes, generation),
        )
        .await
        .expect("grow");
    assert!(grown.backing_resized);
    assert_eq!(grown.effective_size_bytes, grown_bytes);
    assert_eq!(
        grown.guest_notification_status,
        GrowGuestNotification::RetryRequired
    );
}

/// Phase: detach with a drain proof; the volume returns to Ready.
async fn phase_detach(
    provider: &LvmProvider,
    attached: &AttachVolumeResponse,
) -> InspectVolumeResponse {
    let detached = provider
        .detach_volume(
            &volume_id(),
            &attached.attachment_id,
            &fixture_detach_request("itest-att", attached.attachment_generation),
        )
        .await
        .expect("detach");
    assert_eq!(detached.state, VolumeLifecycle::Ready);
    // Attach advanced the generation to `attached.volume_generation`; the
    // grow and the detach each advanced it once more.
    assert_eq!(detached.generation, attached.volume_generation + 2);
    detached
}

/// Phase: delete, then verify the volume is gone.
async fn phase_delete(provider: &LvmProvider, generation: u64) {
    provider
        .delete_volume(&volume_id(), &fixture_delete_request(VOLUME, generation))
        .await
        .expect("delete");
    let err = provider
        .inspect_volume(&volume_id())
        .await
        .expect_err("deleted volume is gone");
    assert_eq!(err.code, ApiErrorCode::NotFound);
}

/// A loop-backed pool under test: the provider, the shared real runner,
/// the state-file path and the cleanup guard (dropped last, on every
/// path).
struct LoopPool {
    /// The provider over the seeded claim record.
    provider: Arc<LvmProvider>,
    /// The real runner shared with the provider (for direct LVM access).
    runner: Arc<dyn CommandRunner>,
    /// The provider state file (restarts reuse it).
    state_path: PathBuf,
    /// vgremove/pvremove/losetup cleanup on drop.
    _guard: LoopGuard,
}

/// Create a loop device, a VG on it and a provider over a seeded claim
/// record. The pool is set up outside `claim_device` (loop devices are
/// excluded from discovery by design; see the module documentation).
fn setup_loop_pool(dir: &std::path::Path, vg_name: &str) -> Result<LoopPool, ApiError> {
    let backing = dir.join(format!("{vg_name}.img"));
    let loop_device = attach_loop_device(&backing)?;
    let runner: Arc<dyn CommandRunner> = Arc::new(RealRunner::default());
    for (program, args) in [
        ("pvcreate", vec!["--yes", loop_device.as_str()]),
        ("vgcreate", vec!["--yes", vg_name, loop_device.as_str()]),
    ] {
        let output = runner.run(program, &args)?;
        if !output.success {
            return Err(internal(format!(
                "{program} failed: {}",
                output.stderr_excerpt()
            )));
        }
    }
    let state_path = dir.join(format!("{vg_name}-state.json"));
    let mut state = LvmState::default();
    state.insert_device(
        DeviceId::new("dev-itestloop0000000000000000000000000").expect("device id"),
        DeviceEntry {
            stable_identity: format!("loop:{loop_device}"),
            path: loop_device.clone(),
            vg_name: vg_name.to_owned(),
            role: DeviceRole::NativePool,
            owner_generation: 1,
        },
    );
    state.save(&state_path)?;
    let provider = LvmProvider::new(
        Arc::clone(&runner),
        state_path.clone(),
        PathBuf::from("/"),
        "volvisorit".to_owned(),
        "integration-token".to_owned(),
    )
    .map(Arc::new)?;
    Ok(LoopPool {
        provider,
        runner,
        state_path,
        _guard: LoopGuard {
            runner: RealRunner::default(),
            loop_device,
            vg_name: vg_name.to_owned(),
        },
    })
}

/// Runtime gate for the real-LVM tests.
fn lvm_tests_enabled() -> bool {
    std::env::var("VOLVISOR_TEST_LVM").ok().as_deref() == Some("1")
}

#[tokio::test]
async fn lvm_lifecycle_on_loop_device() -> Result<(), ApiError> {
    if !lvm_tests_enabled() {
        eprintln!(
            "skipping real-LVM integration test: set VOLVISOR_TEST_LVM=1 (requires root, \
             losetup and lvm2) to run it"
        );
        return Ok(());
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = setup_loop_pool(dir.path(), "volvisorit-itest")?;
    let provider = &pool.provider;

    // Discovery is read-only and honestly excludes the loop device (no
    // stable identity).
    let discovered = provider.discover()?.devices;
    assert!(
        !discovered
            .iter()
            .any(|device| device.capacity_bytes == BACKING_BYTES)
    );

    // The full lifecycle against real LVM.
    phase_create(provider, VOLUME_BYTES).await;
    let attached = phase_attach(provider).await;
    phase_grow(provider, GROWN_BYTES, attached.volume_generation).await;
    let detached = phase_detach(provider, &attached).await;
    phase_delete(provider, detached.generation).await;

    // The LV really is gone from LVM's own report.
    let lvs = pool.runner.run(
        "lvs",
        &["--reportformat", "json", "--units", "b", "--nosuffix"],
    )?;
    assert!(lvs.success);
    assert!(!lvs.stdout.contains(VOLUME));

    // Reconciliation over the now-empty pool reports nothing.
    let report = provider.reconcile_report()?;
    assert!(report.missing_volumes.is_empty());
    assert!(report.foreign_lvs.is_empty());

    Ok(())
}

#[tokio::test]
async fn lvm_non_aligned_create_rounds_up_and_absent_lv_delete_succeeds() -> Result<(), ApiError> {
    if !lvm_tests_enabled() {
        eprintln!(
            "skipping real-LVM integration test: set VOLVISOR_TEST_LVM=1 (requires root, \
             losetup and lvm2) to run it"
        );
        return Ok(());
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = setup_loop_pool(dir.path(), "volvisorit-unaligned")?;
    let volume = VolumeId::new("itest-unaligned").expect("valid volume id");

    // A non-extent-aligned create (1 MiB) succeeds: thick LVM rounds the
    // LV up to whole extents and the response reports that effective size
    // honestly instead of failing the valid request.
    let created = pool
        .provider
        .create_volume(&fixture_create_request("itest-unaligned", UNALIGNED_BYTES))
        .await
        .expect("non-aligned create against real LVM");
    assert!(created.provisioned_bytes >= UNALIGNED_BYTES);
    assert_eq!(
        created.provisioned_bytes % EXTENT_BYTES,
        0,
        "effective size is extent-aligned"
    );
    assert!(
        created.provisioned_bytes < UNALIGNED_BYTES + EXTENT_BYTES,
        "rounded up by at most one extent"
    );
    assert_eq!(created.allocated_bytes, created.provisioned_bytes);

    // Remove the LV behind the provider's back (a crash or operator
    // intervention), then restart: reconciliation marks the volume Failed.
    let lv_path = format!("volvisorit-unaligned/{}", lv_name_for(&volume));
    let removed = pool.runner.run("lvremove", &["--yes", &lv_path])?;
    assert!(removed.success, "manual lvremove for the test setup");
    let restarted = LvmProvider::new(
        Arc::clone(&pool.runner),
        pool.state_path.clone(),
        PathBuf::from("/"),
        "volvisorit".to_owned(),
        "integration-token".to_owned(),
    )?;
    let inspected = restarted
        .inspect_volume(&volume)
        .await
        .expect("inspect after restart");
    assert_eq!(inspected.state, VolumeLifecycle::Failed);

    // Delete of the Failed volume whose LV is absent must succeed (the
    // old code ran lvremove on the missing LV and failed forever).
    restarted
        .delete_volume(
            &volume,
            &fixture_delete_request("itest-unaligned", inspected.generation),
        )
        .await
        .expect("delete of a Failed volume without an LV");
    let err = restarted
        .inspect_volume(&volume)
        .await
        .expect_err("deleted volume is gone");
    assert_eq!(err.code, ApiErrorCode::NotFound);

    Ok(())
}
