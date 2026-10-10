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

use std::time::Duration;
use volvisor_lvm::provider::{LvmProvider, MoveTiming, lv_name_for};
use volvisor_lvm::state::{DeviceEntry, LvmState, MoveRecord};
use volvisor_lvm::{CommandRunner, RealRunner};
use volvisor_provider::VolumeProvider;
use volvisor_provider::conformance::{
    fixture_attach_request, fixture_create_request, fixture_delete_request, fixture_detach_request,
    fixture_grow_request,
};
use volvisor_types::request::{
    AttachVolumeRequest, AttachVolumeResponse, GrowGuestNotification, InspectVolumeResponse,
    MoveVolumeBackingRequest,
};
use volvisor_types::{
    ApiError, ApiErrorCode, AttachmentState, DeviceId, DeviceRole, Frontend,
    MoveVolumeBackingState, OperationId, VolumeId, VolumeLifecycle,
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
    attach_loop_device_sized(backing, BACKING_BYTES)
}

/// Create a `size`-byte backing file and attach a loop device to it.
fn attach_loop_device_sized(backing: &std::path::Path, size: u64) -> Result<String, ApiError> {
    let file = std::fs::File::create(backing)
        .map_err(|e| internal(format!("create backing file: {e}")))?;
    file.set_len(size)
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

    // These tests mutate the host's real LVM (and `pvmove --abort` is
    // host-wide): they serialize on the host lock.
    let _host = host_lock().await;

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
    assert_eq!(report.missing_volumes, Vec::<VolumeId>::new());
    assert_eq!(report.foreign_lvs, Vec::<String>::new());

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

    // These tests mutate the host's real LVM (and `pvmove --abort` is
    // host-wide): they serialize on the host lock.
    let _host = host_lock().await;

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

// ---------------------------------------------------------------------------
// The same-VG extent move (P6-C, ADR-0006 first slice part 2): real
// `pvmove` evacuation under concurrent guest I/O, the restart
// mid-move re-attach, and the honest abort outcomes against real
// LVM 2.03.16.
// ---------------------------------------------------------------------------

/// Backing-file size for each move-test loop device (source and
/// target): room for the volume plus LVM's own metadata extents (a
/// 2-GiB LV does not fit a 2-GiB PV).
const MOVE_BACKING_BYTES: u64 = 3 << 30;
/// The move tests' volume name (distinct from the lifecycle test's
/// volume: its post-delete absence assertion reads the host-wide
/// `lvs` report, so a shared name would make the concurrent LVs
/// look like a survivor).
const MOVE_VOLUME: &str = "itest-movevol";
/// The move-test volume: thick allocation means `pvmove` relocates
/// the full size, so a real sync outlives a short supervision window
/// (the tests that catch the mirror mid-flight rely on that).
const MOVE_VOLUME_BYTES: u64 = 2 << 30;
/// The concurrent-I/O test's volume (small enough to complete well
/// within its generous supervision window).
const MOVE_IO_VOLUME_BYTES: u64 = 512 << 20;
/// One region's size for the checksum discipline (MiB units below).
const REGION_MIB: u64 = 256;

/// The host-wide serialization lock: these tests run as root and
/// mutate the host's real LVM (loop devices, volume groups, and —
/// decisively — `pvmove --abort`, which LVM 2.03.16 applies to every
/// pvmove on the host, with no per-VG scope). Every integration test
/// holds it for its whole body.
static HOST: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

/// Take the host lock for this test's whole body.
async fn host_lock() -> tokio::sync::MutexGuard<'static, ()> {
    HOST.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// The move tests' volume identity.
fn move_volume_id() -> VolumeId {
    VolumeId::new(MOVE_VOLUME).expect("valid move volume id")
}

/// Removes the VG/PVs and detaches both loop devices on every path.
struct MoveLoopGuard {
    runner: RealRunner,
    loops: Vec<String>,
    vg_name: String,
}

impl Drop for MoveLoopGuard {
    fn drop(&mut self) {
        if let Err(e) = self
            .runner
            .run("vgremove", &["--yes", "--force", &self.vg_name])
        {
            eprintln!("cleanup: vgremove failed: {e}");
        }
        for loop_device in &self.loops {
            if let Err(e) = self.runner.run("pvremove", &["--yes", loop_device]) {
                eprintln!("cleanup: pvremove failed: {e}");
            }
            if let Err(e) = self.runner.run("losetup", &["-d", loop_device]) {
                eprintln!("cleanup: losetup -d failed: {e}");
            }
        }
    }
}

/// A two-PV loop-backed pool for the move tests: the claimed source
/// loop carries the VG alone at construction (so the volume's
/// initial placement is the source, deterministically), and the
/// companion target loop joins the VG when the test extends for the
/// move.
struct MovePool {
    /// The provider over the seeded claim record.
    provider: Arc<LvmProvider>,
    /// The real runner shared with the provider (direct LVM access).
    runner: Arc<dyn CommandRunner>,
    /// The provider state file (restarts reuse it).
    state_path: PathBuf,
    /// The claimed source PV (the first loop device).
    source_pv: String,
    /// The evacuation target PV (the second loop device).
    target_pv: String,
    /// The VG spanning both PVs (after the extend).
    vg_name: String,
    /// vgremove/pvremove/losetup cleanup on drop.
    _guard: MoveLoopGuard,
}

/// Create the two loop devices and the source-only VG, seed the
/// claim on the source, and build the provider with the given move
/// timing. The target PV is pvcreated but NOT yet in the VG — call
/// [`extend_for_move`] after creating the volume.
fn setup_move_pool(
    dir: &std::path::Path,
    vg_name: &str,
    timing: MoveTiming,
) -> Result<MovePool, ApiError> {
    let source =
        attach_loop_device_sized(&dir.join(format!("{vg_name}-src.img")), MOVE_BACKING_BYTES)?;
    let target =
        attach_loop_device_sized(&dir.join(format!("{vg_name}-dst.img")), MOVE_BACKING_BYTES)?;
    let runner: Arc<dyn CommandRunner> = Arc::new(RealRunner::default());
    for (program, args) in [
        ("pvcreate", vec!["--yes", source.as_str()]),
        ("pvcreate", vec!["--yes", target.as_str()]),
        ("vgcreate", vec!["--yes", vg_name, source.as_str()]),
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
            stable_identity: format!("loop:{source}"),
            path: source.clone(),
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
    .map(|provider| Arc::new(provider.with_move_timing(timing)))?;
    Ok(MovePool {
        provider,
        runner,
        state_path,
        source_pv: source.clone(),
        target_pv: target.clone(),
        vg_name: vg_name.to_owned(),
        _guard: MoveLoopGuard {
            runner: RealRunner::default(),
            loops: vec![source, target],
            vg_name: vg_name.to_owned(),
        },
    })
}

impl MovePool {
    /// Join the target PV to the VG (after the volume exists on the
    /// source — the deterministic initial placement).
    fn extend_for_move(&self) -> Result<(), ApiError> {
        let output = self
            .runner
            .run("vgextend", &["--yes", &self.vg_name, &self.target_pv])?;
        if !output.success {
            return Err(internal(format!(
                "vgextend failed: {}",
                output.stderr_excerpt()
            )));
        }
        Ok(())
    }
}

/// A move request for the companion target PV.
fn move_request(
    operation_id: &str,
    target: &str,
    expected_generation: u64,
) -> MoveVolumeBackingRequest {
    MoveVolumeBackingRequest {
        api_version: volvisor_types::API_VERSION.to_owned(),
        operation_id: OperationId::new(operation_id).expect("valid operation id"),
        target_pool_id: target.to_owned(),
        expected_generation,
        max_copy_bytes_per_sec: None,
    }
}

/// Run one operator command to success (dd/sha256sum/the out-of-band
/// `pvmove --abort`): the actor's tools, not the daemon's runner).
fn operator_ok(command: &mut Command) -> std::process::Output {
    let program = format!("{:?}", command.get_program());
    let output = command.output().expect("run the operator command");
    assert!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// One file's sha256 (hex).
fn checksum_of(path: &std::path::Path) -> String {
    let output = operator_ok(Command::new("sha256sum").arg(path));
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .expect("a sha256 line")
        .to_owned()
}

/// Write `REGION_MIB` of urandom into the LV at `offset_mib` and
/// return the written bytes' sha256 (hashed from the source copy —
/// the ground truth the read-backs compare against).
fn write_region(lv_device: &str, offset_mib: u64, scratch: &std::path::Path) -> String {
    let seed = scratch.join(format!("seed-{offset_mib}.bin"));
    operator_ok(Command::new("dd").args([
        "if=/dev/urandom",
        &format!("of={}", seed.display()),
        "bs=1M",
        &format!("count={REGION_MIB}"),
    ]));
    operator_ok(Command::new("dd").args([
        &format!("if={}", seed.display()),
        &format!("of={lv_device}"),
        "bs=1M",
        &format!("seek={offset_mib}"),
        &format!("count={REGION_MIB}"),
        "conv=notrunc",
    ]));
    checksum_of(&seed)
}

/// The LV region at `offset_mib`, read back through the device.
fn region_checksum(lv_device: &str, offset_mib: u64, scratch: &std::path::Path) -> String {
    let readback = scratch.join(format!("readback-{offset_mib}.bin"));
    operator_ok(Command::new("dd").args([
        &format!("if={lv_device}"),
        &format!("of={}", readback.display()),
        "bs=1M",
        &format!("skip={offset_mib}"),
        &format!("count={REGION_MIB}"),
    ]));
    checksum_of(&readback)
}

/// The LV's backing PVs from LVM's own report (the `lvs` devices
/// column — the placement truth).
fn lv_backing_pvs(pool: &MovePool) -> Result<Vec<String>, ApiError> {
    let lv = lv_name_for(&move_volume_id());
    let output = pool.runner.run(
        "lvs",
        &[
            "--reportformat",
            "json",
            "-o",
            "devices",
            &format!("{}/{}", pool.vg_name, lv),
        ],
    )?;
    assert!(
        output.success,
        "lvs devices query: {}",
        output.stderr_excerpt()
    );
    let value: serde_json::Value =
        serde_json::from_str(&output.stdout).map_err(|e| internal(format!("parse lvs: {e}")))?;
    let devices = value["report"][0]["lv"][0]["devices"]
        .as_str()
        .ok_or_else(|| internal("missing devices column"))?;
    Ok(devices
        .split(',')
        .map(|entry| entry.split('(').next().unwrap_or(entry).to_owned())
        .collect())
}

/// The volume's move record from the durable state file.
fn recorded_move(state_path: &std::path::Path) -> Option<MoveRecord> {
    LvmState::load(state_path)
        .expect("load state")
        .move_record(&move_volume_id())
        .cloned()
}

/// The volume's generation from the durable state file.
fn volume_generation(state_path: &std::path::Path) -> u64 {
    LvmState::load(state_path)
        .expect("load state")
        .volume(&move_volume_id())
        .map_or(0, |stored| stored.entry.generation)
}

/// The volume's device node.
fn lv_device_node(pool: &MovePool) -> String {
    format!("/dev/{}/{}", pool.vg_name, lv_name_for(&move_volume_id()))
}

/// The same-VG evacuation under concurrent guest I/O: the move
/// completes within its supervision window while a writer hammers a
/// second region of the LV (the dm mirror carries the concurrent
/// writes across the pivot), the relocation is verified against
/// LVM's own report, and both regions' data survives byte-for-byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lvm_move_evacuates_under_concurrent_io() -> Result<(), ApiError> {
    if !lvm_tests_enabled() {
        eprintln!(
            "skipping real-LVM integration test: set VOLVISOR_TEST_LVM=1 (requires root, \
             losetup and lvm2) to run it"
        );
        return Ok(());
    }

    // These tests mutate the host's real LVM (and `pvmove --abort` is
    // host-wide): they serialize on the host lock.
    let _host = host_lock().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = setup_move_pool(
        dir.path(),
        "volvisorit-moveio",
        MoveTiming {
            poll_interval: Duration::from_millis(200),
            supervision_window: Duration::from_secs(60),
        },
    )?;
    pool.provider
        .create_volume(&fixture_create_request(MOVE_VOLUME, MOVE_IO_VOLUME_BYTES))
        .await
        .expect("create");
    pool.extend_for_move()?;

    let lv = lv_device_node(&pool);
    let checksum_a = write_region(&lv, 0, dir.path());

    // The move supervises while the concurrent writer fills the
    // second region through the live mirror.
    let move_task = {
        let provider = Arc::clone(&pool.provider);
        let target = pool.target_pv.clone();
        tokio::spawn(async move {
            provider
                .move_volume_backing(&move_volume_id(), &move_request("op-move-io", &target, 1))
                .await
        })
    };
    let checksum_b = write_region(&lv, REGION_MIB, dir.path());
    let response = move_task
        .await
        .expect("the move task joins")
        .expect("the move completes under supervision");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 2, "one fenced bump at the completion");
    assert_eq!(response.source_pv, pool.source_pv);
    assert_eq!(response.target_pv, pool.target_pv);

    // The relocation is real: LVM's own report places every extent on
    // the target, none on the source.
    let devices = lv_backing_pvs(&pool)?;
    assert!(
        !devices.is_empty(),
        "the devices column names the placement"
    );
    assert!(
        devices.iter().all(|pv| pv == &pool.target_pv),
        "every extent on the target: {devices:?}"
    );
    assert!(
        !devices.iter().any(|pv| pv == &pool.source_pv),
        "no extent left on the source: {devices:?}"
    );

    // The data survived the evacuation and the concurrent writes.
    assert_eq!(
        region_checksum(&lv, 0, dir.path()),
        checksum_a,
        "region A is byte-identical after the evacuation"
    );
    assert_eq!(
        region_checksum(&lv, REGION_MIB, dir.path()),
        checksum_b,
        "region B (written through the live mirror) is byte-identical"
    );
    Ok(())
}

/// The restart mid-move: the drive parks in its supervision window
/// with the real mirror running; a fresh provider over the same
/// durable state (the daemon-death model — a bare provider runs
/// nothing on its own) re-attaches through `pvmove`'s own "Detected
/// pvmove in progress" semantics and completes the evacuation —
/// exactly one generation bump across both incarnations, the data
/// intact.
#[tokio::test]
async fn lvm_move_restart_mid_move_re_attaches_and_completes() -> Result<(), ApiError> {
    if !lvm_tests_enabled() {
        eprintln!(
            "skipping real-LVM integration test: set VOLVISOR_TEST_LVM=1 (requires root, \
             losetup and lvm2) to run it"
        );
        return Ok(());
    }

    // These tests mutate the host's real LVM (and `pvmove --abort` is
    // host-wide): they serialize on the host lock.
    let _host = host_lock().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = setup_move_pool(
        dir.path(),
        "volvisorit-moverestart",
        MoveTiming {
            poll_interval: Duration::from_millis(100),
            supervision_window: Duration::from_millis(300),
        },
    )?;
    pool.provider
        .create_volume(&fixture_create_request(MOVE_VOLUME, MOVE_VOLUME_BYTES))
        .await
        .expect("create");
    pool.extend_for_move()?;

    let lv = lv_device_node(&pool);
    let checksum_a = write_region(&lv, 0, dir.path());

    // The window expires with the mirror live (the 2-GiB sync far
    // outlives 300 ms): the honest answer is COPYING, no bump.
    let first = pool
        .provider
        .move_volume_backing(
            &move_volume_id(),
            &move_request("op-move-1", &pool.target_pv, 1),
        )
        .await
        .expect("the first drive answers");
    assert_eq!(first.state, MoveVolumeBackingState::Copying);
    assert_eq!(first.generation, 1, "no bump while copying");

    // The fresh incarnation over the same durable state (the same
    // runner, the same state file), now with a window that outlives
    // the remaining sync.
    let restarted = LvmProvider::new(
        Arc::clone(&pool.runner),
        pool.state_path.clone(),
        PathBuf::from("/"),
        "volvisorit".to_owned(),
        "integration-token".to_owned(),
    )
    .map(|provider| {
        Arc::new(provider.with_move_timing(MoveTiming {
            poll_interval: Duration::from_millis(200),
            supervision_window: Duration::from_secs(60),
        }))
    })?;
    assert_eq!(
        recorded_move(&pool.state_path).expect("the record").state,
        MoveVolumeBackingState::Copying,
        "the startup reconcile left the live mirror's record alone"
    );

    // A fresh operation id re-attaches and completes.
    let second = restarted
        .move_volume_backing(
            &move_volume_id(),
            &move_request("op-move-2", &pool.target_pv, 1),
        )
        .await
        .expect("the re-attach completes the evacuation");
    assert_eq!(second.state, MoveVolumeBackingState::Complete);
    assert_eq!(second.generation, 2, "exactly one bump across incarnations");

    let devices = lv_backing_pvs(&pool)?;
    assert!(
        devices.iter().all(|pv| pv == &pool.target_pv) && !devices.is_empty(),
        "every extent on the target: {devices:?}"
    );
    assert_eq!(
        region_checksum(&lv, 0, dir.path()),
        checksum_a,
        "the data survived both incarnations"
    );
    Ok(())
}

/// The out-of-band abort while the daemon is down: the operator's
/// `pvmove --abort` tears the mirror down and restores the source
/// placement; the fresh incarnation's startup reconcile parks the
/// record `IN_DOUBT` (never a silent revert, never a generic
/// failure), a fresh operation refuses typed naming the park, and
/// the source data is intact.
#[tokio::test]
async fn lvm_move_aborted_out_of_band_while_down_parks_in_doubt() -> Result<(), ApiError> {
    if !lvm_tests_enabled() {
        eprintln!(
            "skipping real-LVM integration test: set VOLVISOR_TEST_LVM=1 (requires root, \
             losetup and lvm2) to run it"
        );
        return Ok(());
    }

    // These tests mutate the host's real LVM (and `pvmove --abort` is
    // host-wide): they serialize on the host lock.
    let _host = host_lock().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = setup_move_pool(
        dir.path(),
        "volvisorit-moveabort",
        MoveTiming {
            poll_interval: Duration::from_millis(100),
            supervision_window: Duration::from_millis(300),
        },
    )?;
    pool.provider
        .create_volume(&fixture_create_request(MOVE_VOLUME, MOVE_VOLUME_BYTES))
        .await
        .expect("create");
    pool.extend_for_move()?;

    let lv = lv_device_node(&pool);
    let checksum_a = write_region(&lv, 0, dir.path());

    let first = pool
        .provider
        .move_volume_backing(
            &move_volume_id(),
            &move_request("op-move-1", &pool.target_pv, 1),
        )
        .await
        .expect("the first drive answers");
    assert_eq!(first.state, MoveVolumeBackingState::Copying);

    // The daemon is down (a bare provider runs nothing on its own).
    // The operator aborts out of band — the operator's tool, not the
    // daemon's runner (this test host runs no other pvmove).
    operator_ok(Command::new("pvmove").arg("--abort"));

    // The fresh incarnation parks at startup.
    let restarted = LvmProvider::new(
        Arc::clone(&pool.runner),
        pool.state_path.clone(),
        PathBuf::from("/"),
        "volvisorit".to_owned(),
        "integration-token".to_owned(),
    )?;
    let record = recorded_move(&pool.state_path).expect("the parked record");
    assert_eq!(record.state, MoveVolumeBackingState::InDoubt);
    assert!(
        record
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("without relocating")),
        "the park names the un-relocated outcome: {:?}",
        record.detail
    );
    assert_eq!(
        volume_generation(&pool.state_path),
        1,
        "no bump for an unverified outcome"
    );

    // A fresh operation refuses typed; the source keeps serving.
    let error = restarted
        .move_volume_backing(
            &move_volume_id(),
            &move_request("op-move-2", &pool.target_pv, 1),
        )
        .await
        .expect_err("the parked record refuses new work");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert!(
        error.detail.contains("IN_DOUBT"),
        "the refusal names the park: {error}"
    );

    // The abort's real placement semantics (verified against LVM
    // 2.03.16): a non-atomic `pvmove --abort` leaves the segments
    // already synced on the target and the rest on the source — an
    // honestly mixed placement, which is exactly why the record
    // parks IN_DOUBT instead of guessing. What the discipline
    // guarantees: the relocation is NOT verified-complete, and the
    // data is intact wherever it sits.
    let devices = lv_backing_pvs(&pool)?;
    assert!(
        !devices.iter().all(|pv| pv == &pool.target_pv),
        "the abort prevented the verified-complete relocation: {devices:?}"
    );
    assert_eq!(
        region_checksum(&lv, 0, dir.path()),
        checksum_a,
        "the source data is intact after the abort"
    );
    Ok(())
}

/// The abort mid-supervision: the drive is polling the live mirror
/// when the operator aborts out of band. The move ended without
/// relocating the extents, so the honest answer is a park — never a
/// completion claim, never a generic failure: the source extents
/// were never declared freed, the generation never bumped, and the
/// data is intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lvm_move_aborted_mid_supervision_parks_without_freeing() -> Result<(), ApiError> {
    if !lvm_tests_enabled() {
        eprintln!(
            "skipping real-LVM integration test: set VOLVISOR_TEST_LVM=1 (requires root, \
             losetup and lvm2) to run it"
        );
        return Ok(());
    }

    // These tests mutate the host's real LVM (and `pvmove --abort` is
    // host-wide): they serialize on the host lock.
    let _host = host_lock().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = setup_move_pool(
        dir.path(),
        "volvisorit-movepark",
        MoveTiming {
            poll_interval: Duration::from_millis(100),
            supervision_window: Duration::from_secs(30),
        },
    )?;
    pool.provider
        .create_volume(&fixture_create_request(MOVE_VOLUME, MOVE_VOLUME_BYTES))
        .await
        .expect("create");
    pool.extend_for_move()?;

    let lv = lv_device_node(&pool);
    let checksum_a = write_region(&lv, 0, dir.path());

    // The move supervises (a 30-s window over a multi-second sync);
    // the operator aborts it mid-flight.
    let move_task = {
        let provider = Arc::clone(&pool.provider);
        let target = pool.target_pv.clone();
        tokio::spawn(async move {
            provider
                .move_volume_backing(&move_volume_id(), &move_request("op-move-1", &target, 1))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    operator_ok(Command::new("pvmove").arg("--abort"));

    let response = move_task
        .await
        .expect("the move task joins")
        .expect("the drive answers with a park, not a failure");
    assert_eq!(
        response.state,
        MoveVolumeBackingState::InDoubt,
        "the unknown outcome parks: {response:?}"
    );

    let record = recorded_move(&pool.state_path).expect("the parked record");
    assert_eq!(record.state, MoveVolumeBackingState::InDoubt);
    assert_eq!(
        volume_generation(&pool.state_path),
        1,
        "the source extents were never declared freed"
    );

    // The abort's real placement semantics (verified against LVM
    // 2.03.16): a non-atomic `pvmove --abort` leaves the segments
    // already synced on the target and the rest on the source — an
    // honestly mixed placement, which is exactly why the record
    // parks IN_DOUBT instead of guessing. What the discipline
    // guarantees: the relocation is NOT verified-complete, and the
    // data is intact wherever it sits.
    let devices = lv_backing_pvs(&pool)?;
    assert!(
        !devices.iter().all(|pv| pv == &pool.target_pv),
        "the abort prevented the verified-complete relocation: {devices:?}"
    );
    assert_eq!(
        region_checksum(&lv, 0, dir.path()),
        checksum_a,
        "the data is intact after the aborted move"
    );
    Ok(())
}
