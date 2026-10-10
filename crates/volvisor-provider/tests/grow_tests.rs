//! Grow-notification engine tests (P6-B, ADR-0006 first slice part
//! 1): the real `guest_notification_status` machine over a
//! [`FakeVmm`], a mutable facts map and a real durable store in a
//! tempdir — the plan's evidence rows at the engine layer (the
//! daemon-level e2e composes the same engine behind the real router
//! in `volvisord`'s `grow_e2e`).
//!
//! The rows, per the plan's test/evidence shape:
//!
//! - the attached grow with a proven gate notifies the effective
//!   size (`notified`, the VMM told exactly the recorded triple);
//! - a failed notification reports `retry_required` with the reason
//!   recorded, and the retry pass converges (`retry_required` →
//!   `notified` on the tick);
//! - a detached grow reports `not_applicable`;
//! - the version-gate and addressability refusals are recorded, and
//!   nothing is told to the VMM;
//! - the never-shrink rule at the engine layer: the pass drives only
//!   the volume's current size and never a target below a recorded
//!   one;
//! - the crash between the backing grow and the notification (the
//!   store-save seam, P5 plan §3.1): the intent journal dies at the
//!   armed point, a fresh engine over the same durable state
//!   re-drives the notification;
//! - the store round-trips, refuses corrupt state typed and denies
//!   unknown fields.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use volvisor_provider::vmm::{FakeVmm, VmmController};
use volvisor_provider::{
    AttachmentForGrow, FakeResizeCall, GrowAttachmentFacts, GrowNotificationEngine,
    GrowNotificationRecord, GrowNotificationStatus, GrowNotificationStore, GrowNotifier,
    VmmVersionGate,
};
use volvisor_types::crash::{CRASH_PANIC_PREFIX, StoreSavePoint};
use volvisor_types::id::VolumeId;
use volvisor_types::request::GrowGuestNotification;
use volvisor_types::{ApiError, ApiErrorCode};

/// The volume under test.
fn volume() -> VolumeId {
    VolumeId::new("vol-1").expect("valid volume id")
}

/// A proven gate, through the real probe path (a scripted
/// `cloud-hypervisor --version`).
fn proven_gate() -> VmmVersionGate {
    let runner = volvisor_provider::FakeRunner::with_closure(|_program, _args| {
        Some(volvisor_provider::CommandOutput::success(
            "cloud-hypervisor v37.0\n",
        ))
    });
    VmmVersionGate::probe(
        Some(PathBuf::from("/usr/bin/cloud-hypervisor").as_path()),
        Some("37.0.0"),
        &runner,
    )
}

/// The mutable facts map: the rig rearranges attachments between
/// calls exactly like the provider's state would move.
type Facts = Arc<Mutex<BTreeMap<VolumeId, AttachmentForGrow>>>;

fn facts_closure(facts: Facts) -> GrowAttachmentFacts {
    Arc::new(move || {
        let map = facts.lock().expect("facts lock").clone();
        Ok(map)
    })
}

/// The deterministic clock: epoch seconds, advancing one per call.
fn clock() -> (volvisor_provider::grow::Clock, Arc<AtomicU64>) {
    let ticks = Arc::new(AtomicU64::new(1_700_000_000));
    let clock: volvisor_provider::grow::Clock = {
        let ticks = Arc::clone(&ticks);
        Arc::new(move || ticks.fetch_add(1, Ordering::SeqCst))
    };
    (clock, ticks)
}

/// The engine rig: a fake VMM, a mutable facts map and a store under
/// `dir`.
struct Rig {
    engine: Arc<GrowNotificationEngine>,
    vmm: Arc<FakeVmm>,
    facts: Facts,
    /// Kept alive for the rig's lifetime (the store and snapshots
    /// live under it); never read directly.
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new(dir: tempfile::TempDir) -> Self {
        Self::with_gate(dir, proven_gate())
    }

    fn with_gate(dir: tempfile::TempDir, gate: VmmVersionGate) -> Self {
        Self::with(dir, gate, true)
    }

    /// `wired_vmm = false` leaves the controller seam `None` (the
    /// unconfigured-socket-directory posture).
    fn with(dir: tempfile::TempDir, gate: VmmVersionGate, wired_vmm: bool) -> Self {
        let vmm = Arc::new(FakeVmm::new(dir.path().join("snapshots")));
        vmm.create("vm-1", &["/dev/vg/vol-1"])
            .expect("create the VM");
        let facts: Facts = Arc::new(Mutex::new(BTreeMap::new()));
        let store = GrowNotificationStore::open(dir.path().join("grow-notifications.json"))
            .expect("open the store");
        let (clock, _ticks) = clock();
        let engine = GrowNotificationEngine::new(
            (wired_vmm).then(|| Arc::clone(&vmm) as Arc<dyn VmmController>),
            gate,
            facts_closure(Arc::clone(&facts)),
            store,
            clock,
        );
        Self {
            engine: Arc::new(engine),
            vmm,
            facts,
            _dir: dir,
        }
    }

    /// Attach the volume, addressable at `size`.
    fn attach(&self, size: u64) {
        self.attach_as(size, Some("disk-vol-1".to_owned()));
    }

    /// Attach the volume at `size`, with (`Some`) or without (`None`)
    /// a recorded VMM disk id.
    fn attach_as(&self, size: u64, disk_id: Option<String>) {
        let mut facts = self.facts.lock().expect("facts lock");
        let entry = match disk_id {
            Some(vmm_disk_id) => AttachmentForGrow::Addressable {
                vm_id: "vm-1".to_owned(),
                vmm_disk_id,
                current_size_bytes: size,
            },
            None => AttachmentForGrow::Unaddressable,
        };
        facts.insert(volume(), entry);
    }

    /// Detach the volume (drop it from the facts map).
    fn detach(&self) {
        self.facts.lock().expect("facts lock").remove(&volume());
    }

    /// The volume's durable notification record.
    fn record(&self) -> Option<GrowNotificationRecord> {
        self.engine.record(&volume()).expect("record")
    }
}

#[test]
fn an_attached_grow_with_a_proven_gate_notifies_the_effective_size() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rig = Rig::new(dir);
    rig.attach(2_147_483_648);

    let status = rig.engine.notify_grow(&volume(), 2_147_483_648);
    assert_eq!(status, GrowGuestNotification::Notified);

    // The VMM was told exactly the recorded triple.
    assert_eq!(
        rig.vmm.resize_calls().expect("resizes"),
        vec![FakeResizeCall {
            vm_id: "vm-1".to_owned(),
            disk_id: "disk-vol-1".to_owned(),
            new_size_bytes: 2_147_483_648,
        }]
    );
    // The durable state records the notification with the target.
    let record = rig.record().expect("the record exists");
    assert_eq!(record.target_size_bytes, 2_147_483_648);
    // The clock ticked once for the intent journal, once for the
    // outcome (the deterministic injected clock).
    assert_eq!(
        record.status,
        GrowNotificationStatus::Notified { at: 1_700_000_001 }
    );
    assert_eq!(record.vm_id, "vm-1");
    assert_eq!(record.vmm_disk_id, "disk-vol-1");
}

#[test]
fn a_failed_notification_is_retry_required_with_the_reason_recorded_and_the_pass_converges() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rig = Rig::new(dir);
    rig.attach(2_147_483_648);
    rig.vmm
        .set_fail("vm-1", |knobs| knobs.resize_disk = true)
        .expect("arm the fault");

    // The grow's notification fails: retry_required, the injected
    // failure recorded as the reason, nothing told to the VMM.
    let status = rig.engine.notify_grow(&volume(), 2_147_483_648);
    assert_eq!(status, GrowGuestNotification::RetryRequired);
    assert_eq!(rig.vmm.resize_calls().expect("resizes"), Vec::new());
    let record = rig.record().expect("the record exists");
    let reason = record.pending_reason().expect("pending with a reason");
    assert!(reason.contains("injected"), "{reason}");
    assert_eq!(record.target_size_bytes, 2_147_483_648);

    // Recovery through the surface production would: the retry pass
    // converges retry_required -> notified.
    rig.vmm
        .set_fail("vm-1", |knobs| knobs.resize_disk = false)
        .expect("clear the fault");
    let report = rig.engine.retry_pass().expect("the pass");
    assert_eq!(
        report.notified,
        vec![(volume(), 2_147_483_648)],
        "the pass reports the convergence"
    );
    assert_eq!(report.retry_required, Vec::new());
    assert_eq!(
        rig.vmm.resize_calls().expect("resizes"),
        vec![FakeResizeCall {
            vm_id: "vm-1".to_owned(),
            disk_id: "disk-vol-1".to_owned(),
            new_size_bytes: 2_147_483_648,
        }]
    );
    // Convergence is stable: the next pass is a no-op.
    let report = rig.engine.retry_pass().expect("the pass");
    assert_eq!(report.notified, Vec::new());
    assert_eq!(
        rig.vmm.resize_calls().expect("resizes").len(),
        1,
        "an already-notified record is not re-driven"
    );
}

#[test]
fn a_detached_grow_reports_not_applicable_and_resolves_any_pending_record() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rig = Rig::new(dir);
    rig.attach(2_147_483_648);
    rig.vmm
        .set_fail("vm-1", |knobs| knobs.resize_disk = true)
        .expect("arm the fault");
    assert_eq!(
        rig.engine.notify_grow(&volume(), 2_147_483_648),
        GrowGuestNotification::RetryRequired
    );
    assert!(rig.record().expect("record").is_pending());

    // The volume detaches (the VM is gone): no frontend to notify.
    rig.detach();
    assert_eq!(
        rig.engine.notify_grow(&volume(), 3_221_225_472),
        GrowGuestNotification::NotApplicable
    );
    // The pending record resolved with it — nothing retries against
    // a dead socket forever.
    assert_eq!(rig.record(), None, "the detached pending is removed");
    assert_eq!(rig.vmm.resize_calls().expect("resizes"), Vec::new());
}

#[test]
fn an_unaddressable_attachment_is_refused_with_the_recorded_reason() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rig = Rig::new(dir);
    rig.attach_as(2_147_483_648, None);

    let status = rig.engine.notify_grow(&volume(), 2_147_483_648);
    assert_eq!(status, GrowGuestNotification::RetryRequired);
    assert_eq!(rig.vmm.resize_calls().expect("resizes"), Vec::new());
    let record = rig.record().expect("the record exists");
    let reason = record.pending_reason().expect("pending with a reason");
    assert!(reason.contains("vmm_disk_id"), "{reason}");

    // The pass reports the recorded reason and invents no drive.
    let report = rig.engine.retry_pass().expect("the pass");
    assert_eq!(report.notified, Vec::new());
    assert_eq!(
        report.retry_required,
        vec![(volume(), reason)],
        "the unaddressable refusal is reported, not silently dropped"
    );

    // Re-attach addressable (the consumer re-attached with a disk
    // id): the pass drives the notification.
    rig.attach(2_147_483_648);
    let report = rig.engine.retry_pass().expect("the pass");
    assert_eq!(report.notified, vec![(volume(), 2_147_483_648)]);
}

#[test]
fn the_version_gate_refusal_is_recorded_and_notifies_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rig = Rig::with_gate(
        dir,
        VmmVersionGate::refused("the observed cloud-hypervisor version is below the minimum"),
    );
    rig.attach(2_147_483_648);

    let status = rig.engine.notify_grow(&volume(), 2_147_483_648);
    assert_eq!(status, GrowGuestNotification::RetryRequired);
    assert_eq!(rig.vmm.resize_calls().expect("resizes"), Vec::new());
    let reason = rig
        .record()
        .expect("the record exists")
        .pending_reason()
        .expect("pending");
    assert!(reason.contains("version gate"), "{reason}");
    assert!(reason.contains("below the minimum"), "{reason}");
}

#[test]
fn an_unwired_vmm_controller_is_refused_with_the_recorded_reason() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rig = Rig::with(dir, proven_gate(), false);
    rig.attach(2_147_483_648);

    let status = rig.engine.notify_grow(&volume(), 2_147_483_648);
    assert_eq!(status, GrowGuestNotification::RetryRequired);
    assert_eq!(rig.vmm.resize_calls().expect("resizes"), Vec::new());
    let reason = rig
        .record()
        .expect("the record exists")
        .pending_reason()
        .expect("pending");
    assert!(reason.contains("api_socket_dir"), "{reason}");
}

#[test]
fn the_pass_drives_an_attached_volume_with_no_record_the_healing_case() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rig = Rig::new(dir);
    // Attached, never notified through the engine: no record exists.
    // This is both the fresh-attach case and the crash-before-intent
    // window — the invariant drives the notification anyway.
    rig.attach(2_147_483_648);

    let report = rig.engine.retry_pass().expect("the pass");
    assert_eq!(report.notified, vec![(volume(), 2_147_483_648)]);
    assert_eq!(
        rig.vmm.resize_calls().expect("resizes"),
        vec![FakeResizeCall {
            vm_id: "vm-1".to_owned(),
            disk_id: "disk-vol-1".to_owned(),
            new_size_bytes: 2_147_483_648,
        }]
    );
}

#[test]
fn the_pass_re_drives_a_target_below_the_current_size_never_above_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rig = Rig::new(dir);
    // Notified at 1 GiB, then the volume grew to 2 GiB with the
    // outcome lost (any crash between the backing grow and the
    // record): the target is stale below the current size.
    rig.attach(2_147_483_648);
    assert_eq!(
        rig.engine.notify_grow(&volume(), 1_073_741_824),
        GrowGuestNotification::Notified
    );
    assert_eq!(
        rig.vmm.resize_calls().expect("resizes")[0].new_size_bytes,
        1_073_741_824
    );

    let report = rig.engine.retry_pass().expect("the pass");
    assert_eq!(
        report.notified,
        vec![(volume(), 2_147_483_648)],
        "the stale target is re-driven at the current size"
    );
    assert_eq!(
        rig.vmm.resize_calls().expect("resizes")[1].new_size_bytes,
        2_147_483_648
    );

    // The never-shrink shape: a record ABOVE the current size (a
    // deleted-and-recreated volume identity reusing the store entry)
    // triggers no drive — the VMM is never told a smaller size.
    rig.attach(1_073_741_824);
    let report = rig.engine.retry_pass().expect("the pass");
    assert_eq!(report.notified, Vec::new());
    assert_eq!(report.retry_required, Vec::new());
    assert_eq!(
        rig.vmm.resize_calls().expect("resizes").len(),
        2,
        "no resize below a recorded target is ever issued"
    );
}

#[test]
fn a_pending_record_resolves_not_applicable_when_the_volume_detaches() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rig = Rig::new(dir);
    rig.attach(2_147_483_648);
    rig.vmm
        .set_fail("vm-1", |knobs| knobs.resize_disk = true)
        .expect("arm the fault");
    assert_eq!(
        rig.engine.notify_grow(&volume(), 2_147_483_648),
        GrowGuestNotification::RetryRequired
    );
    assert!(rig.record().expect("record").is_pending());

    // The volume detaches between ticks: the pass resolves the
    // pending record not_applicable instead of retrying a dead
    // socket forever.
    rig.detach();
    let report = rig.engine.retry_pass().expect("the pass");
    assert_eq!(report.not_applicable, vec![volume()]);
    assert_eq!(report.retry_required, Vec::new());
    assert_eq!(rig.record(), None);
}

#[test]
fn a_facts_failure_reports_retry_required_and_the_pass_fails_wholesale() {
    let dir = tempfile::tempdir().expect("tempdir");
    let failing: GrowAttachmentFacts = Arc::new(|| {
        Err(ApiError::new(
            ApiErrorCode::Internal,
            "the provider state cannot be read".to_owned(),
        ))
    });
    let store =
        GrowNotificationStore::open(dir.path().join("grow-notifications.json")).expect("open");
    let (clock, _ticks) = clock();
    let engine = GrowNotificationEngine::new(
        Some(Arc::new(FakeVmm::new(dir.path())) as Arc<dyn VmmController>),
        proven_gate(),
        failing,
        store,
        clock,
    );

    // Not silent (retry_required), and the same failure fails the
    // retry pass wholesale — the daemon's task logs it every tick.
    assert_eq!(
        engine.notify_grow(&volume(), 2_147_483_648),
        GrowGuestNotification::RetryRequired
    );
    let error = engine.retry_pass().expect_err("the pass fails");
    assert_eq!(error.code, ApiErrorCode::Internal);
    assert!(error.detail.contains("cannot be read"), "{error}");
}

#[test]
fn a_crash_between_the_backing_grow_and_the_notification_is_re_driven_on_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The production shape, not the Rig: the rig arms the engine's
    // own store hooks through the getter (the migration rig's
    // pattern — the hooks table is shared, the store consults it on
    // every save).
    let vmm = Arc::new(FakeVmm::new(dir.path().join("snapshots")));
    vmm.create("vm-1", &["/dev/vg/vol-1"])
        .expect("create the VM");
    let facts: Facts = Arc::new(Mutex::new(BTreeMap::new()));
    facts.lock().expect("facts lock").insert(
        volume(),
        AttachmentForGrow::Addressable {
            vm_id: "vm-1".to_owned(),
            vmm_disk_id: "disk-vol-1".to_owned(),
            current_size_bytes: 2_147_483_648,
        },
    );
    let store = GrowNotificationStore::open(dir.path().join("grow-notifications.json"))
        .expect("open the store");

    // Arm the store-save seam (P5 plan §3.1): the intent journal's
    // save dies after the rename — the new state is exactly what a
    // reload sees. The kill switch records the supervisor's abort
    // request before the in-band panic unwinds the saving task.
    let killed = Arc::new(AtomicBool::new(false));
    {
        let killed = Arc::clone(&killed);
        store
            .store_crash_hooks()
            .set_kill_switch(Arc::new(move || killed.store(true, Ordering::SeqCst)));
    }
    store.store_crash_hooks().arm(
        volvisor_types::crash::STORE_GROW_NOTIFICATIONS,
        StoreSavePoint::AfterRename,
    );

    let (armed_clock, _ticks) = clock();
    let engine = Arc::new(GrowNotificationEngine::new(
        Some(Arc::clone(&vmm) as Arc<dyn VmmController>),
        proven_gate(),
        facts_closure(Arc::clone(&facts)),
        store,
        armed_clock,
    ));

    // The grow's notification dies mid-intent-save (the panic is the
    // injected process death — the campaign's filtered-hook shape).
    let crashing = Arc::clone(&engine);
    let handle = std::thread::spawn(move || crashing.notify_grow(&volume(), 2_147_483_648));
    let panic = handle.join().expect_err("the armed save must die");
    let message = panic
        .downcast_ref::<String>()
        .expect("the panic payload is the crash marker");
    assert!(
        message.starts_with(CRASH_PANIC_PREFIX),
        "the kill is the campaign's crash marker: {message}"
    );
    assert!(
        killed.load(Ordering::SeqCst),
        "the kill switch fired before the panic"
    );
    // The resize was never attempted: the intent journal died first.
    assert_eq!(vmm.resize_calls().expect("resizes"), Vec::new());

    // The intent IS durable (AfterRename): a fresh engine over the
    // same paths — the restart — re-drives the notification through
    // the startup reconcile pass.
    let (restarted_clock, _ticks) = clock();
    let restarted = GrowNotificationEngine::new(
        Some(Arc::clone(&vmm) as Arc<dyn VmmController>),
        proven_gate(),
        facts_closure(Arc::clone(&facts)),
        GrowNotificationStore::open(dir.path().join("grow-notifications.json"))
            .expect("open the restarted store"),
        restarted_clock,
    );
    let report = restarted.retry_pass().expect("the restart pass");
    assert_eq!(
        report.notified,
        vec![(volume(), 2_147_483_648)],
        "the startup reconcile re-drives the notification"
    );
    assert_eq!(
        vmm.resize_calls().expect("resizes"),
        vec![FakeResizeCall {
            vm_id: "vm-1".to_owned(),
            disk_id: "disk-vol-1".to_owned(),
            new_size_bytes: 2_147_483_648,
        }]
    );
    let record = restarted
        .record(&volume())
        .expect("the store lock")
        .expect("the record exists");
    assert!(!record.is_pending());
}

#[test]
fn the_store_round_trips_and_a_leftover_tmp_is_ignored() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("grow-notifications.json");
    let mut store = GrowNotificationStore::open(&path).expect("open (missing file is empty)");
    assert_eq!(store.records(), Vec::new());
    let record = GrowNotificationRecord {
        volume_id: volume(),
        vm_id: "vm-1".to_owned(),
        vmm_disk_id: "disk-vol-1".to_owned(),
        target_size_bytes: 2_147_483_648,
        status: GrowNotificationStatus::Notified { at: 42 },
        updated_at: 42,
    };
    store.upsert(&record).expect("upsert");
    // A leftover .tmp sibling (the discarded half of an interrupted
    // save) is ignored on load.
    std::fs::write(path.with_extension("json.tmp"), "garbage").expect("write the leftover tmp");
    let reloaded = GrowNotificationStore::open(&path).expect("reload");
    assert_eq!(reloaded.get(&volume()), Some(record));
    assert_eq!(reloaded.records().len(), 1);

    // Removal persists.
    let mut reloaded = reloaded;
    assert_eq!(
        reloaded.remove(&volume()).expect("remove"),
        Some(GrowNotificationRecord {
            volume_id: volume(),
            vm_id: "vm-1".to_owned(),
            vmm_disk_id: "disk-vol-1".to_owned(),
            target_size_bytes: 2_147_483_648,
            status: GrowNotificationStatus::Notified { at: 42 },
            updated_at: 42,
        })
    );
    assert_eq!(
        GrowNotificationStore::open(&path)
            .expect("reload")
            .records(),
        Vec::new()
    );
    // Removing an absent record is a no-op.
    assert_eq!(reloaded.remove(&volume()).expect("remove absent"), None);
}

#[test]
fn a_corrupt_store_is_a_typed_startup_error_and_records_deny_unknown_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("grow-notifications.json");
    std::fs::write(&path, "not json at all").expect("write the corrupt state");
    let error = GrowNotificationStore::open(&path).expect_err("corrupt state refuses typed");
    assert_eq!(error.code, ApiErrorCode::Internal);
    assert!(error.detail.contains("failed to parse"), "{error}");

    // deny_unknown_fields is part of the record's shape.
    let unknown = format!(
        "{{\"{}\": {{\"volume_id\": \"{}\", \"vm_id\": \"vm-1\", \"vmm_disk_id\": \"d\", \
         \"target_size_bytes\": 1, \"status\": {{\"pending\": {{\"reason\": \"r\"}}}}, \
         \"updated_at\": 0, \"rogue_field\": 1}}}}",
        "vol-1", "vol-1"
    );
    std::fs::write(&path, unknown).expect("write the rogue record");
    let error = GrowNotificationStore::open(&path).expect_err("unknown fields refuse typed");
    assert!(error.detail.contains("failed to parse"), "{error}");
}
