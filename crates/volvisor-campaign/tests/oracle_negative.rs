//! The oracle's negative proofs (P5 plan §2.2/§2.3, the stage-A
//! round-1 review's findings 4 and 5): the verifier and the boundary
//! cross-check must be ABLE to fail — a campaign oracle that cannot
//! fail proves nothing. Each test here plants the exact violation
//! the plan's letter names — a verified side with acknowledged blocks
//! missing or corrupt (the COMPLETE-with-missing-blocks shape, and
//! the corruption class), and a barrier recorded before the writes it
//! should have covered — and asserts the oracle reports it.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use volvisor_campaign::oracle::{AckedWrite, WRITER_ID, boundary_skew, tag_block, verify_against};
use volvisor_drbd::resgen::res_file_path;
use volvisor_drbd::state::ReplicationMode;
use volvisor_drbd::{CommandRunner, FakeRunner};
use volvisor_drbd_testkit::{
    FakeDrbd, SEED_MINOR, leak_tempdir, open_device, seed_volume_with_identity, write_raw,
};

/// One mebibyte of device surface (the seeded fixture's size).
const MIB: u64 = 1 << 20;

/// A freshly created host directory (the `host_dir` shape).
fn host_dir() -> PathBuf {
    let base = leak_tempdir();
    fs::create_dir_all(base.join("drbd.d")).expect("config dir");
    base
}

/// One seeded single world (the data-path test pattern), promoted to
/// Primary so the enforced device path accepts writes.
fn promoted_world(volume: &str) -> (PathBuf, Arc<Mutex<FakeDrbd>>, Arc<FakeRunner>, String) {
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
    runner
        .run(
            "drbdadm",
            &[
                "-c",
                &res_file_path(&base.join("drbd.d"), &resource).to_string_lossy(),
                "primary",
                &resource,
            ],
        )
        .expect("primary");
    (base, world, runner, resource)
}

/// Acknowledge `count` tagged writes at fresh block indices through
/// the enforced device path, recording the ack journal exactly as the
/// rig's writer does.
fn acknowledge(world: &Arc<Mutex<FakeDrbd>>, count: u64) -> Vec<AckedWrite> {
    let handle = open_device(world, SEED_MINOR).expect("open the device");
    let mut acked = Vec::new();
    for seq in 1..=count {
        let index = seq; // fresh blocks: the write's own sequence is the index
        let ack = handle
            .write(index, &tag_block(seq, WRITER_ID))
            .expect("write");
        acked.push(AckedWrite {
            seq,
            ack,
            index,
            clock: seq, // a monotonic stand-in clock; the cross-check test below uses its own
        });
    }
    acked
}

/// The verdict flags the plan's marquee violation: acknowledged
/// blocks MISSING and CORRUPT at the verified side (§2.2's three
/// classes), and `prefix_intact` is false for both — a verifier
/// regression to always-pass fails here loudly.
#[test]
fn the_verdict_flags_missing_and_corrupted_blocks() {
    let (_base, world, _runner, resource) = promoted_world("vol-neg-1");
    let acked = acknowledge(&world, 6);

    // The clean shape first: every acknowledged write present and
    // crc-correct — the verdict the COMPLETE rows assert.
    let clean = verify_against(&world, SEED_MINOR, &acked, WRITER_ID);
    assert_eq!(clean.present, 6);
    assert_eq!(clean.corrupted, 0);
    assert_eq!(clean.missing, 0);
    assert!(clean.prefix_intact());

    // Plant the violation: block 2 never arrived (the sparse zero
    // fill — the missing class), block 4 landed with wrong content
    // (the corruption class, a crc-valid tag of the WRONG sequence).
    world
        .lock()
        .expect("world")
        .resources
        .get_mut(&resource)
        .expect("resource")
        .blocks
        .remove(&2);
    write_raw(&world, SEED_MINOR, 4, &tag_block(99, WRITER_ID)).expect("plant the corrupt block");

    let verdict = verify_against(&world, SEED_MINOR, &acked, WRITER_ID);
    assert_eq!(verdict.acknowledged, 6);
    assert_eq!(verdict.present, 4, "the untouched four verify clean");
    assert_eq!(verdict.missing, 1, "the absent block is the missing class");
    assert_eq!(
        verdict.corrupted, 1,
        "the wrong-content block is the corruption class"
    );
    assert!(
        !verdict.prefix_intact(),
        "the acknowledged-prefix property is false for both classes"
    );
}

/// The boundary cross-check fires on its failure mode (§2.3 rule 1):
/// a barrier recorded BEFORE the last acknowledged write — writes
/// the barrier should have covered slipped past it — is the negative
/// skew the rows treat as a violation. A cross-check regression to
/// always-zero fails here.
#[test]
fn the_boundary_cross_check_fires_on_an_early_barrier() {
    let acked = vec![
        AckedWrite {
            seq: 1,
            ack: 1,
            index: 1,
            clock: 10,
        },
        AckedWrite {
            seq: 2,
            ack: 2,
            index: 2,
            clock: 20,
        },
        AckedWrite {
            seq: 3,
            ack: 3,
            index: 3,
            clock: 30,
        },
    ];
    // The violation: the barrier's recorded clock (25) precedes the
    // last acknowledged write's (30) — a negative skew.
    assert_eq!(boundary_skew(&acked, 25), Some(-5));
    // The honest shapes: the barrier at, or after, the last ack.
    assert_eq!(boundary_skew(&acked, 30), Some(0));
    assert_eq!(boundary_skew(&acked, 40), Some(10));
    // No acknowledged writes: nothing to cross-check.
    assert_eq!(boundary_skew(&[], 30), None);
}
