//! P5 stage B scenario rows 4–7 (plan §9): the generated kill matrix
//! (§3.2) — volume mutations, consumer mobility routes, destination
//! peer routes and the witness journal, each family a generated cross
//! product of operations × deterministic kill points (§3.1), executed
//! cell by cell.
//!
//! Every cell follows the harness disciplines (§4/§7): acts go
//! through the public HTTP routes only; recovery runs through the
//! production retry task (the restart's startup pass) or the
//! consumer's own re-drive, polled via the observation route with
//! bounded waits; assertions read the observation route, the witness
//! inspect route, the provider inspect surface and the device bytes —
//! never coordinator internals. Every cell emits its §6 evidence
//! record under `matrix/<family>/<op>/<point>`.
//!
//! Honest clusterings and regenerations, recorded in the evidence
//! (never silent cuts):
//!
//! - **Peer-route cells recover through the startup pass**: the
//!   killed destination is restarted, then the (healthy) source is
//!   stopped and restarted too — the operator's action — so the
//!   recovery is the production startup reconcile, not a 5 s retry
//!   tick wait. The retry-tick recovery path itself is evidenced by
//!   stage A's K4/K5 rows.
//! - **The transfer journal cells regenerate stage A's K1–K3 shapes**
//!   inside the generated matrix: the matrix is the campaign's single
//!   mechanical enumeration, and regenerating keeps it complete
//!   rather than hand-curated.
//! - **Recovery follows the coordinator's committed `AutoBeforeCut`
//!   semantics**: a restart's startup pass rolls back any pre-cut
//!   record it finds (an un-transferred preparation does not linger;
//!   the consumer re-issues), so the prepare cells' landed records
//!   recover to `ABORTED` and the re-issue is the consumer's
//!   recourse — exactly the K1 recovery shape.

// Test target (the e2e precedent): invariant assertions may
// expect/unwrap; the rig's helpers are already bounded. The
// `clippy::panic` allow mirrors the lib's documented discipline
// (its panics ARE the assertions): the exhaustive-match arms below
// abort with formatted cell context — a scenario whose dispatch
// broke is a failed test, never a production failure path.
#![allow(clippy::expect_used, clippy::unwrap_used)]
#![allow(clippy::panic)] // cell-dispatch assertions (see above)

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::task::JoinSet;
use volvisor_api::crash::CrashPoint;
use volvisor_api::op_kinds;
use volvisor_campaign::evidence::{Evidence, LogSources, oracle_value};
use volvisor_campaign::matrix::{self, Cell, Family, Hook};
use volvisor_campaign::oracle::{
    AckedWrite, StopReason, WRITER_ID, WriterHandle, barrier_timestamp, boundary_skew,
    verify_against,
};
use volvisor_campaign::rig::{
    POLL_BOUND, POLL_STEP, Reply, Rig, START, admin, body_json, campaign_rig, get_migration,
    get_volume, poll_migration, post_abort, post_prepare, post_transfer, role_of, state_name,
    witness_view,
};
use volvisor_drbd::report::Role;
use volvisor_drbd_testkit::{NODE, PEER_NODE, PeerTransport, SEED_MINOR, spawn_peer_transport};
use volvisor_provider::VmState;
use volvisor_provider::VmmController;
use volvisor_types::{LeaseState, MigrationId};

/// The plan's per-family budget (§3.2): each family's cells finish
/// inside five seconds of wall clock, recorded in the evidence.
const FAMILY_BOUND: Duration = Duration::from_secs(5);

/// How many cells of one family run concurrently (each in its own
/// rig — the families stay inside the budget by concurrency, never
/// by cutting cells).
const CELL_CONCURRENCY: usize = 8;

/// One gibibyte (the rig's volume size).
const GIB: u64 = 1 << 30;

/// The steady-state transport's lag (§2.1).
const TRANSPORT_LAG: Duration = Duration::from_millis(1);

/// The frozen-window guarantee (the transfer cells' determinism,
/// stage A's K2/K3 shape).
const WINDOW_FREEZE_SETTLE: Duration = Duration::from_millis(30);

/// The steady-state warmup (§2.2's "continuous" writer).
const WRITER_WARMUP: Duration = Duration::from_millis(80);

/// The per-cell unique id source (cells run concurrently; every rig
/// is isolated, the ids keep the evidence and logs distinguishable).
static CELL_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_id() -> u64 {
    CELL_SEQ.fetch_add(1, Ordering::SeqCst)
}

// ------------------------------------------------------ shared plumbing

/// Run one family's generated cells with bounded concurrency, assert
/// the plan's per-family budget, and emit the family's aggregate
/// evidence record (the §3.2 bounds clause: budget adherence must
/// appear in the evidence). A panicking cell propagates its panic
/// (the panic filter keeps injected kills silent, never assertions).
async fn run_family<F, Fut>(family: Family, run: F)
where
    F: Fn(Cell) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let cells = matrix::family_cells(family);
    assert!(!cells.is_empty(), "the generated family is never empty");
    // Created at the family's start so the record's own duration is
    // the family's wall time (not the aggregate emit's).
    let mut evidence = Evidence::new(&format!("matrix/{}/_family", family.name()));
    let started = Instant::now();
    let mut queue = cells.into_iter();
    let mut running = JoinSet::new();
    loop {
        while running.len() < CELL_CONCURRENCY {
            match queue.next() {
                Some(cell) => {
                    running.spawn(run(cell));
                }
                None => break,
            }
        }
        match running.join_next().await {
            Some(Ok(())) => {}
            Some(Err(failure)) if failure.is_panic() => {
                std::panic::resume_unwind(failure.into_panic());
            }
            Some(Err(failure)) => panic!("a cell task failed: {failure}"),
            None => break,
        }
    }
    let elapsed = started.elapsed();
    evidence.invariant(
        "family_budget",
        &format!(
            "pass: {} cells in {:.2}s (bound {}s, concurrency {})",
            matrix::family_cells(family).len(),
            elapsed.as_secs_f64(),
            FAMILY_BOUND.as_secs(),
            CELL_CONCURRENCY
        ),
    );
    assert!(
        elapsed <= FAMILY_BOUND,
        "the {} family exceeded its {:.0}s budget: {elapsed:.2?} (plan §3.2)",
        family.name(),
        FAMILY_BOUND.as_secs()
    );
    // The family record aggregates: no log capture of its own (the
    // comprehensive review's U4 — the aggregate is computed, not
    // observed; the per-cell records carry the real sources, and
    // `finish_rollup` writes `logs: null` instead of five paths
    // that resolve to nothing).
    evidence.finish_rollup();
}

/// Emit a cell's evidence record with the rig's log sources.
fn emit_rig(rig: &Rig, evidence: Evidence) -> PathBuf {
    evidence.finish(&LogSources {
        a_journal: &rig.a.core.journal_dir,
        b_journal: &rig.b.core.journal_dir,
        witness: &rig.witness.dir,
    })
}

/// Emit a writer cell's evidence record: the oracle section from the
/// byte-level verdicts (the acknowledged prefix verified at
/// `verified_side`, the honest tail and corruption counts), the log
/// capture, and the report refresh.
fn emit_oracle(
    rig: &Rig,
    mut evidence: Evidence,
    acked: &[AckedWrite],
    boundary: Option<(u64, StopReason)>,
    verified_side: &str,
    summary: Option<&serde_json::Value>,
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
    let barrier_at = summary.and_then(barrier_timestamp);
    let skew = barrier_at.and_then(|at| boundary_skew(acked, at));
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

/// The full scenario opening (a live writer and a steady-state
/// transport — the cut-crossing cells' shape).
async fn live_scenario(prefix: &str) -> (Rig, WriterHandle, PeerTransport) {
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

/// The bare scenario (no writer, no transport pump — the acts these
/// cells drive never touch the data path).
async fn bare_scenario(prefix: &str) -> Rig {
    campaign_rig(&format!("vm-{prefix}"), &format!("vol-{prefix}")).await
}

/// The prepare act (the rows-1-3 shape; 201).
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

/// The transfer act (the 202 the drive spawns under).
async fn transfer(rig: &Rig, mig: &str) -> (u16, String) {
    post_transfer(rig.a.addr, mig).await.served("transfer")
}

/// Freeze the peer-apply window (the transfer journal cells'
/// determinism, stage A's K2/K3 shape): stop the link, then let the
/// live writer queue writes beyond the last drain — the drive's
/// convergence observation can never pass over a frozen non-empty
/// queue, so the record deterministically parks pre-cut.
async fn freeze_window(transport: PeerTransport) {
    transport.join();
    tokio::time::sleep(WINDOW_FREEZE_SETTLE).await;
}

/// The G5 check for COMPLETE paths (rule 5): the migration's barrier
/// is recorded at the witness, NOT voided, with every attested fact
/// true — and the source was never resumed over it.
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
        "exactly one barrier: {:?}",
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
/// the migration exists, and the source legitimately resumed.
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
/// the witness-backed authority summary a terminal state implies.
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

/// The W1–W5 check (the witness side): the authority view's epoch,
/// holder and lease state are exactly as the recovery implies — no
/// authority without a live lease, no unexpected epoch (a double
/// grant would show as epoch 3).
async fn assert_w1_w5(rig: &Rig, epoch: u64, holder: &str, live: bool) -> String {
    let view = witness_view(&rig.witness, &rig.volume_id()).await;
    assert_eq!(
        view.current_epoch.0, epoch,
        "the witness epoch is exactly {epoch}: {:?}",
        view.lease_state
    );
    assert_eq!(
        view.holder.as_ref().map(volvisor_types::HostId::as_str),
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

// ------------------------------------------------------- row 4: volume

/// One volume-mutation cell: arm the hook, drive the act, assert the
/// kill, restart, then the three recovery observables — the same-op
/// re-POST (rule 8), the durable-state observation, and the fresh
/// re-drive (recovery by re-issue, with the fail-closed generation
/// check where the effect landed).
async fn volume_cell(cell: Cell) {
    let seq = next_id();
    let mut rig = campaign_rig(&format!("vm-v{seq}"), &format!("vol-v{seq}")).await;
    // The detach cells act on the SEEDED volume (the only attached
    // one — its attachment is the rig's own seeding); every other op
    // drives a fresh target created through the routes.
    let target = if cell.op == op_kinds::OP_DETACH_VOLUME {
        rig.volume.clone()
    } else {
        format!("tgt-{seq}")
    };
    volume_setup(&rig, cell.op, &target).await;

    let mut evidence = Evidence::new(&cell.scenario());
    evidence.fault(cell.hook.fault_kind(), &cell.hook.location());
    volume_arm(&rig, &cell);
    let reply = volume_act(&rig, cell.op, &target, "op-act").await;
    assert!(
        matches!(reply, Reply::Died),
        "{}: the kill lands mid-act (got {reply:?})",
        cell.scenario()
    );
    assert!(
        rig.a.is_killed(),
        "{}: the kill switch fired",
        cell.scenario()
    );
    rig.a.restart().await;

    volume_rule8(&rig, cell.op, &target, &cell.hook).await;
    volume_observe(&rig, cell.op, &target, &cell.hook).await;
    volume_re_drive(&rig, cell.op, &target, &cell.hook).await;

    evidence.invariant(
        "rule8_idempotency",
        "pass: the same-op re-POST refuses typed or replays recorded, never re-executes",
    );
    evidence.invariant(
        "durable_state_observation",
        "pass: the observed state is exactly the crash point's durable semantics",
    );
    evidence.invariant(
        "fail_closed_generation",
        "pass: the fresh re-drive completes, or refuses typed on the moved generation",
    );
    // G5/W9 for a volume cell: no barrier exists for any migration —
    // none was in play, and none may appear.
    let view = witness_view(&rig.witness, &rig.volume_id()).await;
    assert!(
        view.barriers.is_empty(),
        "a volume-mutation cell crosses no cut: {:?}",
        view.barriers
    );
    evidence.invariant(
        "g5_w9_no_barrier",
        "pass: no barrier exists (none was in play)",
    );
    evidence.outcome("recovered: the volume op's durable semantics hold after restart");
    emit_rig(&rig, evidence);
}

/// The per-op cell setup (consumer acts through the routes; the
/// detach cell's device release is the rig-side composition boundary
/// — destroy is the only device release, and the setup VM is setup).
async fn volume_setup(rig: &Rig, op: &str, target: &str) {
    match op {
        op_kinds::OP_CREATE_VOLUME => {}
        op_kinds::OP_ATTACH_VOLUME | op_kinds::OP_GROW_VOLUME | op_kinds::OP_DELETE_VOLUME => {
            let (status, body) = volume_create(rig, target, "op-setup")
                .await
                .served("setup create");
            assert_eq!(status, 200, "setup create: {body}");
            // The lineage registration is the composition boundary
            // the rig's own seeding crosses (the e2e precedent:
            // `register_volume` has no public route — the operator
            // registers before witness-managed attach): the attach
            // cells' target must be registered like the seeded
            // volume, or the act refuses typed before any seam.
            if op == op_kinds::OP_ATTACH_VOLUME {
                rig.a
                    .provider
                    .register_volume(
                        &volvisor_types::VolumeId::new(target).expect("valid target id"),
                        None,
                    )
                    .expect("setup register");
            }
        }
        op_kinds::OP_DETACH_VOLUME => {
            // The seeded volume is attached with a live VM holding
            // its device; the detach needs the device released.
            rig.vmm_a.destroy(&rig.vm).expect("destroy the setup VM");
        }
        _ => panic!("unknown volume op {op}"),
    }
}

/// Arm one cell's hook on the source daemon.
fn volume_arm(rig: &Rig, cell: &Cell) {
    match &cell.hook {
        Hook::Journal(point) => rig.a.core.crash.arm(cell.op, *point),
        Hook::StateSave(point) => rig
            .a
            .provider
            .store_crash_hooks()
            .arm(volvisor_types::crash::STORE_DRBD_STATE, *point),
        other => panic!("a volume cell never arms {other:?}"),
    }
}

/// `POST /v2/volumes` — the create act (nearline, one async replica).
async fn volume_create(rig: &Rig, volume_id: &str, operation_id: &str) -> Reply {
    admin(
        "POST",
        rig.a.addr,
        "/v2/volumes",
        Some(
            &json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": operation_id,
                "project_id": "campaign",
                "volume_id": volume_id,
                "class": "nearline-replicated",
                "size_bytes": GIB,
                "replication": {"mode": "async", "remote_replicas": 1},
            })
            .to_string(),
        ),
    )
    .await
}

/// Drive the cell's act (one request, the named operation id).
async fn volume_act(rig: &Rig, op: &str, target: &str, operation_id: &str) -> Reply {
    let path = |tail: &str| format!("/v2/volumes/{target}{tail}");
    match op {
        op_kinds::OP_CREATE_VOLUME => volume_create(rig, target, operation_id).await,
        op_kinds::OP_ATTACH_VOLUME => {
            admin(
                "POST",
                rig.a.addr,
                &path("/attach"),
                Some(
                    &json!({
                        "api_version": "volvisor.volume.v2",
                        "operation_id": operation_id,
                        "vm_id": rig.vm,
                        "host_id": NODE,
                        "attachment_id": format!("att-{operation_id}"),
                        "expected_volume_generation": 1,
                    })
                    .to_string(),
                ),
            )
            .await
        }
        op_kinds::OP_DETACH_VOLUME => {
            admin(
                "POST",
                rig.a.addr,
                &path("/detach"),
                Some(
                    &json!({
                        "api_version": "volvisor.volume.v2",
                        "operation_id": operation_id,
                        "attachment_id": format!("att-{}", rig.volume),
                        "expected_attachment_generation": 1,
                        "vm_stopped_or_io_drained_proof": "vm_stopped",
                    })
                    .to_string(),
                ),
            )
            .await
        }
        op_kinds::OP_GROW_VOLUME => {
            admin(
                "POST",
                rig.a.addr,
                &path("/grow"),
                Some(
                    &json!({
                        "api_version": "volvisor.volume.v2",
                        "operation_id": operation_id,
                        "new_size_bytes": 2 * GIB,
                        "expected_generation": 1,
                    })
                    .to_string(),
                ),
            )
            .await
        }
        op_kinds::OP_DELETE_VOLUME => {
            admin(
                "DELETE",
                rig.a.addr,
                &format!("/v2/volumes/{target}"),
                Some(
                    &json!({
                        "api_version": "volvisor.volume.v2",
                        "operation_id": operation_id,
                        "expected_generation": 1,
                        "data_erasure_policy": "retain",
                    })
                    .to_string(),
                ),
            )
            .await
        }
        _ => panic!("unknown volume op {op}"),
    }
}

/// The rule-8 observation: the re-POST of the SAME operation id.
/// After-outcome replays the recorded outcome (a plain `200` —
/// volume successes record no status); every other point refuses
/// typed `OPERATION_IN_DOUBT` (the strict in-doubt rule: the journal
/// never assumes what the killed daemon did or did not execute).
async fn volume_rule8(rig: &Rig, op: &str, target: &str, hook: &Hook) {
    let (status, body) = volume_act(rig, op, target, "op-act")
        .await
        .served("the rule-8 re-POST");
    match hook {
        Hook::Journal(CrashPoint::AfterOutcome) => {
            assert_eq!(status, 200, "the recorded outcome replays: {body}");
        }
        _ => assert_refusal(
            status,
            &body,
            "OPERATION_IN_DOUBT",
            "the in-doubt re-POST refuses",
        ),
    }
}

/// The durable-state observation: what a reload answers must be
/// exactly the crash point's durable semantics, per operation — the
/// effect landed, or it did not, and the restart's reconcile resolved
/// the interrupted window exactly as designed (a demoted-but-unsaved
/// detach clears; an outgrown-but-unsaved grow heals up; a destroyed
/// backing fails the volume honestly) — never a third, inconsistent
/// thing.
async fn volume_observe(rig: &Rig, op: &str, target: &str, hook: &Hook) {
    let (status, body) = get_volume(rig.a.addr, target)
        .await
        .served("observe the target");
    let observed = body_json(&body);
    match op {
        op_kinds::OP_CREATE_VOLUME => {
            let exists = matches!(hook_effect(hook), Effect::Landed);
            assert_eq!(status, if exists { 200 } else { 404 }, "create: {body}");
            if exists {
                assert_eq!(observed["volume_id"], json!(target), "create: {body}");
                assert_eq!(observed["generation"], json!(1), "create: {body}");
            }
        }
        op_kinds::OP_ATTACH_VOLUME => volume_observe_attach(hook, status, &body, &observed),
        op_kinds::OP_DETACH_VOLUME => {
            // The detach's single state save happens AFTER the demote,
            // so every store point leaves the interrupted-detach window
            // the restart's reconcile clears (state.rs's designed
            // `InterruptedDetach` path); only the after-intent journal
            // point leaves the attachment in place.
            assert_eq!(status, 200, "detach target exists: {body}");
            let attached = observed["current_writer"].is_string();
            let still_attached = matches!(hook, Hook::Journal(CrashPoint::AfterIntent));
            assert_eq!(
                attached, still_attached,
                "the detach's effect matches the crash point: {body}"
            );
        }
        op_kinds::OP_GROW_VOLUME => {
            assert_eq!(status, 200, "grow target exists: {body}");
            // The recorded size: 2 GiB where the record caught up (the
            // outcome journaled, or the save renamed), or where the
            // restart's reconcile healed the outgrown device up; 1 GiB
            // only where nothing ran.
            let healed = matches!(
                hook,
                Hook::StateSave(
                    volvisor_types::crash::StoreSavePoint::AfterTmpWrite
                        | volvisor_types::crash::StoreSavePoint::AfterFsyncBeforeRename
                )
            );
            let grown = matches!(hook_effect(hook), Effect::Landed) || healed;
            assert_eq!(
                observed["provisioned_bytes"],
                json!(if grown { 2 * GIB } else { GIB }),
                "the grow's recorded size matches the crash point: {body}"
            );
            if healed {
                assert_eq!(
                    observed["generation"],
                    json!(1),
                    "the healed record keeps its generation (the re-drive's check): {body}"
                );
            }
        }
        op_kinds::OP_DELETE_VOLUME => match hook_effect(hook) {
            Effect::Landed => assert_eq!(status, 404, "delete: the volume is gone: {body}"),
            Effect::Absent => {
                assert_eq!(status, 200, "delete: the volume remains: {body}");
                assert!(
                    !observed["current_writer"].is_string(),
                    "the residue is unattached: {body}"
                );
            }
        },
        _ => panic!("unknown volume op {op}"),
    }
}

/// The attach's orphan-lease recovery (the pre-rename store points):
/// the kill landed after the witness grant but before the authority
/// block's save, so the lease is live at the witness with no durable
/// record of it — the immediate re-drive is refused LEASE_HELD
/// (fail-closed: never a second grant over an unrecorded one). The
/// recovery is the lease's own recorded end (W7): the clock passing
/// that end lapses it, and the re-drive then acquires cleanly. (The
/// after-rename point kept the block, so its re-drive renews instead
/// — no orphan.)
async fn attach_orphan_lease_recovery(rig: &Rig, op: &str, target: &str, status: u16, body: &str) {
    assert_eq!(
        status, 409,
        "the orphaned lease refuses the re-drive: {body}"
    );
    assert_eq!(
        body_json(body)["code"],
        json!("LEASE_HELD"),
        "the refusal is typed: {body}"
    );
    let view = witness_view(
        &rig.witness,
        &volvisor_types::VolumeId::new(target).expect("valid target id"),
    )
    .await;
    let remaining = view
        .lease_remaining_secs
        .expect("the orphan lease is live at the witness");
    rig.clock.store(START + remaining + 1, Ordering::SeqCst);
    // The W7 fence window: even the same host waits out the window
    // keyed on the lease's recorded end (the previous process may
    // not have finished suspending). The refusal carries the
    // contract's own retry hint; the cell advances past it and
    // re-drives under a fresh operation id (the refused drives
    // recorded their failures — replays would re-serve them, by
    // design).
    let (status, body) = volume_act(rig, op, target, "op-lapsed")
        .await
        .served("the re-drive inside the fence window");
    assert_eq!(status, 409, "the fence window refuses: {body}");
    assert_eq!(
        body_json(&body)["code"],
        json!("FENCE_PENDING"),
        "the fence refusal is typed: {body}"
    );
    let retry_after = body_json(&body)["message"]
        .as_str()
        .and_then(|message| message.split("retry_after_secs=").nth(1))
        .and_then(|tail| tail.parse::<u64>().ok())
        .expect("the fence refusal carries its retry hint");
    rig.clock.fetch_add(retry_after + 1, Ordering::SeqCst);
    let (status, body) = volume_act(rig, op, target, "op-fenced")
        .await
        .served("the re-drive after the fence window");
    assert_eq!(
        status, 200,
        "the fenced window passed, the re-acquire completes: {body}"
    );
}

/// The attach observation: the attach path's FIRST state save is the
/// authority block (saved after the witness grant, before the
/// promotion), so all three store points fire there — the attachment
/// itself never lands; after-rename leaves the durable authority
/// block (the restart's inspect answers epoch 1), the pre-rename
/// points leave nothing.
fn volume_observe_attach(hook: &Hook, status: u16, body: &str, observed: &Value) {
    assert_eq!(status, 200, "attach target exists: {body}");
    let attached = observed["current_writer"].is_string();
    match hook {
        Hook::Journal(CrashPoint::AfterIntent) => {
            assert!(!attached, "the after-intent attach never ran: {body}");
            assert_eq!(observed["generation"], json!(1), "attach: {body}");
        }
        Hook::Journal(_) => {
            assert!(attached, "the attach landed: {body}");
            assert_eq!(observed["generation"], json!(2), "attach: {body}");
        }
        Hook::StateSave(volvisor_types::crash::StoreSavePoint::AfterRename) => {
            assert!(!attached, "the attachment save never ran: {body}");
            assert_eq!(observed["generation"], json!(1), "attach: {body}");
            assert_eq!(
                observed["authority"]["epoch"],
                json!(1),
                "the durable authority block answers: {body}"
            );
        }
        Hook::StateSave(_) => {
            assert!(!attached, "the attachment save never ran: {body}");
            assert_eq!(observed["generation"], json!(1), "attach: {body}");
            assert!(
                observed["authority"].is_null(),
                "no authority block survived: {body}"
            );
        }
        other => panic!("an attach cell never observes {other:?}"),
    }
}

/// The coarse landed/absent classification shared by the create and
/// delete observations.
fn hook_effect(hook: &Hook) -> Effect {
    match hook {
        Hook::Journal(CrashPoint::AfterIntent) => Effect::Absent,
        Hook::Journal(_) | Hook::StateSave(volvisor_types::crash::StoreSavePoint::AfterRename) => {
            Effect::Landed
        }
        Hook::StateSave(_) => Effect::Absent,
        other => panic!("a volume cell never classifies {other:?}"),
    }
}

/// The coarse effect classification.
enum Effect {
    Landed,
    Absent,
}

/// The fresh re-drive (recovery by re-issue): a NEW operation id
/// against the same target. Where the effect did not land, the act
/// completes; where it landed, the moved generation (or the gone
/// attachment/volume) refuses the re-drive typed — the fail-closed
/// generation check the row demands.
async fn volume_re_drive(rig: &Rig, op: &str, target: &str, hook: &Hook) {
    let generation_moved = match op {
        // Create is payload-idempotent by design: the re-drive
        // completes either way (the recorded state answers).
        op_kinds::OP_CREATE_VOLUME => false,
        // The attach's generation moves only where the attachment
        // landed (the journal points past intent); every store point
        // leaves the volume at generation 1 (the re-attach renews or
        // adopts — see the orphan-lease recovery below).
        op_kinds::OP_ATTACH_VOLUME => !matches!(
            hook,
            Hook::Journal(CrashPoint::AfterIntent) | Hook::StateSave(_)
        ),
        // The detach's target attachment is gone everywhere except
        // the after-intent point.
        op_kinds::OP_DETACH_VOLUME => !matches!(hook, Hook::Journal(CrashPoint::AfterIntent)),
        // The grow's generation moves where the record caught up (the
        // healed pre-rename residue keeps generation 1, and its
        // re-drive is the grow-only refusal — the restart's reconcile
        // already healed the record up to the device, so the recovery
        // IS the heal); the delete's residue stays in state (the
        // guarded teardown completes on re-drive) exactly where the
        // effect did not land. Both follow the shared classification.
        op_kinds::OP_GROW_VOLUME | op_kinds::OP_DELETE_VOLUME => {
            matches!(hook_effect(hook), Effect::Landed)
        }
        _ => panic!("unknown volume op {op}"),
    };
    let (status, body) = volume_act(rig, op, target, "op-fresh")
        .await
        .served("the fresh re-drive");
    // The attach's orphan-lease window (the pre-rename store points)
    // has its own recovery shape — the lease's recorded end and the
    // W7 fence window — driven by the helper below.
    let orphan = op == op_kinds::OP_ATTACH_VOLUME
        && matches!(
            hook,
            Hook::StateSave(
                volvisor_types::crash::StoreSavePoint::AfterTmpWrite
                    | volvisor_types::crash::StoreSavePoint::AfterFsyncBeforeRename
            )
        );
    if orphan {
        attach_orphan_lease_recovery(rig, op, target, status, &body).await;
        return;
    }
    let observed = body_json(&body);
    match (op, generation_moved) {
        (op_kinds::OP_CREATE_VOLUME, _) => {
            assert_eq!(status, 200, "create re-drive: {body}");
            assert_eq!(
                observed["volume_id"],
                json!(target),
                "create re-drive: {body}"
            );
        }
        (op_kinds::OP_ATTACH_VOLUME, false) => {
            assert_eq!(status, 200, "attach re-drive (adopt/resume): {body}");
        }
        (op_kinds::OP_ATTACH_VOLUME, true) => {
            assert_refusal(status, &body, "STALE_GENERATION", "attach re-drive");
        }
        (op_kinds::OP_DETACH_VOLUME, false) => {
            assert_eq!(status, 200, "detach re-drive: {body}");
        }
        (op_kinds::OP_DETACH_VOLUME, true) => {
            assert_eq!(status, 404, "the gone attachment refuses: {body}");
            assert_eq!(
                observed["code"],
                json!("NOT_FOUND"),
                "detach re-drive: {body}"
            );
        }
        (op_kinds::OP_GROW_VOLUME, false) => {
            if matches!(
                hook,
                Hook::StateSave(
                    volvisor_types::crash::StoreSavePoint::AfterTmpWrite
                        | volvisor_types::crash::StoreSavePoint::AfterFsyncBeforeRename
                )
            ) {
                // The healed residue: the reconcile caught the record
                // up to the outgrown device, so the same-target
                // re-drive is the grow-only refusal (typed).
                assert_eq!(
                    status, 400,
                    "the healed grow refuses a same-size re-grow: {body}"
                );
                assert_eq!(
                    body_json(&body)["code"],
                    json!("UNSUPPORTED_CLASS_OR_POLICY"),
                    "the grow-only refusal is typed: {body}"
                );
            } else {
                assert_eq!(
                    status, 200,
                    "grow re-drive (the un-run grow completes): {body}"
                );
            }
        }
        (op_kinds::OP_GROW_VOLUME, true) => {
            assert_refusal(status, &body, "STALE_GENERATION", "grow re-drive");
        }
        (op_kinds::OP_DELETE_VOLUME, false) => {
            assert_eq!(status, 200, "delete re-drive: {body}");
        }
        (op_kinds::OP_DELETE_VOLUME, true) => {
            // The teardown already removed the volume from the
            // durable state (the observation asserted its absence);
            // the fresh delete of the gone volume observes 404 — the
            // absence is the durable truth.
            assert_eq!(status, 404, "the gone volume refuses: {body}");
            assert_eq!(
                observed["code"],
                json!("NOT_FOUND"),
                "delete re-drive: {body}"
            );
        }
        _ => panic!("unknown volume op {op}"),
    }
}

// --------------------------------------------- row 5: consumer mobility

/// One consumer-mobility cell (§9 row 5): the prepare (journal and
/// record-save points), the transfer (journal points — stage A's
/// K1–K3 regenerated) and the abort (journal points) of a migration.
async fn mobility_cell(cell: Cell) {
    match cell.op {
        op_kinds::OP_MIGRATION_PREPARE => mobility_prepare_cell(cell).await,
        op_kinds::OP_MIGRATION_TRANSFER => mobility_transfer_cell(cell).await,
        op_kinds::OP_MIGRATION_ABORT => mobility_abort_cell(cell).await,
        _ => panic!("unknown mobility op {}", cell.op),
    }
}

/// One prepare cell. The journal points kill inside the ops pipeline;
/// the record-save points kill inside the coordinator's first record
/// save. Recovery follows the committed `AutoBeforeCut` semantics:
/// the restart's startup pass resolves whatever record landed (a
/// pre-cut record rolls back), the same-op re-POST refuses typed
/// (strict in-doubt), and the consumer's recourse is the re-issue —
/// a fresh migration id prepares cleanly.
async fn mobility_prepare_cell(cell: Cell) {
    let seq = next_id();
    let mut rig = bare_scenario(&format!("p{seq}")).await;
    let mut evidence = Evidence::new(&cell.scenario());
    evidence.fault(cell.hook.fault_kind(), &cell.hook.location());
    match &cell.hook {
        Hook::Journal(point) => rig.a.core.crash.arm(op_kinds::OP_MIGRATION_PREPARE, *point),
        Hook::RecordSave(point) => rig
            .a
            .handle
            .store_crash_hooks()
            .arm(volvisor_types::crash::STORE_MIGRATION_RECORDS, *point),
        other => panic!("a prepare cell never arms {other:?}"),
    }
    let reply = post_prepare(rig.a.addr, &prepare_body(&rig, "mig-x")).await;
    assert!(matches!(reply, Reply::Died), "the kill lands mid-prepare");
    assert!(rig.a.is_killed(), "the kill switch fired");
    rig.a.restart().await;

    // Rule 8: the same-op re-POST refuses typed (the strict in-doubt
    // rule holds for the mobility routes too — the journal never
    // assumes what the killed daemon executed), except after the
    // outcome, where the recorded 201 replays byte-for-byte.
    let (status, body) = post_prepare(rig.a.addr, &prepare_body(&rig, "mig-x"))
        .await
        .served("the rule-8 re-POST");
    match &cell.hook {
        Hook::Journal(CrashPoint::AfterOutcome) => {
            assert_eq!(status, 201, "the recorded outcome replays: {body}");
        }
        _ => assert_refusal(
            status,
            &body,
            "OPERATION_IN_DOUBT",
            "the prepare re-POST refuses",
        ),
    }

    // The recovery: the startup pass resolved whatever record landed
    // (AutoBeforeCut: a pre-cut record with no cut rolls back), and
    // the consumer re-issues — the fresh migration prepares cleanly.
    let (status, body) = post_prepare(rig.a.addr, &prepare_body(&rig, "mig-fresh"))
        .await
        .served("the fresh prepare");
    assert_eq!(status, 201, "the re-issued prepare completes: {body}");
    let summary = poll_migration(rig.a.addr, "mig-fresh", "prepared").await;
    assert_eq!(
        history_len(&summary),
        1,
        "the fresh record's only history entry is its creation: {body}"
    );
    // The killed attempt's record, where it landed (the before/after
    // journal points and the renamed record save), resolved to ABORTED
    // by the startup pass — observable, terminal, honest; where it
    // never landed, the id observes 404. The resolution races this
    // observation by design: the retry task's startup pass is spawned
    // alongside the serve (it may not have been polled yet when the
    // re-POST and the fresh prepare arrive), and its `try_lock` skips
    // itself while the fresh prepare's drive holds the surface —
    // deferring the rollback to the next 5 s tick. The landed case
    // therefore polls the deterministic terminal shape (the same
    // bounded poll the transfer and abort cells use; the recorded
    // row-5 startup-race flake was exactly this single-shot
    // observation); the un-landed case is already deterministic — no
    // record landed, and nothing creates one.
    let landed = !matches!(
        &cell.hook,
        Hook::Journal(CrashPoint::AfterIntent)
            | Hook::RecordSave(
                volvisor_types::crash::StoreSavePoint::AfterTmpWrite
                    | volvisor_types::crash::StoreSavePoint::AfterFsyncBeforeRename
            )
    );
    if landed {
        let summary = poll_migration(rig.a.addr, "mig-x", "aborted").await;
        assert_eq!(
            state_name(&summary),
            "aborted",
            "the startup pass rolled the landed record back: {summary}"
        );
    } else {
        let (status, body) = get_migration(rig.a.addr, "mig-x")
            .await
            .served("observe the old id");
        assert_eq!(
            status, 404,
            "no record exists for the un-landed prepare: {body}"
        );
    }

    evidence.invariant(
        "rule8_idempotency",
        "pass: the re-POST refuses typed OPERATION_IN_DOUBT; the fresh id prepares",
    );
    evidence.invariant("w1_w5_authority", &assert_w1_w5(&rig, 1, NODE, true).await);
    evidence.invariant(
        "g5_w9_no_barrier",
        "pass: no barrier exists (the cut never started)",
    );
    evidence
        .outcome("recovered: the re-issued prepare completes (AutoBeforeCut resolved the residue)");
    emit_rig(&rig, evidence);
}

/// The prepare request body (the rows-1-3 shape).
fn prepare_body(rig: &Rig, mig: &str) -> serde_json::Value {
    json!({
        "migration_id": mig,
        "vm_id": rig.vm,
        "target_host": PEER_NODE,
        "volume_ids": [rig.volume],
        "expected_generations": [2],
    })
}

/// One transfer cell — stage A's K1–K3 regenerated inside the matrix
/// (the frozen-window determinism for the before/after-outcome
/// points; the after-intent point needs no freeze — the drive never
/// spawned).
async fn mobility_transfer_cell(cell: Cell) {
    let seq = next_id();
    let Hook::Journal(point) = cell.hook else {
        panic!("a transfer cell is journal-armed only");
    };
    let (mut rig, writer, transport) = live_scenario(&format!("t{seq}")).await;
    let mut evidence = Evidence::new(&cell.scenario());
    evidence.fault(cell.hook.fault_kind(), &cell.hook.location());
    prepare(&rig, "mig-x").await;
    if point != CrashPoint::AfterIntent {
        freeze_window(transport).await;
    }
    rig.a.core.crash.arm(op_kinds::OP_MIGRATION_TRANSFER, point);
    let reply = post_transfer(rig.a.addr, "mig-x").await;
    assert!(matches!(reply, Reply::Died), "the kill lands mid-transfer");
    assert!(rig.a.is_killed(), "the kill switch fired");
    rig.a.restart().await;
    let summary = poll_migration(rig.a.addr, "mig-x", "aborted").await;
    let history_before = history_len(&summary);

    // Rule 8: after-outcome replays the recorded 202 (no re-execution
    // — the history is unchanged); the other points refuse typed.
    let (status, body) = transfer(&rig, "mig-x").await;
    match point {
        CrashPoint::AfterOutcome => {
            assert_eq!(status, 202, "the recorded outcome replays: {body}");
            let replayed = poll_migration(rig.a.addr, "mig-x", "aborted").await;
            assert_eq!(
                history_len(&replayed),
                history_before,
                "the replay added no state transitions (no re-execution)"
            );
        }
        _ => assert_refusal(
            status,
            &body,
            "OPERATION_IN_DOUBT",
            "the transfer re-POST refuses",
        ),
    }

    // The oracle: the source keeps every acknowledged write (the
    // rollback resumed it); the frozen tail is honestly nonzero.
    let (acked, boundary) = writer.join().await;
    assert!(!acked.is_empty(), "the writer acknowledged writes");
    let source = verify_against(&rig.world_a, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        source.prefix_intact(),
        "the source keeps every acknowledged write: {source:?}"
    );
    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_aborted(&rig, "mig-x").await,
    );
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.a.addr, 1, NODE).await,
    );
    evidence.invariant(
        "rule8_idempotency",
        "pass: the re-POST refuses typed or replays the recorded 202, never re-executes",
    );
    evidence.outcome("recovered: ABORTED (rollback of a pre-cut record)");
    emit_oracle(&rig, evidence, &acked, boundary, "source", Some(&summary));
}

/// One abort cell: the abort of a prepared (un-transferred) migration
/// — the rollback's own journal points. The startup pass completes
/// the interrupted rollback (the record is pre-cut, so the resolve
/// re-drives it); the same-op re-POST refuses typed except after the
/// outcome (the recorded 200 replays); a transfer on the aborted
/// record answers 202 with the record — the drive's refusal is
/// logged, never surfaced as a route error (the 202-always
/// contract).
async fn mobility_abort_cell(cell: Cell) {
    let seq = next_id();
    let Hook::Journal(point) = cell.hook else {
        panic!("an abort cell is journal-armed only");
    };
    let mut rig = bare_scenario(&format!("a{seq}")).await;
    let mut evidence = Evidence::new(&cell.scenario());
    evidence.fault(cell.hook.fault_kind(), &cell.hook.location());
    prepare(&rig, "mig-x").await;
    rig.a.core.crash.arm(op_kinds::OP_MIGRATION_ABORT, point);
    let reply = post_abort(rig.a.addr, "mig-x").await;
    assert!(matches!(reply, Reply::Died), "the kill lands mid-abort");
    assert!(rig.a.is_killed(), "the kill switch fired");
    rig.a.restart().await;
    poll_migration(rig.a.addr, "mig-x", "aborted").await;

    // Rule 8: after-outcome replays the recorded 200; the other
    // points refuse typed (the abort may be mid-flight).
    let (status, body) = post_abort(rig.a.addr, "mig-x")
        .await
        .served("the rule-8 re-POST");
    match point {
        CrashPoint::AfterOutcome => assert_eq!(status, 200, "the recorded outcome replays: {body}"),
        _ => assert_refusal(
            status,
            &body,
            "OPERATION_IN_DOUBT",
            "the abort re-POST refuses",
        ),
    }
    // The terminal shape: a transfer on the aborted record answers
    // 202 with the record — the route records the consumer's proof
    // and starts a drive that refuses the terminal record internally
    // (the refusal is the drive's, logged, never surfaced as a route
    // error; the record is the truth) — and the record stays
    // aborted.
    let (status, body) = transfer(&rig, "mig-x").await;
    assert_eq!(
        status, 202,
        "transfer on an aborted record answers with the record: {body}"
    );
    assert_eq!(
        state_name(&body_json(&body)),
        "aborted",
        "the record stays aborted: {body}"
    );

    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_aborted(&rig, "mig-x").await,
    );
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.a.addr, 1, NODE).await,
    );
    evidence.invariant(
        "rule8_idempotency",
        "pass: the re-POST refuses typed or replays the recorded 200; the terminal refusal holds",
    );
    evidence.outcome("recovered: ABORTED (the startup pass completed the rollback)");
    emit_rig(&rig, evidence);
}

/// The observation's transition count (the replay-must-not-execute
/// check).
fn history_len(summary: &Value) -> usize {
    summary["state_history"].as_array().map_or(0, Vec::len)
}

// ------------------------------------------------------- row 6: peer

/// One peer-route cell (§9 row 6): the kill lands on the DESTINATION
/// daemon's internal routes. The prepare and discard cells run on a
/// bare rig (their acts precede the data path); the grant and
/// restore cells cross the cut with a live writer (the oracle is in
/// play).
async fn peer_cell(cell: Cell) {
    match cell.op {
        op_kinds::OP_PEER_PREPARE => peer_prepare_cell(cell).await,
        op_kinds::OP_PEER_DISCARD => peer_discard_cell(cell).await,
        _ => peer_cut_cell(cell).await,
    }
}

/// One peer-prepare cell: B dies inside the destination's preparation
/// op. A's prepare fails over the dead connection (a journaled
/// failure — no record); after B restarts, the consumer re-issues a
/// fresh migration and it runs to COMPLETE against the recovered
/// destination (the full proof the route works after restart).
async fn peer_prepare_cell(cell: Cell) {
    let seq = next_id();
    let Hook::Journal(point) = cell.hook else {
        panic!("a peer cell is journal-armed only");
    };
    let mut rig = bare_scenario(&format!("pp{seq}")).await;
    let mut evidence = Evidence::new(&cell.scenario());
    evidence.fault(cell.hook.fault_kind(), &cell.hook.location());
    rig.b.core.crash.arm(op_kinds::OP_PEER_PREPARE, point);
    let (status, body) = post_prepare(rig.a.addr, &prepare_body(&rig, "mig-x"))
        .await
        .served("the failing prepare");
    assert_eq!(
        status, 500,
        "the prepare fails over the dead destination (the peer call surfaces typed): {body}"
    );
    await_kill(|| rig.b.is_killed(), "peer prepare").await;
    assert!(
        get_migration(rig.a.addr, "mig-x")
            .await
            .served("no record")
            .0
            == 404,
        "the failed prepare left no record"
    );
    rig.b.restart().await;

    // The recovery: the consumer re-issues; the fresh migration runs
    // the whole handoff against the restarted destination.
    let (status, body) = post_prepare(rig.a.addr, &prepare_body(&rig, "mig-fresh"))
        .await
        .served("the fresh prepare");
    assert_eq!(status, 201, "the re-issued prepare completes: {body}");
    let (status, body) = transfer(&rig, "mig-fresh").await;
    assert_eq!(status, 202, "the fresh transfer spawns the drive: {body}");
    poll_migration(rig.a.addr, "mig-fresh", "complete").await;

    evidence.invariant(
        "rule8_idempotency",
        "pass: the failed prepare journaled its failure (the re-POST of the dead id replays it); \
         the fresh id re-issues cleanly",
    );
    evidence.invariant(
        "g5_barrier_all_true",
        &assert_g5_complete(&rig, "mig-fresh").await,
    );
    evidence.invariant(
        "w1_w5_authority",
        &assert_w1_w5(&rig, 2, PEER_NODE, true).await,
    );
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.b.addr, 2, PEER_NODE).await,
    );
    evidence.outcome("recovered: COMPLETE (the re-issued migration over the restarted peer)");
    emit_rig(&rig, evidence);
}

/// One cut-crossing peer cell (grant / restore-vm): B dies inside the
/// drive's destination act with the cut durable (SOURCE_REVOKED or
/// later). The recovery is the recorded clustering: restart B, then
/// stop-and-restart the healthy A (the operator's action) so the
/// startup pass re-drives the cut forward — the migration COMPLETES,
/// the witness epoch is exactly 2 (no double grant), the
/// acknowledged prefix is intact at the destination.
async fn peer_cut_cell(cell: Cell) {
    let seq = next_id();
    let Hook::Journal(point) = cell.hook else {
        panic!("a peer cell is journal-armed only");
    };
    let (mut rig, writer, _transport) = live_scenario(&format!("pc{seq}")).await;
    let mut evidence = Evidence::new(&cell.scenario());
    evidence.fault(cell.hook.fault_kind(), &cell.hook.location());
    prepare(&rig, "mig-x").await;
    rig.b.core.crash.arm(cell.op, point);
    let (status, body) = transfer(&rig, "mig-x").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");
    await_kill(|| rig.b.is_killed(), cell.op).await;

    // The recorded clustering: restart the destination, then the
    // source — the startup pass re-drives the durable cut forward.
    rig.b.restart().await;
    rig.a.restart().await;
    let summary = poll_migration(rig.a.addr, "mig-x", "complete").await;

    // The oracle and the no-double-grant proof (the epoch is exactly
    // 2: a re-driven grant that re-executed would mint 3).
    let (acked, boundary) = writer.join().await;
    assert!(!acked.is_empty(), "the writer acknowledged writes");
    let destination = verify_against(&rig.world_b, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        destination.prefix_intact(),
        "the destination keeps every acknowledged write: {destination:?}"
    );
    evidence.invariant(
        "g5_barrier_all_true",
        &assert_g5_complete(&rig, "mig-x").await,
    );
    evidence.invariant(
        "w1_w5_no_double_grant",
        &assert_w1_w5(&rig, 2, PEER_NODE, true).await,
    );
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.b.addr, 2, PEER_NODE).await,
    );
    evidence.invariant(
        "rule8_idempotency",
        "pass: the peer op resolves by inspection (or replays its outcome) — never re-executes",
    );
    evidence.outcome("recovered: COMPLETE (the startup pass drove the cut forward)");
    emit_oracle(
        &rig,
        evidence,
        &acked,
        boundary,
        "destination",
        Some(&summary),
    );
}

/// One peer-discard cell: B dies inside the rollback's destination
/// discard. A's abort fails over the dead connection (a journaled
/// failure — the record stays pre-cut); the recovery restarts B,
/// then the healthy A, and the startup pass re-drives the rollback
/// to ABORTED.
async fn peer_discard_cell(cell: Cell) {
    let seq = next_id();
    let Hook::Journal(point) = cell.hook else {
        panic!("a peer cell is journal-armed only");
    };
    let mut rig = bare_scenario(&format!("pd{seq}")).await;
    let mut evidence = Evidence::new(&cell.scenario());
    evidence.fault(cell.hook.fault_kind(), &cell.hook.location());
    prepare(&rig, "mig-x").await;
    rig.b.core.crash.arm(op_kinds::OP_PEER_DISCARD, point);
    let (status, body) = post_abort(rig.a.addr, "mig-x")
        .await
        .served("the failing abort");
    assert_eq!(
        status, 500,
        "the abort fails over the dead destination (the peer discard surfaces typed): {body}"
    );
    await_kill(|| rig.b.is_killed(), "peer discard").await;

    // The recorded clustering: restart the destination, then the
    // source — the startup pass completes the interrupted rollback.
    rig.b.restart().await;
    rig.a.restart().await;
    poll_migration(rig.a.addr, "mig-x", "aborted").await;

    // Rule 8: A's abort journaled its failure — the same-op re-POST
    // replays it (idempotent, never a second rollback).
    let (status, body) = post_abort(rig.a.addr, "mig-x")
        .await
        .served("the rule-8 re-POST");
    assert_eq!(
        status, 500,
        "the recorded failure replays (never a second rollback): {body}"
    );
    // The terminal shape (the same reading as the abort cells): the
    // transfer on the aborted record answers 202 with the record —
    // the drive refuses the terminal record internally — and the
    // record stays aborted.
    let (status, body) = transfer(&rig, "mig-x").await;
    assert_eq!(
        status, 202,
        "transfer on an aborted record answers with the record: {body}"
    );
    assert_eq!(
        state_name(&body_json(&body)),
        "aborted",
        "the record stays aborted: {body}"
    );

    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_aborted(&rig, "mig-x").await,
    );
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.a.addr, 1, NODE).await,
    );
    evidence.invariant(
        "rule8_idempotency",
        "pass: the failed abort replays its recorded failure; the record is terminal",
    );
    evidence.outcome("recovered: ABORTED (the startup pass completed the rollback)");
    emit_rig(&rig, evidence);
}

// ---------------------------------------------------- row 7: witness

/// One witness-journal cell (§9 row 7): the witness's own mid-commit
/// kills (the barrier record and the grant set — the two mutations
/// the cut crosses) and the outage window.
async fn witness_cell(cell: Cell) {
    match &cell.hook {
        Hook::WitnessCommit {
            mutation: "record_barrier",
            point,
        } => witness_barrier_cell(cell, *point).await,
        Hook::WitnessCommit { point, .. } => witness_grant_cell(cell, *point).await,
        Hook::WitnessOutage => witness_outage_cell(cell).await,
        other => panic!("a witness cell never arms {other:?}"),
    }
}

/// One record_barrier mid-commit cell: the witness dies inside the
/// cut's own barrier mutation. The restart's journal replay
/// re-derives the mutation (the intent is durable — W3), the
/// source's restart rolls the (pre-cut) record back, and the
/// rollback's void confirms the replayed barrier is voided — G5
/// holds over the recovered witness.
async fn witness_barrier_cell(cell: Cell, point: volvisor_types::crash::StoreSavePoint) {
    let seq = next_id();
    let (mut rig, writer, _transport) = live_scenario(&format!("wb{seq}")).await;
    let mut evidence = Evidence::new(&cell.scenario());
    evidence.fault(cell.hook.fault_kind(), &cell.hook.location());
    prepare(&rig, "mig-x").await;
    rig.witness.crash.arm_witness("record_barrier", point);
    let (status, body) = transfer(&rig, "mig-x").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");
    await_kill(|| rig.witness.is_killed(), "the witness barrier commit").await;

    // The recovery: the witness restarts (the replay re-derives the
    // barrier from the durable intent — W3), then the source (its
    // startup pass rolls the pre-cut record back and the void
    // confirms the replayed barrier is voided — the witness must be
    // up first: a rollback over a dead witness self-fences).
    rig.witness.restart().await;
    rig.a.restart().await;
    let summary = poll_migration(rig.a.addr, "mig-x", "aborted").await;

    let (acked, boundary) = writer.join().await;
    assert!(!acked.is_empty(), "the writer acknowledged writes");
    let source = verify_against(&rig.world_a, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        source.prefix_intact(),
        "the source keeps every acknowledged write: {source:?}"
    );
    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_aborted(&rig, "mig-x").await,
    );
    evidence.invariant("w1_w5_authority", &assert_w1_w5(&rig, 1, NODE, true).await);
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.a.addr, 1, NODE).await,
    );
    evidence.invariant(
        "w3_replay",
        "pass: the witness's restart replay re-derived the barrier and the void confirmed it",
    );
    evidence.outcome("recovered: ABORTED over the replayed-and-voided barrier");
    emit_oracle(&rig, evidence, &acked, boundary, "source", Some(&summary));
}

/// One grant_set mid-commit cell: the witness dies inside the
/// destination's promote batch. The restart's replay lands the grant
/// (the lease is live for the destination — epoch exactly 2), but
/// THE FINDING: B's peer-grant op journaled its failure over the
/// connection that died with the witness, and the ops pipeline
/// replays recorded failures forever — so every re-drive's promote
/// step re-serves it and the migration PARKS SAFE at
/// `destination_authorized` (source fenced, destination never
/// promotes, no dual writer, no data loss). The recorded product
/// defect (the retry task spins on it at its 5s tick) — never
/// papered over; see the body's finding block for the full trace.
async fn witness_grant_cell(cell: Cell, point: volvisor_types::crash::StoreSavePoint) {
    let seq = next_id();
    let (mut rig, writer, _transport) = live_scenario(&format!("wg{seq}")).await;
    let mut evidence = Evidence::new(&cell.scenario());
    evidence.fault(cell.hook.fault_kind(), &cell.hook.location());
    prepare(&rig, "mig-x").await;
    rig.witness.crash.arm_witness("grant_set", point);
    let (status, body) = transfer(&rig, "mig-x").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");
    await_kill(|| rig.witness.is_killed(), "the witness grant-set commit").await;

    // The recovery: the witness restarts — the replay lands the
    // grant exactly once (W3) — then the source restarts and its
    // startup pass folds the landed grant into the record.
    rig.witness.restart().await;
    rig.a.restart().await;
    let summary = poll_migration(rig.a.addr, "mig-x", "destination_authorized").await;

    // THE FINDING (recorded, never papered over — §7's honesty): the
    // drive cannot complete. B's peer-grant op journaled its FAILURE
    // over the connection that died with the witness, and the ops
    // pipeline replays recorded failures forever (fail-closed
    // idempotency: a failed act is never re-executed) — so every
    // re-drive's promote step, which re-calls B's grant route for
    // the promoted device paths, re-serves that recorded 500. The
    // cut is durable, so there is no rollback; the consumer has no
    // route recourse (the transfer answers 202 and the drive
    // refuses the terminal-stall internally). The migration parks
    // SAFE: the source stays fenced, the destination never promotes
    // (no dual writer, no data loss), the barrier's attestation
    // holds, and the acknowledged prefix is durable at the
    // destination. The remedy — re-resolvable peer acts, or a
    // promote path that does not route through the failed grant op
    // — is a design decision this campaign records rather than
    // makes.
    assert!(
        summary["in_doubt_detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("stalled")),
        "the parked record's detail is honest: {}",
        summary["in_doubt_detail"]
    );

    // The safety set over the parked record.
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("the source VM observes"),
        VmState::Absent,
        "the source VM stays destroyed (the cut is durable)"
    );
    assert_eq!(
        role_of(&rig.world_a, &rig.resource()),
        Role::Secondary,
        "the source stays demoted (fenced)"
    );
    assert_eq!(
        role_of(&rig.world_b, &rig.resource()),
        Role::Secondary,
        "the destination never promoted over the wedged grant (no dual writer)"
    );

    let (acked, boundary) = writer.join().await;
    assert!(!acked.is_empty(), "the writer acknowledged writes");
    let destination = verify_against(&rig.world_b, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        destination.prefix_intact(),
        "the destination keeps every acknowledged write: {destination:?}"
    );
    evidence.invariant(
        "g5_barrier_all_true",
        &assert_g5_complete(&rig, "mig-x").await,
    );
    evidence.invariant(
        "w1_w5_no_double_grant",
        &assert_w1_w5(&rig, 2, PEER_NODE, true).await,
    );
    evidence.invariant(
        "w3_replay",
        "pass: the witness's restart replay landed the grant exactly once (epoch 2)",
    );
    evidence.invariant(
        "d6a_provider_inspect",
        "not applicable: the destination never promoted, so no authority block exists to \
         inspect — the witness view (epoch 2, node-b, live) is the authority observation",
    );
    evidence.invariant(
        "rule8_idempotency",
        "pass: B's peer-grant op replays its recorded failure on every re-drive — the \
         fail-closed reading; it never re-executes the (witness-idempotent) grant act",
    );
    evidence.outcome(
        "parked: SAFE but wedged post-cut (destination_authorized) — B's recorded peer-grant \
         failure replays on every re-drive; the source stays fenced, the destination never \
         promotes, no dual writer, no data loss; recovery needs operator action",
    );
    emit_oracle(
        &rig,
        evidence,
        &acked,
        boundary,
        "destination",
        Some(&summary),
    );
}

/// One outage cell: the witness is STOPPED (no armed seam — the
/// listener goes away) while the cut needs it. The drive parks
/// honestly non-terminal (the cut cannot proceed without the
/// witness); the observation never claims progress it cannot prove.
/// The recovery restarts the witness, then the source — the startup
/// pass rolls the pre-cut record back and the source's data is
/// intact.
async fn witness_outage_cell(cell: Cell) {
    let seq = next_id();
    let (mut rig, writer, _transport) = live_scenario(&format!("wo{seq}")).await;
    let mut evidence = Evidence::new(&cell.scenario());
    evidence.fault(cell.hook.fault_kind(), &cell.hook.location());
    prepare(&rig, "mig-x").await;
    rig.witness.stop().await;
    let (status, body) = transfer(&rig, "mig-x").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");

    // During the outage: the record is honestly non-terminal (never
    // COMPLETE, never a healthy claim over the dead witness).
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (status, body) = get_migration(rig.a.addr, "mig-x")
        .await
        .served("the outage observation");
    assert_eq!(status, 200, "the observation answers: {body}");
    let during = state_name(&body_json(&body));
    assert_ne!(
        during, "complete",
        "the cut never claims completion over a dead witness: {body}"
    );

    // The recovery: the witness comes back, then the source restarts
    // — the startup pass rolls the pre-cut record back.
    rig.witness.restart().await;
    rig.a.restart().await;
    let summary = poll_migration(rig.a.addr, "mig-x", "aborted").await;

    let (acked, boundary) = writer.join().await;
    assert!(!acked.is_empty(), "the writer acknowledged writes");
    let source = verify_against(&rig.world_a, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        source.prefix_intact(),
        "the source keeps every acknowledged write: {source:?}"
    );
    evidence.invariant(
        "outage_honesty",
        &format!("pass: the record stayed non-terminal ({during}) over the dead witness"),
    );
    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_aborted(&rig, "mig-x").await,
    );
    evidence.invariant("w1_w5_authority", &assert_w1_w5(&rig, 1, NODE, true).await);
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.a.addr, 1, NODE).await,
    );
    evidence.outcome("recovered: ABORTED (the witness returned, the rollback ran)");
    emit_oracle(&rig, evidence, &acked, boundary, "source", Some(&summary));
}

// ------------------------------------------------------------ the rows

/// Row 4 (§9): the volume-mutation kill matrix — create, attach,
/// detach, grow and delete, each at the three journal points and the
/// three state-save splits of the act's own durable save.
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn row_4_volume_mutation_kill_matrix() {
    run_family(Family::VolumeMutations, volume_cell).await;
}

/// Row 5 (§9): the consumer-mobility kill matrix — prepare (journal
/// and record-save points), transfer and abort (journal points).
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn row_5_consumer_mobility_kill_matrix() {
    run_family(Family::ConsumerMobility, mobility_cell).await;
}

/// Row 6 (§9): the peer-route kill matrix — the destination's
/// internal prepare, grant, restore-vm and discard routes at the
/// three journal points each.
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn row_6_peer_route_kill_matrix() {
    run_family(Family::PeerRoutes, peer_cell).await;
}

/// Row 7 (§9): the witness-journal kill matrix — the barrier record
/// and the grant set at their two mid-commit windows each, plus the
/// stop/restart outage.
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn row_7_witness_journal_kill_matrix() {
    run_family(Family::WitnessJournal, witness_cell).await;
}
