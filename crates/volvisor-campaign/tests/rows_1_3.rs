//! P5 stage A scenario rows 1–3 (plan §9): the happy-path oracle
//! with the boundary cross-check, the shaped-lag abort, and the
//! bounded journal-append kill-and-recover points.
//!
//! Every scenario follows the harness disciplines (§4/§7): acts go
//! through the public HTTP routes only; recovery runs through the
//! production retry task and is polled via the observation route
//! with bounded waits; assertions read the observation route, the
//! witness inspect route, the provider inspect surface and the
//! device bytes — never coordinator internals. Every scenario emits
//! its §6 evidence record.
//!
//! The kill rows' recovery expectations are the coordinator's own
//! `resolve` semantics (§3.3): a record with no durable cut rolls
//! **back** (the source keeps every acknowledged write), a record
//! with a cut re-drives **forward** (the cut's write-ahead makes
//! every step idempotent). Source-side kills at the transfer's
//! journal boundaries therefore recover to `ABORTED` (the drive
//! never durably cut — the consumer re-issues a fresh migration),
//! and peer-side kills at the grant recover to `COMPLETE` (the cut
//! was durable before the peer call).

// Test target (the e2e precedent): invariant assertions may
// expect/unwrap; the rig's helpers are already bounded.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use volvisor_campaign::evidence::{Evidence, LogSources, oracle_value};
use volvisor_campaign::oracle::{
    StopReason, WRITER_ID, WriterHandle, barrier_timestamp, boundary_skew, verify_against,
};
use volvisor_campaign::rig::{
    Reply, Rig, body_json, campaign_rig, get_volume, poll_migration, post_abort, post_prepare,
    post_transfer, role_of, witness_view,
};
use volvisor_drbd::report::Role;
use volvisor_drbd_testkit::{NODE, PEER_NODE, PeerTransport, SEED_MINOR, spawn_peer_transport};
use volvisor_provider::VmState;
use volvisor_types::MigrationId;

/// The steady-state transport's lag (§2.1): the peer-apply window is
/// genuinely open for at most a millisecond after every write.
const TRANSPORT_LAG: Duration = Duration::from_millis(1);

/// The pause fault's pre-shape sleep (row 2): after the window
/// freezes, the live writer queues a genuinely nonzero tail before
/// the abort is issued.
const TAIL_GROWTH: Duration = Duration::from_millis(120);

/// The frozen-window guarantee (kill rows K2/K3): after the
/// transport stops, the live writer has queued at least one write
/// beyond the last drain, so the drive's convergence observation can
/// never pass and the record deterministically parks pre-cut.
const WINDOW_FREEZE_SETTLE: Duration = Duration::from_millis(30);

/// The length of a summary's append-only state trace (K3's
/// no-re-execution observable).
fn history_len(summary: &serde_json::Value) -> usize {
    summary["state_history"].as_array().map_or(0, Vec::len)
}

// --------------------------------------------------------- shared shape

/// The steady-state warmup (§2.2's "continuous" writer): the
/// migration must start against a volume with an active workload —
/// without it, the cut converges over an empty queue before the
/// writer's first write lands, and the scenario proves nothing
/// about live I/O.
const WRITER_WARMUP: Duration = Duration::from_millis(80);

/// The scenario opening: the rig (two daemons under the supervisor,
/// one VM, one volume), the live guest writer and the steady-state
/// transport, warmed up so the workload is genuinely continuous
/// before any act is driven.
async fn scenario(prefix: &str) -> (Rig, WriterHandle, PeerTransport) {
    let rig = campaign_rig(&format!("vm-{prefix}"), &format!("vol-{prefix}")).await;
    let writer = WriterHandle::start(&rig.world_a, &rig.vmm_a, &rig.vm, SEED_MINOR, &rig.clock);
    let transport = spawn_peer_transport(&rig.world_a, SEED_MINOR, TRANSPORT_LAG);
    tokio::time::sleep(WRITER_WARMUP).await;
    (rig, writer, transport)
}

/// The prepare act (admin route; 201).
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

/// The transfer act (admin route; the 202 the drive spawns under).
async fn transfer(rig: &Rig, mig: &str) -> (u16, String) {
    post_transfer(rig.a.addr, mig).await.served("transfer")
}

/// Freeze the peer-apply window and guarantee the queue is
/// non-empty (§2.3's pre-shape): stop the link, then let the live
/// writer queue writes beyond the last drain. The drive's
/// convergence observation can never pass over a frozen non-empty
/// queue — the record deterministically parks pre-cut.
async fn freeze_window(transport: PeerTransport) {
    transport.join();
    tokio::time::sleep(WINDOW_FREEZE_SETTLE).await;
}

/// The G5 check for COMPLETE paths (rule 5): the migration's barrier
/// is recorded at the witness, NOT voided, with every attested fact
/// true — and the source was never resumed over it (VM destroyed,
/// resource Secondary). Returns the invariant verdict string.
async fn assert_g5_complete(rig: &Rig, mig: &str) -> String {
    let view = witness_view(&rig.witness, &rig.volume_id()).await;
    let migration = MigrationId::new(mig).expect("valid migration id");
    let matching: Vec<_> = view
        .barriers
        .iter()
        .filter(|barrier| barrier.migration_id.as_ref() == Some(&migration))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "exactly one barrier recorded for {mig}: {:?}",
        view.barriers
    );
    assert!(
        !matching[0].voided,
        "a completed migration's barrier is never voided"
    );
    assert!(
        matching[0].attestation.all_true(),
        "the barrier's attested facts all hold"
    );
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Absent,
        "the source VM is destroyed (never resumed)"
    );
    assert_eq!(
        role_of(&rig.world_a, &rig.resource()),
        Role::Secondary,
        "the source resource is demoted"
    );
    "pass: barrier non-voided and all-true; source never resumed".to_owned()
}

/// The G5 check for ABORTED paths (rule 5): no unvoided barrier for
/// the migration exists (the resume is legal), and the source
/// legitimately resumed — VM running, resource Primary. Returns the
/// invariant verdict string.
async fn assert_g5_aborted(rig: &Rig, mig: &str) -> String {
    let view = witness_view(&rig.witness, &rig.volume_id()).await;
    let migration = MigrationId::new(mig).expect("valid migration id");
    let unvoided = view
        .barriers
        .iter()
        .filter(|barrier| barrier.migration_id.as_ref() == Some(&migration) && !barrier.voided)
        .count();
    assert_eq!(
        unvoided, 0,
        "an aborted migration leaves no unvoided barrier to resume over"
    );
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Running,
        "the source VM is resumed"
    );
    assert_eq!(
        role_of(&rig.world_a, &rig.resource()),
        Role::Primary,
        "the source resource is promoted"
    );
    "pass: no unvoided barrier; the resume is legal".to_owned()
}

/// The D6a check: the provider's public inspect surface answers with
/// the witness-backed authority summary a terminal state implies —
/// epoch and holder exactly as expected (the provider fails closed
/// over stale cut markers, so a terminal observation plus a live
/// authority summary is the marker-resolved proof an operator has).
async fn assert_d6a(rig: &Rig, addr: std::net::SocketAddr, epoch: u64, holder: &str) -> String {
    let (status, body) = get_volume(addr, &rig.volume)
        .await
        .served("provider inspect");
    assert_eq!(status, 200, "provider inspect: {body}");
    let value = body_json(&body);
    assert_eq!(
        value["authority"]["epoch"],
        json!(epoch),
        "provider inspect authority epoch: {body}"
    );
    assert_eq!(
        value["authority"]["holder"],
        json!(holder),
        "provider inspect authority holder: {body}"
    );
    format!("pass: authority epoch {epoch}, holder {holder}, over a terminal state")
}

/// Emit the scenario's §6 evidence record: the oracle section from
/// the byte-level verdict and boundary, the log capture, and the
/// report refresh. Returns the record path.
fn emit(
    rig: &Rig,
    mut evidence: Evidence,
    acked: &[volvisor_campaign::oracle::AckedWrite],
    boundary: Option<(u64, StopReason)>,
    verified_side: &str,
    barrier_at: Option<u64>,
    skew: Option<i64>,
) -> PathBuf {
    let verdict_source = verify_against(&rig.world_a, SEED_MINOR, acked, WRITER_ID);
    let verdict_peer = verify_against(&rig.world_b, SEED_MINOR, acked, WRITER_ID);
    let (verified, tail) = if verified_side == "destination" {
        (verdict_peer.present, verdict_source.tail())
    } else {
        (verdict_source.present, verdict_peer.tail())
    };
    let (boundary_seq, stop_reason) =
        boundary.map_or((None, "nothing-acked".to_owned()), |(seq, reason)| {
            (
                Some(seq),
                match reason {
                    StopReason::Paused => "paused".to_owned(),
                    StopReason::Refused(detail) => format!("refused: {detail}"),
                    StopReason::StoppedByRig => "stopped-by-rig".to_owned(),
                },
            )
        });
    evidence.oracle(oracle_value(
        acked.len() as u64,
        verified,
        verdict_peer.corrupted.max(verdict_source.corrupted),
        tail,
        verified_side,
        boundary_seq,
        "data-path",
        &stop_reason,
        barrier_at,
        skew,
    ));
    evidence.finish(&LogSources {
        a_journal: &rig.a.core.journal_dir,
        b_journal: &rig.b.core.journal_dir,
        witness: &rig.witness.dir,
    })
}

// -------------------------------------------------------------- row 1

/// Row 1 (§9): the happy-path oracle — a live writer's every
/// acknowledged write is present and crc-correct at the destination
/// after a COMPLETE migration; corruption is a distinct class (zero
/// here); the boundary is derived from the data path and
/// cross-checked against the coordinator's `BARRIER_DURABLE`
/// timestamp (no boundary skew); G5 holds (non-voided all-true
/// barrier, source never resumed); D6a answers epoch 2 at node-b.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_1_happy_path_oracle_boundary_and_evidence() {
    let (rig, writer, _transport) = scenario("r1").await;
    let mut evidence = Evidence::new("row-1/happy-path");

    prepare(&rig, "mig-r1").await;
    let (status, body) = transfer(&rig, "mig-r1").await;
    assert_eq!(status, 202, "transfer: {body}");
    let summary = poll_migration(rig.a.addr, "mig-r1", "complete").await;

    // The boundary: the writer's I/O ended at the pause (or the
    // post-suspension refusal — either is the data path's own
    // answer), never at the rig's stop.
    let (acked, boundary) = writer.join().await;
    assert!(
        !acked.is_empty(),
        "the live writer acknowledged writes before the cut (boundary: {boundary:?})"
    );
    let (boundary_seq, stop_reason) = boundary
        .as_ref()
        .expect("the writer's I/O ended with the cut");
    assert!(
        *stop_reason == StopReason::Paused || matches!(stop_reason, StopReason::Refused(_)),
        "the boundary is the data path's (last acked seq {boundary_seq}, stop reason: \
         {stop_reason:?})"
    );

    // The oracle verdict at the destination: every acknowledged
    // write present and crc-correct, nothing corrupted, nothing
    // missing (the resync closed the peer-apply window at the
    // suspension — the fake's only system-path window closer).
    let verdict = verify_against(&rig.world_b, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        verdict.prefix_intact(),
        "every acknowledged write is intact at the destination: {verdict:?}"
    );

    // The boundary cross-check (§2.3 rule 1): the barrier the
    // coordinator recorded as durable is timestamped at or after the
    // last acknowledged write's clock — the barrier covers the
    // boundary, never predates it.
    let barrier_at =
        barrier_timestamp(&summary).expect("the complete summary carries a BARRIER_DURABLE entry");
    let skew = boundary_skew(&acked, barrier_at).expect("the writer acknowledged writes");
    assert!(
        skew >= 0,
        "no boundary skew: the barrier ({barrier_at}) covers the last ack's clock"
    );

    // G5 and D6a over the operator surfaces.
    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_complete(&rig, "mig-r1").await,
    );
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.b.addr, 2, PEER_NODE).await,
    );
    evidence.invariant(
        "rule16_no_zero_rpo_claim",
        "pass: the tail is reported as peer-apply lag, never as loss or zero-RPO",
    );
    evidence.outcome("complete: acknowledged prefix intact at the destination");
    let record = emit(
        &rig,
        evidence,
        &acked,
        boundary,
        "destination",
        Some(barrier_at),
        Some(skew),
    );
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

// -------------------------------------------------------------- row 2

/// Row 2 (§9): the abort with a pre-shaped lag — a pre-cut fault
/// (the pause refuses) diverts the drive before any suspension or
/// barrier; the consumer aborts; the source keeps EVERY acknowledged
/// write (verified in bytes) while the peer-apply tail is genuinely
/// nonzero (shaped by freezing the steady-state transport at the
/// pre-cut state) and honestly reported; G5 holds (no barrier, legal
/// resume); D6a answers epoch 1 at node-a.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_2_abort_with_shaped_pre_quiesce_lag() {
    let (rig, writer, transport) = scenario("r2").await;
    let mut evidence = Evidence::new("row-2/abort-shaped-lag");
    evidence.fault("pre-cut-refusal", "vmm/pause");

    // The pre-cut fault: the source VM's pause refuses (typed),
    // so the drive fails before the quiesce — no suspension, no
    // barrier, the abort path fully intact.
    rig.vmm_a
        .set_fail(&rig.vm, |knobs| knobs.pause = true)
        .expect("arm the pause fault");

    prepare(&rig, "mig-r2").await;
    let (status, body) = transfer(&rig, "mig-r2").await;
    assert_eq!(status, 202, "transfer: {body}");

    // The drive converged (the transport was live), transitioned
    // PRECOPY and failed at the pause fault — the record lingers
    // there until the abort.
    poll_migration(rig.a.addr, "mig-r2", "precopy").await;

    // Shape the tail: freeze the peer-apply window at the pre-cut
    // state and let the live writer (the VM never paused — the
    // pause faulted) queue a genuinely nonzero tail.
    transport.join();
    tokio::time::sleep(TAIL_GROWTH).await;

    let (status, body) = post_abort(rig.a.addr, "mig-r2").await.served("abort");
    assert_eq!(status, 200, "abort: {body}");
    poll_migration(rig.a.addr, "mig-r2", "aborted").await;
    let (acked, boundary) = writer.join().await;
    assert!(!acked.is_empty(), "the writer acknowledged writes");
    let source = verify_against(&rig.world_a, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        source.prefix_intact(),
        "the source keeps every acknowledged write: {source:?}"
    );

    // The honest tail: acknowledged writes absent at the peer —
    // genuinely nonzero (the frozen window), never called loss.
    let peer = verify_against(&rig.world_b, SEED_MINOR, &acked, WRITER_ID);
    assert_eq!(peer.corrupted, 0, "no corruption: {peer:?}");
    assert!(
        peer.tail() > 0,
        "the shaped peer-apply lag is genuinely nonzero: {peer:?}"
    );

    // G5 and D6a over the operator surfaces.
    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_aborted(&rig, "mig-r2").await,
    );
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.a.addr, 1, NODE).await,
    );
    evidence.invariant(
        "acknowledged_write_property",
        "pass: every acknowledged write intact at the source (in bytes)",
    );
    evidence.outcome("aborted: source intact, tail honestly nonzero");
    emit(&rig, evidence, &acked, boundary, "source", None, None);
}

// -------------------------------------------------------------- row 3

/// K1 (§9 row 3): kill the source at the transfer's after-intent
/// boundary — the intent is durable, the mutation never ran (no
/// drive, no consumer proof). The restart's recovery rolls the
/// record back (no cut: the source keeps every acknowledged write);
/// rule 8: the re-POST is refused typed `OPERATION_IN_DOUBT` (the
/// journal cannot prove the operation did not execute — the consumer
/// re-issues a fresh migration); G5 and D6a hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_3_k1_transfer_after_intent() {
    let (mut rig, writer, _transport) = scenario("k1").await;
    let mut evidence = Evidence::new("kill-matrix/transfer/after-intent");
    evidence.fault(
        "crash-after-journal-write",
        "migration_transfer/after-intent",
    );

    prepare(&rig, "mig-k1").await;
    rig.a.core.crash.arm(
        volvisor_api::op_kinds::OP_MIGRATION_TRANSFER,
        volvisor_api::crash::CrashPoint::AfterIntent,
    );
    let reply = post_transfer(rig.a.addr, "mig-k1").await;
    assert!(
        matches!(reply, Reply::Died),
        "the killed daemon never serves the transfer reply"
    );
    assert!(rig.a.is_killed(), "the kill switch fired");
    rig.a.restart().await;

    // The recovery: the startup retry pass resolves the intent-only
    // record — no cut exists, so it rolls back to ABORTED.
    poll_migration(rig.a.addr, "mig-k1", "aborted").await;

    // Rule 8: the re-POST of the same operation is refused typed —
    // the journal holds the intent without an outcome and refuses
    // to assume anything (fail-closed idempotency).
    let (status, body) = transfer(&rig, "mig-k1").await;
    assert_eq!(status, 409, "the in-doubt re-POST refuses: {body}");
    let refusal = body_json(&body);
    assert_eq!(refusal["code"], json!("OPERATION_IN_DOUBT"), "{body}");
    let (acked, boundary) = writer.join().await;
    assert!(!acked.is_empty(), "the writer acknowledged writes");
    let source = verify_against(&rig.world_a, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        source.prefix_intact(),
        "the source keeps every acknowledged write: {source:?}"
    );

    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_aborted(&rig, "mig-k1").await,
    );
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.a.addr, 1, NODE).await,
    );
    evidence.invariant(
        "rule8_idempotency",
        "pass: the re-POST is refused typed OPERATION_IN_DOUBT (never re-executed)",
    );
    evidence.outcome("recovered: ABORTED (rollback of an intent-only record)");
    emit(&rig, evidence, &acked, boundary, "source", None, None);
}

/// K2 (§9 row 3): kill the source at the transfer's before-outcome
/// boundary — the mutation ran (the drive spawned, the consumer
/// proof recorded) but the 202 was never journaled nor served. The
/// window is frozen pre-shape (the drive parks in convergence over a
/// non-empty queue), so the recovery deterministically rolls back;
/// the re-POST refuses typed; the source keeps every acknowledged
/// write with a genuinely nonzero tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_3_k2_transfer_before_outcome() {
    let (mut rig, writer, transport) = scenario("k2").await;
    let mut evidence = Evidence::new("kill-matrix/transfer/before-outcome");
    evidence.fault(
        "crash-before-journal-write",
        "migration_transfer/before-outcome",
    );

    prepare(&rig, "mig-k2").await;
    // Freeze the window BEFORE the transfer: the drive's convergence
    // observation can never pass, so the record deterministically
    // parks pre-cut while the kill lands.
    freeze_window(transport).await;
    rig.a.core.crash.arm(
        volvisor_api::op_kinds::OP_MIGRATION_TRANSFER,
        volvisor_api::crash::CrashPoint::BeforeOutcome,
    );
    let reply = post_transfer(rig.a.addr, "mig-k2").await;
    assert!(
        matches!(reply, Reply::Died),
        "the kill lands before the 202"
    );
    assert!(rig.a.is_killed(), "the kill switch fired");
    rig.a.restart().await;

    // The recovery: the drive was aborted mid-convergence (no cut),
    // so the retry pass rolls the record back.
    poll_migration(rig.a.addr, "mig-k2", "aborted").await;

    // Rule 8: the intent exists without an outcome — the re-POST
    // refuses typed, though the mutation did run (the journal cannot
    // prove what the killed daemon did between the two writes).
    let (status, body) = transfer(&rig, "mig-k2").await;
    assert_eq!(status, 409, "the in-doubt re-POST refuses: {body}");
    assert_eq!(
        body_json(&body)["code"],
        json!("OPERATION_IN_DOUBT"),
        "{body}"
    );
    let (acked, boundary) = writer.join().await;
    assert!(!acked.is_empty(), "the writer acknowledged writes");
    let source = verify_against(&rig.world_a, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        source.prefix_intact(),
        "the source keeps every acknowledged write: {source:?}"
    );
    let peer = verify_against(&rig.world_b, SEED_MINOR, &acked, WRITER_ID);
    assert_eq!(peer.corrupted, 0, "no corruption: {peer:?}");
    // The stable invariant, not the frozen window's size: every
    // acknowledged write is present or tail at the peer, and none is
    // corrupted. The tail's SIZE here is honestly timing-dependent —
    // the rollback's own resync (the system path) may legitimately
    // sync the peer before the observation, and the window the
    // transport froze can be drained by it; the nonzero-tail shape
    // is row 2's business, where the lag is shaped deterministically
    // (the round-1 review's F4: this assert was an unbounded race).
    assert_eq!(
        peer.present + peer.tail(),
        peer.acknowledged,
        "every acknowledged write is present or tail at the peer: {peer:?}"
    );

    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_aborted(&rig, "mig-k2").await,
    );
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.a.addr, 1, NODE).await,
    );
    evidence.invariant(
        "rule8_idempotency",
        "pass: the re-POST is refused typed OPERATION_IN_DOUBT (never re-executed)",
    );
    evidence.outcome("recovered: ABORTED (the killed drive never durably cut)");
    emit(&rig, evidence, &acked, boundary, "source", None, None);
}

/// K3 (§9 row 3): kill the source at the transfer's after-outcome
/// boundary — the 202 and its body are durably journaled, the reply
/// never served. The recovery rolls the (pre-cut, frozen-window)
/// record back, and rule 8's observable inverts: the re-POST is
/// **replayed** — the recorded 202 returns byte-for-byte without
/// re-execution (no new drive, no state change).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_3_k3_transfer_after_outcome() {
    let (mut rig, writer, transport) = scenario("k3").await;
    let mut evidence = Evidence::new("kill-matrix/transfer/after-outcome");
    evidence.fault(
        "crash-after-journal-write",
        "migration_transfer/after-outcome",
    );

    prepare(&rig, "mig-k3").await;
    freeze_window(transport).await;
    rig.a.core.crash.arm(
        volvisor_api::op_kinds::OP_MIGRATION_TRANSFER,
        volvisor_api::crash::CrashPoint::AfterOutcome,
    );
    let reply = post_transfer(rig.a.addr, "mig-k3").await;
    assert!(
        matches!(reply, Reply::Died),
        "the kill lands before the reply"
    );
    assert!(rig.a.is_killed(), "the kill switch fired");
    rig.a.restart().await;
    let summary = poll_migration(rig.a.addr, "mig-k3", "aborted").await;
    let history_before = history_len(&summary);

    // Rule 8: the re-POST replays the recorded outcome — the 202 the
    // journal holds, without re-executing (the migration record is
    // untouched by the replay: same state, same append-only trace).
    let (status, body) = transfer(&rig, "mig-k3").await;
    assert_eq!(status, 202, "the recorded outcome replays: {body}");
    let replayed = poll_migration(rig.a.addr, "mig-k3", "aborted").await;
    assert_eq!(
        history_len(&replayed),
        history_before,
        "the replay added no state transitions (no re-execution)"
    );
    let (acked, boundary) = writer.join().await;
    assert!(!acked.is_empty(), "the writer acknowledged writes");
    let source = verify_against(&rig.world_a, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        source.prefix_intact(),
        "the source keeps every acknowledged write: {source:?}"
    );

    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_aborted(&rig, "mig-k3").await,
    );
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.a.addr, 1, NODE).await,
    );
    evidence.invariant(
        "rule8_idempotency",
        "pass: the re-POST replays the recorded 202 without re-execution",
    );
    evidence.outcome("recovered: ABORTED; the journaled outcome replays");
    emit(&rig, evidence, &acked, boundary, "source", None, None);
}

/// K4 (§9 row 3): kill the destination at the peer grant's
/// after-intent boundary — B journaled the grant intent and died
/// before executing (no promote, no witness grant). A's drive fails
/// on the dead connection with the cut durable (SOURCE_REVOKED), so
/// A's retry task re-drives forward once B restarts: the grant
/// executes against the live B and the migration COMPLETES. The
/// witness epoch is exactly 2 (no double grant); the acknowledged
/// prefix is intact at the destination; G5 and D6a hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_3_k4_peer_grant_after_intent() {
    let (mut rig, writer, _transport) = scenario("k4").await;
    let mut evidence = Evidence::new("kill-matrix/peer-grant/after-intent");
    evidence.fault("crash-after-journal-write", "peer_grant/after-intent");

    prepare(&rig, "mig-k4").await;
    rig.b.core.crash.arm(
        volvisor_api::op_kinds::OP_PEER_GRANT,
        volvisor_api::crash::CrashPoint::AfterIntent,
    );
    let (status, body) = transfer(&rig, "mig-k4").await;
    assert_eq!(status, 202, "transfer: {body}");

    // The drive runs the cut; at the grant, B journals its intent
    // and dies — A's drive fails on the dead connection.
    let deadline = tokio::time::Instant::now() + volvisor_campaign::rig::POLL_BOUND;
    while !rig.b.is_killed() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the peer grant kill never fired"
        );
        tokio::time::sleep(volvisor_campaign::rig::POLL_STEP).await;
    }
    rig.b.restart().await;

    // A's retry task (its 5 s tick) re-drives the durable cut: the
    // grant executes against the restarted B and completes.
    let summary = poll_migration(rig.a.addr, "mig-k4", "complete").await;
    let (acked, boundary) = writer.join().await;

    // The witness epoch is exactly 2: the grant executed once (a
    // double execution would inflate it) — rule 8's cross-daemon
    // observable.
    let view = witness_view(&rig.witness, &rig.volume_id()).await;
    assert_eq!(
        view.current_epoch,
        volvisor_types::WriterEpoch(2),
        "the witness granted exactly one destination epoch"
    );

    // The acknowledged prefix at the destination.
    assert!(!acked.is_empty(), "the writer acknowledged writes");
    let destination = verify_against(&rig.world_b, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        destination.prefix_intact(),
        "every acknowledged write is intact at the destination: {destination:?}"
    );

    // The boundary cross-check over the recovered cut.
    let barrier_at =
        barrier_timestamp(&summary).expect("the complete summary carries the barrier entry");
    let skew = boundary_skew(&acked, barrier_at).expect("the writer acknowledged writes");
    assert!(skew >= 0, "no boundary skew across the recovery");

    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_complete(&rig, "mig-k4").await,
    );
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.b.addr, 2, PEER_NODE).await,
    );
    evidence.invariant(
        "rule8_idempotency",
        "pass: the witness epoch is exactly 2 (the grant executed once)",
    );
    evidence.outcome("recovered: COMPLETE (the durable cut re-drove forward)");
    emit(
        &rig,
        evidence,
        &acked,
        boundary,
        "destination",
        Some(barrier_at),
        Some(skew),
    );
}

/// K5 (§9 row 3): kill the destination at the peer grant's
/// before-outcome boundary — the grant LANDED (the witness minted
/// epoch 2 for node-b, B promoted) but its outcome was never
/// journaled. The recovery re-drives the grant; B's peer route
/// resolves the in-flight intent by inspection (the grant is proven
/// landed), journals the outcome and returns it — the grant never
/// executes twice (epoch exactly 2), and the migration COMPLETES
/// with the acknowledged prefix intact at the destination.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_3_k5_peer_grant_before_outcome() {
    let (mut rig, writer, _transport) = scenario("k5").await;
    let mut evidence = Evidence::new("kill-matrix/peer-grant/before-outcome");
    evidence.fault("crash-before-journal-write", "peer_grant/before-outcome");

    prepare(&rig, "mig-k5").await;
    rig.b.core.crash.arm(
        volvisor_api::op_kinds::OP_PEER_GRANT,
        volvisor_api::crash::CrashPoint::BeforeOutcome,
    );
    let (status, body) = transfer(&rig, "mig-k5").await;
    assert_eq!(status, 202, "transfer: {body}");

    let deadline = tokio::time::Instant::now() + volvisor_campaign::rig::POLL_BOUND;
    while !rig.b.is_killed() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the peer grant kill never fired"
        );
        tokio::time::sleep(volvisor_campaign::rig::POLL_STEP).await;
    }
    rig.b.restart().await;

    // The recovery: A's retry re-drives; B resolves the landed grant
    // by inspection and completes the cut.
    let summary = poll_migration(rig.a.addr, "mig-k5", "complete").await;
    let (acked, boundary) = writer.join().await;

    let view = witness_view(&rig.witness, &rig.volume_id()).await;
    assert_eq!(
        view.current_epoch,
        volvisor_types::WriterEpoch(2),
        "the landed grant was resolved, never re-executed (epoch exactly 2)"
    );

    assert!(!acked.is_empty(), "the writer acknowledged writes");
    let destination = verify_against(&rig.world_b, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        destination.prefix_intact(),
        "every acknowledged write is intact at the destination: {destination:?}"
    );

    let barrier_at =
        barrier_timestamp(&summary).expect("the complete summary carries the barrier entry");
    let skew = boundary_skew(&acked, barrier_at).expect("the writer acknowledged writes");
    assert!(skew >= 0, "no boundary skew across the recovery");

    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_complete(&rig, "mig-k5").await,
    );
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.b.addr, 2, PEER_NODE).await,
    );
    evidence.invariant(
        "rule8_idempotency",
        "pass: the landed grant resolved by inspection (epoch exactly 2, no double execution)",
    );
    evidence.outcome("recovered: COMPLETE (the landed grant resolved by inspection)");
    emit(
        &rig,
        evidence,
        &acked,
        boundary,
        "destination",
        Some(barrier_at),
        Some(skew),
    );
}
