//! The same-VG extent move's provider-level semantics (P6-C, ADR-0006
//! first slice part 2).
//!
//! What these tests pin, in order:
//!
//! - **The happy path and its argv**: one `pvmove` invocation with the
//!   exact scoped argv, the LV observed through the exact
//!   move-column `lvs` query, the record passing
//!   PREPARING → COPYING → COMPLETE, and ONE generation bump at the
//!   verified completion — the relocation is a fenced placement
//!   mutation; the LV's identity, path and data are unchanged.
//! - **The refusal table**: the rate-limit parameter (honored or
//!   refused, never ignored), the missing target, the cross-VG
//!   target, the vacuous target, insufficient target capacity, the
//!   spread LV, a foreign (unjournaled) active pvmove, a second move
//!   from the same source PV (LVM attaches it to the first and
//!   IGNORES its arguments — verified against LVM 2.03.16), a
//!   different-target conflict, the stale generation, and the
//!   IN_DOUBT park for new operations.
//! - **The supervision window**: a held move exhausts the window and
//!   answers `COPYING` honestly; a fresh operation re-attaches to the
//!   same move and completes it.
//! - **The honest tails**: an out-of-band abort mid-supervision parks
//!   `IN_DOUBT` with the source intact and serving; a deceptive
//!   completion (the world disagreeing with itself between the
//!   relocation observation and the verification query) never frees
//!   the source; an unobservable world mid-supervision parks
//!   `IN_DOUBT`.
//! - **The crash model** (restart = a fresh provider over the same
//!   state file and the same world): every durable-boundary window —
//!   after the PREPARING save but before the pvmove start, after the
//!   start but before the COPYING save, after the relocation but
//!   before the COMPLETE save — classifies from the world on the
//!   next incarnation and resolves the journaled intent: re-drive,
//!   roll to COPYING, or complete with the generation bump. An abort
//!   that landed while the daemon was down parks `IN_DOUBT` on the
//!   startup reconcile, and an `IN_DOUBT` record rolls forward when
//!   the world later proves completion.
//! - **The concurrency and resolution discipline** (the review
//!   findings' pins): two overlapping supervisions of one in-flight
//!   move — and a retry reconcile whose snapshot predates a
//!   concurrent completion — verify the same landed world and still
//!   bump the generation exactly once, and a competing finish path
//!   whose verification observe fails transiently answers the
//!   stored `COMPLETE` instead of parking over it (the next pass
//!   re-completes nothing, bumps nothing); a parked `IN_DOUBT`
//!   record whose world verifiably shows the pre-move placement
//!   resolves `FAILED` and admits a fresh drive, while the
//!   mixed-placement abort stays parked; an LV unobservable across
//!   the bound parks `IN_DOUBT` typed instead of silently spinning;
//!   and a foreign (unrecorded) pvmove active on the source PV
//!   refuses the fresh start typed — LVM would attach to it and
//!   ignore the arguments.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention,
//! and the bounded-wait helpers fail loudly with `panic!` on their
//! deadlines (the rig never hangs a suite).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::{
    CLAIMED_VG, MOVE_SOURCE_PV, MOVE_TARGET_PV, default_move_timing, fixture, move_fixture,
    move_fixture_with_timing, provider_from, provider_from_with_timing, seed_move,
};
use volvisor_lvm::LvmProvider;
use volvisor_lvm::state::{LvmState, MoveRecord};
use volvisor_provider::FakeRunner;
use volvisor_provider::VolumeProvider;
use volvisor_provider::conformance::{fixture_attach_request, fixture_create_request};
use volvisor_types::crash::{STORE_LVM_STATE, StoreSavePoint};
use volvisor_types::request::{DeleteVolumeRequest, MoveVolumeBackingRequest};
use volvisor_types::{ApiErrorCode, ErasurePolicy, MoveVolumeBackingState, OperationId, VolumeId};

const MIB: u64 = 1 << 20;

fn volume_id(raw: &str) -> VolumeId {
    VolumeId::new(raw).expect("valid fixture volume id")
}

/// A fixture whose drive supervises for a long window (the concurrent
/// tests mutate the world mid-supervision and need the drive still
/// polling when they do).
fn long_window_move_fixture() -> common::Fixture {
    move_fixture_with_timing(volvisor_lvm::provider::MoveTiming {
        poll_interval: std::time::Duration::from_millis(5),
        supervision_window: std::time::Duration::from_secs(30),
    })
}

fn move_request(
    operation_id: &str,
    target: &str,
    expected_generation: u64,
) -> MoveVolumeBackingRequest {
    MoveVolumeBackingRequest {
        api_version: volvisor_types::API_VERSION.to_owned(),
        operation_id: OperationId::new(operation_id).expect("valid fixture operation id"),
        target_pool_id: target.to_owned(),
        expected_generation,
        max_copy_bytes_per_sec: None,
    }
}

/// A delete request with an explicit operation id and erasure policy
/// (the move/delete interaction tests need Retain and distinct retry
/// identities).
fn delete_request_with(
    operation_id: &str,
    expected_generation: u64,
    policy: ErasurePolicy,
) -> DeleteVolumeRequest {
    DeleteVolumeRequest {
        api_version: volvisor_types::API_VERSION.to_owned(),
        operation_id: OperationId::new(operation_id).expect("valid fixture operation id"),
        expected_generation,
        data_erasure_policy: policy,
    }
}

/// The LV path the provider derives for a volume id.
fn lv_path(volume: &str) -> String {
    format!(
        "{}/{}",
        CLAIMED_VG,
        volvisor_lvm::provider::lv_name_for(&volume_id(volume))
    )
}

/// Read one volume's (move record, generation) from a state file (the
/// generation is 0 when the volume entry is gone — the dropped-record
/// shape).
fn recorded_move(state_path: &std::path::Path, volume: &str) -> (Option<MoveRecord>, u64) {
    let state = LvmState::load(state_path).expect("load state");
    (
        state.move_record(&volume_id(volume)).cloned(),
        state
            .volume(&volume_id(volume))
            .map_or(0, |stored| stored.entry.generation),
    )
}

/// The world's current placement of one LV.
fn placement_of(world: &std::sync::Mutex<common::FakeLvm>, volume: &str) -> Vec<String> {
    world
        .lock()
        .expect("world")
        .lv_devices
        .get(&lv_path(volume))
        .cloned()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The happy path and its argv
// ---------------------------------------------------------------------------

#[tokio::test]
async fn move_evacuates_to_the_target_pv_with_one_generation_bump() {
    let fixture = move_fixture();
    let volume = "vol-move-happy";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_SOURCE_PV.to_owned()],
        "lvcreate places on the VG's first PV"
    );

    let response = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect("the move completes inside the window");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 2, "the verified relocation bumps once");
    assert_eq!(response.source_pv, MOVE_SOURCE_PV);
    assert_eq!(response.target_pv, MOVE_TARGET_PV);
    assert_eq!(response.detail, None);

    // The durable record and the world agree.
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    let record = record.expect("the record");
    assert_eq!(record.state, MoveVolumeBackingState::Complete);
    assert_eq!(generation, 2);
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_TARGET_PV.to_owned()],
        "the extents relocated"
    );

    // The per-PV capacity model transferred the extents.
    {
        let world = fixture.world.lock().expect("world");
        let target_free = world
            .pv_free
            .get(MOVE_TARGET_PV)
            .copied()
            .unwrap_or_default();
        let source_free = world
            .pv_free
            .get(MOVE_SOURCE_PV)
            .copied()
            .unwrap_or_default();
        assert_eq!(target_free, common::POOL_BYTES - 64 * MIB);
        assert_eq!(source_free, common::POOL_BYTES);
    }

    // Argv-exact: the scoped pvmove and the move-column lsv query.
    let pvmove = fixture
        .runner
        .invocations()
        .into_iter()
        .find(|invocation| invocation.program == "pvmove")
        .expect("one pvmove");
    assert_eq!(
        pvmove.args,
        vec![
            "--background".to_owned(),
            "--noudevsync".to_owned(),
            "-n".to_owned(),
            lv_path(volume),
            MOVE_SOURCE_PV.to_owned(),
            MOVE_TARGET_PV.to_owned(),
        ],
        "the scoped pvmove argv"
    );
    let lvs = fixture
        .runner
        .invocations()
        .into_iter()
        .find(|invocation| {
            invocation.program == "lvs"
                && invocation
                    .args
                    .contains(&"vg_name,lv_name,lv_attr,copy_percent,devices".to_owned())
        })
        .expect("the move observation query");
    assert_eq!(
        lvs.args,
        vec![
            "--reportformat".to_owned(),
            "json".to_owned(),
            "--units".to_owned(),
            "b".to_owned(),
            "--nosuffix".to_owned(),
            "-o".to_owned(),
            "vg_name,lv_name,lv_attr,copy_percent,devices".to_owned(),
        ],
        "the move-column lvs argv"
    );
}

#[tokio::test]
async fn a_completed_move_reobserves_idempotently_without_a_new_pvmove() {
    let fixture = move_fixture();
    let volume = "vol-move-replay";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect("complete");
    let pvmove_count = fixture
        .runner
        .invocations()
        .iter()
        .filter(|invocation| invocation.program == "pvmove")
        .count();
    assert_eq!(pvmove_count, 1);

    // The same target under a fresh operation id: the recorded
    // completion answers, nothing re-runs, the generation is stable.
    let response = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-2", MOVE_TARGET_PV, 2),
        )
        .await
        .expect("idempotent re-observation");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 2);
    let pvmove_count_again = fixture
        .runner
        .invocations()
        .iter()
        .filter(|invocation| invocation.program == "pvmove")
        .count();
    assert_eq!(pvmove_count_again, 1, "no second pvmove");
    let (_, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(generation, 2, "no second bump");
}

#[tokio::test]
async fn an_attached_volume_moves_online() {
    let fixture = move_fixture();
    let volume = "vol-move-attached";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    fixture
        .provider
        .attach_volume(
            &volume_id(volume),
            &fixture_attach_request(volume, "att-move-1", 1),
        )
        .await
        .expect("attach");

    // An attached volume is admitted: the LV's dm identity is stable
    // across a pvmove, the frontend keeps serving.
    let response = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 2),
        )
        .await
        .expect("the attached volume moves");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 3);
}

// ---------------------------------------------------------------------------
// The refusal table
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_rate_limit_parameter_is_refused_typed_never_ignored() {
    let fixture = move_fixture();
    let volume = "vol-move-rate";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    let mut request = move_request("op-move-rate", MOVE_TARGET_PV, 1);
    request.max_copy_bytes_per_sec = Some(1024 * 1024);
    let error = fixture
        .provider
        .move_volume_backing(&volume_id(volume), &request)
        .await
        .expect_err("refused");
    assert_eq!(error.code, ApiErrorCode::UnsupportedClassOrPolicy);
    assert!(
        error.detail.contains("max_copy_bytes_per_sec"),
        "the refusal names the parameter: {error}"
    );

    // Nothing journaled, nothing moved.
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert!(record.is_none());
    assert_eq!(generation, 1);
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_SOURCE_PV.to_owned()]
    );
}

#[tokio::test]
async fn a_missing_target_pv_is_not_found() {
    let fixture = move_fixture();
    let volume = "vol-move-missing-target";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", "/dev/nonexistent", 1),
        )
        .await
        .expect_err("refused");
    assert_eq!(error.code, ApiErrorCode::NotFound);
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert!(record.is_none());
}

#[tokio::test]
async fn a_cross_vg_target_is_outside_the_qualified_scope() {
    let fixture = move_fixture();
    let volume = "vol-move-cross-vg";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    // A PV in a foreign volume group.
    fixture.world.lock().expect("world").add_pv_to_vg(
        "/dev/pv-foreign",
        "othervg",
        common::POOL_BYTES,
    );

    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", "/dev/pv-foreign", 1),
        )
        .await
        .expect_err("refused");
    assert_eq!(error.code, ApiErrorCode::MoveUnsupportedScope);
    assert!(
        error.detail.contains("cross-VG"),
        "the refusal names the scope: {error}"
    );
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert!(record.is_none());
}

#[tokio::test]
async fn a_vacuous_target_is_rejected() {
    let fixture = move_fixture();
    let volume = "vol-move-vacuous";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_SOURCE_PV, 1),
        )
        .await
        .expect_err("refused");
    assert_eq!(error.code, ApiErrorCode::InvalidRequest);
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert!(record.is_none());
}

#[tokio::test]
async fn insufficient_target_capacity_is_refused() {
    let fixture = move_fixture();
    let volume = "vol-move-capacity";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    fixture
        .world
        .lock()
        .expect("world")
        .pv_free
        .insert(MOVE_TARGET_PV.to_owned(), 32 * MIB);

    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect_err("refused");
    assert_eq!(error.code, ApiErrorCode::NoSafeCapacity);
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert!(record.is_none());
}

#[tokio::test]
async fn a_spread_lv_is_outside_the_single_source_scope() {
    let fixture = move_fixture();
    let volume = "vol-move-spread";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    // The LV's extents sit on both PVs (the single-source first-slice
    // scope cannot derive one evacuation source).
    fixture.world.lock().expect("world").lv_devices.insert(
        lv_path(volume),
        vec![MOVE_SOURCE_PV.to_owned(), MOVE_TARGET_PV.to_owned()],
    );

    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect_err("refused");
    assert_eq!(error.code, ApiErrorCode::MoveUnsupportedScope);
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert!(record.is_none());
}

#[tokio::test]
async fn a_foreign_active_pvmove_is_never_adopted() {
    let fixture = move_fixture();
    let volume = "vol-move-foreign";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    // An unjournaled pvmove is already running on the LV (an
    // operator act): the provider refuses to supervise it.
    fixture.world.lock().expect("world").moves.insert(
        lv_path(volume),
        common::FakeMove {
            source: MOVE_SOURCE_PV.to_owned(),
            target: MOVE_TARGET_PV.to_owned(),
            percent: 10,
        },
    );

    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect_err("refused");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert!(
        error.detail.contains("outside volvisor's journal"),
        "the refusal names the foreign move: {error}"
    );
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert!(
        record.is_none(),
        "no record was journaled for a foreign move"
    );
}

#[tokio::test]
async fn one_move_per_source_pv_at_a_time() {
    let fixture = long_window_move_fixture();
    let first = "vol-move-src-1";
    let second = "vol-move-src-2";
    for volume in [first, second] {
        fixture
            .provider
            .create_volume(&fixture_create_request(volume, 64 * MIB))
            .await
            .expect("create");
    }

    // Hold the first move: it keeps evacuating the source PV.
    fixture.world.lock().expect("world").hold_moves = true;
    let first_move = tokio::spawn({
        let provider = Arc::clone(&fixture.provider);
        let volume = volume_id(first);
        async move {
            provider
                .move_volume_backing(&volume, &move_request("op-move-1", MOVE_TARGET_PV, 1))
                .await
        }
    });
    // Wait for the first move to be running (bounded spin).
    for _ in 0..500 {
        if !fixture.world.lock().expect("world").moves.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .moves
            .contains_key(&lv_path(first)),
        "the first move is running"
    );

    // A second volume from the same source PV: LVM would attach the
    // scoped pvmove to the first and IGNORE its arguments (verified
    // against LVM 2.03.16) — a silent no-op the provider refuses
    // typed instead.
    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(second),
            &move_request("op-move-2", MOVE_TARGET_PV, 1),
        )
        .await
        .expect_err("refused");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert!(
        error.detail.contains("one move per source PV"),
        "the refusal names the LVM constraint: {error}"
    );
    let (record, _) = recorded_move(&fixture.state_path, second);
    assert!(record.is_none(), "the refused second move journals nothing");

    // Release and let the first finish (the spawned drive completes).
    fixture.world.lock().expect("world").hold_moves = false;
    let response = first_move
        .await
        .expect("join")
        .expect("the first move completes");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
}

#[tokio::test]
async fn a_different_target_while_a_move_is_active_conflicts() {
    let fixture = long_window_move_fixture();
    let volume = "vol-move-conflict";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    fixture.world.lock().expect("world").hold_moves = true;
    let active = tokio::spawn({
        let provider = Arc::clone(&fixture.provider);
        let volume = volume_id(volume);
        async move {
            provider
                .move_volume_backing(&volume, &move_request("op-move-1", MOVE_TARGET_PV, 1))
                .await
        }
    });
    for _ in 0..500 {
        if !fixture.world.lock().expect("world").moves.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    // A third PV as the alternative target.
    fixture
        .world
        .lock()
        .expect("world")
        .add_pv_to_vg("/dev/pv-c", CLAIMED_VG, common::POOL_BYTES);
    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-2", "/dev/pv-c", 1),
        )
        .await
        .expect_err("refused");
    assert_eq!(error.code, ApiErrorCode::InvalidState);

    fixture.world.lock().expect("world").hold_moves = false;
    let response = active.await.expect("join").expect("completes");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
}

#[tokio::test]
async fn a_stale_generation_is_refused() {
    let fixture = move_fixture();
    let volume = "vol-move-stale";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 99),
        )
        .await
        .expect_err("refused");
    assert_eq!(error.code, ApiErrorCode::StaleGeneration);
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert!(record.is_none());
}

// ---------------------------------------------------------------------------
// The supervision window and re-attachment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_window_expires_honestly_and_a_fresh_operation_re_attaches() {
    let fixture = move_fixture();
    let volume = "vol-move-window";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // The move never advances: the window expires with it running.
    fixture.world.lock().expect("world").hold_moves = true;
    let response = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect("an honest COPYING answer");
    assert_eq!(response.state, MoveVolumeBackingState::Copying);
    assert_eq!(response.generation, 1, "no bump while copying");
    assert!(
        response
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("supervision window expired"),
        "the detail names the window: {:?}",
        response.detail
    );
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the record").state,
        MoveVolumeBackingState::Copying
    );
    assert_eq!(generation, 1);

    // The world says the same thing the response did.
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .moves
            .contains_key(&lv_path(volume)),
        "the pvmove is still running outside the daemon"
    );

    // A fresh operation re-attaches to the same move and completes.
    fixture.world.lock().expect("world").hold_moves = false;
    let response = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-2", MOVE_TARGET_PV, 1),
        )
        .await
        .expect("the re-attached move completes");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 2, "one bump across both operations");
    let (_, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(generation, 2);
}

// ---------------------------------------------------------------------------
// The honest tails (IN_DOUBT)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_out_of_band_abort_mid_supervision_parks_in_doubt() {
    let fixture = long_window_move_fixture();
    let volume = "vol-move-abort";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // Hold the move so the drive supervises, then abort it
    // out-of-band (the operator act the provider never performs).
    fixture.world.lock().expect("world").hold_moves = true;
    let drive = tokio::spawn({
        let provider = Arc::clone(&fixture.provider);
        let volume = volume_id(volume);
        async move {
            provider
                .move_volume_backing(&volume, &move_request("op-move-1", MOVE_TARGET_PV, 1))
                .await
        }
    });
    for _ in 0..500 {
        if !fixture.world.lock().expect("world").moves.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    // pvmove --abort: the move ends, the placement never left the
    // source.
    fixture.world.lock().expect("world").moves.clear();

    let response = drive.await.expect("join").expect("an IN_DOUBT answer");
    assert_eq!(response.state, MoveVolumeBackingState::InDoubt);
    assert_eq!(response.generation, 1, "the generation never bumped");
    assert!(
        response
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("without relocating"),
        "the detail names the unverified outcome: {:?}",
        response.detail
    );

    // The source is intact and serving; the record parks.
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_SOURCE_PV.to_owned()]
    );
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    let record = record.expect("the record");
    assert_eq!(record.state, MoveVolumeBackingState::InDoubt);
    assert_eq!(generation, 1);

    // New move operations park on the IN_DOUBT record (fail-closed:
    // the outcome is unresolved).
    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-2", MOVE_TARGET_PV, 1),
        )
        .await
        .expect_err("parked");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert!(
        error.detail.contains("IN_DOUBT"),
        "the refusal names the park: {error}"
    );
}

#[tokio::test]
async fn a_deceptive_completion_never_frees_the_source() {
    let fixture = move_fixture();
    let volume = "vol-move-deceptive";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // The world will disagree with itself: the relocation is observed
    // once, then an out-of-band reverse relocation lands before the
    // verification query. The completion verification must refuse it.
    fixture
        .world
        .lock()
        .expect("world")
        .restore_source_after_freed_sightings = 1;

    let response = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect("an IN_DOUBT answer, never a false COMPLETE");
    assert_eq!(response.state, MoveVolumeBackingState::InDoubt);
    assert_eq!(response.generation, 1, "the source was never freed");
    assert!(
        response
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("verification refused"),
        "the detail names the refused verification: {:?}",
        response.detail
    );

    // The extents are on the source (the reverse relocation stands);
    // the record parks IN_DOUBT with the generation unbumped.
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_SOURCE_PV.to_owned()]
    );
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the record").state,
        MoveVolumeBackingState::InDoubt
    );
    assert_eq!(generation, 1);
}

#[tokio::test]
async fn an_unobservable_world_mid_supervision_parks_in_doubt() {
    let fixture = long_window_move_fixture();
    let volume = "vol-move-unobservable";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    fixture.world.lock().expect("world").hold_moves = true;
    let drive = tokio::spawn({
        let provider = Arc::clone(&fixture.provider);
        let volume = volume_id(volume);
        async move {
            provider
                .move_volume_backing(&volume, &move_request("op-move-1", MOVE_TARGET_PV, 1))
                .await
        }
    });
    for _ in 0..500 {
        if !fixture.world.lock().expect("world").moves.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    // The observation itself starts failing mid-supervision (a
    // failing lvs): the outcome is unknown, never a guess.
    fixture.world.lock().expect("world").fail_lvs = true;

    let response = drive.await.expect("join").expect("an IN_DOUBT answer");
    assert_eq!(response.state, MoveVolumeBackingState::InDoubt);
    assert!(
        response
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("observation failed"),
        "the detail names the failed observation: {:?}",
        response.detail
    );
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the record").state,
        MoveVolumeBackingState::InDoubt
    );
}

// ---------------------------------------------------------------------------
// The crash model: restarts classify from the world
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_crash_after_the_preparing_save_re_drives_on_the_retry_pass() {
    let fixture = move_fixture();
    let volume = "vol-move-crash-preparing";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // The dead incarnation's durable facts: a PREPARING record, a
    // world where the pvmove never verifiably started.
    seed_move(
        &fixture.state_path,
        volume,
        MOVE_SOURCE_PV,
        MOVE_TARGET_PV,
        MoveVolumeBackingState::Preparing,
    );
    let restarted =
        provider_from_with_timing(&fixture.state_path, &fixture.world, default_move_timing());
    // The constructor classifies without starting anything.
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("record").state,
        MoveVolumeBackingState::Preparing,
        "the constructor never starts a pvmove"
    );

    // The retry pass resolves the journaled intent: the pvmove
    // starts, the record rolls to COPYING...
    let report = restarted.move_reconcile_pass().expect("pass");
    assert_eq!(report.redriven, vec![volume_id(volume)]);
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .moves
            .contains_key(&lv_path(volume)),
        "the re-drive started the pvmove"
    );
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("record").state,
        MoveVolumeBackingState::Copying
    );

    // ...the world's deterministic clock advances it (pass 2: the
    // mirror is live, the record already says so)...
    let report = restarted.move_reconcile_pass().expect("pass");
    assert_eq!(report.marked_copying, vec![volume_id(volume)]);
    assert_eq!(report.completed, Vec::new());

    // ...and the landing completes it (pass 3): one verified
    // completion, one generation bump.
    let report = restarted.move_reconcile_pass().expect("pass");
    assert_eq!(report.completed, vec![volume_id(volume)]);
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 2);
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_TARGET_PV.to_owned()]
    );
}

#[tokio::test]
async fn a_crash_after_the_pvmove_start_rolls_the_record_to_copying() {
    let fixture = move_fixture();
    let volume = "vol-move-crash-started";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // The dead incarnation started the pvmove but never saved
    // COPYING: the record is PREPARING while the mirror runs (the
    // kernel-side move survives the daemon — it is outside it). The
    // seeded percent (60) is one observation away from landing.
    {
        let mut world = fixture.world.lock().expect("world");
        world.hold_moves = true;
        world.moves.insert(
            lv_path(volume),
            common::FakeMove {
                source: MOVE_SOURCE_PV.to_owned(),
                target: MOVE_TARGET_PV.to_owned(),
                percent: 60,
            },
        );
    }
    seed_move(
        &fixture.state_path,
        volume,
        MOVE_SOURCE_PV,
        MOVE_TARGET_PV,
        MoveVolumeBackingState::Preparing,
    );

    // The startup reconcile classifies from the world: a live mirror
    // observed on a journaled record rolls it to COPYING durably
    // (still held, so it cannot land yet).
    let restarted =
        provider_from_with_timing(&fixture.state_path, &fixture.world, default_move_timing());
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("record").state,
        MoveVolumeBackingState::Copying,
        "the record was rolled to the observed truth"
    );

    // The move lands while nobody supervises; the retry pass observes
    // the landing and completes it with the one generation bump.
    fixture.world.lock().expect("world").hold_moves = false;
    let report = restarted.move_reconcile_pass().expect("pass");
    assert_eq!(report.completed, vec![volume_id(volume)]);
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 2);
}

#[tokio::test]
async fn a_crash_before_the_complete_save_completes_on_the_startup_reconcile() {
    let fixture = move_fixture();
    let volume = "vol-move-crash-landing";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // The dead incarnation's world: the move landed (the placement is
    // the target, no mirror is active) but the COMPLETE save never
    // happened. The record still says COPYING.
    seed_move(
        &fixture.state_path,
        volume,
        MOVE_SOURCE_PV,
        MOVE_TARGET_PV,
        MoveVolumeBackingState::Copying,
    );
    fixture
        .world
        .lock()
        .expect("world")
        .place_lv_on(&lv_path(volume), MOVE_TARGET_PV);

    // Construction itself classifies: the relocation is provable from
    // the world, so the record completes with the generation bump.
    let _restarted =
        provider_from_with_timing(&fixture.state_path, &fixture.world, default_move_timing());
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 2, "the startup completion bumps once");
}

#[tokio::test]
async fn an_abort_that_landed_while_down_parks_in_doubt_on_the_startup_reconcile() {
    let fixture = move_fixture();
    let volume = "vol-move-crash-abort";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // The dead incarnation said COPYING; the move was aborted
    // out-of-band while the daemon was down (the placement never left
    // the source, no mirror is active).
    seed_move(
        &fixture.state_path,
        volume,
        MOVE_SOURCE_PV,
        MOVE_TARGET_PV,
        MoveVolumeBackingState::Copying,
    );

    let _restarted =
        provider_from_with_timing(&fixture.state_path, &fixture.world, default_move_timing());
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    let record = record.expect("record");
    assert_eq!(
        record.state,
        MoveVolumeBackingState::InDoubt,
        "the abort shape parks, never silently reverts to Ready"
    );
    assert!(
        record
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("without relocating"),
        "the park names the unverified outcome"
    );
    assert_eq!(generation, 1, "nothing was freed");
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_SOURCE_PV.to_owned()]
    );
}

#[tokio::test]
async fn an_in_doubt_record_rolls_forward_when_the_world_proves_completion() {
    let fixture = move_fixture();
    let volume = "vol-move-indoubt-rollforward";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // The parked record (the abort shape of the previous test)...
    seed_move(
        &fixture.state_path,
        volume,
        MOVE_SOURCE_PV,
        MOVE_TARGET_PV,
        MoveVolumeBackingState::InDoubt,
    );
    // ...and the world that later proves the relocation completed
    // (the pvmove an operator resumed out-of-band, or the observation
    // that was failing when the park happened now succeeds).
    fixture
        .world
        .lock()
        .expect("world")
        .place_lv_on(&lv_path(volume), MOVE_TARGET_PV);

    // The startup reconcile itself rolls the parked record forward
    // under reconciled authority: the relocation is provable, so the
    // record completes with the one generation bump.
    let _restarted =
        provider_from_with_timing(&fixture.state_path, &fixture.world, default_move_timing());
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("record").state,
        MoveVolumeBackingState::Complete,
        "an IN_DOUBT record rolls forward under reconciled authority"
    );
    assert_eq!(generation, 2);

    // The retry pass over the settled record does nothing (a
    // completed record is a historical fact, never re-completed).
    let restarted =
        provider_from_with_timing(&fixture.state_path, &fixture.world, default_move_timing());
    let report = restarted.move_reconcile_pass().expect("pass");
    assert_eq!(report.completed, Vec::new());
    let (_, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(generation, 2, "no second bump for a settled record");
}

#[tokio::test]
async fn a_dropped_volume_loses_its_move_record() {
    let fixture = move_fixture();
    let volume = "vol-move-dropped";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    seed_move(
        &fixture.state_path,
        volume,
        MOVE_SOURCE_PV,
        MOVE_TARGET_PV,
        MoveVolumeBackingState::Copying,
    );

    // The volume is deleted under the move (the settled tail: the
    // record outlives its volume entry).
    let state_path = fixture.state_path.clone();
    {
        let mut state = LvmState::load(&state_path).expect("load");
        state.remove_volume(&volume_id(volume));
        state.save(&state_path).expect("save");
    }

    // The startup reconcile drops the orphaned record: a record whose
    // volume is gone is dropped, never completed.
    let _restarted = provider_from_with_timing(&state_path, &fixture.world, default_move_timing());
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert!(
        record.is_none(),
        "a record whose volume is gone is dropped, not completed"
    );

    // The retry pass over the settled state has nothing to report.
    let restarted = provider_from_with_timing(&state_path, &fixture.world, default_move_timing());
    let report = restarted.move_reconcile_pass().expect("pass");
    assert_eq!(report.dropped, Vec::new());
}

// ---------------------------------------------------------------------------
// The persist points: injected store-save failures at the move's
// commit boundaries. A fresh drive performs exactly three LVM-state
// saves — PREPARING, COPYING, COMPLETE — so `skip` 0/1/2 targets save
// 1/2/3 (different points neither consume nor decrement the arm).
// ---------------------------------------------------------------------------

/// The `PREPARING` save is the move's sole reconciliation anchor: if it
/// cannot land, nothing durable exists to classify against, so the drive
/// starts no `pvmove` at all. The failure is fail-closed and does not
/// wedge the volume — a later normal move runs the whole sequence.
#[tokio::test]
async fn a_preparing_save_failure_starts_no_pvmove() {
    let fixture = move_fixture();
    let volume = "vol-move-save-preparing";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // The drive's first state save is the PREPARING anchor: skip = 0
    // fails exactly there, before `pvmove` can start.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterTmpWrite,
        0,
    );
    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect_err("the PREPARING save fails");
    assert_eq!(error.code, ApiErrorCode::Internal, "{error}");
    assert!(
        error.detail.contains("PREPARING record could not be saved"),
        "the error names the unsaved anchor: {error}"
    );
    assert!(
        error.detail.contains("no pvmove was started"),
        "the error states the fail-closed outcome: {error}"
    );

    // No durable anchor means no world mutation: no pvmove ran, no
    // mirror is live, and the state file is exactly the pre-move shape.
    assert!(
        fixture.world.lock().expect("world").moves.is_empty(),
        "the failed anchor must start no move"
    );
    assert_eq!(pvmove_count(&fixture.runner), 0, "no pvmove invocation");
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert!(record.is_none(), "the failed anchor left no record");
    assert_eq!(generation, 1);
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_SOURCE_PV.to_owned()]
    );

    // The failed save did not wedge the volume: the one-shot arm is
    // consumed, and a fresh operation drives the whole move.
    let response = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-2", MOVE_TARGET_PV, 1),
        )
        .await
        .expect("the retry move completes");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 2);
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the retry's record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 2);
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_TARGET_PV.to_owned()]
    );
    assert_eq!(pvmove_count(&fixture.runner), 1);
}

/// The `PREPARING` save's rename can land even though the save reported an
/// error at the directory-fsync boundary. The durable anchor then exists,
/// so the in-memory record is kept and the error says so — but the call
/// still starts no `pvmove`. A retry re-attaches to the durable anchor and
/// drives the move to completion.
#[tokio::test]
async fn a_preparing_save_failure_after_rename_keeps_the_anchor_and_starts_no_pvmove() {
    let fixture = move_fixture();
    let volume = "vol-move-save-preparing-rename";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // Fail the anchor save after its rename: the durable record exists
    // even though the save returned an error.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterRenameBeforeDirFsync,
        0,
    );
    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect_err("the PREPARING save fails after its rename");
    assert_eq!(error.code, ApiErrorCode::Internal, "{error}");
    assert!(
        error.detail.contains("reached the durable state"),
        "the error says the anchor landed: {error}"
    );
    assert!(
        error.detail.contains("no pvmove was started"),
        "the error states the fail-closed outcome: {error}"
    );

    // The anchor is durable and the in-memory record was kept, but no
    // world mutation happened.
    assert!(
        fixture.world.lock().expect("world").moves.is_empty(),
        "the failed anchor must start no move"
    );
    assert_eq!(pvmove_count(&fixture.runner), 0, "no pvmove invocation");
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    let record = record.expect("the anchor's rename landed");
    assert_eq!(record.state, MoveVolumeBackingState::Preparing);
    assert_eq!(record.target_pv, MOVE_TARGET_PV);
    assert_eq!(generation, 1);
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_SOURCE_PV.to_owned()]
    );

    // The kept in-memory anchor lets the retry re-attach without a
    // second PREPARING save, and the move completes with one bump.
    let response = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-2", MOVE_TARGET_PV, 1),
        )
        .await
        .expect("the retry move completes");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 2);
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the retry's record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 2);
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_TARGET_PV.to_owned()]
    );
    assert_eq!(pvmove_count(&fixture.runner), 1);
}

/// The `COPYING` save follows the already-started `pvmove`: its failure
/// cannot un-start the kernel-side move. The prior durable record
/// (`PREPARING`) remains the anchor, and the next incarnation's startup
/// reconcile observes the live mirror, lets it land, and verifies the
/// completion with exactly one generation bump.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one end-to-end failure/restart narrative
async fn a_copying_save_failure_keeps_the_anchor_and_startup_completes() {
    let fixture = move_fixture();
    let volume = "vol-move-save-copying";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // skip = 1 passes the PREPARING save and fails the COPYING save:
    // the pvmove is already running, so the anchor stays PREPARING.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterTmpWrite,
        1,
    );
    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect_err("the COPYING save fails");
    assert_eq!(error.code, ApiErrorCode::Internal, "{error}");
    assert!(
        error
            .detail
            .contains("the pvmove was started but its COPYING record could not be saved"),
        "the error names the started-but-unrecorded move: {error}"
    );
    assert!(
        error
            .detail
            .contains("the durable prior record is the anchor"),
        "the error names the surviving anchor: {error}"
    );

    // The world started the move; the durable anchor still says
    // PREPARING and the generation was never bumped. The fake world
    // keeps the source placement until the move lands.
    assert_eq!(
        fixture.world.lock().expect("world").moves.len(),
        1,
        "the pvmove is live despite the failed COPYING save"
    );
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the durable anchor").state,
        MoveVolumeBackingState::Preparing
    );
    assert_eq!(generation, 1);
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_SOURCE_PV.to_owned()]
    );
    assert_eq!(pvmove_count(&fixture.runner), 1);

    // The restart's constructor reconcile observes the world (the move
    // lands across its two lvs queries) and completes the verified
    // relocation: one bump, target placement, no live move.
    let restarted =
        provider_from_with_timing(&fixture.state_path, &fixture.world, default_move_timing());
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the completed record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 2, "exactly one bump across the restart");
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_TARGET_PV.to_owned()]
    );
    assert!(fixture.world.lock().expect("world").moves.is_empty());
    // The constructor's reconcile runs with `redrive = false`, so it
    // starts no pvmove; the re-issue answers the durable record. The
    // single invocation under test is the first incarnation's.
    assert_eq!(pvmove_count(&fixture.runner), 1);

    // A same-target re-issue answers the completed record without
    // another pvmove and without a second bump.
    let response = restarted
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-after", MOVE_TARGET_PV, 2),
        )
        .await
        .expect("the completed record answers");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 2);
    let (_, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(generation, 2);
    assert_eq!(pvmove_count(&fixture.runner), 1, "no new pvmove");
}

/// The `COMPLETE` save failing **before** its rename: the world already
/// relocated, but the durable file still holds the prior `COPYING`
/// record and generation. The in-memory provider restores that prior
/// durable shape (never a completion against a prior record), and the
/// restart's verified relocation completes it with one bump.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one end-to-end failure/restart narrative
async fn a_complete_save_failure_recovers_with_one_bump() {
    let fixture = move_fixture();
    let volume = "vol-move-save-complete";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // skip = 2 passes the PREPARING and COPYING saves; the COMPLETE
    // save fails after its tmp write, before the rename — the durable
    // file still holds COPYING and generation 1.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterTmpWrite,
        2,
    );
    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect_err("the COMPLETE save fails before its rename");
    assert_eq!(error.code, ApiErrorCode::Internal, "{error}");
    assert!(
        error
            .detail
            .contains("the durable move record still says the prior state"),
        "the error names the surviving prior record: {error}"
    );
    assert!(
        error
            .detail
            .contains("reconciliation will complete the move"),
        "the error names the recovery path: {error}"
    );

    // The world already relocated; the durable anchor has not.
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_TARGET_PV.to_owned()]
    );
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the prior record").state,
        MoveVolumeBackingState::Copying
    );
    assert_eq!(generation, 1);

    // The contract restores the prior durable shape in memory too: the
    // provider never reports a completion against a prior durable
    // record (no 200-until-restart).
    let inspected = fixture
        .provider
        .inspect_volume(&volume_id(volume))
        .await
        .expect("inspect the restored in-memory state");
    assert_eq!(inspected.generation, 1);

    // The restart proves the relocation and completes the record
    // exactly once.
    let restarted =
        provider_from_with_timing(&fixture.state_path, &fixture.world, default_move_timing());
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the completed record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 2, "exactly one bump");
    let inspected = restarted
        .inspect_volume(&volume_id(volume))
        .await
        .expect("inspect the completed volume");
    assert_eq!(inspected.generation, 2);
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_TARGET_PV.to_owned()]
    );

    // A same-target re-issue answers the completed record without
    // another pvmove and without a second bump.
    let response = restarted
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-after", MOVE_TARGET_PV, 2),
        )
        .await
        .expect("the completed record answers");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 2);
    let (_, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(generation, 2);
    assert_eq!(pvmove_count(&fixture.runner), 1);
}

/// The post-rename carve-out: the `COMPLETE` save's rename landed before
/// the injected directory-fsync failure, so the **new** state is the
/// durable truth. The in-memory completion is kept (never rolled back
/// against a durable file that already has it), and a restart must not
/// bump the generation again.
#[tokio::test]
async fn a_complete_save_failure_after_rename_is_the_carve_out() {
    let fixture = move_fixture();
    let volume = "vol-move-save-rename";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // skip = 2 passes the PREPARING and COPYING saves; the COMPLETE
    // save fails at the directory-fsync boundary, after its rename.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterRenameBeforeDirFsync,
        2,
    );
    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect_err("the COMPLETE save fails after its rename");
    assert_eq!(error.code, ApiErrorCode::Internal, "{error}");
    assert!(
        error
            .detail
            .contains("the durable state already records the completed move"),
        "the error names the landed completion: {error}"
    );
    assert!(
        error.detail.contains("volume generation 2"),
        "the error names the landed generation: {error}"
    );

    // The rename is the commit: both the durable file and the
    // in-memory view record the completion and generation 2.
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the completed record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 2);
    let inspected = fixture
        .provider
        .inspect_volume(&volume_id(volume))
        .await
        .expect("inspect the kept in-memory completion");
    assert_eq!(
        inspected.generation, 2,
        "the in-memory completion is kept, never rolled back"
    );
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_TARGET_PV.to_owned()]
    );

    // A restart must not bump again: the completed record is a
    // historical fact and the durable generation is already 2.
    let restarted =
        provider_from_with_timing(&fixture.state_path, &fixture.world, default_move_timing());
    let inspected = restarted
        .inspect_volume(&volume_id(volume))
        .await
        .expect("inspect after restart");
    assert_eq!(inspected.generation, 2, "no second bump across the restart");
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the settled record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 2);
}

// ---------------------------------------------------------------------------
// Delete and the move journal: an unsettled placement is never discarded,
// and a settled record is cleared atomically with its volume
// ---------------------------------------------------------------------------

/// A delete must not touch an LV whose online move is still unsettled:
/// `ZeroDiscard` would discard through both legs of a live pvmove
/// mirror, and `lvremove` would refuse the moving LV after the discard
/// had already run. The refusal is typed, runs no destructive command,
/// and leaves both the LV and the durable record exactly as they were.
///
/// The `PREPARING` shape is the primary pin (a crash between the
/// journal write and the pvmove start). The remaining unsettled states
/// are driven through the same refusal: `COPYING` and `IN_DOUBT` are
/// the states this provider actually writes, while `MIRROR_READY` and
/// `PIVOTED` are mirrored here because the contract refuses every
/// unsettled placement even though the same-VG path never writes them.
#[tokio::test]
#[allow(clippy::too_many_lines)] // the unsettled-state matrix
async fn delete_refuses_an_unsettled_move_before_any_destructive_command() {
    let fixture = move_fixture();
    let volume = "vol-delete-unsettled";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    let id = volume_id(volume);
    let lv = lv_path(volume);

    // PREPARING survives the startup classification: no pvmove is
    // active and the extents sit on the source, so the constructor
    // (redrive = false) leaves the journaled intent alone.
    seed_move(
        &fixture.state_path,
        volume,
        MOVE_SOURCE_PV,
        MOVE_TARGET_PV,
        MoveVolumeBackingState::Preparing,
    );
    let restarted =
        common::fixture_from_state(fixture.state_path.clone(), Arc::clone(&fixture.world));
    let before = restarted.runner.invocations().len();
    let error = restarted
        .provider
        .delete_volume(
            &id,
            &delete_request_with("op-delete-preparing", 1, ErasurePolicy::Retain),
        )
        .await
        .expect_err("an unsettled move refuses the delete");
    assert_eq!(error.code, ApiErrorCode::InvalidState, "{error}");
    assert!(
        error.detail.contains("unsettled online move"),
        "the refusal names the unsettled placement: {error}"
    );
    assert!(
        !restarted.runner.invocations()[before..]
            .iter()
            .any(|invocation| matches!(invocation.program.as_str(), "blkdiscard" | "lvremove")),
        "the refusal must run no destructive command"
    );
    assert!(
        fixture.world.lock().expect("world").lvs.contains_key(&lv),
        "the LV is untouched"
    );
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    let record = record.expect("the durable record survives the refusal");
    assert_eq!(record.state, MoveVolumeBackingState::Preparing);
    assert_eq!(generation, 1, "a refused delete bumps nothing");

    // Every other unsettled state takes the same refusal. The world is
    // arranged per state so the startup reconcile preserves the seeded
    // record (a live held mirror for COPYING, a mixed placement for
    // IN_DOUBT; MIRROR_READY/PIVOTED are left as-seeded).
    for state in [
        MoveVolumeBackingState::Copying,
        MoveVolumeBackingState::MirrorReady,
        MoveVolumeBackingState::Pivoted,
        MoveVolumeBackingState::InDoubt,
    ] {
        {
            let mut world = fixture.world.lock().expect("world");
            world.moves.clear();
            world.hold_moves = false;
            world
                .lv_devices
                .insert(lv.clone(), vec![MOVE_SOURCE_PV.to_owned()]);
            match state {
                MoveVolumeBackingState::Copying => {
                    world.moves.insert(
                        lv.clone(),
                        common::FakeMove {
                            source: MOVE_SOURCE_PV.to_owned(),
                            target: MOVE_TARGET_PV.to_owned(),
                            percent: 0,
                        },
                    );
                    world.hold_moves = true;
                }
                MoveVolumeBackingState::InDoubt => {
                    world.lv_devices.insert(
                        lv.clone(),
                        vec![MOVE_SOURCE_PV.to_owned(), MOVE_TARGET_PV.to_owned()],
                    );
                }
                _ => {}
            }
        }
        seed_move(
            &fixture.state_path,
            volume,
            MOVE_SOURCE_PV,
            MOVE_TARGET_PV,
            state,
        );
        let restarted =
            common::fixture_from_state(fixture.state_path.clone(), Arc::clone(&fixture.world));
        let before = restarted.runner.invocations().len();
        let error = restarted
            .provider
            .delete_volume(
                &id,
                &delete_request_with(&format!("op-delete-{state:?}"), 1, ErasurePolicy::Retain),
            )
            .await
            .expect_err("an unsettled move refuses the delete");
        assert_eq!(error.code, ApiErrorCode::InvalidState, "{error}");
        assert!(
            error.detail.contains("unsettled online move"),
            "the {state:?} refusal names the unsettled placement: {error}"
        );
        assert!(
            !restarted.runner.invocations()[before..]
                .iter()
                .any(|invocation| matches!(invocation.program.as_str(), "blkdiscard" | "lvremove")),
            "the {state:?} refusal must run no destructive command"
        );
        assert!(
            fixture.world.lock().expect("world").lvs.contains_key(&lv),
            "the {state:?} refusal leaves the LV untouched"
        );
        let (record, generation) = recorded_move(&fixture.state_path, volume);
        assert_eq!(
            record
                .expect("the durable record survives the refusal")
                .state,
            state,
            "the refusal must not rewrite the {state:?} record"
        );
        assert_eq!(generation, 1, "a refused {state:?} delete bumps nothing");
    }
}

/// Deleting a volume with a settled (`COMPLETE`) move removes the move
/// record in the same atomic save. A same-id recreate then starts at
/// generation 1 with a clean journal: the fresh move to the same target
/// must actually run a pvmove. A stale `COMPLETE` record answering the
/// request without a pvmove is exactly the bug this pins.
#[tokio::test]
async fn delete_with_a_settled_move_clears_the_move_record_and_prevents_a_false_complete() {
    let fixture = move_fixture();
    let volume = "vol-delete-settled-move";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    let id = volume_id(volume);
    let lv = lv_path(volume);

    let completed = fixture
        .provider
        .move_volume_backing(&id, &move_request("op-move-1", MOVE_TARGET_PV, 1))
        .await
        .expect("the first move completes");
    assert_eq!(completed.state, MoveVolumeBackingState::Complete);
    assert_eq!(completed.generation, 2);
    assert_eq!(pvmove_count(&fixture.runner), 1);

    fixture
        .provider
        .delete_volume(
            &id,
            &delete_request_with("op-delete-settled", 2, ErasurePolicy::Retain),
        )
        .await
        .expect("a settled move does not block the delete");

    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_none(), "the volume record is gone");
    assert!(
        durable.delete_intent(&id).is_none(),
        "the completed delete intent is gone"
    );
    assert!(
        durable.move_record(&id).is_none(),
        "the settled COMPLETE record must be removed with the volume, never inherited"
    );
    assert!(
        !fixture.world.lock().expect("world").lvs.contains_key(&lv),
        "the LV is removed"
    );

    // The same id and payload is a fresh volume: generation 1, and
    // lvcreate places it back on the VG's first PV (the move source).
    let recreated = fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("recreate the same volume id");
    assert_eq!(
        recreated.generation, 1,
        "a recreated volume starts a fresh generation history"
    );
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_SOURCE_PV.to_owned()],
        "lvcreate places the recreated LV on the VG's first PV"
    );

    // The same target is a real move for the recreated volume. A stale
    // COMPLETE record answered without a pvmove would be the bug.
    let before = pvmove_count(&fixture.runner);
    let response = fixture
        .provider
        .move_volume_backing(&id, &move_request("op-move-2", MOVE_TARGET_PV, 1))
        .await
        .expect("the recreated volume moves");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 2);
    assert_eq!(
        pvmove_count(&fixture.runner),
        before + 1,
        "a stale COMPLETE record would answer without running a pvmove"
    );
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the fresh record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 2);
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_TARGET_PV.to_owned()],
        "the fresh move really relocated the recreated volume"
    );
}

/// The final delete save fails before its rename: the durable file still
/// holds the volume, the delete intent and the `COMPLETE` move record.
/// The in-memory state is restored to that durable shape (the volume is
/// still inspectable, and the same provider can complete the retry), and
/// the error says the durable intent remains.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one failure/restore/retry narrative
async fn a_delete_completion_save_failure_restores_the_move_record() {
    let fixture = move_fixture();
    let volume = "vol-delete-final-fail-move";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    let id = volume_id(volume);
    let lv = lv_path(volume);

    let completed = fixture
        .provider
        .move_volume_backing(&id, &move_request("op-move-1", MOVE_TARGET_PV, 1))
        .await
        .expect("the move completes");
    assert_eq!(completed.state, MoveVolumeBackingState::Complete);
    assert_eq!(completed.generation, 2);

    // Delete saves: (1) the durable delete intent, (2) the final
    // volume/intent/move removal. Skip the first; fail the second
    // after its tmp write but before its rename, so the durable file
    // keeps the pre-delete shape.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterTmpWrite,
        1,
    );
    let error = fixture
        .provider
        .delete_volume(
            &id,
            &delete_request_with("op-delete-final-fail", 2, ErasurePolicy::Retain),
        )
        .await
        .expect_err("the final delete save fails before its rename");
    assert_eq!(error.code, ApiErrorCode::Internal, "{error}");
    assert!(
        error.detail.contains("durable delete intent"),
        "the error names the surviving authorization: {error}"
    );
    assert!(
        error.detail.contains("remains"),
        "the error says the durable intent remains: {error}"
    );

    // The LV was already removed before the failed save; the durable
    // state still authorizes completing the delete.
    assert!(
        !fixture.world.lock().expect("world").lvs.contains_key(&lv),
        "the LV was removed before the completion save failed"
    );
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_some(), "the volume is restored");
    let intent = durable
        .delete_intent(&id)
        .expect("the durable delete intent remains");
    assert_eq!(intent.expected_generation, 2);
    assert_eq!(intent.erasure_policy, ErasurePolicy::Retain);
    let record = durable
        .move_record(&id)
        .expect("the COMPLETE move record is restored");
    assert_eq!(record.state, MoveVolumeBackingState::Complete);
    assert_eq!(record.source_pv, MOVE_SOURCE_PV);
    assert_eq!(record.target_pv, MOVE_TARGET_PV);

    // The in-memory restore matches the durable file: inspect still
    // answers, so the same provider can run the authorized retry.
    let inspected = fixture
        .provider
        .inspect_volume(&id)
        .await
        .expect("the restored volume is served");
    assert_eq!(inspected.generation, 2);

    // A retry with a new operation id (same generation/policy) is the
    // authorized completion: volume, intent and move record all go.
    fixture
        .provider
        .delete_volume(
            &id,
            &delete_request_with("op-delete-final-fail-retry", 2, ErasurePolicy::Retain),
        )
        .await
        .expect("the retry completes the delete");
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_none(), "the volume record is gone");
    assert!(
        durable.delete_intent(&id).is_none(),
        "the delete intent is gone"
    );
    assert!(
        durable.move_record(&id).is_none(),
        "the move record is gone"
    );
}

/// Startup never completes a pending delete while the volume's move
/// record is unsettled: the LV may be the source (or one leg) of an
/// active pvmove, so discarding/removing it would operate on an
/// unknown placement. Startup fails typed before any destructive
/// command and before rewriting the durable file.
#[tokio::test]
async fn startup_refuses_to_complete_a_delete_with_an_unsettled_move() {
    let fixture = move_fixture();
    let volume = "vol-startup-delete-unsettled";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");
    let id = volume_id(volume);
    let lv = lv_path(volume);

    seed_move(
        &fixture.state_path,
        volume,
        MOVE_SOURCE_PV,
        MOVE_TARGET_PV,
        MoveVolumeBackingState::Copying,
    );
    common::seed_delete_intent(
        &fixture.state_path,
        volume,
        "op-seeded-delete",
        1,
        ErasurePolicy::Retain,
    );
    let bytes_before = std::fs::read(&fixture.state_path).expect("read the seeded state");

    let Err(error) = common::try_provider_from(&fixture.state_path, &fixture.world) else {
        panic!("startup unexpectedly completed a delete over an unsettled move");
    };
    assert_eq!(error.code, ApiErrorCode::Internal, "{error}");
    assert!(
        error.detail.contains("cannot complete"),
        "the refusal names the blocked delete: {error}"
    );
    assert!(
        error.detail.contains("placement is unsettled"),
        "the refusal names the unsettled placement: {error}"
    );

    // No destructive command ran and the durable file was not rewritten.
    assert!(
        fixture.world.lock().expect("world").lvs.contains_key(&lv),
        "startup must not remove an LV during an unsettled move"
    );
    let bytes_after = std::fs::read(&fixture.state_path).expect("read the state after refusal");
    assert_eq!(
        bytes_before, bytes_after,
        "startup fails before rewriting the durable state"
    );
    let durable = LvmState::load(&fixture.state_path).expect("durable state");
    assert!(durable.volume(&id).is_some(), "the volume survives");
    let intent = durable
        .delete_intent(&id)
        .expect("the delete intent survives");
    assert_eq!(intent.erasure_policy, ErasurePolicy::Retain);
    let record = durable.move_record(&id).expect("the move record survives");
    assert_eq!(record.state, MoveVolumeBackingState::Copying);
}

/// The `COPYING` save failing **after** its rename means the new
/// `COPYING` record is the durable anchor, even though the call returns
/// an error. The restart classifies from the world, lets the live move
/// land, and verifies the completion with exactly one generation bump.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one end-to-end failure/restart narrative
async fn a_copying_save_failure_after_rename_keeps_the_anchor_and_startup_completes() {
    let fixture = move_fixture();
    let volume = "vol-move-save-copying-rename";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // skip = 1 passes the PREPARING save and fails the COPYING save at
    // the directory-fsync boundary, after its rename: the durable file
    // already holds COPYING even though the call reports an error.
    fixture.provider.store_crash_hooks().arm_save_failure(
        STORE_LVM_STATE,
        StoreSavePoint::AfterRenameBeforeDirFsync,
        1,
    );
    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect_err("the COPYING save fails after its rename");
    assert_eq!(error.code, ApiErrorCode::Internal, "{error}");
    assert!(
        error
            .detail
            .contains("its COPYING record reported a save error but reached the durable state"),
        "the error names the landed COPYING record: {error}"
    );
    assert!(
        error.detail.contains("durable COPYING anchor remains"),
        "the error names the durable anchor: {error}"
    );
    assert!(
        error.detail.contains("AfterRenameBeforeDirFsync"),
        "the error identifies the post-rename failure boundary: {error}"
    );

    // The COPYING save's rename landed: the durable record is COPYING
    // while the world still shows the live move at its source.
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the durable COPYING anchor").state,
        MoveVolumeBackingState::Copying
    );
    assert_eq!(generation, 1);
    assert_eq!(fixture.world.lock().expect("world").moves.len(), 1);
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_SOURCE_PV.to_owned()]
    );
    assert_eq!(pvmove_count(&fixture.runner), 1);

    // The restart observes the live move, lets it land, and completes
    // the verified relocation with exactly one bump and no new pvmove.
    let restarted =
        provider_from_with_timing(&fixture.state_path, &fixture.world, default_move_timing());
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the completed record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(
        generation, 2,
        "exactly one generation bump across the restart"
    );
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_TARGET_PV.to_owned()]
    );
    assert!(fixture.world.lock().expect("world").moves.is_empty());
    assert_eq!(pvmove_count(&fixture.runner), 1, "no re-drive pvmove");

    // The completed record answers a same-target re-issue without a
    // second move or bump.
    let response = restarted
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-after", MOVE_TARGET_PV, 2),
        )
        .await
        .expect("the completed record answers");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 2);
    assert_eq!(pvmove_count(&fixture.runner), 1);
}

// ---------------------------------------------------------------------------
// The unqualified-provider trait default (the LVM side of the pin)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_trait_default_refuses_when_the_capability_is_unadvertised() {
    // The generic fixture's provider advertises the capability; the
    // DEFAULT trait implementation's refusal is pinned here through a
    // provider that cannot move: the FakeProvider in volvisor-api's
    // route tests carries that pin. What this file pins instead is
    // that the LVM provider DOES advertise the capability it serves.
    let fixture = fixture();
    assert!(
        fixture
            .provider
            .capabilities()
            .contains(volvisor_types::Capability::SameVgExtentMove),
        "the LVM provider advertises the scope it serves"
    );
    assert!(
        !fixture
            .provider
            .capabilities()
            .contains(volvisor_types::Capability::SameHostLiveBackingMove),
        "the QSD mirror/pivot path is advertised nowhere (ADR-0006)"
    );
}

// ---------------------------------------------------------------------------
// The honest-answer races the tightened verification closes
// ---------------------------------------------------------------------------

/// The re-attach race: the move lands during the re-attach's own
/// phase-2 observation, so the phase-4 pvmove re-run refuses — real
/// LVM answers `No data to move` (exit 5), the verified 2.03.16
/// shape — with the relocation verifiably done. The honest answer is
/// `COMPLETE` (the tail classifies from a fresh observation before
/// answering), never a start failure over a finished move.
#[tokio::test]
async fn a_re_attach_whose_move_lands_during_observation_completes() {
    let fixture = move_fixture();
    let volume = "vol-move-landed-race";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // Hold the first move: the window expires and answers COPYING
    // with the move at 0 percent.
    fixture.world.lock().expect("world").hold_moves = true;
    let first = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect("the first move answers honestly");
    assert_eq!(first.state, MoveVolumeBackingState::Copying);

    // Position the held move exactly one observation from landing,
    // then release: the re-attach's first lvs query lands it.
    {
        let mut world = fixture.world.lock().expect("world");
        let advance = world.move_advance_percent;
        let moving = world
            .moves
            .get_mut(&lv_path(volume))
            .expect("the held move");
        moving.percent = 100 - advance;
        world.hold_moves = false;
    }

    let response = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-2", MOVE_TARGET_PV, 1),
        )
        .await
        .expect("the re-attach completes the landed move");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 2, "one bump across both operations");
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 2);
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_TARGET_PV.to_owned()],
        "the extents really relocated"
    );
}

/// A foreign relocation is never this move's completion: while the
/// daemon is not looking, the extents leave the source PV for a
/// third PV of the VG (not the record's target). The source is
/// freed, but the named target does not hold the extents — the
/// re-attach parks `IN_DOUBT` naming the unknown placement, the
/// generation never bumps, and the foreign placement is untouched.
#[tokio::test]
async fn a_foreign_relocation_off_target_parks_in_doubt() {
    let fixture = move_fixture();
    let volume = "vol-move-foreign";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    fixture.world.lock().expect("world").hold_moves = true;
    let first = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect("the first move answers honestly");
    assert_eq!(first.state, MoveVolumeBackingState::Copying);

    // The out-of-band actor: the move is gone and the extents sit on
    // a third PV of the VG.
    {
        let mut world = fixture.world.lock().expect("world");
        world.add_pv_to_vg("/dev/pv-c", CLAIMED_VG, common::POOL_BYTES);
        world.moves.remove(&lv_path(volume));
        world.place_lv_on(&lv_path(volume), "/dev/pv-c");
        world.hold_moves = false;
    }

    let response = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-2", MOVE_TARGET_PV, 1),
        )
        .await
        .expect("the re-attach parks instead of guessing");
    assert_eq!(response.state, MoveVolumeBackingState::InDoubt);
    assert!(
        response
            .detail
            .as_deref()
            .expect("the park reason")
            .contains("neither the source PV"),
        "the park names the unknown placement: {response:?}"
    );
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the record").state,
        MoveVolumeBackingState::InDoubt
    );
    assert_eq!(generation, 1, "no bump for an unverified outcome");
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec!["/dev/pv-c".to_owned()],
        "the foreign placement is untouched"
    );
}

// ---------------------------------------------------------------------------
// The concurrency and resolution discipline (the review findings'
// pins): one bump under overlapping completions (including the
// transient-verification corner), the IN_DOUBT rollback exit, the
// unobservable-LV bound, the foreign-source scan
// ---------------------------------------------------------------------------

/// Two consumer operations supervising the **same** in-flight move
/// (the second re-attaches under its own operation id) both verify
/// the landed relocation and both answer `COMPLETE` — and the
/// generation bumps **exactly once**. Without the idempotency gate
/// in `finish_complete`, whichever supervision completes second
/// would bump again over the first's durable `COMPLETE` record.
#[tokio::test]
async fn two_overlapping_supervisions_of_one_move_bump_the_generation_exactly_once() {
    let fixture = long_window_move_fixture();
    let volume = "vol-move-two-supervisions";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // Hold the move so both operations attach and supervise before
    // anything lands.
    fixture.world.lock().expect("world").hold_moves = true;
    let first = tokio::spawn({
        let provider = Arc::clone(&fixture.provider);
        let volume = volume_id(volume);
        async move {
            provider
                .move_volume_backing(&volume, &move_request("op-move-1", MOVE_TARGET_PV, 1))
                .await
        }
    });
    // The first operation reached its supervision once its record is
    // durably COPYING.
    wait_for_record(&fixture.state_path, volume, |record| {
        record.state == MoveVolumeBackingState::Copying
    })
    .await;

    // The second operation re-attaches to the same in-flight move
    // under its own operation id.
    let second = tokio::spawn({
        let provider = Arc::clone(&fixture.provider);
        let volume = volume_id(volume);
        async move {
            provider
                .move_volume_backing(&volume, &move_request("op-move-2", MOVE_TARGET_PV, 1))
                .await
        }
    });
    // The re-attach reached its supervision once its phase-4 COPYING
    // save refreshed the record's operation id.
    wait_for_record(&fixture.state_path, volume, |record| {
        record.operation_id.as_str() == "op-move-2"
    })
    .await;

    // Release the world: both supervisions observe the landing, both
    // verify the same completed relocation — one bump, two honest
    // COMPLETE answers.
    fixture.world.lock().expect("world").hold_moves = false;
    let first = first
        .await
        .expect("join first")
        .expect("the first completes");
    let second = second
        .await
        .expect("join second")
        .expect("the re-attached second completes");
    assert_eq!(first.state, MoveVolumeBackingState::Complete);
    assert_eq!(second.state, MoveVolumeBackingState::Complete);
    assert_eq!(first.generation, 2);
    assert_eq!(second.generation, 2);
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    let record = record.expect("the record");
    assert_eq!(record.state, MoveVolumeBackingState::Complete);
    assert_eq!(record.source_pv, MOVE_SOURCE_PV);
    assert_eq!(record.target_pv, MOVE_TARGET_PV);
    assert_eq!(
        generation, 2,
        "two overlapping supervisions, exactly one generation bump"
    );
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_TARGET_PV.to_owned()]
    );
    // Two pvmove starts — the original and the re-attach's attach —
    // never a third.
    assert_eq!(pvmove_count(&fixture.runner), 2);
}

/// The retry reconcile's stale-snapshot race: the pass snapshots a
/// `COPYING` record, a concurrent re-attach drive completes the
/// record behind the snapshot's back, and the pass's own
/// `finish_complete` then runs against the already-completed live
/// record. The durable re-check under the state lock answers the
/// stored `COMPLETE` record — the world-verified completion stays
/// and the bump is **not** repeated. (Without the gate, the pass
/// would bump a second time over the drive's completion.)
///
/// The interleaving is choreographed deterministically: the
/// reconcile thread's first `lvs` observation parks (after its
/// snapshot, before its classification) until the test releases it,
/// so the drive provably completes between the snapshot and the
/// pass's completion attempt.
#[tokio::test]
async fn a_reconcile_snapshot_from_before_the_completion_never_bumps_again() {
    let fixture = move_fixture();
    let volume = "vol-move-stale-snapshot";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // The dead incarnation's durable facts: a COPYING record and a
    // live, held pvmove (the crash shape between the pvmove start
    // and its landing).
    seed_move(
        &fixture.state_path,
        volume,
        MOVE_SOURCE_PV,
        MOVE_TARGET_PV,
        MoveVolumeBackingState::Copying,
    );
    {
        let mut world = fixture.world.lock().expect("world");
        world.moves.insert(
            lv_path(volume),
            common::FakeMove {
                source: MOVE_SOURCE_PV.to_owned(),
                target: MOVE_TARGET_PV.to_owned(),
                percent: 0,
            },
        );
        world.hold_moves = true;
    }

    // The retry reconcile runs on its own thread against a provider
    // whose runner parks that thread's first `lvs` observation.
    let pause = ReconcilePause::new();
    let (provider, runner) = gated_provider(&fixture.state_path, &fixture.world, &pause);
    let (thread_tx, thread_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    let (report_tx, report_rx) = mpsc::channel();
    let reconcile = {
        let provider = Arc::clone(&provider);
        thread::spawn(move || {
            thread_tx
                .send(thread::current().id())
                .expect("send the reconcile thread id");
            go_rx.recv().expect("receive the go signal");
            report_tx
                .send(provider.move_reconcile_pass())
                .expect("send the pass report");
        })
    };
    // The pass snapshots COPYING, then parks at its world
    // observation — before it classifies anything.
    pause.arm(thread_rx.recv().expect("the reconcile thread id"));
    go_tx.send(()).expect("release the reconcile thread");
    pause.wait_until_paused();

    // The re-attach drive completes the move behind the snapshot:
    // attach to the held mirror, supervise, release the world, land,
    // verify, ONE bump.
    let drive = tokio::spawn({
        let provider = Arc::clone(&provider);
        let volume = volume_id(volume);
        async move {
            provider
                .move_volume_backing(&volume, &move_request("op-drive", MOVE_TARGET_PV, 1))
                .await
        }
    });
    wait_for_record(&fixture.state_path, volume, |record| {
        record.operation_id.as_str() == "op-drive"
    })
    .await;
    fixture.world.lock().expect("world").hold_moves = false;
    let response = drive
        .await
        .expect("join the drive")
        .expect("the drive completes");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 2);

    // The reconcile thread resumes: its snapshot said COPYING, the
    // world now proves the relocation, and the LIVE record is
    // already COMPLETE — the pass classifies it completed without a
    // second bump.
    pause.release();
    reconcile.join().expect("join the reconcile thread");
    let report = report_rx
        .recv()
        .expect("the pass report")
        .expect("the pass");
    assert_eq!(report.completed, vec![volume_id(volume)]);
    assert_eq!(report.parked_in_doubt, Vec::new());
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    let record = record.expect("the record");
    assert_eq!(record.state, MoveVolumeBackingState::Complete);
    assert_eq!(record.source_pv, MOVE_SOURCE_PV);
    assert_eq!(record.target_pv, MOVE_TARGET_PV);
    assert_eq!(
        generation, 2,
        "the drive's completion bumped once; the stale-snapshot pass bumped nothing"
    );
    // The pass started no pvmove of its own: the drive's attach is
    // the only one.
    assert_eq!(pvmove_count(&runner), 1);
}

/// The observe-Err corner of the completion's once-only bump: a
/// competing finish path — the retry reconcile over a
/// pre-completion snapshot — whose verification observe fails
/// **transiently** must not park over the durably stored
/// `COMPLETE`. The park helper answers the stored record (the same
/// discipline as the idempotency gate), the landed completion never
/// regresses to `IN_DOUBT`, and the next pass re-completes nothing
/// and bumps nothing. Without the guard, the clobbered record would
/// re-complete on the next pass and bump the generation a second
/// time.
#[tokio::test]
async fn a_transient_verification_failure_never_parks_over_a_stored_complete() {
    let fixture = move_fixture();
    let volume = "vol-move-transient-verify-flake";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // The dead incarnation's durable facts: a COPYING record and a
    // live, held pvmove (the crash shape between the pvmove start
    // and its landing).
    seed_move(
        &fixture.state_path,
        volume,
        MOVE_SOURCE_PV,
        MOVE_TARGET_PV,
        MoveVolumeBackingState::Copying,
    );
    {
        let mut world = fixture.world.lock().expect("world");
        world.moves.insert(
            lv_path(volume),
            common::FakeMove {
                source: MOVE_SOURCE_PV.to_owned(),
                target: MOVE_TARGET_PV.to_owned(),
                percent: 0,
            },
        );
        world.hold_moves = true;
    }

    // The competing finish path: the retry reconcile on its own
    // thread against a provider whose runner parks that thread's
    // first `lvs` (its own world observation, after its COPYING
    // snapshot) and fails its **second** `lvs` exactly once — the
    // finishing verification's transient observe failure, injected
    // through the scripted-world seam.
    let flake = FinishingFlake::new();
    let (provider, runner) = flaked_provider(&fixture.state_path, &fixture.world, &flake);
    let (thread_tx, thread_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    let (report_tx, report_rx) = mpsc::channel();
    let reconcile = {
        let provider = Arc::clone(&provider);
        thread::spawn(move || {
            thread_tx
                .send(thread::current().id())
                .expect("send the reconcile thread id");
            go_rx.recv().expect("receive the go signal");
            report_tx
                .send(provider.move_reconcile_pass())
                .expect("send the pass report");
        })
    };
    flake.arm(thread_rx.recv().expect("the reconcile thread id"));
    go_tx.send(()).expect("release the reconcile thread");
    flake.wait_until_paused();

    // The re-attach drive completes the move behind the snapshot:
    // attach to the held mirror, supervise, release the world, land,
    // verify, ONE bump — the stored COMPLETE is durable.
    let drive = tokio::spawn({
        let provider = Arc::clone(&provider);
        let volume = volume_id(volume);
        async move {
            provider
                .move_volume_backing(&volume, &move_request("op-drive", MOVE_TARGET_PV, 1))
                .await
        }
    });
    wait_for_record(&fixture.state_path, volume, |record| {
        record.operation_id.as_str() == "op-drive"
    })
    .await;
    fixture.world.lock().expect("world").hold_moves = false;
    let response = drive
        .await
        .expect("join the drive")
        .expect("the drive completes");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 2);

    // The reconcile thread resumes: its own observation sees the
    // landed relocation, its finishing verification observe fails
    // transiently — and the stored COMPLETE is the durable truth,
    // answered, never parked over.
    flake.release();
    reconcile.join().expect("join the reconcile thread");
    let report = report_rx
        .recv()
        .expect("the pass report")
        .expect("the pass");
    assert_eq!(report.completed, vec![volume_id(volume)]);
    assert_eq!(report.parked_in_doubt, Vec::new());
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    let record = record.expect("the record");
    assert_eq!(
        record.state,
        MoveVolumeBackingState::Complete,
        "the transient verification failure never regressed the landed completion to IN_DOUBT"
    );
    assert_eq!(
        generation, 2,
        "the landed completion's bump is the only one — no clobber, no re-completion"
    );

    // The failure was transient: the world is observable again, and
    // the next pass completes nothing over the stored truth.
    let report = provider.move_reconcile_pass().expect("the next pass");
    assert_eq!(report.completed, Vec::new());
    assert_eq!(report.parked_in_doubt, Vec::new());
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("the record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 2, "no second bump on the next pass");
    // The passes started no pvmove of their own: the drive's attach
    // is the only one.
    assert_eq!(pvmove_count(&runner), 1);
}

/// A parked `IN_DOUBT` record whose world verifiably shows the
/// pre-move placement (exactly the record's source PV, no move
/// active — an out-of-band `pvmove --abort` that completed the
/// rollback) resolves `FAILED` on the retry pass: the known outcome,
/// journaled with the typed reason, generation untouched — and a
/// fresh operation is admitted, driving a new move that completes
/// with exactly one bump.
#[tokio::test]
async fn an_in_doubt_record_whose_extents_verifiably_returned_to_the_source_resolves_failed() {
    let fixture = move_fixture();
    let volume = "vol-move-resolve-failed";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // The park: the record is IN_DOUBT and the world is the honest
    // mixed-placement abort (segments on both PVs — genuinely
    // unresolvable by observation), so the constructor's startup
    // pass leaves it parked.
    seed_move(
        &fixture.state_path,
        volume,
        MOVE_SOURCE_PV,
        MOVE_TARGET_PV,
        MoveVolumeBackingState::InDoubt,
    );
    fixture.world.lock().expect("world").lv_devices.insert(
        lv_path(volume),
        vec![MOVE_SOURCE_PV.to_owned(), MOVE_TARGET_PV.to_owned()],
    );
    let restarted = provider_from(&fixture.state_path, &fixture.world);
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("record").state,
        MoveVolumeBackingState::InDoubt,
        "the mixed-placement park survives the startup pass"
    );

    // The world changes behind the park: the rollback completes (the
    // extents verifiably sit on exactly the source PV again). The
    // retry pass resolves the record FAILED with the typed reason —
    // the relocation verifiably did not happen — bumping nothing.
    fixture
        .world
        .lock()
        .expect("world")
        .lv_devices
        .insert(lv_path(volume), vec![MOVE_SOURCE_PV.to_owned()]);
    let report = restarted.move_reconcile_pass().expect("the pass");
    assert_eq!(report.resolved_failed, vec![volume_id(volume)]);
    assert_eq!(report.parked_in_doubt, Vec::new());
    assert_eq!(report.completed, Vec::new());
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    let record = record.expect("the record");
    assert_eq!(record.state, MoveVolumeBackingState::Failed);
    assert!(
        record
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("verifiably did not happen"),
        "the resolution names the known outcome: {:?}",
        record.detail
    );
    assert_eq!(generation, 1, "a resolution bumps nothing");

    // The exit is real: a fresh operation is admitted under a new
    // operation id, the FAILED record is superseded at the PREPARING
    // journal write, and the fresh move completes with exactly one
    // bump.
    let response = restarted
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-fresh", MOVE_TARGET_PV, 1),
        )
        .await
        .expect("the fresh drive is admitted");
    assert_eq!(response.state, MoveVolumeBackingState::Complete);
    assert_eq!(response.generation, 2);
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 2);
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_TARGET_PV.to_owned()]
    );
}

/// The mixed-placement abort stays parked: a record whose extents
/// verifiably sit on **both** PVs (the honest shape of LVM 2.03.x's
/// non-atomic `pvmove --abort`) is genuinely unresolvable by
/// observation alone — the reconcile pass neither resolves it,
/// re-parks it, nor bumps anything, and a fresh operation still
/// refuses typed naming the park.
#[tokio::test]
async fn an_in_doubt_record_in_a_mixed_placement_world_stays_parked() {
    let fixture = move_fixture();
    let volume = "vol-move-mixed-park";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    seed_move(
        &fixture.state_path,
        volume,
        MOVE_SOURCE_PV,
        MOVE_TARGET_PV,
        MoveVolumeBackingState::InDoubt,
    );
    fixture.world.lock().expect("world").lv_devices.insert(
        lv_path(volume),
        vec![MOVE_SOURCE_PV.to_owned(), MOVE_TARGET_PV.to_owned()],
    );

    let restarted = provider_from(&fixture.state_path, &fixture.world);
    let report = restarted.move_reconcile_pass().expect("the pass");
    assert_eq!(
        report.resolved_failed,
        Vec::new(),
        "the mixed placement resolves nothing"
    );
    assert_eq!(report.parked_in_doubt, Vec::new());
    assert_eq!(report.completed, Vec::new());
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    let record = record.expect("the record");
    assert_eq!(record.state, MoveVolumeBackingState::InDoubt);
    assert_eq!(generation, 1, "nothing was freed");

    // Fail-closed for new work: the park still refuses typed.
    let error = restarted
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-2", MOVE_TARGET_PV, 1),
        )
        .await
        .expect_err("still parked");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert!(
        error.detail.contains("IN_DOUBT"),
        "the refusal names the park: {error}"
    );
}

/// The unobservable-LV bound (the `lvremove`-limbo fix): a moving
/// record whose LV cannot be observed is retried honestly below the
/// bound, parks `IN_DOUBT` with a typed reason at it (surfaced,
/// never silently spinning), and — once the world becomes observable
/// again with the move gone and the extents verifiably back on the
/// source — resolves `FAILED` by the normal arms. The park is an
/// exit from limbo, not a new dead end.
#[tokio::test]
async fn an_lv_unobservable_across_the_bound_parks_in_doubt_typed() {
    let fixture = move_fixture();
    let volume = "vol-move-unobservable-bound";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // The in-flight move (held — the daemon supervises it) whose
    // world then goes dark: the dead incarnation's COPYING record
    // plus the live mirror, then every lvs observation fails (the
    // out-of-band `lvremove` shape).
    seed_move(
        &fixture.state_path,
        volume,
        MOVE_SOURCE_PV,
        MOVE_TARGET_PV,
        MoveVolumeBackingState::Copying,
    );
    {
        let mut world = fixture.world.lock().expect("world");
        world.moves.insert(
            lv_path(volume),
            common::FakeMove {
                source: MOVE_SOURCE_PV.to_owned(),
                target: MOVE_TARGET_PV.to_owned(),
                percent: 0,
            },
        );
        world.hold_moves = true;
    }
    let restarted = provider_from(&fixture.state_path, &fixture.world);
    fixture.world.lock().expect("world").fail_lvs = true;

    // Below the bound (failures one and two): the record is left
    // untouched, each pass reports the honest unknown.
    for failure in 1..=2 {
        let report = restarted.move_reconcile_pass().expect("a below-bound pass");
        assert_eq!(
            report.unobservable,
            vec![volume_id(volume)],
            "consecutive failure {failure}"
        );
        assert_eq!(report.parked_unobservable, Vec::new());
    }
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("record").state,
        MoveVolumeBackingState::Copying,
        "below the bound the record is left untouched"
    );

    // At the bound (the third consecutive unobservable pass): the
    // record parks IN_DOUBT with the typed reason.
    let report = restarted.move_reconcile_pass().expect("the third failure");
    assert_eq!(report.parked_unobservable, vec![volume_id(volume)]);
    assert_eq!(report.unobservable, Vec::new());
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    let record = record.expect("the record");
    assert_eq!(record.state, MoveVolumeBackingState::InDoubt);
    assert!(
        record
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("unobservable"),
        "the park names the unobservable LV: {:?}",
        record.detail
    );
    assert_eq!(generation, 1, "an unobservable world bumps nothing");

    // The park surfaces: a fresh operation refuses typed carrying
    // the reason (the record is queryable, not spinning).
    let error = restarted
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-2", MOVE_TARGET_PV, 1),
        )
        .await
        .expect_err("the parked record refuses");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert!(
        error.detail.contains("unobservable"),
        "the refusal carries the typed reason: {error}"
    );

    // The world becomes observable again with the move gone and the
    // extents verifiably back on the source (the operator resolved
    // the vanished LV): the next pass resolves FAILED — the
    // convergence the park enables, never a dead end.
    {
        let mut world = fixture.world.lock().expect("world");
        world.fail_lvs = false;
        world.moves.clear();
    }
    let report = restarted
        .move_reconcile_pass()
        .expect("the observable pass");
    assert_eq!(report.resolved_failed, vec![volume_id(volume)]);
    let (record, _) = recorded_move(&fixture.state_path, volume);
    assert_eq!(
        record.expect("record").state,
        MoveVolumeBackingState::Failed
    );
}

/// The fresh-start foreign-source scan: an out-of-band (unrecorded)
/// pvmove already active on the source PV would swallow volvisor's
/// scoped pvmove — verified LVM 2.03.16 behavior: the second move
/// attaches, its arguments are IGNORED, it exits 0, and the LV never
/// moves. The fresh start is refused typed from the world itself
/// (the hidden `pvmove*` mirror rows name the PVs they involve),
/// before any record exists and before any pvmove runs.
#[tokio::test]
async fn a_foreign_pvmove_on_the_source_pv_refuses_the_fresh_start_typed() {
    let fixture = move_fixture();
    let volume = "vol-move-foreign-source";
    fixture
        .provider
        .create_volume(&fixture_create_request(volume, 64 * MIB))
        .await
        .expect("create");

    // A foreign, unrecorded pvmove evacuates another LV of the VG
    // off the same source PV (held, so it stays observable through
    // the scan — nothing of volvisor's knows about it).
    {
        let mut world = fixture.world.lock().expect("world");
        let other = format!("{CLAIMED_VG}/vol-move-other-lv");
        world.lvs.insert(other.clone(), 64 * MIB);
        world
            .lv_devices
            .insert(other.clone(), vec![MOVE_SOURCE_PV.to_owned()]);
        world.moves.insert(
            other,
            common::FakeMove {
                source: MOVE_SOURCE_PV.to_owned(),
                target: "/dev/pv-c".to_owned(),
                percent: 0,
            },
        );
        world.hold_moves = true;
    }

    let error = fixture
        .provider
        .move_volume_backing(
            &volume_id(volume),
            &move_request("op-move-1", MOVE_TARGET_PV, 1),
        )
        .await
        .expect_err("the fresh start is refused typed");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert!(
        error
            .detail
            .contains("a foreign pvmove is already active on source PV"),
        "the refusal names the foreign move on the source: {error}"
    );

    // Refused before any state exists: no record, no pvmove, the
    // volume's placement untouched.
    let (record, generation) = recorded_move(&fixture.state_path, volume);
    assert!(record.is_none(), "a phase-2 refusal journals nothing");
    assert_eq!(generation, 1);
    assert_eq!(pvmove_count(&fixture.runner), 0, "no pvmove was started");
    assert_eq!(
        placement_of(&fixture.world, volume),
        vec![MOVE_SOURCE_PV.to_owned()]
    );

    // Argv-exact: the scan ran through the hidden-mirror query —
    // `-a` plus the scan's column set (the only place the PVs an
    // active move involves are lvs-observable).
    let scan = fixture
        .runner
        .invocations()
        .into_iter()
        .find(|invocation| {
            invocation.program == "lvs" && invocation.args.contains(&"-a".to_owned())
        })
        .expect("the foreign-move scan query");
    assert_eq!(
        scan.args,
        vec![
            "-a".to_owned(),
            "--reportformat".to_owned(),
            "json".to_owned(),
            "--units".to_owned(),
            "b".to_owned(),
            "--nosuffix".to_owned(),
            "-o".to_owned(),
            "vg_name,lv_name,devices".to_owned(),
        ],
        "the hidden-mirror scan argv"
    );
}

// ---------------------------------------------------------------------------
// The concurrency rigs' helpers
// ---------------------------------------------------------------------------

/// Wait (bounded) until the volume's durable move record satisfies
/// `predicate` — the state-file poll the concurrent tests use to
/// observe a spawned drive's progress without touching provider
/// internals.
async fn wait_for_record(
    state_path: &std::path::Path,
    volume: &str,
    predicate: impl Fn(&MoveRecord) -> bool,
) {
    for _ in 0..2_500 {
        if let Some(record) = recorded_move(state_path, volume).0 {
            if predicate(&record) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let (record, _) = recorded_move(state_path, volume);
    panic!("the durable record never satisfied the predicate (last: {record:?})");
}

/// How many `pvmove` invocations a runner recorded.
fn pvmove_count(runner: &FakeRunner) -> usize {
    runner
        .invocations()
        .iter()
        .filter(|invocation| invocation.program == "pvmove")
        .count()
}

/// The reconcile-race choreography: a runner wrapper that parks the
/// reconcile thread's `lvs` observations until the test releases
/// them. The parked point is **after** the pass's record snapshot
/// (the snapshot takes no world observation) and **before** its
/// classification — the deterministic shape of the stale-snapshot
/// race: a concurrent drive completes the record behind the
/// snapshot's back while the pass is held.
#[derive(Clone)]
struct ReconcilePause {
    inner: Arc<(Mutex<PauseState>, Condvar)>,
}

struct PauseState {
    /// The thread whose `lvs` observations park (none until armed).
    thread: Option<thread::ThreadId>,
    /// The reconcile thread is parked at its observation.
    paused: bool,
    /// The test's release signal.
    released: bool,
}

impl ReconcilePause {
    fn new() -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(PauseState {
                    thread: None,
                    paused: false,
                    released: false,
                }),
                Condvar::new(),
            )),
        }
    }

    /// Arm the pause for `thread` (called once its id is known).
    fn arm(&self, thread: thread::ThreadId) {
        let mut state = self.inner.0.lock().expect("pause state");
        state.thread = Some(thread);
        self.inner.1.notify_all();
    }

    /// Park the armed thread's `lvs` observations (the runner calls
    /// this before touching the world, so the test's drives keep
    /// running while the reconcile thread is held).
    fn pause_reconcile_thread(&self, program: &str) {
        let mut state = self.inner.0.lock().expect("pause state");
        if program != "lvs" || state.thread != Some(thread::current().id()) {
            return;
        }
        state.paused = true;
        self.inner.1.notify_all();
        while !state.released {
            state = self.inner.1.wait(state).expect("pause wait");
        }
    }

    /// Block (bounded) until the armed thread is parked.
    fn wait_until_paused(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut state = self.inner.0.lock().expect("pause state");
        while !state.paused {
            assert!(
                Instant::now() < deadline,
                "the reconcile thread never reached its parked observation"
            );
            let (guard, _) = self
                .inner
                .1
                .wait_timeout(state, Duration::from_millis(50))
                .expect("pause wait");
            state = guard;
        }
    }

    /// Release the parked thread.
    fn release(&self) {
        let mut state = self.inner.0.lock().expect("pause state");
        state.released = true;
        self.inner.1.notify_all();
    }
}

/// A provider over the fixture's state file and world whose runner
/// wraps the kit's script with the [`ReconcilePause`] choreography
/// (plus the runner itself, for invocation assertions). The timing
/// is the long-window shape: the drives under test must supervise
/// well past the choreography's waits.
fn gated_provider(
    state_path: &std::path::Path,
    world: &Arc<Mutex<common::FakeLvm>>,
    pause: &ReconcilePause,
) -> (Arc<LvmProvider>, Arc<FakeRunner>) {
    let runner = {
        let world = Arc::clone(world);
        let pause = pause.clone();
        Arc::new(FakeRunner::with_closure(move |program, args| {
            pause.pause_reconcile_thread(program);
            let mut world = world.lock().ok()?;
            common::script(&mut world, program, args)
        }))
    };
    let provider = LvmProvider::new(
        Arc::clone(&runner) as Arc<dyn volvisor_provider::CommandRunner>,
        state_path.to_path_buf(),
        common::leak_tempdir(),
        common::VG_PREFIX.to_owned(),
        common::AUTH_TOKEN.to_owned(),
    )
    .map(|provider| {
        Arc::new(
            provider.with_move_timing(volvisor_lvm::provider::MoveTiming {
                poll_interval: Duration::from_millis(5),
                supervision_window: Duration::from_secs(30),
            }),
        )
    })
    .expect("provider construction");
    (provider, runner)
}

/// The competing-finish choreography: the armed thread's first
/// `lvs` (the pass's own world observation, after its record
/// snapshot) parks until released — the [`ReconcilePause`] stage —
/// and its **second** `lvs`, the finishing path's verification
/// observe, fails exactly once through the scripted-world seam
/// (`fail_lvs`), restored immediately after: a transient observe
/// failure, not an outage. The deterministic shape of the observe-Err
/// corner: a completion lands durably behind the pass's snapshot,
/// and the resumed finishing path cannot observe the world it is
/// about to judge.
#[derive(Clone)]
struct FinishingFlake {
    pause: ReconcilePause,
    flake: Arc<Mutex<Option<(thread::ThreadId, u32)>>>,
}

impl FinishingFlake {
    fn new() -> Self {
        Self {
            pause: ReconcilePause::new(),
            flake: Arc::new(Mutex::new(None)),
        }
    }

    /// Arm both stages for `thread` (called once its id is known).
    fn arm(&self, thread: thread::ThreadId) {
        self.pause.arm(thread);
        *self.flake.lock().expect("flake state") = Some((thread, 0));
    }

    /// Park the armed thread's first `lvs` (the pause stage), then
    /// report whether this call is the transient failure (the armed
    /// thread's second `lvs`).
    fn intercept(&self, program: &str) -> bool {
        self.pause.pause_reconcile_thread(program);
        if program != "lvs" {
            return false;
        }
        let mut flake = self.flake.lock().expect("flake state");
        match flake.as_mut() {
            Some((thread, count)) if *thread == thread::current().id() => {
                *count += 1;
                *count == 2
            }
            _ => false,
        }
    }

    /// Block (bounded) until the armed thread is parked.
    fn wait_until_paused(&self) {
        self.pause.wait_until_paused();
    }

    /// Release the parked thread.
    fn release(&self) {
        self.pause.release();
    }
}

/// A provider over the fixture's state file and world whose runner
/// wraps the kit's script with the [`FinishingFlake`] choreography
/// (plus the runner itself, for invocation assertions). The timing
/// is the long-window shape: the drive under test must supervise
/// well past the choreography's waits.
fn flaked_provider(
    state_path: &std::path::Path,
    world: &Arc<Mutex<common::FakeLvm>>,
    flake: &FinishingFlake,
) -> (Arc<LvmProvider>, Arc<FakeRunner>) {
    let runner = {
        let world = Arc::clone(world);
        let flake = flake.clone();
        Arc::new(FakeRunner::with_closure(move |program, args| {
            let fail_this = flake.intercept(program);
            let mut world = world.lock().ok()?;
            if fail_this {
                // The transient observe failure, injected through
                // the scripted-world seam and restored immediately
                // after the one call.
                world.fail_lvs = true;
            }
            let output = common::script(&mut world, program, args);
            if fail_this {
                world.fail_lvs = false;
            }
            output
        }))
    };
    let provider = LvmProvider::new(
        Arc::clone(&runner) as Arc<dyn volvisor_provider::CommandRunner>,
        state_path.to_path_buf(),
        common::leak_tempdir(),
        common::VG_PREFIX.to_owned(),
        common::AUTH_TOKEN.to_owned(),
    )
    .map(|provider| {
        Arc::new(
            provider.with_move_timing(volvisor_lvm::provider::MoveTiming {
                poll_interval: Duration::from_millis(5),
                supervision_window: Duration::from_secs(30),
            }),
        )
    })
    .expect("provider construction");
    (provider, runner)
}
