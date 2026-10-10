//! The rig-machinery regressions (the comprehensive review's S1
//! and S3): the stamp clock's monotonicity and the kill flag's
//! per-launch lifetime. These are not §9 scenario rows — they pin
//! the HARNESS properties the rows' evidence depends on, so a rig
//! regression cannot silently blunt a row's check.
//!
//! S1: the §2.3 rule-1 boundary cross-check must be LIVE in every
//! row that emits it. The old design let the writer advance the
//! shared (lease-domain) clock under a `CLOCK_ADVANCE_CAP` of 40
//! ticks; past the cap the clock froze, every barrier stamp and
//! every ack read the same frozen value, and `boundary_skew_ticks:
//! 0` was a tautology — an early barrier (the honeypot the check
//! exists for) was invisible past the freeze. The writer now
//! advances a dedicated STAMP clock (never the lease clock), one
//! tick per ack, unbounded; the coordinator's history stamps read
//! the same clock.
//!
//! S3: `is_killed()` is per-LAUNCH state. The old sticky flag (set
//! once, never cleared by `restart()`) made every post-restart poll
//! vacuously true — row 14 kills the same daemon every 5th cycle,
//! so from cycle 6 the flag could mask a second kill that never
//! armed or fired. `Reply::Died` remains the primary kill evidence;
//! the flag is the polling convenience, and it resets with the
//! relaunch.

// Test target (the e2e precedent): invariant assertions may
// expect/unwrap; the rig's helpers are already bounded.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use serde_json::json;
use volvisor_campaign::oracle::{WRITER_ID, WriterHandle, boundary_skew, verify_against};
use volvisor_campaign::rig::{
    POLL_BOUND, POLL_STEP, Reply, Rig, campaign_rig, post_prepare, post_transfer,
};
use volvisor_drbd_testkit::{PEER_NODE, PeerTransport, SEED_MINOR, spawn_peer_transport};

/// The steady-state transport's lag (§2.1 — the same shape as the
/// rows' scenario openings).
const TRANSPORT_LAG: Duration = Duration::from_millis(1);
/// The scenario opening's warmup (§2.2's continuous workload).
const WRITER_WARMUP: Duration = Duration::from_millis(80);
/// The old `CLOCK_ADVANCE_CAP`: the freeze point the regression
/// must drive PAST (60 acks > 40 ticks).
const OLD_FREEZE_POINT: usize = 40;

/// The scenario opening (the rows' shared shape): rig, live writer
/// over the rig's STAMP clock, steady-state transport.
async fn scenario(prefix: &str) -> (Rig, WriterHandle, PeerTransport) {
    let rig = campaign_rig(&format!("vm-{prefix}"), &format!("vol-{prefix}")).await;
    let writer = WriterHandle::start(
        &rig.world_a,
        &rig.vmm_a,
        &rig.vm,
        SEED_MINOR,
        &rig.stamp_clock,
    );
    let transport = spawn_peer_transport(&rig.world_a, SEED_MINOR, TRANSPORT_LAG);
    tokio::time::sleep(WRITER_WARMUP).await;
    (rig, writer, transport)
}

/// The prepare act (the rows' shape: admin route, 201).
async fn prepare(rig: &Rig, mig: &str) {
    let (status, body) = post_prepare(
        rig.a.addr,
        &json!({
            "migration_id": mig,
            "vm_id": rig.vm,
            "target_host": PEER_NODE,
            "volume_ids": [rig.volume],
            "expected_generations": [2],
        }),
    )
    .await
    .served("prepare");
    assert_eq!(status, 201, "prepare: {body}");
}

/// S1: the stamp clock stays strictly monotonic per ack for the
/// whole scenario, and the boundary cross-check's honeypot stays
/// live PAST the old freeze point. A live writer acks more than the
/// old `CLOCK_ADVANCE_CAP` (40) writes; the journal's clock values
/// must be strictly increasing (one unique tick per ack, no
/// freeze), the lease clock must be untouched by the writer's I/O
/// (the decoupling that replaced the cap), and a barrier stamped
/// before the last acknowledged write must read a NEGATIVE skew at
/// that tick count — under the old design the same shape read 0
/// (invisible).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_stamp_clock_stays_monotonic_and_the_honeypot_live_past_the_old_freeze() {
    let (rig, writer, _transport) = scenario("stamp").await;

    // Past the old freeze point (bounded wait): the writer acks at
    // ~250 writes/s, so 60 acks land well inside the bound.
    let ack_target = OLD_FREEZE_POINT + 20;
    let deadline = tokio::time::Instant::now() + POLL_BOUND;
    loop {
        let acked = writer.acknowledged();
        if acked.len() >= ack_target {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the writer never reached {ack_target} acks ({} so far) — the monotonicity \
             regression cannot run",
            acked.len()
        );
        tokio::time::sleep(POLL_STEP).await;
    }
    let (acked, _boundary) = writer.join().await;
    assert!(
        acked.len() >= ack_target,
        "the writer acknowledged past the old freeze point: {}",
        acked.len()
    );

    // Strictly monotonic per ack: every recorded clock value is the
    // exact tick ITS ack advanced the stamp clock to (unique under
    // `fetch_add`), so consecutive acks strictly increase — the old
    // frozen-clock shape (a run of identical values past the cap)
    // fails here.
    for pair in acked.windows(2) {
        assert!(
            pair[1].clock > pair[0].clock,
            "the stamp clock advanced per ack: {} then {}",
            pair[0].clock,
            pair[1].clock
        );
    }

    // The decoupling: the writer never advanced the LEASE clock (the
    // property the old cap enforced by freezing BOTH sides — the
    // split preserves it structurally).
    assert_eq!(
        rig.clock.load(std::sync::atomic::Ordering::SeqCst),
        volvisor_campaign::rig::START,
        "the writer's I/O never touches the lease clock"
    );

    // The honeypot, live past the old freeze: a barrier stamped
    // three acks before the last acknowledged write is a negative
    // skew (writes the barrier should have covered slipped past it)
    // — under the old design both sides read the same frozen value
    // and this read 0.
    let early_barrier = acked[acked.len() - 3].clock;
    let skew = boundary_skew(&acked, early_barrier).expect("the writer acknowledged writes");
    assert!(
        skew < 0,
        "an early barrier past the old freeze point is caught, not frozen over: {skew}"
    );
    // The honest shape at the same tick count: a barrier stamped at
    // the last ack's tick reads zero (the barrier covers it).
    let at_last = acked.last().expect("acked writes").clock;
    assert_eq!(
        boundary_skew(&acked, at_last),
        Some(0),
        "a barrier at the last ack's tick is the honest zero"
    );
}

/// S3: the kill flag is per-LAUNCH. Kill the source at the transfer
/// op (`Reply::Died` is the primary evidence), restart it — the
/// flag must read false over the new launch — then kill it AGAIN:
/// the flag must read true only because the SECOND kill fired, and
/// the source's bytes must survive both kills (the rollback
/// semantics the rows rely on).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_kill_flag_is_per_launch_not_per_daemon() {
    let (mut rig, writer, _transport) = scenario("kflag").await;
    let acked_before: Vec<_> = writer
        .acknowledged()
        .into_iter()
        .map(|write| write.seq)
        .collect();
    assert!(!acked_before.is_empty(), "the writer warmed up");

    // The first kill: the transfer op dies before its outcome.
    prepare(&rig, "mig-kflag").await;
    rig.a.core.crash.arm(
        volvisor_api::op_kinds::OP_MIGRATION_TRANSFER,
        volvisor_api::crash::CrashPoint::BeforeOutcome,
    );
    let reply = post_transfer(rig.a.addr, "mig-kflag").await;
    assert!(
        matches!(reply, Reply::Died),
        "the first kill lands before the 202"
    );
    assert!(rig.a.is_killed(), "the first kill's flag is set");
    rig.a.restart().await;

    // The flag resets with the relaunch: the old launch's group is
    // awaited dead inside the restart, so a `true` here could only
    // be the sticky residue the fix removes.
    assert!(
        !rig.a.is_killed(),
        "the restart's launch carries a fresh kill flag"
    );

    // The second kill on the SAME daemon: the flag must read true
    // only because THIS kill fired (under the sticky flag the
    // assertion could not distinguish).
    prepare(&rig, "mig-kflag-2").await;
    rig.a.core.crash.arm(
        volvisor_api::op_kinds::OP_MIGRATION_TRANSFER,
        volvisor_api::crash::CrashPoint::BeforeOutcome,
    );
    let reply = post_transfer(rig.a.addr, "mig-kflag-2").await;
    assert!(
        matches!(reply, Reply::Died),
        "the second kill lands before the 202"
    );
    assert!(rig.a.is_killed(), "the second kill's flag is set");

    // The rollback semantics survive the double kill: the source
    // keeps every acknowledged write.
    let (acked, _boundary) = writer.join().await;
    let verdict = verify_against(&rig.world_a, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        verdict.prefix_intact(),
        "the source keeps every acknowledged write across both kills: {verdict:?}"
    );
}
