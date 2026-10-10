//! The fake data path (P5 plan §2.1/§2.3/§2.4): the block store, the
//! enforced [`open_device`] handle, the async peer-apply queue and its
//! gate-coupling rule, the content-copying resync (the system-path
//! post-barrier drain) and the raw injection surfaces.
//!
//! Every row asserts SAFETY facts, never a happy-path echo: which
//! role a write landed on, what the status tokens say while the
//! peer-apply window is open, what a drain actually applied, and what
//! the raw paths deliberately bypass.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use volvisor_drbd::report::DiskState;
use volvisor_drbd::resgen::res_file_path;
use volvisor_drbd::state::ReplicationMode;
use volvisor_drbd::{CommandRunner, FakeRunner};
use volvisor_drbd_testkit::{
    BLOCK_SIZE, Block, FakeDrbd, SEED_MINOR, apply_peer_writes, leak_tempdir,
    link_replication_peers, open_device, read_raw, seed_peer_volume, seed_volume_with_identity,
    write_raw,
};
use volvisor_types::ApiErrorCode;

/// One mebibyte (extent-aligned to 4 MiB under the fixture's extents:
/// 1024 logical blocks of device surface).
const MIB: u64 = 1 << 20;

/// A freshly created host directory (the `host_dir` shape: `drbd.d`,
/// no state file yet).
fn host_dir() -> PathBuf {
    let base = leak_tempdir();
    fs::create_dir_all(base.join("drbd.d")).expect("config dir");
    base
}

/// One seeded single world: the volume's resource file, LV, metadata
/// and running resource (Secondary, both ends `UpToDate`), plus the
/// scripted runner over it and the resource's name.
fn seeded_world(volume: &str) -> (PathBuf, Arc<Mutex<FakeDrbd>>, Arc<FakeRunner>, String) {
    let base = host_dir();
    let world = Arc::new(Mutex::new(FakeDrbd::default()));
    seed_volume_with_identity(
        &base,
        &world,
        volume,
        MIB,
        ReplicationMode::A,
        SEED_MINOR,
        volvisor_drbd_testkit::SEED_PORT,
    );
    let resource = volvisor_drbd::provider::resource_name_for(
        &volvisor_types::VolumeId::new(volume).expect("valid volume id"),
    );
    let runner = FakeDrbd::runner(&world);
    (base, world, runner, resource)
}

/// The resource file path of a seeded world.
fn res_file(base: &Path, resource: &str) -> String {
    res_file_path(&base.join("drbd.d"), resource)
        .to_string_lossy()
        .into_owned()
}

/// Promote a seeded world's resource through the scripted runner
/// (a plain `primary`: the seeded disk is `UpToDate`).
fn promote(runner: &FakeRunner, base: &Path, resource: &str) {
    runner
        .run(
            "drbdadm",
            &["-c", &res_file(base, resource), "primary", resource],
        )
        .expect("primary");
}

/// `drbdsetup suspend-io|resume-io /dev/drbd<minor>` through the
/// scripted runner.
fn set_suspended(runner: &FakeRunner, minor: u32, suspend: bool) {
    let verb = if suspend { "suspend-io" } else { "resume-io" };
    runner
        .run("drbdsetup", &[verb, &format!("/dev/drbd{minor}")])
        .expect(verb);
}

/// `drbdsetup status <resource>` through the scripted runner.
fn status(runner: &FakeRunner, resource: &str) -> String {
    runner
        .run("drbdsetup", &["status", resource])
        .expect("status")
        .stdout
}

/// A 4 KiB payload filled with one byte.
fn payload(byte: u8) -> [u8; BLOCK_SIZE] {
    [byte; BLOCK_SIZE]
}

/// The world's running resource state (the test's own lock scope).
fn resource_of(
    world: &Arc<Mutex<FakeDrbd>>,
    resource: &str,
) -> volvisor_drbd_testkit::FakeResource {
    world
        .lock()
        .expect("world")
        .resources
        .get(resource)
        .expect("resource exists")
        .clone()
}

// ------------------------------------------------------------- §2.1

/// Writes land only on the Primary (the E_ROFS-shaped typed refusal
/// on a Secondary), and a Primary's write is the source-side ack: the
/// bytes land in the source's block map AND the async peer-apply
/// queue, with a monotonic acknowledgment sequence. A never-written
/// block reads as the sparse zero fill.
#[test]
fn device_writes_land_in_the_source_map_and_the_apply_queue() {
    let (base, world, runner, resource) = seeded_world("vol-data");
    promote(&runner, &base, &resource);
    let device = open_device(&world, SEED_MINOR).expect("open the Primary's device");

    let first = device.write(0, &payload(0xaa)).expect("write 1");
    let second = device.write(0, &payload(0xbb)).expect("overwrite");
    let third = device.write(1, &payload(0xcc)).expect("write 3");
    assert_eq!((first, second, third), (1, 2, 3), "monotonic ack seqs");

    let state = resource_of(&world, &resource);
    assert_eq!(state.blocks.len(), 2, "the overwritten block is one entry");
    assert_eq!(state.blocks[&0].payload, payload(0xbb));
    assert_eq!(state.blocks[&1].payload, payload(0xcc));
    assert!(
        state.blocks.values().all(|block| !block.applied_at_peer),
        "nothing is peer-applied yet — the window is open"
    );
    assert_eq!(state.apply_queue.len(), 3, "every ack queued for the peer");
    // The lineage of a written block is the resource's set.
    assert_eq!(state.blocks[&0].lineage, state.blocks[&1].lineage);

    // A never-written block reads as the sparse zero fill, trivially
    // in sync.
    let unwritten = device.read(7).expect("read");
    assert_eq!(unwritten.payload, [0_u8; BLOCK_SIZE]);
    assert!(unwritten.applied_at_peer);

    // Wrong-size payloads and out-of-bounds blocks refuse typed.
    let short = device
        .write(0, &[0_u8; BLOCK_SIZE - 1])
        .expect_err("short payload");
    assert_eq!(short.code, ApiErrorCode::InvalidRequest);
    let beyond = device
        .write(1 << 20, &payload(1))
        .expect_err("beyond the device");
    assert_eq!(beyond.code, ApiErrorCode::InvalidRequest);
}

/// Opening a Secondary resource's device fails typed, E_ROFS-shaped —
/// the fake enforces the single-writer invariant the real stack
/// enforces at the device.
#[test]
fn open_device_refuses_a_secondary_resource_typed() {
    let (_base, world, _runner, _resource) = seeded_world("vol-rofs");
    let error = open_device(&world, SEED_MINOR)
        .err()
        .expect("a Secondary device is read-only");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert!(
        error.detail.contains("(-30) Read-only file system"),
        "E_ROFS-shaped refusal: {error}"
    );
    // And nothing was recorded as an opener.
    assert!(world.lock().expect("world").device_openers.is_empty());
}

/// A suspended minor refuses writes on its handle (the quiesce is
/// real at the data path, not just a flag), and `resume-io`
/// re-enables them.
#[test]
fn suspension_gates_the_device_write_path() {
    let (base, world, runner, resource) = seeded_world("vol-susp");
    promote(&runner, &base, &resource);
    let device = open_device(&world, SEED_MINOR).expect("open");
    device.write(0, &payload(0x11)).expect("write before");

    set_suspended(&runner, SEED_MINOR, true);
    let frozen = device
        .write(1, &payload(0x22))
        .expect_err("a suspended data path refuses writes");
    assert_eq!(frozen.code, ApiErrorCode::InvalidState);
    assert!(
        frozen.detail.contains("suspended:user"),
        "the refusal names the suspension: {frozen}"
    );

    set_suspended(&runner, SEED_MINOR, false);
    device.write(1, &payload(0x22)).expect("write after resume");
}

/// A held handle participates in the openers/busy model exactly as
/// the `FakeVmm` device hooks do: while it lives the demote refuses
/// with the real held-open error and the status `open:` line reads
/// `yes`; dropping it releases the device.
#[test]
fn a_held_handle_blocks_the_demote_and_releases_it() {
    let (base, world, runner, resource) = seeded_world("vol-busy");
    promote(&runner, &base, &resource);
    let device = open_device(&world, SEED_MINOR).expect("open");

    assert!(
        status(&runner, &resource).contains("open:yes"),
        "the handle holds the device open"
    );
    let busy = runner
        .run(
            "drbdadm",
            &["-c", &res_file(&base, &resource), "secondary", &resource],
        )
        .expect("the demote ran");
    assert!(!busy.success);
    assert!(
        busy.stderr.contains("(-12) Device is held open by someone"),
        "the real held-open refusal: {busy:?}"
    );

    drop(device);
    assert!(
        status(&runner, &resource).contains("open:no"),
        "the dropped handle released the device"
    );
    let demote = runner
        .run(
            "drbdadm",
            &["-c", &res_file(&base, &resource), "secondary", &resource],
        )
        .expect("the demote ran");
    assert!(demote.success, "the demote lands after the release");
}

// --------------------------------------------------- §2.3 (the hinge)

/// The gate-coupling rule: the data-bearing status tokens the
/// convergence gate reads are DERIVED from the apply queue's state —
/// a non-drained queue reports the lagging shape (a resync line over
/// a not-`UpToDate` peer disk), and only a fully drained queue reads
/// `peer-disk:UpToDate` with no `replication:` line. `apply_peer_writes`
/// drains by acknowledgment sequence, so a partial drain keeps the
/// gate honestly closed.
#[test]
fn the_status_tokens_derive_from_the_apply_queue() {
    let (base, world, runner, resource) = seeded_world("vol-couple");
    promote(&runner, &base, &resource);
    let device = open_device(&world, SEED_MINOR).expect("open");
    let first = device.write(0, &payload(0x0f)).expect("write 1");
    let second = device.write(1, &payload(0xf0)).expect("write 2");

    let lagging = status(&runner, &resource);
    assert!(
        lagging.contains("peer-disk:Inconsistent"),
        "an open window holds the peer back: {lagging}"
    );
    assert!(
        lagging.contains("replication:SyncSource"),
        "the lag reports through the real resync token: {lagging}"
    );

    // A partial drain: the first write applied, the tail still open —
    // the gate stays closed.
    apply_peer_writes(&world, SEED_MINOR, first).expect("drain 1");
    let still = status(&runner, &resource);
    assert!(still.contains("peer-disk:Inconsistent"), "{still}");
    let state = resource_of(&world, &resource);
    assert_eq!(state.apply_queue.len(), 1);
    assert!(state.blocks[&0].applied_at_peer);
    assert!(!state.blocks[&1].applied_at_peer);

    // The full drain: the tokens flip, exactly what `track_sync`
    // observes.
    apply_peer_writes(&world, SEED_MINOR, second).expect("drain 2");
    let converged = status(&runner, &resource);
    assert!(
        converged.contains("peer-disk:UpToDate"),
        "a drained queue reads UpToDate: {converged}"
    );
    assert!(
        !converged.contains("replication:"),
        "no resync token while converged: {converged}"
    );
    let state = resource_of(&world, &resource);
    assert!(state.apply_queue.is_empty());
    assert!(state.blocks.values().all(|block| block.applied_at_peer));
}

/// The post-barrier drain is the fake's content-copying resync — the
/// SYSTEM path, never the campaign: `suspend-io` over a live
/// replication link applies the outstanding queue; over a partition
/// (the link down) nothing drains and the window stays honestly open.
#[test]
fn suspend_io_drains_the_queue_only_over_a_live_link() {
    let (base, world, runner, resource) = seeded_world("vol-barrier");
    promote(&runner, &base, &resource);
    let device = open_device(&world, SEED_MINOR).expect("open");
    device.write(3, &payload(0x42)).expect("write");
    assert_eq!(resource_of(&world, &resource).apply_queue.len(), 1);

    // A partitioned link: the barrier freezes the data path but
    // replication cannot progress — the queue survives.
    world.lock().expect("world").peer_online = false;
    set_suspended(&runner, SEED_MINOR, true);
    let partitioned = resource_of(&world, &resource);
    assert_eq!(
        partitioned.apply_queue.len(),
        1,
        "nothing drains over a partition"
    );
    assert!(!partitioned.blocks[&3].applied_at_peer);
    set_suspended(&runner, SEED_MINOR, false);

    // The healed link: the next barrier's resync drains the queue and
    // marks the peer applied.
    world.lock().expect("world").peer_online = true;
    set_suspended(&runner, SEED_MINOR, true);
    let drained = resource_of(&world, &resource);
    assert!(
        drained.apply_queue.is_empty(),
        "the post-barrier resync drained the queue"
    );
    assert!(drained.blocks[&3].applied_at_peer);
}

// --------------------------------------------------- §2.3 across hosts

/// A linked peer world's map receives drained writes — payload AND
/// lineage — and only what the drain actually applied: the
/// cross-host shape the write-trace oracle verifies against.
#[test]
fn drained_writes_reach_the_linked_peer_worlds_map() {
    let (base_a, world_a, runner_a, resource) = seeded_world("vol-link");
    let base_b = host_dir();
    let world_b = Arc::new(Mutex::new(FakeDrbd::default()));
    seed_peer_volume(
        &base_b,
        &world_b,
        "vol-link",
        MIB,
        ReplicationMode::A,
        SEED_MINOR,
        volvisor_drbd_testkit::SEED_PORT,
    );
    link_replication_peers(&world_a, &world_b);

    promote(&runner_a, &base_a, &resource);
    let device = open_device(&world_a, SEED_MINOR).expect("open the source device");
    let first = device.write(0, &payload(0x11)).expect("write 1");
    let second = device.write(1, &payload(0x22)).expect("write 2");

    // Nothing reached the peer before a drain.
    assert!(
        resource_of(&world_b, &resource).blocks.is_empty(),
        "the peer-apply window is real: acked is not yet applied"
    );

    // The partial drain applies exactly the first write at the peer.
    apply_peer_writes(&world_a, SEED_MINOR, first).expect("drain 1");
    let peer = resource_of(&world_b, &resource);
    assert_eq!(peer.blocks.len(), 1);
    assert_eq!(peer.blocks[&0].payload, payload(0x11));
    assert_eq!(
        peer.blocks[&0].lineage,
        resource_of(&world_a, &resource).blocks[&0].lineage,
        "lineage travels with the payload"
    );

    apply_peer_writes(&world_a, SEED_MINOR, second).expect("drain 2");
    assert_eq!(resource_of(&world_b, &resource).blocks.len(), 2);
}

/// S5 (the comprehensive review): the single-drainer invariant is
/// STRUCTURAL — a drain mutex stored on the world
/// ([`FakeDrbd::drain_lock`]) is held across the whole
/// pop-and-apply-and-requeue critical section, so a second drainer
/// can never interleave a failed drain's requeue with a newer
/// same-block delivery. The choreography pins the structure
/// deterministically: while drainer A is blocked mid-critical-section
/// (at the linked peer's lock, after its pop), the drain mutex must
/// be HELD (a `try_lock` refuses), a newer same-block write waits in
/// the queue, and drainer B makes no progress until A completes —
/// after which B delivers the newer write and the peer ends on the
/// LAST payload, never a stale overwrite. Under the old
/// one-drainer-at-a-time-by-convention shape, B would pop and
/// deliver concurrently and the peer's final content depended on the
/// threads' wakeup order.
#[test]
fn the_drain_mutex_spans_the_whole_critical_section() {
    let (base_a, world_a, runner_a, resource) = seeded_world("vol-drain-lock");
    let base_b = host_dir();
    let world_b = Arc::new(Mutex::new(FakeDrbd::default()));
    seed_peer_volume(
        &base_b,
        &world_b,
        "vol-drain-lock",
        MIB,
        ReplicationMode::A,
        SEED_MINOR,
        volvisor_drbd_testkit::SEED_PORT,
    );
    link_replication_peers(&world_a, &world_b);

    promote(&runner_a, &base_a, &resource);
    let device = open_device(&world_a, SEED_MINOR).expect("open the source device");
    device.write(0, &payload(0x11)).expect("write 1");

    // Hold the PEER world's lock: drainer A pops the entry and then
    // blocks inside its critical section (the delivery cannot
    // proceed past the peer's lock).
    let peer_guard = world_b.lock().expect("peer world");
    let drainer_a = {
        let world = Arc::clone(&world_a);
        std::thread::spawn(move || apply_peer_writes(&world, SEED_MINOR, u64::MAX))
    };

    // The deterministic signal that A popped: the queue empties
    // while A is inside its critical section.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !resource_of(&world_a, &resource).apply_queue.is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "drainer A never reached its pop"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    // A newer same-block write lands BEHIND A's in-flight entry.
    device.write(0, &payload(0x22)).expect("write 2");
    assert_eq!(
        resource_of(&world_a, &resource).apply_queue.len(),
        1,
        "the newer write waits in the queue"
    );

    // THE structural pin: A holds the drain mutex while blocked
    // mid-critical-section — the mutex spans pop through delivery,
    // so no second drainer can interleave.
    let drain = Arc::clone(&world_a.lock().expect("world").drain_lock);
    assert!(
        drain.try_lock().is_err(),
        "the drain mutex is held across the in-flight critical section"
    );

    // Drainer B contends: it must make NO progress while A is
    // in flight (it blocks on the drain mutex, before its pop).
    let drainer_b = {
        let world = Arc::clone(&world_a);
        std::thread::spawn(move || apply_peer_writes(&world, SEED_MINOR, u64::MAX))
    };
    std::thread::sleep(std::time::Duration::from_millis(10));
    assert_eq!(
        resource_of(&world_a, &resource).apply_queue.len(),
        1,
        "drainer B cannot pop while A holds the drain mutex"
    );

    // Release the peer: A delivers its entry and completes; only
    // then does B pop and deliver the NEWER write. The peer ends on
    // the last payload — the stale-overwrite interleave is
    // structurally impossible.
    drop(peer_guard);
    drainer_a
        .join()
        .expect("drainer A thread")
        .expect("drainer A delivers cleanly");
    drainer_b
        .join()
        .expect("drainer B thread")
        .expect("drainer B delivers cleanly");
    let peer = resource_of(&world_b, &resource);
    assert_eq!(
        peer.blocks[&0].payload,
        payload(0x22),
        "the peer ends on the LAST write, never a stale overwrite"
    );
    assert!(
        resource_of(&world_a, &resource).apply_queue.is_empty(),
        "both drains completed"
    );
}

/// S4 (the comprehensive review): a poison that lands in the window
/// between the pop and the requeue must not drop the popped entries
/// — the old `?` on the requeue's lock returned early and silently
/// dropped them from the queue AND never delivered them, while the
/// convergence gate kept reading `UpToDate` over un-applied blocks.
/// The choreography is deterministic: the drainer blocks at the
/// linked peer's lock (held by the test) after its pop; the test
/// then poisons the SOURCE world's lock and releases the peer; the
/// delivery fails (the peer carries no same-named resource) and the
/// requeue runs through the RECOVERED guard — the function returns
/// `Ok` and the queue keeps its entries, in order.
// The deliberate panic below IS the mechanism (poisoning the lock);
// clippy's panic lint is allowed for exactly this test.
#[test]
#[allow(clippy::panic)]
fn a_poisoned_window_never_drops_popped_entries() {
    let (base_a, world_a, runner_a, resource) = seeded_world("vol-poison-window");
    // The linked peer carries NO same-named resource: every
    // delivery fails, so the requeue path is the one under test.
    let world_b = Arc::new(Mutex::new(FakeDrbd::default()));
    link_replication_peers(&world_a, &world_b);

    promote(&runner_a, &base_a, &resource);
    let device = open_device(&world_a, SEED_MINOR).expect("open the source device");
    let first = device.write(0, &payload(0x0e)).expect("write 1");
    let second = device.write(1, &payload(0xe0)).expect("write 2");
    assert_eq!(resource_of(&world_a, &resource).apply_queue.len(), 2);

    // Hold the peer's lock: the drainer pops both entries and blocks
    // inside its critical section.
    let peer_guard = world_b.lock().expect("peer world");
    let drainer = {
        let world = Arc::clone(&world_a);
        std::thread::spawn(move || apply_peer_writes(&world, SEED_MINOR, u64::MAX))
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !resource_of(&world_a, &resource).apply_queue.is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the drainer never reached its pop"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    // Poison the SOURCE world's lock. The drainer holds no source
    // lock while blocked at the peer, so the poisoning thread
    // acquires it and panics — the lock lands poisoned exactly in
    // the pop-to-requeue window.
    let poison = Arc::clone(&world_a);
    let _ = std::thread::spawn(move || {
        let _guard = poison.lock().expect("acquire to poison");
        panic!("poison the source world lock");
    })
    .join();

    // Release the peer: the delivery fails (no resource at the
    // peer), and the requeue must RECOVER from the poison instead of
    // dropping the entries.
    drop(peer_guard);
    let outcome = drainer.join().expect("the drainer thread");
    outcome.expect("the requeue recovered from the poison and completed");

    // The queue keeps its entries, in the original order — read
    // through the same poison-recovering idiom the requeue uses.
    let guard = world_a
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = guard.resources.get(&resource).expect("resource");
    let seqs: Vec<u64> = state.apply_queue.iter().map(|entry| entry.seq).collect();
    assert_eq!(
        seqs,
        vec![first, second],
        "the popped entries were requeued, not dropped: {seqs:?}"
    );
}

// ------------------------------------------------------------- §2.4

/// A completing resync copies CONTENT, not just state: payload and
/// lineage reach the linked peer's map, and the apply queue drains
/// with the copy. The peer that needed no resync (already
/// `UpToDate`) keeps its content — a promotion over an in-sync pair
/// resyncs nothing.
#[test]
fn the_seeding_resync_copies_content_to_the_linked_peer() {
    let (base_a, world_a, runner_a, resource) = seeded_world("vol-resync");
    let base_b = host_dir();
    let world_b = Arc::new(Mutex::new(FakeDrbd::default()));
    seed_peer_volume(
        &base_b,
        &world_b,
        "vol-resync",
        MIB,
        ReplicationMode::A,
        SEED_MINOR,
        volvisor_drbd_testkit::SEED_PORT,
    );
    link_replication_peers(&world_a, &world_b);

    // The source writes with the window open, then the peer falls
    // behind (the not-`UpToDate` shape a resync exists for).
    promote(&runner_a, &base_a, &resource);
    let device = open_device(&world_a, SEED_MINOR).expect("open");
    device.write(2, &payload(0x33)).expect("write");
    device.write(9, &payload(0x44)).expect("write");
    assert_eq!(resource_of(&world_b, &resource).blocks.len(), 0);
    world_a
        .lock()
        .expect("world")
        .resources
        .get_mut(&resource)
        .expect("resource")
        .peer_disk = DiskState::Inconsistent;

    // The forced promotion's resync completes: content copied, queue
    // drained.
    runner_a
        .run(
            "drbdadm",
            &[
                "-c",
                &res_file(&base_a, &resource),
                "primary",
                "--force",
                &resource,
            ],
        )
        .expect("force");
    let source = resource_of(&world_a, &resource);
    assert_eq!(source.peer_disk, DiskState::UpToDate);
    assert!(
        source.apply_queue.is_empty(),
        "the resync drained the queue"
    );
    assert!(source.blocks.values().all(|block| block.applied_at_peer));
    let peer = resource_of(&world_b, &resource);
    assert_eq!(peer.blocks.len(), 2, "payload AND lineage were copied");
    assert_eq!(peer.blocks[&2].payload, payload(0x33));
    assert_eq!(peer.blocks[&9].payload, payload(0x44));
    assert_eq!(peer.blocks[&2].lineage, source.blocks[&2].lineage);
}

// ------------------------------------------------------------- raw paths

/// The raw paths are the injection/post-mortem surfaces: no role
/// check, no suspension check, the openers set never touched, and an
/// out-of-band write never enters the apply queue (below volvisor's
/// enforcement is below the replication path too — the peer never
/// sees rogue bytes).
#[test]
fn raw_paths_bypass_role_suspension_and_the_openers_set() {
    let (base, world, runner, resource) = seeded_world("vol-raw");
    // Secondary, unsuspended: a post-mortem read of the sparse fill.
    let fill = read_raw(&world, SEED_MINOR, 4).expect("post-mortem read");
    assert_eq!(fill.payload, [0_u8; BLOCK_SIZE]);

    // A rogue write below enforcement: lands in the map, no queue
    // entry, no opener, and the write path stays refusal-free for the
    // demote.
    write_raw(&world, SEED_MINOR, 4, &payload(0x99)).expect("rogue write");
    let state = resource_of(&world, &resource);
    assert_eq!(state.blocks[&4].payload, payload(0x99));
    assert_eq!(
        state.apply_queue,
        VecDeque::new(),
        "a rogue write never queues for the peer"
    );
    assert!(state.write_seq == 0, "no acknowledgment is minted");

    // Even suspended and Secondary — and the demote is unaffected
    // (the raw path holds nothing open).
    set_suspended(&runner, SEED_MINOR, true);
    write_raw(&world, SEED_MINOR, 5, &payload(0xaa)).expect("rogue write under suspension");
    let locked = world.lock().expect("world");
    assert!(locked.device_openers.is_empty());
    assert!(locked.open_devices.is_empty());
    drop(locked);
    let demote = runner
        .run(
            "drbdadm",
            &["-c", &res_file(&base, &resource), "secondary", &resource],
        )
        .expect("the demote ran");
    assert!(demote.success, "a raw write never blocks the demote");

    // The raw read sees exactly the rogue bytes (bytes are the ground
    // truth).
    assert_eq!(
        read_raw(&world, SEED_MINOR, 5).expect("read").payload,
        payload(0xaa)
    );

    // The enforced path is unchanged by all of this: the Secondary
    // device still refuses to open.
    let refusal = open_device(&world, SEED_MINOR)
        .err()
        .expect("still Secondary");
    assert_eq!(refusal.code, ApiErrorCode::InvalidState);
}

/// The raw write's shape is checked too: exactly one logical block,
/// typed refusal otherwise (an injection that cannot say what it
/// wrote would corrupt the oracle's ground truth silently).
#[test]
fn raw_writes_refuse_a_wrong_size_payload() {
    let (_base, world, _runner, _resource) = seeded_world("vol-rawsize");
    let error =
        write_raw(&world, SEED_MINOR, 0, &[0_u8; 8]).expect_err("one logical block exactly");
    assert_eq!(error.code, ApiErrorCode::InvalidRequest);
}

/// A block map is Block-shaped ground truth: the type carries
/// payload, lineage and the apply flag (the compile-time shape the
/// oracle's verdicts are written against).
#[test]
fn the_block_type_carries_payload_lineage_and_apply_state() {
    let block = Block {
        payload: payload(0x5a),
        lineage: volvisor_drbd_testkit::GiSet::for_resource("shape"),
        applied_at_peer: false,
    };
    let round = block.clone();
    assert_eq!(block, round);
    assert_eq!(block.payload[0], 0x5a);
    assert!(!block.applied_at_peer);
}

/// The steady-state protocol-A transport (P5 plan §2.1): while the
/// link runs, a live writer's queued writes reach the linked peer's
/// map within a bounded real-time lag (the convergence gate's
/// premise — "asynchronous peer apply is real time"); once the link
/// stops, the queue freezes exactly as [`apply_peer_writes`'s] lag
/// shaping needs (a stopped transport is a frozen window, not a
/// closed one).
#[test]
fn the_peer_transport_drains_with_a_lag_and_freezes_when_stopped() {
    use std::time::{Duration, Instant};

    use volvisor_drbd_testkit::spawn_peer_transport;

    let (base, world_a, _runner, resource) = seeded_world("vol-transport");
    let base_b = host_dir();
    let world_b = Arc::new(Mutex::new(FakeDrbd::default()));
    seed_peer_volume(
        &base_b,
        &world_b,
        "vol-transport",
        MIB,
        ReplicationMode::A,
        SEED_MINOR,
        volvisor_drbd_testkit::SEED_PORT,
    );
    link_replication_peers(&world_a, &world_b);

    // The source is the writer (the seeded plain promote).
    let runner = FakeDrbd::runner(&world_a);
    promote(&runner, &base, &resource);
    let device = open_device(&world_a, SEED_MINOR).expect("open device");

    // No transport: a queued write stays queued (the window is open).
    device.write(0, &[1_u8; BLOCK_SIZE]).expect("write 1");
    assert!(
        read_raw(&world_b, SEED_MINOR, 0)
            .expect("read peer")
            .payload
            .iter()
            .all(|byte| *byte == 0),
        "without the link the write has not reached the peer"
    );

    // The link runs: the write drains within a generous bound over
    // the 1-ms lag (real time, so the bound is slack, not tight).
    let transport = spawn_peer_transport(&world_a, SEED_MINOR, Duration::from_millis(1));
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let drained = read_raw(&world_b, SEED_MINOR, 0)
            .expect("read peer")
            .payload[0]
            == 1;
        if drained {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the transport drained the queued write within the bound"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    // The link stops: the window freezes (a later write stays
    // queued at the source, absent at the peer).
    transport.join();
    device.write(1, &[2_u8; BLOCK_SIZE]).expect("write 2");
    assert!(
        read_raw(&world_b, SEED_MINOR, 1)
            .expect("read peer")
            .payload
            .iter()
            .all(|byte| *byte == 0),
        "after the link stops the queue no longer drains"
    );
}

/// A FAILED delivery requeues (§2.3's hinge): a linked peer world
/// alive but without the same-named resource is a drain that cannot
/// land — without the requeue, the source-side flags would flip and
/// the gate would read `UpToDate` over blocks that never arrived,
/// exactly the lie the coupling rule forbids. With it, the entries go
/// back to the queue (front-first, order preserved), the flags stay
/// unflipped, and the gate stays closed until the peer can receive.
#[test]
fn a_failed_delivery_requeues_and_keeps_the_gate_closed() {
    let (base, world, runner, resource) = seeded_world("vol-faildel");
    // The linked peer world exists but carries NO resource of this
    // name (an absent replica, not a down link — the down-link shape
    // is the transport test above).
    let world_b = Arc::new(Mutex::new(FakeDrbd::default()));
    link_replication_peers(&world, &world_b);

    promote(&runner, &base, &resource);
    let device = open_device(&world, SEED_MINOR).expect("open");
    let ack = device.write(0, &payload(0x3c)).expect("write");

    // The drain cannot deliver: the entries requeue, the flags stay
    // unflipped, the window stays open.
    apply_peer_writes(&world, SEED_MINOR, ack).expect("the drain attempted");
    let state = resource_of(&world, &resource);
    assert_eq!(
        state.apply_queue.len(),
        1,
        "the refused entries requeue instead of dropping"
    );
    assert!(
        !state.blocks[&0].applied_at_peer,
        "no source-side flag flips on a refused drain"
    );
    let tokens = status(&runner, &resource);
    assert!(
        tokens.contains("peer-disk:Inconsistent"),
        "the gate stays closed: {tokens}"
    );
    assert!(tokens.contains("replication:SyncSource"), "{tokens}");

    // The peer gains its resource (the replica is seeded): the SAME
    // drain command now delivers — the queue was never lost, only
    // refused; order and content are intact.
    let base_b = host_dir();
    seed_peer_volume(
        &base_b,
        &world_b,
        "vol-faildel",
        MIB,
        ReplicationMode::A,
        SEED_MINOR,
        volvisor_drbd_testkit::SEED_PORT,
    );
    apply_peer_writes(&world, SEED_MINOR, ack).expect("the retried drain");
    let state = resource_of(&world, &resource);
    assert!(
        state.apply_queue.is_empty(),
        "the retried drain delivers the requeued entries"
    );
    assert!(state.blocks[&0].applied_at_peer);
    assert_eq!(
        read_raw(&world_b, SEED_MINOR, 0)
            .expect("read the peer's bytes")
            .payload,
        payload(0x3c),
        "the delivered content is the write's, not a zero fill"
    );
    let tokens = status(&runner, &resource);
    assert!(
        tokens.contains("peer-disk:UpToDate"),
        "the gate opens only on real delivery: {tokens}"
    );
}

/// The transport honors the world's `peer_online` flag (the §5.5
/// partition injection): while the link is flagged down the
/// transport delivers nothing — the queue stays open and the gate
/// stays closed — and healing the flag lets the same lagged transport
/// drain. A partition must never read as caught-up.
#[test]
fn the_transport_honors_a_partitioned_link() {
    use std::time::{Duration, Instant};

    use volvisor_drbd_testkit::spawn_peer_transport;

    let (base, world, runner, resource) = seeded_world("vol-part");
    let base_b = host_dir();
    let world_b = Arc::new(Mutex::new(FakeDrbd::default()));
    seed_peer_volume(
        &base_b,
        &world_b,
        "vol-part",
        MIB,
        ReplicationMode::A,
        SEED_MINOR,
        volvisor_drbd_testkit::SEED_PORT,
    );
    link_replication_peers(&world, &world_b);

    promote(&runner, &base, &resource);
    let device = open_device(&world, SEED_MINOR).expect("open");

    // The partition is flagged BEFORE the transport can deliver
    // anything; the transport keeps running (the link is up, the
    // FLAG is down — the in-place partition).
    world.lock().expect("world").peer_online = false;
    let transport = spawn_peer_transport(&world, SEED_MINOR, Duration::from_millis(1));
    device.write(0, &payload(0x77)).expect("write");

    // A generous negative window — 100× the transport's lag — over
    // which nothing may arrive. (A bare wait is the only way to
    // prove absence; it is bounded, and the heal below proves the
    // same transport CAN deliver when the flag allows.)
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        read_raw(&world_b, SEED_MINOR, 0)
            .expect("read peer")
            .payload
            .iter()
            .all(|byte| *byte == 0),
        "the partitioned transport delivered nothing"
    );
    let state = resource_of(&world, &resource);
    assert_eq!(
        state.apply_queue.len(),
        1,
        "the queue stays open over the partition"
    );
    // The partition's own honest shape: the real `WFConnection`
    // token (the link is down), and no caught-up token anywhere —
    // `peer-disk:UpToDate` must not appear over undelivered blocks.
    let tokens = status(&runner, &resource);
    assert!(
        tokens.contains("connection:WFConnection"),
        "the partition renders the real disconnect token: {tokens}"
    );
    assert!(
        !tokens.contains("peer-disk:UpToDate"),
        "a partition never reads as caught-up: {tokens}"
    );

    // The heal: the flag up, the same transport drains within the
    // bounded lag.
    world.lock().expect("world").peer_online = true;
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let drained = read_raw(&world_b, SEED_MINOR, 0)
            .expect("read peer")
            .payload[0]
            == 0x77;
        if drained {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the healed transport drained within the bound"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    transport.join();
}
