//! The native-local LVM provider: thick logical volumes on claimed disks.
//!
//! [`LvmProvider`] implements [`VolumeProvider`] for
//! [`VolumeClass::NativeLocal`]: volumes are thick LVs created with
//! `lvcreate` on volume groups established by an explicit
//! [`claim_device`](crate::admin) under a destructive-authorization token.
//! Every LVM interaction goes through the shell-free
//! [`CommandRunner`]; every mutation is persisted to the
//! durable JSON state after the backend confirmed it, and effective sizes
//! are always verified against `lvs` output — a successful exit status
//! alone is never trusted as evidence (honest reporting).
//!
//! # Durable mutation contracts
//!
//! Every privileged mutation journals a durable intent before the
//! allocating or destructive step, and startup reconciliation resolves
//! the intent against the observed LVM world:
//!
//! - **Create**: the intent is persisted before `lvcreate`. Startup
//!   adopts an intent-owned LV only when it is present and at least the
//!   recorded requested size (a smaller LV fails startup rather than
//!   adopting a wrong geometry; a missing LV drops the intent). The
//!   final save commits the volume record and drops the intent in one
//!   atomic write; a failed final save compensates with a checked
//!   `lvremove` and a cleared state, except when the durable file
//!   already holds this volume's record — then the volume and LV are
//!   kept (the post-rename carve-out). An intent that a failing cleanup
//!   could not clear may complete/adopt on a later restart rather than
//!   leak an untracked LV.
//! - **Grow**: the intent is persisted before `lvextend`. Startup rolls
//!   the grow forward (at or above the request completes the record and
//!   bumps the generation; still at the from-size drops the intent;
//!   anything else fails startup rather than guessing). A failed final
//!   save keeps the observed new size in memory and leaves the durable
//!   grow intent for restart; the LV is **never** shrunk as
//!   compensation (data may already live in the new extents).
//! - **Delete**: the intent is persisted before `blkdiscard`/
//!   `lvremove`. Startup completes the recorded delete; a
//!   stale-generation intent is dropped without any deletion (never
//!   delete on a stale authorization), foreign LVs are never touched
//!   (AGENTS rule 7), and an unsettled online move refuses the delete
//!   before any destructive command. A failed destructive step keeps
//!   the durable intent so the volume stays fenced until a retry or
//!   restart completes it; completion removes the settled move record
//!   with the volume and intent in one atomic save.
//! - **Move**: `PREPARING` is the reconciliation anchor. A failed
//!   `PREPARING` save starts no `pvmove`; a failed `COPYING`/`COMPLETE`
//!   save restores the prior durable record in memory and leaves
//!   classification to startup/reconcile — the world is never assumed
//!   lost and never blindly restarted.
//!
//! Create is fail-closed: if the state save fails, the just-created LV is
//! removed with a checked `lvremove` and the in-memory entry is dropped (a
//! failing cleanup names the exact manual `lvremove` and leaves the LV
//! visible in `lvs` but absent from provider state), except when the durable
//! state already contains this volume's record — then the durable record,
//! the in-memory entry and the LV are all kept. In that last case the API
//! serves the volume; a retry with a new `operation_id` and the same payload
//! is idempotent, while reusing the failed `operation_id` replays the
//! failure or fails closed as in-doubt.
//!
//! Thick LVM rounds logical-volume sizes **up** to whole physical extents
//! (4 MiB by default). The provider therefore treats
//! `actual_lvm_size >= requested_size` as success, persists and reports
//! the *effective* (rounded) size, and keeps the *requested* size for
//! idempotent-create replay comparison.
//!
//! P0 honesty constraints: attach returns a `Prepared` (never `Active`)
//! frontend handle because no VMM integration exists; health and backend
//! health are `Unknown` until proven; `evidence_status` is
//! `PrototypeOnly`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};
use volvisor_provider::VolumeProvider;
use volvisor_provider::semantics::{
    AttachmentIdentity, AttachmentRecordView, AttachmentStateRecord, apply_attachment_report,
    ensure_attachment_id_not_retired, ensure_expected_attachment_generation,
    ensure_expected_generation, ensure_no_active_attachment, resolve_attachment_replay,
};
use volvisor_types::domain::{
    AccessMode, AttachmentEvidence, EffectiveProtection, EvidenceStatus, FailureDomain, Frontend,
    Health, Provisioning, VolumeClass,
};

use volvisor_types::crash::StoreCrashHooks;
use volvisor_types::request::{
    AttachVolumeRequest, AttachVolumeResponse, AttachmentView, CreateVolumeRequest,
    DeleteVolumeRequest, DetachVolumeRequest, ErasurePolicy, GrowGuestNotification,
    GrowVolumeRequest, GrowVolumeResponse, InspectVolumeResponse, LocalProtectionModeRequest,
    MoveVolumeBackingRequest, MoveVolumeBackingResponse, ReportAttachmentStateRequest,
    ReportAttachmentStateResponse,
};
use volvisor_types::{
    ApiError, ApiErrorCode, AttachmentId, AttachmentState, Capability, CapabilitySet, DeviceId,
    HostId, MoveVolumeBackingState, ProjectId, VolumeId, VolumeLifecycle, unix_ms_now,
    validate_api_version,
};

use crate::report::{LvRow, VgRow};
use crate::state::{
    AttachmentRecord, CreateIntent, DeleteIntent, GrowIntent, LvmState, StoredVolume, VolumeEntry,
    VolumeRuntime,
};
use crate::{CommandOutput, CommandRunner};

/// Stable provider name for diagnostics (never a secret).
pub const PROVIDER_NAME: &str = "lvm-native-local";

/// The only volume class served by this provider.
static SUPPORTED_CLASSES: &[VolumeClass] = &[VolumeClass::NativeLocal];

/// Logical block size assumed when a create request omits one.
const DEFAULT_BLOCK_SIZE: u32 = 4096;

/// Free-space headroom kept in every volume group so LVM metadata can
/// always be written (`NO_SAFE_CAPACITY` is reported before the VG fills).
pub(crate) const VG_HEADROOM_BYTES: u64 = 4 << 20;

/// Physical extent size assumed when `vgs` does not report one (LVM's
/// own default; bigger arrays legitimately use bigger extents).
const DEFAULT_EXTENT_BYTES: u64 = 4 << 20;

/// The free-space picture of one volume group.
///
/// Capacity checks need both numbers: thick LVM rounds every allocation
/// up to whole physical extents, so the *effective* demand of a size
/// request is [`extent_rounded`] against `extent_bytes`.
#[derive(Clone, Copy, Debug)]
struct VgCapacity {
    /// Free bytes reported by `vgs`.
    free_bytes: u64,
    /// Physical extent bytes reported by `vgs` (4 MiB when absent).
    extent_bytes: u64,
}

/// The space `size` bytes effectively occupy on a VG with `extent`-byte
/// physical extents: `size` rounded up to a whole multiple of `extent`.
///
/// A zero or absent extent is treated as "no rounding" (guarded, never a
/// division by zero).
fn extent_rounded(size: u64, extent: u64) -> u64 {
    if extent == 0 {
        return size;
    }
    size.div_ceil(extent) * extent
}

/// Host identity of the single-host P0 daemon.
const LOCAL_HOST_ID: &str = "local";

/// The move drive's observation poll interval while a `pvmove` runs
/// (the brief's 250–500 ms band, centered).
const MOVE_POLL_INTERVAL: Duration = Duration::from_millis(400);

/// How long one `move_volume_backing` call supervises an in-flight
/// move before returning `COPYING` honestly. The move itself keeps
/// running (the kernel mirror is outside the daemon): a fresh
/// operation re-attaches, and the daemon's retry reconcile completes
/// the record without any consumer call.
const MOVE_SUPERVISION_WINDOW: Duration = Duration::from_secs(30);

/// The move retry-reconcile tick (the background task spawned by the
/// daemon's LVM path).
pub const MOVE_RETRY_TICK: Duration = Duration::from_secs(5);

/// How many consecutive world-observe failures the retry reconcile
/// tolerates for one record before parking it `IN_DOUBT` (the
/// unobservable-LV limbo bound). Without it, an out-of-band
/// `lvremove` of a moving LV would leave its record retried forever
/// with no surfaced state; at the bound the record parks with a
/// typed reason naming the unobservable LV. The count is in-memory
/// and per-incarnation — a daemon restart resets it, never the
/// honesty of any single pass.
const MOVE_OBSERVE_FAILURE_BOUND: u32 = 3;

/// The move drive's timing knobs (the production defaults above,
/// overridable for the deterministic fake world and the fault rows —
/// never for a production daemon).
#[derive(Clone, Copy, Debug)]
pub struct MoveTiming {
    /// The `lvs` observation interval while supervising.
    pub poll_interval: Duration,
    /// One call's supervision window.
    pub supervision_window: Duration,
}

impl Default for MoveTiming {
    fn default() -> Self {
        Self {
            poll_interval: MOVE_POLL_INTERVAL,
            supervision_window: MOVE_SUPERVISION_WINDOW,
        }
    }
}

/// What the world was observed to say about one LV being moved: the
/// honest `lvs`-observable truth the whole move discipline rests on
/// (the record plus this observation are the crash model's two
/// survivors; the kernel dm mirror and the backgrounded `pvmove`
/// process are outside the daemon).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MoveObservation {
    /// A `pvmove` mirror segment is active (the LV's device list
    /// references a `pvmove*` segment).
    pub moving: bool,
    /// The LV's backing device names (extent suffixes stripped; the
    /// `pvmove*` segment name included while moving).
    pub devices: Vec<String>,
}

impl MoveObservation {
    /// The relocation proof: no move is active and the source PV no
    /// longer backs the LV.
    #[must_use]
    pub(crate) fn source_freed(&self, source_pv: &str) -> bool {
        !self.moving && !self.devices.iter().any(|pv| pv == source_pv)
    }

    /// The relocation proof for the same-VG scope: no move is
    /// active and the LV's extents sit on **exactly the named
    /// target** — off the source *and* on the target, never a
    /// third PV (a foreign relocation is not this move's
    /// completion, and a completion claim names where the extents
    /// are).
    #[must_use]
    pub(crate) fn relocated_to(&self, source_pv: &str, target_pv: &str) -> bool {
        !self.moving
            && self.devices.len() == 1
            && self.devices[0] == target_pv
            && source_pv != target_pv
    }

    /// The verified-rollback proof: no move is active and the LV's
    /// extents sit on **exactly the record's source PV** — the
    /// pre-move placement. The relocation verifiably did not happen
    /// (an out-of-band `pvmove --abort` that completed the rollback,
    /// or a move that never started), so a parked `IN_DOUBT` record
    /// may resolve `FAILED` with the source intact and serving. The
    /// mixed-placement world (segments on both PVs — the honest
    /// shape of LVM 2.03.x's non-atomic abort) is **not** this and
    /// stays parked: it is genuinely unresolvable by observation
    /// alone.
    #[must_use]
    pub(crate) fn restored_to_source(&self, source_pv: &str) -> bool {
        !self.moving && self.devices.len() == 1 && self.devices[0] == source_pv
    }
}

/// One `move_reconcile_pass` outcome report (the retry task logs it;
/// the fault rows assert on it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MoveReconcileReport {
    /// Moves whose completion was observed and verified this pass.
    pub completed: Vec<VolumeId>,
    /// Records rolled to `COPYING` (a live mirror observed, or a
    /// re-driven start).
    pub marked_copying: Vec<VolumeId>,
    /// Records parked `IN_DOUBT` (the move ended without relocating).
    pub parked_in_doubt: Vec<VolumeId>,
    /// Records resolved `FAILED` by this pass: a parked `IN_DOUBT`
    /// whose world verifiably shows the pre-move placement (the
    /// relocation did not happen; the source intact and serving).
    pub resolved_failed: Vec<VolumeId>,
    /// Records parked `IN_DOUBT` at the unobservable-LV bound: the
    /// world could not be observed for `MOVE_OBSERVE_FAILURE_BOUND`
    /// consecutive passes, so the record surfaces instead of
    /// silently spinning.
    pub parked_unobservable: Vec<VolumeId>,
    /// PREPARING records whose `pvmove` was (re-)started this pass.
    pub redriven: Vec<VolumeId>,
    /// Records dropped (their volume no longer exists).
    pub dropped: Vec<VolumeId>,
    /// Records skipped: the world could not be observed this pass
    /// (retried on the next tick; never resolved destructively).
    pub unobservable: Vec<VolumeId>,
}

/// The native-local LVM provider.
///
/// All state lives in the durable JSON state file (see [`LvmState`]); the
/// in-memory `Mutex` only serializes access within this daemon. The
/// constructor runs a reconciliation pass: state entries whose LV no
/// longer exists in LVM are marked `Failed` (and persisted); foreign LVs
/// under our volume groups are *reported* by
/// [`reconcile_report`](crate::admin) and never touched (AGENTS rule 7).
pub struct LvmProvider {
    /// Shell-free command executor for the LVM toolchain.
    pub(crate) runner: Arc<dyn CommandRunner>,
    /// Path of the durable JSON state file.
    pub(crate) state_path: PathBuf,
    /// Filesystem root for discovery (`/dev/disk/by-id` lives under it).
    pub(crate) sysfs_root: PathBuf,
    /// Prefix for volume groups created by `claim_device`.
    pub(crate) vg_prefix: String,
    /// Scoped destructive-authorization token (compared, never logged).
    pub(crate) expected_auth_token: String,
    /// The state file's store-save crash seam (inert unless a rig
    /// arms it — the move fault rows' kill surface).
    store_crash: Arc<StoreCrashHooks>,
    /// The move drive's timing (production defaults; overridable for
    /// the deterministic fake world only).
    move_timing: MoveTiming,
    /// Consecutive world-observe failures per move record, counted
    /// by the retry reconcile against
    /// [`MOVE_OBSERVE_FAILURE_BOUND`] (the unobservable-LV limbo
    /// bound). In-memory and per-incarnation: it never persists, and
    /// a successful observation clears it.
    move_observe_failures: Mutex<BTreeMap<VolumeId, u32>>,
    state: Mutex<LvmState>,
}

/// The phase-1 admission outcome for a move request.
enum MoveAdmission {
    /// Answer now: the idempotent re-observation of a completed
    /// move — nothing is touched, nothing re-runs.
    Answered(MoveVolumeBackingResponse),
    /// Qualified for the drive: the LV's names plus the active
    /// move's target PV when the request re-attaches to one.
    Drive {
        vg_name: String,
        lv_name: String,
        existing_target: Option<String>,
    },
}

/// What a pending grow intent says about this grow request.
enum GrowResume {
    /// No pending intent: a fresh grow must persist one.
    NoIntent,
    /// The intent matches and the LV is still at its from-size: run
    /// `lvextend` without writing another intent.
    Continue,
    /// The grow already landed: the record was finalized from the
    /// observed effective size and the response reports it.
    Finalized(u64),
}

impl LvmProvider {
    /// Construct the provider.
    ///
    /// Loads the durable state (a missing file is a fresh, empty state) and
    /// immediately reconciles it against real LVM state: volumes whose LV
    /// vanished are marked `Failed`, and device claims whose volume group
    /// is verifiably absent from a successful `vgs` query are dropped
    /// (claims are kept when the query fails — an honest unknown is never
    /// resolved destructively). `vg_prefix` namespaces the volume
    /// groups created by [`claim_device`](crate::admin);
    /// `expected_auth_token` is the scoped destructive-authorization token
    /// required for device claiming and release (it is compared, never
    /// logged).
    pub fn new(
        runner: Arc<dyn CommandRunner>,
        state_path: PathBuf,
        sysfs_root: PathBuf,
        vg_prefix: String,
        expected_auth_token: String,
    ) -> Result<Self, ApiError> {
        let state = LvmState::load(&state_path)?;
        let provider = Self {
            runner,
            state_path,
            sysfs_root,
            vg_prefix,
            expected_auth_token,
            store_crash: Arc::new(StoreCrashHooks::new()),
            move_timing: MoveTiming::default(),
            move_observe_failures: Mutex::new(BTreeMap::new()),
            state: Mutex::new(state),
        };
        provider.reconcile()?;
        Ok(provider)
    }

    /// Override the move drive's timing (the deterministic fake
    /// world and the fault rows poll in milliseconds and bound the
    /// supervision window tightly; a production daemon keeps the
    /// defaults). Must be called before the first move.
    #[must_use]
    pub fn with_move_timing(mut self, timing: MoveTiming) -> Self {
        self.move_timing = timing;
        self
    }

    /// The state file's crash seam (the rig arms it to kill at a
    /// move record's commit boundaries).
    #[must_use]
    pub fn store_crash_hooks(&self) -> &Arc<StoreCrashHooks> {
        &self.store_crash
    }

    /// Persist the in-memory state through the crash-hooked atomic
    /// save (every internal mutation — volume, move and admin claim —
    /// routes through here, so the armed seam sees every commit
    /// boundary of the state file).
    pub(crate) fn persist(&self, state: &mut LvmState) -> Result<(), ApiError> {
        state.save_with_hooks(&self.state_path, Some(&self.store_crash))
    }

    /// Count one more consecutive world-observe failure for a move
    /// record (the unobservable-LV limbo bound's counter; advisory
    /// only — a poisoned lock reports past the bound so the record
    /// parks, the fail-closed direction, never spins).
    fn bump_move_observe_failures(&self, volume_id: &VolumeId) -> u32 {
        self.move_observe_failures
            .lock()
            .map_or(MOVE_OBSERVE_FAILURE_BOUND, |mut failures| {
                let count = failures.entry(volume_id.clone()).or_insert(0);
                *count += 1;
                *count
            })
    }

    /// Clear a record's consecutive world-observe failure count (a
    /// successful observation, a completed record, or a dropped one).
    fn clear_move_observe_failures(&self, volume_id: &VolumeId) {
        if let Ok(mut failures) = self.move_observe_failures.lock() {
            failures.remove(volume_id);
        }
    }

    /// Lock the in-memory state, mapping poisoning to `INTERNAL`.
    pub(crate) fn lock_state(&self) -> Result<MutexGuard<'_, LvmState>, ApiError> {
        self.state.lock().map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "LVM provider state lock poisoned by a previous failure",
            )
        })
    }

    /// The attachment enumeration the daemon's grow-notification
    /// engine drives (P6-B, ADR-0006 first slice part 1): every
    /// volume with an attachment, derived from the provider's own
    /// durable state under its lock — the participant-facts pattern,
    /// never a consumer assertion. `current_size_bytes` is the
    /// volume's current size (the notification target; sizes are
    /// grow-only, so it never decreases). An attachment without a
    /// recorded `vmm_disk_id` reports `Unaddressable`: the
    /// notification is refused with a recorded reason, never a
    /// silent `not_applicable` (a frontend exists).
    pub fn grow_attachment_facts(
        &self,
    ) -> Result<BTreeMap<VolumeId, volvisor_provider::AttachmentForGrow>, ApiError> {
        let state = self.lock_state()?;
        Ok(state
            .volumes()
            .iter()
            .filter_map(|(volume_id, stored)| {
                let record = stored.runtime.attachment.as_ref()?;
                Some((
                    volume_id.clone(),
                    match &record.vmm_disk_id {
                        Some(vmm_disk_id) => volvisor_provider::AttachmentForGrow::Addressable {
                            vm_id: record.vm_id.clone(),
                            vmm_disk_id: vmm_disk_id.clone(),
                            current_size_bytes: stored.entry.size_bytes,
                        },
                        None => volvisor_provider::AttachmentForGrow::Unaddressable,
                    },
                ))
            })
            .collect())
    }

    /// Run `lvs` and return its report rows.
    pub(crate) fn list_lvs(&self) -> Result<Vec<LvRow>, ApiError> {
        let output = self.runner.run("lvs", lvm_json_args())?;
        if !output.success {
            return Err(command_failed("lvs", &output));
        }
        crate::report::parse_report(&output.stdout, "lv")
    }

    /// Run `vgs` and return its report rows.
    ///
    /// The invocation explicitly requests `vg_extent_size` (it is not
    /// part of `vgs`' default columns): thick LVM rounds every
    /// allocation up to whole physical extents, so capacity checks must
    /// know the extent size to compare against the *rounded* demand.
    pub(crate) fn list_vgs(&self) -> Result<Vec<VgRow>, ApiError> {
        let output = self.runner.run("vgs", vgs_json_args())?;
        if !output.success {
            return Err(command_failed("vgs", &output));
        }
        crate::report::parse_report(&output.stdout, "vg")
    }

    /// Run `pvs` and return its report rows.
    pub(crate) fn list_pvs(&self) -> Result<Vec<crate::report::PvRow>, ApiError> {
        let output = self.runner.run("pvs", lvm_json_args())?;
        if !output.success {
            return Err(command_failed("pvs", &output));
        }
        crate::report::parse_report(&output.stdout, "pv")
    }

    /// Free space and extent size per volume group name, from one `vgs`
    /// query.
    fn vg_capacity_map(&self) -> Result<BTreeMap<String, VgCapacity>, ApiError> {
        let mut map = BTreeMap::new();
        for row in self.list_vgs()? {
            if let (Some(name), Some(free)) = (row.vg_name.clone(), row.free_bytes()) {
                map.insert(
                    name,
                    VgCapacity {
                        free_bytes: free,
                        // Permissive: 4 MiB when `vgs` does not report
                        // the extent size (LVM's own default).
                        extent_bytes: row.extent_bytes().unwrap_or(DEFAULT_EXTENT_BYTES),
                    },
                );
            }
        }
        Ok(map)
    }

    /// Free space and extent size of one volume group; `INTERNAL`
    /// (honest) when `vgs` does not report the group at all.
    fn vg_capacity(&self, vg_name: &str) -> Result<VgCapacity, ApiError> {
        self.vg_capacity_map()?
            .get(vg_name)
            .copied()
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!("volume group {vg_name:?} is not reported by vgs"),
                )
            })
    }

    /// Actual size of one LV as reported by `lvs`, when it exists.
    pub(crate) fn lv_size(&self, vg_name: &str, lv_name: &str) -> Result<Option<u64>, ApiError> {
        let wanted = format!("{vg_name}/{lv_name}");
        Ok(self
            .list_lvs()?
            .into_iter()
            .find(|row| row.vg_slash_lv().as_deref() == Some(wanted.as_str()))
            .and_then(|row| row.size_bytes()))
    }

    /// Verify a freshly created LV and return its effective size.
    ///
    /// Success is `actual >= requested`: thick LVM rounds sizes up to
    /// whole physical extents, so a non-extent-aligned request legitimately
    /// yields a larger LV. If the LV is missing or smaller than requested
    /// (which should be impossible), a best-effort `lvremove` cleans the
    /// orphaned LV up before the honest `INTERNAL` error is returned, so
    /// no half-created volume is left behind.
    fn verify_created_size(
        &self,
        vg_name: &str,
        lv_name: &str,
        requested: u64,
    ) -> Result<u64, ApiError> {
        match self.lv_size(vg_name, lv_name)? {
            Some(actual) if actual >= requested => Ok(actual),
            None => {
                self.remove_lv_best_effort(vg_name, lv_name);
                Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!("lvcreate reported success but lvs does not list {vg_name}/{lv_name}"),
                ))
            }
            Some(actual) => {
                self.remove_lv_best_effort(vg_name, lv_name);
                Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "LV size mismatch after lvcreate of {vg_name}/{lv_name}: requested \
                         {requested} bytes, LVM reports {actual} bytes"
                    ),
                ))
            }
        }
    }

    /// Best-effort `lvremove` used to clean up a failed create.
    ///
    /// The outcome is deliberately swallowed: the caller is already on an
    /// error path, and a failing cleanup must not mask the original
    /// failure.
    fn remove_lv_best_effort(&self, vg_name: &str, lv_name: &str) {
        drop(
            self.runner
                .run("lvremove", &["--yes", &format!("{vg_name}/{lv_name}")]),
        );
    }

    /// Reconcile a create whose final state save failed (issue #58,
    /// issue #70).
    ///
    /// The in-memory volume entry has already been inserted and the
    /// create intent removed (the final save commits both together).
    /// Normally the old durable file is still intact, so the
    /// just-created LV is removed with a **checked** `lvremove` (unlike
    /// [`Self::remove_lv_best_effort`], a failing cleanup is never
    /// swallowed) and the memory entry is dropped; the cleared state is
    /// then persisted so the durable create intent does not survive the
    /// compensation. Memory, durable state and LVM then all agree the
    /// volume does not exist. The original save error is preserved;
    /// when the cleanup itself fails the exact manual remediation is
    /// appended and the LV is described as what an operator can verify:
    /// still present in `lvs`, absent from provider state.
    ///
    /// Carve-out: if the atomic save's rename already landed and only a
    /// later step (the directory fsync) failed, the durable file contains
    /// the volume. All three sources then agree it exists, so the memory
    /// entry and the LV are kept and the error says the record nonetheless
    /// reached the state file. The durable file is re-read to decide; a
    /// load error is treated as *not* committed (the fail-closed direction).
    /// The decision is presence-based: a record for this volume left by an
    /// earlier failed mutation takes the same keep branch, which is still
    /// the safe direction (all sources say the volume exists).
    ///
    /// If the LV removal fails **and** clearing the durable create intent
    /// fails, the error says the durable create intent remains and
    /// startup recovery will adopt the LV only if it is present and at
    /// least the requested size; otherwise a manual `lvremove` is
    /// required. Neither failure ever masks the original save error.
    fn compensate_unpersisted_create(
        &self,
        state: &mut LvmState,
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        save_error: ApiError,
    ) -> ApiError {
        let committed = LvmState::load(&self.state_path)
            .is_ok_and(|durable| durable.volume(volume_id).is_some());
        if committed {
            return ApiError::new(
                save_error.code,
                format!(
                    "{}; the durable state already contains this volume's record, so volume \
                     {volume_id} and its LV {vg_name}/{lv_name} were kept; the API serves the \
                     volume, and a retry with a new operation_id and the same payload returns it \
                     (reusing the failed operation_id replays the failure, or fails closed as \
                     in-doubt when the journal could not record the outcome)",
                    save_error.detail
                ),
            );
        }

        let removed = self
            .runner
            .run("lvremove", &["--yes", &format!("{vg_name}/{lv_name}")])
            .is_ok_and(|output| output.success);
        state.remove_volume(volume_id);
        let mut detail = save_error.detail;
        if !removed {
            detail.push_str(
                "; in-memory state was rolled back but the LV could not be removed: manual \
                 lvremove ",
            );
            detail.push_str(vg_name);
            detail.push('/');
            detail.push_str(lv_name);
            detail.push_str(
                " required (the LV remains visible in lvs and is absent from provider state)",
            );
        }
        // The create intent was removed from memory before the failed
        // final save; this save clears it durably so no stale intent
        // outlives the compensation. Classify the clear's own failure:
        // the rename may have landed, in which case the durable intent
        // is gone and the compensation is complete.
        if let Err(clear_error) = self.persist(state) {
            match LvmState::load(&self.state_path) {
                Ok(durable) if durable.create_intent(volume_id).is_some() => {
                    detail.push_str(
                        "; the durable create intent could not be cleared (the failed save \
                         left it in place) and remains durable: startup recovery will adopt the \
                         LV only if it is present and at least the requested size",
                    );
                    if removed {
                        detail.push_str(
                            "; the LV was removed, so recovery will drop the intent once it \
                             observes no such LV",
                        );
                    } else {
                        detail.push_str("; otherwise manual lvremove ");
                        detail.push_str(vg_name);
                        detail.push('/');
                        detail.push_str(lv_name);
                        detail.push_str(" is required");
                    }
                }
                Ok(_) => {
                    // The clear save's rename landed: the intent is gone
                    // and memory/durable agree on the compensated state.
                    detail.push_str(
                        "; the compensating clear save reported an error but the durable \
                         state no longer records the create intent (its rename landed), so the \
                         compensated state is durable",
                    );
                }
                Err(load_error) => {
                    detail.push_str(
                        "; the durable create intent could not be verified after the failed \
                         clear save",
                    );
                    detail.push_str(" (");
                    detail.push_str(&load_error.to_string());
                    detail.push(')');
                    detail.push_str(
                        ": treat it as possibly durable; startup recovery will adopt the LV \
                         only if it is present and at least the requested size",
                    );
                    if removed {
                        detail.push_str(
                            "; the LV was removed, so recovery will drop the intent once it \
                             observes no such LV",
                        );
                    } else {
                        detail.push_str("; otherwise manual lvremove ");
                        detail.push_str(vg_name);
                        detail.push('/');
                        detail.push_str(lv_name);
                        detail.push_str(" is required");
                    }
                }
            }
            let cleanup_note = format!(" (cleanup save failed: {clear_error})");
            detail.push_str(&cleanup_note);
        }
        ApiError::new(save_error.code, detail)
    }

    /// Refuse an operation that would grant new authority over a
    /// volume with a durable pending delete.
    ///
    /// A pending delete means the LV may already be discarded/removed
    /// or is about to be; attaching, growing, starting a move or
    /// replaying the create would race that authorization. Delete
    /// retries are allowed (they are the completion path). The check
    /// reads the durable intent in the caller's already-held state
    /// guard — never a second lock.
    #[allow(clippy::unused_self)]
    fn ensure_no_pending_delete(
        &self,
        state: &LvmState,
        volume_id: &VolumeId,
    ) -> Result<(), ApiError> {
        if let Some(intent) = state.delete_intent(volume_id) {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "volume {volume_id} has a durable pending delete authorized at generation \
                     {}; no create replay, attachment, grow or move is admitted until that \
                     delete is completed (retry the delete or restart to let recovery complete \
                     it)",
                    intent.expected_generation
                ),
            ));
        }
        Ok(())
    }

    /// Run the destructive delete steps for one LV: the
    /// `ZeroDiscard` `blkdiscard` first (never followed by `lvremove`
    /// when it fails — the caller asked for erased data and must learn
    /// the truth), then the checked `lvremove`. Shared by the live
    /// delete path and startup completion of a durable delete intent.
    fn discard_and_remove_lv(
        &self,
        vg_name: &str,
        lv_name: &str,
        policy: ErasurePolicy,
    ) -> Result<(), ApiError> {
        if matches!(policy, ErasurePolicy::ZeroDiscard) {
            let device = format!("/dev/{vg_name}/{lv_name}");
            let output = self.runner.run("blkdiscard", &["-f", &device])?;
            if !output.success {
                return Err(command_failed("blkdiscard", &output));
            }
        }
        let output = self
            .runner
            .run("lvremove", &["--yes", &format!("{vg_name}/{lv_name}")])?;
        if !output.success {
            return Err(command_failed("lvremove", &output));
        }
        Ok(())
    }

    /// Build the stored volume an adopted create intent describes.
    ///
    /// Used by both the retry path and startup recovery, so an adopted
    /// LV has exactly the record a successful create would have
    /// written: generation 1, data epoch 0, `Ready`, unattached, the
    /// observed effective size, and the intent's requested size,
    /// project, block size and canonical payload.
    fn stored_volume_from_create_intent(intent: &CreateIntent, observed: u64) -> StoredVolume {
        StoredVolume {
            entry: VolumeEntry {
                vg_name: intent.vg_name.clone(),
                lv_name: intent.lv_name.clone(),
                size_bytes: observed,
                requested_size_bytes: intent.requested_size_bytes,
                generation: 1,
                data_epoch: 0,
                project_id: intent.project_id.clone(),
                block_size: intent.block_size,
                creation_payload: intent.creation_payload.clone(),
            },
            runtime: VolumeRuntime {
                state: VolumeLifecycle::Ready,
                attachment: None,
                retired_attachment_ids: Vec::new(),
            },
        }
    }

    /// Remove a create intent after a failed `lvcreate` invocation
    /// (runner error or non-success exit) and attempt to clear it
    /// durably.
    ///
    /// The original command error is returned unchanged when the
    /// removal save succeeds. When it fails, the cleanup failure is
    /// appended and the intent is left durable for startup recovery.
    fn clear_create_intent_after_failed_start(
        &self,
        state: &mut LvmState,
        volume_id: &VolumeId,
        error: ApiError,
    ) -> ApiError {
        state.remove_create_intent(volume_id);
        match self.persist(state) {
            Ok(()) => error,
            Err(clear_error) => {
                let note = match LvmState::load(&self.state_path) {
                    Ok(durable) if durable.create_intent(volume_id).is_some() => format!(
                        "the create intent could not be cleared durably ({clear_error}) and \
                         remains durable; startup recovery will drop it if no intent-owned LV is \
                         present, adopt it only if present and at least the requested size, and \
                         otherwise refuse rather than adopt a wrong geometry"
                    ),
                    Ok(_) => format!(
                        "the create-intent clear save reported an error ({clear_error}), but \
                         the durable state no longer records that intent (its rename landed); \
                         the failed start left no durable intent to recover"
                    ),
                    Err(load_error) => format!(
                        "the create intent could not be verified after its clear save failed \
                         ({clear_error}; re-read failed: {load_error}); treat it as possibly \
                         durable — startup recovery will drop it if no intent-owned LV is \
                         present and adopt it only if present and at least the requested size"
                    ),
                };
                ApiError::new(error.code, format!("{}; {note}", error.detail))
            }
        }
    }

    /// Remove a grow intent after a failed `lvextend` invocation
    /// (runner error or non-success exit) and attempt to clear it
    /// durably.
    ///
    /// The original command error is returned unchanged when the
    /// removal save succeeds. When it fails, the cleanup failure is
    /// appended and the intent is left durable for startup recovery.
    fn clear_grow_intent_after_failed_start(
        &self,
        state: &mut LvmState,
        volume_id: &VolumeId,
        error: ApiError,
    ) -> ApiError {
        state.remove_grow_intent(volume_id);
        match self.persist(state) {
            Ok(()) => error,
            Err(clear_error) => {
                let note = match LvmState::load(&self.state_path) {
                    Ok(durable) if durable.grow_intent(volume_id).is_some() => format!(
                        "the grow intent could not be cleared durably ({clear_error}) and \
                         remains for recovery, which completes the grow only if the LV is at \
                         least the requested size"
                    ),
                    Ok(_) => format!(
                        "the grow-intent clear save reported an error ({clear_error}), but the \
                         durable state no longer records that intent (its rename landed); the \
                         failed start left no durable grow to recover"
                    ),
                    Err(load_error) => format!(
                        "the grow intent could not be verified after its clear save failed \
                         ({clear_error}; re-read failed: {load_error}); treat it as possibly \
                         durable — recovery completes the grow only if the LV is at least the \
                         requested size"
                    ),
                };
                ApiError::new(error.code, format!("{}; {note}", error.detail))
            }
        }
    }

    /// Classify a failed final grow save without ever shrinking the LV.
    ///
    /// The in-memory entry already carries the observed new size and
    /// the bumped generation, and the grow intent was removed from
    /// memory. If the durable file already records that generation and
    /// a size at least `actual` (the post-rename carve-out), memory is
    /// kept and the error says the durable state already contains the
    /// grow. Otherwise memory also keeps the observed shape (the honest
    /// current world) and the error says the durable grow intent will
    /// complete the grow on restart. A load error is treated as *not*
    /// committed (the fail-closed direction).
    fn persist_completed_grow(
        &self,
        mut save_error: ApiError,
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        actual: u64,
        new_generation: u64,
    ) -> ApiError {
        let committed = LvmState::load(&self.state_path).is_ok_and(|durable| {
            durable.volume(volume_id).is_some_and(|stored| {
                stored.entry.generation == new_generation && stored.entry.size_bytes >= actual
            })
        });
        if committed {
            return ApiError::new(
                save_error.code,
                format!(
                    "{}; the durable state already contains the grow of {vg_name}/{lv_name} \
                     (generation {new_generation}, size at least {actual} bytes), so the \
                     in-memory completion was kept",
                    save_error.detail
                ),
            );
        }
        let grow_note = format!(
            "; the LV {vg_name}/{lv_name} was extended to {actual} bytes but the grow record \
             could not be saved; the durable grow intent will complete the grow on restart, and \
             inspect reports the observed size (the LV is never shrunk as compensation). A retry \
             of the same request before a restart fails closed as a stale generation"
        );
        save_error.detail.push_str(&grow_note);
        save_error
    }

    /// Pick the first claimed native-pool volume group with enough free
    /// space for the **extent-rounded** demand (headroom included);
    /// `NO_SAFE_CAPACITY` when none qualifies.
    fn pick_pool_vg(&self, state: &LvmState, size_bytes: u64) -> Result<String, ApiError> {
        let capacity_map = self.vg_capacity_map()?;
        for entry in state.devices().values() {
            if entry.role != volvisor_types::DeviceRole::NativePool {
                continue;
            }
            if let Some(capacity) = capacity_map.get(&entry.vg_name) {
                let demand = extent_rounded(size_bytes, capacity.extent_bytes);
                if demand.saturating_add(VG_HEADROOM_BYTES) <= capacity.free_bytes {
                    return Ok(entry.vg_name.clone());
                }
            }
        }
        Err(ApiError::new(
            ApiErrorCode::NoSafeCapacity,
            format!(
                "no safe capacity: no claimed native-local pool has {size_bytes} bytes \
                 (extent-rounded) free"
            ),
        ))
    }

    /// Startup reconciliation.
    ///
    /// The intent passes run first, then the volume and device passes,
    /// all against one `lvs` snapshot:
    ///
    /// - **Create intents**: an existing durable volume wins (the
    ///   intent is dropped). An intent whose LV name does not match the
    ///   id-derived name fails startup. A present LV is adopted only
    ///   when its reported size is at least the requested size; a
    ///   missing LV drops the intent; a smaller LV fails startup rather
    ///   than adopting a wrong geometry.
    /// - **Grow intents**: a matching volume/LV at or above the
    ///   requested size rolls forward (observed size, generation
    ///   `expected + 1`, intent dropped); a stale generation/size or a
    ///   still-from-size LV drops the intent; an absent LV drops it for
    ///   the volume pass to mark `Failed`; an ambiguous size fails
    ///   startup.
    /// - **Delete intents**: a missing volume or a stale stored
    ///   generation drops the intent **without** deleting anything. A
    ///   live LV at the authorized generation runs the recorded
    ///   discard/`lvremove`; a failure fails startup loudly. The
    ///   volume and intent are then removed together.
    /// - **Volumes**: a state entry whose LV is absent from `lvs` output
    ///   is marked `Failed` and persisted (its data is gone or the VG
    ///   was removed — the volume is never silently recreated). Foreign
    ///   LVs under our volume groups are not touched here; they are
    ///   surfaced by [`reconcile_report`](crate::admin).
    /// - **Device claims**: a claim whose volume group is absent from a
    ///   *successful* `vgs` query is dropped (state must match observed
    ///   reality — e.g. the VG was removed while the daemon was down,
    ///   or the crash landed between `vgremove` success and the state
    ///   save). When `vgs` cannot be queried the claims are kept: an
    ///   honest unknown is never resolved destructively. Volume entries
    ///   are *not* touched here — a volume on a vanished VG is already
    ///   marked `Failed` by the volume pass above.
    fn reconcile(&self) -> Result<(), ApiError> {
        let lvs_rows = self.list_lvs()?;
        let mut present: Vec<String> = Vec::new();
        let mut lv_sizes: BTreeMap<String, u64> = BTreeMap::new();
        for row in &lvs_rows {
            if let Some(key) = row.vg_slash_lv() {
                if let Some(size) = row.size_bytes() {
                    lv_sizes.insert(key.clone(), size);
                }
                present.push(key);
            }
        }
        // None = the query failed (honest unknown): keep every claim.
        let observed_vgs: Option<Vec<String>> = match self.list_vgs() {
            Ok(rows) => Some(rows.into_iter().filter_map(|row| row.vg_name).collect()),
            Err(_) => None,
        };
        let mut state = self.lock_state()?;
        let mut changed = false;

        changed |= Self::apply_create_intents(&mut state, &present, &lv_sizes)?;
        changed |= Self::apply_grow_intents(&mut state, &present, &lv_sizes)?;
        changed |= self.apply_delete_intents(&mut state, &present)?;

        // -- Existing volume pass -------------------------------------
        for volume in state.volumes_mut() {
            let key = format!("{}/{}", volume.entry.vg_name, volume.entry.lv_name);
            if !present.contains(&key) && volume.runtime.state != VolumeLifecycle::Failed {
                volume.runtime.state = VolumeLifecycle::Failed;
                changed = true;
            }
        }
        if let Some(vg_names) = &observed_vgs {
            let stale_claims: Vec<DeviceId> = state
                .devices()
                .iter()
                .filter(|(_, entry)| !vg_names.contains(&entry.vg_name))
                .map(|(id, _)| id.clone())
                .collect();
            for id in stale_claims {
                state.remove_device(&id);
                changed = true;
            }
        }
        if changed {
            self.persist(&mut state)?;
        }
        drop(state);
        // Move records (P6-C): classify every record against the
        // observed world at startup — a live mirror rolls the record
        // to COPYING, a provable relocation completes it, an ended
        // unrelocated move parks IN_DOUBT. Classification and durable
        // rolls only: the constructor never starts a pvmove (the
        // daemon's retry task owns the re-drive of a verifiably
        // unstarted PREPARING record).
        self.reconcile_moves(false)?;
        Ok(())
    }

    /// Startup completion of pending create intents (the kill-9
    /// `lvcreate`→persist window).
    ///
    /// A durable volume record wins over a stale intent. An
    /// intent-owned LV is adopted only when its name matches the
    /// id-derived name and `lvs` reports a size at least the recorded
    /// request; a missing LV drops the intent, and a smaller LV fails
    /// startup rather than adopting a wrong geometry.
    fn apply_create_intents(
        state: &mut LvmState,
        present: &[String],
        lv_sizes: &BTreeMap<String, u64>,
    ) -> Result<bool, ApiError> {
        let mut changed = false;
        let create_intents: Vec<(VolumeId, CreateIntent)> = state
            .create_intents()
            .iter()
            .map(|(id, intent)| (id.clone(), intent.clone()))
            .collect();
        for (volume_id, intent) in create_intents {
            if state.volume(&volume_id).is_some() {
                // The durable volume record wins over a stale intent.
                state.remove_create_intent(&volume_id);
                changed = true;
                continue;
            }
            if intent.lv_name != lv_name_for(&volume_id) {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "pending create intent for volume {volume_id} names LV {:?}, but this \
                         provider derives {:?}; refusing to adopt an intent that does not own \
                         its LV",
                        intent.lv_name,
                        lv_name_for(&volume_id)
                    ),
                ));
            }
            let key = format!("{}/{}", intent.vg_name, intent.lv_name);
            if !present.contains(&key) {
                state.remove_create_intent(&volume_id);
                changed = true;
                continue;
            }
            let Some(&observed) = lv_sizes.get(&key) else {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "pending create intent for volume {volume_id}: lvs lists {key} but \
                         reports no size; refusing to adopt an unverifiable geometry"
                    ),
                ));
            };
            if observed < intent.requested_size_bytes {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "pending create intent for volume {volume_id}: LV {key} is {observed} \
                         bytes, smaller than the requested {} bytes; refusing to adopt a wrong \
                         geometry",
                        intent.requested_size_bytes
                    ),
                ));
            }
            let stored = Self::stored_volume_from_create_intent(&intent, observed);
            state.remove_create_intent(&volume_id);
            state.insert_volume(volume_id, stored);
            changed = true;
        }
        Ok(changed)
    }

    /// Startup roll-forward of pending grow intents.
    ///
    /// A stale generation/from-size or an absent LV drops the intent
    /// (the volume pass marks a vanished LV `Failed`); an LV at or
    /// above the request completes the record and bumps the
    /// generation; an LV still at the from-size drops the intent (the
    /// grow never landed); any other observed size fails startup
    /// rather than guessing.
    fn apply_grow_intents(
        state: &mut LvmState,
        present: &[String],
        lv_sizes: &BTreeMap<String, u64>,
    ) -> Result<bool, ApiError> {
        let mut changed = false;
        let grow_intents: Vec<(VolumeId, GrowIntent)> = state
            .grow_intents()
            .iter()
            .map(|(id, intent)| (id.clone(), intent.clone()))
            .collect();
        for (volume_id, intent) in grow_intents {
            let Some(stored) = state.volume(&volume_id) else {
                // The volume is gone: nothing to roll forward.
                state.remove_grow_intent(&volume_id);
                changed = true;
                continue;
            };
            if stored.entry.generation != intent.expected_generation
                || intent.from_size_bytes != stored.entry.size_bytes
            {
                // A stale authorization: the volume moved on, so this
                // grow was never (or no longer) authorized.
                state.remove_grow_intent(&volume_id);
                changed = true;
                continue;
            }
            let key = format!("{}/{}", stored.entry.vg_name, stored.entry.lv_name);
            if !present.contains(&key) {
                // The LV is absent: drop the intent and let the volume
                // pass mark the volume Failed.
                state.remove_grow_intent(&volume_id);
                changed = true;
                continue;
            }
            let Some(&observed) = lv_sizes.get(&key) else {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "pending grow intent for volume {volume_id}: lvs lists {key} but \
                         reports no size; refusing to guess the geometry"
                    ),
                ));
            };
            if observed >= intent.requested_size_bytes {
                if let Some(stored) = state.volume_mut(&volume_id) {
                    stored.entry.size_bytes = observed;
                    stored.entry.generation = intent.expected_generation + 1;
                }
                state.remove_grow_intent(&volume_id);
                changed = true;
            } else if observed == intent.from_size_bytes {
                // The grow never landed.
                state.remove_grow_intent(&volume_id);
                changed = true;
            } else {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "pending grow intent for volume {volume_id}: LV {key} is {observed} \
                         bytes, between the from-size {} and the requested {} bytes; the \
                         geometry is ambiguous and is never guessed",
                        intent.from_size_bytes, intent.requested_size_bytes
                    ),
                ));
            }
        }
        Ok(changed)
    }

    /// Startup completion of pending delete intents.
    ///
    /// A missing volume or a stale stored generation drops the intent
    /// **without** deleting anything (never delete on a stale
    /// authorization). A live LV at the authorized generation runs the
    /// recorded discard/`lvremove`; a failure fails startup loudly
    /// instead of serving a volume whose authorized delete is pending.
    /// Only the intent's own recorded volume/LV is ever touched.
    fn apply_delete_intents(
        &self,
        state: &mut LvmState,
        present: &[String],
    ) -> Result<bool, ApiError> {
        let mut changed = false;
        let delete_intents: Vec<(VolumeId, DeleteIntent)> = state
            .delete_intents()
            .iter()
            .map(|(id, intent)| (id.clone(), intent.clone()))
            .collect();
        for (volume_id, intent) in delete_intents {
            let Some(stored) = state.volume(&volume_id) else {
                // The volume is gone: the delete already landed.
                state.remove_delete_intent(&volume_id);
                changed = true;
                continue;
            };
            if stored.entry.generation != intent.expected_generation {
                // Never delete on a stale authorization.
                state.remove_delete_intent(&volume_id);
                changed = true;
                continue;
            }
            // The live delete path refuses attached and non-stable
            // volumes before persisting an intent. An intent that
            // somehow coexists with either is anomalous: fail startup
            // rather than remove an LV a consumer may still be using
            // (AGENTS rule 7's never-destroy-unowned direction).
            if stored.runtime.attachment.is_some() {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "pending delete for volume {volume_id} cannot complete: the volume \
                         still records an attachment; refusing to remove an attached LV \
                         during startup"
                    ),
                ));
            }
            match stored.runtime.state {
                VolumeLifecycle::Ready | VolumeLifecycle::Failed => {}
                other => {
                    return Err(ApiError::new(
                        ApiErrorCode::Internal,
                        format!(
                            "pending delete for volume {volume_id} cannot complete: the volume \
                             is {other:?}, and delete is authorized only for Ready/Failed \
                             volumes; refusing to remove it during startup"
                        ),
                    ));
                }
            }
            if let Some(record) = state.move_record(&volume_id) {
                if Self::move_blocks_delete(record) {
                    return Err(ApiError::new(
                        ApiErrorCode::Internal,
                        format!(
                            "pending delete for volume {volume_id} cannot complete: an online \
                             move is in state {:?}; refusing to discard/remove an LV during \
                             startup while its placement is unsettled",
                            record.state
                        ),
                    ));
                }
            }
            let vg_name = stored.entry.vg_name.clone();
            let lv_name = stored.entry.lv_name.clone();
            let key = format!("{vg_name}/{lv_name}");
            if present.contains(&key) {
                self.discard_and_remove_lv(&vg_name, &lv_name, intent.erasure_policy)
                    .map_err(|error| {
                        ApiError::new(
                            ApiErrorCode::Internal,
                            format!(
                                "pending delete for volume {volume_id} could not complete on \
                                 LV {key}: {error}; startup fails rather than serving a volume \
                                 whose authorized delete is pending"
                            ),
                        )
                    })?;
            }
            state.remove_volume(&volume_id);
            state.remove_delete_intent(&volume_id);
            // A completed/failed move record describes only the deleted
            // volume; drop it in the same atomic save so a later create
            // with the same id cannot inherit stale placement history.
            state.remove_move(&volume_id);
            changed = true;
        }
        Ok(changed)
    }

    // -- Volume operations (sync bodies behind the async trait surface) --

    /// Resume a matching pending create intent.
    ///
    /// Returns `Some(response)` when the intent-owned LV was verified
    /// and adopted (or `Err` when that adoption could not be made
    /// durable). Returns `None` when there is no matching intent or
    /// the intent's LV is verifiably absent (the intent is cleared in
    /// memory so a fresh create can proceed). A different payload or
    /// requested size is a typed idempotency conflict; a present LV
    /// smaller than the request is never adopted.
    fn resume_create_intent(
        &self,
        state: &mut LvmState,
        req: &CreateVolumeRequest,
        payload: &str,
    ) -> Result<Option<InspectVolumeResponse>, ApiError> {
        let Some(intent) = state.create_intent(&req.volume_id).cloned() else {
            return Ok(None);
        };
        if intent.creation_payload != payload || intent.requested_size_bytes != req.size_bytes {
            return Err(ApiError::idempotency_conflict(&req.volume_id));
        }
        match self.lv_size(&intent.vg_name, &intent.lv_name)? {
            Some(observed) if observed >= intent.requested_size_bytes => {
                let stored = Self::stored_volume_from_create_intent(&intent, observed);
                state.remove_create_intent(&req.volume_id);
                state.insert_volume(req.volume_id.clone(), stored.clone());
                if let Err(save_error) = self.persist(state) {
                    // The adoption's save failed. If its rename landed
                    // (the post-rename carve-out), the durable record
                    // already holds the volume: keep the in-memory
                    // adoption and say so. Otherwise restore the
                    // intent, fail closed, and leave the verified LV
                    // for a retry/restart to adopt.
                    let committed = LvmState::load(&self.state_path)
                        .is_ok_and(|durable| durable.volume(&req.volume_id).is_some());
                    if committed {
                        return Err(ApiError::new(
                            save_error.code,
                            format!(
                                "{}; the durable state already contains the adopted record for \
                                 volume {}, so the in-memory adoption was kept and the API \
                                 serves it",
                                save_error.detail, req.volume_id
                            ),
                        ));
                    }
                    state.remove_volume(&req.volume_id);
                    state.insert_create_intent(req.volume_id.clone(), intent.clone());
                    return Err(ApiError::new(
                        save_error.code,
                        format!(
                            "{}; the verified create intent for volume {} was not committed, \
                             so the LV {}/{} is left in place and startup recovery will adopt \
                             it (it is at least the requested size)",
                            save_error.detail, req.volume_id, intent.vg_name, intent.lv_name
                        ),
                    ));
                }
                Ok(Some(inspect_response(&req.volume_id, &stored)))
            }
            Some(observed) => {
                // Never adopt a wrong geometry.
                Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "pending create intent for volume {}: LV {}/{} is {observed} bytes, \
                         smaller than the requested {} bytes; refusing to adopt a wrong \
                         geometry",
                        req.volume_id, intent.vg_name, intent.lv_name, intent.requested_size_bytes
                    ),
                ))
            }
            None => {
                // The prior attempt never created the LV: clear the
                // stale intent in memory and create fresh.
                state.remove_create_intent(&req.volume_id);
                Ok(None)
            }
        }
    }

    /// Persist the create intent and run the allocating command.
    ///
    /// Durable-first: the intent is saved before `lvcreate`, so a kill-9
    /// after the CLI created the LV leaves an intent startup recovery
    /// can verify and adopt. A failed intent save creates no LV; a
    /// failed start or size verification leaves/clears the intent
    /// exactly as the create contract documents. Returns the chosen
    /// VG/LV names, the block size and the verified effective size.
    fn create_lv_with_intent(
        &self,
        state: &mut LvmState,
        req: &CreateVolumeRequest,
        payload: &str,
    ) -> Result<(String, String, u32, u64), ApiError> {
        let vg_name = self.pick_pool_vg(state, req.size_bytes)?;
        let lv_name = lv_name_for(&req.volume_id);
        let block_size = req.logical_block_size.unwrap_or(DEFAULT_BLOCK_SIZE);
        let intent = CreateIntent {
            vg_name: vg_name.clone(),
            lv_name: lv_name.clone(),
            requested_size_bytes: req.size_bytes,
            project_id: req.project_id.clone(),
            block_size,
            creation_payload: payload.to_owned(),
        };
        state.insert_create_intent(req.volume_id.clone(), intent);
        if let Err(save_error) = self.persist(state) {
            state.remove_create_intent(&req.volume_id);
            // Classify the failed save: the rename may have landed
            // before the error (the post-rename carve-out), in which
            // case the intent IS durable. No LV was created either
            // way; startup observes the absent intent-owned LV and
            // drops the stale intent.
            let intent_durable = LvmState::load(&self.state_path)
                .is_ok_and(|durable| durable.create_intent(&req.volume_id).is_some());
            let note = if intent_durable {
                format!(
                    "the durable state did record the create intent for volume {}, but no LV \
                     was created; startup recovery observes no intent-owned LV and drops it",
                    req.volume_id
                )
            } else {
                format!(
                    "the create intent for volume {} could not be made durable, so no LV was \
                     created",
                    req.volume_id
                )
            };
            return Err(ApiError::new(
                save_error.code,
                format!("{}; {note}", save_error.detail),
            ));
        }

        let output = match self.runner.run(
            "lvcreate",
            &[
                "--yes",
                "-L",
                &format!("{}B", req.size_bytes),
                "-n",
                &lv_name,
                &vg_name,
            ],
        ) {
            Ok(output) => output,
            Err(error) => {
                return Err(self.clear_create_intent_after_failed_start(
                    state,
                    &req.volume_id,
                    error,
                ));
            }
        };
        if !output.success {
            return Err(self.clear_create_intent_after_failed_start(
                state,
                &req.volume_id,
                command_failed("lvcreate", &output),
            ));
        }
        // Verify against LVM's own report: a successful exit status is
        // not evidence of the requested geometry. Thick LVM rounds the
        // size up to whole physical extents, so anything at or above
        // the request is a success — the effective size is what gets
        // persisted and reported.
        match self.verify_created_size(&vg_name, &lv_name, req.size_bytes) {
            Ok(effective) => Ok((vg_name, lv_name, block_size, effective)),
            Err(error) => {
                // The durable intent stays: startup recovery adopts the
                // LV only if it is present and at least the requested
                // size, so a partially-created LV cannot leak unowned.
                Err(ApiError::new(
                    error.code,
                    format!(
                        "{}; the durable create intent remains, and startup recovery will \
                         adopt the LV only if it is present and at least the requested size, \
                         drop the intent if the LV is absent, and refuse startup if the LV \
                         is present but smaller",
                        error.detail
                    ),
                ))
            }
        }
    }

    /// Create with a durable-first create intent.
    ///
    /// The intent is persisted before `lvcreate`; a retry with a
    /// matching intent adopts an intent-owned LV only when it is
    /// present and at least the requested size, and clears the intent
    /// when the LV is verifiably absent. A failed start clears the
    /// intent (leaving it durable if that clear fails, for startup
    /// recovery). A failed size verification leaves the durable intent
    /// so recovery never adopts a wrong geometry. The final save
    /// commits the volume record and drops the intent together; a
    /// failed final save runs [`Self::compensate_unpersisted_create`],
    /// whose carve-out keeps the volume when the durable record already
    /// landed.
    fn create_volume_inner(
        &self,
        req: &CreateVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError> {
        req.validate()?;
        if !self.supported_classes().contains(&req.volume_class) {
            return Err(unsupported(format!(
                "volume class {:?} is not served by provider {PROVIDER_NAME}",
                req.volume_class
            )));
        }
        check_policies(req)?;

        let payload = canonical_create_payload(req)?;
        let mut state = self.lock_state()?;
        // A durable pending delete owns this volume id: even an
        // idempotent create replay must not report a doomed volume as
        // live. Retry the delete or restart to complete it; a create
        // of a different volume id is unaffected.
        self.ensure_no_pending_delete(&state, &req.volume_id)?;
        // Idempotent replay: same volume_id + same payload returns the
        // current state; a different payload is a typed conflict. The
        // comparison uses the *requested* size (stored in
        // `requested_size_bytes` and embedded in the canonical payload),
        // never the extent-rounded effective size.
        if let Some(existing) = state.volume(&req.volume_id) {
            if existing.entry.creation_payload == payload
                && existing.entry.requested_size_bytes == req.size_bytes
            {
                return Ok(inspect_response(&req.volume_id, existing));
            }
            return Err(ApiError::idempotency_conflict(&req.volume_id));
        }

        // A durable create intent means an earlier attempt reached (or
        // may have reached) `lvcreate` without committing the volume
        // record. This call is the retry: a different payload/requested
        // size is a typed idempotency conflict; a matching intent either
        // adopts the verified LV or, when the LV is verifiably absent,
        // clears the intent and proceeds with a fresh create.
        if let Some(response) = self.resume_create_intent(&mut state, req, &payload)? {
            return Ok(response);
        }

        let (vg_name, lv_name, block_size, effective) =
            self.create_lv_with_intent(&mut state, req, &payload)?;

        let stored = StoredVolume {
            entry: VolumeEntry {
                vg_name: vg_name.clone(),
                lv_name: lv_name.clone(),
                size_bytes: effective,
                requested_size_bytes: req.size_bytes,
                generation: 1,
                data_epoch: 0,
                project_id: req.project_id.clone(),
                block_size,
                creation_payload: payload,
            },
            runtime: VolumeRuntime {
                state: VolumeLifecycle::Ready,
                attachment: None,
                retired_attachment_ids: Vec::new(),
            },
        };
        state.insert_volume(req.volume_id.clone(), stored.clone());
        // The final save commits the volume record and the intent
        // removal together.
        state.remove_create_intent(&req.volume_id);
        if let Err(save_error) = self.persist(&mut state) {
            return Err(self.compensate_unpersisted_create(
                &mut state,
                &req.volume_id,
                &vg_name,
                &lv_name,
                save_error,
            ));
        }
        Ok(inspect_response(&req.volume_id, &stored))
    }

    fn attach_volume_inner(
        &self,
        volume_id: &VolumeId,
        req: &AttachVolumeRequest,
    ) -> Result<AttachVolumeResponse, ApiError> {
        req.validate()?;
        if let Some(frontend) = &req.requested_frontend {
            if frontend != "virtio-blk" {
                return Err(unsupported(format!(
                    "requested frontend {frontend:?}: only virtio-blk exists in this provider"
                )));
            }
        }
        let mode = requested_mode(req.access_mode);
        let mut state = self.lock_state()?;
        // A durable pending delete owns this volume's fate: no new
        // attachment may be granted while it is outstanding.
        self.ensure_no_pending_delete(&state, volume_id)?;

        // Crash-replay idempotency: an attachment id that is already
        // recorded replays the recorded response (even if the volume
        // generation has since moved) and never fabricates a second
        // attachment; the same id with a different payload conflicts.
        let identity = AttachmentIdentity::from_request(volume_id, req, mode);
        for (recorded_volume, stored) in state.volumes() {
            if let Some(record) = stored.runtime.attachment.as_ref() {
                if record.id == req.attachment_id {
                    resolve_attachment_replay(recorded_volume, record, &identity)?;
                    return Ok(attach_response(record, &stored.entry));
                }
            }
        }

        let stored = state
            .volume_mut(volume_id)
            .ok_or_else(|| not_found(volume_id))?;
        ensure_attachment_id_not_retired(
            stored
                .runtime
                .retired_attachment_ids
                .contains(&req.attachment_id),
            &req.attachment_id,
        )?;
        ensure_expected_generation(req.expected_volume_generation, stored.entry.generation)?;
        ensure_no_active_attachment(stored.runtime.attachment.is_some(), volume_id)?;
        match stored.runtime.state {
            VolumeLifecycle::Ready | VolumeLifecycle::Degraded => {}
            other => {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!("attach requires a Ready volume, volume is {other:?}"),
                ));
            }
        }

        let record = AttachmentRecord {
            id: req.attachment_id.clone(),
            vm_id: req.vm_id.clone(),
            host_id: req.host_id.clone(),
            generation: 1,
            access_mode: mode,
            vmm_disk_id: req.vmm_disk_id.clone(),
            state: AttachmentState::Prepared,
            state_revision: volvisor_types::domain::initial_state_revision(),
            evidence: None,
            reporter: None,
            recorded_at_ms: None,
        };
        stored.runtime.attachment = Some(record.clone());
        stored.runtime.state = VolumeLifecycle::Attached;
        stored.entry.generation += 1;
        let response = attach_response(&record, &stored.entry);
        self.persist(&mut state)?;
        Ok(response)
    }

    fn detach_volume_inner(
        &self,
        volume_id: &VolumeId,
        attachment_id: &AttachmentId,
        req: &DetachVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError> {
        req.validate()?;
        let mut state = self.lock_state()?;
        let stored = state
            .volume_mut(volume_id)
            .ok_or_else(|| not_found(volume_id))?;
        let Some(record) = stored.runtime.attachment.as_ref() else {
            return Err(ApiError::not_found(format!(
                "volume {volume_id} has no attachment"
            )));
        };
        if record.id != *attachment_id {
            return Err(ApiError::not_found(format!(
                "attachment {attachment_id} is not attached to volume {volume_id}"
            )));
        }
        ensure_expected_attachment_generation(
            req.expected_attachment_generation,
            record.generation,
        )?;
        // The drain proof (`vm_stopped_or_io_drained_proof`) is a
        // type-mandatory attestation: authority is never released without
        // one, though the provider cannot verify the claim itself
        // (Volume API v2 section 3).

        let retired_id = record.id.clone();
        stored.runtime.attachment = None;
        if !stored.runtime.retired_attachment_ids.contains(&retired_id) {
            stored.runtime.retired_attachment_ids.push(retired_id);
        }
        if stored.runtime.state == VolumeLifecycle::Attached {
            stored.runtime.state = VolumeLifecycle::Ready;
        }
        stored.entry.generation += 1;
        stored.entry.data_epoch += 1;
        let response = inspect_response(volume_id, stored);
        self.persist(&mut state)?;
        Ok(response)
    }

    fn report_attachment_state_inner(
        &self,
        volume_id: &VolumeId,
        req: &ReportAttachmentStateRequest,
    ) -> Result<ReportAttachmentStateResponse, ApiError> {
        req.validate()?;
        if !self
            .capabilities()
            .contains(Capability::AttachmentStateReport)
        {
            return Err(ApiError::not_found(format!(
                "provider {} does not record attachment-state reports; attachment {} on \
                 volume {volume_id} is not a reportable instance",
                self.name(),
                req.attachment_id
            )));
        }
        let mut state = self.lock_state()?;
        let response = {
            let stored = state
                .volume_mut(volume_id)
                .ok_or_else(|| not_found(volume_id))?;
            let Some(record) = stored.runtime.attachment.as_mut() else {
                return Err(ApiError::not_found(format!(
                    "volume {volume_id} has no attachment"
                )));
            };
            if record.id != req.attachment_id {
                return Err(ApiError::not_found(format!(
                    "attachment {} is not attached to volume {volume_id}",
                    req.attachment_id
                )));
            }
            apply_attachment_report(record, req, unix_ms_now())?
        };
        if response.applied {
            self.persist(&mut state)?;
        }
        Ok(response)
    }

    /// Resume a pending grow intent against the observed world.
    ///
    /// The intent must authorize exactly this request (expected
    /// generation/current generation, requested size and from-size);
    /// otherwise a different grow is pending and the request is
    /// refused. An LV at/above the request finalizes the record from
    /// the observed size and bumps the generation; an LV still at the
    /// from-size continues to `lvextend`; any other observed size (or
    /// an absent LV) is ambiguous and refused.
    fn resume_pending_grow(
        &self,
        state: &mut LvmState,
        volume_id: &VolumeId,
        req: &GrowVolumeRequest,
        vg_name: &str,
        lv_name: &str,
        current_size: u64,
        current_generation: u64,
    ) -> Result<GrowResume, ApiError> {
        let Some(intent) = state.grow_intent(volume_id).cloned() else {
            return Ok(GrowResume::NoIntent);
        };
        let matches_request = intent.expected_generation == req.expected_generation
            && intent.expected_generation == current_generation
            && intent.requested_size_bytes == req.new_size_bytes
            && intent.from_size_bytes == current_size;
        if !matches_request {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "volume {volume_id} has a different grow pending (operation {}, \
                     generation {}, from {} to {} bytes); wait for startup recovery to \
                     resolve it or re-issue that exact request",
                    intent.operation_id,
                    intent.expected_generation,
                    intent.from_size_bytes,
                    intent.requested_size_bytes
                ),
            ));
        }
        match self.lv_size(vg_name, lv_name)? {
            Some(observed) if observed >= intent.requested_size_bytes => {
                // The grow landed before the crash/retry: finalize
                // the record without another `lvextend`.
                let new_generation = {
                    let stored = state
                        .volume_mut(volume_id)
                        .ok_or_else(|| not_found(volume_id))?;
                    stored.entry.size_bytes = observed;
                    stored.entry.generation = intent.expected_generation + 1;
                    stored.entry.generation
                };
                state.remove_grow_intent(volume_id);
                if let Err(save_error) = self.persist(state) {
                    return Err(self.persist_completed_grow(
                        save_error,
                        volume_id,
                        vg_name,
                        lv_name,
                        observed,
                        new_generation,
                    ));
                }
                Ok(GrowResume::Finalized(observed))
            }
            Some(observed) if observed == intent.from_size_bytes => Ok(GrowResume::Continue),
            Some(observed) => Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "pending grow intent for volume {volume_id}: LV {vg_name}/{lv_name} is \
                     {observed} bytes, between the from-size {} and the requested {} bytes; \
                     the geometry is ambiguous and is never guessed",
                    intent.from_size_bytes, intent.requested_size_bytes
                ),
            )),
            None => Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "pending grow intent for volume {volume_id} but lvs does not report \
                     {vg_name}/{lv_name}; refusing to guess whether the grow landed"
                ),
            )),
        }
    }

    /// Execute a grow that already passed the request/intent checks.
    ///
    /// Validates capacity (extent-rounded demand plus headroom), then
    /// persists the grow intent when the resume path did not already
    /// have one, runs `lvextend`, verifies the effective size from
    /// `lvs`, and commits the new size/generation with the intent
    /// removal. Start failures clear the intent (or leave it durable
    /// when the clear fails); verification/final-save failures leave
    /// the durable intent for roll-forward. Returns the observed
    /// effective size.
    fn execute_grow(
        &self,
        state: &mut LvmState,
        volume_id: &VolumeId,
        req: &GrowVolumeRequest,
        vg_name: &str,
        lv_name: &str,
        current_size: u64,
        current_generation: u64,
        intent_is_durable: bool,
    ) -> Result<u64, ApiError> {
        let capacity = self.vg_capacity(vg_name)?;
        let delta = req.new_size_bytes - current_size;
        // Thick LVM rounds the grown LV up to whole physical extents,
        // so the space the grow actually consumes is the
        // extent-rounded delta (the current size is already
        // extent-aligned). Comparing the raw delta would under-cover
        // the rounding and surface the over-allocation later as a raw
        // INTERNAL instead of the typed NO_SAFE_CAPACITY.
        let demand = extent_rounded(delta, capacity.extent_bytes);
        if demand.saturating_add(VG_HEADROOM_BYTES) > capacity.free_bytes {
            return Err(ApiError::new(
                ApiErrorCode::NoSafeCapacity,
                format!(
                    "grow needs {demand} more bytes (extent-rounded from {delta}), {} free \
                     in {vg_name}",
                    capacity.free_bytes
                ),
            ));
        }
        if !intent_is_durable {
            // Durable-first: persist the intent before `lvextend`, so a
            // crash between the CLI's success and the volume-record
            // commit rolls the grow forward on restart.
            state.insert_grow_intent(
                volume_id.clone(),
                GrowIntent {
                    operation_id: req.operation_id.clone(),
                    from_size_bytes: current_size,
                    requested_size_bytes: req.new_size_bytes,
                    expected_generation: current_generation,
                },
            );
            if let Err(save_error) = self.persist(state) {
                state.remove_grow_intent(volume_id);
                // The rename may have landed before the error (the
                // post-rename carve-out), so the intent may be
                // durable even though this call did not grow. Startup
                // observes the LV still at its from-size and drops it.
                let intent_durable = LvmState::load(&self.state_path)
                    .is_ok_and(|durable| durable.grow_intent(volume_id).is_some());
                let note = if intent_durable {
                    format!(
                        "the durable state did record the grow intent for volume {volume_id}, \
                         but no grow was attempted; startup recovery observes the LV at its \
                         from-size and drops the intent"
                    )
                } else {
                    format!(
                        "the grow intent for volume {volume_id} could not be made durable, so \
                         no grow was attempted"
                    )
                };
                return Err(ApiError::new(
                    save_error.code,
                    format!("{}; {note}", save_error.detail),
                ));
            }
        }
        let output = match self.runner.run(
            "lvextend",
            &[
                "--yes",
                "-L",
                &format!("{}B", req.new_size_bytes),
                &format!("{vg_name}/{lv_name}"),
            ],
        ) {
            Ok(output) => output,
            Err(error) => {
                return Err(self.clear_grow_intent_after_failed_start(state, volume_id, error));
            }
        };
        if !output.success {
            return Err(self.clear_grow_intent_after_failed_start(
                state,
                volume_id,
                command_failed("lvextend", &output),
            ));
        }
        // Verify the effective size from LVM's own report. Like create,
        // grow rounds up to whole extents: anything at or above the
        // requested size is a success, and the effective size is what
        // gets persisted and reported. A verification failure leaves
        // the durable grow intent for recovery (never a silent
        // success).
        let actual = self.verify_grown_size(vg_name, lv_name, req.new_size_bytes)?;

        let new_generation = {
            let stored = state
                .volume_mut(volume_id)
                .ok_or_else(|| not_found(volume_id))?;
            stored.entry.size_bytes = actual;
            stored.entry.generation += 1;
            stored.entry.generation
        };
        state.remove_grow_intent(volume_id);
        if let Err(save_error) = self.persist(state) {
            return Err(self.persist_completed_grow(
                save_error,
                volume_id,
                vg_name,
                lv_name,
                actual,
                new_generation,
            ));
        }
        Ok(actual)
    }

    /// Verify the post-`lvextend` size from LVM's own report.
    ///
    /// Grow rounds up to whole extents, so anything at or above the
    /// requested size is a success and the effective size is returned.
    /// A missing LV or a smaller observed size is an error and leaves
    /// the durable grow intent for recovery, which never shrinks the
    /// LV.
    fn verify_grown_size(
        &self,
        vg_name: &str,
        lv_name: &str,
        requested_size_bytes: u64,
    ) -> Result<u64, ApiError> {
        match self.lv_size(vg_name, lv_name) {
            Ok(Some(actual)) if actual >= requested_size_bytes => Ok(actual),
            Ok(Some(actual)) => Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "LV size mismatch after lvextend of {vg_name}/{lv_name}: requested \
                     {requested_size_bytes} bytes, LVM reports {actual} bytes; the durable grow \
                     intent remains, and startup recovery completes the grow only if the LV is \
                     at least the requested size (it never shrinks the LV)"
                ),
            )),
            Ok(None) => Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "lvs no longer reports {vg_name}/{lv_name} after lvextend; the durable grow \
                     intent remains for recovery (it never shrinks the LV)"
                ),
            )),
            Err(error) => Err(ApiError::new(
                error.code,
                format!(
                    "{}; the durable grow intent remains, and startup recovery completes the \
                     grow only if the LV is at least the requested size (it never shrinks the \
                     LV)",
                    error.detail
                ),
            )),
        }
    }

    /// Grow with a durable-first grow intent.
    ///
    /// Rejected while a delete is pending. A matching pending intent is
    /// a resume: at/above the request finalizes from the observed size,
    /// exactly at the from-size continues to `lvextend`, and anything
    /// else is ambiguous and refused. A fresh grow persists the intent
    /// before `lvextend`; start/command failures clear it (or leave it
    /// durable when the clear fails). A failed size verification or a
    /// failed final save leaves the durable intent to roll the grow
    /// forward on restart, and the LV is never shrunk as compensation.
    fn grow_volume_inner(
        &self,
        volume_id: &VolumeId,
        req: &GrowVolumeRequest,
    ) -> Result<GrowVolumeResponse, ApiError> {
        validate_api_version(&req.api_version)?;
        if !self.capabilities().contains(Capability::Resize) {
            return Err(unsupported("the resize capability is not advertised"));
        }
        let mut state = self.lock_state()?;
        // A durable pending delete owns this volume's fate: growing it
        // would race a delete authorization (and possibly resurrect a
        // half-discarded LV).
        self.ensure_no_pending_delete(&state, volume_id)?;
        let (vg_name, lv_name, current_size, current_generation, has_attachment) = {
            let stored = state
                .volume(volume_id)
                .ok_or_else(|| not_found(volume_id))?;
            if req.expected_generation != stored.entry.generation {
                return Err(ApiError::stale_generation(
                    req.expected_generation,
                    stored.entry.generation,
                ));
            }
            match stored.runtime.state {
                VolumeLifecycle::Ready | VolumeLifecycle::Attached | VolumeLifecycle::Degraded => {}
                other => {
                    return Err(ApiError::new(
                        ApiErrorCode::InvalidState,
                        format!("grow requires Ready/Attached/Degraded, volume is {other:?}"),
                    ));
                }
            }
            // Grow-only plus alignment, fail-closed (Volume API v2 4A).
            req.validate(stored.entry.size_bytes)?;
            (
                stored.entry.vg_name.clone(),
                stored.entry.lv_name.clone(),
                stored.entry.size_bytes,
                stored.entry.generation,
                stored.runtime.attachment.is_some(),
            )
        };
        let respond = |effective_size_bytes: u64| GrowVolumeResponse {
            backing_resized: true,
            // The provider-layer placeholder, honestly labeled: this
            // layer has no VMM to notify. When the daemon wires the
            // P6-B grow-notification engine (volvisor-provider's
            // `grow` module — the LVM daemon path always does), the
            // API layer composes the real status over this response,
            // inside the journal's execute closure, so the recorded
            // outcome carries it and replays byte-compatibly. This
            // placeholder remains the honest standalone answer (the
            // conformance kit, provider-direct callers): an attached
            // frontend still needs a (retried) notification; a
            // detached volume has nobody to notify. Never `Notified`
            // from here.
            guest_notification_status: if has_attachment {
                GrowGuestNotification::RetryRequired
            } else {
                GrowGuestNotification::NotApplicable
            },
            effective_size_bytes,
        };

        // A durable grow intent means an earlier attempt may already
        // have extended the LV. Roll it forward against the observed
        // world; never extend again when the request differs.
        let mut intent_is_durable = false;
        match self.resume_pending_grow(
            &mut state,
            volume_id,
            req,
            &vg_name,
            &lv_name,
            current_size,
            current_generation,
        )? {
            GrowResume::NoIntent => {}
            GrowResume::Continue => intent_is_durable = true,
            GrowResume::Finalized(observed) => return Ok(respond(observed)),
        }

        let actual = self.execute_grow(
            &mut state,
            volume_id,
            req,
            &vg_name,
            &lv_name,
            current_size,
            current_generation,
            intent_is_durable,
        )?;
        Ok(respond(actual))
    }

    /// Whether a move record means the LV's placement is still
    /// unsettled, so a delete must not touch it.
    ///
    /// A `ZeroDiscard` delete runs `blkdiscard` before `lvremove`; a
    /// live pvmove mirror would propagate that discard through both
    /// legs, and `lvremove` then refuses the moving LV. A completed
    /// (`Complete`) or terminally failed (`Failed`) record describes a
    /// settled placement and may be deleted with the volume.
    fn move_blocks_delete(record: &crate::state::MoveRecord) -> bool {
        matches!(
            record.state,
            MoveVolumeBackingState::Preparing
                | MoveVolumeBackingState::Copying
                | MoveVolumeBackingState::MirrorReady
                | MoveVolumeBackingState::Pivoted
                | MoveVolumeBackingState::InDoubt
        )
    }

    /// Validate that `req` may delete `volume_id` now and return the
    /// current record plus any matching pending delete intent.
    ///
    /// Refuses stale generations, attached volumes, quarantine or
    /// non-stable lifecycle states, an unsettled online move,
    /// cryptographic erasure, and a different pending delete. The
    /// returned pending intent (when present) authorizes this exact
    /// retry, so the caller need not persist it again.
    fn validate_delete_request(
        state: &LvmState,
        volume_id: &VolumeId,
        req: &DeleteVolumeRequest,
    ) -> Result<(StoredVolume, Option<DeleteIntent>), ApiError> {
        let stored_before = state
            .volume(volume_id)
            .cloned()
            .ok_or_else(|| not_found(volume_id))?;
        let volume_state = stored_before.runtime.state;
        if req.expected_generation != stored_before.entry.generation {
            return Err(ApiError::stale_generation(
                req.expected_generation,
                stored_before.entry.generation,
            ));
        }
        if stored_before.runtime.attachment.is_some() {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!("volume {volume_id} must be fully detached before delete"),
            ));
        }
        if let Some(record) = state.move_record(volume_id) {
            if Self::move_blocks_delete(record) {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "volume {volume_id} has an unsettled online move in state {:?}; \
                         complete or reconcile the move before deleting (a ZeroDiscard \
                         delete would discard through a live pvmove mirror)",
                        record.state
                    ),
                ));
            }
        }
        match volume_state {
            VolumeLifecycle::Ready | VolumeLifecycle::Failed => {}
            VolumeLifecycle::Quarantined => {
                return Err(ApiError::new(
                    ApiErrorCode::ForeignDeviceState,
                    format!(
                        "volume {volume_id} is quarantined: foreign backend state is \
                         investigated, never auto-deleted (AGENTS rule 7)"
                    ),
                ));
            }
            other => {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!("delete requires a detached, stable state; volume is {other:?}"),
                ));
            }
        }
        if matches!(req.data_erasure_policy, ErasurePolicy::Cryptographic) {
            return Err(unsupported(
                "cryptographic erasure requires a separate evidence gate",
            ));
        }
        // A durable pending delete is this delete's own retry (which is
        // allowed) only when it authorizes the same generation and
        // erasure policy; a different delete must be resolved first.
        let pending_intent = state.delete_intent(volume_id).cloned();
        if let Some(intent) = &pending_intent {
            if intent.expected_generation != req.expected_generation
                || intent.erasure_policy != req.data_erasure_policy
            {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "volume {volume_id} has a different delete pending (operation {}, \
                         generation {}, policy {:?}); complete that delete before issuing \
                         another",
                        intent.operation_id, intent.expected_generation, intent.erasure_policy
                    ),
                ));
            }
        }
        Ok((stored_before, pending_intent))
    }

    /// Make the delete intent durable before any destructive command.
    ///
    /// A matching pending intent is this delete's retry and is already
    /// durable. Otherwise the new intent is persisted and a failed save
    /// fails closed, leaving the LV untouched.
    fn ensure_delete_intent(
        &self,
        state: &mut LvmState,
        volume_id: &VolumeId,
        req: &DeleteVolumeRequest,
        stored_before: &StoredVolume,
        pending_intent: Option<DeleteIntent>,
        vg_name: &str,
        lv_name: &str,
    ) -> Result<DeleteIntent, ApiError> {
        if let Some(intent) = pending_intent {
            return Ok(intent);
        }
        let intent = DeleteIntent {
            operation_id: req.operation_id.clone(),
            expected_generation: stored_before.entry.generation,
            erasure_policy: req.data_erasure_policy,
        };
        state.insert_delete_intent(volume_id.clone(), intent.clone());
        if let Err(save_error) = self.persist(state) {
            state.remove_delete_intent(volume_id);
            // The rename may have landed before the error (the
            // post-rename carve-out), so the intent may be durable
            // even though this call ran no destructive command. A
            // durable delete authorization is completed by a retry or
            // restart; say so rather than claiming it was not saved. A
            // re-read failure is an honest unknown: keep the intent in
            // memory so the fence cannot be lifted and a later save
            // cannot erase a possibly-durable authorization.
            match LvmState::load(&self.state_path) {
                Ok(durable) if durable.delete_intent(volume_id).is_some() => {
                    // Keep memory matching the durable file: the intent
                    // is the authorization a retry/restart completes,
                    // and the pending-delete fence must keep blocking
                    // new authority until it does.
                    state.insert_delete_intent(volume_id.clone(), intent.clone());
                    Err(ApiError::new(
                        save_error.code,
                        format!(
                            "{}; the durable state did record the delete intent for volume \
                             {volume_id}, so no destructive command ran in this call (the LV \
                             {vg_name}/{lv_name} is untouched), but the pending delete still \
                             fences new attachments, grows and moves: a retry or restart will \
                             complete the authorized delete",
                            save_error.detail
                        ),
                    ))
                }
                Ok(_) => Err(ApiError::new(
                    save_error.code,
                    format!(
                        "{}; the delete intent for volume {volume_id} could not be made \
                         durable, so the LV {vg_name}/{lv_name} was not touched",
                        save_error.detail
                    ),
                )),
                Err(load_error) => {
                    // Treat the intent as possibly durable and keep the
                    // fence up. Never report a clean "not saved" when
                    // the durable file could not be read.
                    state.insert_delete_intent(volume_id.clone(), intent.clone());
                    Err(ApiError::new(
                        save_error.code,
                        format!(
                            "{}; the delete intent for volume {volume_id} could not be \
                             verified after the save failed (re-read failed: {load_error}); \
                             treat it as possibly durable — the pending delete stays fenced \
                             and a retry or restart may complete it",
                            save_error.detail
                        ),
                    ))
                }
            }
        } else {
            Ok(intent)
        }
    }

    /// Delete with a durable-first delete intent.
    ///
    /// A matching pending intent is this delete's retry; a different
    /// generation/policy is refused. An absent LV is a loud `INTERNAL`
    /// for a non-Failed volume with no pending intent (never "deleted"
    /// as if erased). The intent is persisted before `blkdiscard`/
    /// `lvremove`; a failed command clears it when possible (restoring
    /// it in memory and leaving it durable when the clear fails). The
    /// final save removes the volume and intent together; its failed
    /// save keeps the in-memory removal only when the durable file
    /// already records both removals, otherwise it restores the durable
    /// shape and reports that a retry/restart completes the delete.
    /// Foreign LVs are never touched.
    fn delete_volume_inner(
        &self,
        volume_id: &VolumeId,
        req: &DeleteVolumeRequest,
    ) -> Result<(), ApiError> {
        validate_api_version(&req.api_version)?;
        let mut state = self.lock_state()?;
        let (stored_before, pending_intent) =
            Self::validate_delete_request(&state, volume_id, req)?;
        let vg_name = stored_before.entry.vg_name.clone();
        let lv_name = stored_before.entry.lv_name.clone();
        let volume_state = stored_before.runtime.state;

        // A Failed volume whose LV is already absent has nothing to
        // remove: check LVM's own report first so a vanished LV does not
        // pin the pool forever (lvremove on a missing LV always fails).
        // For any *other* state an absent LV is unexpected: a Ready
        // volume whose LV is (perhaps only transiently) invisible must
        // never be deleted as if it had been erased — a ZeroDiscard
        // delete would silently skip erasure and leave a foreign LV
        // holding live data — so it fails loudly instead. A pending
        // delete intent is the exception: it authorizes completing the
        // observed absence (the LV may have been removed before a crash).
        let lv_exists = self.lv_size(&vg_name, &lv_name)?.is_some();
        if !lv_exists && volume_state != VolumeLifecycle::Failed && pending_intent.is_none() {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "LV {vg_name}/{lv_name} unexpectedly absent for volume in state \
                     {volume_state:?}"
                ),
            ));
        }

        // Durable-first: the delete intent is persisted before any
        // destructive command, so a crash between the command's success
        // and the volume-record removal leaves an authorization startup
        // recovery can complete.
        let intent = self.ensure_delete_intent(
            &mut state,
            volume_id,
            req,
            &stored_before,
            pending_intent,
            &vg_name,
            &lv_name,
        )?;

        if lv_exists {
            if let Err(command_error) =
                self.discard_and_remove_lv(&vg_name, &lv_name, intent.erasure_policy)
            {
                // The delete was already authorized durably. A failed
                // step does not prove nothing happened: a `blkdiscard`
                // may have partially erased and `lvremove` may have
                // raced a live move. Keep the durable intent — it
                // fences attach/grow/move and a retry or restart
                // completes the recorded delete — and never report the
                // volume as fully intact.
                return Err(ApiError::new(
                    command_error.code,
                    format!(
                        "{}; the delete of {vg_name}/{lv_name} remains authorized and \
                         pending: the volume stays fenced against new attachment, grow and \
                         move, and a retry or restart will complete the recorded delete \
                         (inspect the LV and the recorded erasure policy before retrying)",
                        command_error.detail
                    ),
                ));
            }
        }
        state.remove_volume(volume_id);
        state.remove_delete_intent(volume_id);
        // A grow/create intent for this volume is stale once the volume
        // is deleted. Clear it in the same atomic save so a later
        // create with the same id cannot inherit an old authorization
        // (the intent is restored with the volume if the save fails
        // before its rename).
        let pending_create = state.remove_create_intent(volume_id);
        let pending_grow = state.remove_grow_intent(volume_id);
        // A completed/failed move record is historical once the volume
        // is gone; any unsettled move was refused above (or fails
        // startup). Remove it atomically with the volume so a same-id
        // recreate cannot inherit a false COMPLETE or re-drive pvmove.
        let pending_move = state.remove_move(volume_id);
        self.persist_delete_completion(
            &mut state,
            volume_id,
            stored_before,
            intent,
            pending_create,
            pending_grow,
            pending_move,
            &vg_name,
            &lv_name,
        )
    }

    /// Persist the atomic volume-record + delete-intent removal and
    /// classify a failed save.
    ///
    /// If the durable file already has neither the volume nor the
    /// intent (the post-rename carve-out), the in-memory removal is
    /// kept and the error says the deletion is durably recorded.
    /// Otherwise the volume, delete intent, any stale create/grow
    /// intent and the move record are reinserted in memory (matching
    /// the durable file) and the error says the durable intent remains
    /// for a retry/restart to complete.
    fn persist_delete_completion(
        &self,
        state: &mut LvmState,
        volume_id: &VolumeId,
        stored_before: StoredVolume,
        intent: DeleteIntent,
        pending_create: Option<CreateIntent>,
        pending_grow: Option<GrowIntent>,
        pending_move: Option<crate::state::MoveRecord>,
        vg_name: &str,
        lv_name: &str,
    ) -> Result<(), ApiError> {
        if let Err(save_error) = self.persist(state) {
            let committed = LvmState::load(&self.state_path).is_ok_and(|durable| {
                durable.volume(volume_id).is_none()
                    && durable.delete_intent(volume_id).is_none()
                    && durable.move_record(volume_id).is_none()
            });
            if committed {
                return Err(ApiError::new(
                    save_error.code,
                    format!(
                        "{}; the durable state already records the deletion of volume \
                         {volume_id} (its volume record, delete intent and move record are all \
                         absent), so the in-memory removal was kept: a retry reports not-found \
                         and a restart is consistent",
                        save_error.detail
                    ),
                ));
            }
            // Match the durable file: it still holds the volume, the
            // delete intent, any create/grow intent that was present
            // before this delete, and the move record.
            state.insert_volume(volume_id.clone(), stored_before);
            state.insert_delete_intent(volume_id.clone(), intent);
            if let Some(intent) = pending_create {
                state.insert_create_intent(volume_id.clone(), intent);
            }
            if let Some(intent) = pending_grow {
                state.insert_grow_intent(volume_id.clone(), intent);
            }
            if let Some(record) = pending_move {
                state.insert_move(volume_id.clone(), record);
            }
            return Err(ApiError::new(
                save_error.code,
                format!(
                    "{}; the durable delete intent for volume {volume_id} remains, and a retry \
                     or restart will complete the delete of {vg_name}/{lv_name}",
                    save_error.detail
                ),
            ));
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Online same-VG extent moves (P6-C, ADR-0006 first slice part 2)
    // ------------------------------------------------------------------
    //
    // The crash model, stated once and binding for everything below:
    // the kernel-side pvmove mirror and the backgrounded pvmove
    // process live OUTSIDE the daemon — they survive daemon death and
    // advance past any journal write. The two survivors a restart
    // reasons from are (1) the durable move record in the state file
    // and (2) the lvs-observable world (the LV's device list: a
    // `pvmove*` segment reference while moving, the real PVs when
    // settled). Recovery is always classification of those two
    // against each other — never a guess, never a destructive
    // resolution of an honest unknown.

    /// Observe one LV's move-relevant truth: whether a pvmove mirror
    /// segment is active and which devices back the LV.
    ///
    /// `INTERNAL` when `lvs` fails or the LV is absent — the caller
    /// decides what an unobservable world means (the drive parks
    /// `IN_DOUBT`; the reconcile pass skips and retries).
    fn observe_lv(&self, vg_name: &str, lv_name: &str) -> Result<MoveObservation, ApiError> {
        let output = self.runner.run("lvs", move_lvs_args())?;
        if !output.success {
            return Err(command_failed("lvs", &output));
        }
        let rows: Vec<LvRow> = crate::report::parse_report(&output.stdout, "lv")?;
        let wanted = format!("{vg_name}/{lv_name}");
        rows.into_iter()
            .find(|row| row.vg_slash_lv().as_deref() == Some(wanted.as_str()))
            .map(|row| MoveObservation {
                moving: row.move_segment_active(),
                devices: row.device_pvs().into_iter().map(str::to_owned).collect(),
            })
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!("LV {wanted} is not reported by lvs"),
                )
            })
    }

    /// Start (or resume) the scoped pvmove. Verified LVM 2.03.x
    /// behavior this builds on: a re-run while the source PV carries
    /// an active move attaches to it (exit 0, remaining arguments
    /// ignored — which is why the provider refuses a second move
    /// from the same source PV instead of silently no-oping), and a
    /// re-run after completion fails with "No data to move" (exit 5 —
    /// which is why the drive always observes completion before ever
    /// invoking this).
    fn start_pvmove(
        &self,
        vg_name: &str,
        lv_name: &str,
        source: &str,
        target: &str,
    ) -> Result<(), ApiError> {
        let output = self.runner.run(
            "pvmove",
            &[
                "--background",
                "--noudevsync",
                "-n",
                &format!("{vg_name}/{lv_name}"),
                source,
                target,
            ],
        )?;
        if output.success {
            return Ok(());
        }
        Err(command_failed("pvmove", &output))
    }

    /// The response for the volume's current recorded move shape.
    fn move_response(
        state: &LvmState,
        volume_id: &VolumeId,
        record: &crate::state::MoveRecord,
    ) -> MoveVolumeBackingResponse {
        let generation = state
            .volume(volume_id)
            .map_or(0, |stored| stored.entry.generation);
        MoveVolumeBackingResponse {
            state: record.state,
            generation,
            source_pv: record.source_pv.clone(),
            target_pv: record.target_pv.clone(),
            detail: record.detail.clone(),
        }
    }

    /// Park the volume's move record `IN_DOUBT` with the honest
    /// reason and answer the response from the parked record. The
    /// source is intact by construction of every caller (an
    /// unverified outcome never freed anything); the record stays
    /// queryable by the reconcile pass, which rolls it forward if
    /// the world later proves completion.
    ///
    /// Parking is the conservative in-memory view: the record is set
    /// `IN_DOUBT` even if the save below fails, and the caller receives
    /// the save error rather than a success response. The **durable
    /// file remains the anchor** in that case — the prior record when
    /// the rename did not land, or the already-landed `IN_DOUBT` when
    /// it did — and the daemon never memory-only-completes or
    /// memory-only-resolves anything; startup reconcile re-parks the
    /// record from the observed world. A persisted `IN_DOUBT` is
    /// likewise rolled forward only by a verified completion.
    ///
    /// **A landed completion is never parked over.** Every caller
    /// parks on a judgment made *before* this lock — a snapshot of
    /// the record, or a world observation that may itself have
    /// failed transiently — so the durable record is re-checked
    /// under the lock, the same stale-snapshot discipline as
    /// [`Self::finish_complete`]'s idempotency gate: a stored
    /// `COMPLETE` record means the completion already landed
    /// durably (this caller's snapshot predates it, or a concurrent
    /// finish path won the race — including the case where this
    /// caller's own verification observe failed transiently), and it
    /// is answered as-is. Parking over it would resurrect the
    /// resolved move to `IN_DOUBT`, and the next reconcile pass
    /// would re-complete it and bump the generation a second time.
    fn park_in_doubt(
        &self,
        volume_id: &VolumeId,
        detail: String,
    ) -> Result<MoveVolumeBackingResponse, ApiError> {
        let mut state = self.lock_state()?;
        if let Some(stored) = state.move_record(volume_id) {
            if stored.state == MoveVolumeBackingState::Complete {
                return Ok(Self::move_response(&state, volume_id, stored));
            }
        }
        // The parked record is the response (the guard's state is
        // exactly what gets persisted — no re-lock after the save).
        let response = {
            if let Some(record) = state.move_record_mut(volume_id) {
                record.state = MoveVolumeBackingState::InDoubt;
                record.detail = Some(detail);
            }
            state
                .move_record(volume_id)
                .map(|record| Self::move_response(&state, volume_id, record))
        };
        self.persist(&mut state)?;
        response.ok_or_else(|| not_found(volume_id))
    }

    /// Verify the relocation with a **fresh** observation and durably
    /// complete the move: one atomic state save carries both the
    /// `COMPLETE` record and the volume's generation bump (the
    /// relocation is a fenced placement mutation; the LV's identity,
    /// path and data are unchanged).
    ///
    /// A verification that cannot prove the source freed parks
    /// `IN_DOUBT` — the source extents are never declared freed on
    /// pvmove's word alone.
    ///
    /// **The bump is exactly once.** Two real callers can verify the
    /// same landed world — two consumer re-attach POSTs supervising
    /// one in-flight record, or the retry reconcile classifying a
    /// snapshot taken before a concurrent drive completed the
    /// record — so the durable record is re-checked under the state
    /// lock: a stored `COMPLETE` record is the already-once
    /// completion and is answered as-is, bumping nothing and saving
    /// nothing. The transient-observation corner carries the same
    /// discipline: a verification observe that fails (or refuses)
    /// parks through [`Self::park_in_doubt`], which never parks
    /// over a stored `COMPLETE` — the stored record is answered, so
    /// the next pass re-completes nothing and bumps nothing.
    ///
    /// A failed final save is classified against a re-read of the
    /// durable file: if it already records the `COMPLETE` record and
    /// the new generation (the post-rename carve-out) the in-memory
    /// completion is kept; otherwise the previous generation and
    /// previous move record are restored in memory and the error says
    /// the durable record still says the prior state, which
    /// reconciliation will complete.
    #[allow(clippy::too_many_lines)] // one verified-completion/persist-classification narrative
    fn finish_complete(
        &self,
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        record: &crate::state::MoveRecord,
    ) -> Result<MoveVolumeBackingResponse, ApiError> {
        let verification = match self.observe_lv(vg_name, lv_name) {
            Ok(verification) => verification,
            // The completion was observed but cannot be verified:
            // unknown, never a silent success.
            Err(e) => {
                return self.park_in_doubt(
                    volume_id,
                    format!("verification could not observe the LV after the move ended: {e}"),
                );
            }
        };
        if !verification.relocated_to(&record.source_pv, &record.target_pv) {
            return self.park_in_doubt(
                volume_id,
                format!(
                    "verification refused: the LV's devices do not verify the relocation \
                     to {} (observed {:?}); the source PV {} must no longer back the LV",
                    record.target_pv, verification.devices, record.source_pv
                ),
            );
        }
        let mut state = self.lock_state()?;
        // The idempotency gate: this caller verified the world, but
        // the completion may have landed durably between that
        // observation and this lock. A stored COMPLETE record for
        // the same source→target is the already-once completion —
        // answer it without a second bump. A stored COMPLETE record
        // for a *different* pair means this caller's record was
        // superseded by a newer move's completion; the stored record
        // is the durable truth, answered as-is — never clobbered,
        // never re-bumped.
        if let Some(stored_record) = state.move_record(volume_id) {
            if stored_record.state == MoveVolumeBackingState::Complete {
                return Ok(Self::move_response(&state, volume_id, stored_record));
            }
        }
        let previous_record = state.move_record(volume_id).cloned();
        let Some(stored) = state.volume_mut(volume_id) else {
            // The volume was deleted while the move ran. The delete
            // path refuses an unsettled move and this completion is
            // only reached with a settled one, so this is a settled
            // tail: drop the record.
            state.remove_move(volume_id);
            if let Err(save_error) = self.persist(&mut state) {
                // Classify the failed clear: if the rename landed the
                // durable file no longer has the settled record, so the
                // in-memory removal matches it. Otherwise restore the
                // prior record for reconciliation.
                let cleared = LvmState::load(&self.state_path)
                    .is_ok_and(|durable| durable.move_record(volume_id).is_none());
                if cleared {
                    return Err(ApiError::new(
                        save_error.code,
                        format!(
                            "{}; the durable state no longer records the settled move for \
                             volume {volume_id} (the clear's rename landed), so the in-memory \
                             removal was kept",
                            save_error.detail
                        ),
                    ));
                }
                if let Some(previous) = previous_record {
                    state.insert_move(volume_id.clone(), previous);
                }
                return Err(ApiError::new(
                    save_error.code,
                    format!(
                        "{}; the settled move record could not be cleared durably and remains \
                         for reconciliation",
                        save_error.detail
                    ),
                ));
            }
            return Err(not_found(volume_id));
        };
        let previous_generation = stored.entry.generation;
        stored.entry.generation += 1;
        let new_generation = stored.entry.generation;
        state.insert_move(
            volume_id.clone(),
            crate::state::MoveRecord {
                operation_id: record.operation_id.clone(),
                source_pv: record.source_pv.clone(),
                target_pv: record.target_pv.clone(),
                state: MoveVolumeBackingState::Complete,
                detail: None,
            },
        );
        let response = state
            .move_record(volume_id)
            .map(|completed| Self::move_response(&state, volume_id, completed));
        if let Err(save_error) = self.persist(&mut state) {
            // Fail-closed classification of a failed final save: the
            // durable file re-read decides whether the completion
            // landed (post-rename) or the prior record is still the
            // anchor.
            let committed = LvmState::load(&self.state_path).is_ok_and(|durable| {
                durable
                    .move_record(volume_id)
                    .is_some_and(|stored| stored.state == MoveVolumeBackingState::Complete)
                    && durable
                        .volume(volume_id)
                        .is_some_and(|stored| stored.entry.generation == new_generation)
            });
            if committed {
                return Err(ApiError::new(
                    save_error.code,
                    format!(
                        "{}; the durable state already records the completed move and volume \
                         generation {new_generation}, so the in-memory completion was kept",
                        save_error.detail
                    ),
                ));
            }
            // The durable file still says the prior state: restore it
            // in memory (never leave a completion against a prior
            // durable record) and let reconciliation complete the
            // move.
            if let Some(stored) = state.volume_mut(volume_id) {
                stored.entry.generation = previous_generation;
            }
            match previous_record {
                Some(previous) => {
                    state.insert_move(volume_id.clone(), previous);
                }
                None => {
                    state.remove_move(volume_id);
                }
            }
            return Err(ApiError::new(
                save_error.code,
                format!(
                    "{}; the durable move record still says the prior state (generation \
                     {previous_generation}), so reconciliation will complete the move",
                    save_error.detail
                ),
            ));
        }
        response.ok_or_else(|| not_found(volume_id))
    }

    /// The MoveVolumeBackingOnline drive (contract section 4A,
    /// `same_vg_extent_move` scope).
    ///
    /// Phases: qualify under the state lock → observe the world
    /// (source derivation, target validation, capacity) → journal
    /// `PREPARING` → start pvmove → journal `COPYING` → supervise
    /// within the window → on completion verify and journal
    /// `COMPLETE` (one save with the generation bump). An unknown
    /// mid-move outcome parks `IN_DOUBT`; the window expiring with
    /// the move progressing answers `COPYING` (a truthful
    /// observation of this call, not a completion claim).
    /// Phase 1 — qualify a move request under the state lock: the
    /// generation fence, the lifecycle admission, and the existing
    /// record's shape (an idempotent re-observation answers; an
    /// active move to the same target re-attaches; a different
    /// target or an `IN_DOUBT` park refuses; a `FAILED` resolution
    /// admits a fresh drive under the new operation; a mirror-path
    /// state is refused typed as foreign to this journal). Nothing
    /// is journaled and nothing in the world is touched here.
    fn admit_move(
        &self,
        volume_id: &VolumeId,
        req: &MoveVolumeBackingRequest,
    ) -> Result<MoveAdmission, ApiError> {
        let state = self.lock_state()?;
        self.ensure_no_pending_delete(&state, volume_id)?;
        let stored = state
            .volume(volume_id)
            .ok_or_else(|| not_found(volume_id))?;
        if stored.entry.generation != req.expected_generation {
            return Err(ApiError::stale_generation(
                req.expected_generation,
                stored.entry.generation,
            ));
        }
        match stored.runtime.state {
            VolumeLifecycle::Ready | VolumeLifecycle::Attached | VolumeLifecycle::Degraded => {}
            other => {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "a same-VG move admits Ready/Attached/Degraded volumes; \
                         volume is {other:?}"
                    ),
                ));
            }
        }
        let names = (stored.entry.vg_name.clone(), stored.entry.lv_name.clone());
        match state.move_record(volume_id) {
            None => Ok(MoveAdmission::Drive {
                vg_name: names.0,
                lv_name: names.1,
                existing_target: None,
            }),
            Some(record) => match record.state {
                MoveVolumeBackingState::Complete => {
                    if record.target_pv == req.target_pool_id {
                        // Idempotent re-observation: this exact move
                        // already completed. The answer is the
                        // durable record's **historical truth** — the
                        // world is not re-observed here, so an
                        // out-of-band post-completion `pvmove` back
                        // to the source is not detected on this path
                        // (fail-closed: a fresh operation with a
                        // different target re-derives the world; the
                        // record, never a guess, is the authority).
                        return Ok(MoveAdmission::Answered(Self::move_response(
                            &state, volume_id, record,
                        )));
                    }
                    // A different target after a completed move is a
                    // fresh move.
                    Ok(MoveAdmission::Drive {
                        vg_name: names.0,
                        lv_name: names.1,
                        existing_target: None,
                    })
                }
                MoveVolumeBackingState::Preparing | MoveVolumeBackingState::Copying => {
                    if record.target_pv != req.target_pool_id {
                        return Err(ApiError::new(
                            ApiErrorCode::InvalidState,
                            format!(
                                "a move to {} is already active for this volume; \
                                 wait for it to complete or re-issue against its target",
                                record.target_pv
                            ),
                        ));
                    }
                    // Re-attach to the active move.
                    Ok(MoveAdmission::Drive {
                        vg_name: names.0,
                        lv_name: names.1,
                        existing_target: Some(record.target_pv.clone()),
                    })
                }
                MoveVolumeBackingState::InDoubt => Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "the volume's previous move (to {}) is IN_DOUBT{}; it parks \
                         until reconciliation proves completion or an operator \
                         resolves it",
                        record.target_pv,
                        record
                            .detail
                            .as_deref()
                            .map_or_else(String::new, |detail| format!(": {detail}"))
                    ),
                )),
                MoveVolumeBackingState::Failed => {
                    // A resolved failure is terminal for **that
                    // move** only: the reconcile proved the extents
                    // verifiably sit back on the record's source (the
                    // relocation did not happen), so a fresh request
                    // re-derives the world and drives a new move;
                    // the drive's PREPARING journal write supersedes
                    // this record durably.
                    Ok(MoveAdmission::Drive {
                        vg_name: names.0,
                        lv_name: names.1,
                        existing_target: None,
                    })
                }
                // `MIRROR_READY`/`PIVOTED` are mirror-path states
                // this same-VG implementation never writes (the
                // kernel-internal mirror is not `lvs`-observable,
                // and a same-VG extent move never pivots). A record
                // holding one is foreign to this provider's journal:
                // refuse typed rather than silently superseding a
                // state the provider cannot even observe.
                MoveVolumeBackingState::MirrorReady | MoveVolumeBackingState::Pivoted => {
                    Err(ApiError::new(
                        ApiErrorCode::Internal,
                        format!(
                            "the volume's move record holds {} — a mirror-path state the \
                             same-VG extent move never writes; the record is foreign to \
                             this provider's journal, refusing rather than superseding it",
                            match record.state {
                                MoveVolumeBackingState::MirrorReady => "MIRROR_READY",
                                _ => "PIVOTED",
                            }
                        ),
                    ))
                }
            },
        }
    }

    /// Phase 2a — qualify the request's named target against the
    /// observed world: it must exist and belong to the volume's own
    /// VG (the capability is same-VG only). The target is observed,
    /// never assumed.
    fn qualify_move_target(
        &self,
        req: &MoveVolumeBackingRequest,
        vg_name: &str,
    ) -> Result<crate::report::PvRow, ApiError> {
        let pvs = self.list_pvs()?;
        let target_row = pvs
            .iter()
            .find(|row| row.pv_name.as_deref() == Some(req.target_pool_id.as_str()))
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::NotFound,
                    format!("target PV {} is not reported by pvs", req.target_pool_id),
                )
            })?
            .clone();
        let target_vg = target_row.vg_name.clone().unwrap_or_default();
        if target_vg != vg_name {
            return Err(ApiError::new(
                ApiErrorCode::MoveUnsupportedScope,
                format!(
                    "cross-VG move refused: target PV {} belongs to VG {target_vg:?}, the \
                     volume's VG is {vg_name:?} (the same_vg_extent_move capability is \
                     same-VG only)",
                    req.target_pool_id
                ),
            ));
        }
        Ok(target_row)
    }

    /// Phase 2b — derive the evacuation source and validate it: a
    /// fresh move requires the LV's extents to sit on exactly one
    /// source PV (the single-source scope), a target distinct from
    /// the source, room for the extents in the target's free space,
    /// and no other volume already evacuating the same source PV
    /// (LVM would attach the second scoped pvmove to the first and
    /// silently ignore its arguments). A re-attach skips all of it:
    /// the record's source is the durable source and the extents
    /// are mid-transfer.
    fn derive_move_source(
        &self,
        volume_id: &VolumeId,
        req: &MoveVolumeBackingRequest,
        vg_name: &str,
        lv_name: &str,
        existing_target: Option<&str>,
        observation: &MoveObservation,
        target_row: &crate::report::PvRow,
    ) -> Result<Option<String>, ApiError> {
        if existing_target.is_some() {
            // Re-attach: the record's source is the durable source
            // (the devices column shows the pvmove segment while
            // moving, not the real PVs).
            return Ok(None);
        }
        let sources = observation.devices.clone();
        if sources.len() != 1 {
            return Err(ApiError::new(
                ApiErrorCode::MoveUnsupportedScope,
                format!(
                    "the same-VG move scope requires the LV's extents to sit on \
                     exactly one source PV; {vg_name}/{lv_name} is spread across \
                     {sources:?}"
                ),
            ));
        }
        let source = sources[0].clone();
        if source == req.target_pool_id {
            return Err(ApiError::invalid_request(format!(
                "target PV {} already holds the volume's extents; the request \
                 describes no move",
                req.target_pool_id
            )));
        }
        // Capacity: the LV's extent-rounded size must fit in the
        // target's free space (LVM allocates whole extents; the
        // stored effective size is already extent-aligned).
        let lv_size = {
            let state = self.lock_state()?;
            state
                .volume(volume_id)
                .map(|stored| stored.entry.size_bytes)
                .ok_or_else(|| not_found(volume_id))?
        };
        let target_free = target_row.free_bytes().ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "target PV {} does not report free space; refusing the move \
                     rather than assuming capacity",
                    req.target_pool_id
                ),
            )
        })?;
        if target_free < lv_size {
            return Err(ApiError::new(
                ApiErrorCode::NoSafeCapacity,
                format!(
                    "target PV {} has {target_free} bytes free; the volume's \
                     extents need {lv_size}",
                    req.target_pool_id
                ),
            ));
        }
        // One pvmove per source PV: LVM attaches a second scoped
        // pvmove to the first and IGNORES its arguments — a second
        // move from the same source would be a silent no-op, so it
        // is refused typed instead.
        let same_source = {
            let state = self.lock_state()?;
            state.moves().iter().any(|(other, record)| {
                other != volume_id
                    && record.source_pv == source
                    && matches!(
                        record.state,
                        MoveVolumeBackingState::Preparing | MoveVolumeBackingState::Copying
                    )
            })
        };
        if same_source {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "another volume is already evacuating source PV {source}; LVM \
                     attaches a second scoped pvmove to the first and ignores its \
                     arguments, so one move per source PV at a time"
                ),
            ));
        }
        Ok(Some(source))
    }

    /// Phase 2c — the fresh-start world scan for a foreign
    /// (unrecorded) pvmove already active on the source PV.
    ///
    /// Verified LVM 2.03.16 behavior: a scoped `pvmove` whose source
    /// PV already carries an active move attaches to it, **ignores
    /// its arguments** and exits 0 — a silent no-op (the probe: a
    /// second scoped move prints `Detected pvmove in progress` and
    /// the LV never moves). The records-only same-source guard in
    /// [`Self::derive_move_source`] sees volvisor's own journaled
    /// moves; an out-of-band operator move is invisible to it, so
    /// the world is scanned directly: with `-a`, `lvs` reports the
    /// hidden `pvmove*` mirror LVs, and their device lists name
    /// exactly the PVs each active move involves.
    fn guard_fresh_move_source(&self, vg_name: &str, source_pv: &str) -> Result<(), ApiError> {
        let output = self.runner.run("lvs", move_scan_lvs_args())?;
        if !output.success {
            return Err(command_failed("lvs", &output));
        }
        let rows: Vec<LvRow> = crate::report::parse_report(&output.stdout, "lv")?;
        let foreign = rows.iter().find(|row| {
            row.vg_name.as_deref() == Some(vg_name)
                && row.lv_name.as_deref().is_some_and(|name| {
                    name.trim_start_matches('[')
                        .trim_end_matches(']')
                        .starts_with("pvmove")
                })
                && row.device_pvs().contains(&source_pv)
        });
        if let Some(row) = foreign {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "a foreign pvmove is already active on source PV {source_pv} (the hidden \
                     mirror segment {} lists it); LVM would attach this move to it and \
                     ignore its arguments — a silent no-op — so the move is refused until \
                     the foreign move ends",
                    row.lv_name.as_deref().unwrap_or("pvmove*")
                ),
            ));
        }
        Ok(())
    }

    /// Phase 3 — journal the move's `PREPARING` anchor.
    ///
    /// A re-attach to an existing PREPARING/COPYING record saves
    /// nothing (an unchanged state has no durable boundary to write;
    /// the operation-id refresh lands with the COPYING save). A new
    /// record must be persisted before `pvmove` starts: if that save
    /// fails the in-memory record is removed and the caller starts
    /// nothing (no durable anchor would mean a blind restart).
    fn journal_move_intent(
        &self,
        volume_id: &VolumeId,
        req: &MoveVolumeBackingRequest,
        fallback_source: Option<&str>,
    ) -> Result<(String, crate::state::MoveRecord), ApiError> {
        let mut state = self.lock_state()?;
        let mut inserted = false;
        let (source, record) = match state.move_record(volume_id) {
            Some(record)
                if matches!(
                    record.state,
                    MoveVolumeBackingState::Preparing | MoveVolumeBackingState::Copying
                ) =>
            {
                (record.source_pv.clone(), record.clone())
            }
            _ => {
                let source = fallback_source.unwrap_or_default().to_owned();
                let record = crate::state::MoveRecord {
                    operation_id: req.operation_id.clone(),
                    source_pv: source.clone(),
                    target_pv: req.target_pool_id.clone(),
                    state: MoveVolumeBackingState::Preparing,
                    detail: None,
                };
                state.insert_move(volume_id.clone(), record.clone());
                inserted = true;
                (source, record)
            }
        };
        if inserted {
            if let Err(save_error) = self.persist(&mut state) {
                // Classify the failed anchor save: if its rename landed,
                // the durable file already holds this PREPARING record.
                // Keep the in-memory anchor so a retry re-attaches to it
                // (and startup reconcile classifies it), but the current
                // call still starts no pvmove. A load error is treated
                // as "not confirmed durable" and the in-memory anchor is
                // dropped: no durable anchor means no pvmove.
                let anchor_durable = LvmState::load(&self.state_path).is_ok_and(|durable| {
                    durable.move_record(volume_id).is_some_and(|stored| {
                        stored.state == MoveVolumeBackingState::Preparing
                            && stored.source_pv == record.source_pv
                            && stored.target_pv == record.target_pv
                    })
                });
                if anchor_durable {
                    return Err(ApiError::new(
                        save_error.code,
                        format!(
                            "the move's PREPARING record reported a save error but reached the \
                             durable state (its rename landed); no pvmove was started. The \
                             durable anchor remains for a retry or startup reconciliation to \
                             classify — do not assume the move failed: {}",
                            save_error.detail
                        ),
                    ));
                }
                state.remove_move(volume_id);
                return Err(ApiError::new(
                    save_error.code,
                    format!(
                        "the move's PREPARING record could not be saved, so no pvmove was \
                         started: {}",
                        save_error.detail
                    ),
                ));
            }
        }
        Ok((source, record))
    }

    /// Drive one online same-VG extent move.
    ///
    /// The `PREPARING` record is the sole reconciliation anchor: its
    /// save must land before `pvmove` starts (a failed save starts
    /// nothing). A failed `COPYING` save restores the prior record in
    /// memory unless the rename landed, in which case the durable
    /// COPYING record is kept; a failed `COMPLETE` save restores the
    /// prior record (or keeps the completion only when the durable file
    /// already has it). In every case startup/reconcile classifies the
    /// durable record against the observed world — there is no blind
    /// restart.
    async fn move_volume_backing_inner(
        &self,
        volume_id: &VolumeId,
        req: &MoveVolumeBackingRequest,
    ) -> Result<MoveVolumeBackingResponse, ApiError> {
        validate_api_version(&req.api_version)?;
        // Honored or refused, never silently ignored: no same-VG
        // pvmove implementation can rate-limit the copy.
        if let Some(rate) = req.max_copy_bytes_per_sec {
            return Err(unsupported(format!(
                "max_copy_bytes_per_sec ({rate}) cannot be honored by a same-VG pvmove \
                 (LVM exposes no copy rate limit); refusing the request rather than \
                 ignoring the parameter"
            )));
        }

        // Phase 1 — qualify under the state lock. Nothing is
        // journaled and nothing in the world is touched until every
        // check passes.
        let (vg_name, lv_name, existing_target) = match self.admit_move(volume_id, req)? {
            MoveAdmission::Answered(response) => return Ok(response),
            MoveAdmission::Drive {
                vg_name,
                lv_name,
                existing_target,
            } => (vg_name, lv_name, existing_target),
        };

        // Phase 2 — observe the world (no lock held): the target
        // qualification first (the request's own named reference is
        // validated against the observed world), then the source
        // derivation.
        let observation = self.observe_lv(&vg_name, &lv_name)?;
        if existing_target.is_none() && observation.moving {
            // A moving LV with no journaled record: someone else's
            // pvmove. Never touched, never adopted.
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "a pvmove mirror segment is active on {vg_name}/{lv_name} outside \
                     volvisor's journal; refusing to supervise a foreign move"
                ),
            ));
        }
        let target_row = self.qualify_move_target(req, &vg_name)?;
        let source_pv = self.derive_move_source(
            volume_id,
            req,
            &vg_name,
            &lv_name,
            existing_target.as_deref(),
            &observation,
            &target_row,
        )?;
        // Phase 2c — the fresh-start world scan: the records-only
        // same-source guard cannot see an out-of-band move, so the
        // world itself must verifiably carry no active pvmove on the
        // source PV before a fresh pvmove starts (a re-attach skips
        // it: attaching to our own journaled move is the point).
        if let Some(source_pv) = source_pv.as_deref() {
            self.guard_fresh_move_source(&vg_name, source_pv)?;
        }

        // Phase 3 — journal the intent. The record is the
        // reconciliation anchor: after this save, a daemon death
        // anywhere below leaves a durable fact to classify against.
        let (source_pv, record_for_drive) =
            self.journal_move_intent(volume_id, req, source_pv.as_deref())?;

        // Phase 4 — start (or resume) the pvmove, then journal
        // COPYING. Between the start and this save the record says
        // PREPARING: a crash there classifies from the world (the
        // mirror observed → COPYING; settled and unrelocated →
        // re-drive).
        if let Err(e) =
            self.start_pvmove(&vg_name, &lv_name, &source_pv, &record_for_drive.target_pv)
        {
            // Honest tail: classify from a fresh observation before
            // answering.
            if let Some(answer) = self.classify_failed_start(
                volume_id,
                &vg_name,
                &lv_name,
                &source_pv,
                &record_for_drive,
                e,
            )? {
                return Ok(answer);
            }
        }
        {
            let mut state = self.lock_state()?;
            if let Some(record) = state.move_record_mut(volume_id) {
                record.state = MoveVolumeBackingState::Copying;
                record.operation_id = req.operation_id.clone();
                record.detail = None;
            }
            if let Err(save_error) = self.persist(&mut state) {
                // The pvmove is already running. The prior durable
                // record (PREPARING, or the re-attached COPYING) is the
                // anchor — except when this save's rename landed, in
                // which case the durable file already says COPYING and
                // memory must keep that same record (never roll back
                // against a durable file that has moved on).
                let copying_durable = LvmState::load(&self.state_path).is_ok_and(|durable| {
                    durable
                        .move_record(volume_id)
                        .is_some_and(|record| record.state == MoveVolumeBackingState::Copying)
                });
                if copying_durable {
                    return Err(ApiError::new(
                        save_error.code,
                        format!(
                            "the pvmove was started and its COPYING record reported a save \
                             error but reached the durable state (its rename landed); the \
                             durable COPYING anchor remains, so a retry or restart classifies \
                             the world — do not assume the move failed: {}",
                            save_error.detail
                        ),
                    ));
                }
                state.insert_move(volume_id.clone(), record_for_drive.clone());
                return Err(ApiError::new(
                    save_error.code,
                    format!(
                        "the pvmove was started but its COPYING record could not be saved: \
                         {}; the durable prior record is the anchor and startup/reconcile \
                         classifies the world — do not assume the move failed",
                        save_error.detail
                    ),
                ));
            }
        }

        // Phase 5 — supervise within the window: poll the world,
        // complete on a verified relocation, park `IN_DOUBT` on
        // anything unknown.
        self.supervise_move(volume_id, &vg_name, &lv_name, &source_pv, &record_for_drive)
            .await
    }

    /// Phase 4's honest tail: a failed `pvmove` start classified
    /// from a fresh observation before answering —
    ///
    /// - a live mirror: the move is running despite the error —
    ///   `Ok(None)` (fall through to the `COPYING` save and the
    ///   supervision);
    /// - a verified relocation: the move already finished (the
    ///   re-attach race — there was nothing left to move) —
    ///   complete it;
    /// - the source freed without the target holding the extents:
    ///   an unknown placement — park `IN_DOUBT`;
    /// - a verifiably untouched world: drop a `PREPARING` record
    ///   and fail typed.
    fn classify_failed_start(
        &self,
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        source_pv: &str,
        record: &crate::state::MoveRecord,
        error: ApiError,
    ) -> Result<Option<MoveVolumeBackingResponse>, ApiError> {
        let observation = self.observe_lv(vg_name, lv_name)?;
        if observation.moving {
            return Ok(None);
        }
        if observation.relocated_to(source_pv, &record.target_pv) {
            return self
                .finish_complete(volume_id, vg_name, lv_name, record)
                .map(Some);
        }
        if observation.source_freed(source_pv) {
            return self
                .park_in_doubt(
                    volume_id,
                    format!(
                        "the pvmove start refused while the LV's extents sit on \
                         neither the source PV {source_pv} nor the target PV {} \
                         (observed {:?}); an unknown outcome, never a silent failure",
                        record.target_pv, observation.devices
                    ),
                )
                .map(Some);
        }
        let mut state = self.lock_state()?;
        let preparing = state
            .move_record(volume_id)
            .filter(|record| record.state == MoveVolumeBackingState::Preparing)
            .cloned();
        if let Some(preparing) = preparing {
            state.remove_move(volume_id);
            if let Err(clear_error) = self.persist(&mut state) {
                // The durable record still says PREPARING: restore the
                // in-memory view and leave it for reconciliation.
                state.insert_move(volume_id.clone(), preparing);
                return Err(ApiError::new(
                    error.code,
                    format!(
                        "{}; the PREPARING record could not be cleared durably \
                         ({clear_error}) and remains for reconciliation",
                        error.detail
                    ),
                ));
            }
        }
        Err(error)
    }

    /// Phase 5 — supervise the started move within the request's
    /// supervision window. The state lock is never held across
    /// awaits; the world is re-observed every poll:
    ///
    /// - still moving past the window → an honest `COPYING` answer
    ///   (the record stays `COPYING`; the retry reconcile completes
    ///   it, a fresh operation re-attaches);
    /// - the observation fails → `IN_DOUBT` (an unobservable world,
    ///   never a guess);
    /// - the source is freed → [`Self::finish_complete`] (a fresh
    ///   verification observation before anything is freed);
    /// - the move ended without relocating → `IN_DOUBT` with the
    ///   source intact and serving.
    async fn supervise_move(
        &self,
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        source_pv: &str,
        record: &crate::state::MoveRecord,
    ) -> Result<MoveVolumeBackingResponse, ApiError> {
        let deadline = Instant::now() + self.move_timing.supervision_window;
        loop {
            let observation = match self.observe_lv(vg_name, lv_name) {
                Ok(observation) => observation,
                Err(e) => {
                    return self.park_in_doubt(
                        volume_id,
                        format!("the lvs observation failed while supervising the move: {e}"),
                    );
                }
            };
            if observation.moving {
                if Instant::now() >= deadline {
                    // The window expired with the move progressing:
                    // answer the honest state. The record stays
                    // COPYING; the daemon's retry reconcile completes
                    // it, and a fresh operation re-attaches.
                    let mut state = self.lock_state()?;
                    let response = {
                        if let Some(record) = state.move_record_mut(volume_id) {
                            record.detail = Some(
                                "the supervision window expired with the move in progress; \
                                 re-issue with a fresh operation_id to re-attach"
                                    .to_owned(),
                            );
                        }
                        state
                            .move_record(volume_id)
                            .map(|record| Self::move_response(&state, volume_id, record))
                    };
                    self.persist(&mut state)?;
                    return response.ok_or_else(|| not_found(volume_id));
                }
                tokio::time::sleep(self.move_timing.poll_interval).await;
                continue;
            }
            if observation.relocated_to(source_pv, &record.target_pv) {
                return self.finish_complete(volume_id, vg_name, lv_name, record);
            }
            // The move ended without relocating the extents —
            // aborted out-of-band or failed. Unknown outcome: park,
            // source intact.
            return self.park_in_doubt(
                volume_id,
                format!(
                    "the pvmove ended without relocating the extents to {} (aborted \
                     out-of-band or failed); the source PV {} is intact and serving",
                    record.target_pv, source_pv
                ),
            );
        }
    }

    /// The move-records reconciliation: classify every record
    /// against the observed world and roll the durable state. This
    /// is the restart-and-retry heart of the crash model — the
    /// constructor runs it with `redrive: false` (classification and
    /// durable rolls only; the constructor never starts world
    /// mutations), and the daemon's retry task runs it with
    /// `redrive: true` (a PREPARING record whose pvmove verifiably
    /// never started is re-driven — the consumer's journaled intent
    /// resolved by the world, the migration-drive discipline).
    ///
    /// Never destructive: an unobservable record is skipped and
    /// retried (parking `IN_DOUBT` at the consecutive-failure bound
    /// so it surfaces), an unrelocated `COPYING` record parks
    /// `IN_DOUBT`, a parked `IN_DOUBT` record whose world verifiably
    /// shows the pre-move placement resolves `FAILED` (the known
    /// rollback outcome; the mixed-placement park stays parked), and
    /// nothing but a verified relocation ever completes.
    fn reconcile_moves(&self, redrive: bool) -> Result<MoveReconcileReport, ApiError> {
        let mut report = MoveReconcileReport::default();
        let snapshot: Vec<(VolumeId, crate::state::MoveRecord)> = {
            let state = self.lock_state()?;
            state
                .moves()
                .iter()
                .map(|(id, record)| (id.clone(), record.clone()))
                .collect()
        };
        for (volume_id, record) in snapshot {
            let names = {
                let state = self.lock_state()?;
                state
                    .volume(&volume_id)
                    .map(|stored| (stored.entry.vg_name.clone(), stored.entry.lv_name.clone()))
            };
            let Some((vg_name, lv_name)) = names else {
                // The volume is gone (deleted under the move):
                // nothing to move, drop the record.
                let mut state = self.lock_state()?;
                state.remove_move(&volume_id);
                self.persist(&mut state)?;
                self.clear_move_observe_failures(&volume_id);
                report.dropped.push(volume_id);
                continue;
            };
            if record.state == MoveVolumeBackingState::Complete {
                // A completed record is a historical fact: the world
                // moving on afterwards is a new move for a new
                // consumer request, never a re-completion (and never
                // a second generation bump).
                self.clear_move_observe_failures(&volume_id);
                continue;
            }
            let observation = match self.observe_lv(&vg_name, &lv_name) {
                Ok(observation) => {
                    self.clear_move_observe_failures(&volume_id);
                    observation
                }
                Err(error) => {
                    // Honest unknown — but never an unbounded one:
                    // the unobservable-world tail owns the bound.
                    self.reconcile_unobservable_world(
                        &volume_id,
                        &vg_name,
                        &lv_name,
                        &error,
                        &mut report,
                    )?;
                    continue;
                }
            };
            if observation.moving {
                // The move is alive (whether this daemon started it
                // or a previous incarnation did): the record must say
                // so durably.
                if record.state != MoveVolumeBackingState::Copying {
                    let mut state = self.lock_state()?;
                    if let Some(record) = state.move_record_mut(&volume_id) {
                        record.state = MoveVolumeBackingState::Copying;
                        record.detail = None;
                    }
                    self.persist(&mut state)?;
                }
                report.marked_copying.push(volume_id);
                continue;
            }
            if observation.relocated_to(&record.source_pv, &record.target_pv) {
                // Completion is provable from the world — including
                // for an IN_DOUBT record ("rolls forward under
                // reconciled authority") — and verified against the
                // record's own target, never a foreign relocation.
                self.finish_complete(&volume_id, &vg_name, &lv_name, &record)?;
                report.completed.push(volume_id);
                continue;
            }
            match record.state {
                MoveVolumeBackingState::Preparing if redrive => {
                    // The pvmove never verifiably started: re-drive
                    // the journaled intent. A start failure leaves
                    // the record PREPARING for the next tick.
                    if self
                        .start_pvmove(&vg_name, &lv_name, &record.source_pv, &record.target_pv)
                        .is_ok()
                    {
                        let mut state = self.lock_state()?;
                        if let Some(record) = state.move_record_mut(&volume_id) {
                            record.state = MoveVolumeBackingState::Copying;
                            record.detail = None;
                        }
                        self.persist(&mut state)?;
                        report.redriven.push(volume_id);
                    }
                }
                MoveVolumeBackingState::Copying => {
                    // The move ended without relocating: park (unless
                    // a landed completion won the race — the park
                    // helper answers the stored truth, and the report
                    // says park only if the record actually parked).
                    let parked = self.park_in_doubt(
                        &volume_id,
                        format!(
                            "the pvmove ended without relocating the extents to {} \
                             (aborted out-of-band or failed); the source PV {} is intact \
                             and serving",
                            record.target_pv, record.source_pv
                        ),
                    )?;
                    if parked.state == MoveVolumeBackingState::InDoubt {
                        report.parked_in_doubt.push(volume_id);
                    }
                }
                MoveVolumeBackingState::InDoubt
                    if observation.restored_to_source(&record.source_pv) =>
                {
                    // The rollback direction's exit (the helper owns
                    // the live-record re-check and the journal).
                    self.resolve_in_doubt_rollback(&volume_id, &record, &mut report)?;
                }
                _ => {}
            }
        }
        Ok(report)
    }

    /// The unobservable-world tail of one reconcile record: below
    /// [`MOVE_OBSERVE_FAILURE_BOUND`] consecutive failures the
    /// record is left untouched and the pass reports the honest
    /// unknown; at the bound the record parks `IN_DOUBT` with a
    /// typed reason (e.g. an out-of-band `lvremove` of the moving LV
    /// would otherwise spin forever with no surfaced state); a
    /// record already parked surfaces as parked without being
    /// re-written.
    fn reconcile_unobservable_world(
        &self,
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        error: &ApiError,
        report: &mut MoveReconcileReport,
    ) -> Result<(), ApiError> {
        let failures = self.bump_move_observe_failures(volume_id);
        let already_parked = {
            let state = self.lock_state()?;
            state
                .move_record(volume_id)
                .is_some_and(|record| record.state == MoveVolumeBackingState::InDoubt)
        };
        let mut completed_elsewhere = false;
        if !already_parked && failures >= MOVE_OBSERVE_FAILURE_BOUND {
            let parked = self.park_in_doubt(
                volume_id,
                format!(
                    "the LV {vg_name}/{lv_name} has been unobservable for {failures} \
                     consecutive reconcile passes ({error}); the record parks instead of \
                     silently spinning — an operator must resolve the placement"
                ),
            )?;
            // A landed completion won the race while this pass could
            // not observe the world: the park helper answered the
            // stored truth — the record did not park, so it is not
            // reported parked (the concurrent finisher owns the
            // completion's report).
            completed_elsewhere = parked.state != MoveVolumeBackingState::InDoubt;
        }
        if !completed_elsewhere {
            if already_parked || failures >= MOVE_OBSERVE_FAILURE_BOUND {
                report.parked_unobservable.push(volume_id.clone());
            } else {
                report.unobservable.push(volume_id.clone());
            }
        }
        Ok(())
    }

    /// The rollback direction's exit: a parked record whose world
    /// verifiably shows the pre-move placement (exactly the record's
    /// source PV, no move active) resolves `FAILED` — the known
    /// outcome: the relocation verifiably did not happen (an
    /// out-of-band `pvmove --abort` that completed the rollback, or
    /// a move that never started), the source is intact and serving,
    /// and a fresh operation may drive a new move. The
    /// mixed-placement world still parks (reached by no arm): it is
    /// genuinely unresolvable by observation alone.
    ///
    /// The live record is re-checked under the lock — the same
    /// stale-snapshot discipline as `finish_complete`: a record that
    /// rolled forward to `COMPLETE` (or was superseded) behind this
    /// pass's snapshot is the durable truth and is never overwritten
    /// with a resolution.
    fn resolve_in_doubt_rollback(
        &self,
        volume_id: &VolumeId,
        record: &crate::state::MoveRecord,
        report: &mut MoveReconcileReport,
    ) -> Result<(), ApiError> {
        let mut state = self.lock_state()?;
        let resolves = state.move_record(volume_id).is_some_and(|stored| {
            stored.state == MoveVolumeBackingState::InDoubt
                && stored.source_pv == record.source_pv
                && stored.target_pv == record.target_pv
        });
        if resolves {
            let previous = state.move_record(volume_id).cloned();
            if let Some(stored) = state.move_record_mut(volume_id) {
                stored.state = MoveVolumeBackingState::Failed;
                stored.detail = Some(format!(
                    "resolved FAILED by reconciliation: the move to {} verifiably did \
                     not happen — the extents sit on the source PV {} with no move \
                     active (an out-of-band pvmove --abort completed the rollback, or \
                     the move never started); the source is intact and serving",
                    record.target_pv, record.source_pv
                ));
            }
            if let Err(save_error) = self.persist(&mut state) {
                // Classify the failed resolution save: if its rename
                // landed, the durable file already records FAILED and
                // the in-memory resolution matches it. Otherwise never
                // leave FAILED in memory against a durable IN_DOUBT:
                // restore the parked record (with its previous detail).
                let committed = LvmState::load(&self.state_path).is_ok_and(|durable| {
                    durable.move_record(volume_id).is_some_and(|stored| {
                        stored.state == MoveVolumeBackingState::Failed
                            && stored.source_pv == record.source_pv
                            && stored.target_pv == record.target_pv
                    })
                });
                if committed {
                    return Err(ApiError::new(
                        save_error.code,
                        format!(
                            "{}; the durable state already records the verified rollback of \
                             volume {volume_id} as FAILED, so the in-memory resolution was kept",
                            save_error.detail
                        ),
                    ));
                }
                if let Some(previous) = previous {
                    state.insert_move(volume_id.clone(), previous);
                }
                return Err(ApiError::new(
                    save_error.code,
                    format!(
                        "{}; the verified rollback of volume {volume_id} could not be recorded \
                         durably, so the durable IN_DOUBT record remains the anchor",
                        save_error.detail
                    ),
                ));
            }
            report.resolved_failed.push(volume_id.clone());
        }
        Ok(())
    }

    /// One retry reconcile pass (the daemon's background task and
    /// the fault rows' recovery entry point): classify, roll, and
    /// re-drive verifiably-unstarted moves.
    pub fn move_reconcile_pass(&self) -> Result<MoveReconcileReport, ApiError> {
        self.reconcile_moves(true)
    }
}

#[async_trait]
impl VolumeProvider for LvmProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self) -> CapabilitySet {
        // P6-C (ADR-0006 first slice part 2): the same-VG extent
        // move is qualified — `same_host_live_backing_move` (the QSD
        // mirror/pivot path) remains advertised NOWHERE until its
        // acceptance suite passes (the ADR's own gate).
        let mut capabilities = CapabilitySet::native_local_p0();
        capabilities.insert(Capability::SameVgExtentMove);
        capabilities
    }

    fn supported_classes(&self) -> &[VolumeClass] {
        SUPPORTED_CLASSES
    }

    async fn create_volume(
        &self,
        req: &CreateVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError> {
        self.create_volume_inner(req)
    }

    async fn inspect_volume(&self, id: &VolumeId) -> Result<InspectVolumeResponse, ApiError> {
        let state = self.lock_state()?;
        state
            .volume(id)
            .map(|stored| inspect_response(id, stored))
            .ok_or_else(|| not_found(id))
    }

    async fn list_volumes(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<InspectVolumeResponse>, ApiError> {
        let state = self.lock_state()?;
        Ok(state
            .volumes()
            .iter()
            .filter(|(_, stored)| project.is_none_or(|p| &stored.entry.project_id == p))
            .map(|(id, stored)| inspect_response(id, stored))
            .collect())
    }

    async fn attach_volume(
        &self,
        volume_id: &VolumeId,
        req: &AttachVolumeRequest,
    ) -> Result<AttachVolumeResponse, ApiError> {
        self.attach_volume_inner(volume_id, req)
    }

    async fn detach_volume(
        &self,
        volume_id: &VolumeId,
        attachment_id: &AttachmentId,
        req: &DetachVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError> {
        self.detach_volume_inner(volume_id, attachment_id, req)
    }

    async fn report_attachment_state(
        &self,
        volume_id: &VolumeId,
        req: &ReportAttachmentStateRequest,
    ) -> Result<ReportAttachmentStateResponse, ApiError> {
        self.report_attachment_state_inner(volume_id, req)
    }

    async fn grow_volume(
        &self,
        volume_id: &VolumeId,
        req: &GrowVolumeRequest,
    ) -> Result<GrowVolumeResponse, ApiError> {
        self.grow_volume_inner(volume_id, req)
    }

    async fn move_volume_backing(
        &self,
        volume_id: &VolumeId,
        req: &MoveVolumeBackingRequest,
    ) -> Result<MoveVolumeBackingResponse, ApiError> {
        // Unlike every other op (single CLI invocations), the drive
        // supervises an in-flight pvmove for up to the window — the
        // poll loop sleeps asynchronously so no executor worker is
        // pinned; the CLI calls themselves stay inline like every
        // other op.
        self.move_volume_backing_inner(volume_id, req).await
    }

    async fn delete_volume(
        &self,
        volume_id: &VolumeId,
        req: &DeleteVolumeRequest,
    ) -> Result<(), ApiError> {
        self.delete_volume_inner(volume_id, req)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The shared `--reportformat json --units b --nosuffix` LVM arguments.
fn lvm_json_args() -> &'static [&'static str] {
    &["--reportformat", "json", "--units", "b", "--nosuffix"]
}

/// The `vgs`-specific report arguments: the shared JSON arguments plus
/// an explicit column list (so `vg_extent_size` is reported — it is not
/// part of `vgs`' default columns, and capacity checks must round the
/// demand to whole extents).
fn vgs_json_args() -> &'static [&'static str] {
    &[
        "--reportformat",
        "json",
        "--units",
        "b",
        "--nosuffix",
        "-o",
        "vg_name,vg_free,vg_size,vg_extent_size",
    ]
}

/// The move observation's `lvs` arguments: the shared JSON arguments
/// plus the explicit move-relevant columns — `devices` is not among
/// `lvs`' default columns, and it is the honest moving/relocated
/// observation (a `pvmove*` segment reference while moving, the real
/// PVs when settled). Shaped and verified against LVM 2.03.16's JSON
/// report.
fn move_lvs_args() -> &'static [&'static str] {
    &[
        "--reportformat",
        "json",
        "--units",
        "b",
        "--nosuffix",
        "-o",
        "vg_name,lv_name,lv_attr,copy_percent,devices",
    ]
}

/// The foreign-move scan's `lvs` arguments: the shared JSON
/// arguments plus `-a` and the columns the scan reads. `-a` is the
/// load-bearing flag: the temporary `pvmove*` mirror LVs are hidden
/// from the default report, and their device lists are the only
/// `lvs`-observable naming of **which PVs** an active move involves
/// (shaped and verified against LVM 2.03.16's JSON report — the
/// hidden row prints as `[pvmove0]` with `devices` naming the source
/// and target PVs).
fn move_scan_lvs_args() -> &'static [&'static str] {
    &[
        "-a",
        "--reportformat",
        "json",
        "--units",
        "b",
        "--nosuffix",
        "-o",
        "vg_name,lv_name,devices",
    ]
}

/// An `INTERNAL` error carrying the command name and a stderr excerpt.
pub(crate) fn command_failed(program: &str, output: &CommandOutput) -> ApiError {
    ApiError::new(
        ApiErrorCode::Internal,
        format!("{program} failed: {}", output.stderr_excerpt()),
    )
}

/// An `UNSUPPORTED_CLASS_OR_POLICY` rejection (fail-closed negotiation).
fn unsupported(detail: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorCode::UnsupportedClassOrPolicy, detail)
}

/// A `NOT_FOUND` rejection for a missing volume.
fn not_found(volume_id: &VolumeId) -> ApiError {
    ApiError::not_found(format!("volume {volume_id} not found"))
}

/// The maximum length of the sanitized identity segment of an LV name.
///
/// LVM rejects LV names beyond ~127 characters; capping the sanitized
/// segment at 100 keeps the full name (`vol-` + segment + `-` + 8 hex)
/// at 113 characters even for a maximum-length (128-byte) volume id.
const LV_NAME_SANITIZED_MAX_CHARS: usize = 100;

/// The LV name for a volume identity.
///
/// `.` and `:` are not safe in all LVM tooling contexts, so they are
/// replaced with `-` (the ID charset otherwise consists of
/// `[A-Za-z0-9_.:-]`). Sanitization alone is **not injective** (`vol.a`,
/// `vol:a` and `vol-a` all map to `vol-a`), and with the length cap
/// below it is not even collision-free for distinct long ids; the name
/// is therefore injective only up to its 32-bit hash suffix — the first
/// 8 hex characters of SHA-256 over the **full** volume id (collision
/// probability <= 2^-32 per distinct pair). The `vol-` prefix
/// guarantees the name is never dash-leading, and the sanitized segment
/// is truncated to `LV_NAME_SANITIZED_MAX_CHARS` characters so a
/// 128-byte volume id still yields a name LVM accepts (at most
/// `4 + 100 + 1 + 8 = 113` characters).
#[must_use]
pub fn lv_name_for(volume_id: &VolumeId) -> String {
    let sanitized: String = volume_id
        .as_str()
        .chars()
        .map(|c| if matches!(c, '.' | ':') { '-' } else { c })
        .take(LV_NAME_SANITIZED_MAX_CHARS)
        .collect();
    let digest = Sha256::digest(volume_id.as_str().as_bytes());
    format!(
        "vol-{sanitized}-{}",
        crate::discover::hex_prefix(&digest, 4)
    )
}

/// The access mode granted for a requested mode.
fn requested_mode(mode: volvisor_types::request::AccessModeRequest) -> AccessMode {
    match mode {
        volvisor_types::request::AccessModeRequest::SingleWriter => AccessMode::SingleWriter,
        volvisor_types::request::AccessModeRequest::ReadOnly => AccessMode::ReadOnly,
    }
}

/// Canonical creation payload with the idempotency key normalized out.
fn canonical_create_payload(req: &CreateVolumeRequest) -> Result<String, ApiError> {
    let mut value = serde_json::to_value(req).map_err(|e| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("creation payload canonicalization failed: {e}"),
        )
    })?;
    if let serde_json::Value::Object(map) = &mut value {
        map.remove("operation_id");
    }
    Ok(value.to_string())
}

/// Fail-closed policy negotiation for create (mirrors the fake provider).
fn check_policies(req: &CreateVolumeRequest) -> Result<(), ApiError> {
    if req.provisioning == Some(Provisioning::Thin) {
        return Err(unsupported(
            "thin provisioning: this provider is thick-only",
        ));
    }
    if req.encryption.is_some() {
        return Err(unsupported("encryption: this provider implements none"));
    }
    if req.migration_policy.is_some() {
        return Err(unsupported(
            "migration policy: this provider implements no migration semantics",
        ));
    }
    if let Some(local) = &req.local_protection {
        match local.mode {
            LocalProtectionModeRequest::None => {
                if local.min_healthy_legs.is_some() {
                    return Err(unsupported(
                        "min_healthy_legs is meaningless without a local mirror",
                    ));
                }
            }
            LocalProtectionModeRequest::Mirror | LocalProtectionModeRequest::ProviderSpecific => {
                return Err(unsupported(
                    "local mirror protection is not implemented by this provider",
                ));
            }
        }
    }
    if let Some(placement) = &req.placement {
        if let Some(preferred) = placement.preferred_host_id.as_ref() {
            if preferred.as_str() != LOCAL_HOST_ID {
                return Err(ApiError::new(
                    ApiErrorCode::InsufficientFailureDomains,
                    format!(
                        "cannot honor preferred_host_id {preferred}: this provider serves \
                         the single host {LOCAL_HOST_ID:?}"
                    ),
                ));
            }
        }
        if placement.failure_domain == Some(FailureDomain::Rack) {
            return Err(ApiError::new(
                ApiErrorCode::InsufficientFailureDomains,
                "rack failure-domain placement is unavailable on the single-host provider",
            ));
        }
    }
    Ok(())
}

/// Build the contract-shaped inspect response from stored state.
fn inspect_response(volume_id: &VolumeId, stored: &StoredVolume) -> InspectVolumeResponse {
    let attachment = stored.runtime.attachment.as_ref();
    InspectVolumeResponse {
        volume_id: volume_id.clone(),
        backend_class: VolumeClass::NativeLocal,
        project_id: stored.entry.project_id.clone(),
        generation: stored.entry.generation,
        state: stored.runtime.state,
        // Thick LVs: provisioning equals allocation, both at the effective
        // (extent-rounded) size LVM actually created.
        provisioned_bytes: stored.entry.size_bytes,
        allocated_bytes: stored.entry.size_bytes,
        effective_protection: EffectiveProtection::default(),
        failure_domain: FailureDomain::Host,
        // Unknown until proven; never a healthy default.
        health: Health::Unknown,
        attachment_ids: attachment.map(|a| vec![a.id.clone()]).unwrap_or_default(),
        attachments: attachment
            .map(|a| attachment_view(a, &stored.entry))
            .into_iter()
            .collect(),
        current_writer: attachment
            .filter(|a| a.access_mode == AccessMode::SingleWriter)
            .map(|a| a.id.clone()),
        backend_health: Health::Unknown,
        evidence_status: EvidenceStatus::PrototypeOnly,
        authority: None,
    }
}

fn attachment_view(record: &AttachmentRecord, entry: &VolumeEntry) -> AttachmentView {
    AttachmentView {
        attachment_id: record.id.clone(),
        vm_id: record.vm_id.clone(),
        host_id: record.host_id.clone(),
        access_mode: record.access_mode,
        attachment_generation: record.generation,
        // Host-scoped, ephemeral backend handle; never a secret.
        frontend: Frontend::VirtioBlk {
            host_device_path: format!("/dev/{}/{}", entry.vg_name, entry.lv_name),
        },
        vmm_disk_id: record.vmm_disk_id.clone(),
        state: record.state,
        state_revision: record.state_revision,
        evidence: record.evidence,
        reporter: record.reporter.clone(),
        recorded_at_ms: record.recorded_at_ms,
    }
}

/// Build the attach response for a recorded attachment.
fn attach_response(record: &AttachmentRecord, entry: &VolumeEntry) -> AttachVolumeResponse {
    AttachVolumeResponse {
        attachment_id: record.id.clone(),
        attachment_generation: record.generation,
        volume_generation: entry.generation,
        // Host-scoped, ephemeral backend handle; never a secret.
        frontend: Frontend::VirtioBlk {
            host_device_path: format!("/dev/{}/{}", entry.vg_name, entry.lv_name),
        },
        // A fresh record is provider-recorded Prepared; a replay answers
        // the recorded evidence, exactly as the conformance fake does.
        state: record.state,
    }
}

impl AttachmentRecordView for AttachmentRecord {
    fn attachment_id(&self) -> &AttachmentId {
        &self.id
    }

    fn vm_id(&self) -> &str {
        &self.vm_id
    }

    fn host_id(&self) -> &HostId {
        &self.host_id
    }

    fn access_mode(&self) -> AccessMode {
        self.access_mode
    }

    fn vmm_disk_id(&self) -> Option<&str> {
        self.vmm_disk_id.as_deref()
    }

    fn attachment_generation(&self) -> u64 {
        self.generation
    }
}

impl AttachmentStateRecord for AttachmentRecord {
    fn attachment_state(&self) -> AttachmentState {
        self.state
    }

    fn state_revision(&self) -> u64 {
        self.state_revision
    }

    fn evidence(&self) -> Option<AttachmentEvidence> {
        self.evidence
    }

    fn reporter(&self) -> Option<&str> {
        self.reporter.as_deref()
    }

    fn recorded_at_ms(&self) -> Option<u64> {
        self.recorded_at_ms
    }

    fn record_reported_state(
        &mut self,
        state: AttachmentState,
        evidence: AttachmentEvidence,
        reporter: Option<String>,
        recorded_at_ms: u64,
    ) {
        self.state = state;
        self.state_revision += 1;
        self.evidence = Some(evidence);
        self.reporter = reporter;
        self.recorded_at_ms = Some(recorded_at_ms);
    }
}
