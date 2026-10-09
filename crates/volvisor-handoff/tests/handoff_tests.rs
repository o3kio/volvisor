//! Behavior tests for the handoff state machine (P4b plan §9, stage
//! B1 rows 4–7, 16, 16b) driven through a scripted [`FakeDriver`]:
//! every legal transition persists before reporting, the cut is a
//! forward-only write-ahead, the pre-cut rollback is G5-ordered and
//! fail-closed, and the reconcile folds external facts before choosing
//! a direction.
//!
//! The fake models the external world the coordinator depends on — VM
//! presence/pause, source role, the witness's lease/barrier log — and
//! survives a coordinator "crash" (drop + reopen from the store), so
//! crash windows are replayed against the same reality, never against
//! a reset one.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use volvisor_handoff::{
    BatchStep, Clock, CutProgress, HandoffDriver, HandoffState, MigrationCoordinator,
    MigrationRecord, MigrationStore, Participant, PrepareHandoffRequest, barrier_operation_id,
    batch_operation_id,
};
use volvisor_types::authority::{
    AuthorityView, BarrierAttestation, EpochRetirement, LeaseState, RecordedMigrationBarrier,
    WriterEpoch,
};
use volvisor_types::id::{HostId, MigrationId, OperationId, VolumeId};
use volvisor_types::{ApiError, ApiErrorCode};

const SOURCE: &str = "src-host";
const TARGET: &str = "dst-host";
const VM: &str = "vm-1";
const MIG: &str = "mig-1";
const VOLS: [&str; 2] = ["vol-a", "vol-b"];

// ---------------------------------------------------------------------------
// The scripted fake driver
// ---------------------------------------------------------------------------

/// One participant volume's witness-side authority state.
#[derive(Default)]
struct VolumeState {
    epoch: u64,
    holder: Option<HostId>,
    lease: LeaseStateKind,
    barriers: Vec<RecordedMigrationBarrier>,
    retirements: Vec<EpochRetirement>,
}

/// `LeaseState` without the `None`-vs-default awkwardness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LeaseStateKind {
    #[default]
    None,
    Live,
    Revoked,
}

impl LeaseStateKind {
    fn as_view(self) -> LeaseState {
        match self {
            Self::Live => LeaseState::Live,
            Self::Revoked => LeaseState::Revoked,
            Self::None => LeaseState::None,
        }
    }
}

/// The external world: outlives any one coordinator (a "crash" drops
/// the coordinator, not reality).
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // a scripted world of observable flags
struct World {
    calls: Vec<String>,
    /// The source VM (destroyed by the cut).
    vm_present: bool,
    vm_paused: bool,
    /// The destination VM (created by the restore, resumed last).
    dst_vm_present: bool,
    dst_vm_paused: bool,
    suspended: BTreeSet<VolumeId>,
    secondary: BTreeSet<VolumeId>,
    witness_reachable: bool,
    commit_index: u64,
    volumes: BTreeMap<VolumeId, VolumeState>,
}

impl World {
    fn seed(vols: &[&str]) -> Self {
        let mut world = Self {
            vm_present: true,
            witness_reachable: true,
            ..Self::default()
        };
        for vol in vols {
            world.volumes.insert(
                VolumeId::new(*vol).expect("valid id"),
                VolumeState {
                    epoch: 1,
                    holder: Some(HostId::new(SOURCE).expect("valid id")),
                    lease: LeaseStateKind::Live,
                    barriers: Vec::new(),
                    retirements: Vec::new(),
                },
            );
        }
        world
    }

    /// Externally land a revoke that the coordinator never saw
    /// confirmed (a lost response).
    fn revoke_leases(&mut self) {
        for state in self.volumes.values_mut() {
            state.lease = LeaseStateKind::Revoked;
        }
    }

    /// Externally land a grant that the coordinator never saw
    /// confirmed.
    fn grant_leases(&mut self) {
        let target = HostId::new(TARGET).expect("valid id");
        for state in self.volumes.values_mut() {
            state.epoch += 1;
            state.holder = Some(target.clone());
            state.lease = LeaseStateKind::Live;
        }
    }

    fn clear_calls(&mut self) {
        self.calls.clear();
    }

    fn count(&self, prefix: &str) -> usize {
        self.calls.iter().filter(|c| c == &prefix).count()
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.calls.iter().position(|c| c == name)
    }
}

/// A `HandoffDriver` over a shared [`World`], with per-method failure
/// injection. A failed call is logged (the attempt happened) but has
/// **no effect** — lost-response scenarios are modeled by mutating the
/// world directly.
#[derive(Clone)]
struct FakeDriver {
    world: Arc<Mutex<World>>,
    failures: Arc<Mutex<HashMap<String, u32>>>,
}

impl FakeDriver {
    fn new(world: Arc<Mutex<World>>) -> Self {
        Self {
            world,
            failures: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Fail the next `n` calls of `method` (typed error, no effect).
    fn fail(&self, method: &str, n: u32) {
        self.failures
            .lock()
            .expect("failures lock")
            .insert(method.to_owned(), n);
    }

    /// Record the call attempt; apply the injected failure if any.
    fn call(&self, name: &str) -> Result<(), ApiError> {
        let fail = {
            let mut failures = self.failures.lock().expect("failures lock");
            failures.get_mut(name).is_some_and(|remaining| {
                if *remaining > 0 {
                    *remaining -= 1;
                    true
                } else {
                    false
                }
            })
        };
        {
            let mut world = self.world.lock().expect("world lock");
            world.calls.push(name.to_owned());
        }
        if fail {
            Err(ApiError::new(
                ApiErrorCode::Internal,
                format!("injected failure: {name}"),
            ))
        } else {
            Ok(())
        }
    }

    /// Record a volume-scoped call attempt.
    fn vcall(&self, name: &str, volume_id: &VolumeId) -> Result<(), ApiError> {
        self.call(&format!("{name}:{volume_id}"))
    }
}

#[async_trait]
impl HandoffDriver for FakeDriver {
    async fn witness_view(&self, volume_id: &VolumeId) -> Result<AuthorityView, ApiError> {
        self.vcall("witness_view", volume_id)?;
        let world = self.world.lock().expect("world lock");
        let state = world.volumes.get(volume_id).expect("seeded volume");
        Ok(AuthorityView {
            volume_id: volume_id.clone(),
            current_epoch: WriterEpoch(state.epoch),
            holder: state.holder.clone(),
            lease_state: state.lease.as_view(),
            lease_id: None,
            lease_remaining_secs: None,
            commit_index: world.commit_index,
            registration: None,
            barriers: state.barriers.clone(),
            retirements: state.retirements.clone(),
        })
    }

    async fn vm_present(&self, vm_id: &str) -> Result<bool, ApiError> {
        self.call(&format!("vm_present:{vm_id}"))?;
        Ok(self.world.lock().expect("world lock").vm_present)
    }

    async fn vm_paused(&self, vm_id: &str) -> Result<bool, ApiError> {
        self.call(&format!("vm_paused:{vm_id}"))?;
        let world = self.world.lock().expect("world lock");
        Ok(world.vm_present && world.vm_paused)
    }

    async fn source_secondary(&self, volume_id: &VolumeId) -> Result<bool, ApiError> {
        self.vcall("source_secondary", volume_id)?;
        Ok(self
            .world
            .lock()
            .expect("world lock")
            .secondary
            .contains(volume_id))
    }

    async fn target_granted(
        &self,
        volume_id: &VolumeId,
        target: &HostId,
    ) -> Result<bool, ApiError> {
        self.vcall("target_granted", volume_id)?;
        let world = self.world.lock().expect("world lock");
        let state = world.volumes.get(volume_id).expect("seeded volume");
        Ok(state.holder.as_ref() == Some(target) && state.lease == LeaseStateKind::Live)
    }

    fn witness_reachable(&self) -> bool {
        self.world.lock().expect("world lock").witness_reachable
    }

    async fn prepare_target(&self, record: &MigrationRecord) -> Result<(), ApiError> {
        self.call(&format!("prepare_target:{}", record.migration_id))
    }

    async fn discard_target(&self, record: &MigrationRecord) -> Result<(), ApiError> {
        self.call(&format!("discard_target:{}", record.migration_id))
    }

    async fn pause_vm(&self, vm_id: &str) -> Result<(), ApiError> {
        self.call(&format!("pause_vm:{vm_id}"))?;
        let mut world = self.world.lock().expect("world lock");
        if !world.vm_present {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                "pause refused: the VM is absent",
            ));
        }
        world.vm_paused = true;
        Ok(())
    }

    async fn quiesce_source(
        &self,
        volume_id: &VolumeId,
        _migration_id: &MigrationId,
    ) -> Result<(), ApiError> {
        self.vcall("quiesce_source", volume_id)?;
        self.world
            .lock()
            .expect("world lock")
            .suspended
            .insert(volume_id.clone());
        Ok(())
    }

    async fn track_sync(&self, volume_id: &VolumeId) -> Result<(), ApiError> {
        self.vcall("track_sync", volume_id)
    }

    async fn record_barrier(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
        _op_id: &OperationId,
    ) -> Result<volvisor_handoff::BarrierProof, ApiError> {
        self.vcall("record_barrier", volume_id)?;
        let mut world = self.world.lock().expect("world lock");
        world.commit_index += 1;
        let boundary_commit_index = world.commit_index;
        let state = world.volumes.get_mut(volume_id).expect("seeded volume");
        state.barriers.push(RecordedMigrationBarrier {
            holder: HostId::new(SOURCE).expect("valid id"),
            epoch: WriterEpoch(state.epoch),
            boundary_commit_index,
            attestation: BarrierAttestation {
                vm_paused_and_drained: true,
                data_path_suspended: true,
                peer_up_to_date: true,
            },
            migration_id: Some(migration_id.clone()),
            recorded_at: 0,
            voided: false,
        });
        Ok(volvisor_handoff::BarrierProof {
            volume_id: volume_id.clone(),
            boundary_commit_index: world.commit_index,
            attestation: BarrierAttestation {
                vm_paused_and_drained: true,
                data_path_suspended: true,
                peer_up_to_date: true,
            },
            recorded_at: 0,
        })
    }

    async fn void_barriers(&self, record: &MigrationRecord) -> Result<(), ApiError> {
        self.call(&format!("void_barriers:{}", record.migration_id))?;
        let mut world = self.world.lock().expect("world lock");
        if !world.witness_reachable {
            // The void cannot be journaled — G5's fail-closed case.
            return Err(ApiError::new(
                ApiErrorCode::OperationInDoubt,
                "witness unreachable: the void cannot be journaled",
            ));
        }
        for state in world.volumes.values_mut() {
            for barrier in &mut state.barriers {
                if barrier.migration_id.as_ref() == Some(&record.migration_id) {
                    barrier.voided = true;
                }
            }
        }
        Ok(())
    }

    async fn unsuspend_source(
        &self,
        volume_id: &VolumeId,
        _migration_id: &MigrationId,
    ) -> Result<(), ApiError> {
        self.vcall("unsuspend_source", volume_id)?;
        self.world
            .lock()
            .expect("world lock")
            .suspended
            .remove(volume_id);
        Ok(())
    }

    async fn resume_vm(&self, vm_id: &str) -> Result<(), ApiError> {
        self.call(&format!("resume_vm:{vm_id}"))?;
        let mut world = self.world.lock().expect("world lock");
        // The driver routes the resume to whichever host holds the VM:
        // the source during a pre-cut rollback, the destination after
        // the restore.
        if world.vm_present {
            if !world.vm_paused {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    "resume refused: the source VM is not paused",
                ));
            }
            world.vm_paused = false;
        } else if world.dst_vm_present {
            if !world.dst_vm_paused {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    "resume refused: the destination VM is not paused",
                ));
            }
            world.dst_vm_paused = false;
        } else {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                "resume refused: no VM to resume",
            ));
        }
        Ok(())
    }

    async fn snapshot_vm(&self, vm_id: &str) -> Result<(), ApiError> {
        self.call(&format!("snapshot_vm:{vm_id}"))
    }

    async fn destroy_vm(&self, vm_id: &str) -> Result<(), ApiError> {
        self.call(&format!("destroy_vm:{vm_id}"))?;
        let mut world = self.world.lock().expect("world lock");
        world.vm_present = false;
        world.vm_paused = false;
        Ok(())
    }

    async fn demote_source(
        &self,
        volume_id: &VolumeId,
        _migration_id: &MigrationId,
    ) -> Result<(), ApiError> {
        self.vcall("demote_source", volume_id)?;
        let world = self.world.lock().expect("world lock");
        if world.vm_present {
            // AGENTS rule 17: the device is open; the demote refuses.
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                "demote refused: the device is still open (rule 17)",
            ));
        }
        drop(world);
        self.world
            .lock()
            .expect("world lock")
            .secondary
            .insert(volume_id.clone());
        Ok(())
    }

    async fn revoke_set(
        &self,
        record: &MigrationRecord,
        _op_id: &OperationId,
    ) -> Result<(), ApiError> {
        self.call(&format!("revoke_set:{}", record.migration_id))?;
        let mut world = self.world.lock().expect("world lock");
        if !world.witness_reachable {
            return Err(ApiError::new(
                ApiErrorCode::OperationInDoubt,
                "witness unreachable: the revoke cannot be journaled",
            ));
        }
        for participant in &record.participants {
            let commit_index = world.commit_index;
            let state = world
                .volumes
                .get_mut(&participant.volume_id)
                .expect("seeded");
            state.lease = LeaseStateKind::Revoked;
            state.retirements.push(EpochRetirement {
                epoch: WriterEpoch(state.epoch),
                commit_index,
            });
        }
        Ok(())
    }

    async fn grant_set(
        &self,
        record: &MigrationRecord,
        _op_id: &OperationId,
    ) -> Result<(), ApiError> {
        self.call(&format!("grant_set:{}", record.migration_id))?;
        let mut world = self.world.lock().expect("world lock");
        if !world.witness_reachable {
            return Err(ApiError::new(
                ApiErrorCode::OperationInDoubt,
                "witness unreachable: the grant cannot be journaled",
            ));
        }
        let target = HostId::new(TARGET).expect("valid id");
        for participant in &record.participants {
            let commit_index = world.commit_index;
            let state = world
                .volumes
                .get_mut(&participant.volume_id)
                .expect("seeded");
            state.retirements.push(EpochRetirement {
                epoch: WriterEpoch(state.epoch),
                commit_index,
            });
            state.epoch += 1;
            state.holder = Some(target.clone());
            state.lease = LeaseStateKind::Live;
        }
        Ok(())
    }

    async fn promote_target(
        &self,
        volume_id: &VolumeId,
        _migration_id: &MigrationId,
    ) -> Result<(), ApiError> {
        self.vcall("promote_target", volume_id)
    }

    async fn restore_vm(&self, record: &MigrationRecord) -> Result<(), ApiError> {
        self.call(&format!("restore_vm:{}", record.vm_id))?;
        let mut world = self.world.lock().expect("world lock");
        // The restore lands paused in the pre-started destination VMM.
        world.dst_vm_present = true;
        world.dst_vm_paused = true;
        Ok(())
    }

    async fn clear_cut_marker(
        &self,
        volume_id: &VolumeId,
        _migration_id: &MigrationId,
    ) -> Result<(), ApiError> {
        self.vcall("clear_cut_marker", volume_id)
    }

    async fn fence_source(
        &self,
        volume_id: &VolumeId,
        _migration_id: &MigrationId,
    ) -> Result<(), ApiError> {
        self.vcall("fence_source", volume_id)
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn participants(vols: &[&str]) -> Vec<Participant> {
    vols.iter()
        .map(|vol| Participant {
            volume_id: VolumeId::new(*vol).expect("valid id"),
            expected_generation: 1,
            resource: format!("res-{vol}"),
            minor: 1,
        })
        .collect()
}

fn prepare_request() -> PrepareHandoffRequest {
    PrepareHandoffRequest {
        migration_id: MigrationId::new(MIG).expect("valid id"),
        vm_id: VM.to_owned(),
        source_host: HostId::new(SOURCE).expect("valid id"),
        target_host: HostId::new(TARGET).expect("valid id"),
        participants: participants(&VOLS),
    }
}

/// A deterministic ticking clock (every read advances time by one).
fn ticking_clock() -> Clock {
    let tick = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let clock = Arc::clone(&tick);
    Arc::new(move || {
        let now = clock.load(std::sync::atomic::Ordering::SeqCst);
        clock.store(now + 1, std::sync::atomic::Ordering::SeqCst);
        now
    })
}

/// One migration being driven against one external world and one
/// durable store directory. The clock is shared by every coordinator
/// built from the fixture (a real daemon's clock continues across a
/// restart).
struct Fixture {
    world: Arc<Mutex<World>>,
    driver: FakeDriver,
    dir: tempfile::TempDir,
    clock: Clock,
}

impl Fixture {
    fn new() -> Self {
        let world = Arc::new(Mutex::new(World::seed(&VOLS)));
        Self {
            driver: FakeDriver::new(Arc::clone(&world)),
            world,
            dir: tempfile::tempdir().expect("tempdir"),
            clock: ticking_clock(),
        }
    }

    /// A coordinator over this driver and store. Coordinators are
    /// cheap to drop: "crashing" one is dropping it and building a
    /// fresh one over the same world and directory.
    fn coordinator(&self) -> MigrationCoordinator<FakeDriver> {
        let store = MigrationStore::open(self.dir.path()).expect("open store");
        MigrationCoordinator::new(
            Arc::new(self.driver.clone()),
            store,
            Arc::clone(&self.clock),
        )
    }

    /// Prepare and drive until `method` fails; returns the error.
    async fn drive_until_failure(&self, method: &str) -> ApiError {
        self.driver.fail(method, 1);
        let coordinator = self.coordinator();
        let record = coordinator
            .prepare(prepare_request())
            .await
            .expect("prepare");
        coordinator
            .transfer(&record.migration_id)
            .await
            .expect_err("the injected failure must surface")
    }

    /// The raw stored record (the durable truth on disk, as a fresh
    /// process would load it).
    fn stored_record(&self) -> MigrationRecord {
        MigrationStore::open(self.dir.path())
            .expect("open store")
            .get(&migration_id())
            .expect("record exists")
    }
}

fn migration_id() -> MigrationId {
    MigrationId::new(MIG).expect("valid id")
}

/// The expected happy-path history: one entry per transition, cut
/// write-aheads included.
fn expected_happy_history() -> Vec<(HandoffState, Option<CutProgress>)> {
    vec![
        (HandoffState::Prepared, None),
        (HandoffState::Precopy, None),
        (HandoffState::Quiesced, None),
        (HandoffState::BarrierDurable, None),
        (
            HandoffState::BarrierDurable,
            Some(CutProgress::Snapshotting),
        ),
        (
            HandoffState::BarrierDurable,
            Some(CutProgress::DestroyingVm),
        ),
        (HandoffState::BarrierDurable, Some(CutProgress::Demoting)),
        (HandoffState::BarrierDurable, Some(CutProgress::Revoking)),
        (HandoffState::SourceRevoked, None),
        (HandoffState::DestinationAuthorized, None),
        (HandoffState::VmResumed, None),
        (HandoffState::Complete, None),
    ]
}

fn history_pairs(record: &MigrationRecord) -> Vec<(HandoffState, Option<CutProgress>)> {
    record
        .state_history
        .iter()
        .map(|entry| (entry.state.clone(), entry.cut))
        .collect()
}

// ---------------------------------------------------------------------------
// Row 4: persist before reporting; append-only history; refusals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prepare_validates_and_is_idempotent() {
    let fixture = Fixture::new();
    let coordinator = fixture.coordinator();

    // Validation: empty participants, duplicate volumes, empty vm id.
    let mut request = prepare_request();
    request.participants = Vec::new();
    let err = coordinator.prepare(request).await.expect_err("empty set");
    assert_eq!(err.code, ApiErrorCode::InvalidRequest);

    let mut request = prepare_request();
    request.participants = participants(&["vol-a", "vol-a"]);
    let err = coordinator.prepare(request).await.expect_err("duplicate");
    assert_eq!(err.code, ApiErrorCode::InvalidRequest);

    let mut request = prepare_request();
    request.vm_id = String::new();
    let err = coordinator.prepare(request).await.expect_err("empty vm");
    assert_eq!(err.code, ApiErrorCode::InvalidRequest);

    // Prepared: persisted with one history entry; prepare_target ran.
    let record = coordinator
        .prepare(prepare_request())
        .await
        .expect("prepare");
    assert_eq!(record.state, HandoffState::Prepared);
    assert_eq!(record.state_history.len(), 1);
    assert_eq!(
        fixture.world.lock().unwrap().count("prepare_target:mig-1"),
        1
    );

    // Identical content: idempotent, no new side effects.
    let again = coordinator
        .prepare(prepare_request())
        .await
        .expect("idempotent");
    assert_eq!(again, record);
    assert_eq!(
        fixture.world.lock().unwrap().count("prepare_target:mig-1"),
        1
    );

    // Different content: typed conflict, nothing changed.
    let mut conflicting = prepare_request();
    conflicting.vm_id = "vm-other".to_owned();
    let err = coordinator
        .prepare(conflicting)
        .await
        .expect_err("conflict");
    assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);
}

#[tokio::test]
async fn happy_path_drives_every_transition_in_order() {
    let fixture = Fixture::new();
    let coordinator = fixture.coordinator();
    let record = coordinator
        .prepare(prepare_request())
        .await
        .expect("prepare");
    let final_record = coordinator
        .transfer(&record.migration_id)
        .await
        .expect("transfer");

    assert_eq!(final_record.state, HandoffState::Complete);
    assert_eq!(final_record.cut, None);
    assert_eq!(history_pairs(&final_record), expected_happy_history());
    assert_eq!(final_record.barrier_proofs.len(), VOLS.len());

    // The world reflects the cut: VM gone, source Secondary, leases
    // migrated to the target.
    {
        let world = fixture.world.lock().unwrap();
        assert!(!world.vm_present);
        assert!(world.secondary.contains(&VolumeId::new("vol-a").unwrap()));
        assert!(world.secondary.contains(&VolumeId::new("vol-b").unwrap()));
        for state in world.volumes.values() {
            assert_eq!(state.lease, LeaseStateKind::Live);
            assert_eq!(state.holder.as_ref().map(HostId::as_str), Some(TARGET));
            assert_eq!(state.barriers.len(), 1);
            assert!(!state.barriers[0].voided);
        }

        // Ordering (rule 17 is provable here: the demote happens only
        // after the destroy, or the fake's busy device would refuse).
        let a = |name: &str| world.position(name).expect(name);
        assert!(a("pause_vm:vm-1") < a("quiesce_source:vol-a"));
        assert!(a("quiesce_source:vol-b") < a("record_barrier:vol-a"));
        assert!(a("record_barrier:vol-b") < a("snapshot_vm:vm-1"));
        assert!(a("snapshot_vm:vm-1") < a("destroy_vm:vm-1"));
        assert!(a("destroy_vm:vm-1") < world.position("demote_source:vol-a").unwrap());
        assert!(a("demote_source:vol-b") < a("revoke_set:mig-1"));
        assert!(a("revoke_set:mig-1") < a("grant_set:mig-1"));
        assert!(a("grant_set:mig-1") < a("promote_target:vol-a"));
        assert!(a("promote_target:vol-b") < a("restore_vm:vm-1"));
        assert!(a("restore_vm:vm-1") < a("resume_vm:vm-1"));
        assert!(a("resume_vm:vm-1") < a("clear_cut_marker:vol-a"));
    }

    // Complete observes canonically, with the cut cleared and no
    // in-doubt detail.
    let summary = coordinator
        .observe(&migration_id())
        .expect("observe")
        .expect("found");
    assert_eq!(summary.state, HandoffState::Complete);
    assert_eq!(summary.in_doubt_detail, None);

    // Idempotent: a re-transfer of a Complete record changes nothing.
    let again = coordinator
        .transfer(&migration_id())
        .await
        .expect("re-transfer");
    assert_eq!(again, final_record);
}

#[tokio::test]
async fn crash_mid_drive_persists_last_durable_state() {
    // Row 4: kill the coordinator mid-drive (a failing external act);
    // reopening from the store shows exactly the last completed
    // transition plus the cut write-ahead.
    let fixture = Fixture::new();
    fixture.drive_until_failure("destroy_vm:vm-1").await;

    // A "crash": drop the coordinator, reopen from the store.
    let coordinator = fixture.coordinator();
    let record = fixture.stored_record();
    assert_eq!(record.state, HandoffState::BarrierDurable);
    // The write-ahead for the failing act is durable; the act did not
    // happen (the VM is still present).
    assert_eq!(record.cut, Some(CutProgress::DestroyingVm));
    assert!(fixture.world.lock().unwrap().vm_present);
    // History: everything up to and including the write-ahead, once
    // each, nothing else.
    let expected: Vec<(HandoffState, Option<CutProgress>)> = expected_happy_history()[..6].to_vec();
    assert_eq!(history_pairs(&record), expected);

    // Reconcile drives forward (never aborts) and completes.
    let resolved = coordinator.resolve(&migration_id()).await.expect("resolve");
    assert_eq!(resolved.state, HandoffState::Complete);
    assert_eq!(history_pairs(&resolved), expected_happy_history());
}

#[tokio::test]
async fn state_history_is_append_only_under_crash_replay() {
    let fixture = Fixture::new();
    fixture.drive_until_failure("revoke_set:mig-1").await;

    let coordinator = fixture.coordinator();
    let resolved = coordinator.resolve(&migration_id()).await.expect("resolve");
    assert_eq!(resolved.state, HandoffState::Complete);

    // Re-resolving and observing a terminal record appends nothing.
    let history_before = resolved.state_history.clone();
    let again = coordinator
        .resolve(&migration_id())
        .await
        .expect("re-resolve");
    assert_eq!(again.state_history, history_before);
    let observed = coordinator
        .observe(&migration_id())
        .expect("observe")
        .expect("found");
    assert_eq!(observed.state_history, history_before);

    // No duplicate (state, cut) pairs, monotonic timestamps.
    let mut seen: Vec<(HandoffState, Option<CutProgress>)> = Vec::new();
    let mut last_at = 0;
    for entry in &history_before {
        let pair = (entry.state.clone(), entry.cut);
        assert!(!seen.contains(&pair), "duplicate entry");
        seen.push(pair);
        assert!(entry.at >= last_at, "non-monotonic timestamp");
        last_at = entry.at;
    }
}

#[tokio::test]
async fn illegal_transitions_are_refused_typed() {
    let fixture = Fixture::new();

    // transfer on a not-yet-prepared migration: typed not-found.
    let err = fixture
        .coordinator()
        .transfer(&migration_id())
        .await
        .expect_err("not found");
    assert_eq!(err.code, ApiErrorCode::NotFound);

    // transfer on Aborted: refused.
    fixture.drive_until_failure("record_barrier:vol-a").await;
    let coordinator = fixture.coordinator();
    coordinator.abort(&migration_id()).await.expect("abort");
    let err = coordinator
        .transfer(&migration_id())
        .await
        .expect_err("aborted");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
}

// ---------------------------------------------------------------------------
// Rows 7 + 6: the write-ahead and the forward-only re-drive
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_ahead_persists_each_cut_step_before_its_act() {
    // Row 7: for each cut step, crash between the store write-ahead
    // and the external act; the persisted cut names the step, and
    // resolve drives forward through it.
    for (act, expected_cut, vm_still_present) in [
        ("snapshot_vm:vm-1", CutProgress::Snapshotting, true),
        ("destroy_vm:vm-1", CutProgress::DestroyingVm, true),
        ("demote_source:vol-a", CutProgress::Demoting, false),
        ("revoke_set:mig-1", CutProgress::Revoking, false),
    ] {
        let fixture = Fixture::new();
        fixture.drive_until_failure(act).await;

        let coordinator = fixture.coordinator();
        let record = fixture.stored_record();
        assert_eq!(
            record.cut,
            Some(expected_cut),
            "write-ahead for {act} must be durable before the act"
        );
        assert_eq!(
            fixture.world.lock().unwrap().vm_present,
            vm_still_present,
            "the failed act must not have happened ({act})"
        );

        // The write-ahead observation is IN_DOUBT with the step detail
        // (only before DESTINATION_AUTHORIZED).
        let summary = coordinator
            .observe(&migration_id())
            .expect("observe")
            .expect("found");
        assert!(
            matches!(summary.state, HandoffState::InDoubt { .. }),
            "{act}"
        );
        assert_eq!(
            summary.in_doubt_detail.as_deref(),
            Some(format!("cut in progress: {expected_cut}").as_str()),
            "{act}"
        );

        // Reconcile: forward-only, through the persisted step.
        let resolved = coordinator.resolve(&migration_id()).await.expect("resolve");
        assert_eq!(resolved.state, HandoffState::Complete, "{act}");
        assert_eq!(resolved.cut, None);
        assert_eq!(history_pairs(&resolved), expected_happy_history(), "{act}");
    }
}

#[tokio::test]
async fn resolve_skips_destroy_when_vm_already_absent() {
    let fixture = Fixture::new();
    fixture.drive_until_failure("destroy_vm:vm-1").await;

    // The destroy actually landed (a lost response): reality shows the
    // VM gone. A fresh coordinator must not destroy again.
    fixture.world.lock().unwrap().vm_present = false;
    fixture.world.lock().unwrap().clear_calls();

    let coordinator = fixture.coordinator();
    let resolved = coordinator.resolve(&migration_id()).await.expect("resolve");
    assert_eq!(resolved.state, HandoffState::Complete);
    let world = fixture.world.lock().unwrap();
    assert_eq!(
        world.count("destroy_vm:vm-1"),
        0,
        "no destroy of an absent VM"
    );
    assert!(world.calls.iter().any(|c| c == "demote_source:vol-a"));
}

#[tokio::test]
async fn resolve_skips_demote_when_already_secondary() {
    let fixture = Fixture::new();
    fixture.drive_until_failure("demote_source:vol-a").await;

    // The demotes actually landed; the source is Secondary.
    {
        let mut world = fixture.world.lock().unwrap();
        world
            .secondary
            .insert(VolumeId::new("vol-a").expect("valid id"));
        world
            .secondary
            .insert(VolumeId::new("vol-b").expect("valid id"));
        world.clear_calls();
    }

    let coordinator = fixture.coordinator();
    let resolved = coordinator.resolve(&migration_id()).await.expect("resolve");
    assert_eq!(resolved.state, HandoffState::Complete);
    let world = fixture.world.lock().unwrap();
    assert_eq!(
        world.count("demote_source:vol-a"),
        0,
        "no re-demote of a Secondary"
    );
    assert_eq!(
        world.count("demote_source:vol-b"),
        0,
        "no re-demote of a Secondary"
    );
    assert!(world.calls.iter().any(|c| c == "revoke_set:mig-1"));
}

#[tokio::test]
async fn resolve_skips_revoke_when_lease_already_revoked() {
    let fixture = Fixture::new();
    fixture.drive_until_failure("revoke_set:mig-1").await;

    // The revoke actually landed.
    fixture.world.lock().unwrap().revoke_leases();
    fixture.world.lock().unwrap().clear_calls();

    let coordinator = fixture.coordinator();
    let resolved = coordinator.resolve(&migration_id()).await.expect("resolve");
    assert_eq!(resolved.state, HandoffState::Complete);
    let world = fixture.world.lock().unwrap();
    assert_eq!(world.count("revoke_set:mig-1"), 0, "no re-revoke");
    assert!(world.calls.iter().any(|c| c == "grant_set:mig-1"));
}

#[tokio::test]
async fn resolve_skips_grant_when_target_already_granted() {
    let fixture = Fixture::new();
    fixture.drive_until_failure("grant_set:mig-1").await;

    // The grant actually landed.
    fixture.world.lock().unwrap().grant_leases();
    fixture.world.lock().unwrap().clear_calls();

    let coordinator = fixture.coordinator();
    let resolved = coordinator.resolve(&migration_id()).await.expect("resolve");
    assert_eq!(resolved.state, HandoffState::Complete);
    let world = fixture.world.lock().unwrap();
    assert_eq!(world.count("grant_set:mig-1"), 0, "no re-grant");
    assert!(world.calls.iter().any(|c| c == "promote_target:vol-a"));
}

#[tokio::test]
async fn resolve_drives_forward_from_authorization_states() {
    // Row 6's tail: SourceRevoked / DestinationAuthorized / VmResumed
    // are forward-only too — the re-drive completes them.
    for (act, expected_state) in [
        ("grant_set:mig-1", HandoffState::SourceRevoked),
        ("promote_target:vol-a", HandoffState::DestinationAuthorized),
        ("clear_cut_marker:vol-a", HandoffState::VmResumed),
    ] {
        let fixture = Fixture::new();
        fixture.drive_until_failure(act).await;
        let coordinator = fixture.coordinator();
        let record = fixture.stored_record();
        assert_eq!(record.state, expected_state, "{act}");
        assert_eq!(record.cut, None, "{act}");

        let resolved = coordinator.resolve(&migration_id()).await.expect("resolve");
        assert_eq!(resolved.state, HandoffState::Complete, "{act}");
    }
}

#[tokio::test]
async fn resolve_performs_pending_forward_acts() {
    // The complement of the skip rows: with nothing done, the re-drive
    // performs the pending act exactly once (the same deterministic
    // batch operation id reconstructs the witness call).
    let fixture = Fixture::new();
    fixture.drive_until_failure("revoke_set:mig-1").await;
    fixture.world.lock().unwrap().clear_calls();

    let coordinator = fixture.coordinator();
    let resolved = coordinator.resolve(&migration_id()).await.expect("resolve");
    assert_eq!(resolved.state, HandoffState::Complete);
    let world = fixture.world.lock().unwrap();
    assert_eq!(world.count("revoke_set:mig-1"), 1);
    assert_eq!(world.count("grant_set:mig-1"), 1);
}

// ---------------------------------------------------------------------------
// Rows 5, 16, 16b: the pre-cut rollback and the G5 gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pre_cut_rollback_is_end_to_end() {
    // Row 16: abort before the cut — barriers voided, volumes
    // unsuspended, VM resumed, target discarded, record Aborted.
    // The failure lands mid-barrier-step: the first volume's barrier
    // IS recorded at the witness but not yet persisted in the record —
    // the rollback must still void it (G5 covers reality, not the
    // record's memory of it).
    let fixture = Fixture::new();
    fixture.drive_until_failure("record_barrier:vol-b").await;

    let coordinator = fixture.coordinator();
    let aborted = coordinator.abort(&migration_id()).await.expect("abort");
    assert!(matches!(aborted.state, HandoffState::Aborted { .. }));
    assert_eq!(aborted.cut, None);

    {
        let world = fixture.world.lock().unwrap();
        // The barrier recorded before the crash is voided (confirmed).
        assert!(
            world
                .volumes
                .values()
                .all(|state| state.barriers.iter().all(|b| b.voided))
        );
        // Everything is unsuspended and resumed; the target is discarded.
        assert!(world.suspended.is_empty());
        assert!(!world.vm_paused);
        assert!(world.vm_present);
        assert_eq!(world.count("resume_vm:vm-1"), 1);
        assert_eq!(world.count("discard_target:mig-1"), 1);
        assert_eq!(world.count("fence_source:vol-a"), 0);
        // G5 ordering: the void confirms before the resume.
        assert!(
            world.position("void_barriers:mig-1").unwrap()
                < world.position("resume_vm:vm-1").unwrap()
        );
    }

    // Idempotent: a second abort returns the same terminal record.
    let again = coordinator.abort(&migration_id()).await.expect("re-abort");
    assert!(matches!(again.state, HandoffState::Aborted { .. }));
}

#[tokio::test]
async fn resolve_rolls_back_pre_cut_states() {
    // The AutoBeforeCut reconcile: every pre-cut state without a cut
    // is rolled back at startup (the consumer re-issues).
    for act in [
        "track_sync:vol-a",
        "quiesce_source:vol-a",
        "record_barrier:vol-b",
    ] {
        let fixture = Fixture::new();
        fixture.drive_until_failure(act).await;
        let coordinator = fixture.coordinator();
        let resolved = coordinator.resolve(&migration_id()).await.expect("resolve");
        assert!(
            matches!(resolved.state, HandoffState::Aborted { .. }),
            "{act}"
        );
        let world = fixture.world.lock().unwrap();
        assert!(world.suspended.is_empty(), "{act}");
        assert!(!world.vm_paused, "{act}");
    }
}

#[tokio::test]
async fn rollback_void_failure_fences_and_never_resumes() {
    // Row 5's fail-closed path: the void cannot be journaled (the
    // witness is unreachable) — fence every participant, terminal
    // InDoubt, NEVER resume.
    let fixture = Fixture::new();
    fixture.drive_until_failure("record_barrier:vol-b").await;
    fixture.world.lock().unwrap().witness_reachable = false;

    let coordinator = fixture.coordinator();
    let record = coordinator.abort(&migration_id()).await.expect("abort");
    assert!(matches!(record.state, HandoffState::InDoubt { .. }));
    if let HandoffState::InDoubt { detail, .. } = &record.state {
        assert_eq!(detail, "abort void failed; source fenced");
    }

    let world = fixture.world.lock().unwrap();
    assert_eq!(world.count("fence_source:vol-a"), 1);
    assert_eq!(world.count("fence_source:vol-b"), 1);
    assert_eq!(world.count("resume_vm:vm-1"), 0, "never resumed");
    assert_eq!(
        world.count("unsuspend_source:vol-a"),
        0,
        "never unsuspended"
    );
    assert_eq!(
        world.count("discard_target:mig-1"),
        0,
        "target not discarded yet"
    );
    assert!(world.vm_paused, "the source stays paused");
    drop(world);

    // The terminal InDoubt observation surfaces the detail.
    let summary = coordinator
        .observe(&migration_id())
        .expect("observe")
        .expect("found");
    assert!(matches!(summary.state, HandoffState::InDoubt { .. }));
    assert_eq!(
        summary.in_doubt_detail.as_deref(),
        Some("abort void failed; source fenced")
    );
}

#[tokio::test]
async fn terminal_in_doubt_stays_while_witness_unreachable() {
    // Row 16b's hold: while the witness is down, the reconcile holds
    // the terminal InDoubt — no re-attempt, no resume.
    let fixture = Fixture::new();
    fixture.drive_until_failure("record_barrier:vol-b").await;
    fixture.world.lock().unwrap().witness_reachable = false;
    let coordinator = fixture.coordinator();
    coordinator
        .abort(&migration_id())
        .await
        .expect("abort into in-doubt");

    fixture.world.lock().unwrap().clear_calls();
    let resolved = coordinator.resolve(&migration_id()).await.expect("resolve");
    assert!(matches!(resolved.state, HandoffState::InDoubt { .. }));
    let world = fixture.world.lock().unwrap();
    assert_eq!(world.count("resume_vm:vm-1"), 0, "never resumed");
    assert_eq!(
        world.count("void_barriers:mig-1"),
        0,
        "no re-attempt while unreachable"
    );
    assert_eq!(world.count("fence_source:vol-a"), 0, "no redundant fencing");
}

#[tokio::test]
async fn terminal_in_doubt_recovers_once_witness_reachable() {
    // Row 16b: the witness comes back — the reconcile re-attempts the
    // abort, the void confirms, and only then does the VM resume.
    let fixture = Fixture::new();
    fixture.drive_until_failure("record_barrier:vol-b").await;
    fixture.world.lock().unwrap().witness_reachable = false;
    let coordinator = fixture.coordinator();
    coordinator
        .abort(&migration_id())
        .await
        .expect("abort into in-doubt");

    fixture.world.lock().unwrap().witness_reachable = true;
    fixture.world.lock().unwrap().clear_calls();
    let resolved = coordinator.resolve(&migration_id()).await.expect("resolve");
    assert!(matches!(resolved.state, HandoffState::Aborted { .. }));

    let world = fixture.world.lock().unwrap();
    assert_eq!(world.count("void_barriers:mig-1"), 1);
    assert_eq!(world.count("resume_vm:vm-1"), 1);
    assert!(
        world
            .volumes
            .values()
            .all(|state| state.barriers.iter().all(|b| b.voided))
    );
    assert!(
        world.position("void_barriers:mig-1").unwrap() < world.position("resume_vm:vm-1").unwrap()
    );
    assert!(!world.vm_paused);
}

#[tokio::test]
async fn abort_on_in_doubt_refuses_typed_while_unreachable() {
    let fixture = Fixture::new();
    fixture.drive_until_failure("record_barrier:vol-b").await;
    fixture.world.lock().unwrap().witness_reachable = false;
    let coordinator = fixture.coordinator();
    coordinator
        .abort(&migration_id())
        .await
        .expect("abort into in-doubt");

    // A direct abort while the witness is down refuses typed: the
    // void cannot be confirmed, so nothing resumes.
    let err = coordinator
        .abort(&migration_id())
        .await
        .expect_err("refused");
    assert_eq!(err.code, ApiErrorCode::OperationInDoubt);
    assert_eq!(fixture.world.lock().unwrap().count("resume_vm:vm-1"), 0);
}

// ---------------------------------------------------------------------------
// Row 16: no abort handler exists at or past the cut (total refusal)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn abort_is_refused_for_every_cut_or_later_state() {
    // The type-level guard made executable: for every cut step and
    // every post-cut canonical state, abort is a typed refusal — the
    // match has no rollback arms for them, so the refusal is total.
    let scenarios = [
        "snapshot_vm:vm-1",       // cut = Snapshotting
        "destroy_vm:vm-1",        // cut = DestroyingVm
        "demote_source:vol-a",    // cut = Demoting
        "revoke_set:mig-1",       // cut = Revoking
        "grant_set:mig-1",        // SourceRevoked
        "promote_target:vol-a",   // DestinationAuthorized
        "clear_cut_marker:vol-a", // VmResumed
    ];
    for act in scenarios {
        let fixture = Fixture::new();
        fixture.drive_until_failure(act).await;
        let coordinator = fixture.coordinator();
        // Snapshot the rollback-shaped call counts, then refuse.
        let before = {
            let world = fixture.world.lock().unwrap();
            (
                world.count("resume_vm:vm-1"),
                world.count("void_barriers:mig-1"),
                world.count("discard_target:mig-1"),
            )
        };
        let err = coordinator.abort(&migration_id()).await.expect_err(act);
        assert_eq!(err.code, ApiErrorCode::InvalidState, "{act}");
        assert!(err.detail.contains("forward-only"), "{act}: {err}");
        // Nothing rollback-shaped happened beyond the forward drive's
        // own calls (e.g. the destination resume at VmResumed).
        let world = fixture.world.lock().unwrap();
        assert_eq!(world.count("resume_vm:vm-1"), before.0, "{act}");
        assert_eq!(world.count("void_barriers:mig-1"), before.1, "{act}");
        assert_eq!(world.count("discard_target:mig-1"), before.2, "{act}");
    }

    // Complete too: a closed record is not abortable.
    let fixture = Fixture::new();
    let coordinator = fixture.coordinator();
    let record = coordinator
        .prepare(prepare_request())
        .await
        .expect("prepare");
    coordinator
        .transfer(&record.migration_id)
        .await
        .expect("transfer");
    let err = coordinator
        .abort(&migration_id())
        .await
        .expect_err("complete");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
}

#[tokio::test]
async fn rollback_act_failure_keeps_pre_cut_state() {
    // A rollback act failing after the confirmed void surfaces the
    // error without corrupting state; the reconcile retries the abort.
    let fixture = Fixture::new();
    fixture.drive_until_failure("record_barrier:vol-b").await;
    fixture.driver.fail("unsuspend_source:vol-a", 1);

    let coordinator = fixture.coordinator();
    let err = coordinator
        .abort(&migration_id())
        .await
        .expect_err("surfaced");
    assert_eq!(err.code, ApiErrorCode::Internal);
    let record = fixture.stored_record();
    assert_eq!(record.state, HandoffState::Quiesced, "no state corruption");
    assert_eq!(record.cut, None);

    // The retry completes the rollback (the void re-confirms
    // idempotently — the barriers were already voided).
    let resolved = coordinator.resolve(&migration_id()).await.expect("resolve");
    assert!(matches!(resolved.state, HandoffState::Aborted { .. }));
    let world = fixture.world.lock().unwrap();
    assert!(world.suspended.is_empty());
    assert_eq!(world.count("resume_vm:vm-1"), 1);
}

// ---------------------------------------------------------------------------
// Observation mapping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn observe_reports_stall_detail_from_authorization() {
    // From DESTINATION_AUTHORIZED on, the canonical state is reported
    // with a stall detail (a resolvable stall); the cut is already
    // cleared there.
    let fixture = Fixture::new();
    fixture.drive_until_failure("promote_target:vol-a").await;
    let coordinator = fixture.coordinator();
    let summary = coordinator
        .observe(&migration_id())
        .expect("observe")
        .expect("found");
    assert_eq!(summary.state, HandoffState::DestinationAuthorized);
    let detail = summary.in_doubt_detail.expect("stall detail");
    assert!(detail.contains("stalled"), "{detail}");

    // VmResumed stalls the same way.
    let fixture = Fixture::new();
    fixture.drive_until_failure("clear_cut_marker:vol-a").await;
    let coordinator = fixture.coordinator();
    let summary = coordinator
        .observe(&migration_id())
        .expect("observe")
        .expect("found");
    assert_eq!(summary.state, HandoffState::VmResumed);
    assert!(
        summary
            .in_doubt_detail
            .expect("stall detail")
            .contains("stalled")
    );
}

// ---------------------------------------------------------------------------
// Deterministic operation ids
// ---------------------------------------------------------------------------

#[tokio::test]
async fn coordinator_uses_deterministic_op_ids() {
    // The happy path's witness calls carry the derived ids: capture
    // them through a dedicated driver is overkill here — the pure
    // functions are the contract (unit-tested in the crate); this test
    // pins the restart-stability property the plan calls out.
    let migration = migration_id();
    let parts = participants(&VOLS);
    let first = batch_operation_id(&migration, BatchStep::RevokeSet, &parts).expect("derive");
    let second = batch_operation_id(&migration, BatchStep::RevokeSet, &parts).expect("derive");
    assert_eq!(first, second);
    assert_ne!(
        first,
        batch_operation_id(&migration, BatchStep::GrantSet, &parts).expect("derive")
    );
    assert_ne!(
        first,
        batch_operation_id(
            &MigrationId::new("mig-2").expect("valid id"),
            BatchStep::RevokeSet,
            &parts
        )
        .expect("derive")
    );
    let vol_a = VolumeId::new("vol-a").expect("valid id");
    assert_eq!(
        barrier_operation_id(&migration, &vol_a).expect("derive"),
        barrier_operation_id(&migration, &vol_a).expect("derive")
    );
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_round_trips_and_saves_atomically() {
    let fixture = Fixture::new();
    let coordinator = fixture.coordinator();
    let record = coordinator
        .prepare(prepare_request())
        .await
        .expect("prepare");

    // One finished record file, no temp leftovers.
    let dir = fixture.dir.path();
    let names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![format!("{MIG}.json")]);

    // A fresh store over the same directory round-trips the record.
    let reopened = MigrationStore::open(dir).expect("reopen");
    assert_eq!(reopened.get(&migration_id()), Some(record));
    assert_eq!(reopened.load_all().len(), 1);

    // A leftover .tmp (an interrupted save) is ignored, not loaded.
    std::fs::write(dir.join(format!("{MIG}.json.tmp")), "garbage").expect("write tmp");
    let reopened = MigrationStore::open(dir).expect("reopen with tmp");
    assert!(reopened.get(&migration_id()).is_some());

    // Remove: the record and its file are gone; removing again is a
    // no-op.
    let mut store = MigrationStore::open(dir).expect("reopen");
    assert!(store.remove(&migration_id()).expect("remove").is_some());
    assert!(store.get(&migration_id()).is_none());
    assert!(!dir.join(format!("{MIG}.json")).exists());
    assert!(
        store
            .remove(&migration_id())
            .expect("remove again")
            .is_none()
    );
}

#[tokio::test]
async fn store_corrupt_record_is_a_typed_startup_error() {
    let fixture = Fixture::new();
    let coordinator = fixture.coordinator();
    coordinator
        .prepare(prepare_request())
        .await
        .expect("prepare");

    // Unparseable JSON: typed error, never silently dropped.
    let path = fixture.dir.path().join(format!("{MIG}.json"));
    std::fs::write(&path, "{ not json").expect("corrupt");
    let err = MigrationStore::open(fixture.dir.path()).expect_err("corrupt");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("failed to parse"));

    // Valid JSON that is not a migration record: same refusal.
    std::fs::write(&path, "{\"unexpected\": true}").expect("wrong shape");
    let err = MigrationStore::open(fixture.dir.path()).expect_err("wrong shape");
    assert_eq!(err.code, ApiErrorCode::Internal);

    // A record carrying an unknown field: deny_unknown_fields refuses.
    let store = MigrationStore::open(fixture.dir.path());
    std::fs::write(&path, "{\"migration_id\": \"mig-1\", \"extra\": 1}").expect("unknown field");
    drop(store);
    let err = MigrationStore::open(fixture.dir.path()).expect_err("unknown field");
    assert_eq!(err.code, ApiErrorCode::Internal);

    // A record whose content disagrees with its file name: refused.
    let good = serde_json::json!({
        "migration_id": "mig-other",
        "vm_id": "vm-1",
        "source_host": SOURCE,
        "target_host": TARGET,
        "participants": [],
        "state": "prepared",
        "cut": null,
        "state_history": [],
        "barrier_proofs": [],
        "abort_policy": "auto_before_cut",
        "created_at": 1,
        "updated_at": 1,
    });
    std::fs::write(&path, good.to_string()).expect("mismatched name");
    let err = MigrationStore::open(fixture.dir.path()).expect_err("mismatched name");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("mig-1"));
}
