//! # The migration coordinator (P4b plan §3)
//!
//! [`MigrationCoordinator`] owns the canonical state machine: every
//! transition is *perform the side effects → persist the store → only
//! then report the new state*, and every cut step is persisted
//! **before** its external act (the D1a write-ahead). Transitions are
//! idempotent: a re-drive first reconciles the external world and
//! skips what is already true — never a blind re-execution.
//!
//! Two structural invariants (plan §8):
//!
//! - **There is no abort handler for cut-or-later states.** The abort
//!   match's only rollback arms end at `BarrierDurable`-without-cut;
//!   everything else is a total typed refusal. This is enforced by the
//!   shape of the match, and a test proves every cut-or-later state
//!   lands in the refusal.
//! - **An unvoided recorded barrier is a hard gate on any source
//!   resume** (G5): the pre-cut rollback confirms the void before any
//!   resume, and a void that cannot be journaled fails the whole
//!   rollback into `self_fence` with a terminal `InDoubt` record.
//!
//! The [`HandoffDriver`] trait is the stage-B2 seam: it abstracts the
//! VMM controller, the peer daemon, the witness client and the
//! provider handoff surface behind one engine-neutral vocabulary.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use volvisor_types::authority::{AuthorityView, LeaseState};
use volvisor_types::id::{HostId, MigrationId, OperationId, VolumeId};
use volvisor_types::{ApiError, ApiErrorCode};

use crate::store::MigrationStore;
use crate::types::{
    AbortPolicy, BarrierProof, CutProgress, HandoffState, MigrationRecord, MigrationSummary,
    Participant, PrepareHandoffRequest, StateHistoryEntry,
};

/// Deterministic wall clock for timestamps (unix epoch seconds).
/// Injected as a closure so tests are deterministic.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The external world of one migration, as the coordinator drives it.
///
/// Stage B2 implements this seam over the real surfaces
/// (`VmmController` + `ch-remote`, the destination daemon's peer API,
/// the witness client, and the provider's `HandoffSurface`); stage B1
/// tests the state machine against a scripted fake. The semantic set
/// is normative (plan §3): observations are read-only reconcile
/// inputs; preparation, quiesce and barrier acts are pre-cut;
/// everything from `snapshot_vm` on is the forward-only cut.
#[async_trait]
pub trait HandoffDriver: Send + Sync {
    /// The witness's full view of one participant's authority (lease
    /// state/holder/epoch, barrier log, retirements).
    async fn witness_view(&self, volume_id: &VolumeId) -> Result<AuthorityView, ApiError>;

    /// Whether the source VM exists.
    async fn vm_present(&self, vm_id: &str) -> Result<bool, ApiError>;

    /// Whether the source VM exists and is paused.
    async fn vm_paused(&self, vm_id: &str) -> Result<bool, ApiError>;

    /// Whether one participant's local source role is Secondary.
    async fn source_secondary(&self, volume_id: &VolumeId) -> Result<bool, ApiError>;

    /// Whether the target host currently holds a live granted lease
    /// for one participant (a `GrantSet` outcome).
    async fn target_granted(&self, volume_id: &VolumeId, target: &HostId)
    -> Result<bool, ApiError>;

    /// Whether the witness is reachable right now. Synchronous by
    /// design: it gates the terminal-`InDoubt` re-attempt decision,
    /// which must not await (and half-execute) a flaky connection.
    fn witness_reachable(&self) -> bool;

    /// Verify and prepare the target replica (resource present,
    /// Secondary, connected, no fence marker; snapshot dir readable).
    async fn prepare_target(&self, record: &MigrationRecord) -> Result<(), ApiError>;

    /// Discard the target-side preparation (the pre-cut abort tail).
    async fn discard_target(&self, record: &MigrationRecord) -> Result<(), ApiError>;

    /// Pause the source VM (verified: the adapter requires the
    /// observed `Paused` state, not the command's exit status).
    async fn pause_vm(&self, vm_id: &str) -> Result<(), ApiError>;

    /// Suspend one participant's source I/O and stamp the durable
    /// migration-cut marker (plan D6a).
    async fn quiesce_source(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<(), ApiError>;

    /// Prove replication catch-up (peer `UpToDate`, no resync) —
    /// taken **after** the suspension fixed the boundary (plan D2).
    /// The driver owns the bounded wait; a timeout is a typed error
    /// surfaced by the coordinator (no state corruption).
    async fn track_sync(&self, volume_id: &VolumeId) -> Result<(), ApiError>;

    /// Record one participant's serving boundary with the witness and
    /// return the durable proof. Idempotent per `op_id`.
    async fn record_barrier(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
        op_id: &OperationId,
    ) -> Result<BarrierProof, ApiError>;

    /// Void every barrier this migration recorded. **Must confirm**:
    /// `Ok` means every recorded barrier of the epoch is durably
    /// voided (G5's hard gate on any source resume).
    async fn void_barriers(&self, record: &MigrationRecord) -> Result<(), ApiError>;

    /// Unsuspend one participant's source I/O (the abort tail).
    async fn unsuspend_source(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<(), ApiError>;

    /// Resume the source VM (only after every barrier is confirmed
    /// voided — the coordinator enforces the ordering).
    async fn resume_vm(&self, vm_id: &str) -> Result<(), ApiError>;

    /// Snapshot the paused source VM (re-runnable: the memory is
    /// identical until the VM resumes, which it never will here).
    async fn snapshot_vm(&self, vm_id: &str) -> Result<(), ApiError>;

    /// Destroy the source VM — the point of no return (the device
    /// closes; there is no rollback past this line).
    async fn destroy_vm(&self, vm_id: &str) -> Result<(), ApiError>;

    /// Demote one participant's source role. Refuses typed while the
    /// device is open (AGENTS rule 17) — never forces.
    async fn demote_source(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<(), ApiError>;

    /// Witness `RevokeSet`: the set-wide self-release (only after
    /// every participant was proven demoted — never a subset).
    /// Idempotent per `op_id`.
    async fn revoke_set(
        &self,
        record: &MigrationRecord,
        op_id: &OperationId,
    ) -> Result<(), ApiError>;

    /// Witness `GrantSet`: mint fresh epochs for the target and retire
    /// lingering ones (set-wide). Idempotent per `op_id`.
    async fn grant_set(
        &self,
        record: &MigrationRecord,
        op_id: &OperationId,
    ) -> Result<(), ApiError>;

    /// Promote one participant on the target, under the granted lease
    /// (the B2 promote-under-granted-lease path, plan §6).
    async fn promote_target(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<(), ApiError>;

    /// Restore the destination VM from the snapshot into the
    /// pre-started empty VMM (a half-restored VM is destroyed first —
    /// the re-drive is idempotent).
    async fn restore_vm(&self, record: &MigrationRecord) -> Result<(), ApiError>;

    /// Clear one participant's migration-cut marker (the completion
    /// tail; only the coordinator clears it — plan D6a).
    async fn clear_cut_marker(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<(), ApiError>;

    /// Fail-closed fencing for a rollback whose barrier void could not
    /// be confirmed: durably self-fence one participant (the P4a
    /// `PendingFence` path). Never followed by a resume.
    async fn fence_source(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<(), ApiError>;
}

/// The batch witness mutation a deterministic operation id is derived
/// for (plan §3, round-2 MINOR D).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchStep {
    /// The set-wide source self-release (`RevokeSet`).
    RevokeSet,
    /// The set-wide target grant (`GrantSet`).
    GrantSet,
}

impl BatchStep {
    /// The domain tag folded into the operation-id hash (also the
    /// human-readable step name inside the id).
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::RevokeSet => "revoke-set",
            Self::GrantSet => "grant-set",
        }
    }
}

/// Derive the deterministic operation id for a batch witness mutation
/// (`RevokeSet`/`GrantSet`) of one migration.
///
/// The id is a pure function of the migration id, the step and the
/// **ordered** participant volume set — domain-separated SHA-256,
/// hex-prefixed — so a post-crash retry reconstructs it without having
/// remembered the prior call (the journal replays the recorded
/// outcome byte-identically; a fresh random id would mint a redundant
/// epoch). A different participant set, or a different order, yields a
/// different id.
///
/// # Errors
/// `INTERNAL` only if the derived string failed identity validation
/// (unreachable for the fixed tag and hex alphabet).
pub fn batch_operation_id(
    migration_id: &MigrationId,
    step: BatchStep,
    participants: &[Participant],
) -> Result<OperationId, ApiError> {
    let volumes: Vec<&str> = participants.iter().map(|p| p.volume_id.as_str()).collect();
    derived_operation_id(migration_id, step.tag(), &volumes)
}

/// Derive the deterministic per-volume operation id for a
/// `RecordBarrier` call (the same discipline as
/// [`batch_operation_id`], scoped to one volume).
///
/// # Errors
/// `INTERNAL` only if the derived string failed identity validation
/// (unreachable for the fixed tag and hex alphabet).
pub fn barrier_operation_id(
    migration_id: &MigrationId,
    volume_id: &VolumeId,
) -> Result<OperationId, ApiError> {
    derived_operation_id(migration_id, "record-barrier", &[volume_id.as_str()])
}

/// The first `bytes * 2` hex characters of a digest, without `format!`
/// (the house `resource_name_for` discipline).
fn hex_prefix(digest: &[u8], bytes: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes * 2);
    for byte in digest.iter().take(bytes) {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// Domain-separated SHA-256 over the migration id, the step tag and
/// the ordered volume list, rendered as
/// `mig-{tag}-{16 hex chars}`.
fn derived_operation_id(
    migration_id: &MigrationId,
    tag: &str,
    volumes: &[&str],
) -> Result<OperationId, ApiError> {
    let mut hasher = Sha256::new();
    hasher.update(b"volvisor.handoff.op.v1:");
    hasher.update(migration_id.as_str().as_bytes());
    hasher.update(b":");
    hasher.update(tag.as_bytes());
    hasher.update(b":");
    hasher.update(volumes.join(",").as_bytes());
    let digest = hasher.finalize();
    let hash16 = hex_prefix(&digest, 8);
    let raw = format!("mig-{tag}-{hash16}");
    OperationId::new(raw).map_err(|e| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("derived operation id failed validation: {e}"),
        )
    })
}

/// The forward-only boundary: states at or past `SourceRevoked` (and
/// any record with a live cut) have no abort handler.
const FORWARD_ONLY_RANK: u8 = 4;

/// The migration state machine: drives the canonical cutover sequence
/// through a [`HandoffDriver`], persisting every transition in the
/// [`MigrationStore`] before reporting it.
///
/// Not internally serialized: the caller (the B2 daemon's operations
/// pipeline and reconcile task) must not run two operations against
/// one migration concurrently. The store mutations are whole-record
/// upserts through the single append-only transition helper, so a
/// serialized caller sees no torn state.
pub struct MigrationCoordinator<D: HandoffDriver> {
    driver: Arc<D>,
    store: Mutex<MigrationStore>,
    clock: Clock,
}

impl<D: HandoffDriver> MigrationCoordinator<D> {
    /// Build a coordinator over a driver, an opened store and a clock.
    pub fn new(driver: Arc<D>, store: MigrationStore, clock: Clock) -> Self {
        Self {
            driver,
            store: Mutex::new(store),
            clock,
        }
    }

    /// Prepare a migration: validate the request, verify/prepare the
    /// target through the driver, then persist the `Prepared` record.
    ///
    /// Idempotent by `migration_id`: an existing record with identical
    /// content is returned without side effects; the same id with
    /// different content is a typed conflict.
    ///
    /// # Errors
    /// `INVALID_REQUEST` for an empty participant set, duplicate
    /// volume ids or an empty vm id; `IDEMPOTENCY_CONFLICT` when the
    /// `migration_id` exists with different content; the driver's
    /// typed error when target preparation fails (nothing is
    /// persisted).
    pub async fn prepare(
        &self,
        request: PrepareHandoffRequest,
    ) -> Result<MigrationRecord, ApiError> {
        if request.vm_id.is_empty() {
            return Err(ApiError::invalid_request("vm_id must not be empty"));
        }
        if request.participants.is_empty() {
            return Err(ApiError::invalid_request(
                "a migration needs at least one participant",
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for participant in &request.participants {
            if !seen.insert(participant.volume_id.clone()) {
                return Err(ApiError::invalid_request(format!(
                    "duplicate participant volume {}",
                    participant.volume_id
                )));
            }
        }
        if let Some(existing) = self.with_store(|store| Ok(store.get(&request.migration_id)))? {
            if existing.vm_id == request.vm_id
                && existing.source_host == request.source_host
                && existing.target_host == request.target_host
                && existing.participants == request.participants
            {
                return Ok(existing);
            }
            return Err(ApiError::idempotency_conflict(&request.migration_id));
        }
        let now = (self.clock)();
        let record = MigrationRecord {
            migration_id: request.migration_id.clone(),
            vm_id: request.vm_id,
            source_host: request.source_host,
            target_host: request.target_host,
            participants: request.participants,
            state: HandoffState::Prepared,
            cut: None,
            state_history: vec![StateHistoryEntry {
                state: HandoffState::Prepared,
                cut: None,
                at: now,
                detail: None,
            }],
            barrier_proofs: Vec::new(),
            abort_policy: AbortPolicy::AutoBeforeCut,
            created_at: now,
            updated_at: now,
        };
        // Side effects first, then persist, then report (plan §3).
        self.driver.prepare_target(&record).await?;
        self.with_store(|store| store.upsert(&record))?;
        Ok(record)
    }

    /// Drive the migration forward to `Complete` (or to the last
    /// durable state, erroring, if an external act fails — the record
    /// is persisted at every completed transition and at every cut
    /// write-ahead, so a retry or the reconcile resumes exactly there).
    ///
    /// Refused typed on terminal states (`Aborted`, terminal
    /// `InDoubt`); idempotent on `Complete`.
    ///
    /// # Errors
    /// `INVALID_STATE` when the migration is `Aborted` or terminally
    /// `InDoubt` (the reconcile owns those); `NOT_FOUND` when the
    /// record does not exist; the driver's typed error when an
    /// external act fails.
    pub async fn transfer(&self, migration_id: &MigrationId) -> Result<MigrationRecord, ApiError> {
        let mut record = self.record(migration_id)?;
        match &record.state {
            HandoffState::Complete => return Ok(record),
            HandoffState::Aborted { .. } => {
                return Err(invalid_state(
                    "transfer refused: the migration is aborted (prepare again to re-issue)",
                ));
            }
            HandoffState::InDoubt { .. } => {
                return Err(invalid_state(
                    "transfer refused: the migration is terminally in doubt (resolve it)",
                ));
            }
            _ => {}
        }
        self.drive_forward(&mut record).await?;
        Ok(record)
    }

    /// Abort a migration — **only before the cut**. With a cut
    /// present, or at or past `SourceRevoked`, the refusal is total:
    /// there is no abort handler for cut-or-later states, by
    /// construction (plan §8 item 4).
    ///
    /// The pre-cut rollback is G5-ordered: `void_barriers` must
    /// **confirm** before anything is resumed; a void that cannot be
    /// journaled fails the whole rollback into `self_fence`
    /// (`fence_source` per participant) with a terminal `InDoubt`
    /// record — the source is never resumed with a live barrier. The
    /// fail-closed outcome is returned as `Ok` with the `InDoubt`
    /// record (it is an observable terminal state, not an error).
    ///
    /// # Errors
    /// `INVALID_STATE` for every cut-or-later state (the total
    /// refusal); `OPERATION_IN_DOUBT` when re-attempting a terminally
    /// in-doubt record while the witness is unreachable (the void
    /// cannot be confirmed; the source stays fenced); the driver's
    /// typed error when a rollback act after the confirmed void fails
    /// (the record keeps its pre-cut state; the reconcile retries).
    pub async fn abort(&self, migration_id: &MigrationId) -> Result<MigrationRecord, ApiError> {
        let mut record = self.record(migration_id)?;
        match (&record.state, record.cut) {
            // The only rollback arms: pre-cut states without a cut.
            (
                HandoffState::Prepared
                | HandoffState::Precopy
                | HandoffState::Quiesced
                | HandoffState::BarrierDurable,
                None,
            ) => {}
            (HandoffState::Aborted { .. }, None) => return Ok(record),
            (HandoffState::InDoubt { .. }, None) => {
                if !self.driver.witness_reachable() {
                    return Err(ApiError::new(
                        ApiErrorCode::OperationInDoubt,
                        "abort refused: the witness is unreachable, the barrier void cannot be \
                         confirmed; the source stays fenced (never resumed)",
                    ));
                }
            }
            // Cut-or-later states: no abort handler exists — the
            // refusal is total (plan G1/D1a).
            _ => {
                return Err(invalid_state(format!(
                    "abort refused: no abort path exists at or past the cut (state {}, cut {}); \
                     the migration is forward-only",
                    record.state,
                    record
                        .cut
                        .map_or_else(|| "none".to_owned(), |cut| cut.to_string())
                )));
            }
        }
        self.rollback(&mut record).await?;
        Ok(record)
    }

    /// Reconcile one migration against the external world (the
    /// startup pass and the retry task; plan §3).
    ///
    /// **External facts first**: the witness view, VM presence, source
    /// roles and target grants are queried and folded into the store
    /// (a destroy that already landed advances the cut; a revoke that
    /// already landed lands the record at `SourceRevoked`; a grant
    /// that already landed lands it at `DestinationAuthorized`) — the
    /// stored state alone is never trusted to classify a crash window.
    /// Only then is a direction chosen:
    ///
    /// - pre-cut, no cut → the abort path (the `AutoBeforeCut` policy;
    ///   G5-ordered, fail-closed into terminal `InDoubt` on a failed
    ///   void);
    /// - cut present or state ≥ `SourceRevoked` → the forward-only
    ///   re-drive, skipping acts the external facts prove already
    ///   done;
    /// - terminal `InDoubt` (a failed void) → the abort is re-attempted
    ///   **only if the witness is reachable**; while it is not, the
    ///   record stays `InDoubt` and nothing is resumed;
    /// - `Complete`/`Aborted` → nothing.
    ///
    /// # Errors
    /// `NOT_FOUND` when the record does not exist; the driver's typed
    /// error when an observation or a chosen direction's act fails
    /// (the record stays at its last durable state).
    pub async fn resolve(&self, migration_id: &MigrationId) -> Result<MigrationRecord, ApiError> {
        let mut record = self.record(migration_id)?;
        match &record.state {
            HandoffState::Complete | HandoffState::Aborted { .. } => return Ok(record),
            HandoffState::InDoubt { .. } => {
                if self.driver.witness_reachable() {
                    self.rollback(&mut record).await?;
                }
                return Ok(record);
            }
            _ => {}
        }
        self.fold_external_facts(&mut record).await?;
        if record.cut.is_some()
            || record
                .state
                .forward_rank()
                .is_some_and(|rank| rank >= FORWARD_ONLY_RANK)
        {
            self.drive_forward(&mut record).await?;
        } else {
            self.rollback(&mut record).await?;
        }
        Ok(record)
    }

    /// The read-only `ObserveHandoff` projection.
    ///
    /// # Errors
    /// `INTERNAL` when the store lock is poisoned; `Ok(None)` when the
    /// record does not exist.
    pub fn observe(
        &self,
        migration_id: &MigrationId,
    ) -> Result<Option<MigrationSummary>, ApiError> {
        self.with_store(|store| Ok(store.get(migration_id)))
            .map(|record| record.map(|record| record.observe()))
    }

    /// The forward drive (steps 1–7 of the cutover sequence). Every
    /// cut step persists its write-ahead **before** the external act;
    /// every re-drive reconciles the external facts and skips what is
    /// already true.
    async fn drive_forward(&self, record: &mut MigrationRecord) -> Result<(), ApiError> {
        self.drive_precopy(record).await?;
        self.drive_quiesce(record).await?;
        self.drive_barriers(record).await?;
        self.drive_cut(record).await?;
        self.drive_authorize(record).await?;
        self.drive_resume(record).await?;
        self.drive_complete(record).await?;
        Ok(())
    }

    /// `Prepared → Precopy`: observe replication catch-up per
    /// participant. The driver owns the bounded wait; a timeout is its
    /// typed error (surfaced here — no state corruption, plan §9 row
    /// 9).
    async fn drive_precopy(&self, record: &mut MigrationRecord) -> Result<(), ApiError> {
        if record.state != HandoffState::Prepared {
            return Ok(());
        }
        for participant in &record.participants {
            self.driver.track_sync(&participant.volume_id).await?;
        }
        self.transition(record, HandoffState::Precopy, None, None)
    }

    /// `Precopy → Quiesced`: pause the VM, then suspend every
    /// participant's source I/O with the durable cut marker (D2's
    /// ordering: the boundary is fixed before it is proven).
    async fn drive_quiesce(&self, record: &mut MigrationRecord) -> Result<(), ApiError> {
        if record.state != HandoffState::Precopy {
            return Ok(());
        }
        self.driver.pause_vm(&record.vm_id).await?;
        for participant in &record.participants {
            self.driver
                .quiesce_source(&participant.volume_id, &record.migration_id)
                .await?;
        }
        self.transition(record, HandoffState::Quiesced, None, None)
    }

    /// `Quiesced → BarrierDurable`: post-suspension `TrackSync` proof
    /// per participant, then `RecordBarrier` per participant with the
    /// deterministic per-volume operation id; the proofs are persisted
    /// with the state.
    async fn drive_barriers(&self, record: &mut MigrationRecord) -> Result<(), ApiError> {
        if record.state != HandoffState::Quiesced {
            return Ok(());
        }
        for participant in &record.participants {
            self.driver.track_sync(&participant.volume_id).await?;
        }
        let mut proofs = Vec::with_capacity(record.participants.len());
        for participant in &record.participants {
            let op_id = barrier_operation_id(&record.migration_id, &participant.volume_id)?;
            proofs.push(
                self.driver
                    .record_barrier(&participant.volume_id, &record.migration_id, &op_id)
                    .await?,
            );
        }
        record.barrier_proofs = proofs;
        self.transition(record, HandoffState::BarrierDurable, None, None)
    }

    /// `BarrierDurable → SourceRevoked`: the cut. Write-ahead before
    /// every act; forward-only; every step skips what the external
    /// facts prove already done.
    async fn drive_cut(&self, record: &mut MigrationRecord) -> Result<(), ApiError> {
        if record.state != HandoffState::BarrierDurable {
            return Ok(());
        }
        if record.cut.is_none() {
            // Write-ahead: the store records the step before the act.
            self.set_cut(record, CutProgress::Snapshotting, None)?;
        }
        if record.cut == Some(CutProgress::Snapshotting) {
            self.driver.snapshot_vm(&record.vm_id).await?;
            self.set_cut(record, CutProgress::DestroyingVm, None)?;
        }
        if record.cut == Some(CutProgress::DestroyingVm) {
            // A crashed predecessor's destroy may have landed; an
            // absent VM is skipped, not repeated.
            if self.driver.vm_present(&record.vm_id).await? {
                self.driver.destroy_vm(&record.vm_id).await?;
            }
            self.set_cut(record, CutProgress::Demoting, None)?;
        }
        if record.cut == Some(CutProgress::Demoting) {
            for participant in &record.participants {
                // An already-Secondary volume is not re-demoted.
                if !self.driver.source_secondary(&participant.volume_id).await? {
                    self.driver
                        .demote_source(&participant.volume_id, &record.migration_id)
                        .await?;
                }
            }
            self.set_cut(record, CutProgress::Revoking, None)?;
        }
        if record.cut == Some(CutProgress::Revoking) {
            // An already-revoked lease is not re-revoked.
            let mut revoked = true;
            for participant in &record.participants {
                let view = self.driver.witness_view(&participant.volume_id).await?;
                if view.lease_state != LeaseState::Revoked {
                    revoked = false;
                }
            }
            if !revoked {
                let op_id = batch_operation_id(
                    &record.migration_id,
                    BatchStep::RevokeSet,
                    &record.participants,
                )?;
                self.driver.revoke_set(record, &op_id).await?;
            }
            // The cut is over: the state itself carries forward-only
            // from here.
            self.transition(record, HandoffState::SourceRevoked, None, None)?;
        }
        Ok(())
    }

    /// `SourceRevoked → DestinationAuthorized`: the set-wide grant
    /// (an already-granted target skips the grant — D3's two
    /// observable steps are never collapsed, but neither repeated).
    async fn drive_authorize(&self, record: &mut MigrationRecord) -> Result<(), ApiError> {
        if record.state != HandoffState::SourceRevoked {
            return Ok(());
        }
        let mut granted = true;
        for participant in &record.participants {
            if !self
                .driver
                .target_granted(&participant.volume_id, &record.target_host)
                .await?
            {
                granted = false;
            }
        }
        if !granted {
            let op_id = batch_operation_id(
                &record.migration_id,
                BatchStep::GrantSet,
                &record.participants,
            )?;
            self.driver.grant_set(record, &op_id).await?;
        }
        self.transition(record, HandoffState::DestinationAuthorized, None, None)
    }

    /// `DestinationAuthorized → VmResumed`: promote every participant
    /// on the target, restore the destination VM, resume it.
    async fn drive_resume(&self, record: &mut MigrationRecord) -> Result<(), ApiError> {
        if record.state != HandoffState::DestinationAuthorized {
            return Ok(());
        }
        for participant in &record.participants {
            self.driver
                .promote_target(&participant.volume_id, &record.migration_id)
                .await?;
        }
        self.driver.restore_vm(record).await?;
        self.driver.resume_vm(&record.vm_id).await?;
        self.transition(record, HandoffState::VmResumed, None, None)
    }

    /// `VmResumed → Complete`: clear the cut markers, verify the
    /// source Secondary everywhere, close the record.
    async fn drive_complete(&self, record: &mut MigrationRecord) -> Result<(), ApiError> {
        if record.state != HandoffState::VmResumed {
            return Ok(());
        }
        for participant in &record.participants {
            self.driver
                .clear_cut_marker(&participant.volume_id, &record.migration_id)
                .await?;
        }
        for participant in &record.participants {
            if !self.driver.source_secondary(&participant.volume_id).await? {
                return Err(invalid_state(format!(
                    "source volume {} is not Secondary at completion",
                    participant.volume_id
                )));
            }
        }
        self.transition(record, HandoffState::Complete, None, None)
    }

    /// The pre-cut rollback (G5-ordered). Returns `Ok` with the record
    /// in `Aborted` — or, when the void could not be confirmed, in
    /// terminal `InDoubt` after fencing every participant (never a
    /// resume).
    async fn rollback(&self, record: &mut MigrationRecord) -> Result<(), ApiError> {
        // G5: every recorded barrier must be confirmed voided before
        // anything is resumed. The void "must confirm": an error here
        // — for any reason, including witness unreachability — fails
        // the whole rollback into self_fence.
        if let Err(void_error) = self.driver.void_barriers(record).await {
            let mut fenced = 0;
            for participant in &record.participants {
                if self
                    .driver
                    .fence_source(&participant.volume_id, &record.migration_id)
                    .await
                    .is_ok()
                {
                    fenced += 1;
                }
            }
            // Fail-closed regardless of individual fence outcomes: the
            // record is InDoubt either way; the history entry carries
            // the diagnostics (a failed fence is never silently
            // resolved, it is reported for the operator). A re-attempt
            // that fails again keeps the original `since` — the doubt
            // began when the void first failed.
            let since = match &record.state {
                HandoffState::InDoubt { since, .. } => *since,
                _ => (self.clock)(),
            };
            let detail = "abort void failed; source fenced".to_owned();
            self.transition(
                record,
                HandoffState::InDoubt { since, detail },
                None,
                Some(format!(
                    "void_barriers failed ({void_error}); fenced {fenced} of {} participants",
                    record.participants.len()
                )),
            )?;
            return Ok(());
        }
        for participant in &record.participants {
            self.driver
                .unsuspend_source(&participant.volume_id, &record.migration_id)
                .await?;
        }
        for participant in &record.participants {
            self.driver
                .clear_cut_marker(&participant.volume_id, &record.migration_id)
                .await?;
        }
        if self.driver.vm_present(&record.vm_id).await?
            && self.driver.vm_paused(&record.vm_id).await?
        {
            self.driver.resume_vm(&record.vm_id).await?;
        }
        self.driver.discard_target(record).await?;
        self.transition(
            record,
            HandoffState::Aborted {
                reason: "aborted before the cut (auto-before-cut policy)".to_owned(),
                at: (self.clock)(),
            },
            None,
            None,
        )
    }

    /// Query the external facts and fold them into the store. The fold
    /// only *advances* the record along the forward path when an act
    /// is provably done; it never rolls anything back and never
    /// invents a decision the facts do not support.
    async fn fold_external_facts(&self, record: &mut MigrationRecord) -> Result<(), ApiError> {
        let vm_present = self.driver.vm_present(&record.vm_id).await?;
        let mut all_secondary = true;
        let mut all_revoked = true;
        let mut all_granted = true;
        for participant in &record.participants {
            let view = self.driver.witness_view(&participant.volume_id).await?;
            if view.lease_state != LeaseState::Revoked {
                all_revoked = false;
            }
            if !self.driver.source_secondary(&participant.volume_id).await? {
                all_secondary = false;
            }
            if !self
                .driver
                .target_granted(&participant.volume_id, &record.target_host)
                .await?
            {
                all_granted = false;
            }
        }
        // Cut-progress folds: the act provably landed even though the
        // store still records the write-ahead.
        if record.cut == Some(CutProgress::DestroyingVm) && !vm_present {
            self.set_cut(
                record,
                CutProgress::Demoting,
                Some("vm absent: the destroy is observed done".to_owned()),
            )?;
        }
        if record.cut == Some(CutProgress::Demoting) && all_secondary {
            self.set_cut(
                record,
                CutProgress::Revoking,
                Some("all participants Secondary: the demotes are observed done".to_owned()),
            )?;
        }
        if record.cut == Some(CutProgress::Revoking) && all_revoked {
            self.transition(
                record,
                HandoffState::SourceRevoked,
                None,
                Some("leases revoked: the revoke is observed done".to_owned()),
            )?;
        }
        // State folds.
        if record.state == HandoffState::SourceRevoked && all_granted {
            self.transition(
                record,
                HandoffState::DestinationAuthorized,
                None,
                Some("target grants observed: the grant is observed done".to_owned()),
            )?;
        }
        Ok(())
    }

    /// The single state-transition helper: append-only by construction.
    ///
    /// Sets the state and cut, stamps `updated_at`, appends one
    /// history entry (a repeated transition with an identical
    /// `(state, cut)` pair appends nothing) and persists the whole
    /// record before returning.
    fn transition(
        &self,
        record: &mut MigrationRecord,
        state: HandoffState,
        cut: Option<CutProgress>,
        detail: Option<String>,
    ) -> Result<(), ApiError> {
        if record.state == state && record.cut == cut {
            return Ok(());
        }
        let now = (self.clock)();
        record.state = state.clone();
        record.cut = cut;
        record.updated_at = now;
        record.state_history.push(StateHistoryEntry {
            state,
            cut,
            at: now,
            detail,
        });
        self.with_store(|store| store.upsert(record))
    }

    /// Persist a cut write-ahead: the same state, a new cut step.
    fn set_cut(
        &self,
        record: &mut MigrationRecord,
        step: CutProgress,
        detail: Option<String>,
    ) -> Result<(), ApiError> {
        self.transition(record, record.state.clone(), Some(step), detail)
    }

    /// Load one record or fail typed.
    fn record(&self, migration_id: &MigrationId) -> Result<MigrationRecord, ApiError> {
        self.with_store(|store| Ok(store.get(migration_id)))?
            .ok_or_else(|| ApiError::not_found(format!("migration {migration_id} does not exist")))
    }

    /// Run one store operation under the lock. The lock is never held
    /// across an await (store operations are synchronous).
    fn with_store<R>(
        &self,
        operation: impl FnOnce(&mut MigrationStore) -> Result<R, ApiError>,
    ) -> Result<R, ApiError> {
        let mut store = self.store.lock().map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "the migration store lock is poisoned (a prior operation panicked)",
            )
        })?;
        operation(&mut store)
    }
}

/// A typed `INVALID_STATE` refusal.
fn invalid_state(detail: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorCode::InvalidState, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(volume: &str) -> Participant {
        Participant {
            volume_id: VolumeId::new(volume).expect("valid id"),
            expected_generation: 1,
            resource: format!("res-{volume}"),
            minor: 1,
        }
    }

    fn participants(volumes: &[&str]) -> Vec<Participant> {
        volumes.iter().map(|v| participant(v)).collect()
    }

    #[test]
    fn batch_operation_ids_are_deterministic() {
        let migration = MigrationId::new("mig-1").expect("valid id");
        let set = participants(&["vol-a", "vol-b"]);
        let first = batch_operation_id(&migration, BatchStep::RevokeSet, &set).expect("derive");
        let second = batch_operation_id(&migration, BatchStep::RevokeSet, &set).expect("derive");
        assert_eq!(first, second);

        // A different step, set or order yields a different id.
        assert_ne!(
            first,
            batch_operation_id(&migration, BatchStep::GrantSet, &set).expect("derive")
        );
        assert_ne!(
            first,
            batch_operation_id(&migration, BatchStep::RevokeSet, &participants(&["vol-a"]))
                .expect("derive")
        );
        assert_ne!(
            first,
            batch_operation_id(
                &migration,
                BatchStep::RevokeSet,
                &participants(&["vol-b", "vol-a"])
            )
            .expect("derive")
        );

        // The id is stable across process restarts (a pure function of
        // its inputs) and carries the step tag.
        assert!(first.as_str().starts_with("mig-revoke-set-"));
        assert_eq!(first.as_str().len(), "mig-revoke-set-".len() + 16);
    }

    #[test]
    fn barrier_operation_ids_are_deterministic_and_volume_scoped() {
        let migration = MigrationId::new("mig-1").expect("valid id");
        let vol_a = VolumeId::new("vol-a").expect("valid id");
        let vol_b = VolumeId::new("vol-b").expect("valid id");
        assert_eq!(
            barrier_operation_id(&migration, &vol_a).expect("derive"),
            barrier_operation_id(&migration, &vol_a).expect("derive")
        );
        assert_ne!(
            barrier_operation_id(&migration, &vol_a).expect("derive"),
            barrier_operation_id(&migration, &vol_b).expect("derive")
        );
        assert!(
            barrier_operation_id(&migration, &vol_a)
                .expect("derive")
                .as_str()
                .starts_with("mig-record-barrier-")
        );
    }
}
