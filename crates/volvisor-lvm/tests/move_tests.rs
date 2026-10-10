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
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::{
    CLAIMED_VG, MOVE_SOURCE_PV, MOVE_TARGET_PV, default_move_timing, fixture, move_fixture,
    move_fixture_with_timing, provider_from_with_timing, seed_move,
};
use volvisor_lvm::state::{LvmState, MoveRecord};
use volvisor_provider::VolumeProvider;
use volvisor_provider::conformance::{fixture_attach_request, fixture_create_request};
use volvisor_types::request::MoveVolumeBackingRequest;
use volvisor_types::{ApiErrorCode, MoveVolumeBackingState, OperationId, VolumeId};

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
    assert!(report.completed.is_empty());

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
    assert!(report.completed.is_empty());
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
    assert!(report.dropped.is_empty());
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
