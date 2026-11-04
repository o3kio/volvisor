//! The convergence-horizon scenarios (the grant_set wedge's horizon
//! remedy — the diagnosis doc's horizon section, PR #33): the two
//! mechanisms that close the wedge's recovery horizon.
//!
//! **Mechanism 1 — grant-hold lease renewal during parks.** The
//! destination's renewal task renews the grant-hold leases (the
//! pre-promote participants of a live preparation) alongside its own
//! volume leases: no new authority (usable only through
//! `promote_target`'s existing gate), and the renewal is coupled to
//! uniform liveness — a preparation with any dead member is skipped
//! whole so its members converge to the uniformly-dead shape the
//! fresh-grant gate arbitrates (renewing a survivor would block that
//! gate forever). [`park_past_ttl_with_the_witness_alive_renews_and_converges`]
//! drives a park past the lease's original end (two and a half
//! TTLs deep) and converges with the epoch exactly
//! 2 (no re-mint ever); [`renewal_stops_at_the_terminal_shape`] is
//! the api-crate test for the terminal stop (the campaign tail asserts
//! the converged record's shape).
//!
//! **Mechanism 2 — the gated fresh-grant re-mint.** When the
//! recorded grant's leases are provably dead (the witness
//! authoritatively reports every participant Expired and still held
//! by this host) and the park is provably pre-promote with the cut's
//! fence evidence intact, the grant act re-issues under a NEW derived
//! operation id — the journaled attempt counter folded into the
//! reserved namespace — and the re-minted grant drives the promotes
//! to completion. [`witness_outage_past_ttl_the_fresh_grant_converges`]
//! proves the convergence (epoch exactly 3);
//! [`fresh_grant_survives_a_kill_inside_its_own_commit`] and
//! [`fresh_grant_attempt_counter_survives_a_kill_at_the_bump`] kill
//! the fresh grant's own durable boundaries and prove the re-drive's
//! determinism (the same attempt id replays, never a second mint;
//! the bumped counter survives its own kill). The negatives:
//! [`fresh_grant_refused_at_promote_parks_honestly`] (gate b — a
//! re-mint would fence a legitimate promote),
//! [`fresh_grant_never_mints_over_a_live_member`] (the still-live
//! boundary — the mixed park spins, the re-mint never fires),
//! [`fresh_grant_refused_when_the_world_moved`] (the source's host
//! re-granted over the dead lease — a live foreign lease is never
//! re-minted over), and
//! [`undriven_mixed_park_lapses_to_the_fresh_grant`] — the composite:
//! an undriven mixed park is left to lapse by the coupling, and the
//! now-uniformly-dead park converges through the re-mint.
//!
//! Time compression: the rig's frozen witness clock plus its 100 s
//! lease TTL (the existing §3.3 seam — no new seam introduced). A
//! park's lease death is a single deliberate clock jump past the
//! end (the renewal's due window is never entered, so nothing
//! renews); a renewal park advances in due-window steps (85 s: after
//! each renewal the next due window opens TTL−INTERVAL on, so every
//! step lands inside it with the lease still live), polling for
//! each renewal to land — the rig's 1 s renewal tick is the thing
//! being waited for, never assumed. A re-drive is fired either by
//! the daemon's own retry tick (the production cadence, observed in
//! the spin watches: one full tick plus load headroom) or by a
//! restart's startup sweep — the same production retry_pass the
//! tick runs, the healed cells' precedent for a scenario whose
//! subject is a boundary rather than the cadence.
//!
//! Every scenario is self-contained (the campaign's house style:
//! each file defines its own helpers over the rig's public surface),
//! emits its evidence record under `matrix/`-parallel scenario names
//! (discovered by `campaign-summary`; no fixed list to register),
//! and asserts the full safety set over whatever shape it ends in —
//! converged rows get the data-path oracle, parked rows get the
//! honest-spin proof (the typed refusal reproduces, the epoch never
//! moves, nothing resurrects).

#![allow(clippy::expect_used, clippy::unwrap_used)] // the rig's own idiom
#![allow(clippy::panic)] // scenario assertions (see above)

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use volvisor_api::peer::{TargetPreparation, TargetPreparationStore};
use volvisor_campaign::evidence::{Evidence, LogSources, oracle_value};
use volvisor_campaign::oracle::{
    AckedWrite, StopReason, WRITER_ID, WriterHandle, barrier_timestamp, boundary_skew,
    verify_against,
};
use volvisor_campaign::rig::{
    INTERVAL, POLL_BOUND, POLL_STEP, Rig, TTL, body_json, campaign_rig, campaign_rig_volumes,
    get_migration, poll_migration, post_prepare, post_transfer, role_of, state_name, witness_view,
};
use volvisor_drbd::report::Role;
use volvisor_drbd_testkit::{NODE, PEER_NODE, PeerTransport, SEED_MINOR, spawn_peer_transport};
use volvisor_journal::{Journal, JournalRecord};
use volvisor_provider::VmState;
use volvisor_types::crash::{STORE_PEER_PREPARATION, StoreSavePoint};
use volvisor_types::{HostId, LeaseState, MigrationId, OperationId, VolumeId};
use volvisor_witness::client::WitnessConnection;
use volvisor_witness::proto::{GrantRequest, RenewRequest, WITNESS_PROTOCOL_VERSION};
use volvisord::handoff::peer_preparations_dir;

// ---------------------------------------------------------- constants

/// The steady-state warmup (§2.2's "continuous" writer).
const WRITER_WARMUP: Duration = Duration::from_millis(80);

/// The steady-state transport's lag (§2.1).
const TRANSPORT_LAG: Duration = Duration::from_millis(1);

/// One stepped clock advance (witness seconds) for a renewal park:
/// after each renewal the lease has the full TTL of life and the
/// next due window opens at `TTL − INTERVAL` — a step of 85 lands
/// in every due window (15 s of life left, due, still live), so
/// each step makes exactly one renewal due and the park crosses
/// the original end on the renewal's coattails.
const CLOCK_STEP: u64 = 85;

/// How long a parked scenario watches its spin: one full retry tick
/// (the production `MIGRATION_RETRY_TICK`, ~5 s) plus ~3 s of
/// re-drive, observation and load headroom (the PR #21 amendment's
/// derived-headroom precedent) — enough to prove the refusal
/// reproduces on the daemon's own tick, without pretending to
/// observe forever.
const SPIN_WATCH: Duration = Duration::from_secs(8);

/// How long the mixed-liveness scenario watches the grant-hold
/// renewal pass (its tick is the rig's 1 s renewal cadence): three
/// passes is the proof the survivor was skipped, not sampled
/// between ticks.
const COUPLING_WATCH: Duration = Duration::from_secs(3);

/// The per-scenario unique id source (scenarios run concurrently in
/// the suite; every rig is isolated, the ids keep the evidence and
/// logs distinguishable).
static SCENARIO_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_id() -> u64 {
    SCENARIO_SEQ.fetch_add(1, Ordering::SeqCst)
}

// ------------------------------------------------------ shared plumbing

/// Emit a scenario's evidence record with the rig's log sources.
fn emit_rig(rig: &Rig, evidence: Evidence) -> PathBuf {
    evidence.finish(&LogSources {
        a_journal: &rig.a.core.journal_dir,
        b_journal: &rig.b.core.journal_dir,
        witness: &rig.witness.dir,
    })
}

/// Emit a single-volume scenario's evidence record: the oracle
/// section from the byte-level verdicts (the acknowledged prefix
/// verified at `verified_side`, the honest tail and corruption
/// counts), plus the boundary cross-check (§2.3 rule 1) from the
/// migration summary whenever the coordinator recorded a barrier.
fn emit_oracle(
    rig: &Rig,
    mut evidence: Evidence,
    acked: &[AckedWrite],
    boundary: Option<(u64, StopReason)>,
    verified_side: &str,
    summary: Option<&Value>,
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

/// Emit a multi-volume scenario's evidence record: one oracle value
/// per participant (each writer's acknowledged prefix verified at
/// `verified_side`), each carrying the boundary cross-check against
/// the migration's barrier when one was recorded.
fn emit_oracle_n(
    rig: &Rig,
    mut evidence: Evidence,
    acked: &[Vec<AckedWrite>],
    boundary: Option<(u64, StopReason)>,
    verified_side: &str,
    summary: Option<&Value>,
) -> PathBuf {
    let barrier_at = summary.and_then(barrier_timestamp);
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
    for (index, acked_i) in acked.iter().enumerate() {
        let minor = minor_of(index);
        let verdict_source = verify_against(&rig.world_a, minor, acked_i, WRITER_ID);
        let verdict_peer = verify_against(&rig.world_b, minor, acked_i, WRITER_ID);
        let (verified, tail) = if verified_side == "destination" {
            (verdict_peer.present, verdict_source.tail())
        } else {
            (verdict_source.present, verdict_peer.tail())
        };
        let skew = barrier_at.and_then(|at| boundary_skew(acked_i, at));
        evidence.oracle(oracle_value(
            acked_i.len() as u64,
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
    }
    emit_rig(rig, evidence)
}

/// The full scenario opening (a live writer and a steady-state
/// transport — the cut-crossing shape).
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

/// The multi-volume scenario opening (one writer and one transport
/// per participant).
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
    let writers = (0..count)
        .map(|i| {
            WriterHandle::start(
                &rig.world_a,
                &rig.vmm_a,
                &rig.vm,
                minor_of(i),
                &rig.stamp_clock,
            )
        })
        .collect();
    let transports = (0..count)
        .map(|i| spawn_peer_transport(&rig.world_a, minor_of(i), TRANSPORT_LAG))
        .collect();
    tokio::time::sleep(WRITER_WARMUP).await;
    (rig, writers, transports)
}

/// One participant's volume id (the rig assigns volume names in
/// participant order).
fn vol_id(rig: &Rig, index: usize) -> VolumeId {
    VolumeId::new(rig.volumes[index].as_str()).expect("valid volume id")
}

/// One participant's device minor (the rig assigns minors in attach
/// order from [`SEED_MINOR`]; the bounded cast is the rig's own
/// `try_into` idiom).
fn minor_of(index: usize) -> u32 {
    let index: u32 = index.try_into().expect("volume index fits u32");
    SEED_MINOR + index
}

/// The prepare act (the stage-A/B shape; 201) — works for single-
/// and multi-volume rigs alike.
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

/// Bounded wait for a fired kill.
async fn await_kill(killed: impl Fn() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + POLL_BOUND;
    while !killed() {
        assert!(
            std::time::Instant::now() < deadline,
            "the {what} kill never fired"
        );
        tokio::time::sleep(POLL_STEP).await;
    }
}

/// The parked record's current state name (one observation — the
/// spin's re-check after the watch window).
async fn parked_state(rig: &Rig, mig: &str) -> String {
    let (status, body) = get_migration(rig.a.addr, mig)
        .await
        .served("observe the parked record");
    assert_eq!(status, 200, "observe the parked record: {body}");
    state_name(&body_json(&body))
}

/// Read the destination's durable preparation record through a
/// FRESH store instance over the same directory: a killed launch's
/// in-process store lock is poisoned by design (the crash injection
/// runs inside the store's lock), and the durable files are the
/// honest view a fresh process would load — exactly the view the
/// crash-recovery assertions need.
fn load_preparation(rig: &Rig, mig: &str) -> Option<TargetPreparation> {
    let store = TargetPreparationStore::open(peer_preparations_dir(&rig.b.core.journal_dir))
        .expect("open the destination's preparation store");
    store
        .load(&MigrationId::new(mig).expect("valid id"))
        .expect("load the preparation")
}

/// Open a journal read-only, absorbing the asynchronous flock free
/// after a stop (the same race the rig's own `open_journal` absorbs
/// for the daemons' launches: the aborted tasks' `Arc` drops land on
/// the runtime after the stop returns).
async fn journal_records(dir: &std::path::Path) -> Vec<JournalRecord> {
    let deadline = std::time::Instant::now() + POLL_BOUND;
    loop {
        match Journal::open_with_records(dir) {
            Ok((_journal, records)) => return records,
            // A compacted control-plane journal cannot yield the complete
            // history this forensic helper counts. Fail fast with the real
            // reason instead of polling the flock deadline.
            Err(error) if error.detail.contains("compacted") => {
                panic!(
                    "the campaign's control-plane journal at {} compacted; the \
                     full-history record counter cannot derive its count from a \
                     snapshot: {error}",
                    dir.display()
                );
            }
            Err(error) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the journal flock at {} did not free within {POLL_BOUND:?}: {error}",
                    dir.display()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

/// Stop the destination and count the grant act's journaled
/// failures: every retry tick's re-drive that reached the act
/// journaled its failure under the route's deterministic
/// peer-grant operation id — the spin's proof it is alive and
/// refusing, never sitting silently.
async fn count_peer_grant_failures(rig: &mut Rig) -> usize {
    rig.b.stop().await;
    let records = journal_records(&rig.b.core.journal_dir).await;
    records
        .iter()
        .filter(|record| {
            matches!(record, JournalRecord::Outcome(outcome) if !outcome.success
                && outcome.operation_id.as_str().starts_with("derived:mig-api-peer-grant-"))
        })
        .count()
}

/// The park every horizon scenario opens from: the witness dies
/// inside the destination's grant commit (the intent is durable —
/// the restart's roll-forward lands the grant), B's route journals
/// its failure over the dead connection, and the drive parks past
/// the cut. This is the healed grant_set wedge's cell shape; the
/// horizon scenarios then diverge by what happens to the LEASE.
async fn wedge_park(rig: &Rig, mig: &str) {
    rig.witness
        .crash
        .arm_witness("grant_set", StoreSavePoint::WitnessAfterIntentAppend);
    let (status, body) = transfer(rig, mig).await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");
    await_kill(|| rig.witness.is_killed(), "the witness grant-set commit").await;
}

/// Bounded wait for one participant's witness view to reach its full
/// expected shape (epoch, holder, lease state) — the row-12b
/// observation-race discipline: the poll predicate is the whole
/// expected view, so the strength is the single-shot assert's.
async fn await_view(rig: &Rig, volume: &VolumeId, epoch: u64, holder: &str, live: bool) {
    let expected = if live {
        LeaseState::Live
    } else {
        LeaseState::Expired
    };
    let deadline = std::time::Instant::now() + POLL_BOUND;
    loop {
        let view = witness_view(&rig.witness, volume).await;
        if view.current_epoch.0 == epoch
            && view.holder.as_ref().map(HostId::as_str) == Some(holder)
            && view.lease_state == expected
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the witness view of {volume} never reached epoch {epoch} held by {holder} \
             with a {expected:?} lease within {POLL_BOUND:?}; last observed: epoch {} \
             held by {:?} with a {:?} lease",
            view.current_epoch.0,
            view.holder.as_ref().map(HostId::as_str),
            view.lease_state,
        );
        tokio::time::sleep(POLL_STEP).await;
    }
}

/// Watch a parked record's spin: several retry ticks pass (the
/// retry task re-drives every ~5 s), the record stays at its parked
/// state, and every participant's epoch stays where the park left
/// it — no re-mint ever fired over the park.
async fn assert_spin_holds(rig: &Rig, mig: &str, parked: &str, epoch: u64, count: usize) {
    tokio::time::sleep(SPIN_WATCH).await;
    let observed = parked_state(rig, mig).await;
    assert_eq!(observed, parked, "the park keeps its honest spin");
    for index in 0..count {
        let view = witness_view(&rig.witness, &vol_id(rig, index)).await;
        assert_eq!(
            view.current_epoch.0, epoch,
            "volume {index}: no re-mint over the park"
        );
    }
}

/// Join a multi-volume scenario's writers and verify every
/// participant's acknowledged prefix at the destination (the drain
/// completed for every participant before the cut — what parks is
/// the promotion, never the replication).
async fn verify_parked_data_n(
    rig: &Rig,
    writers: Vec<WriterHandle>,
) -> (Vec<Vec<AckedWrite>>, Option<(u64, StopReason)>) {
    let mut joined = Vec::new();
    for writer in writers {
        joined.push(writer.join().await);
    }
    let acked: Vec<Vec<AckedWrite>> = joined.iter().map(|(a, _)| a.clone()).collect();
    for (index, acked_i) in acked.iter().enumerate() {
        let verdict = verify_against(&rig.world_b, minor_of(index), acked_i, WRITER_ID);
        assert!(
            verdict.prefix_intact(),
            "volume {index}: the acknowledged prefix is at the destination: {verdict:?}"
        );
    }
    let boundary = joined[0].1.clone();
    (acked, boundary)
}

/// The composite's journal shape: exactly one re-mint attempt
/// spent, the witness journal holding exactly one grant-set intent
/// under the attempt-1 derived id (the re-mint fired once — over
/// the uniformly-dead park, never over the live member).
async fn assert_single_re_mint_shape(rig: &mut Rig, mig: &str) {
    let preparation = rig
        .b
        .peer_preparations
        .load(&MigrationId::new(mig).expect("valid id"))
        .expect("load the preparation")
        .expect("the preparation is on record");
    assert_eq!(
        preparation.grant_attempt, 1,
        "exactly one re-mint attempt was spent"
    );
    rig.witness.stop().await;
    let records = journal_records(&rig.witness.dir).await;
    let reissue_ids: Vec<&str> = records
        .iter()
        .filter_map(|record| match record {
            JournalRecord::Intent(intent) => intent
                .operation_id
                .as_str()
                .starts_with("derived:mig-grant-set-r")
                .then_some(intent.operation_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        reissue_ids.len(),
        1,
        "exactly one re-mint intent reached the witness: {reissue_ids:?}"
    );
    assert!(
        reissue_ids[0].starts_with("derived:mig-grant-set-r1-"),
        "the re-mint fired under the attempt-1 id: {reissue_ids:?}"
    );
    rig.witness.restart().await;
}

/// The divergence and the coupling observation (the mixed-liveness
/// park's middle): renew ONLY the first participant's lease under
/// the destination's credential (the partial renewal's shape — one
/// member's end moves past the other's), cross the second member's
/// original end, then verify the destination's grant-hold renewal
/// does NOT re-renew the survivor — the preparation is not
/// uniformly live, and renewing the survivor would block the
/// fresh-grant gate forever. Several renewal ticks pass; the
/// survivor's remaining duration keeps falling with the clock (a
/// renewal would have reset it to the full TTL).
async fn assert_uniform_liveness_coupling(rig: &Rig) {
    // The divergence: advance the clock, then renew the first
    // participant's lease under the destination's credential.
    rig.clock.fetch_add(CLOCK_STEP, Ordering::SeqCst);
    let view = witness_view(&rig.witness, &vol_id(rig, 0)).await;
    let lease = view.lease_id.expect("the live lease's id");
    rig.witness
        .host_client(PEER_NODE)
        .renew(
            &vol_id(rig, 0),
            RenewRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: OperationId::new("manual-divergence-renew-1")
                    .expect("valid operation id"),
                host_id: HostId::new(PEER_NODE).expect("valid host id"),
                epoch: view.current_epoch,
                lease_id: lease,
            },
        )
        .await
        .expect("the manual renewal lands");
    // The clock crosses the second member's ORIGINAL end (START+TTL,
    // the batch's shared mint) but stays well inside the renewed
    // member's window: the sibling dies, the survivor lives on its
    // renewed end.
    let original_end = volvisor_campaign::rig::START + TTL;
    let now = rig.clock.load(Ordering::SeqCst);
    rig.clock
        .fetch_add(original_end + 1 - now, Ordering::SeqCst);
    await_view(rig, &vol_id(rig, 1), 2, PEER_NODE, false).await;
    await_view(rig, &vol_id(rig, 0), 2, PEER_NODE, true).await;

    // THE COUPLING, OBSERVED: several renewal ticks pass while the
    // preparation is not uniformly live; the survivor's remaining
    // duration keeps falling with the clock.
    tokio::time::sleep(COUPLING_WATCH).await;
    let before = witness_view(&rig.witness, &vol_id(rig, 0)).await;
    rig.clock.fetch_add(5, Ordering::SeqCst);
    let after = witness_view(&rig.witness, &vol_id(rig, 0)).await;
    assert_eq!(after.lease_state, LeaseState::Live);
    let remaining = after.lease_remaining_secs.unwrap_or(0);
    assert!(
        remaining > 0 && remaining <= before.lease_remaining_secs.unwrap_or(0),
        "the survivor was NOT re-renewed by the grant-hold pass (its remaining fell \
         from {:?} to {remaining} with the clock — a renewal would have reset it to \
         the full TTL)",
        before.lease_remaining_secs
    );
}

/// The at-promote park's exact participant shape (the gate-b
/// negative's world): the first participant's promote completed (its
/// device path durably recorded — the gate's input) but its
/// resource was SELF-FENCED by the destination's restart
/// reconciliation — every Primary found at startup is unproven
/// until its lease validates, and the outage killed the lease
/// (fail-closed: the writer is fenced, never silently resumed). The
/// others never promoted, and the source stays fenced.
fn assert_at_promote_park_shape(rig: &Rig) {
    assert_eq!(
        role_of(&rig.world_b, &rig.resources()[0]),
        Role::Secondary,
        "the promoted participant was self-fenced by the destination's restart \
         reconciliation (its lease died in the outage — an unprovable writer is \
         fenced fail-closed, never silently resumed)"
    );
    assert_eq!(
        role_of(&rig.world_b, &rig.resources()[1]),
        Role::Secondary,
        "the un-promoted participants never promote"
    );
    assert_eq!(
        role_of(&rig.world_b, &rig.resources()[2]),
        Role::Secondary,
        "the un-promoted participants never promote"
    );
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Absent,
        "the source VM is destroyed"
    );
    for index in 0..3 {
        assert_eq!(
            role_of(&rig.world_a, &rig.resources()[index]),
            Role::Secondary,
            "volume {index}: the source stays demoted (fenced)"
        );
    }
}

/// The W1–W5 check for one volume (the witness side): the authority
/// view's epoch, holder and lease state are exactly as the recovery
/// implies — the fresh-grant rows assert epoch 3 (the re-minted
/// grant; a double mint would show as 4), the renewal row epoch 2
/// (the renewed grant; a re-mint would show as 3).
async fn assert_w1_w5_vol(
    rig: &Rig,
    volume: &VolumeId,
    epoch: u64,
    holder: &str,
    live: bool,
) -> String {
    await_view(rig, volume, epoch, holder, live).await;
    format!("pass: epoch {epoch} at {holder}, lease live={live} (W1-W5)")
}

/// The G5 check for COMPLETE paths over one volume (rule 5): the
/// migration's barrier is recorded at the witness, NOT voided, with
/// every attested fact true — and the source was never resumed over
/// it.
async fn assert_g5_complete(rig: &Rig, volume: &VolumeId, mig: &str) -> String {
    let view = witness_view(&rig.witness, volume).await;
    let migration = MigrationId::new(mig).expect("valid migration id");
    let matching: Vec<_> = view
        .barriers
        .iter()
        .filter(|barrier| barrier.migration_id.as_ref() == Some(&migration))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "exactly one barrier for {mig}: {:?}",
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
    "pass: barrier non-voided and all-true".to_owned()
}

/// The shared safety set over a CONVERGED single-volume horizon
/// scenario: the source stays destroyed and demoted, the destination
/// promoted, the record complete with the healed path ordered in its
/// history, and the data-path oracle verified at the destination.
async fn assert_converged(
    rig: &Rig,
    mig: &str,
    epoch: u64,
    writer: WriterHandle,
    mut evidence: Evidence,
    verified_detail: &str,
) -> PathBuf {
    let summary = poll_migration(rig.a.addr, mig, "complete").await;
    let history: Vec<&str> = summary["state_history"]
        .as_array()
        .expect("the summary carries the state history")
        .iter()
        .map(|entry| entry["state"].as_str().expect("a state name"))
        .collect();
    let authorized = history
        .iter()
        .position(|state| *state == "destination_authorized")
        .expect("the fold landed destination_authorized");
    let resumed = history
        .iter()
        .position(|state| *state == "vm_resumed")
        .expect("the promote drove the restore and resume");
    assert!(
        authorized < resumed,
        "the recovered path is ordered: {history:?}"
    );
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("the source VM observes"),
        VmState::Absent,
        "the source VM stays destroyed (the cut is durable)"
    );
    assert_eq!(
        role_of(&rig.world_a, &rig.resource()),
        Role::Secondary,
        "the source stays demoted (fenced, never resumed)"
    );
    assert_eq!(
        role_of(&rig.world_b, &rig.resource()),
        Role::Primary,
        "the destination promoted through the recovered path"
    );
    evidence.invariant(
        "w1_w5_no_double_grant",
        &assert_w1_w5_vol(rig, &rig.volume_id(), epoch, PEER_NODE, true).await,
    );
    evidence.invariant(
        "g5_barrier_all_true",
        &assert_g5_complete(rig, &rig.volume_id(), mig).await,
    );
    let (acked, boundary) = writer.join().await;
    assert!(!acked.is_empty(), "the writer acknowledged writes");
    let destination = verify_against(&rig.world_b, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        destination.prefix_intact(),
        "the destination keeps every acknowledged write: {destination:?}"
    );
    evidence.outcome(verified_detail);
    emit_oracle(
        rig,
        evidence,
        &acked,
        boundary,
        "destination",
        Some(&summary),
    )
}

// ------------------------------------------------------------- the rows

/// **Mechanism 1, the convergence:** a grant_set-wedge park (the
/// witness died inside the grant commit; the witness's restart lands
/// the lease) with the source held down while the park crosses the
/// lease's original end by half again the TTL. The destination's
/// grant-hold renewal keeps the lease live through the whole park
/// (the stepped clock advance enters the due window while the lease
/// still has life — one deliberate jump would skip it), and when the
/// source returns its startup pass folds the live grant and the
/// migration converges through the ordinary re-issue — the epoch is
/// EXACTLY 2 (the renewal extended the recorded grant; the re-mint
/// never fired, and never needed to).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn park_past_ttl_with_the_witness_alive_renews_and_converges() {
    let seq = next_id();
    let (mut rig, writer, _transport) = live_scenario(&format!("whr{seq}")).await;
    let mut evidence = Evidence::new("wedge-horizon/park-past-ttl/witness-alive-renews");
    evidence.fault(
        "witness_kill_inside_grant_commit",
        "grant_set at WitnessAfterIntentAppend (the park), then the source held down while \
         the frozen clock crosses the lease's original end by half again the TTL",
    );
    prepare(&rig, "mig-x").await;
    wedge_park(&rig, "mig-x").await;

    // The witness's restart lands the grant (the roll-forward
    // completes the journaled intent): epoch 2, live at the
    // destination, its end the original mint (clock + TTL).
    rig.witness.restart().await;
    await_view(&rig, &rig.volume_id(), 2, PEER_NODE, true).await;
    // The source dies: the park holds with no re-drive while the
    // lease's whole original TTL passes.
    rig.a.stop().await;

    // THE PARK PAST THE TTL: three stepped advances (85 witness
    // seconds each — after each renewal the next due window opens
    // TTL−INTERVAL on, so every step lands inside it with 15 s of
    // life left, still live and due), each observed by waiting for
    // the renewal it makes due (a poll, never a fixed settle: the
    // rig's 1 s renewal tick is the thing being waited for, and a
    // loaded box stretches it). The original end is START+TTL; the
    // park ends 255 s past START — 155 s past the original end, two
    // and a half TTLs deep. Every step observes the lease still live
    // at epoch 2 for the destination: the grant-hold renewal did it.
    let original_end = volvisor_campaign::rig::START + TTL;
    for step in 0..3 {
        rig.clock.fetch_add(CLOCK_STEP, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + POLL_BOUND;
        loop {
            let view = witness_view(&rig.witness, &rig.volume_id()).await;
            assert_eq!(
                view.current_epoch.0, 2,
                "the renewed grant's epoch never moves (W1-W5): {:?}",
                view.lease_state
            );
            assert_eq!(
                view.holder.as_ref().map(HostId::as_str),
                Some(PEER_NODE),
                "the renewed lease stays the destination's grant hold"
            );
            assert_eq!(
                view.lease_state,
                LeaseState::Live,
                "the grant-hold renewal kept the lease live (the park is \
                 {} s past its original end {})",
                rig.clock.load(Ordering::SeqCst) - volvisor_campaign::rig::START,
                original_end
            );
            let remaining = view.lease_remaining_secs.unwrap_or(0);
            if remaining > INTERVAL {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the grant-hold renewal never fired for step {step} \
                 (remaining {remaining}s at {}s past the original end) — the \
                 lease cannot live past its original end without it",
                rig.clock.load(Ordering::SeqCst) - original_end
            );
            tokio::time::sleep(POLL_STEP).await;
        }
    }
    assert!(
        rig.clock.load(Ordering::SeqCst) > original_end + 100,
        "the park crossed the original end by a wide margin"
    );

    // The renewals are journaled at the witness under the
    // grant-hold operation ids — counted by reading the journal
    // read-only (the witness is stopped for the read and restarted
    // after: the journal is fully durable, the restart re-serves
    // the same state; the source's return needs the witness alive).
    rig.witness.stop().await;
    let renewals = {
        let records = journal_records(&rig.witness.dir).await;
        records
            .iter()
            .filter(|record| {
                matches!(record, JournalRecord::Intent(intent) if intent
                    .operation_id
                    .as_str()
                    .starts_with("grant-hold-renew-"))
            })
            .count()
    };
    assert!(
        renewals >= 3,
        "the grant-hold renewal fired at least three times over the park \
         (observed {renewals}) — the lease could not have lived past its \
         original end otherwise"
    );
    rig.witness.restart().await;

    // The source returns: the startup pass re-drives, folds the live
    // grant (the authorize step's target-granted check) and the
    // promote re-issue replays the recorded grant under its original
    // id — the epoch stays EXACTLY 2, the re-mint never fired.
    rig.a.restart().await;
    evidence.invariant(
        "grant_hold_renewal_fired",
        &format!(
            "pass: the grant-hold renewal fired {renewals} times over the park \
             (the witness journal's grant-hold-renew intents), keeping the lease \
             live {} s past its original end",
            rig.clock.load(Ordering::SeqCst) - original_end
        ),
    );
    let record = assert_converged(
        &rig,
        "mig-x",
        2,
        writer,
        evidence,
        "recovered: COMPLETE through the renewed grant — the grant-hold renewal kept the \
         destination's lease live across a park two and a half TTLs deep (the source held \
         down, the witness alive), and the source's return converged through the \
         ordinary re-issue; the epoch is exactly 2 (the recorded grant was renewed, \
         never re-minted), the acknowledged prefix byte-exact at the destination",
    )
    .await;
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

/// **Mechanism 2, the convergence:** the same wedge park, but the
/// witness stays DOWN while the frozen clock crosses the lease's
/// end and the whole fence window (one deliberate jump — the due
/// window is never entered, so nothing renews). When the witness
/// returns, the roll-forward lands the grant BORN DEAD (its end was
/// fixed at the mint), and the next re-drive's grant act sees the
/// witness-authoritative shape — every participant's lease Expired,
/// still held by this host, the park provably pre-promote, the cut's
/// fence evidence intact — and re-issues under the attempt-1 derived
/// id. The re-minted grant (epoch exactly 3) drives the promotes and
/// the migration completes: the wedge's convergence horizon closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn witness_outage_past_ttl_the_fresh_grant_converges() {
    let seq = next_id();
    let (mut rig, writer, _transport) = live_scenario(&format!("whf{seq}")).await;
    let mut evidence = Evidence::new("wedge-horizon/witness-outage-past-ttl/fresh-grant");
    evidence.fault(
        "witness_outage_past_the_lease_ttl",
        "the witness stays down (killed inside the grant commit) while the frozen clock \
         crosses the lease end and the fence window; the grant lands born-dead on the \
         witness's return",
    );
    prepare(&rig, "mig-x").await;
    wedge_park(&rig, "mig-x").await;

    // The outage: the witness stays down while the lease's end and
    // the fence window pass (the source's re-drives fail at the
    // unreachable witness — the park holds). The jump is deliberate:
    // the renewal's due window is never entered, so no renewal fires.
    rig.clock.fetch_add(TTL + 200, Ordering::SeqCst);
    // The witness returns: the roll-forward lands the grant with its
    // ORIGINAL end — the lease is born dead.
    rig.witness.restart().await;
    await_view(&rig, &rig.volume_id(), 2, PEER_NODE, false).await;

    // The next re-drive (the retry task, ~5 s cadence): the authorize
    // step sees the dead lease, the grant route re-issues, the act's
    // gates pass, the attempt counter is journaled (r1), and the
    // fresh grant mints epoch 3. The convergence follows through the
    // promotes, the restore and the resume.
    let record = assert_converged(
        &rig,
        "mig-x",
        3,
        writer,
        evidence,
        "recovered: COMPLETE through the gated fresh-grant re-mint — the witness's \
         outage outlived the lease (born dead on the witness's return), the re-drive's \
         grant act proved the uniformly-dead pre-promote park with its fence evidence \
         intact, journaled attempt 1, and re-issued under the reserved-namespace derived \
         id; the re-minted grant drove the promotes (epoch exactly 3, never a fourth), \
         the acknowledged prefix byte-exact at the destination",
    )
    .await;
    assert!(record.is_file(), "the evidence record landed: {record:?}");

    // The horizon remedy's journal shape: the attempt counter is 1
    // on the preparation (durable before the witness call), and the
    // witness journal holds exactly one grant-set intent under the
    // attempt-1 derived id (the original bare id's intent is the
    // outage's dead grant).
    let preparation = rig
        .b
        .peer_preparations
        .load(&MigrationId::new("mig-x").expect("valid id"))
        .expect("load the preparation")
        .expect("the preparation is on record");
    assert_eq!(preparation.grant_attempt, 1);
}

/// **The fresh grant's own durable boundary, part 1:** the fresh
/// grant's witness commit is killed after its intent append (the
/// same seam, re-armed — the roll-forward will land it). The
/// re-drive re-derives the SAME attempt-1 id (the counter was
/// journaled before the witness call), so the witness journal
/// REPLAYS the landed grant instead of minting a second one — the
/// epoch is exactly 3, never 4 — and the promotes complete under
/// the replayed grant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_grant_survives_a_kill_inside_its_own_commit() {
    let seq = next_id();
    let (mut rig, writer, _transport) = live_scenario(&format!("whk{seq}")).await;
    let mut evidence = Evidence::new("wedge-horizon/fresh-grant-commit-kill/same-attempt-replay");
    evidence.fault(
        "witness_kill_inside_the_fresh_grant_commit",
        "the witness dies again at the attempt-1 grant_set's intent append — the fresh \
         grant's own durable boundary",
    );
    prepare(&rig, "mig-x").await;
    wedge_park(&rig, "mig-x").await;

    // The source is held down for the whole boundary sequence (the
    // park holds with no re-drive; each re-drive below is fired
    // deliberately through the source's restart sweep — the healed
    // cells' precedent: the startup sweep is the same production
    // retry_pass the 5 s tick runs, so a scenario whose subject is
    // the BOUNDARY need not also wait out the CADENCE).
    rig.a.stop().await;

    // The outage past the TTL and the fence window; the witness
    // returns and lands the original grant born dead.
    rig.clock.fetch_add(TTL + 200, Ordering::SeqCst);
    rig.witness.restart().await;
    await_view(&rig, &rig.volume_id(), 2, PEER_NODE, false).await;

    // Re-arm the witness kill for the FRESH grant's own commit (the
    // seam is shared across witness restarts, one shot per arming),
    // then fire the re-drive: the gates pass, attempt 1 is journaled,
    // the fresh grant's commit begins — and the witness dies inside
    // it.
    rig.witness
        .crash
        .arm_witness("grant_set", StoreSavePoint::WitnessAfterIntentAppend);
    rig.a.restart().await;
    await_kill(|| rig.witness.is_killed(), "the fresh grant's commit").await;
    // The witness returns: the roll-forward LANDS the fresh grant
    // (its end is the re-mint clock + TTL — live).
    rig.witness.restart().await;
    await_view(&rig, &rig.volume_id(), 3, PEER_NODE, true).await;

    // The re-drive (fired the same way): the authorize step folds the
    // live re-minted grant, the promote re-issue re-executes the
    // grant act, which re-derives the SAME attempt-1 id — the
    // witness replays the landed grant (never a second mint: the
    // epoch stays exactly 3) — and the promotes complete under it.
    rig.a.restart().await;
    let record = assert_converged(
        &rig,
        "mig-x",
        3,
        writer,
        evidence,
        "recovered: COMPLETE through the fresh grant's own crash window — the witness \
         died inside the attempt-1 commit, the roll-forward landed the re-minted grant, \
         and the re-drive re-derived the same attempt-1 id so the witness journal \
         replayed the landed grant (epoch exactly 3, never a second mint); the \
         acknowledged prefix byte-exact at the destination",
    )
    .await;
    assert!(record.is_file(), "the evidence record landed: {record:?}");

    // The counter was spent once: the re-drive's replay took the
    // serviceable path, not a second re-mint.
    let preparation = rig
        .b
        .peer_preparations
        .load(&MigrationId::new("mig-x").expect("valid id"))
        .expect("load the preparation")
        .expect("the preparation is on record");
    assert_eq!(
        preparation.grant_attempt, 1,
        "the kill inside the commit spent exactly one attempt"
    );
}

/// **The fresh grant's own durable boundary, part 2:** the attempt
/// counter's bump is killed after its rename (the preparation
/// store's own §3.1 seam — the bump IS durable, the daemon dies
/// before the witness call). The re-drive re-reads the durable
/// counter, bumps AGAIN (attempt 2), and issues under the r2 id —
/// the witness journal holds r2, never r1: a crashed bump's
/// half-spent attempt is never re-derived as its own successor's
/// id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_grant_attempt_counter_survives_a_kill_at_the_bump() {
    let seq = next_id();
    let (mut rig, writer, _transport) = live_scenario(&format!("whb{seq}")).await;
    let mut evidence = Evidence::new("wedge-horizon/bump-kill/attempt-counter-durable");
    evidence.fault(
        "daemon_kill_at_the_attempt_counter_bump",
        "the destination dies at the preparation store's AfterRename (the bump is \
         durable, the witness call never happens)",
    );
    prepare(&rig, "mig-x").await;

    // The wedge park, then the outage past the TTL and fence window
    // (the source held down — the re-drives below are fired
    // deliberately through its restart sweep, the healed cells'
    // precedent).
    wedge_park(&rig, "mig-x").await;
    rig.a.stop().await;
    rig.clock.fetch_add(TTL + 200, Ordering::SeqCst);
    rig.witness.restart().await;
    await_view(&rig, &rig.volume_id(), 2, PEER_NODE, false).await;

    // Arm the destination's preparation-store seam at the rename:
    // the next preparation save is the attempt-counter bump (the
    // promote loop's device-path recordings only follow a landed
    // grant). The kill lands after the rename — the bump is durable.
    rig.b
        .peer_preparations
        .store_crash_hooks()
        .arm(STORE_PEER_PREPARATION, StoreSavePoint::AfterRename);
    // Fire the re-drive: the gates pass, the bump saves — and the
    // destination dies over its own durable counter.
    rig.a.restart().await;
    await_kill(|| rig.b.is_killed(), "the attempt-counter bump").await;
    let preparation = load_preparation(&rig, "mig-x").expect("the preparation is on record");
    assert_eq!(
        preparation.grant_attempt, 1,
        "the killed bump is durable (the counter reads 1 after the crash)"
    );

    // The destination returns; the re-drive (fired the same way)
    // re-reads the durable counter, bumps to 2, and issues under the
    // r2 id.
    rig.b.restart().await;
    rig.a.restart().await;
    let record = assert_converged(
        &rig,
        "mig-x",
        3,
        writer,
        evidence,
        "recovered: COMPLETE through the bump's own crash window — the destination died \
         at the attempt-counter's rename (the bump durable, the witness call never \
         made), and the re-drive burned the NEXT attempt (r2, journaled before its \
         witness call) rather than re-deriving the crashed bump's id; the epoch is \
         exactly 3, the acknowledged prefix byte-exact at the destination",
    )
    .await;
    assert!(record.is_file(), "the evidence record landed: {record:?}");

    // The journal shape: the witness holds the r2 grant-set intent,
    // never r1 (the crashed bump's witness call never happened).
    let preparation = rig
        .b
        .peer_preparations
        .load(&MigrationId::new("mig-x").expect("valid id"))
        .expect("load the preparation")
        .expect("the preparation is on record");
    assert_eq!(preparation.grant_attempt, 2);
    rig.witness.stop().await;
    let records = journal_records(&rig.witness.dir).await;
    let reissue_ids: Vec<&str> = records
        .iter()
        .filter_map(|record| match record {
            JournalRecord::Intent(intent) => intent
                .operation_id
                .as_str()
                .starts_with("derived:mig-grant-set-r")
                .then_some(intent.operation_id.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        reissue_ids
            .iter()
            .any(|id| id.starts_with("derived:mig-grant-set-r2-")),
        "the witness journal holds the r2 grant-set intent: {reissue_ids:?}"
    );
    assert!(
        !reissue_ids
            .iter()
            .any(|id| id.starts_with("derived:mig-grant-set-r1-")),
        "the crashed bump's r1 id never reached the witness: {reissue_ids:?}"
    );
}

/// **Gate (b), the negative:** a three-volume park where the
/// destination died mid-promote-loop (the first participant's device
/// path durably recorded, the other two never promoted — the
/// preparation store's own seam), then the witness outage killed
/// every lease. The re-mint is REFUSED typed: the record is at
/// promote (a re-mint would retire the epoch a completed promote's
/// authority block is anchored to). The park keeps its honest spin —
/// the refusal reproduces on every retry tick, the epochs never
/// move, nothing re-mints — operator territory, never weakened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_grant_refused_at_promote_parks_honestly() {
    let seq = next_id();
    let (mut rig, writers, _transports) = live_scenario_n(&format!("whp{seq}"), 3).await;
    let mut evidence = Evidence::new("wedge-horizon/at-promote-park/refused-typed");
    evidence.fault(
        "daemon_kill_mid_promote_loop_then_witness_outage_past_ttl",
        "the destination dies at the first participant's device-path recording \
         (AfterRename — the promote is durable), then the witness outage kills every \
         lease; the re-mint is refused at the promote gate",
    );
    prepare(&rig, "mig-x").await;

    // Arm the destination's preparation seam AFTER prepare: the next
    // preparation save is the promote loop's first device-path
    // recording (the grant act's only earlier store mutation is the
    // attempt bump — not reached on the first, never-killed grant).
    rig.b
        .peer_preparations
        .store_crash_hooks()
        .arm(STORE_PEER_PREPARATION, StoreSavePoint::AfterRename);
    // The drive: the cut lands set-wide, the grant mints epoch 2 for
    // every volume, the promote loop promotes the first participant
    // — and the destination dies over its durable device path.
    let (status, body) = transfer(&rig, "mig-x").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");
    await_kill(|| rig.b.is_killed(), "the promote loop's path recording").await;

    // The mixed-path park is durable: the first participant promoted,
    // the others never reached.
    let preparation = load_preparation(&rig, "mig-x").expect("the preparation is on record");
    assert!(preparation.participants[0].device_path.is_some());
    assert!(preparation.participants[1].device_path.is_none());
    assert!(preparation.participants[2].device_path.is_none());

    // The witness outage kills every lease (the destination is dead;
    // nothing renews), past every fence window.
    rig.witness.stop().await;
    rig.clock.fetch_add(TTL + 200, Ordering::SeqCst);
    rig.witness.restart().await;
    rig.b.restart().await;
    for index in 0..3 {
        await_view(&rig, &vol_id(&rig, index), 2, PEER_NODE, false).await;
    }

    // Fire the first re-drive now through the source's restart sweep
    // (the same production retry_pass its 5 s tick runs — the healed
    // cells' precedent): the park observes IN_DOUBT and never
    // completes.
    rig.a.restart().await;
    let summary = poll_migration(rig.a.addr, "mig-x", "in_doubt").await;
    // THE SPIN: the retry ticks reproduce the refusal (one full tick
    // plus headroom — the sweep's refusal is already on record); the
    // epochs never move (no re-mint), the dead leases stay dead.
    assert_spin_holds(&rig, "mig-x", "in_doubt", 2, 3).await;
    for index in 0..3 {
        let view = witness_view(&rig.witness, &vol_id(&rig, index)).await;
        assert_eq!(
            view.lease_state,
            LeaseState::Expired,
            "volume {index}: the dead leases stay dead"
        );
    }
    // The exact participant shape holds (the helper below): the
    // promote's record stands, the world was fenced fail-closed,
    // and the gate-b refusal stands on the RECORD's shape
    // (at-promote), which the fence does not erase.
    assert_at_promote_park_shape(&rig);

    // The spin is ALIVE, not silent: every retry tick's re-drive
    // re-executed the grant act and journaled the typed refusal (the
    // destination's route records the act's failure under its
    // deterministic peer-grant operation id) — counted read-only
    // after the observation (the destination is stopped for the
    // read; the evidence capture needs no server).
    let refusals = count_peer_grant_failures(&mut rig).await;
    assert!(
        refusals >= 2,
        "the re-drive's typed refusals are on record at the destination \
         (observed {refusals} over the watch window) — the park refuses \
         on every retry tick, it does not sit silently"
    );

    // The data is nevertheless all there (the drain completed for
    // every participant before the cut — what parks is the
    // promotion, not the replication).
    let (acked, boundary) = verify_parked_data_n(&rig, writers).await;

    evidence.invariant(
        "gate_b_refusal",
        "pass: the re-mint is refused typed at the promote gate (the record is at \
         promote — a re-mint would retire the epoch a completed promote's authority is \
         anchored to); the refusal reproduces on every retry tick, the epochs stay 2, \
         nothing re-mints",
    );
    evidence.invariant(
        "exact_participant_state",
        "pass: the first participant's promote completed (its device path durably \
         recorded — the gate's input) but its resource was self-fenced by the \
         destination's restart reconciliation (the outage killed its lease; an \
         unprovable writer is fenced fail-closed) / Secondary / Secondary; every \
         lease expired; the source fully fenced",
    );
    evidence.outcome(
        "parked safe (IN_DOUBT): the at-promote park is the re-mint's honest boundary — \
         the un-promoted tail's completion and the promoted head's authority cannot be \
         finished by a fresh grant without fencing a legitimate promote; the spin is \
         bounded and typed, recovery is operator territory (never weakened to make this \
         scenario pass)",
    );
    let record = emit_oracle_n(
        &rig,
        evidence,
        &acked,
        boundary,
        "destination",
        Some(&summary),
    );
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

/// **The still-live boundary, the negative:** a two-volume park
/// where one member's lease was renewed past the other's death (the
/// manual divergence — a partial renewal's shape). The re-mint never
/// fires over the live member (the uniform gate): the re-drive takes
/// the ordinary path, the live member's promote lands, the dead
/// member's promote refuses, and the park spins — the surviving
/// member is deliberately allowed to die only when nothing promotes
/// it first; once its promote lands, the park is the at-promote
/// shape (operator territory). The uniform-liveness coupling is
/// observed directly: the grant-hold renewal does NOT re-renew the
/// survivor while its sibling is dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_grant_never_mints_over_a_live_member() {
    let seq = next_id();
    let (mut rig, writers, _transports) = live_scenario_n(&format!("whm{seq}"), 2).await;
    let mut evidence = Evidence::new("wedge-horizon/mixed-liveness/never-mints-over-live");
    evidence.fault(
        "partial_renewal_divergence",
        "one participant's lease renewed past its sibling's death (the manual \
         divergence a partial renewal leaves); the re-mint never fires over the live \
         member",
    );
    prepare(&rig, "mig-x").await;
    wedge_park(&rig, "mig-x").await;

    // The witness's restart lands both grants (epoch 2 each, their
    // ends identical — the batch minted them together).
    rig.witness.restart().await;
    await_view(&rig, &vol_id(&rig, 0), 2, PEER_NODE, true).await;
    await_view(&rig, &vol_id(&rig, 1), 2, PEER_NODE, true).await;
    // The source dies: the park holds with no re-drive.
    rig.a.stop().await;

    // The divergence and the coupling observation (the helper below):
    // one member's lease renewed past its sibling's death, then the
    // grant-hold pass's refusal to re-renew the survivor.
    assert_uniform_liveness_coupling(&rig).await;

    // The source returns: the re-drive's authorize sees the dead
    // member, the grant route re-issues, and the act takes the
    // ORDINARY path (never a re-mint over the live member): the
    // witness batch replays the recorded grant, the live member's
    // promote lands, the dead member's promote refuses — the park
    // spins at the mixed shape.
    rig.a.restart().await;
    let summary = poll_migration(rig.a.addr, "mig-x", "in_doubt").await;
    assert_spin_holds(&rig, "mig-x", "in_doubt", 2, 2).await;
    // The live member's promote landed (its lease was serviceable —
    // the renewal mechanism's whole point); the dead member never
    // promoted.
    assert_eq!(
        role_of(&rig.world_b, &rig.resources()[0]),
        Role::Primary,
        "the live member's promote landed under its renewed lease"
    );
    assert_eq!(
        role_of(&rig.world_b, &rig.resources()[1]),
        Role::Secondary,
        "the dead member never promotes"
    );
    let preparation = rig
        .b
        .peer_preparations
        .load(&MigrationId::new("mig-x").expect("valid id"))
        .expect("load the preparation")
        .expect("the preparation is on record");
    assert_eq!(
        preparation.grant_attempt, 0,
        "no attempt was ever spent over the live member"
    );
    assert!(preparation.participants[0].device_path.is_some());
    assert!(preparation.participants[1].device_path.is_none());

    // The spin is alive: the grant act's failures are on record at
    // the destination across the watch window (the initial wedge
    // failure plus the mixed park's repeated promote refusals) —
    // counted read-only after the observation.
    let failures = count_peer_grant_failures(&mut rig).await;
    assert!(
        failures >= 3,
        "the grant act's failures are on record across the spin (observed \
         {failures}: the wedge's connection failure plus the mixed park's \
         repeated promote refusals) — the park refuses on every retry tick"
    );

    // The data is all there (the drain completed before the cut).
    let (acked, boundary) = verify_parked_data_n(&rig, writers).await;

    evidence.invariant(
        "uniform_liveness_coupling",
        "pass: the grant-hold renewal skipped the whole preparation while one member \
         was dead — the survivor's remaining duration fell with the clock across \
         several renewal ticks (a renewal would have reset it to the full TTL)",
    );
    evidence.invariant(
        "never_mints_over_live",
        "pass: the re-drive took the ordinary path over the mixed shape (the live \
         member's promote landed under its lease, the dead member's promote refused), \
         the epochs stay 2, no attempt was ever spent",
    );
    evidence.outcome(
        "parked safe (IN_DOUBT): the mixed-liveness park is the re-mint's honest \
         boundary — the live member completed (the renewal mechanism's purpose), the \
         dead member's tail needs a lease the re-mint may not mint over it; the spin \
         is bounded and typed, recovery is operator territory (never weakened to make \
         this scenario pass)",
    );
    let record = emit_oracle_n(
        &rig,
        evidence,
        &acked,
        boundary,
        "destination",
        Some(&summary),
    );
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

/// **The undriven mixed park, walked end to end (the composite):**
/// the two halves the boundary scenarios pin separately — the
/// uniform-liveness coupling (a mixed preparation is skipped whole
/// by the renewal) and the uniformly-dead re-mint — compose into
/// one path: a mixed park that stays UNDRIVEN (the source held
/// down, no promote ever attempted over the live member) is left to
/// lapse by the renewal's own coupling, the survivor's lease dying
/// with the frozen clock past its end and the W7 fence window, and
/// the now-uniformly-dead park selects the fresh grant: the re-mint
/// fires under the attempt-1 id, the promotes complete, and the
/// migration converges at epoch exactly 3. The boundary scenario's
/// spin is what a DRIVEN mixed park becomes; this is what an
/// undriven one always was — self-resolving by construction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undriven_mixed_park_lapses_to_the_fresh_grant() {
    let seq = next_id();
    let (mut rig, writers, _transports) = live_scenario_n(&format!("whc{seq}"), 2).await;
    let mut evidence = Evidence::new("wedge-horizon/mixed-liveness/undriven-lapse-converges");
    evidence.fault(
        "partial_renewal_divergence_left_undriven",
        "one participant's lease renewed past its sibling's death (the manual \
         divergence a partial renewal leaves), the park left undriven while the \
         survivor's lease lapses and the uniformly-dead gate opens",
    );
    prepare(&rig, "mig-x").await;
    wedge_park(&rig, "mig-x").await;
    rig.witness.restart().await;
    await_view(&rig, &vol_id(&rig, 0), 2, PEER_NODE, true).await;
    await_view(&rig, &vol_id(&rig, 1), 2, PEER_NODE, true).await;
    // The source dies: the park stays UNDRIVEN — no re-drive ever
    // attempts the live member's promote (the boundary scenario's
    // driven shape is what turns this park operator territory; the
    // undriven one self-resolves).
    rig.a.stop().await;

    // The divergence and the coupling observation (the shared
    // helper): one member renewed past its sibling's death, then the
    // grant-hold pass's refusal to re-renew the survivor — the
    // preparation is not uniformly live, and the coupling is what
    // leaves the survivor to lapse instead of blocking the gate.
    assert_uniform_liveness_coupling(&rig).await;

    // THE LAPSE: the frozen clock crosses the survivor's renewed end
    // (START+CLOCK_STEP+TTL — the manual renew extended from the
    // divergence clock) and its W7 fence window, well past both
    // members' ends. The preparation is now UNIFORMLY dead: the
    // shape the fresh-grant gate arbitrates.
    let survivor_end = volvisor_campaign::rig::START + CLOCK_STEP + TTL;
    let now = rig.clock.load(Ordering::SeqCst);
    rig.clock
        .fetch_add(survivor_end + 150 - now, Ordering::SeqCst);
    await_view(&rig, &vol_id(&rig, 0), 2, PEER_NODE, false).await;
    await_view(&rig, &vol_id(&rig, 1), 2, PEER_NODE, false).await;
    evidence.invariant(
        "uniform_liveness_lapsed_uniformly_dead",
        "pass: the mixed preparation was skipped whole by the grant-hold renewal \
         (the survivor's remaining fell with the clock, never reset), and the \
         survivor's lease then lapsed with the clock — the park is uniformly dead",
    );

    // The source returns: the re-drive's authorize sees the dead
    // leases, the grant route re-issues, the act's gates pass (every
    // participant Expired and ours, the park provably pre-promote —
    // no device path was ever recorded over the live member — the
    // fence evidence intact), and the re-mint fires under the
    // attempt-1 derived id.
    rig.a.restart().await;
    let summary = poll_migration(rig.a.addr, "mig-x", "complete").await;
    let history: Vec<&str> = summary["state_history"]
        .as_array()
        .expect("the summary carries the state history")
        .iter()
        .map(|entry| entry["state"].as_str().expect("a state name"))
        .collect();
    let authorized = history
        .iter()
        .position(|state| *state == "destination_authorized")
        .expect("the park landed destination_authorized");
    let resumed = history
        .iter()
        .position(|state| *state == "vm_resumed")
        .expect("the re-mint drove the restore and resume");
    assert!(
        authorized < resumed,
        "the recovered path is ordered: {history:?}"
    );
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("the source VM observes"),
        VmState::Absent,
        "the source VM stays destroyed (the cut is durable)"
    );
    for index in 0..2 {
        assert_eq!(
            role_of(&rig.world_a, &rig.resources()[index]),
            Role::Secondary,
            "volume {index}: the source stays demoted (fenced, never resumed)"
        );
        assert_eq!(
            role_of(&rig.world_b, &rig.resources()[index]),
            Role::Primary,
            "volume {index}: the destination promoted through the re-minted grant"
        );
        evidence.invariant(
            "w1_w5_no_double_grant",
            &assert_w1_w5_vol(&rig, &vol_id(&rig, index), 3, PEER_NODE, true).await,
        );
        evidence.invariant(
            "g5_barrier",
            &assert_g5_complete(&rig, &vol_id(&rig, index), "mig-x").await,
        );
    }

    // The horizon remedy's journal shape (the helper below): exactly
    // one attempt spent, the witness journal holding the r1
    // grant-set intent (the re-mint fired once — over the
    // uniformly-dead park, never over the live member).
    assert_single_re_mint_shape(&mut rig, "mig-x").await;

    // The data-path oracle over both volumes (the converged shape).
    let (acked, boundary) = verify_parked_data_n(&rig, writers).await;
    evidence.outcome(
        "recovered: COMPLETE through the composed path — the undriven mixed park \
         was skipped whole by the grant-hold renewal (the coupling), the survivor's \
         lease lapsed with the clock, and the now-uniformly-dead park selected the \
         fresh grant: the re-mint fired under the attempt-1 id (epoch exactly 3, \
         exactly one re-mint intent at the witness), the promotes completed under \
         it, and the acknowledged prefix is byte-exact at the destination",
    );
    let record = emit_oracle_n(
        &rig,
        evidence,
        &acked,
        boundary,
        "destination",
        Some(&summary),
    );
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}

/// **The world-moved refusal:** after the park's leases died, the
/// SOURCE's host re-granted the volume over the dead lease (past the
/// fence window — a legal re-acquire; the source's VM is gone but
/// its host credential is the volume's lineage endpoint). The
/// re-drive's grant act sees a live lease held by ANOTHER host: not
/// the uniformly-ours-dead shape — the ordinary path, never a
/// re-mint — and the promote refuses on the foreign lease. The park
/// spins honestly: the world moved, the wedge's record did not, and
/// no fresh grant may mint over a live foreign writer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_grant_refused_when_the_world_moved() {
    let seq = next_id();
    let (mut rig, writer, _transport) = live_scenario(&format!("whw{seq}")).await;
    let mut evidence = Evidence::new("wedge-horizon/world-moved/source-regranted-refused");
    evidence.fault(
        "foreign_regrant_over_the_dead_lease",
        "the source's host re-granted the volume over the destination's dead lease \
         (past the fence window — a legal re-acquire); the world moved past the \
         wedge's record",
    );
    prepare(&rig, "mig-x").await;
    wedge_park(&rig, "mig-x").await;

    // The witness's restart lands the grant; the source dies (the
    // park holds); the clock crosses the lease end and the fence
    // window in one jump (the due window is never entered).
    rig.witness.restart().await;
    await_view(&rig, &rig.volume_id(), 2, PEER_NODE, true).await;
    rig.a.stop().await;
    rig.clock.fetch_add(TTL + 200, Ordering::SeqCst);
    await_view(&rig, &rig.volume_id(), 2, PEER_NODE, false).await;

    // The world moves: the source's host re-grants over the dead
    // lease (the fence window has passed) — a live epoch-3 lease at
    // the source's host.
    rig.witness
        .host_client(NODE)
        .grant(
            &rig.volume_id(),
            GrantRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: OperationId::new("manual-world-moved-regrant-1")
                    .expect("valid operation id"),
                host_id: HostId::new(NODE).expect("valid host id"),
            },
        )
        .await
        .expect("the foreign re-grant lands");
    await_view(&rig, &rig.volume_id(), 3, NODE, true).await;

    // The source returns: the re-drive's authorize sees a live lease
    // held by ANOTHER host, the grant route re-issues, and the act
    // takes the ordinary path (the view is not the destination's
    // dead lease): the witness batch replays the recorded grant, and
    // the promote refuses on the foreign live lease — the park spins.
    rig.a.restart().await;
    let summary = poll_migration(rig.a.addr, "mig-x", "in_doubt").await;
    assert_spin_holds(&rig, "mig-x", "in_doubt", 3, 1).await;
    let view = witness_view(&rig.witness, &rig.volume_id()).await;
    assert_eq!(
        view.holder.as_ref().map(HostId::as_str),
        Some(NODE),
        "the foreign lease stands"
    );
    let preparation = rig
        .b
        .peer_preparations
        .load(&MigrationId::new("mig-x").expect("valid id"))
        .expect("load the preparation")
        .expect("the preparation is on record");
    assert_eq!(
        preparation.grant_attempt, 0,
        "no attempt was ever spent over the foreign live lease"
    );
    assert!(
        preparation.participants[0].device_path.is_none(),
        "the promote never ran over the foreign lease"
    );
    assert_eq!(
        role_of(&rig.world_b, &rig.resource()),
        Role::Secondary,
        "the destination never promotes over a foreign lease"
    );
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Absent,
        "the source VM stays destroyed (the cut is durable)"
    );

    let (acked, boundary) = writer.join().await;
    assert!(!acked.is_empty(), "the writer acknowledged writes");
    let destination = verify_against(&rig.world_b, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        destination.prefix_intact(),
        "the destination keeps every acknowledged write through the park: {destination:?}"
    );
    evidence.invariant(
        "world_moved_refusal",
        "pass: the re-mint never fired over the foreign live lease (the act took the \
         ordinary path; the promote refused on the lease that is not the \
         destination's); the epoch stands at the foreign re-grant's 3, no attempt \
         was spent",
    );
    evidence.outcome(
        "parked safe (IN_DOUBT): the world-moved park is the re-mint's honest boundary — \
         the source's host legally re-acquired the volume after the destination's \
         lease lapsed, and no fresh grant may mint over a live foreign writer; the \
         spin is bounded and typed, recovery is operator territory (the foreign \
         lease's disposition is the operator's call, never the wedge's)",
    );
    let record = emit_oracle(
        &rig,
        evidence,
        &acked,
        boundary,
        "destination",
        Some(&summary),
    );
    assert!(record.is_file(), "the evidence record landed: {record:?}");
}
