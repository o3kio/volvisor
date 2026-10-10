//! P5 stage C (plan §5, §9 rows 8-14): the adversarial injections.
//!
//! Stage A proved the honest crash matrix (rows 1-3), stage B the
//! mid-commit kills and outages (rows 4-7). Stage C injects what a
//! crash never produces on its own: a rogue writer below volvisor's
//! enforcement (row 8), foreign data wearing the right epoch (row
//! 9), forged authority proofs (row 10), a witness whose durable
//! view is rewound behind the daemons' (row 11), concurrent
//! multi-volume cuts under rotating faults (row 12), a replication
//! partition mid-migration (row 13) and a 25-cycle abort storm on
//! one rig (row 14). Every row asserts ONLY through the observation
//! routes, the witness inspect, the provider inspect and the device
//! bytes (§7); the oracle participates wherever a writer is in play.
//!
//! The honesty rules that shape these rows (§0/§7, verbatim in
//! spirit): a parked record is reported as parked, never as
//! progress; the grant_set wedge that parks rows 8 and 12b is the
//! RECORDED product defect from stage B — it is used as the
//! deterministic injection window and never papered over; and the
//! out-of-band `write_raw` surface models an actor volvisor cannot
//! see, so its divergence is invisible to the classification BY
//! CONSTRUCTION — the fence (the live lease the witness holds) is
//! the protection, and the row proves exactly that.

// Test target (the e2e precedent): invariant assertions may
// expect/unwrap; the rig's helpers are already bounded. The
// `clippy::panic` allow mirrors the lib's documented discipline
// (its panics ARE the assertions): every row's asserts abort with
// formatted scenario context — a scenario whose invariant broke is
// a failed test, never a production failure path.
#![allow(clippy::expect_used, clippy::unwrap_used)]
#![allow(clippy::panic)] // row-dispatch assertions (see above)

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;
use volvisor_api::crash::CrashPoint;
use volvisor_api::op_kinds;
use volvisor_campaign::evidence::{Evidence, LogSources, oracle_value};
use volvisor_campaign::oracle::{
    AckedWrite, ClockBudget, StopReason, WRITER_ID, WriterHandle, tag_block, verify_against,
};
use volvisor_campaign::rig::{
    POLL_BOUND, POLL_STEP, Reply, Rig, admin, body_json, campaign_rig, campaign_rig_volumes,
    get_migration, get_volume, poll_migration, post_abort, post_prepare, post_transfer, role_of,
    state_name, witness_view,
};
use volvisor_drbd::report::Role;
use volvisor_drbd_testkit::{
    BLOCK_SIZE, NODE, PEER_NODE, SEED_MINOR, inject_foreign_blocks, read_raw, spawn_peer_transport,
    write_raw,
};
use volvisor_journal::{JOURNAL_LOG_FILE, Journal};
use volvisor_provider::{VmState, VmmController};
use volvisor_types::crash::StoreSavePoint;
use volvisor_types::{
    BarrierAttestation, HostId, LeaseState, MigrationId, OperationId, VolumeId, WriterEpoch,
};
use volvisor_witness::client::WitnessConnection;
use volvisor_witness::proto::{
    RecordBarrierRequest, RenewRequest, VoidBarrierRequest, WITNESS_PROTOCOL_VERSION, WitnessError,
};

/// The steady-state transport's lag (§2.1, the stage-B value).
const TRANSPORT_LAG: Duration = Duration::from_millis(1);

/// The steady-state warmup (§2.2's "continuous" writer).
const WRITER_WARMUP: Duration = Duration::from_millis(80);

/// The settle time that makes a partition deterministic: the link
/// drops, the live writer queues writes beyond the last drain, and
/// the drive's convergence observation can never pass over a
/// non-empty queue (stage A's frozen-window guarantee, in-place
/// partition form).
const PARTITION_SETTLE: Duration = Duration::from_millis(200);

/// The settle time for a drive fault that fails fast (a refused
/// pause, a refused barrier connection): the record is durably
/// parked long before this expires.
const FAULT_SETTLE: Duration = Duration::from_millis(100);

/// The abort storm's cycles (§9 row 14: 25 cycles on ONE rig).
const STORM_CYCLES: usize = 25;

/// The abort storm's budget (§9 row 14: at most ten seconds).
const STORM_BOUND: Duration = Duration::from_secs(10);

/// The per-row unique id source (rows run concurrently; every rig is
/// isolated, the ids keep the evidence and logs distinguishable).
static CELL_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_id() -> u64 {
    CELL_SEQ.fetch_add(1, Ordering::SeqCst)
}

/// Emit a row's evidence record with the rig's log sources.
fn emit_rig(rig: &Rig, evidence: Evidence) -> PathBuf {
    evidence.finish(&LogSources {
        a_journal: &rig.a.core.journal_dir,
        b_journal: &rig.b.core.journal_dir,
        witness: &rig.witness.dir,
    })
}

/// Emit a single-volume row's evidence record: the oracle section
/// from the byte-level verdicts (the acknowledged prefix verified at
/// `verified_side`, the honest tail and corruption counts).
fn emit_oracle(
    rig: &Rig,
    mut evidence: Evidence,
    acked: &[AckedWrite],
    boundary: Option<(u64, StopReason)>,
    verified_side: &str,
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
        None,
        None,
    ));
    emit_rig(rig, evidence)
}

/// Emit a multi-volume row's evidence record: one oracle value per
/// participant (each writer's acknowledged prefix verified at
/// `verified_side`).
fn emit_oracle_n(
    rig: &Rig,
    mut evidence: Evidence,
    acked: &[Vec<AckedWrite>],
    boundary: Option<(u64, StopReason)>,
    verified_side: &str,
) -> PathBuf {
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
    let mut values = Vec::with_capacity(acked.len());
    for (index, acked_i) in acked.iter().enumerate() {
        let minor = minor_of(index);
        let verdict_source = verify_against(&rig.world_a, minor, acked_i, WRITER_ID);
        let verdict_peer = verify_against(&rig.world_b, minor, acked_i, WRITER_ID);
        let (verified, tail) = if verified_side == "destination" {
            (verdict_peer.present, verdict_source.tail())
        } else {
            (verdict_source.present, verdict_peer.tail())
        };
        values.push(oracle_value(
            acked_i.len() as u64,
            verified,
            verdict_peer.corrupted.max(verdict_source.corrupted),
            tail,
            verified_side,
            boundary_seq,
            "data-path",
            &stop_reason,
            None,
            None,
        ));
    }
    evidence.oracle(json!(values));
    emit_rig(rig, evidence)
}

/// Bounded wait for a fired kill (the drive is asynchronous; the
/// armed op fires when the drive reaches it).
async fn await_kill(killed: impl Fn() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + POLL_BOUND;
    while !killed() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the {what} kill never fired"
        );
        tokio::time::sleep(POLL_STEP).await;
    }
}

/// The rule-8 shape for a typed refusal: 409 with the exact code.
fn assert_refusal(status: u16, body: &str, code: &str, context: &str) {
    assert_eq!(status, 409, "{context}: {body}");
    assert_eq!(body_json(body)["code"], json!(code), "{context}: {body}");
}

/// The full single-volume scenario opening (a live writer and a
/// steady-state transport — the cut-crossing rows' shape).
async fn live_scenario(prefix: &str) -> (Rig, WriterHandle, volvisor_drbd_testkit::PeerTransport) {
    let rig = campaign_rig(&format!("vm-{prefix}"), &format!("vol-{prefix}")).await;
    let writer = WriterHandle::start(&rig.world_a, &rig.vmm_a, &rig.vm, SEED_MINOR, &rig.clock);
    let transport = spawn_peer_transport(&rig.world_a, SEED_MINOR, TRANSPORT_LAG);
    tokio::time::sleep(WRITER_WARMUP).await;
    (rig, writer, transport)
}

/// The multi-volume scenario opening (§9 row 12): one rig, `count`
/// participants each with its own writer and transport link (the
/// kit's links are per-minor; one wired VMM covers every device).
/// The writers SHARE one clock-advance budget (the cap is per
/// scenario — N writers each burning it would expire the lease as a
/// rig artifact; see `CLOCK_ADVANCE_CAP`).
async fn live_scenario_n(
    prefix: &str,
    count: usize,
) -> (
    Rig,
    Vec<WriterHandle>,
    Vec<volvisor_drbd_testkit::PeerTransport>,
) {
    let volumes: Vec<String> = (0..count).map(|i| format!("vol-{prefix}-{i}")).collect();
    let refs: Vec<&str> = volumes.iter().map(String::as_str).collect();
    let rig = campaign_rig_volumes(&format!("vm-{prefix}"), &refs).await;
    let budget: ClockBudget = Arc::new(AtomicU64::new(0));
    let writers = (0..count)
        .map(|i| {
            WriterHandle::start_shared(
                &rig.world_a,
                &rig.vmm_a,
                &rig.vm,
                minor_of(i),
                &rig.clock,
                &budget,
            )
        })
        .collect();
    let transports = (0..count)
        .map(|i| spawn_peer_transport(&rig.world_a, minor_of(i), TRANSPORT_LAG))
        .collect();
    tokio::time::sleep(WRITER_WARMUP).await;
    (rig, writers, transports)
}

/// The prepare act (the stage-A/B shape; 201) — works for single-
/// and multi-volume rigs alike (`Rig::volumes` is the participant
/// set; every volume was attached once, so every expected
/// generation is 2).
async fn prepare(rig: &Rig, mig: &str) {
    let (status, body) = post_prepare(
        rig.a.addr,
        &json!({
            "migration_id": mig,
            "vm_id": rig.vm,
            "target_host": PEER_NODE,
            "volume_ids": rig.volumes,
            "expected_generations": (0..rig.volumes.len()).map(|_| 2).collect::<Vec<_>>(),
        }),
    )
    .await
    .served("prepare");
    assert_eq!(status, 201, "prepare: {body}");
}

/// The transfer act (the 202 the drive spawns under).
async fn transfer(rig: &Rig, mig: &str) -> (u16, String) {
    post_transfer(rig.a.addr, mig).await.served("transfer")
}

/// Flip the replication link in place (§5's partition: the world's
/// `peer_online` flag stops every transport drain and closes the
/// convergence gate — the queue stays open, the foreground keeps
/// acknowledging at the source).
fn set_partition(rig: &Rig, online: bool) {
    rig.world_a.lock().expect("source world").peer_online = online;
}

/// The `VolumeId` of participant `index`.
fn vol_id(rig: &Rig, index: usize) -> VolumeId {
    VolumeId::new(rig.volumes[index].as_str()).expect("valid volume id")
}

/// The participant's device minor (the rig assigns minors in attach
/// order from [`SEED_MINOR`]; the bounded cast is the rig's own
/// `try_into` idiom).
fn minor_of(index: usize) -> u32 {
    let index: u32 = index.try_into().expect("volume index fits u32");
    SEED_MINOR + index
}

/// The W1-W5 check (the witness side) for one volume: the authority
/// view's epoch, holder and lease state are exactly as the recovery
/// implies — no authority without a live lease, no unexpected epoch
/// (a double grant would show as epoch 3).
async fn assert_w1_w5_vol(
    rig: &Rig,
    vol: &VolumeId,
    epoch: u64,
    holder: &str,
    live: bool,
) -> String {
    let view = witness_view(&rig.witness, vol).await;
    assert_eq!(
        view.current_epoch.0, epoch,
        "the witness epoch is exactly {epoch}: {:?}",
        view.lease_state
    );
    assert_eq!(
        view.holder.as_ref().map(HostId::as_str),
        Some(holder),
        "the epoch-{epoch} holder is {holder}"
    );
    let expected = if live {
        LeaseState::Live
    } else {
        LeaseState::Revoked
    };
    assert_eq!(
        view.lease_state, expected,
        "the epoch-{epoch} lease state is {expected:?}"
    );
    format!("pass: epoch {epoch} at {holder}, lease {expected:?} (W1-W5)")
}

/// The D6a check for one volume: the provider's public inspect
/// surface answers with the witness-backed authority summary.
async fn assert_d6a_vol(
    addr: std::net::SocketAddr,
    volume: &str,
    epoch: u64,
    holder: &str,
) -> String {
    let (status, body) = get_volume(addr, volume).await.served("provider inspect");
    assert_eq!(status, 200, "provider inspect: {body}");
    let value = body_json(&body);
    assert_eq!(
        value["authority"]["epoch"],
        json!(epoch),
        "authority epoch: {body}"
    );
    assert_eq!(
        value["authority"]["holder"],
        json!(holder),
        "authority holder: {body}"
    );
    format!("pass: authority epoch {epoch} at {holder}")
}

/// The G5 check for COMPLETE paths over every participant (rule 5):
/// the migration's barrier is recorded at the witness for each
/// volume, NOT voided, with every attested fact true — and the
/// source was never resumed over it.
async fn assert_g5_complete_n(rig: &Rig, mig: &str) -> String {
    let migration = MigrationId::new(mig).expect("valid migration id");
    for index in 0..rig.volumes.len() {
        let view = witness_view(&rig.witness, &vol_id(rig, index)).await;
        let matching: Vec<_> = view
            .barriers
            .iter()
            .filter(|barrier| barrier.migration_id.as_ref() == Some(&migration))
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "volume {index}: exactly one barrier: {:?}",
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
            role_of(&rig.world_a, &rig.resources()[index]),
            Role::Secondary,
            "volume {index}: the source resource is demoted"
        );
    }
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Absent,
        "the source VM is destroyed (never resumed)"
    );
    "pass: every participant's barrier non-voided and all-true; source never resumed".to_owned()
}

/// The G5 check for ABORTED paths over every participant (rule 5):
/// no unvoided barrier for the migration exists, and the source
/// legitimately resumed.
async fn assert_g5_aborted_n(rig: &Rig, mig: &str) -> String {
    let migration = MigrationId::new(mig).expect("valid migration id");
    for index in 0..rig.volumes.len() {
        let view = witness_view(&rig.witness, &vol_id(rig, index)).await;
        let unvoided = view
            .barriers
            .iter()
            .filter(|barrier| barrier.migration_id.as_ref() == Some(&migration) && !barrier.voided)
            .count();
        assert_eq!(
            unvoided, 0,
            "volume {index}: an aborted migration leaves no unvoided barrier to resume over"
        );
        assert_eq!(
            role_of(&rig.world_a, &rig.resources()[index]),
            Role::Primary,
            "volume {index}: the source resource is promoted"
        );
    }
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Running,
        "the source VM is resumed"
    );
    "pass: no unvoided barrier; the resume is legal".to_owned()
}

// ------------------------------------------------------------- row 8

/// Row 8 (§5.1, §9): the stale source write after the fence.
///
/// The cut revokes the source's authority (epoch 2 granted to the
/// destination) but parks mid-commit in the recorded grant_set wedge
/// — the deterministic window where the source is FENCED yet its
/// device is still writable by an actor below volvisor. A rogue
/// writer then writes the source's map out-of-band (§2.4's
/// `write_raw`): one block at a never-written index, one over an
/// acknowledged index. The honest outcomes: the destination never
/// receives either byte (out-of-band is below the replication path
/// too — no queue entry, no resync from a fenced source, and the cut
/// marker keeps the reconciler from resuming the source); the
/// destination's acknowledged prefix stays byte-exact; the survivor
/// daemon's adopt-and-promote refuses `unsafe` (the live epoch-2
/// lease at the destination — the fence is the protection, the
/// divergence is invisible to the classification BY CONSTRUCTION and
/// recorded as such); and the abort refuses typed (cut-or-later,
/// G1/D1a).
// One row is one coherent scenario (§9): the narrative reads top-to-bottom
// through the fault, the recovery and the safety set — splitting it would
// scatter the evidence.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_8_stale_source_write_after_fence() {
    let seq = next_id();
    let (mut rig, writer, _transport) = live_scenario(&format!("r8-{seq}")).await;
    let mut evidence = Evidence::new("row-8/stale-source-write-after-fence");
    evidence.fault(
        "stale_source_write_after_fence",
        "the grant_set wedge (stage B's recorded park defect) + out-of-band write_raw on the \
         fenced source's map, post-revoke",
    );

    prepare(&rig, "mig-r8").await;
    // The wedge: the witness dies inside the destination's promote
    // batch. B's peer-grant op journals its failure, the record parks
    // at destination_authorized — the recorded product defect, used
    // here as the deterministic fenced-but-writable window.
    rig.witness
        .crash
        .arm_witness("grant_set", StoreSavePoint::WitnessAfterApplyBeforeOutcome);
    let (status, body) = transfer(&rig, "mig-r8").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");
    await_kill(|| rig.witness.is_killed(), "witness grant_set commit").await;
    rig.witness.restart().await;
    rig.a.restart().await;
    poll_migration(rig.a.addr, "mig-r8", "destination_authorized").await;

    // The writer stopped at the cut (the VM is Absent); the
    // acknowledged prefix is fixed.
    let (acked, boundary) = writer.join().await;
    assert!(!acked.is_empty(), "the writer acknowledged writes");

    // The rogue writes, out-of-band (§2.4): below the role check,
    // below the suspension check, and below the replication path.
    let never_written: u64 = 240_000;
    let rogue_fresh = [0xA5_u8; BLOCK_SIZE];
    let last_acked = acked.last().expect("at least one acknowledged write");
    let overwrite = last_acked.index;
    let overwritten_seq = last_acked.seq;
    let rogue_overwrite = [0xD9_u8; BLOCK_SIZE];
    write_raw(&rig.world_a, SEED_MINOR, never_written, &rogue_fresh)
        .expect("the rogue write at a never-written index");
    write_raw(&rig.world_a, SEED_MINOR, overwrite, &rogue_overwrite)
        .expect("the rogue overwrite of an acknowledged index");

    // The positive control: the source's map holds the rogue bytes.
    assert_eq!(
        read_raw(&rig.world_a, SEED_MINOR, never_written)
            .expect("read the source")
            .payload,
        rogue_fresh,
        "the rogue bytes are in the source's map"
    );
    assert_eq!(
        read_raw(&rig.world_a, SEED_MINOR, overwrite)
            .expect("read the source")
            .payload,
        rogue_overwrite,
        "the rogue overwrite is in the source's map"
    );

    // The destination is untouched: the never-written index reads
    // zero fill, the acknowledged index still holds the writer's
    // exact payload (the wedge's drain delivered it pre-rogue-write;
    // nothing resyncs from a fenced source).
    assert_eq!(
        read_raw(&rig.world_b, SEED_MINOR, never_written)
            .expect("read the destination")
            .payload,
        [0_u8; BLOCK_SIZE],
        "the destination never received the rogue write"
    );
    let tag_expected = tag_block(overwritten_seq, WRITER_ID);
    assert_eq!(
        read_raw(&rig.world_b, SEED_MINOR, overwrite)
            .expect("read the destination")
            .payload,
        tag_expected,
        "the destination's acknowledged byte is the writer's, not the rogue's"
    );

    // The survivor daemon (P4a's unplanned-promotion route: a fresh
    // state file over the source's fixtures) asks to adopt. The
    // classification is UNSAFE: the witness holds a live epoch-2
    // lease for the destination — the fence, not the divergence, is
    // what refuses (the out-of-band write is invisible to it by
    // construction, and this row records that honestly).
    let survivor = rig.launch_source_survivor("row8").await;
    let (status, body) = admin(
        "POST",
        survivor.addr,
        &format!("/v2/admin/nearline/{}/adopt", rig.volume),
        Some(
            &json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": "op-adopt-r8",
                "allow_loss": false,
            })
            .to_string(),
        ),
    )
    .await
    .served("adopt");
    assert_eq!(
        status, 200,
        "the adopt answers with its classification: {body}"
    );
    let value = body_json(&body);
    assert!(
        value["classification"]["unsafe"].is_object(),
        "the classification is unsafe: {body}"
    );
    let reasons = value["classification"]["unsafe"]["reasons"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        reasons
            .iter()
            .any(|reason| reason.as_str().is_some_and(|r| r.contains("live lease"))),
        "the refusal reason is the live lease: {body}"
    );
    assert!(value["volume"].is_null(), "no volume was adopted: {body}");

    // The abort refuses typed (cut-or-later — G1/D1a).
    let (status, body) = post_abort(rig.a.addr, "mig-r8").await.served("abort");
    assert_refusal(status, &body, "INVALID_STATE", "the post-cut abort");

    // The safety set: the source is fenced, the destination never
    // promoted, the epoch-2 lease is live at the witness.
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Absent,
        "the source VM is destroyed (the cut)"
    );
    assert_eq!(
        role_of(&rig.world_a, &rig.resource()),
        Role::Secondary,
        "the source resource is demoted"
    );
    assert_eq!(
        role_of(&rig.world_b, &rig.resource()),
        Role::Secondary,
        "the destination never promoted over the wedge"
    );
    evidence.invariant(
        "destination_untouched",
        "pass: neither rogue byte reached the destination (no queue entry, no resync from a \
         fenced source); the acknowledged prefix is byte-exact",
    );
    evidence.invariant(
        "unsafe_classification",
        "pass: the survivor's adopt refuses unsafe over the live epoch-2 lease — the fence is \
         the protection; the out-of-band divergence is invisible to the classification by \
         construction (recorded, not papered over)",
    );
    evidence.invariant(
        "abort_refused",
        "pass: the abort at destination_authorized refuses INVALID_STATE (cut-or-later, G1/D1a)",
    );
    let w = assert_w1_w5_vol(&rig, &rig.volume_id(), 2, PEER_NODE, true).await;
    evidence.invariant("w1_w5", &w);
    evidence.outcome(
        "pass: parked safe: the record is in doubt at destination_authorized (the recorded grant_set \
         wedge defect), the destination holds the acknowledged prefix and never promoted, the \
         rogue writes are confined to the fenced source's map, and no route resumes the source \
         without fenced reconciliation",
    );
    let record = emit_oracle(&rig, evidence, &acked, boundary, "destination");
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

// ------------------------------------------------------------- row 9

/// Row 9 (§5.2, §9): wrong-lineage data at the target.
///
/// `inject_foreign_blocks` replaces the target replica's
/// resource-level lineage (the botched-create-md shape: right epoch,
/// wrong lineage) and writes foreign blocks under it, BEFORE the
/// prepare. THE FINDING (fixed in the stage-C base commit):
/// `verify_target_replica` had no lineage check at any granularity —
/// the prepare would have driven a cut over a replica whose data
/// volvisor never provisioned. The gate now compares the replica's
/// live lineage against the source-supplied expected set and refuses
/// typed (`FOREIGN_DEVICE_STATE`); this row proves the refusal is
/// total (same-id re-drive and fresh-id re-issue both refuse),
/// `ObserveHandoff` never reports COMPLETE over foreign data, and
/// the source is untouched.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_9_wrong_lineage_data_at_target() {
    let seq = next_id();
    let (rig, writer, _transport) = live_scenario(&format!("r9-{seq}")).await;
    let mut evidence = Evidence::new("row-9/wrong-lineage-data-at-target");
    evidence.fault(
        "wrong_lineage_target_data",
        "inject_foreign_blocks at the target replica pre-prepare: the resource-level lineage \
         replaced (right epoch, wrong lineage) with foreign blocks under it",
    );

    // The injection: the replica's lineage becomes a generation no
    // world-minted salt can reach, with foreign bytes at indexes the
    // writer has already acknowledged (the divergence shape — not
    // merely absent data, but present-but-foreign data).
    let foreign_blocks: &[u64] = &[0, 1, 2, 3];
    let foreign_generation = u64::MAX;
    inject_foreign_blocks(&rig.world_b, SEED_MINOR, foreign_blocks, foreign_generation)
        .expect("the foreign-lineage injection");

    // The prepare refuses typed at the replica-level lineage gate.
    let (status, body) = post_prepare(
        rig.a.addr,
        &json!({
            "migration_id": "mig-r9",
            "vm_id": rig.vm,
            "target_host": PEER_NODE,
            "volume_ids": rig.volumes,
            "expected_generations": (0..rig.volumes.len()).map(|_| 2).collect::<Vec<_>>(),
        }),
    )
    .await
    .served("prepare over a foreign-lineage target");
    assert_refusal(status, &body, "FOREIGN_DEVICE_STATE", "the lineage gate");

    // The same migration id re-drives to the same typed refusal (the
    // recorded failure replays — rule 8, never a silent re-execution).
    let (status, body) = post_prepare(
        rig.a.addr,
        &json!({
            "migration_id": "mig-r9",
            "vm_id": rig.vm,
            "target_host": PEER_NODE,
            "volume_ids": rig.volumes,
            "expected_generations": (0..rig.volumes.len()).map(|_| 2).collect::<Vec<_>>(),
        }),
    )
    .await
    .served("the same-id re-drive");
    assert_refusal(
        status,
        &body,
        "FOREIGN_DEVICE_STATE",
        "the idempotent refusal",
    );

    // A fresh migration id also refuses: the foreign state persists
    // (the gate is not a one-shot — recovery requires the operator
    // to re-seed the target replica, out of this row's scope).
    let (status, body) = post_prepare(
        rig.a.addr,
        &json!({
            "migration_id": "mig-r9-fresh",
            "vm_id": rig.vm,
            "target_host": PEER_NODE,
            "volume_ids": rig.volumes,
            "expected_generations": (0..rig.volumes.len()).map(|_| 2).collect::<Vec<_>>(),
        }),
    )
    .await
    .served("the fresh-id re-issue");
    assert_refusal(
        status,
        &body,
        "FOREIGN_DEVICE_STATE",
        "the persistent foreign state",
    );

    // ObserveHandoff never reports COMPLETE over foreign data.
    let (status, body) = get_migration(rig.a.addr, "mig-r9")
        .await
        .served("the refused migration's observation");
    let observed = if status == 200 {
        let state = state_name(&body_json(&body));
        assert_ne!(
            state, "complete",
            "never COMPLETE over foreign data: {body}"
        );
        format!("the record answers {state} (non-terminal, never complete)")
    } else {
        assert_eq!(status, 404, "no record lingers: {body}");
        "no record lingers (the refused prepare left nothing observable)".to_owned()
    };

    // The source is untouched: the VM runs, the resource is Primary,
    // the epoch-1 lease is live, no barrier was ever recorded.
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Running,
        "the source VM never paused"
    );
    assert_eq!(
        role_of(&rig.world_a, &rig.resource()),
        Role::Primary,
        "the source resource was never demoted"
    );
    let view = witness_view(&rig.witness, &rig.volume_id()).await;
    assert!(
        view.barriers.is_empty(),
        "no cut ever ran over the foreign target: {:?}",
        view.barriers
    );

    // The byte-level positive control: the destination really holds
    // the foreign bytes (block idx LE + generation LE + 0xC3 fill —
    // never the writer's tag shape).
    let mut expect_foreign = [0_u8; BLOCK_SIZE];
    expect_foreign[0..8].copy_from_slice(&0_u64.to_le_bytes());
    expect_foreign[8..16].copy_from_slice(&foreign_generation.to_le_bytes());
    expect_foreign[16..].fill(0xC3);
    assert_eq!(
        read_raw(&rig.world_b, SEED_MINOR, 0)
            .expect("read the destination")
            .payload,
        expect_foreign,
        "the foreign data is present and detectable at the byte level"
    );

    let (acked, boundary) = writer.join().await;
    let verdict = verify_against(&rig.world_a, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        verdict.prefix_intact(),
        "the source holds every acknowledged write: {verdict:?}"
    );

    evidence.invariant(
        "lineage_gate",
        "pass: the replica-level gate refuses FOREIGN_DEVICE_STATE (the stage-C FINDING, fixed \
         in the base commit — the gate did not exist before)",
    );
    evidence.invariant(
        "refusal_total",
        "pass: the same-id re-drive and the fresh-id re-issue both refuse typed — the foreign \
         state persists until the operator re-seeds the target",
    );
    evidence.invariant(
        "never_complete_over_foreign",
        &format!("pass: ObserveHandoff answers non-terminal, never complete ({observed})"),
    );
    evidence.invariant(
        "source_untouched",
        "pass: the VM runs, the resource is Primary, the epoch-1 lease is live, no barrier exists",
    );
    evidence.outcome(
        "refused typed: the wrong-lineage target never entered a migration — the cut that would \
         have crossed foreign data never started",
    );
    let record = emit_oracle(&rig, evidence, &acked, boundary, "source");
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

// ------------------------------------------------------------- row 10

/// Row 10 (§5.3, §9): the forged barrier proofs.
///
/// Three forgeries against the witness's record-barrier route: a
/// wrong-epoch claim (typed `StaleEpoch` — W4), a wrong-credential
/// claim (typed holder refusal — W8), and a foreign-migration-id
/// barrier at the right epoch from the entitled holder — which the
/// witness records VERBATIM (holder-entitled facts are facts). The
/// partition this row pins is TWO-LEVEL, discovered exactly here:
/// the void enumeration is migration-keyed (a real migration's
/// rollback voids exactly its own barrier — the foreign one is
/// never touched), but the source's resume gate is EPOCH-wide (the
/// G5 enforcement at the unsuspend point: never lift the suspension
/// while ANY unvoided barrier of the writer epoch exists). So the
/// real migration's abort parks `OPERATION_IN_DOUBT` over the
/// foreign barrier — never a silent resume over evidence the holder
/// recorded — and completes only after the recording holder voids
/// the foreign barrier (the one legitimate cleanup path).
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_10_forged_barrier_proofs() {
    let seq = next_id();
    let (mut rig, writer, _transport) = live_scenario(&format!("r10-{seq}")).await;
    let mut evidence = Evidence::new("row-10/forged-barrier-proofs");
    evidence.fault(
        "forged_barrier_proof",
        "the witness record-barrier route: a wrong-epoch claim, a wrong-credential claim, and a \
         foreign-migration-id barrier from the entitled holder",
    );

    let forge = |migration: &str, host: &str, epoch: u64| RecordBarrierRequest {
        protocol_version: WITNESS_PROTOCOL_VERSION,
        operation_id: OperationId::new(format!("op-forged-{migration}")).expect("valid op id"),
        host_id: HostId::new(host).expect("valid host id"),
        epoch: WriterEpoch(epoch),
        attestation: BarrierAttestation {
            vm_paused_and_drained: true,
            data_path_suspended: true,
            peer_up_to_date: true,
        },
        migration_id: Some(MigrationId::new(migration).expect("valid migration id")),
    };

    // Forgery A: the wrong epoch (2 while the live lease is epoch 1)
    // — the typed W4 refusal.
    let err = rig
        .witness
        .host_client(NODE)
        .record_barrier(&rig.volume_id(), forge("mig-forged-wrong-epoch", NODE, 2))
        .await
        .expect_err("the wrong-epoch forged barrier refuses");
    assert!(
        matches!(err, WitnessError::StaleEpoch { .. }),
        "the wrong-epoch forgery refuses typed W4: {err:?}"
    );

    // Forgery B: the wrong credential (node-b's identity over
    // node-a's token) — the typed holder refusal.
    let err = rig
        .witness
        .host_client(NODE)
        .record_barrier(
            &rig.volume_id(),
            forge("mig-forged-wrong-holder", PEER_NODE, 1),
        )
        .await
        .expect_err("the wrong-credential forged barrier refuses");
    assert!(
        matches!(err, WitnessError::IdentityRequired),
        "the wrong-credential forgery refuses typed W8: {err:?}"
    );

    // Forgery C: the foreign migration id at the right epoch from
    // the entitled holder — recorded verbatim, holder-entitled.
    let foreign_mig = MigrationId::new("mig-foreign-row10").expect("valid migration id");
    rig.witness
        .host_client(NODE)
        .record_barrier(&rig.volume_id(), forge("mig-foreign-row10", NODE, 1))
        .await
        .expect("the holder-entitled foreign barrier is recorded verbatim");

    // The real migration parks inside its own barrier (the witness
    // dies after the intent append — the restart's replay re-derives
    // the real barrier), and the source's restart attempts the
    // pre-cut rollback.
    prepare(&rig, "mig-r10").await;
    rig.witness
        .crash
        .arm_witness("record_barrier", StoreSavePoint::WitnessAfterIntentAppend);
    let (status, body) = transfer(&rig, "mig-r10").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");
    await_kill(|| rig.witness.is_killed(), "witness barrier commit").await;
    rig.witness.restart().await;
    rig.a.restart().await;

    // THE FINDING (pinned here): the rollback's void is
    // migration-keyed — the real barrier is voided — but the resume
    // gate is EPOCH-wide. The foreign unvoided barrier of epoch 1
    // refuses the abort typed: the source stays suspended, never a
    // silent resume over evidence the holder recorded.
    let (status, body) = post_abort(rig.a.addr, "mig-r10")
        .await
        .served("the gated abort");
    assert_refusal(
        status,
        &body,
        "OPERATION_IN_DOUBT",
        "the abort over the foreign unvoided barrier",
    );
    assert!(
        body.contains("never a silent resume"),
        "the refusal is the epoch-wide G5 gate: {body}"
    );

    // The void enumeration is migration-keyed: the real migration's
    // barrier is voided by its own rollback; the foreign barrier
    // survives non-voided (never another migration's void target).
    let real_mig = MigrationId::new("mig-r10").expect("valid migration id");
    let view = witness_view(&rig.witness, &rig.volume_id()).await;
    assert_eq!(
        view.barriers.len(),
        2,
        "exactly the two barriers: {:?}",
        view.barriers
    );
    let real: Vec<_> = view
        .barriers
        .iter()
        .filter(|barrier| barrier.migration_id.as_ref() == Some(&real_mig))
        .collect();
    assert_eq!(real.len(), 1, "the real migration's barrier exists");
    assert!(real[0].voided, "the real barrier is voided by its rollback");
    let foreign: Vec<_> = view
        .barriers
        .iter()
        .filter(|barrier| barrier.migration_id.as_ref() == Some(&foreign_mig))
        .collect();
    assert_eq!(foreign.len(), 1, "the foreign barrier exists");
    assert!(
        !foreign[0].voided,
        "a foreign migration's barrier is never voided by another migration's abort"
    );

    // The recording holder voids the foreign barrier — the one
    // legitimate cleanup path — and the retry pass completes the
    // abort over the now-clean epoch.
    rig.witness
        .host_client(NODE)
        .void_barrier(
            &rig.volume_id(),
            VoidBarrierRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: OperationId::new("op-void-foreign-r10").expect("valid op id"),
                host_id: HostId::new(NODE).expect("valid host id"),
                epoch: WriterEpoch(1),
                migration_id: Some(foreign_mig.clone()),
            },
        )
        .await
        .expect("the recording holder voids the foreign barrier");
    poll_migration(rig.a.addr, "mig-r10", "aborted").await;

    // The G5-aborted shape (the real barrier voided, the source
    // legitimately resumed) and the W1-W5 set.
    let g5 = assert_g5_aborted_n(&rig, "mig-r10").await;
    let w = assert_w1_w5_vol(&rig, &rig.volume_id(), 1, NODE, true).await;

    let (acked, boundary) = writer.join().await;
    let verdict = verify_against(&rig.world_a, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        verdict.prefix_intact(),
        "the source holds every acknowledged write: {verdict:?}"
    );

    evidence.invariant(
        "wrong_epoch_refused",
        "pass: the wrong-epoch forged barrier refuses typed StaleEpoch (W4)",
    );
    evidence.invariant(
        "wrong_credential_refused",
        "pass: the wrong-credential forged barrier refuses typed IdentityRequired (W8)",
    );
    evidence.invariant(
        "foreign_barrier_inert_for_void",
        "pass: the holder-entitled foreign barrier is recorded verbatim and is never another \
         migration's void target — the void enumeration is migration-keyed",
    );
    evidence.invariant(
        "epoch_wide_resume_gate",
        "FINDING (correct fail-closed behavior, pinned): the source's resume gate is EPOCH-wide, \
         not migration-scoped — the real migration's abort parked OPERATION_IN_DOUBT over the \
         foreign unvoided barrier of epoch 1 (never a silent resume over evidence the holder \
         recorded), and completed only after the recording holder voided it",
    );
    evidence.invariant("g5_aborted", &g5);
    evidence.invariant("w1_w5", &w);
    evidence.outcome(
        "refused, gated, then clean: neither forgery became evidence — the wrong-epoch and \
         wrong-credential claims refuse typed; the foreign-migration barrier is recorded verbatim \
         and never voided by another migration's abort; and the epoch-wide resume gate parked the \
         real migration's abort until the recording holder voided the foreign barrier",
    );
    let record = emit_oracle(&rig, evidence, &acked, boundary, "source");
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

// ------------------------------------------------------------- row 11

/// Row 11 (§5.4, §9): the witness divergence — the journal rollback.
///
/// After a COMPLETED handoff (the destination holds epoch 2 live),
/// the witness's journal is rewound out-of-band
/// (`Journal::rollback_last_mutation` drops the grant_set: the last
/// mutation — the renewal throttle guarantees no renew journaled
/// within the interval). The restarted witness derives the STALE
/// view (epoch 1, no live lease) while the destination's durable
/// state holds the newer one. The honest outcomes: the direct renew
/// over the stale view refuses typed (fail closed — nothing
/// proceeds on a stale authority view); the destination's restart
/// re-loads its durable epoch-2 authority and self-fences AT
/// CONSTRUCTION — the startup reconcile's writer-authority
/// validation (P4a plan §4) treats every Primary as unproven until
/// validated, and the stale view's refusal fails the epoch-2 block
/// (the volume Failed, the authority cleared, the data path
/// suspended — the W4 proof; the renewal path would refuse the same
/// way, the construction reconcile simply gets there first), with
/// the DEMOTION pending behind the in-use device (the restored VM
/// holds it open; the kernel's own refusal — AGENTS rule 17 — the
/// renewal loop's fence-completion pass owns it). The re-mint
/// hazard (a fresh grant could mint a second epoch 2 for the source
/// over the stale view) is recorded, not driven.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_11_witness_journal_rollback() {
    let seq = next_id();
    let (mut rig, writer, _transport) = live_scenario(&format!("r11-{seq}")).await;
    let mut evidence = Evidence::new("row-11/witness-divergence-journal-rollback");
    evidence.fault(
        "witness_journal_rollback",
        "journal.rollback_last_mutation drops the grant_set after the handoff completed — the \
         witness's durable view rewound behind the destination's",
    );

    // The completed handoff: the destination holds the full prefix
    // and the epoch-2 lease.
    prepare(&rig, "mig-r11").await;
    let (status, body) = transfer(&rig, "mig-r11").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");
    poll_migration(rig.a.addr, "mig-r11", "complete").await;
    let (acked, boundary) = writer.join().await;
    let verdict = verify_against(&rig.world_b, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        verdict.prefix_intact(),
        "the completed handoff's prefix is intact at the destination: {verdict:?}"
    );

    // The pre-rollback view: the newer authority (epoch 2 at node-b,
    // live, with its lease identity).
    let before = witness_view(&rig.witness, &rig.volume_id()).await;
    assert_eq!(
        before.current_epoch.0, 2,
        "the completed handoff granted epoch 2"
    );
    assert_eq!(
        before.holder.as_ref().map(HostId::as_str),
        Some(PEER_NODE),
        "the destination holds epoch 2"
    );
    assert_eq!(before.lease_state, LeaseState::Live);
    let lease = before.lease_id.expect("the live lease identity");

    // Stop the destination inside the renewal throttle window (no
    // renew journaled — the last witness mutation IS the grant_set),
    // then rewind the witness's journal behind the daemons' view.
    rig.b.stop().await;
    rig.witness.stop().await;
    let dropped =
        Journal::rollback_last_mutation(&rig.witness.dir).expect("the witness journal rollback");
    assert_eq!(
        dropped.as_deref(),
        Some("witness_grant_set"),
        "the dropped mutation is exactly the grant_set: {dropped:?}"
    );
    rig.witness.restart().await;

    // The stale view: the grant_set never happened.
    let stale = witness_view(&rig.witness, &rig.volume_id()).await;
    assert_eq!(
        stale.current_epoch.0, 1,
        "the rewound witness never saw epoch 2: {:?}",
        stale.lease_state
    );
    assert_ne!(
        stale.lease_state,
        LeaseState::Live,
        "no live lease exists on the stale view"
    );

    // The direct renew (the destination's credential, its durable
    // epoch-2 lease identity) refuses typed — fail closed.
    let err = rig
        .witness
        .host_client(PEER_NODE)
        .renew(
            &rig.volume_id(),
            RenewRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: OperationId::new("op-renew-r11").expect("valid op id"),
                host_id: HostId::new(PEER_NODE).expect("valid host id"),
                epoch: WriterEpoch(2),
                lease_id: lease,
            },
        )
        .await
        .expect_err("the renew over the stale view refuses");
    assert!(
        matches!(err, WitnessError::StaleEpoch { .. }),
        "the renew over the stale authority view refuses typed: {err:?}"
    );

    // The destination restarts holding the newer durable view. Its
    // STARTUP RECONCILE fences it immediately — not the renewal
    // loop: the provider's construction runs the reconcile, whose
    // writer-authority validation (P4a plan §4) treats every Primary
    // found on the host as unproven until validated, suspends I/O
    // first (fail-closed ordering: a stalled witness must leave the
    // writer frozen, never serving), validates the epoch-2 block
    // against the witness, and self-fences on the stale view's
    // refusal. The direct renew above already proved the witness's
    // typed refusal — the renewal path would fence the same way; the
    // construction reconcile simply gets there first.
    rig.b.restart().await;

    // The fence is durable in the provider's state: the volume is
    // Failed with its writer authority cleared (bounded wait — the
    // fence fires inside the restart's construction).
    let deadline = Instant::now() + POLL_BOUND;
    loop {
        let (status, body) = get_volume(rig.b.addr, &rig.volume)
            .await
            .served("the fenced provider inspect");
        assert_eq!(status, 200, "the fenced volume is observable: {body}");
        let value = body_json(&body);
        if value["state"] == json!("Failed") && value["authority"].is_null() {
            assert_eq!(
                value["current_writer"],
                json!(null),
                "the fenced writer holds no device either: {body}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the destination never self-fenced over the stale authority view: {body}"
        );
        tokio::time::sleep(POLL_STEP).await;
    }

    // The DEMOTION is pending, honestly: the restored destination VM
    // holds the resource's device open, and the kernel's own refusal
    // (the in-use demote discipline — AGENTS rule 17) leaves the
    // role at Primary while the suspension and the durable fence
    // marker are already in place. The renewal loop's
    // fence-completion pass owns the demote once the device releases
    // (the rig does not expose the destination's VMM — recorded, not
    // driven).
    assert_eq!(
        role_of(&rig.world_b, &rig.resource()),
        Role::Primary,
        "the demote is pending behind the in-use device (the fence's suspension is the durable \
         act)"
    );
    // The witness is unchanged by the fence (the refused renewal
    // wrote nothing — the fence is local and durable), and the source
    // stays fenced (no resurrection).
    let after = witness_view(&rig.witness, &rig.volume_id()).await;
    assert_eq!(
        after.current_epoch.0, 1,
        "the witness view is unchanged by the fence"
    );
    assert_ne!(after.lease_state, LeaseState::Live);
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Absent,
        "the source stays destroyed (no resurrection)"
    );
    assert_eq!(
        role_of(&rig.world_a, &rig.resource()),
        Role::Secondary,
        "the source stays demoted"
    );

    // The completed record stays terminal and immutable through the
    // divergence.
    let (status, body) = get_migration(rig.a.addr, "mig-r11")
        .await
        .served("the completed record");
    assert_eq!(status, 200, "the record answers: {body}");
    assert_eq!(
        state_name(&body_json(&body)),
        "complete",
        "the terminal record is immutable: {body}"
    );

    evidence.invariant(
        "stale_view_fail_closed",
        "pass: the direct renew over the stale view refuses typed StaleEpoch — nothing proceeds \
         on a stale authority view",
    );
    evidence.invariant(
        "daemon_self_fenced",
        "pass: the destination's restart re-loaded its durable epoch-2 authority and self-fenced \
         AT CONSTRUCTION — the startup reconcile's writer-authority validation (P4a plan §4) \
         refused the epoch-2 block over the stale view; the volume is Failed with its writer \
         authority cleared; the demotion is pending behind the in-use device (the kernel's own \
         refusal, AGENTS rule 17) with the suspension and fence marker already durable",
    );
    evidence.invariant(
        "no_new_witness_mutations",
        "pass: the witness view is unchanged by the fence (the fence is local and durable)",
    );
    evidence.invariant(
        "source_stays_fenced",
        "pass: the source stays destroyed and demoted — the divergence resurrected nothing",
    );
    evidence.outcome(
        "fail closed everywhere: the rewound witness refuses the destination's renewal and the \
         destination self-fences over the refusal. RECORDED HAZARD (not driven): a fresh grant \
         over the stale view could mint a second epoch 2 for the source — the daemons' \
         fail-closed checks, not the witness's journal, are what keep the divergence safe",
    );
    let record = emit_oracle(&rig, evidence, &acked, boundary, "destination");
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

// ------------------------------------------------------------- row 12

/// Row 12a (§9 row 12): the multi-volume cut converges. Three
/// participants, one VM, one wired VMM — the healthy baseline that
/// the rotating-fault cells park against: every participant's
/// barrier is recorded and never voided, the set-wide grant lands
/// (epoch 2 at the destination for every volume), the VM is
/// destroyed once, and every writer's prefix is byte-exact at the
/// destination.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_12a_multi_volume_cut_converges() {
    let seq = next_id();
    let (rig, writers, _transports) = live_scenario_n(&format!("m12a-{seq}"), 3).await;
    let mut evidence = Evidence::new("row-12/multi-volume-cut/converges");
    evidence.fault(
        "none",
        "the healthy multi-volume baseline (the rotating-fault cells' control)",
    );

    prepare(&rig, "mig-r12a").await;
    let (status, body) = transfer(&rig, "mig-r12a").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");
    poll_migration(rig.a.addr, "mig-r12a", "complete").await;

    let g5 = assert_g5_complete_n(&rig, "mig-r12a").await;
    let mut w = Vec::new();
    for index in 0..rig.volumes.len() {
        w.push(assert_w1_w5_vol(&rig, &vol_id(&rig, index), 2, PEER_NODE, true).await);
    }
    let mut d = Vec::new();
    for volume in &rig.volumes {
        d.push(assert_d6a_vol(rig.b.addr, volume, 2, PEER_NODE).await);
    }

    let mut joined = Vec::new();
    for writer in writers {
        joined.push(writer.join().await);
    }
    let acked: Vec<Vec<AckedWrite>> = joined.iter().map(|(a, _)| a.clone()).collect();
    for (index, acked_i) in acked.iter().enumerate() {
        let verdict = verify_against(&rig.world_b, minor_of(index), acked_i, WRITER_ID);
        assert!(
            verdict.prefix_intact(),
            "volume {index}: every acknowledged write is at the destination: {verdict:?}"
        );
    }

    evidence.invariant("g5_complete", &g5);
    evidence.invariant("w1_w5", &format!("pass: {}", w.join("; ")));
    evidence.invariant("d6a", &format!("pass: {}", d.join("; ")));
    evidence.outcome(
        "pass: complete: the concurrent multi-volume cut converged — every participant promoted under \
         the set-wide grant, the source destroyed once, every prefix byte-exact",
    );
    let boundary = joined[0].1.clone();
    let record = emit_oracle_n(&rig, evidence, &acked, boundary, "destination");
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

/// Row 12b (§9 row 12): one participant fails its promote — the cut
/// parks IN_DOUBT with the EXACT participant state. The set-wide
/// grant mints epoch 2 at the destination for every volume, then the
/// per-participant promotes run in order: the first succeeds
/// (promoted, attached, its lease held live), the second refuses
/// (its `drbdadm primary` fails — and its promote path fail-closed
/// RELEASES its own lease: an authority whose device never promoted
/// is surrendered), the third is never reached (its minted lease
/// stays live but unused — inert, the source is revoked and nobody
/// can write; it expires at the TTL). The drive parks IN_DOUBT (the
/// detail: "source revoked; destination grant not yet authorized")
/// — past the cut (the source's authority is revoked set-wide and
/// the grant is minted) but the promote batch failed — in the
/// recorded grant_set-wedge mechanism (B's peer-grant op journaled
/// its failure; the ops pipeline replays recorded failures forever)
/// — the recorded product defect, never weakened.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_12b_multi_volume_one_fails_promote_parks_exactly() {
    let seq = next_id();
    let (rig, writers, _transports) = live_scenario_n(&format!("m12b-{seq}"), 3).await;
    let mut evidence = Evidence::new("row-12/multi-volume-cut/one-fails-promote");
    evidence.fault(
        "promote_refusal",
        "fail_primary_resources on the second participant's resource at the destination (the \
         cut reaches the promote step set-wide)",
    );

    // The second participant's promote will refuse.
    let failed_resource = rig.resources()[1].clone();
    rig.world_b
        .lock()
        .expect("destination world")
        .fail_primary_resources
        .insert(failed_resource);

    prepare(&rig, "mig-r12b").await;
    let (status, body) = transfer(&rig, "mig-r12b").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");
    // The record parks IN_DOUBT — the cut landed (the source's
    // authority revoked set-wide, the destination's grant minted at
    // the witness) but B's grant route failed at the second
    // participant's promote: the drive's fail-closed observation is
    // "source revoked; destination grant not yet authorized".
    poll_migration(rig.a.addr, "mig-r12b", "in_doubt").await;

    // The IN_DOUBT observation is mapped from SourceRevoked the
    // moment the REVOKE lands — which can precede the witness
    // grant_set minting the epochs. The park's full shape includes
    // the landed grant, so bounded-wait for it (it lands within
    // milliseconds of the revoke; the promote failure follows it).
    let deadline = Instant::now() + POLL_BOUND;
    loop {
        let mut epochs = Vec::with_capacity(rig.volumes.len());
        for index in 0..rig.volumes.len() {
            epochs.push(witness_view(&rig.witness, &vol_id(&rig, index)).await);
        }
        if epochs.iter().all(|view| view.current_epoch.0 == 2) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the set-wide grant never landed: {:?}",
            epochs
                .iter()
                .map(|view| (view.current_epoch.0, view.lease_state))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(POLL_STEP).await;
    }

    // The EXACT participant state. Witness side: the set-wide grant
    // minted epoch 2 at the destination for every volume — and the
    // promote outcomes diverge per participant: the promoted one
    // holds its lease live, the FAILED promote released its own
    // (fail-closed — an authority whose device never promoted is
    // surrendered), and the never-reached one keeps a minted but
    // unused lease (inert: the source is revoked, nobody can write,
    // and it expires at the TTL).
    let w0 = assert_w1_w5_vol(&rig, &vol_id(&rig, 0), 2, PEER_NODE, true).await;
    let w1 = assert_w1_w5_vol(&rig, &vol_id(&rig, 1), 2, PEER_NODE, false).await;
    let w2 = assert_w1_w5_vol(&rig, &vol_id(&rig, 2), 2, PEER_NODE, true).await;
    let w = format!("{w0}; {w1}; {w2}");

    // Destination side: the first participant promoted and attached
    // (its authority is inspectable), the second stayed Secondary,
    // the third was never reached.
    assert_eq!(
        role_of(&rig.world_b, &rig.resources()[0]),
        Role::Primary,
        "the first participant promoted"
    );
    assert_eq!(
        role_of(&rig.world_b, &rig.resources()[1]),
        Role::Secondary,
        "the failed participant never promoted"
    );
    assert_eq!(
        role_of(&rig.world_b, &rig.resources()[2]),
        Role::Secondary,
        "the never-reached participant never promoted"
    );
    let d = assert_d6a_vol(rig.b.addr, &rig.volumes[0], 2, PEER_NODE).await;
    // The failed promote's participant: its destination entry EXISTS
    // as the fail-closed record — state Failed, no writer, no
    // attachment (the refused promote left its honest trail, not an
    // absent volume).
    let (status, body) = get_volume(rig.b.addr, &rig.volumes[1])
        .await
        .served("the failed participant's inspect");
    assert_eq!(status, 200, "the failed participant is observable: {body}");
    let failed = body_json(&body);
    assert_eq!(
        failed["state"],
        json!("Failed"),
        "the failed promote's participant is Failed at the destination: {body}"
    );
    assert_eq!(
        failed["current_writer"],
        json!(null),
        "the failed promote's participant holds no writer: {body}"
    );
    // The never-reached participant: no destination state at all.
    let (status, body) = get_volume(rig.b.addr, &rig.volumes[2])
        .await
        .served("the never-reached participant's inspect");
    assert_eq!(
        status, 404,
        "the never-reached participant has no destination state: {body}"
    );

    // Source side: the cut is set-wide — every resource demoted, the
    // VM destroyed.
    for index in 0..rig.volumes.len() {
        assert_eq!(
            role_of(&rig.world_a, &rig.resources()[index]),
            Role::Secondary,
            "volume {index}: the source resource is demoted (the cut is set-wide)"
        );
    }
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Absent,
        "the source VM is destroyed"
    );

    // The data is nevertheless ALL there: the drain completed for
    // every participant before the cut (the convergence gate is
    // all-participants) — what failed is the promotion, not the
    // replication.
    let mut joined = Vec::new();
    for writer in writers {
        joined.push(writer.join().await);
    }
    let acked: Vec<Vec<AckedWrite>> = joined.iter().map(|(a, _)| a.clone()).collect();
    for (index, acked_i) in acked.iter().enumerate() {
        let verdict = verify_against(&rig.world_b, minor_of(index), acked_i, WRITER_ID);
        assert!(
            verdict.prefix_intact(),
            "volume {index}: the acknowledged prefix is at the destination even though its \
             promote failed: {verdict:?}"
        );
    }

    // The abort refuses typed (cut-or-later).
    let (status, body) = post_abort(rig.a.addr, "mig-r12b").await.served("abort");
    assert_refusal(status, &body, "INVALID_STATE", "the post-cut abort");

    evidence.invariant(
        "exact_participant_state",
        "pass: promoted+attached (lease live) / Secondary with a FAILED destination entry and \
         its lease released fail-closed / never-reached with NO destination entry (404) and a \
         minted lease live but unused (inert, expires at the TTL); every volume's authority is \
         epoch 2 at the destination; the source is fully fenced",
    );
    evidence.invariant(
        "data_delivered_promotion_failed",
        "pass: every participant's acknowledged prefix is byte-exact at the destination — the \
         failure is the promotion, not the replication",
    );
    evidence.invariant("w1_w5", &w);
    evidence.invariant("d6a_first", &d);
    evidence.outcome(
        "parked safe (IN_DOUBT, detail \"source revoked; destination grant not yet authorized\" \
         — past the cut, the promote batch failed): one failed promote parks the whole set with \
         no dual writer and no data loss. THE RECORDED DEFECT (stage B's grant_set wedge \
         mechanism): B's peer-grant op journaled its failure and the ops pipeline replays \
         recorded failures forever — recovery requires operator action (the retry task re-serves \
         the failure at its 5s tick); never weakened to make this row pass",
    );
    let boundary = joined[0].1.clone();
    let record = emit_oracle_n(&rig, evidence, &acked, boundary, "destination");
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

/// Row 12c (§9 row 12): the source is killed mid-drive — the
/// multi-volume transfer dies at its op journal (the kill lands
/// before the 202), the restart's startup pass rolls the pre-cut
/// record back (the auto-before-cut policy), and the consumer's
/// re-issue converges. The exact state at the park: every
/// participant Secondary, the VM Running, the epoch-1 lease live at
/// the source, every prefix intact at the source.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_12c_multi_volume_source_killed_mid_drive_recovers() {
    let seq = next_id();
    let (mut rig, writers, _transports) = live_scenario_n(&format!("m12c-{seq}"), 3).await;
    let mut evidence = Evidence::new("row-12/multi-volume-cut/source-killed-mid-drive");
    evidence.fault(
        "source_kill",
        "the source dies at the transfer op's outcome journal write (the drive is in flight, the \
         202 never served)",
    );

    prepare(&rig, "mig-r12c").await;
    rig.a
        .core
        .crash
        .arm(op_kinds::OP_MIGRATION_TRANSFER, CrashPoint::BeforeOutcome);
    let reply = post_transfer(rig.a.addr, "mig-r12c").await;
    assert!(
        matches!(reply, Reply::Died),
        "the kill lands before the 202"
    );
    assert!(rig.a.is_killed(), "the kill switch fired");
    rig.a.restart().await;

    // The startup pass resolves the in-flight record: pre-cut, no
    // cut — the auto-before-cut rollback.
    poll_migration(rig.a.addr, "mig-r12c", "aborted").await;

    // The exact state: nothing crossed.
    for index in 0..rig.volumes.len() {
        assert_eq!(
            role_of(&rig.world_a, &rig.resources()[index]),
            Role::Primary,
            "volume {index}: the source resource stayed Primary"
        );
        let w = assert_w1_w5_vol(&rig, &vol_id(&rig, index), 1, NODE, true).await;
        if index == 0 {
            evidence.invariant("w1_w5", &w);
        }
    }
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Running,
        "the source VM never paused for long (the rollback resumed it)"
    );

    // The consumer re-issues: a fresh migration id converges.
    prepare(&rig, "mig-r12c-reissue").await;
    let (status, body) = transfer(&rig, "mig-r12c-reissue").await;
    assert_eq!(
        status, 202,
        "the re-issued transfer spawns the drive: {body}"
    );
    poll_migration(rig.a.addr, "mig-r12c-reissue", "complete").await;

    let g5 = assert_g5_complete_n(&rig, "mig-r12c-reissue").await;
    let mut joined = Vec::new();
    for writer in writers {
        joined.push(writer.join().await);
    }
    let acked: Vec<Vec<AckedWrite>> = joined.iter().map(|(a, _)| a.clone()).collect();
    for (index, acked_i) in acked.iter().enumerate() {
        let verdict = verify_against(&rig.world_b, minor_of(index), acked_i, WRITER_ID);
        assert!(
            verdict.prefix_intact(),
            "volume {index}: the re-issued cut delivered every acknowledged write: {verdict:?}"
        );
    }

    evidence.invariant(
        "safe_abort_exact_state",
        "pass: the killed-mid-drive record aborted with every participant Secondary, the VM \
         Running and the epoch-1 lease live — nothing crossed",
    );
    evidence.invariant("g5_complete_reissue", &g5);
    evidence.outcome(
        "pass: recovered by re-issue: the source's death mid-drive parked nothing dangerous (the \
         startup pass rolled the pre-cut record back) and the consumer's fresh migration \
         converged with every prefix byte-exact",
    );
    let boundary = joined[0].1.clone();
    let record = emit_oracle_n(&rig, evidence, &acked, boundary, "destination");
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

/// Row 12d (§9 row 12): the witness restarts mid-drive — the
/// partition parks the drive at its convergence retry (the drive
/// task holds the drive lock; the record honestly stays PREPARED),
/// the witness dies and restarts over its journal (the epoch-1
/// lease survives via the W3 replay), the link heals, and the SAME
/// drive converges to COMPLETE. The convergence gate never claimed
/// caught-up while the link was down.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_12d_multi_volume_witness_restart_mid_drive_recovers() {
    let seq = next_id();
    let (mut rig, writers, _transports) = live_scenario_n(&format!("m12d-{seq}"), 3).await;
    let mut evidence = Evidence::new("row-12/multi-volume-cut/witness-restart-mid-drive");
    evidence.fault(
        "witness_restart",
        "the witness dies and restarts while the drive is parked at its convergence retry (the \
         partition holds the link down)",
    );

    prepare(&rig, "mig-r12d").await;
    set_partition(&rig, false);
    tokio::time::sleep(PARTITION_SETTLE).await;
    let (status, body) = transfer(&rig, "mig-r12d").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");

    // Mid-partition: the record is honestly non-terminal and never
    // claims caught-up; the destination honestly lags.
    let (status, body) = get_migration(rig.a.addr, "mig-r12d")
        .await
        .served("the mid-partition observation");
    assert_eq!(status, 200, "the observation answers: {body}");
    assert_eq!(
        state_name(&body_json(&body)),
        "prepared",
        "the convergence gate never claimed caught-up over the down link: {body}"
    );

    // The witness restart mid-drive (over its journal — the lease
    // survives), then the link heals and the SAME drive converges.
    rig.witness.stop().await;
    rig.witness.restart().await;
    set_partition(&rig, true);
    poll_migration(rig.a.addr, "mig-r12d", "complete").await;

    let g5 = assert_g5_complete_n(&rig, "mig-r12d").await;
    let mut w = Vec::new();
    for index in 0..rig.volumes.len() {
        w.push(assert_w1_w5_vol(&rig, &vol_id(&rig, index), 2, PEER_NODE, true).await);
    }
    let mut joined = Vec::new();
    for writer in writers {
        joined.push(writer.join().await);
    }
    let acked: Vec<Vec<AckedWrite>> = joined.iter().map(|(a, _)| a.clone()).collect();
    for (index, acked_i) in acked.iter().enumerate() {
        let verdict = verify_against(&rig.world_b, minor_of(index), acked_i, WRITER_ID);
        assert!(
            verdict.prefix_intact(),
            "volume {index}: the healed cut delivered every acknowledged write: {verdict:?}"
        );
    }

    evidence.invariant(
        "honest_park",
        "pass: the record stayed PREPARED over the partition (never a caught-up claim over a \
         down link)",
    );
    evidence.invariant(
        "witness_replay_held",
        "pass: the restarted witness held the epoch-1 lease via its journal replay — the cut's \
         barrier and grant proceeded over it",
    );
    evidence.invariant("g5_complete", &g5);
    evidence.invariant("w1_w5", &format!("pass: {}", w.join("; ")));
    evidence.outcome(
        "pass: converged: the witness restart mid-drive lost nothing — the same drive task converged \
         once the link healed, and the completion claim is byte-backed for every participant",
    );
    let boundary = joined[0].1.clone();
    let record = emit_oracle_n(&rig, evidence, &acked, boundary, "destination");
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

/// Row 12e (§9 row 12): the resync under foreground writes. The
/// partition opens the dirty-bitmap window (the queue grows while
/// the foreground keeps acknowledging at the source — rule 16); the
/// convergence gate refuses throughout (the destination honestly
/// holds less than the acknowledged prefix, and the record never
/// claims caught-up); the heal drains the window through the ONE
/// drainer (the transport — the drive never calls
/// `apply_peer_writes`, the single-drainer invariant); and the
/// completion claim is byte-backed: the full prefix, including the
/// partition-period tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_12e_resync_under_foreground_never_claims_caught_up() {
    let seq = next_id();
    let (rig, writer, _transport) = live_scenario(&format!("r12e-{seq}")).await;
    let mut evidence = Evidence::new("row-12/resync-under-foreground");
    evidence.fault(
        "replication_partition",
        "peer_online=false mid-migration: the dirty-bitmap window opens under a live foreground \
         writer, then heals",
    );

    prepare(&rig, "mig-r12e").await;
    set_partition(&rig, false);
    tokio::time::sleep(PARTITION_SETTLE).await;
    let (status, body) = transfer(&rig, "mig-r12e").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");

    // The foreground keeps acknowledging at the source (rule 16).
    let acked_first = writer.acknowledged().len();
    tokio::time::sleep(PARTITION_SETTLE).await;
    let acked_second = writer.acknowledged().len();
    assert!(
        acked_second > acked_first,
        "the foreground kept acknowledging at the source through the partition (rule 16)"
    );

    // The convergence gate never lies over the in-flight resync: the
    // record stays PREPARED and the destination honestly holds less
    // than the acknowledged prefix (verified in bytes, corrupted
    // nowhere).
    let (status, body) = get_migration(rig.a.addr, "mig-r12e")
        .await
        .served("the mid-resync observation");
    assert_eq!(status, 200, "the observation answers: {body}");
    assert_eq!(
        state_name(&body_json(&body)),
        "prepared",
        "the gate never claimed caught-up over the in-flight resync: {body}"
    );
    let mid_acked = writer.acknowledged();
    let mid_verdict = verify_against(&rig.world_b, SEED_MINOR, &mid_acked, WRITER_ID);
    assert!(
        mid_verdict.present < mid_acked.len() as u64,
        "the destination honestly holds less than the acknowledged prefix mid-resync: \
         {mid_verdict:?}"
    );
    assert_eq!(mid_verdict.corrupted, 0, "nothing is corrupted mid-resync");

    // The heal: the transport (the single drainer) delivers the
    // dirty window, the gate passes over the drained state, and the
    // completion claim covers exactly the partition-period writes.
    set_partition(&rig, true);
    poll_migration(rig.a.addr, "mig-r12e", "complete").await;

    let (acked, boundary) = writer.join().await;
    let verdict = verify_against(&rig.world_b, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        verdict.prefix_intact(),
        "the completion claim is byte-backed — the full prefix including the partition-period \
         tail: {verdict:?}"
    );
    assert!(
        acked.len() > mid_acked.len(),
        "writes continued through the partition (the tail claim covers exactly them)"
    );
    let g5 = assert_g5_complete_n(&rig, "mig-r12e").await;
    let w = assert_w1_w5_vol(&rig, &rig.volume_id(), 2, PEER_NODE, true).await;

    evidence.invariant(
        "foreground_survives_partition",
        "pass: the source kept acknowledging foreground writes through the partition (rule 16)",
    );
    evidence.invariant(
        "gate_never_lies",
        "pass: mid-resync the record stayed PREPARED and the destination's verified prefix was \
         strictly behind the acknowledged one (observed in bytes, corrupted nowhere)",
    );
    evidence.invariant(
        "single_drainer",
        "pass: the heal drained through the transport alone — the drive never calls \
         apply_peer_writes (the single-drainer invariant held)",
    );
    evidence.invariant("g5_complete", &g5);
    evidence.invariant("w1_w5", &w);
    evidence.outcome(
        "pass: complete and byte-backed: the in-flight resync under foreground writes never produced a \
         caught-up claim, and the completion covered every acknowledged write",
    );
    let record = emit_oracle(&rig, evidence, &acked, boundary, "destination");
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

/// Row 12f (§9 row 12): the source VMM dies mid-cut. The drive parks
/// inside its barrier (the witness dies at the intent append — the
/// record is at QUIESCED: the VM paused, the data path suspended,
/// pre-revoke), and the VMM destroys the paused VM out-of-band (the
/// operator's host loss in the cut's window). The abort rolls the
/// pre-cut record back: the absent VM needs no resume (the rollback
/// skips it — never a wrong resume), the source stays Primary, the
/// barrier is voided. The attachment to the destroyed VM is residue
/// the reconcile path owns — recorded, not driven.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_12f_source_vmm_death_mid_cut_rolls_back_safely() {
    let seq = next_id();
    let (mut rig, writer, _transport) = live_scenario(&format!("r12f-{seq}")).await;
    let mut evidence = Evidence::new("row-12/source-vmm-death-mid-cut");
    evidence.fault(
        "source_vmm_death",
        "the VMM destroys the paused source VM while the drive is parked inside its barrier \
         (the record at QUIESCED, pre-revoke)",
    );

    prepare(&rig, "mig-r12f").await;
    rig.witness
        .crash
        .arm_witness("record_barrier", StoreSavePoint::WitnessAfterIntentAppend);
    let (status, body) = transfer(&rig, "mig-r12f").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");
    await_kill(|| rig.witness.is_killed(), "witness barrier commit").await;

    // The VMM dies mid-cut: the paused VM is destroyed out-of-band.
    rig.vmm_a
        .destroy(&rig.vm)
        .expect("the source VMM dies mid-cut");
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Absent,
        "the source VM is gone (the injection)"
    );

    // The abort rolls the pre-cut record back over the restarted
    // witness (the void must confirm — the witness comes back first).
    rig.witness.restart().await;
    let (status, body) = post_abort(rig.a.addr, "mig-r12f").await.served("abort");
    assert_eq!(
        status, 200,
        "the abort rolls the pre-cut record back: {body}"
    );
    poll_migration(rig.a.addr, "mig-r12f", "aborted").await;

    // The safety set: the barrier is voided (G5's no-unvoided rule),
    // the source never demoted (still Primary — the revoke never
    // landed), and the VM stays Absent (honestly — it died; the
    // rollback skips the resume for an absent VM rather than
    // fabricating one).
    let migration = MigrationId::new("mig-r12f").expect("valid migration id");
    let view = witness_view(&rig.witness, &rig.volume_id()).await;
    let unvoided = view
        .barriers
        .iter()
        .filter(|barrier| barrier.migration_id.as_ref() == Some(&migration) && !barrier.voided)
        .count();
    assert_eq!(unvoided, 0, "the mid-cut barrier is voided by the rollback");
    assert_eq!(
        role_of(&rig.world_a, &rig.resource()),
        Role::Primary,
        "the source resource stayed Primary (the revoke never landed)"
    );
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Absent,
        "the VM stays absent — the rollback never fabricated a resume"
    );
    let w = assert_w1_w5_vol(&rig, &rig.volume_id(), 1, NODE, true).await;

    let (acked, boundary) = writer.join().await;
    let verdict = verify_against(&rig.world_a, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        verdict.prefix_intact(),
        "the source's device holds every acknowledged write: {verdict:?}"
    );

    evidence.invariant(
        "absent_vm_never_resumed",
        "pass: the rollback skipped the resume for the absent VM (never a wrong resume) and the \
         record still closed ABORTED",
    );
    evidence.invariant(
        "barrier_voided",
        "pass: the mid-cut barrier is voided — no unvoided barrier to resume over (G5)",
    );
    evidence.invariant("w1_w5", &w);
    evidence.outcome(
        "pass: aborted safely: the source's VMM death inside the cut window left the storage state \
         exactly pre-cut (Primary, suspended-then-unsuspended, every byte intact). RECORDED \
         RESIDUE: the volume's attachment to the destroyed VM is the reconcile path's cleanup — \
         out of this row's scope",
    );
    let record = emit_oracle(&rig, evidence, &acked, boundary, "source");
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

/// Row 12g (§9 row 12): the kill during the in-flight resync (the
/// dirty-bitmap logical window). The partition opens the window (a
/// queue of undelivered writes); the destination's control plane
/// dies inside it (an idle-daemon group abort — the destination has
/// no acts pre-cut) and restarts while the link is still down; the
/// heal then drains every queued write through the data plane
/// (world-level, control-plane-independent) and the cut converges
/// with the full prefix — nothing is lost in the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_12g_kill_during_divergence_window_loses_nothing() {
    let seq = next_id();
    let (mut rig, writer, _transport) = live_scenario(&format!("r12g-{seq}")).await;
    let mut evidence = Evidence::new("row-12/kill-during-divergence-window");
    evidence.fault(
        "destination_control_plane_death",
        "the destination daemon's task group is aborted inside the dirty-bitmap window (the \
         partition's queue of undelivered writes is non-empty)",
    );

    prepare(&rig, "mig-r12g").await;
    set_partition(&rig, false);
    tokio::time::sleep(PARTITION_SETTLE).await;
    let (status, body) = transfer(&rig, "mig-r12g").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");

    // The window is open (the queue grows); the destination's
    // control plane dies inside it and comes back while the link is
    // still down.
    let queued_before = writer.acknowledged().len();
    rig.b.stop().await;
    rig.b.restart().await;

    // The heal: the data plane drains the window regardless of the
    // control-plane death, and the drive converges.
    set_partition(&rig, true);
    poll_migration(rig.a.addr, "mig-r12g", "complete").await;

    let (acked, boundary) = writer.join().await;
    assert!(
        acked.len() > queued_before,
        "writes were outstanding in the window (the kill covered a real divergence)"
    );
    let verdict = verify_against(&rig.world_b, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        verdict.prefix_intact(),
        "nothing was lost in the dirty-bitmap window: {verdict:?}"
    );
    let g5 = assert_g5_complete_n(&rig, "mig-r12g").await;
    let w = assert_w1_w5_vol(&rig, &rig.volume_id(), 2, PEER_NODE, true).await;

    evidence.invariant(
        "data_plane_independent",
        "pass: the destination's control-plane death inside the window lost nothing — the drain \
         is world-level and the restart re-observed the drained state",
    );
    evidence.invariant("g5_complete", &g5);
    evidence.invariant("w1_w5", &w);
    evidence.outcome(
        "pass: complete: the kill during the in-flight resync (the dirty-bitmap window) is survivable \
         by construction — the queued writes are the bitmap, and the heal delivered every one",
    );
    let record = emit_oracle(&rig, evidence, &acked, boundary, "destination");
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

// ------------------------------------------------------------- row 13

/// Row 13 (§5.5, §9): the replication partition mid-migration. The
/// link drops after the prepare: the cut refuses and parks honestly
/// (the record stays PREPARED — never COMPLETE, never a caught-up
/// claim) while the foreground keeps acknowledging at the source
/// (rule 16) and the destination honestly lags (verified in bytes).
/// No cut artifact exists while partitioned (no barrier, the VM
/// Running, the source Primary). The heal converges the SAME drive
/// and the oracle's tail claim covers exactly the partition-period
/// writes. The single-drainer invariant holds throughout (the
/// transport is the only drainer).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_13_replication_partition_mid_migration() {
    let seq = next_id();
    let (rig, writer, _transport) = live_scenario(&format!("r13-{seq}")).await;
    let mut evidence = Evidence::new("row-13/replication-partition-mid-migration");
    evidence.fault(
        "replication_partition",
        "peer_online=false mid-migration (after the prepare, over the transfer) — the link \
         heals after the honest park",
    );

    prepare(&rig, "mig-r13").await;
    set_partition(&rig, false);
    tokio::time::sleep(PARTITION_SETTLE).await;
    let (status, body) = transfer(&rig, "mig-r13").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");

    // The park: the record is honestly non-terminal.
    let (status, body) = get_migration(rig.a.addr, "mig-r13")
        .await
        .served("the mid-partition observation");
    assert_eq!(status, 200, "the observation answers: {body}");
    assert_eq!(
        state_name(&body_json(&body)),
        "prepared",
        "the cut refuses over the partition (the record parks honestly): {body}"
    );

    // The foreground keeps acknowledging at the source (rule 16) —
    // two samples prove the liveness, not a coincidence.
    let acked_first = writer.acknowledged().len();
    tokio::time::sleep(PARTITION_SETTLE).await;
    let acked_second = writer.acknowledged().len();
    assert!(
        acked_second > acked_first,
        "the foreground kept acknowledging at the source through the partition (rule 16)"
    );

    // The destination honestly lags (the tail is real, in bytes) and
    // no cut artifact exists while partitioned.
    let mid_acked = writer.acknowledged();
    let mid_verdict = verify_against(&rig.world_b, SEED_MINOR, &mid_acked, WRITER_ID);
    assert!(
        mid_verdict.present < mid_acked.len() as u64,
        "the destination honestly lags mid-partition: {mid_verdict:?}"
    );
    assert_eq!(mid_verdict.corrupted, 0);
    let view = witness_view(&rig.witness, &rig.volume_id()).await;
    assert!(
        view.barriers.is_empty(),
        "no barrier was recorded over the partition: {:?}",
        view.barriers
    );
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Running,
        "the source VM was never paused for the partition"
    );
    assert_eq!(
        role_of(&rig.world_a, &rig.resource()),
        Role::Primary,
        "the source stayed Primary (no revoke over a partition)"
    );
    let w_mid = assert_w1_w5_vol(&rig, &rig.volume_id(), 1, NODE, true).await;

    // The heal: the same drive converges, and the completion's tail
    // claim covers exactly the partition-period writes.
    set_partition(&rig, true);
    poll_migration(rig.a.addr, "mig-r13", "complete").await;

    let (acked, boundary) = writer.join().await;
    let verdict = verify_against(&rig.world_b, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        verdict.prefix_intact(),
        "the healed cut delivered the full prefix — the tail claim covers exactly the \
         partition-period writes: {verdict:?}"
    );
    assert!(
        acked.len() > mid_acked.len(),
        "the tail is the partition-period writes"
    );
    let g5 = assert_g5_complete_n(&rig, "mig-r13").await;
    let w = assert_w1_w5_vol(&rig, &rig.volume_id(), 2, PEER_NODE, true).await;

    evidence.invariant(
        "honest_park",
        "pass: the record stayed PREPARED over the partition — never COMPLETE, never a \
         caught-up claim",
    );
    evidence.invariant(
        "foreground_acks_survive",
        "pass: the source kept acknowledging foreground writes through the partition (rule 16) \
         — the oracle's tail claim covers exactly them",
    );
    evidence.invariant(
        "no_cut_artifacts",
        &format!(
            "pass: no barrier, the VM Running, the source Primary, {w_mid} — while partitioned"
        ),
    );
    evidence.invariant("g5_complete", &g5);
    evidence.invariant("w1_w5", &w);
    evidence.outcome(
        "refused then converged: the partitioned cut parked honestly until the heal, the \
         foreground never stopped acknowledging, and the completion is byte-backed over the \
         healed link",
    );
    let record = emit_oracle(&rig, evidence, &acked, boundary, "destination");
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

// ------------------------------------------------------------- row 14

/// Row 14 (§5.6, §9): the abort storm. 25 cycles of prepare ->
/// pre-cut fault -> abort/rollback -> verify-recovery on ONE rig,
/// with a live writer running through the storm (the VM pauses at
/// each cycle's cut attempt and resumes at each rollback). The
/// faults rotate through the pre-cut fail matrix (an abort at or
/// past the cut is a typed refusal by construction — G1/D1a — and
/// forward completion under post-cut faults is row 12's business):
///
/// * F1 — the VMM pause refuses (a pre-shaped lag, stage A row 2's
///   shape): the drive fails at the pause, the consumer aborts.
/// * F2 — the source dies at the transfer op's journal write: the
///   restart's startup pass rolls the pre-cut record back.
/// * F3 — the witness dies inside the barrier commit after the
///   intent append: the restart's replay re-derives the barrier, the
///   source's restart rolls back and the void confirms.
/// * F4 — the same, after the in-memory apply: the restart's
///   roll-forward completes the barrier, the rollback voids it.
/// * F5 — the witness outage: the drive parks at quiesced over the
///   unreachable barrier, the witness returns, the consumer aborts.
///
/// The storm's purpose is RESIDUE and IDEMPOTENCY, not coverage:
/// every cycle first asserts the previous terminal record is intact
/// (immutable facts), every cycle's prepare proves no cut-marker or
/// lease residue lingers (a lingering marker would refuse), the
/// witness view stays exactly epoch 1 at node-a (no lease leak), the
/// source keeps every acknowledged write through all 25 rollbacks,
/// and the whole storm fits its ten-second budget.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_14_abort_storm() {
    let seq = next_id();
    let (mut rig, writer, _transport) = live_scenario(&format!("r14-{seq}")).await;
    let mut evidence = Evidence::new("row-14/abort-storm");
    evidence.fault(
        "abort_storm",
        "25 cycles of prepare -> pre-cut fault -> abort/rollback on one rig; the faults rotate \
         through [pause-refusal park, source kill at the transfer op, witness kill at \
         record_barrier (intent), witness kill at record_barrier (applied), witness outage]",
    );

    let a_log = rig.a.core.journal_dir.join(JOURNAL_LOG_FILE);
    let witness_log = rig.witness.dir.join(JOURNAL_LOG_FILE);
    let size_before_a = std::fs::metadata(&a_log).expect("the source journal").len();
    let size_before_w = std::fs::metadata(&witness_log)
        .expect("the witness journal")
        .len();

    let storm_start = Instant::now();
    for cycle in 0..STORM_CYCLES {
        let mig = format!("mig-storm-{cycle}");

        // The previous terminal record is intact — an immutable
        // fact no later cycle may disturb.
        if cycle > 0 {
            let (status, body) = get_migration(rig.a.addr, &format!("mig-storm-{}", cycle - 1))
                .await
                .served("the previous terminal record");
            assert_eq!(
                status, 200,
                "cycle {cycle}: the previous record answers: {body}"
            );
            assert_eq!(
                state_name(&body_json(&body)),
                "aborted",
                "cycle {cycle}: the previous terminal record is intact: {body}"
            );
        }

        prepare(&rig, &mig).await;
        match cycle % 5 {
            // F1: the pause refuses; the drive parks pre-quiesce; the
            // consumer aborts.
            0 => {
                rig.vmm_a
                    .set_fail(&rig.vm, |knobs| knobs.pause = true)
                    .expect("arm the pause fault");
                let (status, body) = transfer(&rig, &mig).await;
                assert_eq!(
                    status, 202,
                    "cycle {cycle}: the transfer spawns the drive: {body}"
                );
                tokio::time::sleep(FAULT_SETTLE).await;
                let (status, body) = post_abort(rig.a.addr, &mig).await.served("abort");
                assert_eq!(status, 200, "cycle {cycle}: the abort: {body}");
                rig.vmm_a
                    .set_fail(&rig.vm, |knobs| knobs.pause = false)
                    .expect("clear the pause fault");
            }
            // F2: the source dies at the transfer op; the restart's
            // startup pass rolls the pre-cut record back.
            1 => {
                rig.a
                    .core
                    .crash
                    .arm(op_kinds::OP_MIGRATION_TRANSFER, CrashPoint::BeforeOutcome);
                let reply = post_transfer(rig.a.addr, &mig).await;
                assert!(
                    matches!(reply, Reply::Died),
                    "cycle {cycle}: the kill lands before the 202"
                );
                assert!(rig.a.is_killed(), "cycle {cycle}: the kill switch fired");
                rig.a.restart().await;
            }
            // F3/F4: the witness dies inside the barrier commit; the
            // restart lands the barrier (replay or roll-forward) and
            // the source's restart rolls back over it.
            2 | 3 => {
                let point = if cycle % 5 == 2 {
                    StoreSavePoint::WitnessAfterIntentAppend
                } else {
                    StoreSavePoint::WitnessAfterApplyBeforeOutcome
                };
                rig.witness.crash.arm_witness("record_barrier", point);
                let (status, body) = transfer(&rig, &mig).await;
                assert_eq!(
                    status, 202,
                    "cycle {cycle}: the transfer spawns the drive: {body}"
                );
                await_kill(
                    || rig.witness.is_killed(),
                    "cycle {cycle}: the barrier kill",
                )
                .await;
                rig.witness.restart().await;
                rig.a.restart().await;
            }
            // F5: the witness outage; the drive parks at quiesced
            // over the unreachable barrier; the witness returns and
            // the consumer aborts (the void must confirm).
            _ => {
                rig.witness.stop().await;
                let (status, body) = transfer(&rig, &mig).await;
                assert_eq!(
                    status, 202,
                    "cycle {cycle}: the transfer spawns the drive: {body}"
                );
                tokio::time::sleep(FAULT_SETTLE).await;
                rig.witness.restart().await;
                let (status, body) = post_abort(rig.a.addr, &mig).await.served("abort");
                assert_eq!(status, 200, "cycle {cycle}: the abort: {body}");
            }
        }
        poll_migration(rig.a.addr, &mig, "aborted").await;

        // The cheap per-cycle residue checks: the source is serving
        // (VM Running, resource Primary) and the witness is exactly
        // where it was (epoch 1 at node-a, live — no lease leak).
        assert_eq!(
            rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
            VmState::Running,
            "cycle {cycle}: the source VM is resumed"
        );
        assert_eq!(
            role_of(&rig.world_a, &rig.resource()),
            Role::Primary,
            "cycle {cycle}: the source resource is promoted"
        );
        assert_w1_w5_vol(&rig, &rig.volume_id(), 1, NODE, true).await;
    }
    let elapsed = storm_start.elapsed();
    assert!(
        elapsed <= STORM_BOUND,
        "the storm exceeded its 10s budget: {elapsed:.2?} (plan §9 row 14)"
    );

    // The writer through the storm: every acknowledged write survives
    // at the source — 25 rollbacks never lost a byte.
    let (acked, boundary) = writer.join().await;
    let verdict = verify_against(&rig.world_a, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        verdict.prefix_intact(),
        "no acknowledged write was lost through the storm: {verdict:?}"
    );
    assert_eq!(verdict.corrupted, 0);

    // The residue is growth, not state: the journals grew (recorded),
    // but no cut marker, lease or barrier state lingers — every
    // cycle's prepare succeeding is the proof.
    let size_after_a = std::fs::metadata(&a_log).expect("the source journal").len();
    let size_after_w = std::fs::metadata(&witness_log)
        .expect("the witness journal")
        .len();
    assert!(
        size_after_a > size_before_a && size_after_w > size_before_w,
        "the journals grew across the storm (the recorded residue)"
    );

    evidence.invariant(
        "budget",
        &format!(
            "pass: {STORM_CYCLES} cycles in {:.2}s (bound {}s)",
            elapsed.as_secs_f64(),
            STORM_BOUND.as_secs()
        ),
    );
    evidence.invariant(
        "terminal_records_immutable",
        "pass: every cycle found the previous ABORTED record intact",
    );
    evidence.invariant(
        "no_marker_residue",
        "pass: the per-cycle W1-W5/role/VM checks plus the journal-growth accounting hold — \
         the cut marker is voided with each rollback (the abort's participant voids) and \
         nothing accumulates. (Note: prepare itself does not consult the cut marker — the \
         residue evidence is the per-cycle state checks, not a prepare refusal.)",
    );
    evidence.invariant(
        "no_lease_leak",
        "pass: the witness view stayed exactly epoch 1 at node-a, live, at every cycle",
    );
    evidence.invariant(
        "prefix_survives",
        "pass: every acknowledged write is intact at the source through all 25 rollbacks",
    );
    evidence.invariant(
        "journal_growth_recorded",
        &format!(
            "pass: the source journal grew {size_before_a} -> {size_after_a} bytes, the witness \
             journal {size_before_w} -> {size_after_w} bytes (growth is the residue; the \
             observable state is clean)"
        ),
    );
    // The outcome carries the typed marker ("pass") CG4 matches
    // against the outcome text alone (round-1 review, MINOR-3): the
    // storm's honest classification IS a pass — idempotency under
    // rotation with no residue — and the marker states it.
    evidence.outcome(
        "pass: idempotent and residue-free in state: 25 abort cycles on one rig left the \
         source serving, the witness exactly at epoch 1, and every byte intact — the \
         recovery paths (consumer abort, startup rollback, witness replay and roll-forward) \
         are idempotent under rotation",
    );
    let record = emit_oracle(&rig, evidence, &acked, boundary, "source");
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}
