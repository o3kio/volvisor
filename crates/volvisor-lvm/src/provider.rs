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
//! # Coalescing durable writer
//!
//! Every production save of the provider state — volume, move, admin
//! claim/release and every compensation path — goes through one writer
//! (Phase D D3 slices 1–2, #62). A caller captures the exact bytes and
//! snapshot and reserves a monotonic sequence **under** the provider state
//! lock, so capture order equals sequence order. Attach, detach and an
//! applied attachment report then release that lock before waiting for
//! durability; synchronous mutations keep it and lead inline. One leader
//! performs a single physical write of the latest pending capture and
//! marks every covered sequence durable, so concurrent attach/detach/
//! report saves coalesce instead of writing once per caller.
//!
//! Watermarks are permanent and per provider lineage: `durable_seq` and
//! `failed_through` only advance, and a waiter resolves through them or
//! the terminal poison — there is no transient barrier. On an unprovable
//! write failure (readable-but-different or unreadable durable) every
//! outstanding sequence fails and admission must re-align with the
//! durable file; only a leader panic, abandonment or a poisoned mutex
//! poisons the writer. The leader never takes the provider state lock.
//!
//! The physical-write gate is per canonical state path in this process,
//! so two live providers aliasing one path serialize their writes while
//! different paths proceed independently. Two live providers aliasing one
//! path are **unsupported**: their sequence spaces are per lineage even
//! though their gate is shared. Each write uses a unique bounded
//! `.volvisor-<hash>.<pid>.<sequence>.tmp`, so writers can never truncate
//! or delete each other's temp file. The constructor sweeps stale sibling
//! temp files from a crashed save before it loads or writes state (see
//! [`crate::state`]). Cross-process exclusion remains the journal flock;
//! two daemons must not alias a custom `lvm_state_path`.
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
//!   leak an untracked LV. A runner-level `lvcreate` error (no exit
//!   status) has an unknown outcome, so the intent is kept for startup
//!   recovery instead of being cleared.
//! - **Grow**: the intent is persisted before `lvextend`. Startup rolls
//!   the grow forward (at or above the request completes the record and
//!   bumps the generation; still at the from-size drops the intent;
//!   anything else fails startup rather than guessing). A durable
//!   delete intent at the current generation takes precedence: the grow
//!   is dropped and the delete proceeds, so a grow never bumps the
//!   generation and voids a later delete. A runner-level `lvextend`
//!   error keeps the intent (unknown outcome). A failed final save
//!   keeps the observed new size in memory and leaves the durable grow
//!   intent for restart; the LV is **never** shrunk as compensation
//!   (data may already live in the new extents).
//! - **Delete**: the intent is persisted before `blkdiscard`/
//!   `lvremove`. Startup completes the recorded delete; a
//!   stale-generation intent is dropped without any deletion (never
//!   delete on a stale authorization), foreign LVs are never touched
//!   (AGENTS rule 7), and an unsettled online move refuses the delete
//!   before any destructive command. A failed destructive step keeps
//!   the durable intent so the volume stays fenced until a retry or
//!   restart completes it (a startup failure leaves the LV and
//!   authorization untouched and fails startup); completion removes the
//!   settled move record with the volume and intent in one atomic save.
//! - **Move**: `PREPARING` is the reconciliation anchor. A failed
//!   `PREPARING` save starts no `pvmove`. A failed `COPYING` save keeps
//!   the prior durable record in memory, or the just-saved `Copying`
//!   record when its rename landed or the durable file cannot be
//!   re-read (possibly durable). A failed `COMPLETE` save restores the
//!   prior record and generation when the re-read proves the durable
//!   file still says the prior state, but keeps the verified completion
//!   (with its bumped generation) when the rename landed or the re-read
//!   fails — so a later save cannot erase the completion or regress the
//!   generation. In every case classification is left to
//!   startup/reconcile: the world is never assumed lost and never
//!   blindly restarted.
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::{Deref, DerefMut};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, Weak};

use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
use std::sync::atomic::AtomicUsize;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};
use volvisor_provider::VolumeProvider;
use volvisor_provider::semantics::{
    AttachmentIdentity, AttachmentRecordView, AttachmentStateRecord, apply_attachment_report,
    ensure_expected_attachment_generation, ensure_expected_generation, ensure_no_active_attachment,
    resolve_attachment_replay,
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
    AttachmentRecord, CreateIntent, DeleteIntent, GrowIntent, LvmState, RetiredAttachmentFilter,
    StoredVolume, VolumeEntry, VolumeRuntime,
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

/// Maximum optimistic capacity-sample rounds for one grow admission.
///
/// Each stale round costs another `vgs` call. The cap keeps a grow from
/// looping forever under sustained allocation churn; exhaustion returns an
/// `OVERLOADED` refusal. The refusal is raised inside the journaled run
/// closure, so it is the terminal outcome for that `operation_id`: a client
/// retry must use a new operation id.
const GROW_ADMISSION_ATTEMPTS: usize = 32;

/// Maximum optimistic capacity-sample rounds for one create admission.
///
/// Same liveness guard as [`GROW_ADMISSION_ATTEMPTS`]: each stale round
/// costs another `vgs` call, so sustained allocation churn must not keep one
/// create looping forever. Exhaustion returns an `OVERLOADED` refusal and the
/// client retries with a new operation id.
const CREATE_ADMISSION_ATTEMPTS: usize = 32;

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

/// The outcome of one grow-preparation attempt under the state lock.
///
/// `SampleStale` means an allocation completed after the `vgs` sample was
/// taken; the reservation has been released and the caller must re-sample.
enum GrowPrepare<'a> {
    Prepared {
        grow_intent: GrowIntent,
        reservation: CapacityReservation<'a>,
    },
    SampleStale,
}

/// The outcome of one create-preparation attempt under the state lock.
///
/// `SampleStale` means an allocation completed after the `vgs` sample was
/// taken; the reservation has been released and the caller must re-sample.
enum CreatePrepare<'a> {
    Prepared {
        intent: CreateIntent,
        reservation: CapacityReservation<'a>,
    },
    SampleStale,
}

/// The volume fields one grow admission derives under the state lock and
/// re-validates after the capacity sample releases and reacquires it.
struct GrowAdmission {
    vg_name: String,
    lv_name: String,
    current_size: u64,
    current_generation: u64,
    has_attachment: bool,
}

/// A grow admission that has sampled capacity, reserved it, and made its
/// recovery intent durable.
///
/// The [`CapacityReservation`] must stay alive until the `lvextend` answer
/// arrives; the capacity admission domain itself is released when
/// [`LvmProvider::prepare_grow_admission`] returns.
struct PreparedGrow<'a> {
    grow_intent: GrowIntent,
    reservation: CapacityReservation<'a>,
    vg_name: String,
    lv_name: String,
    current_size: u64,
    current_generation: u64,
    has_attachment: bool,
}

/// The outcome of grow admission: either a pending grow that the world
/// already proves landed (answer without another `lvextend`), or a drive
/// ready to extend.
enum GrowAdmissionOutcome<'a> {
    Finalized {
        has_attachment: bool,
        effective_size_bytes: u64,
    },
    Prepared(PreparedGrow<'a>),
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
    /// Records resolved `FAILED` because the LV is **verifiably absent**
    /// (a successful `lvs` query has no row for it). The move can never
    /// complete and no `pvmove` can be active; the settled record stops
    /// fencing delete, and the volume is marked `Failed` in the same
    /// durable save.
    pub resolved_absent: Vec<VolumeId>,
    /// Records parked `IN_DOUBT` at the unobservable-LV bound: the
    /// world could not be observed for `MOVE_OBSERVE_FAILURE_BOUND`
    /// consecutive passes, so the record surfaces instead of
    /// silently spinning.
    pub parked_unobservable: Vec<VolumeId>,
    /// PREPARING records whose `pvmove` was (re-)started this pass.
    pub redriven: Vec<VolumeId>,
    /// PREPARING records whose re-drive did not start this pass: the
    /// foreign-move scan could not clear the source (an out-of-band
    /// `pvmove` is active there, or the world could not be observed),
    /// the record has no usable source/target, the record changed or
    /// was superseded under the lock, no **durable** anchor exists
    /// (memory-only, or the durable file could not be re-read), or the
    /// `pvmove` start itself failed. The record stays PREPARING and is
    /// retried on the next tick; surfaced so a persistent failure is
    /// not silent.
    pub redrive_failed: Vec<VolumeId>,
    /// Records dropped (their volume no longer exists).
    pub dropped: Vec<VolumeId>,
    /// Records skipped: the world could not be observed this pass
    /// (retried on the next tick; never resolved destructively).
    pub unobservable: Vec<VolumeId>,
    /// Records whose verified completion this pass deferred because the
    /// volume has a live grow. A completion bumps the volume generation
    /// without touching the grow's durable intent, so recording it while
    /// the resize is in flight would make the intent look stale to
    /// recovery; the pass must skip this record and keep classifying the
    /// rest, not abort. Retried on the next tick (the grow normally
    /// resolves well inside one tick; a durable intent with no live drive
    /// — an unknown `lvextend` outcome — can defer until a same-request
    /// retry or a restart classifies it, without starving other records).
    pub deferred_while_growing: Vec<VolumeId>,
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
    /// The coalescing durable writer every production save uses (Phase D
    /// D3 slice 2, #62). Per provider lineage: a rig that drops and
    /// rebuilds the provider after a crash gets a fresh sequence space.
    writer: Arc<WriterCore>,
    /// The move drive's timing (production defaults; overridable for
    /// the deterministic fake world only).
    move_timing: MoveTiming,
    /// Consecutive world-observe failures per move record, counted
    /// by the retry reconcile against
    /// [`MOVE_OBSERVE_FAILURE_BOUND`] (the unobservable-LV limbo
    /// bound). In-memory and per-incarnation: it never persists, and
    /// a successful observation clears it.
    move_observe_failures: Mutex<BTreeMap<VolumeId, u32>>,
    /// Volume ids with a live grow drive. A grow releases the provider
    /// state lock across the long `lvextend`/`lvs` commands, so this
    /// in-memory set prevents a second grow of the same volume from
    /// resuming the same durable intent concurrently. It is fail-closed
    /// and non-blocking: the second grow is refused, never queued on a
    /// runtime worker.
    grow_drives: Mutex<BTreeSet<VolumeId>>,
    /// Volume ids with a live create or delete drive. Both paths release the
    /// provider state lock across external LVM commands, so this map refuses
    /// a concurrent structural mutation of the same volume instead of
    /// letting two bodies race (or spend the same capacity). It is
    /// fail-closed and non-blocking: the duplicate is refused, never queued.
    /// The durable intent (not this map) survives a restart.
    volume_drives: Mutex<BTreeMap<VolumeId, StructuralDriveKind>>,
    /// Bytes promised to in-flight grow and create allocations, keyed by
    /// volume group. The state lock no longer serializes the long
    /// `lvextend`/`lvcreate`, so capacity admission subtracts these
    /// reservations to stay honest: a create (or another grow) that takes
    /// the state lock while an allocation runs must not spend the same free
    /// space twice. The entry is removed when the command returns, after
    /// which `vgs` reports the real free space. Leaf lock: never held while
    /// taking the provider state lock.
    capacity_reservations: Mutex<BTreeMap<String, u64>>,
    /// Counts completed allocations (create `lvcreate`, grow `lvextend`)
    /// that may have reduced free space.
    ///
    /// Grow admission samples `vgs` without any admission lock so
    /// concurrent grows do not serialize on the LVM CLI. A sample is used
    /// only if this counter is unchanged across the whole sample and the
    /// reservation insertion; a create or a completed `lvextend` bumps it
    /// (under the state lock for a create) and forces a re-sample. Moves
    /// allocate outside this protocol (pre-existing residual).
    capacity_epoch: AtomicU64,
    /// The writer view: every mutation is admitted under this mutex.
    /// Attach/detach/report capture and reserve under it, then release it
    /// before waiting for durability; the synchronous multi-save paths
    /// still hold it across their save. Readers do **not** take it.
    state: Mutex<LvmState>,
    /// The last committed read view (issue #62). Published by the writer
    /// after durability (and by a lock-holding synchronous guard on drop),
    /// so `GET`/list/inspect never wait behind another request's whole-file
    /// fsync or LVM CLI call. Readers clone the `Arc` only; the lock is
    /// held for that clone, never for the duration of a read.
    read_snapshot: Arc<RwLock<Arc<LvmState>>>,
}

/// The detail of a physical-write gate poisoned by an earlier panic. The
/// exact string is part of the slice-1 fail-closed evidence.
const GATE_POISON_DETAIL: &str = "LVM provider durable writer lock poisoned by a previous failure";

/// The detail of a writer poisoned by a leader panic, abandonment or a
/// poisoned writer-state mutex.
const WRITER_POISON_DETAIL: &str = "LVM provider durable writer poisoned by a previous failure";

/// The detail returned when a capture is attempted before admission has
/// re-aligned in-memory state with the durable file.
const NEEDS_ALIGNMENT_DETAIL: &str =
    "LVM provider durable writer requires admission alignment before another save";

/// The detail returned when a leader quiesces without a permanent
/// watermark resolving its own capture. That is an internal invariant
/// violation, so it fails closed instead of reporting an unproven success.
const UNRESOLVED_LEADER_DETAIL: &str =
    "LVM provider durable writer leader could not prove its save durable or failed";

/// One reserved capture: the exact bytes and snapshot a caller wants
/// durable, plus the monotonic sequence assigned under the provider state
/// lock so capture order equals sequence order.
struct Capture {
    /// Encoded state bytes for this capture.
    encoded: Arc<[u8]>,
    /// In-memory snapshot this capture commits.
    snapshot: Arc<LvmState>,
}

/// The coalescing writer's bookkeeping (Phase D D3 slice 2, #62).
///
/// Watermarks are permanent and monotonic: a waiter resolves only when
/// `durable_seq` covers it (success), `failed_through` covers it (the
/// recorded failure), or the writer is poisoned (typed `INTERNAL`). There
/// is no transient barrier flag.
struct WriterState {
    /// Next sequence to assign (starts at one).
    next_seq: u64,
    /// The newest capture awaiting a physical write. Older captures are
    /// dropped: in-memory state is monotonic, so the newest snapshot is a
    /// superset and one write of it covers every earlier sequence. Keeping
    /// only the newest bounds retained memory to one encoded state plus one
    /// snapshot regardless of how many callers coalesce.
    latest: Option<(u64, Capture)>,
    /// Highest sequence proven durable by a completed physical write.
    durable_seq: u64,
    /// Highest sequence published to the read view.
    published_seq: u64,
    /// Highest sequence that has been terminally failed.
    failed_through: u64,
    /// The failure recorded for `failed_through`.
    failure: Option<(ApiErrorCode, String)>,
    /// Whether a leader currently owns the physical write loop.
    leader_active: bool,
    /// A hard failure requires the next admission to re-align in-memory
    /// state with the durable file before another capture is accepted.
    needs_alignment: bool,
    /// Terminal poison (leader panic, abandonment, or poisoned mutex).
    poisoned: Option<String>,
}

impl Default for WriterState {
    fn default() -> Self {
        Self {
            next_seq: 1,
            latest: None,
            durable_seq: 0,
            published_seq: 0,
            failed_through: 0,
            failure: None,
            leader_active: false,
            needs_alignment: false,
            poisoned: None,
        }
    }
}

/// The result of one physical write attempt, kept distinct from the
/// panic payload so an armed crash seam can be re-raised in band.
enum WriteOutcome {
    /// The write completed.
    Saved,
    /// The write returned a typed error (possibly after the rename).
    Failed(ApiError),
    /// The write panicked (an armed crash seam).
    Panicked(Box<dyn std::any::Any + Send>),
}

/// One leader-loop decision, taken under a single writer-lock critical
/// section so the no-pending-capture check and leadership release are
/// atomic.
enum LeaderStep {
    /// The writer is terminally poisoned.
    Poisoned(String),
    /// Write the latest pending capture.
    Write {
        /// Sequence of the capture to write.
        seq: u64,
        /// Pre-encoded bytes of the latest capture.
        encoded: Arc<[u8]>,
        /// Snapshot the latest capture commits.
        snapshot: Arc<LvmState>,
    },
    /// Nothing is pending: leadership has been released.
    Quiesce,
}

/// RAII leadership guard: a leader that panics or is abandoned before
/// clearing leadership poisons the writer and wakes/fails every waiter.
struct LeaderToken {
    /// The writer whose leadership this token owns.
    core: Arc<WriterCore>,
    /// Set once the leader has completed normally (or already poisoned).
    armed: bool,
}

impl LeaderToken {
    /// Disarm the token after the leader has resolved leadership.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LeaderToken {
    fn drop(&mut self) {
        if self.armed {
            self.core.abandon_leader();
        }
    }
}

/// One caller's reservation. The leader owns a [`LeaderToken`]; a waiter
/// resolves only through the permanent watermarks.
pub(crate) struct Reservation {
    /// The caller's reserved sequence.
    seq: u64,
    /// Whether this caller leads the physical write or waits.
    role: ReservationRole,
}

/// The two reservation roles.
enum ReservationRole {
    /// This caller owns the physical write loop (RAII-guarded).
    Leader(LeaderToken),
    /// This caller waits for a watermark.
    Waiter,
}

/// One admission-gate decision, read atomically under the writer mutex.
///
/// An admission may only derive an answer from the in-memory state when
/// this returns [`AdmissionGate::Ready`]: every reserved capture has then
/// resolved, so the in-memory view is the durable view. The variants are
/// checked in this order because a capture can coexist with
/// `needs_alignment` (for example an encode failure while an older
/// capture is still outstanding); that capture must resolve before the
/// durable file is reloaded.
enum AdmissionGate {
    /// No capture is outstanding and no alignment is required.
    Ready,
    /// A capture is outstanding; wait for its permanent watermark.
    Wait(u64),
    /// No capture is outstanding, but a failure requires reloading the
    /// durable file before this admission may proceed.
    Align,
}

/// The coalescing durable writer for one provider lineage.
///
/// Phase D D3 slice 2 (#62): attach/detach/report reserve a sequence under
/// the provider state lock, release it, and wait. The leader writes the
/// latest pending capture once and marks every covered sequence durable.
///
/// The physical-write gate is **per canonical state path in this process**,
/// so two live providers aliasing one state path serialize their writes,
/// while different paths get independent gates. Watermarks and pending
/// captures are **per provider lineage**, not shared across instances: a
/// rig that drops the old provider on restart gets a fresh sequence space.
/// Two live providers aliasing one state path are unsupported (cross-process
/// exclusion remains the journal flock).
///
/// Lock order is provider state lock, then this writer's state mutex. The
/// leader loop never takes the provider state lock, so a synchronous waiter
/// may hold it safely.
struct WriterCore {
    /// Canonical state file path.
    path: PathBuf,
    /// Per-canonical-path physical-write gate.
    gate: Arc<Mutex<()>>,
    /// The store-save crash seam shared with the provider.
    crash: Arc<StoreCrashHooks>,
    /// The read view the leader publishes after durability.
    read_snapshot: Arc<RwLock<Arc<LvmState>>>,
    /// The writer watermark/pending bookkeeping.
    state: Mutex<WriterState>,
    /// Wakes synchronous waiters (permanent watermark checks).
    wake: Condvar,
    /// Wakes asynchronous waiters; subscribe-before-check prevents a lost
    /// wake between the check and the await.
    notify: tokio::sync::Notify,
    /// Test-only count of physical write attempts (coalescing evidence).
    #[cfg(test)]
    physical_writes: AtomicUsize,
    /// Test-only hook that runs inside the leader's quiesce critical
    /// section, so a regression test can hold a reservation at the exact
    /// leadership-handoff boundary.
    #[cfg(test)]
    handoff_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// Process-wide gates keyed by canonical state-path identity. A `Weak`
/// lets a dropped provider's gate go away; dead entries are pruned on
/// every lookup.
static WRITER_GATES: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

impl WriterCore {
    /// Build the per-lineage writer around the provider's read view and
    /// crash seam. The gate itself is shared per canonical state path.
    fn new(
        path: &Path,
        crash: Arc<StoreCrashHooks>,
        read_snapshot: Arc<RwLock<Arc<LvmState>>>,
    ) -> Self {
        Self {
            path: path.to_path_buf(),
            gate: Self::gate_for(path),
            crash,
            read_snapshot,
            state: Mutex::new(WriterState::default()),
            wake: Condvar::new(),
            notify: tokio::sync::Notify::new(),
            #[cfg(test)]
            physical_writes: AtomicUsize::new(0),
            #[cfg(test)]
            handoff_hook: Mutex::new(None),
        }
    }

    /// The physical-write gate shared by every writer on `path` in this
    /// process.
    fn gate_for(path: &Path) -> Arc<Mutex<()>> {
        let identity = crate::state::canonical_state_identity(path);
        let mut gates = WRITER_GATES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        gates.retain(|_, gate| gate.strong_count() > 0);
        gates
            .get(&identity)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let gate = Arc::new(Mutex::new(()));
                gates.insert(identity, Arc::downgrade(&gate));
                gate
            })
    }

    /// Lock the writer bookkeeping, converting a poisoned mutex into the
    /// terminal poison state (so waiters cannot park forever).
    fn lock_writer(&self) -> MutexGuard<'_, WriterState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                if guard.poisoned.is_none() {
                    guard.poisoned = Some(WRITER_POISON_DETAIL.to_owned());
                }
                guard.needs_alignment = true;
                guard
            }
        }
    }

    /// Wake both waiter classes. Always call both: a notify with no
    /// current async waiter is harmless because waiters subscribe before
    /// re-checking the watermarks.
    fn wake_all(&self) {
        self.wake.notify_all();
        self.notify.notify_waiters();
    }

    /// Resolve one sequence against the permanent watermarks.
    fn resolution(state: &WriterState, seq: u64) -> Option<Result<(), ApiError>> {
        if state.durable_seq >= seq {
            return Some(Ok(()));
        }
        if state.failed_through >= seq {
            let (code, detail) = state.failure.clone().unwrap_or((
                ApiErrorCode::Internal,
                "LVM provider durable writer failed without a recorded cause".to_owned(),
            ));
            return Some(Err(ApiError::new(code, detail)));
        }
        if let Some(reason) = &state.poisoned {
            return Some(Err(ApiError::new(ApiErrorCode::Internal, reason.clone())));
        }
        None
    }

    /// Reserve a sequence for one capture. Poison and admission
    /// alignment are checked here, under the writer mutex, before the
    /// caller releases the provider state lock.
    fn reserve(
        self: &Arc<Self>,
        encoded: Arc<[u8]>,
        snapshot: Arc<LvmState>,
    ) -> Result<Reservation, ApiError> {
        let mut state = self.lock_writer();
        if let Some(reason) = state.poisoned.clone() {
            // A caller refused here may already hold an uncommitted
            // in-memory mutation (async attach/detach mutate before they
            // reserve). Force the next admission to re-align with the
            // durable file so that mutation can never be replayed as a
            // phantom success.
            state.needs_alignment = true;
            return Err(ApiError::new(ApiErrorCode::Internal, reason));
        }
        if state.needs_alignment {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                NEEDS_ALIGNMENT_DETAIL,
            ));
        }
        let seq = state.next_seq;
        state.next_seq += 1;
        state.latest = Some((seq, Capture { encoded, snapshot }));
        let role = if state.leader_active {
            ReservationRole::Waiter
        } else {
            state.leader_active = true;
            ReservationRole::Leader(LeaderToken {
                core: Arc::clone(self),
                armed: true,
            })
        };
        Ok(Reservation { seq, role })
    }

    /// Wait synchronously for a permanent watermark.
    fn wait_sync(&self, seq: u64) -> Result<(), ApiError> {
        let mut state = self.lock_writer();
        loop {
            if let Some(result) = Self::resolution(&state, seq) {
                return result;
            }
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Wait asynchronously for a permanent watermark using
    /// subscribe-before-check, so a wake between the check and the await
    /// is never lost.
    async fn wait_async(&self, seq: u64) -> Result<(), ApiError> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.lock_writer();
                if let Some(result) = Self::resolution(&state, seq) {
                    return result;
                }
            }
            notified.await;
        }
    }

    /// Mark a sequence durable and publish its snapshot when it is newer
    /// than the last published one.
    fn mark_durable(&self, seq: u64, snapshot: &Arc<LvmState>) {
        {
            let mut state = self.lock_writer();
            state.durable_seq = state.durable_seq.max(seq);
            // The written capture covers every sequence up to `seq`. Drop
            // it if it is the one that was just written; keep a newer
            // capture for the next loop iteration.
            if state
                .latest
                .as_ref()
                .is_some_and(|(latest_seq, _)| *latest_seq <= seq)
            {
                state.latest = None;
            }
        }
        self.publish_if_newer(seq, snapshot);
    }

    /// Publish `snapshot` only when `seq` is newer than the published
    /// sequence; a lower sequence never replaces a higher snapshot.
    fn publish_if_newer(&self, seq: u64, snapshot: &Arc<LvmState>) {
        let publish = {
            let mut state = self.lock_writer();
            if seq > state.published_seq {
                state.published_seq = seq;
                true
            } else {
                false
            }
        };
        if publish {
            match self.read_snapshot.write() {
                Ok(mut current) => *current = Arc::clone(snapshot),
                // A poisoned reader-view lock must not wedge every future
                // read: recover the inner value (the snapshot holds no
                // cross-field invariant that a panic could tear).
                Err(poisoned) => *poisoned.into_inner() = Arc::clone(snapshot),
            }
        }
    }

    /// Poison the writer and fail every outstanding sequence. Used by the
    /// RAII leadership guard when a leader is abandoned before resolving.
    fn abandon_leader(&self) {
        self.poison(WRITER_POISON_DETAIL);
    }

    /// Set the terminal poison, fail everything pending through the
    /// permanent watermark, and wake all waiters.
    fn poison(&self, detail: &str) {
        {
            let mut state = self.lock_writer();
            let max_pending = state
                .latest
                .as_ref()
                .map_or(state.failed_through, |(seq, _)| *seq);
            state.failed_through = state.failed_through.max(max_pending);
            state.latest = None;
            // Keep the first terminal poison: a later re-raise (for
            // example `commit_async` observing the leader's panic join)
            // must not replace the failure every waiter already sees.
            if state.poisoned.is_none() {
                state.poisoned = Some(detail.to_owned());
                state.failure = Some((ApiErrorCode::Internal, detail.to_owned()));
            }
            state.needs_alignment = true;
            state.leader_active = false;
        }
        self.wake_all();
    }

    /// Resolve the leader's own sequence after the pending queue is empty.
    fn finish_leader(&self, own_seq: u64, own_error: Option<ApiError>) -> Result<(), ApiError> {
        if let Some(error) = own_error {
            return Err(error);
        }
        let state = self.lock_writer();
        if let Some(result) = Self::resolution(&state, own_seq) {
            return result;
        }
        // The leader's own capture was reserved before leadership began,
        // so one of the watermarks above must resolve it. If none does,
        // the invariant is broken: never report success for a save whose
        // durability is unproven. Fail closed and force the next
        // admission to re-align.
        drop(state);
        self.require_alignment();
        Err(ApiError::new(
            ApiErrorCode::Internal,
            UNRESOLVED_LEADER_DETAIL,
        ))
    }

    /// The leader loop: take the latest pending capture, perform one
    /// physical write under the per-path gate, then advance the watermark
    /// or fail/poison.
    ///
    /// Must never take the provider state lock: a synchronous waiter may
    /// hold it while this runs.
    fn run_leader(&self, own_seq: u64, token: &mut LeaderToken) -> Result<(), ApiError> {
        let mut own_error: Option<ApiError> = None;
        loop {
            let (seq, encoded, snapshot) = match self.leader_step() {
                LeaderStep::Poisoned(reason) => {
                    self.finish_poisoned(token);
                    return Err(ApiError::new(ApiErrorCode::Internal, reason));
                }
                LeaderStep::Quiesce => {
                    self.wake_all();
                    token.disarm();
                    return self.finish_leader(own_seq, own_error);
                }
                LeaderStep::Write {
                    seq,
                    encoded,
                    snapshot,
                } => (seq, encoded, snapshot),
            };

            #[cfg(test)]
            self.physical_writes.fetch_add(1, Ordering::SeqCst);

            let outcome = match self.write_pending(&encoded) {
                Ok(outcome) => outcome,
                Err(error) => {
                    token.disarm();
                    return Err(error);
                }
            };
            match outcome {
                WriteOutcome::Saved => {
                    self.mark_durable(seq, &snapshot);
                    self.wake_all();
                }
                WriteOutcome::Failed(error) => {
                    // The rename is the commit point. A readable durable
                    // file that equals the attempted snapshot proves this
                    // capture landed even though the save returned an
                    // error (the `AfterRenameBeforeDirFsync` class).
                    let landed =
                        LvmState::load(&self.path).is_ok_and(|durable| durable == *snapshot);
                    if landed {
                        self.mark_durable(seq, &snapshot);
                        // The leader caller still receives the original
                        // save error, matching the single-save behavior.
                        own_error.get_or_insert(error);
                        self.wake_all();
                        continue;
                    }
                    // Readable-but-different or unreadable: the capture
                    // cannot be proven durable. Fail every outstanding
                    // sequence, publish nothing, and require the next
                    // admission to re-align. Never poison here: the
                    // existing sync compensation tests classify these
                    // strings per caller.
                    self.fail_unproven(seq, &error);
                    self.wake_all();
                    token.disarm();
                    return match own_error {
                        Some(leader_error) => Err(leader_error),
                        None => Err(error),
                    };
                }
                WriteOutcome::Panicked(payload) => {
                    self.fail_panicked(seq);
                    self.wake_all();
                    token.disarm();
                    resume_unwind(payload);
                }
            }
        }
    }

    /// Choose the leader's next step under one writer-lock critical
    /// section.
    ///
    /// The no-pending-capture check and the leadership release must be atomic:
    /// if they were separate, a reservation between them would observe
    /// `leader_active == true`, enqueue itself as a waiter, and then wait
    /// forever after this leader clears leadership and exits.
    fn leader_step(&self) -> LeaderStep {
        let mut state = self.lock_writer();
        if let Some(reason) = state.poisoned.clone() {
            return LeaderStep::Poisoned(reason);
        }
        let next = state.latest.as_ref().map(|(seq, capture)| {
            (
                *seq,
                Arc::clone(&capture.encoded),
                Arc::clone(&capture.snapshot),
            )
        });
        if let Some((seq, encoded, snapshot)) = next {
            LeaderStep::Write {
                seq,
                encoded,
                snapshot,
            }
        } else {
            // Test-only seam: run the probe while the writer mutex is
            // held. A reservation attempted here must block until
            // leadership is released atomically below; it can then
            // only become the next leader, never a stranded waiter.
            #[cfg(test)]
            let hook = self
                .handoff_hook
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            #[cfg(test)]
            if let Some(hook) = hook {
                hook();
            }
            state.leader_active = false;
            LeaderStep::Quiesce
        }
    }

    /// One physical write under the per-path gate. A poisoned gate is a
    /// terminal writer poison; the caller returns the typed error.
    fn write_pending(&self, encoded: &[u8]) -> Result<WriteOutcome, ApiError> {
        if let Ok(gate) = self.gate.lock() {
            let outcome = match catch_unwind(AssertUnwindSafe(|| {
                LvmState::save_encoded_with_hooks(encoded, &self.path, Some(&self.crash))
            })) {
                Ok(Ok(())) => WriteOutcome::Saved,
                Ok(Err(error)) => WriteOutcome::Failed(error),
                Err(payload) => WriteOutcome::Panicked(payload),
            };
            // Drop the gate before touching writer bookkeeping or
            // re-raising a panic; the gate is a leaf.
            drop(gate);
            Ok(outcome)
        } else {
            // A poisoned physical-write gate is itself a terminal writer
            // poison: no save can be serialized safely.
            self.poison(GATE_POISON_DETAIL);
            Err(ApiError::new(ApiErrorCode::Internal, GATE_POISON_DETAIL))
        }
    }

    /// Clear leadership and fail every pending sequence after the writer
    /// was already poisoned.
    fn finish_poisoned(&self, token: &mut LeaderToken) {
        self.lock_writer().leader_active = false;
        self.wake_all();
        token.disarm();
    }

    /// Record an unproven physical-write failure: cover every outstanding
    /// sequence through the permanent failure watermark, publish nothing,
    /// and require admission alignment. Never poison.
    fn fail_unproven(&self, seq: u64, error: &ApiError) {
        let mut state = self.lock_writer();
        let max_seq = state
            .latest
            .as_ref()
            .map_or(seq, |(latest_seq, _)| *latest_seq)
            .max(seq);
        state.failed_through = state.failed_through.max(max_seq);
        state.latest = None;
        state.failure = Some((error.code, error.detail.clone()));
        state.needs_alignment = true;
        state.leader_active = false;
    }

    /// Record a leader panic: cover every outstanding sequence, poison the
    /// writer, and require admission alignment before any restart.
    fn fail_panicked(&self, seq: u64) {
        let mut state = self.lock_writer();
        let max_seq = state
            .latest
            .as_ref()
            .map_or(seq, |(latest_seq, _)| *latest_seq)
            .max(seq);
        state.failed_through = state.failed_through.max(max_seq);
        state.latest = None;
        state.failure = Some((ApiErrorCode::Internal, WRITER_POISON_DETAIL.to_owned()));
        state.poisoned = Some(WRITER_POISON_DETAIL.to_owned());
        state.needs_alignment = true;
        state.leader_active = false;
    }

    /// Commit a reservation from an async caller: the leader runs its
    /// blocking write loop on `spawn_blocking`; a waiter awaits the
    /// permanent watermarks via `Notify` without pinning a worker.
    async fn commit_async(self: &Arc<Self>, reservation: Reservation) -> Result<(), ApiError> {
        match reservation.role {
            ReservationRole::Leader(mut token) => {
                let core = Arc::clone(self);
                let own_seq = reservation.seq;
                let joined = tokio::task::spawn_blocking(move || {
                    let result = core.run_leader(own_seq, &mut token);
                    token.disarm();
                    result
                })
                .await;
                match joined {
                    Ok(result) => result,
                    Err(join_error) => {
                        let detail = if join_error.is_panic() {
                            "LVM provider durable writer leader panicked"
                        } else {
                            "LVM provider durable writer leader task was cancelled"
                        };
                        self.poison(detail);
                        if join_error.is_panic() {
                            // Keep the armed-kill payload in band for the
                            // campaign rigs.
                            resume_unwind(join_error.into_panic());
                        }
                        Err(ApiError::new(ApiErrorCode::Internal, detail))
                    }
                }
            }
            ReservationRole::Waiter => self.wait_async(reservation.seq).await,
        }
    }

    /// The admission alignment check (read under the provider state lock).
    fn needs_alignment(&self) -> bool {
        self.lock_writer().needs_alignment
    }

    /// Whether admission must reload the durable file now: a failure is
    /// pending and no capture is outstanding.
    ///
    /// Alignment must never happen while a capture is outstanding: that
    /// capture's snapshot may be newer than the durable file, so loading
    /// the file first would point in-memory state below a snapshot that is
    /// about to be published. The caller holds the provider state lock, so
    /// no new capture can be reserved between this read and the clear.
    fn needs_idle_alignment(&self) -> bool {
        let state = self.lock_writer();
        state.needs_alignment && state.latest.is_none()
    }

    /// The current admission gate, read atomically under the writer mutex.
    ///
    /// The caller must hold the provider state lock: that guarantees no
    /// new capture can be reserved while the returned decision is acted
    /// on. `Ready` therefore cannot be invalidated by a later failure,
    /// because a failure requires an outstanding capture and there is
    /// none.
    fn admission_gate(&self) -> AdmissionGate {
        let state = self.lock_writer();
        if let Some((seq, _)) = state.latest.as_ref() {
            AdmissionGate::Wait(*seq)
        } else if state.needs_alignment {
            AdmissionGate::Align
        } else {
            AdmissionGate::Ready
        }
    }

    /// Clear the alignment flag after `lock_state` has replaced the guard
    /// with a readable durable file. No capture can be reserved between
    /// the flag observation and this clear: `reserve` holds the same
    /// mutex and refuses while the flag is set.
    ///
    /// The clear is refused while a capture is still outstanding: the
    /// durable file that was just loaded may predate that capture, so the
    /// next admission must wait for the capture and align again.
    fn clear_needs_alignment(&self) {
        let mut state = self.lock_writer();
        if state.latest.is_none() {
            state.needs_alignment = false;
        }
    }

    /// Require the next admission to re-align in-memory state with the
    /// durable file before accepting another capture. Used when a caller
    /// may still hold an uncommitted mutation (a refused reservation or
    /// an unproven leader result).
    fn require_alignment(&self) {
        self.lock_writer().needs_alignment = true;
    }
}

/// A writer view of [`LvmState`].
///
/// On drop it publishes the resulting in-memory state as the new read
/// snapshot if the guard was used mutably. This keeps the reader view
/// exactly as consistent with in-memory state as the old design (where
/// reads took the same mutex), without making reads wait for the writer's
/// fsync.
pub(crate) struct StateGuard<'a> {
    guard: MutexGuard<'a, LvmState>,
    writer: &'a Arc<WriterCore>,
    dirty: bool,
}

impl Deref for StateGuard<'_> {
    type Target = LvmState;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl DerefMut for StateGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.dirty = true;
        &mut self.guard
    }
}

impl StateGuard<'_> {
    /// Discard this guard's in-memory mutation after a failed durable
    /// save and suppress publication on drop.
    ///
    /// A failed save must never be observable as committed: without
    /// this, a subsequent read would report the mutation the caller
    /// just saw fail (and a same-id retry could replay it from memory
    /// instead of re-executing the durable write). Restoring the last
    /// published snapshot keeps reads and the provider's idempotency
    /// view aligned with what is actually durable.
    ///
    /// **Precondition: at most one successful durable save per guard.**
    /// The published snapshot is updated on guard drop, not per save, so
    /// reverting after a later failed save would also discard an earlier
    /// successful save that is already durable. Every current caller
    /// either performs one save or classifies the durable file explicitly;
    /// a future multi-save caller must not use this method.
    pub(crate) fn revert(&mut self) {
        let committed = match self.writer.read_snapshot.read() {
            Ok(current) => Arc::clone(&current),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        };
        *self.guard = (*committed).clone();
        self.dirty = false;
    }

    /// Capture the current state and reserve a durable sequence without
    /// publishing; the caller must drop this guard before it waits.
    ///
    /// Phase D D3 slice 2 (#62): attach/detach/report use this to capture
    /// `(sequence, encoded bytes, snapshot)` **under** the state lock, so
    /// reservation order equals mutation order, then drop the guard and
    /// wait for durability without holding the provider state lock.
    /// Publication is the writer's job after the physical write; dropping
    /// this guard does not publish.
    ///
    /// On an encode or reservation error the guard restores the last
    /// durable view, suppresses publication, and forces the next admission
    /// to align to the durable file. That is what prevents an uncommitted
    /// in-memory mutation (for example an attach the caller saw fail) from
    /// being replayed as a phantom success by a same-id retry.
    pub(crate) fn capture_and_reserve(&mut self) -> Result<Reservation, ApiError> {
        let encoded = match self.guard.encode_for_save() {
            Ok(encoded) => encoded,
            Err(error) => {
                self.revert();
                self.writer.require_alignment();
                return Err(error);
            }
        };
        let snapshot = Arc::new(self.guard.clone());
        let reserved = self.writer.reserve(Arc::from(encoded), snapshot);
        if let Err(error) = reserved {
            // A refused reservation (poison or a prior hard failure) must
            // never leave this mutation where a later admission can see
            // it. Restore the last published view and require admission
            // alignment.
            self.revert();
            self.writer.require_alignment();
            return Err(error);
        }
        // The writer publishes after durability; this guard must not.
        self.dirty = false;
        reserved
    }
}

impl Drop for StateGuard<'_> {
    fn drop(&mut self) {
        // A panicking writer may have left a half-applied mutation; the
        // mutex poisons (every later writer fails), and readers keep the
        // last good snapshot instead of observing torn state.
        if !self.dirty || std::thread::panicking() {
            return;
        }
        // Drop-publication is kept for lock-holding synchronous callers.
        // It cannot regress a writer-published snapshot: while this guard
        // holds the state lock no later capture can be reserved, so its
        // in-memory state is at least as new as every in-flight capture
        // the writer could publish. The async capture path uses
        // `capture_and_reserve` plus an explicit drop, which clears `dirty`.
        let published = Arc::new(self.guard.clone());
        match self.writer.read_snapshot.write() {
            Ok(mut current) => *current = published,
            // A poisoned reader-view lock must not wedge every future
            // read: recover the inner value (the snapshot holds no
            // cross-field invariant that a panic could tear).
            Err(poisoned) => *poisoned.into_inner() = published,
        }
    }
}

/// RAII marker for one live grow drive.
///
/// The drive releases the provider state lock across `lvextend`, so this
/// marker is what prevents a second grow of the same volume from
/// concurrently resuming that volume's durable intent. Removal on drop
/// (including unwinding) is what makes the exclusion recoverable.
struct GrowDriveGuard<'a> {
    provider: &'a LvmProvider,
    volume_id: VolumeId,
}

impl Drop for GrowDriveGuard<'_> {
    fn drop(&mut self) {
        let mut drives = self
            .provider
            .grow_drives
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        drives.remove(&self.volume_id);
    }
}

/// Which structural drive currently holds a volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StructuralDriveKind {
    Create,
    Delete,
}

impl StructuralDriveKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Delete => "delete",
        }
    }
}

/// RAII marker for one live create or delete drive.
///
/// Both paths release the provider state lock across external LVM commands,
/// so this marker prevents a concurrent structural mutation of the same
/// volume from racing the durable intent or spending the same capacity.
/// Removal on drop (including unwinding) is what makes the exclusion
/// recoverable; the durable intent is the cross-restart fence.
struct StructuralDriveGuard<'a> {
    provider: &'a LvmProvider,
    volume_id: VolumeId,
}

impl Drop for StructuralDriveGuard<'_> {
    fn drop(&mut self) {
        let mut drives = self
            .provider
            .volume_drives
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        drives.remove(&self.volume_id);
    }
}

/// A pool choice and the `vgs` sample it was admitted against.
struct PickedPool {
    vg_name: String,
    /// Free bytes the sample reported, before subtracting in-flight
    /// capacity reservations.
    sampled_free: u64,
    /// Extent-rounded bytes this allocation needs.
    demand: u64,
}

/// The outcome of running `lvcreate` without the provider state lock.
enum CreateCommandOutcome {
    /// The LV exists at or above the requested size; commit it.
    Created { effective: u64 },
    /// The command runner returned no exit status: the outcome is unknown.
    Unknown(ApiError),
    /// `lvcreate` reported a non-zero exit: a failed start to classify.
    StartFailed(ApiError),
    /// `lvcreate` succeeded but LVM's own report disagrees with the request.
    VerifyFailed(ApiError),
}

/// One in-flight allocation reservation in a volume group.
///
/// Taken under the provider state lock immediately after the capacity
/// admission that promised the bytes, and released when the allocation
/// command returns (after which `vgs` reports the truth). Removal on drop
/// covers every error and unwinding path.
struct CapacityReservation<'a> {
    provider: &'a LvmProvider,
    vg_name: String,
    bytes: u64,
}

impl Drop for CapacityReservation<'_> {
    fn drop(&mut self) {
        let mut reserved = self
            .provider
            .capacity_reservations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let remove = match reserved.get_mut(&self.vg_name) {
            Some(total) => {
                *total = total.saturating_sub(self.bytes);
                *total == 0
            }
            None => false,
        };
        if remove {
            reserved.remove(&self.vg_name);
        }
    }
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
        /// The stored record this request re-attaches to (its state
        /// and source direct the world scan and the start), `None`
        /// for a fresh drive.
        existing_record: Option<crate::state::MoveRecord>,
    },
}

/// The result of a verified move completion attempt.
enum MoveCompletion {
    /// The completion was recorded (or already was): the response to
    /// answer.
    Completed(MoveVolumeBackingResponse),
    /// A live grow holds the volume's durable recovery anchor, so
    /// recording the completion now would void it. The error detail
    /// names the in-flight grow; the reconcile pass records the
    /// deferral and classifies the remaining records.
    Deferred(ApiError),
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

/// The result of one `lvextend` invocation, kept distinct so the caller
/// can classify a known start failure (clear the intent) from an unknown
/// outcome (keep it for recovery) after the provider state lock has been
/// released for the command.
enum LvextendOutcome {
    /// The command reported success; the size is verified separately.
    Extended,
    /// A non-zero exit status from `lvextend`. Whether the command
    /// resized the LV is **not** assumed here: `settle_failed_lvextend`
    /// re-verifies the observed size before clearing the intent, so an
    /// exit status alone never proves the LV did not move.
    StartFailed(ApiError),
    /// The command runner returned an error with no exit status: the
    /// command may have reached LVM, so the outcome is unknown.
    Unknown(ApiError),
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
        // Remove temps a crashed previous incarnation left in this state
        // path's namespace before loading or writing. This runs before the
        // writer can allocate a name, so a recycled pid cannot collide with
        // a stale file; other files in the directory are never touched.
        crate::state::sweep_stale_temps(&state_path);
        let state = LvmState::load(&state_path)?;
        let read_snapshot = Arc::new(RwLock::new(Arc::new(state.clone())));
        let store_crash = Arc::new(StoreCrashHooks::new());
        let writer = Arc::new(WriterCore::new(
            &state_path,
            Arc::clone(&store_crash),
            Arc::clone(&read_snapshot),
        ));
        let provider = Self {
            runner,
            state_path,
            sysfs_root,
            vg_prefix,
            expected_auth_token,
            store_crash,
            writer,
            move_timing: MoveTiming::default(),
            move_observe_failures: Mutex::new(BTreeMap::new()),
            grow_drives: Mutex::new(BTreeSet::new()),
            volume_drives: Mutex::new(BTreeMap::new()),
            capacity_reservations: Mutex::new(BTreeMap::new()),
            capacity_epoch: AtomicU64::new(0),
            state: Mutex::new(state),
            read_snapshot,
        };
        // Startup reconciliation persists its result through the writer
        // guard, which publishes the post-reconcile reader view on drop.
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

    /// Persist the in-memory state through the coalescing durable writer.
    ///
    /// Every production mutation — volume, move, admin claim/release and
    /// every compensation path — routes through here, so the armed crash
    /// seam sees every commit boundary of the state file and no class
    /// keeps a second direct-file path (Phase D D3 slices 1–2, #62).
    ///
    /// The caller holds the provider state lock, so capture and sequence
    /// reservation happen under it (capture order equals sequence order).
    /// If another caller already leads, this call waits on the permanent
    /// watermarks while still holding the lock; the leader never takes that
    /// lock, so the wait cannot deadlock.
    pub(crate) fn persist(&self, state: &mut LvmState) -> Result<(), ApiError> {
        let encoded = state.encode_for_save()?;
        let snapshot = Arc::new(state.clone());
        let reservation = self.writer.reserve(Arc::from(encoded), snapshot)?;
        match reservation.role {
            ReservationRole::Leader(mut token) => {
                self.writer.run_leader(reservation.seq, &mut token)
            }
            ReservationRole::Waiter => self.writer.wait_sync(reservation.seq),
        }
    }

    /// Reconcile in-memory state after its durable save returned an error,
    /// keeping the per-caller classification slice 1 had.
    ///
    /// A readable durable file that equals `state` means the mutation is
    /// durable and memory is already aligned. Otherwise state returns to
    /// the last published snapshot (today's behavior), never to an
    /// arbitrary readable file: the caller still classifies its own save
    /// error. The writer's `needs_alignment` flag clears only when the
    /// resulting state is aligned to a readable durable file; an unreadable
    /// file leaves it set so admission retries (fail-closed), and an
    /// unaligned readable file leaves it set for the next `lock_state`.
    ///
    /// **Precondition: at most one successful durable save per guard.**
    /// The published snapshot is updated on guard drop, not per save, so
    /// reverting after a later failed save would also discard an earlier
    /// successful save that is already durable. Every current caller either
    /// performs one save or classifies the durable file explicitly; a
    /// future multi-save caller must not use this method.
    pub(crate) fn reconcile_failed_save(&self, state: &mut StateGuard<'_>) {
        match LvmState::load(&self.state_path) {
            Ok(durable) => {
                if durable != **state {
                    // Restore the last published view and suppress this
                    // guard's publication, so a later leader snapshot is
                    // never regressed. Callers still classify the error.
                    state.revert();
                }
                if durable == **state {
                    self.writer.clear_needs_alignment();
                }
            }
            Err(_) => {
                // Keep the existing per-caller classification: an
                // unreadable re-read reverts and does not clear alignment.
                state.revert();
            }
        }
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

    /// Lock the writer state, mapping poisoning to `INTERNAL`, and align
    /// in-memory state with the durable file when a writer failure left
    /// `needs_alignment` set and no capture is outstanding.
    ///
    /// This is the alignment point (Phase D D3 slice 2, #62). The durable
    /// file is loaded under the provider state lock; a successful load
    /// replaces the guard and clears the flag. An unreadable file returns a
    /// typed `INTERNAL` and leaves the flag set, so later admissions retry
    /// instead of serving state that may not match disk.
    ///
    /// Alignment is deliberately skipped while a capture is outstanding:
    /// that capture's snapshot may be newer than the durable file. Callers
    /// must consult [`WriterCore::admission_gate`] before deriving an
    /// answer from the returned state; [`Self::lock_state_stable`] does
    /// that for the synchronous callers, and the async
    /// attach/detach/report admissions wait without holding this lock.
    pub(crate) fn lock_state(&self) -> Result<StateGuard<'_>, ApiError> {
        let mut guard = self.state.lock().map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "LVM provider state lock poisoned by a previous failure",
            )
        })?;
        if self.writer.needs_idle_alignment() {
            let durable = LvmState::load(&self.state_path).map_err(|error| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "LVM provider requires admission alignment but the durable state \
                         could not be read: {}",
                        error.detail
                    ),
                )
            })?;
            *guard = durable;
            self.writer.clear_needs_alignment();
        }
        Ok(StateGuard {
            guard,
            writer: &self.writer,
            dirty: false,
        })
    }

    /// Admission for synchronous callers: hold the provider state lock and
    /// wait for every outstanding capture before returning a view that a
    /// decision may rely on.
    ///
    /// The wait is synchronous, but it cannot deadlock: the leader never
    /// takes the provider state lock, and holding that lock prevents any
    /// new capture from being reserved, so the sequence being waited for
    /// is already in flight. A covering failure resolves the wait, then
    /// the loop re-enters [`Self::lock_state`] so the failure's durable
    /// alignment replaces the uncommitted view before the caller decides
    /// anything.
    pub(crate) fn lock_state_stable(&self) -> Result<StateGuard<'_>, ApiError> {
        loop {
            let guard = self.lock_state()?;
            match self.writer.admission_gate() {
                AdmissionGate::Ready => return Ok(guard),
                AdmissionGate::Align => {
                    // `needs_alignment` was set after `lock_state` read
                    // it; re-enter so the durable file is reloaded.
                    drop(guard);
                }
                AdmissionGate::Wait(seq) => {
                    // Resolve the covering capture, then re-enter and
                    // check for a newer one (there cannot be one while
                    // this guard holds the state lock, but the failure
                    // path also needs the re-entry to align).
                    let _ = self.writer.wait_sync(seq);
                    drop(guard);
                }
            }
        }
    }

    /// Lock the writer state without the admission-alignment load.
    ///
    /// The failed-create compensation has already classified the durable
    /// file explicitly; when that file is unreadable it leaves
    /// `needs_alignment` set, so [`Self::lock_state_stable`] refuses to hand
    /// back a view. The compensation still needs to attempt its clear save
    /// and classify that attempt's outcome (the existing fail-closed
    /// direction), so this raw guard bypasses only the alignment load. A
    /// poisoned mutex still fails here.
    fn lock_state_compensation(&self) -> Result<StateGuard<'_>, ApiError> {
        let guard = self.state.lock().map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "LVM provider state lock poisoned by a previous failure",
            )
        })?;
        Ok(StateGuard {
            guard,
            writer: &self.writer,
            dirty: false,
        })
    }

    /// The committed read view (issue #62).
    ///
    /// Clones the published `Arc` under a short read lock; the caller
    /// then reads without any provider-state lock, so a concurrent
    /// mutation's whole-file fsync or LVM CLI call cannot block it.
    /// Publication happens after the writer proves a capture durable (or
    /// when a lock-holding synchronous guard drops, preserving the
    /// pre-#62 read behavior).
    pub(crate) fn state_snapshot(&self) -> Arc<LvmState> {
        match self.read_snapshot.read() {
            Ok(current) => Arc::clone(&current),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
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
        let state = self.state_snapshot();
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
    /// required. When the clear's rename did not land (or its re-read is
    /// unknown) the removed intent is re-inserted in memory, so a later
    /// successful save cannot erase the durable anchor. If the clear's
    /// re-read proves the durable file already holds this volume's record
    /// (the final save's rename had landed even though the first re-read
    /// failed), the volume is never erased from memory while its LV
    /// remains: the durable record is restored and the volume is served.
    /// Neither failure ever masks the original save error.
    #[allow(clippy::too_many_lines)] // one compensation/durable-classification narrative
    fn compensate_unpersisted_create(
        &self,
        mut state: StateGuard<'_>,
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        intent: Option<CreateIntent>,
        save_error: &ApiError,
    ) -> ApiError {
        let committed = LvmState::load(&self.state_path)
            .is_ok_and(|durable| durable.volume(volume_id).is_some());
        if committed {
            return Self::create_already_durable_error(volume_id, vg_name, lv_name, save_error);
        }

        // The failed final save may have left the writer requiring
        // admission alignment. This caller's compensation is the next
        // save in the same critical section, so reconcile here (never
        // inside `persist`): adopt a landed save, otherwise revert, and
        // leave the flag set when the durable file is unreadable.
        if self.writer.needs_alignment() {
            self.reconcile_failed_save(&mut state);
        }
        // Roll the uncommitted in-memory edit (volume inserted, intent
        // removed) back to the last published durable view before the guard
        // is released: a failed save must never become visible as a phantom
        // volume when the guard drops. The intent is removed from memory
        // only after the compensating removal below, then cleared durably.
        state.revert();

        // The compensating `lvremove` is an external CLI call, so run it
        // without the provider state lock; the durable-file classification
        // above stays under the lock, and the in-memory rollback plus clear
        // save below reacquire it. Phase F F2c (#62).
        drop(state);
        let removed = self
            .runner
            .run("lvremove", &["--yes", &format!("{vg_name}/{lv_name}")])
            .is_ok_and(|output| output.success);
        let mut state = match self.lock_state_stable() {
            Ok(state) => state,
            // The durable-file classification above already ran. When that
            // file is unreadable, alignment cannot load it and
            // `lock_state_stable` refuses; fall back to the raw guard so the
            // clear save is still attempted and its failure is classified
            // by the existing re-read (never a silent success). A poisoned
            // mutex fails here too and is reported below.
            Err(_align_error) => match self.lock_state_compensation() {
                Ok(state) => state,
                Err(lock_error) => {
                    // The removal already ran (or its outcome is unknown) and
                    // the create intent could not be cleared. Never mask the
                    // original save error: report both and leave the durable
                    // intent for recovery to classify.
                    return Self::compensation_relock_error(
                        save_error,
                        vg_name,
                        lv_name,
                        removed,
                        &lock_error,
                    );
                }
            },
        };
        state.remove_volume(volume_id);
        // Reconciliation may have restored the durable intent; the clear
        // save below is what removes it durably.
        state.remove_create_intent(volume_id);
        let mut detail = save_error.detail.clone();
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
        if let Err(clear_error) = self.persist(&mut state) {
            // If the durable file may still hold the create intent, put
            // it back in memory: a later successful save from this
            // provider must not erase an authorization startup
            // recovery still has to classify (the LV may be present if
            // the checked `lvremove` failed). The intent was removed
            // from memory before the failed final save, so the removed
            // copy is the only value available when the re-read itself
            // is unknown.
            let mut reinsert: Option<CreateIntent> = None;
            match LvmState::load(&self.state_path) {
                Ok(durable) => match durable.create_intent(volume_id) {
                    Some(durable_intent) => {
                        // The rename did not land: keep memory matching
                        // the durable file.
                        reinsert = Some(durable_intent.clone());
                        detail.push_str(
                            "; the durable create intent could not be cleared (the failed save \
                             left it in place) and remains durable: startup recovery will adopt the \
                             LV only if it is present and at least the requested size",
                        );
                        Self::push_create_recovery_note(&mut detail, vg_name, lv_name, removed);
                    }
                    None => match durable.volume(volume_id).cloned() {
                        None => {
                            // The clear save's rename landed: neither
                            // the intent nor the volume is durable, and
                            // memory agrees.
                            detail.push_str(
                                "; the compensating clear save reported an error but the \
                                 durable state records neither the create intent nor this \
                                 volume (the clear's rename landed), so the compensated \
                                 state is durable",
                            );
                        }
                        Some(_durable_volume) if removed => {
                            // The final save's rename had landed (the
                            // initial re-read failed), so the durable
                            // file still records the volume even though
                            // the compensating `lvremove` removed its
                            // LV. Never claim the compensated state is
                            // durable; startup marks the LV-less record
                            // Failed and a retry/restart converges.
                            detail.push_str(
                                "; the durable state still records this volume (the final \
                                 save's rename had landed) even though the compensating \
                                 lvremove removed its LV; the failed clear save left that \
                                 record in place, so startup marks a volume with no LV \
                                 Failed and a retry or restart converges — do not recreate \
                                 the LV by hand",
                            );
                        }
                        Some(durable_volume) => {
                            // The LV is still present and the durable
                            // file records the volume: the carve-out.
                            // Restore the durable record in memory so a
                            // later save cannot erase a volume the
                            // durable file proves exists.
                            state.insert_volume(volume_id.clone(), durable_volume);
                            detail = format!(
                                "{}; the durable state already contains this volume's record \
                                 (the final save's rename had landed) and the LV \
                                 {vg_name}/{lv_name} is still present, so the volume was kept: \
                                 the API serves it, and a retry with a new operation_id and the \
                                 same payload returns it",
                                save_error.detail
                            );
                        }
                    },
                },
                Err(load_error) => {
                    // Unknown: treat the intent as possibly durable and
                    // keep it in memory so the next save cannot erase it
                    // before a restart classifies it.
                    reinsert = intent;
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
                    Self::push_create_recovery_note(&mut detail, vg_name, lv_name, removed);
                }
            }
            if let Some(intent) = reinsert {
                state.insert_create_intent(volume_id.clone(), intent);
            }
            let cleanup_note = format!(" (cleanup save failed: {clear_error})");
            detail.push_str(&cleanup_note);
        }
        ApiError::new(save_error.code, detail)
    }

    /// Report a compensation whose durable-file classification completed but
    /// whose state lock could not be reacquired after the compensating
    /// `lvremove`.
    ///
    /// The original save error is never masked: its code and detail lead, and
    /// the removal outcome plus the re-lock failure are appended so recovery
    /// can classify the durable intent.
    fn compensation_relock_error(
        save_error: &ApiError,
        vg_name: &str,
        lv_name: &str,
        removed: bool,
        lock_error: &ApiError,
    ) -> ApiError {
        let mut detail = save_error.detail.clone();
        detail.push_str("; the compensating lvremove ");
        detail.push_str(if removed {
            "removed the LV"
        } else {
            "could not remove the LV"
        });
        detail.push_str(
            ", but the provider state lock could not be reacquired to clear the create intent (",
        );
        detail.push_str(&lock_error.to_string());
        detail.push_str("): ");
        Self::push_create_recovery_note(&mut detail, vg_name, lv_name, removed);
        ApiError::new(save_error.code, detail)
    }

    /// The carve-out's already-durable answer: the final save's rename
    /// landed, so the durable file (and the LV) agree the volume exists.
    fn create_already_durable_error(
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        save_error: &ApiError,
    ) -> ApiError {
        ApiError::new(
            save_error.code,
            format!(
                "{}; the durable state already contains this volume's record, so volume \
                 {volume_id} and its LV {vg_name}/{lv_name} were kept; the API serves the \
                 volume, and a retry with a new operation_id and the same payload returns it \
                 (reusing the failed operation_id replays the failure, or fails closed as \
                 in-doubt when the journal could not record the outcome)",
                save_error.detail
            ),
        )
    }

    /// Append the create-intent recovery note for a failed compensating
    /// clear save: either the LV was removed (recovery drops the intent
    /// once it observes no such LV) or it remains and needs a manual
    /// `lvremove`.
    fn push_create_recovery_note(detail: &mut String, vg_name: &str, lv_name: &str, removed: bool) {
        if removed {
            detail.push_str(
                "; the LV was removed, so recovery will drop the intent once it observes no \
                 such LV",
            );
        } else {
            detail.push_str("; otherwise manual lvremove ");
            detail.push_str(vg_name);
            detail.push('/');
            detail.push_str(lv_name);
            detail.push_str(" is required");
        }
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

    /// Refuse a structural mutation of a volume whose durable grow intent
    /// is still in flight.
    ///
    /// A grow releases the provider state lock across `lvextend` (so the
    /// long LVM resize no longer stalls every other volume). The durable
    /// intent is the anchor that startup recovery uses to roll the grow
    /// forward, and it records the generation the grow was authorized
    /// against. Allowing an attach, detach, move start or delete to change
    /// that generation (or remove the LV) while the resize is in flight
    /// would make a crash between `lvextend` and the final commit look
    /// like a stale authorization, so those mutations are refused until
    /// the grow resolves. A report is *not* refused: it only advances the
    /// attachment record and never moves the volume generation.
    fn ensure_no_pending_grow(state: &LvmState, volume_id: &VolumeId) -> Result<(), ApiError> {
        Self::pending_grow_error(state, volume_id).map_or(Ok(()), Err)
    }

    /// The typed refusal for a live simultaneous grow, if one exists.
    ///
    /// Split from [`Self::ensure_no_pending_grow`] so the move
    /// completion can *defer* to a grow (recording the refusal in the
    /// reconcile report) instead of aborting the whole pass.
    fn pending_grow_error(state: &LvmState, volume_id: &VolumeId) -> Option<ApiError> {
        state.grow_intent(volume_id).map(|intent| {
            ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "volume {volume_id} has a durable grow in progress (operation {}, \
                     generation {}); retry after that grow completes or resolves",
                    intent.operation_id, intent.expected_generation
                ),
            )
        })
    }

    /// Refuse a grow while an online move leaves the volume's placement
    /// unsettled.
    ///
    /// A completed move bumps the volume generation. A grow that ran
    /// across that completion would hold a durable intent whose recorded
    /// generation no longer matches a crash-time recovery read, so the
    /// two structural operations are mutually excluded.
    fn ensure_no_unsettled_move(state: &LvmState, volume_id: &VolumeId) -> Result<(), ApiError> {
        if let Some(record) = state.move_record(volume_id) {
            if Self::move_blocks_delete(record) {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "volume {volume_id} has an unsettled online move in state {:?}; \
                         resolve the move before growing the volume",
                        record.state
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Enter one live grow drive for `volume_id`, refusing a second one.
    ///
    /// The guard is removed on drop, so a completed, failed or unwound
    /// drive never leaves the volume permanently un-growable. The durable
    /// intent (not this set) is what survives a process restart.
    fn begin_grow_drive(&self, volume_id: &VolumeId) -> Result<GrowDriveGuard<'_>, ApiError> {
        let mut drives = self
            .grow_drives
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !drives.insert(volume_id.clone()) {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "volume {volume_id} already has a grow in progress; retry after it \
                     completes or resolves"
                ),
            ));
        }
        drop(drives);
        Ok(GrowDriveGuard {
            provider: self,
            volume_id: volume_id.clone(),
        })
    }

    /// Enter one live create or delete drive for `volume_id`, refusing a
    /// concurrent structural mutation of the same volume.
    ///
    /// Both paths release the provider state lock across external LVM
    /// commands. A second create/delete must not run that body against the
    /// same volume, so it is refused typed rather than queued. The guard is
    /// removed on drop, and the durable intent remains the crash-recovery
    /// fence.
    fn begin_structural_drive(
        &self,
        volume_id: &VolumeId,
        kind: StructuralDriveKind,
    ) -> Result<StructuralDriveGuard<'_>, ApiError> {
        let mut drives = self
            .volume_drives
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = drives.get(volume_id) {
            let existing = existing.as_str();
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "volume {volume_id} already has a {existing} in progress; retry after it \
                     completes or resolves"
                ),
            ));
        }
        drives.insert(volume_id.clone(), kind);
        drop(drives);
        Ok(StructuralDriveGuard {
            provider: self,
            volume_id: volume_id.clone(),
        })
    }

    /// Read the allocation epoch.
    ///
    /// This is a protocol counter, not a lock: readers compare it before
    /// and after a `vgs` sample and again after inserting their
    /// reservation. Bumps are release-ordered with the allocation they
    /// record; a grow needs the sample and the reservation insertion to
    /// observe the same epoch.
    fn capacity_epoch(&self) -> u64 {
        self.capacity_epoch.load(Ordering::SeqCst)
    }

    /// Record that an allocation completed and may have reduced free
    /// space. A create and a grow both call this under the state lock when
    /// the command returns, before releasing their reservation.
    fn note_capacity_change(&self) {
        self.capacity_epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Bytes currently promised to in-flight structural allocations in
    /// `vg_name`.
    fn reserved_capacity(&self, vg_name: &str) -> u64 {
        self.capacity_reservations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(vg_name)
            .copied()
            .unwrap_or_default()
    }

    /// Atomically check and promise `demand` bytes of `vg_name`'s free
    /// space, given a `vgs` sample.
    ///
    /// The check and the insertion happen under the reservation leaf lock,
    /// so concurrent allocations cannot both spend the same sampled free
    /// bytes (grow admission samples without the state lock, so grow-grow
    /// and grow-create exclusion rests on this leaf during the sample
    /// window).
    /// Returns the bytes already reserved for the VG on refusal so the
    /// typed refusal can report the honest accounting.
    ///
    /// Called under the provider state lock. Both create and grow subtract
    /// these reservations while the other command runs, so the freed-lock
    /// window cannot spend the same extents twice.
    fn try_reserve_capacity(
        &self,
        vg_name: &str,
        sampled_free: u64,
        demand: u64,
    ) -> Result<CapacityReservation<'_>, (u64, u64)> {
        let mut reserved = self
            .capacity_reservations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let already = reserved.get(vg_name).copied().unwrap_or_default();
        let free = sampled_free.saturating_sub(already);
        if demand.saturating_add(VG_HEADROOM_BYTES) > free {
            return Err((free, already));
        }
        let total = reserved.entry(vg_name.to_owned()).or_insert(0);
        *total = total.saturating_add(demand);
        drop(reserved);
        Ok(CapacityReservation {
            provider: self,
            vg_name: vg_name.to_owned(),
            bytes: demand,
        })
    }

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
                retired_attachments: RetiredAttachmentFilter::default(),
            },
        }
    }

    /// Remove a create intent after a failed `lvcreate` invocation
    /// (a non-success exit status with a known outcome) and attempt to
    /// clear it durably.
    ///
    /// The original command error is returned unchanged when the
    /// removal save succeeds. When the clear save fails, the intent is
    /// put back in memory whenever the durable file may still hold it
    /// (the rename did not land, or the re-read is unknown), so a later
    /// successful save cannot erase a durable authorization that
    /// startup recovery must still classify.
    fn clear_create_intent_after_failed_start(
        &self,
        state: &mut LvmState,
        volume_id: &VolumeId,
        error: ApiError,
    ) -> ApiError {
        let removed = state.remove_create_intent(volume_id);
        match self.persist(state) {
            Ok(()) => error,
            Err(clear_error) => {
                let note = match LvmState::load(&self.state_path) {
                    Ok(durable) => match durable.create_intent(volume_id) {
                        Some(durable_intent) => {
                            // The rename did not land: keep memory
                            // matching the durable file.
                            state.insert_create_intent(volume_id.clone(), durable_intent.clone());
                            format!(
                                "the create intent could not be cleared durably \
                                 ({clear_error}) and remains durable; startup recovery will \
                                 drop it if no intent-owned LV is present, adopt it only if \
                                 present and at least the requested size, and otherwise \
                                 refuse rather than adopt a wrong geometry"
                            )
                        }
                        None => format!(
                            "the create-intent clear save reported an error ({clear_error}), \
                             but the durable state no longer records that intent (its rename \
                             landed); the failed start left no durable intent to recover"
                        ),
                    },
                    Err(load_error) => {
                        // Unknown: treat the intent as possibly durable
                        // and keep it in memory so the next save cannot
                        // erase it before a restart classifies it.
                        if let Some(intent) = removed {
                            state.insert_create_intent(volume_id.clone(), intent);
                        }
                        format!(
                            "the create intent could not be verified after its clear save \
                             failed ({clear_error}; re-read failed: {load_error}); treat it as \
                             possibly durable — startup recovery will drop it if no \
                             intent-owned LV is present and adopt it only if present and at \
                             least the requested size"
                        )
                    }
                };
                ApiError::new(error.code, format!("{}; {note}", error.detail))
            }
        }
    }

    /// Remove a grow intent after a failed `lvextend` invocation
    /// (a non-success exit status with a known outcome) and attempt to
    /// clear it durably.
    ///
    /// The original command error is returned unchanged when the
    /// removal save succeeds. When the clear save fails, the intent is
    /// put back in memory whenever the durable file may still hold it
    /// (the rename did not land, or the re-read is unknown), so a later
    /// successful save cannot erase a durable authorization that
    /// startup recovery must still classify.
    fn clear_grow_intent_after_failed_start(
        &self,
        state: &mut LvmState,
        volume_id: &VolumeId,
        error: ApiError,
    ) -> ApiError {
        let removed = state.remove_grow_intent(volume_id);
        match self.persist(state) {
            Ok(()) => error,
            Err(clear_error) => {
                let note = match LvmState::load(&self.state_path) {
                    Ok(durable) => match durable.grow_intent(volume_id) {
                        Some(durable_intent) => {
                            // The rename did not land: keep memory
                            // matching the durable file.
                            state.insert_grow_intent(volume_id.clone(), durable_intent.clone());
                            format!(
                                "the grow intent could not be cleared durably \
                                 ({clear_error}) and remains for recovery, which completes the \
                                 grow only if the LV is at least the requested size"
                            )
                        }
                        None => format!(
                            "the grow-intent clear save reported an error ({clear_error}), but \
                             the durable state no longer records that intent (its rename \
                             landed); the failed start left no durable grow to recover"
                        ),
                    },
                    Err(load_error) => {
                        // Unknown: treat the intent as possibly durable
                        // and keep it in memory so the next save cannot
                        // erase it before a restart classifies it.
                        if let Some(intent) = removed {
                            state.insert_grow_intent(volume_id.clone(), intent);
                        }
                        format!(
                            "the grow intent could not be verified after its clear save failed \
                             ({clear_error}; re-read failed: {load_error}); treat it as \
                             possibly durable — recovery completes the grow only if the LV is \
                             at least the requested size"
                        )
                    }
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
    /// Choose the pool for a create from a capacity sample already taken
    /// outside the state lock.
    ///
    /// `capacity_map` is a `vgs` sample that the caller took while the state
    /// lock was released; it is validated against the allocation epoch there
    /// and again when the reservation is inserted. Selection uses the state
    /// under the lock, so a device claimed or released during the sample is
    /// still classified against the current registry.
    fn pick_pool_vg(
        &self,
        state: &LvmState,
        size_bytes: u64,
        capacity_map: &BTreeMap<String, VgCapacity>,
    ) -> Result<PickedPool, ApiError> {
        for entry in state.devices().values() {
            if entry.role != volvisor_types::DeviceRole::NativePool {
                continue;
            }
            if let Some(capacity) = capacity_map.get(&entry.vg_name) {
                let demand = extent_rounded(size_bytes, capacity.extent_bytes);
                // A grow (or another create) that released the state lock
                // promised these bytes to itself; do not spend them again.
                let free = capacity
                    .free_bytes
                    .saturating_sub(self.reserved_capacity(&entry.vg_name));
                if demand.saturating_add(VG_HEADROOM_BYTES) <= free {
                    return Ok(PickedPool {
                        vg_name: entry.vg_name.clone(),
                        sampled_free: capacity.free_bytes,
                        demand,
                    });
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
    ///   startup. A durable delete intent at the current generation is
    ///   the later authority: the grow intent is dropped so the delete
    ///   pass removes the volume without a generation bump voiding it.
    /// - **Delete intents**: a missing volume or a stale stored
    ///   generation drops the intent **without** deleting anything. A
    ///   live LV at the authorized generation runs the recorded
    ///   discard/`lvremove`; a failure fails startup loudly without
    ///   rewriting state. The volume, intent, any stale create/grow
    ///   intent and the settled move record are then removed together.
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
        let mut state = self.lock_state_stable()?;
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
            if let Err(error) = self.persist(&mut state) {
                self.reconcile_failed_save(&mut state);
                return Err(error);
            }
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
    /// A durable delete intent at the current generation is the later
    /// authorization: the grow intent is dropped so the delete pass
    /// removes the volume (a grow must not bump the generation and void
    /// the delete). A stale generation/from-size or an absent LV drops
    /// the intent (the volume pass marks a vanished LV `Failed`); an LV
    /// at or above the request completes the record and bumps the
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
            if state
                .delete_intent(&volume_id)
                .is_some_and(|pending| pending.expected_generation == stored.entry.generation)
            {
                // A durable delete authorized at this generation is
                // the later authorization. Rolling the grow forward
                // would bump the generation and silently void the
                // delete (a ZeroDiscard delete could then present
                // erased data as a live volume); let the delete pass
                // remove the volume instead.
                state.remove_grow_intent(&volume_id);
                changed = true;
                continue;
            }
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
    /// without rewriting state instead of serving a volume whose
    /// authorized delete is pending. The volume, intent, stale
    /// create/grow intents and the settled move record are removed
    /// together. Only the intent's own recorded volume/LV is ever
    /// touched.
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
            // A create/grow intent for this volume id is stale once the
            // volume is deleted: clear it in the same atomic save so a
            // later create with the same id cannot inherit an old
            // authorization.
            state.remove_create_intent(&volume_id);
            state.remove_grow_intent(&volume_id);
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
    /// The caller observed `intent` under the provider state lock and then
    /// dropped the guard: the size check is an `lvs` command and must not
    /// run under the state lock. Returns `Some(response)` when the
    /// intent-owned LV was verified and adopted (or `Err` when that
    /// adoption could not be made durable). Returns `None` when the intent's
    /// LV is verifiably absent (the intent is cleared in memory so a fresh
    /// create can proceed). A different payload or requested size is a typed
    /// idempotency conflict; a present LV smaller than the request is never
    /// adopted, and a state view that no longer holds the same intent fails
    /// closed.
    fn resume_create_intent(
        &self,
        req: &CreateVolumeRequest,
        payload: &str,
        intent: &CreateIntent,
    ) -> Result<Option<InspectVolumeResponse>, ApiError> {
        if intent.creation_payload != payload || intent.requested_size_bytes != req.size_bytes {
            return Err(ApiError::idempotency_conflict(&req.volume_id));
        }
        // Observe the intent-owned LV without the provider state lock.
        let observed = self.lv_size(&intent.vg_name, &intent.lv_name)?;
        let mut state = self.lock_state_stable()?;
        // The create/delete drive guard excludes another structural mutation
        // of this volume while the observation ran, so the intent must still
        // be the one this call observed. Refuse to adopt under a different
        // authorization (an aligned/reloaded state view is the only way this
        // can move).
        if state.create_intent(&req.volume_id) != Some(intent) {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "the durable create intent for volume {} changed while its LV was being \
                     verified; refusing to adopt a volume under a mismatched intent",
                    req.volume_id
                ),
            ));
        }
        match observed {
            Some(observed) if observed >= intent.requested_size_bytes => {
                let stored = Self::stored_volume_from_create_intent(intent, observed);
                state.remove_create_intent(&req.volume_id);
                state.insert_volume(req.volume_id.clone(), stored.clone());
                if let Err(save_error) = self.persist(&mut state) {
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
                // stale intent in memory and let the caller create fresh.
                state.remove_create_intent(&req.volume_id);
                Ok(None)
            }
        }
    }

    /// Run `lvcreate` and verify the observed geometry without the provider
    /// state lock.
    ///
    /// The caller writes the durable create intent before this call; it is
    /// the recovery anchor for the gap. The caller releases the create's
    /// capacity reservation as soon as this returns.
    fn run_create_lv(
        &self,
        req: &CreateVolumeRequest,
        intent: &CreateIntent,
    ) -> CreateCommandOutcome {
        let (vg_name, lv_name) = (intent.vg_name.as_str(), intent.lv_name.as_str());
        let output = match self.runner.run(
            "lvcreate",
            &[
                "--yes",
                "-L",
                &format!("{}B", req.size_bytes),
                "-n",
                lv_name,
                vg_name,
            ],
        ) {
            Ok(output) => output,
            Err(error) => {
                // The runner returned an error without an exit status
                // (spawn failure, timeout kill, or a wait/read failure),
                // so the outcome is unknown: the command may have
                // reached LVM. Keep the durable intent — never clear it
                // on an unknown outcome — and let startup recovery
                // classify the observed LV.
                return CreateCommandOutcome::Unknown(ApiError::new(
                    error.code,
                    format!(
                        "{}; the lvcreate outcome is unknown (the command runner returned an \
                         error rather than an exit status); the durable create intent remains, \
                         and startup recovery will adopt the LV only if it is present and at \
                         least the requested size, drop the intent if the LV is absent, and \
                         refuse startup if the LV is present but smaller",
                        error.detail
                    ),
                ));
            }
        };
        if !output.success {
            return CreateCommandOutcome::StartFailed(command_failed("lvcreate", &output));
        }
        // Verify against LVM's own report: a successful exit status is
        // not evidence of the requested geometry. Thick LVM rounds the
        // size up to whole physical extents, so anything at or above
        // the request is a success — the effective size is what gets
        // persisted and reported.
        match self.verify_created_size(vg_name, lv_name, req.size_bytes) {
            Ok(effective) => CreateCommandOutcome::Created { effective },
            Err(error) => CreateCommandOutcome::VerifyFailed(ApiError::new(
                error.code,
                format!(
                    "{}; the durable create intent remains, and startup recovery will \
                     adopt the LV only if it is present and at least the requested size, \
                     drop the intent if the LV is absent, and refuse startup if the LV \
                     is present but smaller",
                    error.detail
                ),
            )),
        }
    }

    /// Publish a completed create allocation and release its capacity
    /// reservation.
    ///
    /// `vgs` now reports the real free space, so the in-flight promise can
    /// be released on every outcome. The allocation epoch is bumped *before*
    /// the reservation is dropped (the same ordering as grow): a grow that
    /// sampled before this allocation must either see the bump and re-sample,
    /// or have observed this reservation in its accounting. Dropping first
    /// would let a stale sample reserve the freed bytes twice. The state lock
    /// is taken only for the atomic bump and the leaf-lock removal, never
    /// across an LVM command.
    fn publish_created_allocation(&self, reservation: CapacityReservation<'_>) {
        match self.lock_state_stable() {
            Ok(_state) => {
                self.note_capacity_change();
                drop(reservation);
            }
            Err(error) => {
                // `lock_state_stable` fails only on a poisoned state mutex
                // or an unreadable alignment load. The allocation has
                // already happened and `vgs` reports its real free space,
                // so the epoch must still be bumped and the reservation
                // released. Log it rather than swallowing the error,
                // because the invariant "publish under the state lock" did
                // not hold.
                tracing::warn!(
                    error = %error,
                    "create publish could not take the provider state lock; capacity epoch \
                     bumped and reservation released without the state guard"
                );
                self.note_capacity_change();
                drop(reservation);
            }
        }
    }

    /// Clear a create intent after a known-failed start, reacquiring the
    /// provider state lock only for the state change and its durable save.
    fn clear_create_intent_drive(&self, volume_id: &VolumeId, error: ApiError) -> ApiError {
        let mut state = match self.lock_state_stable() {
            Ok(state) => state,
            Err(lock_error) => {
                let code = error.code;
                return ApiError::new(
                    code,
                    format!(
                        "{}; the create intent could not be cleared because the provider state \
                         could not be re-read ({lock_error}); the durable intent remains, and \
                         startup recovery will adopt the LV only if it is present and at least \
                         the requested size, drop the intent if the LV is absent, and refuse \
                         startup if the LV is present but smaller",
                        error.detail
                    ),
                );
            }
        };
        self.clear_create_intent_after_failed_start(&mut state, volume_id, error)
    }

    /// Commit a verified create under a fresh provider state lock.
    ///
    /// The command ran without the lock. Reacquire and revalidate that the
    /// durable intent is still the one this create was authorized under, then
    /// insert the record, drop the intent and save once. A failed final save
    /// runs the existing compensation, whose compensating `lvremove` now runs
    /// outside the provider state lock (Phase F F2c).
    fn commit_created_volume(
        &self,
        req: &CreateVolumeRequest,
        intent: &CreateIntent,
        effective: u64,
    ) -> Result<InspectVolumeResponse, ApiError> {
        let mut state = self.lock_state_stable()?;
        if state.create_intent(&req.volume_id) != Some(intent) {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "the durable create intent for volume {} changed while lvcreate ran; \
                     refusing to commit a volume under a mismatched intent",
                    req.volume_id
                ),
            ));
        }
        let stored = StoredVolume {
            entry: VolumeEntry {
                vg_name: intent.vg_name.clone(),
                lv_name: intent.lv_name.clone(),
                size_bytes: effective,
                requested_size_bytes: req.size_bytes,
                generation: 1,
                data_epoch: 0,
                project_id: req.project_id.clone(),
                block_size: intent.block_size,
                creation_payload: intent.creation_payload.clone(),
            },
            runtime: VolumeRuntime {
                state: VolumeLifecycle::Ready,
                attachment: None,
                retired_attachments: RetiredAttachmentFilter::default(),
            },
        };
        state.insert_volume(req.volume_id.clone(), stored.clone());
        // The final save commits the volume record and the intent
        // removal together. Keep the intent so a failed compensating
        // clear save can put it back in memory when the durable file
        // may still hold it.
        let create_intent = state.create_intent(&req.volume_id).cloned();
        state.remove_create_intent(&req.volume_id);
        if let Err(save_error) = self.persist(&mut state) {
            return Err(self.compensate_unpersisted_create(
                state,
                &req.volume_id,
                &intent.vg_name,
                &intent.lv_name,
                create_intent,
                &save_error,
            ));
        }
        Ok(inspect_response(&req.volume_id, &stored))
    }

    /// Create with a durable-first create intent.
    ///
    /// The intent is persisted before `lvcreate`; a retry with a
    /// matching intent adopts an intent-owned LV only when it is
    /// present and at least the requested size, and clears the intent
    /// when the LV is verifiably absent. A failed start clears the
    /// intent (leaving it durable if that clear fails, for startup
    /// recovery). A failed size verification leaves the durable intent
    /// so recovery never adopts a wrong geometry.
    ///
    /// Phase E E2 (#62): once the intent is durable, the provider state lock
    /// is released for `lvcreate` and its size-verifying `lvs`. The final
    /// commit reacquires it, and a failed final save runs
    /// [`Self::compensate_unpersisted_create`], whose carve-out keeps the
    /// volume when the durable record already landed.
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
        // One live structural drive per volume. Create and delete both
        // release the state lock across external LVM commands, so a
        // concurrent structural mutation of the same volume is refused
        // rather than racing the durable intent or spending the same
        // capacity.
        let _drive = self.begin_structural_drive(&req.volume_id, StructuralDriveKind::Create)?;

        // Admission and allocation are deliberately separate critical
        // sections. Admission decides (replay, adopt, or a fresh durable
        // intent) under the state lock; the allocation and its verification
        // run without it. A durable intent found at admission needs an `lvs`
        // look before it can be adopted or cleared, so that look is outside
        // the lock too.
        let mut resumed: Option<CreateIntent> = None;
        // A stale capacity sample costs another `vgs` call. Bound the
        // optimistic rounds so sustained allocation churn cannot keep one
        // create looping forever; the caller can retry with a new operation
        // id. This is a liveness guard, not a capacity answer.
        let mut attempts = 0usize;
        let (intent, reservation) = loop {
            attempts += 1;
            if attempts > CREATE_ADMISSION_ATTEMPTS {
                return Err(ApiError::new(
                    ApiErrorCode::Overloaded,
                    format!(
                        "capacity admission for volume {} could not obtain a stable \
                         free-space sample after {CREATE_ADMISSION_ATTEMPTS} attempts; retry \
                         the create with a new operation_id",
                        req.volume_id
                    ),
                ));
            }
            let mut state = self.lock_state_stable()?;
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
            //
            // Any retry that finds an intent must treat the allocation epoch
            // as changed, because the earlier attempt may already have spent
            // space: adoption returns early below, and a refusal would
            // otherwise leave a grow's pre-attempt sample valid. Bumping on
            // the absent-LV path too is conservative (one extra sample).
            if let Some(pending) = state.create_intent(&req.volume_id).cloned() {
                if resumed.as_ref() == Some(&pending) {
                    // This call already verified this intent's LV absent; a
                    // state-alignment reload restored the durable copy. Clear
                    // it in memory and continue with the fresh allocation.
                    state.remove_create_intent(&req.volume_id);
                } else if resumed.is_some() {
                    return Err(ApiError::new(
                        ApiErrorCode::Internal,
                        format!(
                            "the durable create intent for volume {} changed while its LV was \
                             being verified; refusing to allocate under a different intent",
                            req.volume_id
                        ),
                    ));
                } else {
                    self.note_capacity_change();
                    resumed = Some(pending.clone());
                    drop(state);
                    match self.resume_create_intent(req, &payload, &pending)? {
                        Some(response) => return Ok(response),
                        // The prior attempt never created the LV; its intent
                        // is cleared in memory. Re-enter admission to create
                        // fresh.
                        None => continue,
                    }
                }
            }

            // Sample pool capacity outside the state lock. `vgs` is an
            // external CLI call; holding the single provider mutex across it
            // queued every other volume's mutation behind this create's
            // probe. The allocation epoch fences a stale sample: a grow or
            // another create that completes an allocation during the sample
            // bumps it and this attempt re-samples. Phase F F2b (#62).
            let epoch = self.capacity_epoch();
            drop(state);
            let capacity_map = self.vg_capacity_map()?;
            if self.capacity_epoch() != epoch {
                // An allocation completed during the sample; the free
                // figure may already be stale.
                continue;
            }

            let mut state = self.lock_state_stable()?;
            // Re-check this volume's admission after the released window.
            // The structural drive already excludes a second create or
            // delete of the same id, but a state-alignment reload must not
            // let a replay or a durable intent slip past this attempt: if one
            // appeared, re-enter admission so its rules classify it rather
            // than allocating over it.
            self.ensure_no_pending_delete(&state, &req.volume_id)?;
            if state.volume(&req.volume_id).is_some()
                || state.create_intent(&req.volume_id).is_some()
            {
                continue;
            }

            match self.prepare_create_intent(&mut state, req, &payload, &capacity_map, epoch)? {
                CreatePrepare::SampleStale => {
                    // The reservation was released by
                    // `prepare_create_intent`; a create or completed grow got
                    // between the sample and the reservation. Re-sample on
                    // the next loop iteration.
                }
                CreatePrepare::Prepared {
                    intent,
                    reservation,
                } => {
                    break (intent, reservation);
                }
            }
        };

        // The intent is durable; the state lock is now released. `lvcreate`
        // and the size-verifying `lvs` are the long-running steps of a
        // create; holding the single provider mutex across them queued every
        // other volume's mutation behind one allocation.
        let outcome = self.run_create_lv(req, &intent);
        // The command returned: `vgs` now reports the real free space, so
        // the in-flight promise can be released on every outcome.
        self.publish_created_allocation(reservation);
        match outcome {
            CreateCommandOutcome::Created { effective } => {
                // The final commit is a fresh critical section: other
                // volumes may have saved state while the command ran.
                self.commit_created_volume(req, &intent, effective)
            }
            CreateCommandOutcome::Unknown(error) => {
                // Keep the durable intent for recovery; it stays in the
                // in-memory state, so structural mutations remain fenced.
                Err(error)
            }
            CreateCommandOutcome::StartFailed(error) => {
                Err(self.clear_create_intent_drive(&req.volume_id, error))
            }
            CreateCommandOutcome::VerifyFailed(error) => {
                // The durable intent stays: recovery adopts only a correct
                // geometry, and a partially-created LV cannot leak unowned.
                Err(error)
            }
        }
    }

    /// Validate the sampled pool capacity, promise the extents, and persist
    /// a fresh create intent under the provider state lock.
    ///
    /// Runs under the provider state lock: the in-flight reservation, the
    /// intent insertion and its save are atomic with respect to every other
    /// admission. `capacity_map` was sampled *without* that lock (and without
    /// a capacity mutex), which is what keeps the `vgs` CLI off both locks;
    /// `expected_epoch` is the allocation epoch the sample observed. If a
    /// create or grow completed an allocation in the meantime, this returns
    /// [`CreatePrepare::SampleStale`] and the caller re-samples. The
    /// reservation covers the released-lock window up to the `lvcreate`
    /// answer, so a concurrent grow (or create) cannot spend the same sampled
    /// free bytes. The intent save is durable-first: no LV exists until it
    /// succeeds. A failed save returns with the reservation dropped (no LV
    /// was created).
    fn prepare_create_intent<'a>(
        &'a self,
        state: &mut LvmState,
        req: &CreateVolumeRequest,
        payload: &str,
        capacity_map: &BTreeMap<String, VgCapacity>,
        expected_epoch: u64,
    ) -> Result<CreatePrepare<'a>, ApiError> {
        let picked = self.pick_pool_vg(state, req.size_bytes, capacity_map)?;
        let reservation =
            match self.try_reserve_capacity(&picked.vg_name, picked.sampled_free, picked.demand) {
                Ok(reservation) => reservation,
                Err((free, already)) => {
                    return Err(ApiError::new(
                        ApiErrorCode::NoSafeCapacity,
                        format!(
                            "no safe capacity: pool {} has {free} bytes free after {already} \
                             bytes already promised to in-flight allocations, and this create \
                             needs {} bytes (extent-rounded) plus headroom",
                            picked.vg_name, picked.demand
                        ),
                    ));
                }
            };
        // Reserve first, then check: a reservation inserted before an
        // allocation completes still covers it, so a bump observed here means
        // the sample may be stale in the optimistic direction and the caller
        // must re-read `vgs` before promising anything else.
        if self.capacity_epoch() != expected_epoch {
            drop(reservation);
            return Ok(CreatePrepare::SampleStale);
        }
        let intent = CreateIntent {
            vg_name: picked.vg_name.clone(),
            lv_name: lv_name_for(&req.volume_id),
            requested_size_bytes: req.size_bytes,
            project_id: req.project_id.clone(),
            block_size: req.logical_block_size.unwrap_or(DEFAULT_BLOCK_SIZE),
            creation_payload: payload.to_owned(),
        };
        state.insert_create_intent(req.volume_id.clone(), intent.clone());
        if let Err(save_error) = self.persist(state) {
            state.remove_create_intent(&req.volume_id);
            // Classify the failed save in three ways: the rename may have
            // landed (the post-rename carve-out), it may provably not have
            // landed, or the re-read may itself fail. Never report a clean
            // "not saved" (or drop the possibly-durable intent from memory)
            // when the durable file could not be read. No LV was created on
            // any path; startup observes no intent-owned LV and drops the
            // intent.
            let note = match LvmState::load(&self.state_path) {
                Ok(durable) if durable.create_intent(&req.volume_id).is_some() => format!(
                    "the durable state did record the create intent for volume {}, but no LV \
                     was created; startup recovery observes no intent-owned LV and drops it",
                    req.volume_id
                ),
                Ok(_) => format!(
                    "the create intent for volume {} could not be made durable, so no LV was \
                     created",
                    req.volume_id
                ),
                Err(load_error) => {
                    state.insert_create_intent(req.volume_id.clone(), intent.clone());
                    format!(
                        "the create intent for volume {} could not be verified after the \
                         failed save ({load_error}); treat it as possibly durable — no LV was \
                         created, and startup recovery observes no intent-owned LV and drops \
                         it",
                        req.volume_id
                    )
                }
            };
            return Err(ApiError::new(
                save_error.code,
                format!("{}; {note}", save_error.detail),
            ));
        }
        Ok(CreatePrepare::Prepared {
            intent,
            reservation,
        })
    }

    async fn attach_volume_inner(
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
        // Everything that touches state lives in this loop: the guard is
        // dropped before the durability wait, so the provider state lock is
        // never held across an `await`. Admission first resolves any
        // outstanding capture: a same-id attach replay must never answer
        // success from a mutation whose durability is still unproven (the
        // journal replays that success as terminal), and a failed covering
        // capture must re-align to the durable file and re-execute.
        let (response, reservation) = loop {
            let mut state = self.lock_state()?;
            match self.writer.admission_gate() {
                AdmissionGate::Ready => {}
                AdmissionGate::Wait(seq) => {
                    drop(state);
                    // The covering capture's failure is not this caller's
                    // failure: re-enter admission, which reloads the
                    // durable view and re-executes from it.
                    let _ = self.writer.wait_async(seq).await;
                    continue;
                }
                AdmissionGate::Align => continue,
            }
            // A durable pending delete owns this volume's fate: no new
            // attachment may be granted while it is outstanding.
            self.ensure_no_pending_delete(&state, volume_id)?;

            // Crash-replay idempotency: an attachment id that is already
            // recorded replays the recorded response (even if the volume
            // generation has since moved) and never fabricates a second
            // attachment; the same id with a different payload conflicts.
            // A replay is read-only, so it is answered even while a grow
            // is in flight (the grow leaves the volume generation
            // unchanged until it commits).
            let identity = AttachmentIdentity::from_request(volume_id, req, mode);
            for (recorded_volume, stored) in state.volumes() {
                if let Some(record) = stored.runtime.attachment.as_ref() {
                    if record.id == req.attachment_id {
                        resolve_attachment_replay(recorded_volume, record, &identity)?;
                        return Ok(attach_response(record, &stored.entry));
                    }
                }
            }

            // A fresh attach bumps the volume generation, which would void
            // the in-flight grow's recovery anchor: refuse it typed.
            Self::ensure_no_pending_grow(&state, volume_id)?;

            let response = {
                let stored = state
                    .volume_mut(volume_id)
                    .ok_or_else(|| not_found(volume_id))?;
                ensure_expected_generation(
                    req.expected_volume_generation,
                    stored.entry.generation,
                )?;
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

                // The attachment generation is the detach/report fence and is
                // strictly increasing per volume: it is the volume generation this
                // attach consumed, never a constant `1`. A delayed report or detach
                // from a retired instance therefore carries an older generation and
                // is refused, even if a consumer reuses the attachment id.
                let record = AttachmentRecord {
                    id: req.attachment_id.clone(),
                    vm_id: req.vm_id.clone(),
                    host_id: req.host_id.clone(),
                    generation: stored.entry.generation,
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
                attach_response(&record, &stored.entry)
            };
            // Capture and reserve under the state lock; the guard drops at
            // the end of this block, before the wait. A failure returns
            // typed and the next admission aligns (the leader failure path
            // never takes the provider state lock).
            let reservation = state.capture_and_reserve()?;
            break (response, reservation);
        };
        self.writer.commit_async(reservation).await?;
        Ok(response)
    }

    async fn detach_volume_inner(
        &self,
        volume_id: &VolumeId,
        attachment_id: &AttachmentId,
        req: &DetachVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError> {
        req.validate()?;
        // The guard is confined to this loop so it is dropped before the
        // durability wait. A not-found answer must not be derived from a
        // detach whose durability is still unproven: a failed covering
        // capture has to re-align and re-execute, or the journal would
        // record a terminal absence for an attachment that is still
        // durable.
        let (response, reservation) = loop {
            let mut state = self.lock_state()?;
            match self.writer.admission_gate() {
                AdmissionGate::Ready => {}
                AdmissionGate::Wait(seq) => {
                    drop(state);
                    let _ = self.writer.wait_async(seq).await;
                    continue;
                }
                AdmissionGate::Align => continue,
            }
            Self::ensure_no_pending_grow(&state, volume_id)?;
            let response = {
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

                // The retired id is not tombstoned: the strictly increasing
                // attachment generation is the fence, so a delayed report for this
                // instance is `STALE_GENERATION` against any later one. Bounding
                // per-id state is what keeps a churned volume usable (issue #63).
                stored.runtime.attachment = None;
                if stored.runtime.state == VolumeLifecycle::Attached {
                    stored.runtime.state = VolumeLifecycle::Ready;
                }
                stored.entry.generation += 1;
                stored.entry.data_epoch += 1;
                inspect_response(volume_id, stored)
            };
            let reservation = state.capture_and_reserve()?;
            break (response, reservation);
        };
        self.writer.commit_async(reservation).await?;
        Ok(response)
    }

    async fn report_attachment_state_inner(
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
        // The guard is confined to this loop so it is dropped before the
        // durability wait. A not-found or no-op answer must not be derived
        // from a covering capture that has not proven durable: on failure
        // the loop re-aligns and re-executes.
        let (response, reservation) = loop {
            let mut state = self.lock_state()?;
            match self.writer.admission_gate() {
                AdmissionGate::Ready => {}
                AdmissionGate::Wait(seq) => {
                    drop(state);
                    let _ = self.writer.wait_async(seq).await;
                    continue;
                }
                AdmissionGate::Align => continue,
            }
            // A report only advances the attachment record's state; it
            // never moves the volume generation or size, so it cannot
            // invalidate an in-flight grow's recovery anchor. It stays
            // admitted (idempotent no-op reports included).
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
            // Preserve the no-op gate: a report that did not advance the
            // recorded state performs no save (and reserves no sequence).
            let reservation = if response.applied {
                Some(state.capture_and_reserve()?)
            } else {
                None
            };
            break (response, reservation);
        };
        if let Some(reservation) = reservation {
            self.writer.commit_async(reservation).await?;
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

    /// Classify a failed durable grow-intent save and never misreport it.
    ///
    /// The rename may have landed (the intent is durable), may provably
    /// not have, or the re-read may itself fail. Never report a clean
    /// "not saved" (or drop a possibly-durable intent) when the durable
    /// file could not be read. No grow was attempted on any path, so
    /// startup observes the LV still at its from-size and drops the
    /// intent.
    fn grow_intent_save_failure(
        &self,
        state: &mut LvmState,
        volume_id: &VolumeId,
        grow_intent: GrowIntent,
        save_error: &ApiError,
    ) -> ApiError {
        state.remove_grow_intent(volume_id);
        let note = match LvmState::load(&self.state_path) {
            Ok(durable) if durable.grow_intent(volume_id).is_some() => format!(
                "the durable state did record the grow intent for volume {volume_id}, but no \
                 grow was attempted; startup recovery observes the LV at its from-size and \
                 drops the intent"
            ),
            Ok(_) => format!(
                "the grow intent for volume {volume_id} could not be made durable, so no grow \
                 was attempted"
            ),
            Err(load_error) => {
                state.insert_grow_intent(volume_id.clone(), grow_intent);
                format!(
                    "the grow intent for volume {volume_id} could not be verified after the \
                     failed save ({load_error}); treat it as possibly durable — no grow was \
                     attempted, and startup recovery observes the LV at its from-size and \
                     drops the intent"
                )
            }
        };
        ApiError::new(save_error.code, format!("{}; {note}", save_error.detail))
    }

    /// Validate one grow admission against the current state and derive
    /// the volume fields the drive needs.
    ///
    /// Runs under the provider state lock. A durable pending delete owns
    /// the volume's fate (growing would race the delete authorization); a
    /// move completion bumps the generation and a grow must not straddle
    /// it (the durable intent would look stale to recovery). Grow-only
    /// plus alignment is enforced fail-closed (Volume API v2 §4A). The
    /// caller re-runs this after reacquiring admission around the
    /// capacity sample, so a record that moved in that window is refused
    /// or re-derived rather than used stale.
    fn grow_admission(
        &self,
        state: &LvmState,
        volume_id: &VolumeId,
        req: &GrowVolumeRequest,
    ) -> Result<GrowAdmission, ApiError> {
        self.ensure_no_pending_delete(state, volume_id)?;
        Self::ensure_no_unsettled_move(state, volume_id)?;
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
        req.validate(stored.entry.size_bytes)?;
        Ok(GrowAdmission {
            vg_name: stored.entry.vg_name.clone(),
            lv_name: stored.entry.lv_name.clone(),
            current_size: stored.entry.size_bytes,
            current_generation: stored.entry.generation,
            has_attachment: stored.runtime.attachment.is_some(),
        })
    }

    /// Validate the sampled capacity, reserve it, and make the grow intent
    /// durable.
    ///
    /// Runs under the provider state lock: the in-flight reservation, the
    /// intent insertion and its save are atomic with respect to every
    /// other admission. `capacity` was sampled *without* that lock (and
    /// without a capacity mutex), which is what keeps the `vgs` CLI off
    /// both locks; `expected_epoch` is the allocation epoch the sample
    /// observed. If a create or another grow completed an allocation in
    /// the meantime, this returns [`GrowPrepare::SampleStale`] and the
    /// caller re-samples. The returned intent is the exact anchor this
    /// drive later re-validates before committing the observed size; the
    /// returned reservation covers the released-lock window up to the
    /// `lvextend` answer.
    fn prepare_grow(
        &self,
        state: &mut LvmState,
        volume_id: &VolumeId,
        req: &GrowVolumeRequest,
        vg_name: &str,
        capacity: VgCapacity,
        current_size: u64,
        current_generation: u64,
        intent_is_durable: bool,
        expected_epoch: u64,
    ) -> Result<GrowPrepare<'_>, ApiError> {
        let delta = req.new_size_bytes - current_size;
        // Thick LVM rounds the grown LV up to whole physical extents,
        // so the space the grow actually consumes is the
        // extent-rounded delta (the current size is already
        // extent-aligned). Comparing the raw delta would under-cover
        // the rounding and surface the over-allocation later as a raw
        // INTERNAL instead of the typed NO_SAFE_CAPACITY.
        let demand = extent_rounded(delta, capacity.extent_bytes);
        let reservation = match self.try_reserve_capacity(vg_name, capacity.free_bytes, demand) {
            Ok(reservation) => reservation,
            Err((free, reserved)) => {
                return Err(ApiError::new(
                    ApiErrorCode::NoSafeCapacity,
                    format!(
                        "grow needs {demand} more bytes (extent-rounded from {delta}), {free} free \
                         in {vg_name} ({reserved} reserved for in-flight allocations)"
                    ),
                ));
            }
        };
        // Reserve first, then check: a reservation inserted before an
        // allocation completes still covers it, so a bump observed here
        // means the sample may be stale in the optimistic direction and
        // the caller must re-read `vgs` before promising anything else.
        if self.capacity_epoch() != expected_epoch {
            drop(reservation);
            return Ok(GrowPrepare::SampleStale);
        }
        let grow_intent = match state.grow_intent(volume_id).cloned() {
            Some(intent) => intent,
            // A caller that verified the durable file holds this intent
            // must find it in memory too. Fabricating one and skipping
            // the save would run `lvextend` without a durable anchor,
            // reopening the crash window the durable-first rule closes.
            None if intent_is_durable => {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "volume {volume_id} was admitted as resuming a durable grow intent, \
                         but the in-memory state no longer holds it; refusing to extend \
                         without a durable anchor"
                    ),
                ));
            }
            None => GrowIntent {
                operation_id: req.operation_id.clone(),
                from_size_bytes: current_size,
                requested_size_bytes: req.new_size_bytes,
                expected_generation: current_generation,
            },
        };
        if !intent_is_durable {
            // Durable-first: persist the intent before `lvextend`, so a
            // crash between the CLI's success and the volume-record
            // commit rolls the grow forward on restart. The already
            // recorded intent keeps its original authorizing operation
            // when this call is the payload-identical resume.
            state.insert_grow_intent(volume_id.clone(), grow_intent.clone());
            if let Err(save_error) = self.persist(state) {
                return Err(self.grow_intent_save_failure(
                    state,
                    volume_id,
                    grow_intent,
                    &save_error,
                ));
            }
        }
        Ok(GrowPrepare::Prepared {
            grow_intent,
            reservation,
        })
    }

    /// Run `lvextend` without the provider state lock.
    ///
    /// This is the only long-running step of a grow (~200 ms of LVM CLI
    /// on the measured host). Holding the state lock across it made every
    /// other volume's admission queue behind one resize; the caller
    /// therefore drops the guard before this call and reacquires it only
    /// after the size has been verified.
    fn run_lvextend(&self, vg_name: &str, lv_name: &str, new_size_bytes: u64) -> LvextendOutcome {
        match self.runner.run(
            "lvextend",
            &[
                "--yes",
                "-L",
                &format!("{new_size_bytes}B"),
                &format!("{vg_name}/{lv_name}"),
            ],
        ) {
            Ok(output) if output.success => LvextendOutcome::Extended,
            Ok(output) => LvextendOutcome::StartFailed(command_failed("lvextend", &output)),
            Err(error) => LvextendOutcome::Unknown(ApiError::new(
                error.code,
                format!(
                    "{}; the lvextend outcome is unknown (the command runner returned an \
                     error rather than an exit status); the durable grow intent remains, \
                     and startup recovery will roll the grow forward only if the LV is at \
                     least the requested size, drop the intent if the LV is still at the \
                     from-size, and refuse startup on an ambiguous size",
                    error.detail
                ),
            )),
        }
    }

    /// Classify a non-zero `lvextend` exit after the state lock was released.
    ///
    /// Re-reads the state under a fresh admission and classifies the LV's
    /// observed size. A lock failure keeps the original command error
    /// visible and leaves the durable intent for recovery.
    fn settle_failed_lvextend_drive(
        &self,
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        from_size_bytes: u64,
        error: ApiError,
    ) -> ApiError {
        let mut state = match self.lock_state_stable() {
            Ok(state) => state,
            Err(lock_error) => {
                let code = lock_error.code;
                return ApiError::new(
                    code,
                    format!(
                        "{}; the grow intent could not be cleared because the provider \
                         state could not be re-read ({lock_error}); the durable intent \
                         remains, and a retry or startup will classify the observed size",
                        error.detail
                    ),
                );
            }
        };
        self.settle_failed_lvextend(
            &mut state,
            volume_id,
            vg_name,
            lv_name,
            from_size_bytes,
            error,
        )
    }

    /// Classify a non-zero `lvextend` exit against the observed world.
    ///
    /// The intent is cleared only when `lvs` proves the LV is still at its
    /// from-size. A non-zero exit that nevertheless left the LV grown (or
    /// whose size cannot be re-verified) keeps the durable intent: clearing
    /// it there would silently drop the recovery anchor and leave the
    /// recorded size below the real LV.
    fn settle_failed_lvextend(
        &self,
        state: &mut LvmState,
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        from_size_bytes: u64,
        error: ApiError,
    ) -> ApiError {
        match self.lv_size(vg_name, lv_name) {
            Ok(Some(observed)) if observed == from_size_bytes => {
                self.clear_grow_intent_after_failed_start(state, volume_id, error)
            }
            Ok(Some(observed)) => ApiError::new(
                error.code,
                format!(
                    "{}; the lvextend exit status was non-zero but LVM reports {observed} bytes \
                     for {vg_name}/{lv_name} (from {from_size_bytes}); the durable grow intent \
                     remains, and a retry or startup classifies the observed size rather than \
                     assuming the command did nothing",
                    error.detail
                ),
            ),
            Ok(None) => ApiError::new(
                error.code,
                format!(
                    "{}; lvs no longer reports {vg_name}/{lv_name} after the non-zero lvextend \
                     exit; the durable grow intent remains for recovery (it never recreates or \
                     shrinks the LV)",
                    error.detail
                ),
            ),
            Err(observe_error) => ApiError::new(
                error.code,
                format!(
                    "{}; the LV size could not be re-verified after the non-zero lvextend exit \
                     ({observe_error}); the durable grow intent remains for recovery",
                    error.detail
                ),
            ),
        }
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

    /// Commit the observed post-`lvextend` size under a fresh admission.
    ///
    /// The drive guard and the durable intent excluded every structural
    /// mutation of this volume while the state lock was released, so the
    /// anchor and generation must still be exactly what the drive
    /// recorded; refuse to commit against a changed anchor rather than
    /// fabricate a record the recovery path cannot explain.
    fn commit_grown_size(
        &self,
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        grow_intent: &GrowIntent,
        current_generation: u64,
        actual: u64,
    ) -> Result<(), ApiError> {
        let mut state = self.lock_state_stable()?;
        if state.grow_intent(volume_id) != Some(grow_intent) {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "the durable grow intent for volume {volume_id} changed while lvextend was \
                     in flight; refusing to commit a size against an unverified anchor. The LV \
                     has already been extended to {actual} bytes; the durable intent is left in \
                     place, but recovery refuses to apply it against a changed anchor, so the \
                     recorded provisioned size may under-report until an operator reconciles it"
                ),
            ));
        }
        // Check against the immutable view first so a refusal does not
        // mark the state guard dirty.
        let observed_generation = state
            .volume(volume_id)
            .ok_or_else(|| not_found(volume_id))?
            .entry
            .generation;
        if observed_generation != current_generation {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "volume {volume_id} generation moved from {current_generation} to \
                     {observed_generation} while lvextend was in flight; refusing to commit the \
                     observed size {actual} against a stale grow anchor. The LV has already been \
                     extended; the durable intent is left in place, but startup recovery drops an \
                     intent whose recorded generation moved, so the recorded provisioned size may \
                     under-report until an operator reconciles it"
                ),
            ));
        }
        let new_generation = {
            let stored = state
                .volume_mut(volume_id)
                .ok_or_else(|| not_found(volume_id))?;
            stored.entry.size_bytes = actual;
            stored.entry.generation += 1;
            stored.entry.generation
        };
        state.remove_grow_intent(volume_id);
        if let Err(save_error) = self.persist(&mut state) {
            return Err(self.persist_completed_grow(
                save_error,
                volume_id,
                vg_name,
                lv_name,
                actual,
                new_generation,
            ));
        }
        Ok(())
    }

    /// Admit one grow drive: sample capacity, reserve it, and make the
    /// recovery intent durable, with the `vgs` sample outside the provider
    /// state lock.
    ///
    /// The sample is also outside any capacity mutex: concurrent grows
    /// must not queue behind each other's `vgs` CLI call. A non-evidence
    /// diagnostic build with a capacity mutex (dirty tree, not a published
    /// bundle) showed the c128 collapse return when grows were enabled;
    /// the convoy is the suspected mechanism, and the published residual
    /// tail remains unattributed. Safety comes from the allocation epoch
    /// instead — a sample is used only if no create attempt or completed
    /// `lvextend` changed free space during it, and the reservation is
    /// inserted before the epoch is re-read so an allocation completing in
    /// the window is still accounted for.
    /// A record that moved between the two admissions is re-derived (or
    /// refused typed), never used stale. A concurrent attach or detach
    /// that lands in the sample window therefore wins: the grow is
    /// refused with `STALE_GENERATION` (its CAS token no longer matches)
    /// instead of making the attach wait behind the grow's CLI. That is
    /// contract-correct but a client-visible inversion from the
    /// parent shape, which held the state lock across admission.
    fn prepare_grow_admission(
        &self,
        volume_id: &VolumeId,
        req: &GrowVolumeRequest,
    ) -> Result<GrowAdmissionOutcome<'_>, ApiError> {
        // A stale sample costs another `vgs` call. Bound the optimistic
        // rounds so a sustained allocation churn cannot keep one grow
        // looping forever; the caller can retry the request. This is a
        // liveness guard, not a capacity answer.
        let mut attempts = 0usize;
        // The pending-intent check observes the LV with `lvs` and may
        // finalize a rolled-forward grow. It runs once per drive: while
        // this drive holds `grow_drives`, no other actor can insert or
        // remove this volume's grow intent, so re-entering admission for
        // the capacity revalidation does not need to repeat the world
        // observation. This check (and the failed-`lvextend` settlement)
        // still runs its `lvs` under the state lock — a recorded residual,
        // not a property this change fixes.
        let mut resume_checked = false;
        let mut intent_is_durable = false;
        loop {
            attempts += 1;
            if attempts > GROW_ADMISSION_ATTEMPTS {
                return Err(ApiError::new(
                    ApiErrorCode::Overloaded,
                    format!(
                        "capacity admission for volume {volume_id} could not obtain a stable \
                         free-space sample after {GROW_ADMISSION_ATTEMPTS} attempts; retry the \
                         grow with a new operation_id"
                    ),
                ));
            }
            // Read the admission record under the state lock, then release
            // it for the capacity sample.
            let first = {
                let state = self.lock_state_stable()?;
                self.grow_admission(&state, volume_id, req)?
            };
            let epoch = self.capacity_epoch();
            let capacity = self.vg_capacity(&first.vg_name)?;
            if self.capacity_epoch() != epoch {
                // A create or a completed grow allocated during the
                // sample; the free figure may already be stale.
                continue;
            }

            // Re-enter admission and re-validate against whatever happened
            // while the state lock was released for the CLI.
            let mut state = self.lock_state_stable()?;
            let current = self.grow_admission(&state, volume_id, req)?;
            if current.vg_name != first.vg_name {
                // The record moved to another pool; the sample describes
                // the wrong VG.
                drop(state);
                continue;
            }
            if !resume_checked {
                resume_checked = true;
                match self.resume_pending_grow(
                    &mut state,
                    volume_id,
                    req,
                    &current.vg_name,
                    &current.lv_name,
                    current.current_size,
                    current.current_generation,
                )? {
                    GrowResume::NoIntent => {}
                    GrowResume::Continue => {
                        // The in-memory intent may be *possibly* durable: a
                        // failed intent save whose re-read was unreadable
                        // reinserted it. Never promote that to an anchor by
                        // inference — running `lvextend` without a confirmed
                        // durable intent reopens the crash window the
                        // durable-first save exists to close. Re-save unless
                        // the durable file verifiably holds this exact intent;
                        // an unreadable re-read also takes the safe re-save
                        // path (and fails closed if that save fails).
                        intent_is_durable = state.grow_intent(volume_id).is_some_and(|memory| {
                            LvmState::load(&self.state_path)
                                .is_ok_and(|durable| durable.grow_intent(volume_id) == Some(memory))
                        });
                    }
                    GrowResume::Finalized(observed) => {
                        return Ok(GrowAdmissionOutcome::Finalized {
                            has_attachment: current.has_attachment,
                            effective_size_bytes: observed,
                        });
                    }
                }
            }
            match self.prepare_grow(
                &mut state,
                volume_id,
                req,
                &current.vg_name,
                capacity,
                current.current_size,
                current.current_generation,
                intent_is_durable,
                epoch,
            )? {
                GrowPrepare::SampleStale => {
                    // The reservation was released by `prepare_grow`; a
                    // create or completed grow got between the sample and
                    // the reservation. Re-sample.
                    drop(state);
                }
                GrowPrepare::Prepared {
                    grow_intent,
                    reservation,
                } => {
                    return Ok(GrowAdmissionOutcome::Prepared(PreparedGrow {
                        grow_intent,
                        reservation,
                        vg_name: current.vg_name,
                        lv_name: current.lv_name,
                        current_size: current.current_size,
                        current_generation: current.current_generation,
                        has_attachment: current.has_attachment,
                    }));
                }
            }
        }
    }

    /// Grow with a durable-first grow intent.
    ///
    /// Rejected while a delete is pending. A matching pending intent is
    /// a resume: at/above the request finalizes from the observed size,
    /// exactly at the from-size continues to `lvextend`, and anything
    /// else is ambiguous and refused. A fresh grow persists the intent
    /// before `lvextend`; a known failed start (proven by `lvs` to have
    /// left the LV at its from-size) clears it, while an unknown outcome
    /// leaves it durable. A failed size verification or a failed final
    /// save leaves the durable intent to roll the grow forward on
    /// restart, and the LV is never shrunk as compensation.
    fn grow_volume_inner(
        &self,
        volume_id: &VolumeId,
        req: &GrowVolumeRequest,
    ) -> Result<GrowVolumeResponse, ApiError> {
        validate_api_version(&req.api_version)?;
        if !self.capabilities().contains(Capability::Resize) {
            return Err(unsupported("the resize capability is not advertised"));
        }
        // The drive guard (one live grow per volume) is held by the async
        // wrapper; structural mutations are refused by the durable intent.
        let prepared = match self.prepare_grow_admission(volume_id, req)? {
            GrowAdmissionOutcome::Finalized {
                has_attachment,
                effective_size_bytes,
            } => return Ok(grow_response(has_attachment, effective_size_bytes)),
            GrowAdmissionOutcome::Prepared(prepared) => prepared,
        };
        let PreparedGrow {
            grow_intent,
            reservation,
            vg_name,
            lv_name,
            current_size,
            current_generation,
            has_attachment,
        } = prepared;
        // The resize and its verification are the only long-running steps.
        // The provider state guard was scoped to the revalidation loop and
        // is already released, so other volumes' writes are not queued
        // behind this volume's LVM CLI call. The durable intent (written
        // above) is the recovery anchor for the gap, and the capacity
        // reservation keeps concurrent admissions honest.
        let respond =
            |effective_size_bytes: u64| grow_response(has_attachment, effective_size_bytes);
        let lvextend = self.run_lvextend(&vg_name, &lv_name, req.new_size_bytes);
        // The command returned: `vgs` now reports the real free space, so
        // the in-flight promise can be released on every outcome. Bump the
        // allocation epoch *before* dropping the reservation: a grow that
        // sampled before this allocation must either see the bump and
        // re-sample, or have observed this reservation in its accounting.
        // Dropping first would let a stale sample reserve the freed bytes
        // twice.
        // Publish the completed allocation under the state lock. A create
        // takes that lock for its `vgs` sample and its `reserved_capacity`
        // read; a grow's completion otherwise needs no lock. Without this
        // ordering, a create could sample free space before this
        // allocation, then read the reservations after they were dropped,
        // and admit against space that no longer exists. Holding the lock
        // makes the sample and the reservation accounting atomic with
        // respect to completed allocations. It is held for an atomic bump
        // and a leaf-lock removal only, never across an LVM command.
        match self.lock_state_stable() {
            Ok(_state) => {
                self.note_capacity_change();
                drop(reservation);
            }
            Err(error) => {
                // `lock_state_stable` fails only on a poisoned state mutex
                // or an unreadable alignment load. The allocation has
                // already happened and `vgs` reports its real free space,
                // so the epoch must still be bumped and the reservation
                // released. Without the state guard this is the documented
                // degraded path: create admissions that hold the mutex
                // block rather than observe the drop, and a poisoned mutex
                // refuses every new admission. Log it rather than
                // swallowing the error, because the invariant "publish
                // under the state lock" did not hold.
                tracing::warn!(
                    volume_id = %volume_id,
                    error = %error,
                    "grow publish could not take the provider state lock; capacity epoch \
                     bumped and reservation released without the state guard"
                );
                self.note_capacity_change();
                drop(reservation);
            }
        }
        match lvextend {
            LvextendOutcome::Extended => {}
            LvextendOutcome::Unknown(error) => {
                // The command may have reached LVM: keep the durable
                // intent for recovery. It stays in the in-memory state
                // (the dropped guard published it), so structural
                // mutations remain refused until a retry or restart
                // classifies the observed size.
                return Err(error);
            }
            LvextendOutcome::StartFailed(error) => {
                // A non-zero exit is a failed start only when LVM proves
                // the LV did not move; otherwise the durable intent must
                // survive so recovery classifies the observed size.
                return Err(self.settle_failed_lvextend_drive(
                    volume_id,
                    &vg_name,
                    &lv_name,
                    current_size,
                    error,
                ));
            }
        }
        // Verify the effective size from LVM's own report. Like create,
        // grow rounds up to whole extents: anything at or above the
        // requested size is a success, and the effective size is what
        // gets persisted and reported. A verification failure leaves
        // the durable grow intent for recovery (never a silent success).
        let actual = self.verify_grown_size(&vg_name, &lv_name, req.new_size_bytes)?;
        // Reacquire the state lock only to commit the observed size.
        self.commit_grown_size(
            volume_id,
            &vg_name,
            &lv_name,
            &grow_intent,
            current_generation,
            actual,
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
        // A live grow releases the state lock across `lvextend`; deleting
        // under it would destroy the LV the resize is operating on and
        // leave a durable intent that can no longer be classified.
        Self::ensure_no_pending_grow(state, volume_id)?;
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
    /// `lvremove`, and the destructive body runs **without** the provider
    /// state lock (Phase E E1); the durable intent is the fence and the
    /// recovery anchor while it runs. The final save removes the volume and
    /// intent together; its failed save keeps the in-memory removal only
    /// when the durable file already records both removals, otherwise it
    /// restores the durable shape and reports that a retry/restart
    /// completes the delete. Foreign LVs are never touched.
    fn delete_volume_inner(
        &self,
        volume_id: &VolumeId,
        req: &DeleteVolumeRequest,
    ) -> Result<(), ApiError> {
        validate_api_version(&req.api_version)?;
        // One live structural drive per volume. The destructive body runs
        // without the provider state lock, so a second delete or create of
        // the same volume must be refused instead of racing
        // `blkdiscard`/`lvremove` against the same durable intent.
        let _drive = self.begin_structural_drive(volume_id, StructuralDriveKind::Delete)?;
        let (mut stored_before, _) = {
            let state = self.lock_state_stable()?;
            Self::validate_delete_request(&state, volume_id, req)?
        };
        let vg_name = stored_before.entry.vg_name.clone();
        let lv_name = stored_before.entry.lv_name.clone();

        // Observe the LV outside the state lock. `lvs` is an external CLI
        // call; holding the single provider mutex across it queued every
        // other volume's mutation behind this check. The structural drive
        // guard keeps this volume's own create/delete out; the full
        // revalidation below is the fence against a grow/move that started
        // in the observation window. Phase F F2a (#62).
        let lv_exists = self.lv_size(&vg_name, &lv_name)?.is_some();

        let mut state = self.lock_state_stable()?;
        let (fresh_before, pending_intent) = Self::validate_delete_request(&state, volume_id, req)?;
        // The observation ran without the lock. Fail closed if the volume
        // changed under it rather than authorizing a delete from a stale
        // snapshot; a same-id recreate bumps the generation and is caught
        // by `validate_delete_request` itself.
        if fresh_before.entry.generation != stored_before.entry.generation
            || fresh_before.entry.vg_name != vg_name
            || fresh_before.entry.lv_name != lv_name
        {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "volume {volume_id} changed while its LV existence was observed outside \
                     the state lock; refusing to authorize a delete from a stale snapshot"
                ),
            ));
        }
        stored_before = fresh_before;
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
            // Phase E E1 (#62): the intent is durable, so the destructive
            // body no longer needs the provider state lock. Dropping the
            // guard publishes the intent; the volume and its fence stay in
            // the committed state until the final removal, so readers and
            // structural operations see the same pending-delete truth they
            // saw before the lock was released.
            drop(state);
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
            // Reacquire only for the final atomic record removal. Other
            // volumes' mutations may have saved state while the command
            // ran; admission alignment resolves any in-flight capture.
            state = self.lock_state_stable()?;
            // Fail closed if the volume or its matching authorization
            // disappeared while the command was in flight. A different
            // intent must never be removed under this call's authority.
            let current = state.volume(volume_id).cloned().ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "volume {volume_id} disappeared from state while its authorized \
                         delete of {vg_name}/{lv_name} was in flight; the LV was removed, \
                         but the durable record cannot be cleared safely"
                    ),
                )
            })?;
            if current.entry.generation != intent.expected_generation
                || state.delete_intent(volume_id) != Some(&intent)
            {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "volume {volume_id} changed generation or authorization while its \
                         delete of {vg_name}/{lv_name} was in flight; refusing to remove a \
                         record under a mismatched delete intent"
                    ),
                ));
            }
            stored_before = current;
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
    /// `Ok(Some(_))` is the LV's row. `Ok(None)` is a **proven
    /// absence**: the `lvs` query succeeded and no row names the LV —
    /// a `pvmove` cannot be active on an LV that does not exist, so
    /// the caller may resolve the record terminally instead of
    /// spinning. `Err` is an unobservable world (`lvs` failed or its
    /// output could not be parsed) — the caller decides what an
    /// unknown means (the drive parks `IN_DOUBT`; the reconcile pass
    /// skips and retries).
    fn observe_lv(
        &self,
        vg_name: &str,
        lv_name: &str,
    ) -> Result<Option<MoveObservation>, ApiError> {
        let output = self.runner.run("lvs", move_lvs_args())?;
        if !output.success {
            return Err(command_failed("lvs", &output));
        }
        let rows: Vec<LvRow> = crate::report::parse_report(&output.stdout, "lv")?;
        let wanted = format!("{vg_name}/{lv_name}");
        Ok(rows
            .into_iter()
            .find(|row| row.vg_slash_lv().as_deref() == Some(wanted.as_str()))
            .map(|row| MoveObservation {
                moving: row.move_segment_active(),
                devices: row.device_pvs().into_iter().map(str::to_owned).collect(),
            }))
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
    /// Parking is applied to the in-memory view before the save. If the
    /// save fails, [`LvmProvider::reconcile_failed_save`] keeps the parked
    /// state only when the durable file proves the rename landed; when
    /// durability cannot be proven the in-memory park is reverted to the
    /// last committed snapshot. The caller receives the save error either
    /// way, never a success response. The **durable file remains the
    /// anchor** — the prior record when the rename did not land, or the
    /// already-landed `IN_DOUBT` when it did — and the daemon never
    /// memory-only-completes or memory-only-resolves anything; startup
    /// reconcile re-parks the record from the observed world. A persisted
    /// `IN_DOUBT` is likewise rolled forward only by a verified
    /// completion.
    ///
    /// **A terminal record is never parked over.** Every caller parks
    /// on a judgment made *before* this lock — a snapshot of the
    /// record, or a world observation that may itself have failed
    /// transiently — so the durable record is re-checked under the
    /// lock, the same stale-snapshot discipline as
    /// [`Self::finish_complete`]'s idempotency gate:
    ///
    /// - a stored `COMPLETE` means the completion already landed
    ///   durably (this caller's snapshot predates it, or a concurrent
    ///   finish path won the race — including the case where this
    ///   caller's own verification observe failed transiently); parking
    ///   over it would resurrect the resolved move to `IN_DOUBT`, and
    ///   the next reconcile pass would re-complete it and bump the
    ///   generation a second time;
    /// - a stored `FAILED` is the reconciliation's settled rollback
    ///   verdict (the extents verifiably sit on the source with no move
    ///   active). Parking over it would re-erect the delete fence and
    ///   destroy the resolution audit detail until the next pass
    ///   re-resolved it.
    ///
    /// Both are answered as-is.
    fn park_in_doubt(
        &self,
        volume_id: &VolumeId,
        detail: String,
    ) -> Result<MoveVolumeBackingResponse, ApiError> {
        let mut state = self.lock_state_stable()?;
        if let Some(stored) = state.move_record(volume_id) {
            if matches!(
                stored.state,
                MoveVolumeBackingState::Complete | MoveVolumeBackingState::Failed
            ) {
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
        self.persist(&mut state).inspect_err(|_| {
            self.reconcile_failed_save(&mut state);
        })?;
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
    /// completion is kept; if it records the prior state the previous
    /// generation and previous move record are restored in memory and
    /// the error says reconciliation will complete the move; if the
    /// re-read itself fails, the verified completion is kept as
    /// possibly durable (never rolled back, so a later save cannot
    /// regress the generation or erase the anchor).
    fn finish_complete(
        &self,
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        record: &crate::state::MoveRecord,
    ) -> Result<MoveVolumeBackingResponse, ApiError> {
        match self.complete_move_unless_growing(volume_id, vg_name, lv_name, record)? {
            MoveCompletion::Completed(response) => Ok(response),
            MoveCompletion::Deferred(error) => Err(error),
        }
    }

    /// The verified-completion body, answering `Deferred` (instead of an
    /// error) when a live grow owns the volume's generation anchor.
    ///
    /// Consumer-driven callers ([`Self::finish_complete`]) turn a
    /// deferral into the typed refusal; the background reconcile pass
    /// records it and keeps classifying the remaining records instead
    /// of starving them behind one volume's grow.
    #[allow(clippy::too_many_lines)] // one verified-completion/persist-classification narrative
    fn complete_move_unless_growing(
        &self,
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        record: &crate::state::MoveRecord,
    ) -> Result<MoveCompletion, ApiError> {
        let verification = match self.observe_lv(vg_name, lv_name) {
            Ok(Some(verification)) => verification,
            // The LV is verifiably absent (a successful `lvs` with no
            // row): not an unobservable world. The move can never
            // complete and no pvmove can be active, so resolve the
            // record terminally instead of parking it forever.
            Ok(None) => {
                return self
                    .resolve_absent_move(volume_id, vg_name, lv_name, record)
                    .map(MoveCompletion::Completed);
            }
            // The completion was observed but cannot be verified:
            // unknown, never a silent success.
            Err(e) => {
                return self
                    .park_in_doubt(
                        volume_id,
                        format!("verification could not observe the LV after the move ended: {e}"),
                    )
                    .map(MoveCompletion::Completed);
            }
        };
        if !verification.relocated_to(&record.source_pv, &record.target_pv) {
            return self
                .park_in_doubt(
                    volume_id,
                    format!(
                        "verification refused: the LV's devices do not verify the relocation \
                         to {} (observed {:?}); the source PV {} must no longer back the LV",
                        record.target_pv, verification.devices, record.source_pv
                    ),
                )
                .map(MoveCompletion::Completed);
        }
        let mut state = self.lock_state_stable()?;
        // The completion snapshot may be stale: this caller verified the
        // world without the lock, and a delete (or a newer move) may
        // have removed/replaced the record in between. Never complete a
        // volume whose move record is gone or belongs to a different
        // operation — that would bump a recreated volume's generation
        // and write a COMPLETE record it never earned.
        let Some(stored_record) = state.move_record(volume_id).cloned() else {
            return Err(not_found(volume_id));
        };
        // Compare the placement pair, not the operation id: the
        // COPYING save refreshes the stored operation id to the
        // attaching request's id, so the drive's PREPARING snapshot is
        // expected to differ there. A different source→target pair
        // means this caller's record was superseded.
        if stored_record.source_pv != record.source_pv
            || stored_record.target_pv != record.target_pv
        {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "the move record for volume {volume_id} was superseded while the \
                     relocation was being verified; refusing to complete from a stale snapshot"
                ),
            ));
        }
        // The idempotency gate: this caller verified the world, but
        // the completion may have landed durably between that
        // observation and this lock. A stored COMPLETE record for
        // the same source→target is the already-once completion —
        // answer it without a second bump. A stored COMPLETE record
        // for a *different* pair means this caller's record was
        // superseded by a newer move's completion; the stored record
        // is the durable truth, answered as-is — never clobbered,
        // never re-bumped.
        if stored_record.state == MoveVolumeBackingState::Complete {
            return Ok(MoveCompletion::Completed(Self::move_response(
                &state,
                volume_id,
                &stored_record,
            )));
        }
        // A live grow holds the volume's durable recovery anchor at the
        // current generation. A completion bumps that generation without
        // touching the intent, so a crash before the grow commits would
        // make the anchor look stale and recovery would drop it while the
        // LV is already extended. Defer the completion: the next reconcile
        // tick re-observes the world after the grow commits or clears its
        // intent. (Unsettled records cannot coexist with a live grow —
        // grow admission refuses them — so this can only defer a settled,
        // e.g. previously FAILED, record whose world moved again.)
        if let Some(error) = Self::pending_grow_error(&state, volume_id) {
            return Ok(MoveCompletion::Deferred(error));
        }
        // A stored `FAILED` is deliberately **not** a barrier on this
        // path: unlike `park_in_doubt`, this caller holds a fresh world
        // observation that proves the relocation. A `FAILED` verdict came
        // from an earlier observation that proved the extents back on the
        // source; contradictory observations mean the world moved again
        // between them. The verified relocation is the authority here, so
        // completing is a roll-forward, not a resurrection of an
        // unresolved move.
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
            match LvmState::load(&self.state_path) {
                Ok(durable) => {
                    let committed = durable
                        .move_record(volume_id)
                        .is_some_and(|stored| stored.state == MoveVolumeBackingState::Complete)
                        && durable
                            .volume(volume_id)
                            .is_some_and(|stored| stored.entry.generation == new_generation);
                    if committed {
                        return Err(ApiError::new(
                            save_error.code,
                            format!(
                                "{}; the durable state already records the completed move and \
                                 volume generation {new_generation}, so the in-memory completion \
                                 was kept",
                                save_error.detail
                            ),
                        ));
                    }
                    // The durable file still says the prior state:
                    // restore it in memory (never leave a completion
                    // against a prior durable record) and let
                    // reconciliation complete the move.
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
                            "{}; the durable move record still says the prior state \
                             (generation {previous_generation}), so reconciliation will complete \
                             the move",
                            save_error.detail
                        ),
                    ));
                }
                Err(load_error) => {
                    // Unknown: the completion's rename may have landed.
                    // Keep the verified completion and the bumped
                    // generation in memory (never roll back against a
                    // possibly-durable file) so a later save cannot
                    // erase the COMPLETE record or regress the
                    // generation; report it honestly.
                    return Err(ApiError::new(
                        save_error.code,
                        format!(
                            "{}; the completion's durable record could not be re-read \
                             ({load_error}), so treat the completed move and volume generation \
                             {new_generation} as possibly durable — do not complete it again; \
                             a retry or restart verifies the world before acting",
                            save_error.detail
                        ),
                    ));
                }
            }
        }
        response
            .map(MoveCompletion::Completed)
            .ok_or_else(|| not_found(volume_id))
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
        let state = self.state_snapshot();
        self.ensure_no_pending_delete(&state, volume_id)?;
        // Snapshot fence only: the authoritative re-check runs under the
        // writer lock in `journal_move_intent` before the record is
        // inserted, so this early answer cannot race a grow that persists
        // its intent in the insert→publish gap.
        Self::ensure_no_pending_grow(&state, volume_id)?;
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
                existing_record: None,
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
                        existing_record: None,
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
                        existing_record: Some(record.clone()),
                    })
                }
                MoveVolumeBackingState::InDoubt => Err(Self::in_doubt_move_error(record)),
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
                        existing_record: None,
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
                    Err(Self::foreign_move_error(record))
                }
            },
        }
    }

    /// The typed refusal for a parked (`IN_DOUBT`) move record.
    ///
    /// Used by both admission and the under-lock journal re-check so a
    /// record parked between admission's published-snapshot read and the
    /// writer lock cannot be silently superseded by a fresh drive.
    fn in_doubt_move_error(record: &crate::state::MoveRecord) -> ApiError {
        ApiError::new(
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
        )
    }

    /// The typed refusal for a mirror-path move record foreign to this
    /// same-VG provider (reachable only through hand-edited/imported
    /// state).
    fn foreign_move_error(record: &crate::state::MoveRecord) -> ApiError {
        ApiError::new(
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
        )
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
            let state = self.state_snapshot();
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
        // is refused typed instead. This is only the pre-CLI check;
        // `journal_move_intent` re-runs it under the writer lock so a
        // concurrent admission cannot slip in while this request's
        // CLI observations are in flight.
        {
            let state = self.state_snapshot();
            ensure_no_other_active_move_on_source(&state, volume_id, &source)?;
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
    /// A re-attach to an existing `Copying` record saves nothing (an
    /// unchanged state has no durable boundary to write; the
    /// operation-id refresh lands with the COPYING save) and must still
    /// target the same PV. A re-attach to a `Preparing` record first
    /// ensures the anchor is durable (normally it already is after a
    /// crash/retry; a memory-only anchor from a failed save whose
    /// re-read was unknown is persisted now) — no `pvmove` may start
    /// without a durable anchor. A new record requires the verified
    /// source from admission and must be persisted before `pvmove`
    /// starts: if that save fails the in-memory record is removed and
    /// the caller starts nothing (no durable anchor would mean a blind
    /// restart), except when the save's rename may have landed — then
    /// the in-memory anchor is kept as possibly durable and the caller
    /// still starts nothing.
    fn journal_move_intent(
        &self,
        volume_id: &VolumeId,
        req: &MoveVolumeBackingRequest,
        fallback_source: Option<&str>,
    ) -> Result<(String, crate::state::MoveRecord), ApiError> {
        let mut state = self.lock_state_stable()?;
        // Re-validate under the journal lock: admission ran before the
        // CLI observations (target/source/foreign-pvmove queries), and
        // a delete of this volume may have completed in that window.
        // Inserting an anchor for a deleted volume would let a later retry
        // redrive pvmove on a volume that never authorized this move. A
        // delete-and-recreate is caught only when the recreated volume has
        // a different generation; a same-id/same-generation ABA recreate is
        // not distinguishable with the current fence (a per-incarnation
        // nonce would be needed).
        let Some(stored) = state.volume(volume_id) else {
            return Err(not_found(volume_id));
        };
        if stored.entry.generation != req.expected_generation {
            return Err(ApiError::stale_generation(
                req.expected_generation,
                stored.entry.generation,
            ));
        }
        if state.delete_intent(volume_id).is_some() {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "volume {volume_id} has a durable pending delete; no move is admitted \
                     until that delete is completed or resolved"
                ),
            ));
        }
        Self::ensure_no_pending_grow(&state, volume_id)?;
        let mut inserted = false;
        let (source, record) = match state.move_record(volume_id) {
            Some(record)
                if matches!(
                    record.state,
                    MoveVolumeBackingState::Preparing | MoveVolumeBackingState::Copying
                ) =>
            {
                // The record may have been replaced between admission
                // and this lock. Only the target this caller asked for
                // is a legitimate re-attach; a different target is a
                // newer/different move, never adopted silently. A
                // record with no source cannot drive or classify a
                // pvmove: refuse it rather than pass an empty PV.
                if record.source_pv.is_empty() {
                    return Err(ApiError::new(
                        ApiErrorCode::InvalidState,
                        format!(
                            "the move record for volume {volume_id} has no recorded source PV; \
                             refusing to drive a move that cannot be classified"
                        ),
                    ));
                }
                if record.target_pv != req.target_pool_id {
                    return Err(ApiError::new(
                        ApiErrorCode::InvalidState,
                        format!(
                            "the move record for volume {volume_id} now targets {} rather \
                             than {}; refusing to attach to a different move",
                            record.target_pv, req.target_pool_id
                        ),
                    ));
                }
                (record.source_pv.clone(), record.clone())
            }
            _ => {
                // Admission read a published snapshot before this
                // request's lock-free CLI observations. If the record was
                // parked, completed, or replaced by a foreign record in
                // that window, the `_` arm must not silently supersede it.
                // Re-check under the writer lock, exactly as admission
                // would.
                Self::refuse_superseded_fresh_drive(&state, volume_id, req)?;
                // A fresh drive always carries a source verified from
                // the world. Admission may have decided to re-attach and
                // the record then vanished/settled under a concurrent
                // operation; a source-less PREPARING record could never
                // re-drive or classify a pvmove, so refuse instead.
                let Some(source) = fallback_source else {
                    return Err(ApiError::new(
                        ApiErrorCode::InvalidState,
                        format!(
                            "the move record for volume {volume_id} changed while the move \
                             was being admitted (no active record remains); refusing to journal \
                             a move without a verified source — retry the move"
                        ),
                    ));
                };
                let source = source.to_owned();
                // Re-check the one-move-per-source rule under the writer
                // lock. Admission's check reads a published snapshot taken
                // before this request's CLI observations; a concurrent move
                // that journaled its anchor in that window would otherwise
                // let two pvmoves start on one source PV, and LVM silently
                // attaches the second to the first and ignores its
                // arguments.
                ensure_no_other_active_move_on_source(&state, volume_id, &source)?;
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
                return Err(self.classify_preparing_save_failure(
                    &mut state,
                    volume_id,
                    &record,
                    &save_error,
                ));
            }
        } else if record.state == MoveVolumeBackingState::Preparing {
            self.ensure_preparing_anchor_durable(&mut state, volume_id, &record)?;
        }
        Ok((source, record))
    }

    /// Refuse a fresh drive whose live move record changed after admission
    /// into a state a fresh `PREPARING` record must not supersede.
    ///
    /// Admission reads a published snapshot before the caller's lock-free
    /// CLI observations, so the durable record can move underneath it:
    ///
    /// - an `IN_DOUBT` park waits for reconciliation or an operator;
    /// - `MIRROR_READY`/`PIVOTED` are foreign to this same-VG journal;
    /// - the exact requested target may have completed (admission would
    ///   answer idempotently; superseding it would run a no-op pvmove,
    ///   bump the generation again, and transiently re-erect the delete
    ///   fence).
    ///
    /// `Failed` (a settled rollback) and a different-target `Complete` (a
    /// new move) remain valid fresh drives.
    ///
    /// In a live race the same-target completion also bumps the volume
    /// generation in the same save, so the generation fence normally
    /// refuses the stale request first. This branch still matters for
    /// imported/hand-edited state where a terminal record and an unbumped
    /// generation coexist, and it mirrors admission's idempotent answer.
    fn refuse_superseded_fresh_drive(
        state: &LvmState,
        volume_id: &VolumeId,
        req: &MoveVolumeBackingRequest,
    ) -> Result<(), ApiError> {
        let Some(stored) = state.move_record(volume_id) else {
            return Ok(());
        };
        match stored.state {
            MoveVolumeBackingState::InDoubt => Err(Self::in_doubt_move_error(stored)),
            MoveVolumeBackingState::MirrorReady | MoveVolumeBackingState::Pivoted => {
                Err(Self::foreign_move_error(stored))
            }
            MoveVolumeBackingState::Complete if stored.target_pv == req.target_pool_id => {
                Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "the move for volume {volume_id} to {} completed while this request \
                         was being admitted; retry to observe the completed record",
                        stored.target_pv
                    ),
                ))
            }
            // The call site's outer match consumes active records before
            // this helper; refuse explicitly rather than relying on that
            // caller contract, so a future reuse cannot silently supersede
            // a live move.
            MoveVolumeBackingState::Preparing | MoveVolumeBackingState::Copying => {
                Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "the move record for volume {volume_id} is active ({}); re-attach to it \
                     rather than journaling a fresh drive",
                        if stored.state == MoveVolumeBackingState::Preparing {
                            "PREPARING"
                        } else {
                            "COPYING"
                        }
                    ),
                ))
            }
            MoveVolumeBackingState::Complete | MoveVolumeBackingState::Failed => Ok(()),
        }
    }

    /// Ensure a re-attached `Preparing` record has a **durable** anchor
    /// before `pvmove` starts.
    ///
    /// Normally the crash/retry anchor already is durable; persist only
    /// when the re-read proves it is not (a memory-only anchor from a
    /// failed save whose re-read was unknown, or an unreadable file).
    /// Never start on an unknown anchor: a crash between the start and
    /// the COPYING save would lose the only record of the move.
    fn ensure_preparing_anchor_durable(
        &self,
        state: &mut LvmState,
        volume_id: &VolumeId,
        record: &crate::state::MoveRecord,
    ) -> Result<(), ApiError> {
        let anchor_durable = LvmState::load(&self.state_path).is_ok_and(|durable| {
            durable.move_record(volume_id).is_some_and(|stored| {
                stored.state == MoveVolumeBackingState::Preparing
                    && stored.source_pv == record.source_pv
                    && stored.target_pv == record.target_pv
            })
        });
        if anchor_durable {
            return Ok(());
        }
        if let Err(save_error) = self.persist(state) {
            return Err(self.classify_preparing_save_failure(
                state,
                volume_id,
                record,
                &save_error,
            ));
        }
        Ok(())
    }

    /// Classify a failed `PREPARING`-anchor save after the record was
    /// inserted in memory.
    ///
    /// If the save's rename landed, the durable file already holds this
    /// PREPARING record: the in-memory anchor is kept and the caller
    /// still starts no `pvmove`. If the re-read proves no durable anchor,
    /// the in-memory record is removed (no anchor means no `pvmove`). If
    /// the re-read itself fails, the anchor may be durable: keep it in
    /// memory so a later save cannot erase it, and still start nothing.
    fn classify_preparing_save_failure(
        &self,
        state: &mut LvmState,
        volume_id: &VolumeId,
        record: &crate::state::MoveRecord,
        save_error: &ApiError,
    ) -> ApiError {
        match LvmState::load(&self.state_path) {
            Ok(durable) => {
                let anchor_durable = durable.move_record(volume_id).is_some_and(|stored| {
                    stored.state == MoveVolumeBackingState::Preparing
                        && stored.source_pv == record.source_pv
                        && stored.target_pv == record.target_pv
                });
                if anchor_durable {
                    return ApiError::new(
                        save_error.code,
                        format!(
                            "the move's PREPARING record reported a save error but reached the \
                             durable state (its rename landed); no pvmove was started. The \
                             durable anchor remains for a retry or startup reconciliation to \
                             classify — do not assume the move failed: {}",
                            save_error.detail
                        ),
                    );
                }
                state.remove_move(volume_id);
                ApiError::new(
                    save_error.code,
                    format!(
                        "the move's PREPARING record could not be saved, so no pvmove was \
                         started: {}",
                        save_error.detail
                    ),
                )
            }
            Err(load_error) => {
                // Unknown: the anchor may be durable. Keep the in-memory
                // record so a later save cannot erase a possibly-durable
                // PREPARING anchor; no pvmove was started either way.
                ApiError::new(
                    save_error.code,
                    format!(
                        "the move's PREPARING record reported a save error, but the durable \
                         record could not be re-read ({load_error}); treat it as possibly \
                         durable and start no pvmove — a retry or restart classifies it: {}",
                        save_error.detail
                    ),
                )
            }
        }
    }

    /// Drive one online same-VG extent move.
    ///
    /// The `PREPARING` record is the sole reconciliation anchor: its
    /// save must land before `pvmove` starts (a failed save starts
    /// nothing). A failed `COPYING` save restores the prior record in
    /// memory unless the rename landed or the durable file cannot be
    /// re-read, in which case the just-saved `Copying` record is kept as
    /// possibly durable. A failed `COMPLETE` save restores the prior
    /// record and generation when the re-read proves the prior state, but
    /// keeps the verified completion when the rename landed or the
    /// re-read fails (never regressing the generation). In every case
    /// startup/reconcile classifies the durable record against the
    /// observed world — there is no blind restart.
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
        let (vg_name, lv_name, existing_target, existing_record) =
            match self.admit_move(volume_id, req)? {
                MoveAdmission::Answered(response) => return Ok(response),
                MoveAdmission::Drive {
                    vg_name,
                    lv_name,
                    existing_target,
                    existing_record,
                } => (vg_name, lv_name, existing_target, existing_record),
            };

        // Phase 2 — observe the world (no lock held): the target
        // qualification first (the request's own named reference is
        // validated against the observed world), then the source
        // derivation.
        let observation = self.observe_lv(&vg_name, &lv_name)?.ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "LV {vg_name}/{lv_name} is verifiably absent from a successful lvs \
                     query; refusing to move a volume whose LV does not exist (restart \
                     marks the volume Failed; its data is gone)"
                ),
            )
        })?;
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
        // source PV before a fresh pvmove starts. A re-attach to a
        // running (`Copying`) journaled move must skip the scan: its
        // own hidden mirror row would be mistaken for a foreign move.
        // A re-attach to a `Preparing` record is different — the
        // pvmove verifiably never started when the LV is not observed
        // moving, so the scan is both safe and required (otherwise an
        // out-of-band move on the source would swallow the start).
        if let Some(source_pv) = source_pv.as_deref() {
            self.guard_fresh_move_source(&vg_name, source_pv)?;
        } else if let Some(existing) = existing_record.as_ref() {
            if existing.state == MoveVolumeBackingState::Preparing
                && !observation.moving
                && !existing.source_pv.is_empty()
            {
                self.guard_fresh_move_source(&vg_name, &existing.source_pv)?;
            }
        }

        // Phase 3 — journal the intent. The record is the
        // reconciliation anchor: after this save, a daemon death
        // anywhere below leaves a durable fact to classify against.
        let (source_pv, record_for_drive) =
            self.journal_move_intent(volume_id, req, source_pv.as_deref())?;
        // The scan above validated the admission snapshot's source. The
        // journal re-read the record under the lock and may have driven
        // a different source (a concurrent re-drive replaced the
        // record); a `Preparing` record that is still unstarted must have
        // its actual source scanned too. A safe refusal here is fine —
        // never start over an unscanned source.
        if record_for_drive.state == MoveVolumeBackingState::Preparing
            && existing_record
                .as_ref()
                .is_some_and(|existing| existing.source_pv != source_pv)
        {
            self.guard_fresh_move_source(&vg_name, &source_pv)?;
        }

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
            self.persist_copying_record(volume_id, req, &record_for_drive)?;
        }

        // Phase 5 — supervise within the window: poll the world,
        // complete on a verified relocation, park `IN_DOUBT` on
        // anything unknown.
        self.supervise_move(volume_id, &vg_name, &lv_name, &source_pv, &record_for_drive)
            .await
    }

    /// Whether a stored move record may still be rolled to `COPYING`.
    ///
    /// `COMPLETE` and `FAILED` are terminal facts about this placement:
    /// regressing either would let the next reconcile pass re-classify
    /// the move, re-complete it and bump the volume generation a second
    /// time (or resurrect a resolved failure). `IN_DOUBT` is explicitly
    /// not terminal — it rolls forward when the world proves completion.
    fn move_record_may_regress_to_copying(record: &crate::state::MoveRecord) -> bool {
        !matches!(
            record.state,
            MoveVolumeBackingState::Complete | MoveVolumeBackingState::Failed
        )
    }

    /// Durably mark a live move `COPYING` only when the stored record
    /// still matches this pass's snapshot placement and is still
    /// rollable. Returns whether the stored record is the current live
    /// move (whether or not a save was needed).
    ///
    /// The caller's `lvs` observation happened outside the state lock,
    /// so a concurrent supervisor may have completed the move in
    /// between. Writing `COPYING` unconditionally would overwrite the
    /// landed `COMPLETE`, and the next pass would complete the move
    /// again and bump the volume generation a second time. A record
    /// that already says `COPYING` needs no rewrite.
    fn mark_move_copying_if_current(
        &self,
        volume_id: &VolumeId,
        snapshot: &crate::state::MoveRecord,
    ) -> Result<bool, ApiError> {
        let mut state = self.lock_state_stable()?;
        let Some(stored) = state.move_record(volume_id).cloned() else {
            return Ok(false);
        };
        if stored.source_pv != snapshot.source_pv || stored.target_pv != snapshot.target_pv {
            return Ok(false);
        }
        if !Self::move_record_may_regress_to_copying(&stored) {
            return Ok(false);
        }
        // Already exactly the live classification: the durable anchor is
        // correct, so do not rewrite it. Besides avoiding write
        // amplification, an unnecessary save per pass would consume a
        // crash-injection point intended for the completing save.
        if stored.state == MoveVolumeBackingState::Copying {
            return Ok(true);
        }
        if let Some(stored) = state.move_record_mut(volume_id) {
            stored.state = MoveVolumeBackingState::Copying;
            stored.detail = None;
        }
        self.persist(&mut state).inspect_err(|_| {
            self.reconcile_failed_save(&mut state);
        })?;
        Ok(true)
    }

    /// Persist the move's `COPYING` record after `pvmove` started, and
    /// classify a failed save against the durable file.
    ///
    /// The pvmove is already running. The prior durable record
    /// (`PREPARING`, or the re-attached `COPYING`) is the anchor — except
    /// when this save's rename landed, or the durable file cannot be
    /// re-read, in which case the `COPYING` record may be the durable one
    /// and memory must keep it (never roll back against a durable file
    /// that may have moved on).
    fn persist_copying_record(
        &self,
        volume_id: &VolumeId,
        req: &MoveVolumeBackingRequest,
        record_for_drive: &crate::state::MoveRecord,
    ) -> Result<(), ApiError> {
        let mut state = self.lock_state_stable()?;
        // The record may have moved on between the pvmove start and this
        // lock: a concurrent supervisor can complete the same move, or a
        // delete can drop it. Never regress a terminal record to
        // `COPYING`; answer the already-completed move instead, and
        // refuse to clobber a superseded placement.
        let Some(stored) = state.move_record(volume_id).cloned() else {
            return Err(not_found(volume_id));
        };
        if stored.source_pv != record_for_drive.source_pv
            || stored.target_pv != record_for_drive.target_pv
        {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "the move record for volume {volume_id} was superseded while the pvmove \
                     was starting; refusing to overwrite it with this drive's COPYING record"
                ),
            ));
        }
        if !Self::move_record_may_regress_to_copying(&stored) {
            if stored.state == MoveVolumeBackingState::Complete {
                // A concurrent supervisor already completed this exact
                // move; the supervision below re-verifies the world and
                // answers the stored record without a second bump.
                return Ok(());
            }
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "the move record for volume {volume_id} already resolved as {:?} while \
                     the pvmove was starting; refusing to regress it to COPYING",
                    stored.state
                ),
            ));
        }
        if let Some(record) = state.move_record_mut(volume_id) {
            record.state = MoveVolumeBackingState::Copying;
            record.operation_id = req.operation_id.clone();
            record.detail = None;
        }
        let Err(save_error) = self.persist(&mut state) else {
            return Ok(());
        };
        match LvmState::load(&self.state_path) {
            Ok(durable) => {
                let copying_durable = durable
                    .move_record(volume_id)
                    .is_some_and(|record| record.state == MoveVolumeBackingState::Copying);
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
                Err(ApiError::new(
                    save_error.code,
                    format!(
                        "the pvmove was started but its COPYING record could not be saved: \
                         {}; the durable prior record is the anchor and startup/reconcile \
                         classifies the world — do not assume the move failed",
                        save_error.detail
                    ),
                ))
            }
            Err(load_error) => {
                // Unknown: the COPYING rename may have landed. Keep the
                // in-memory COPYING record (rather than restoring the
                // prior one) so a later save cannot erase a
                // possibly-durable anchor, and say so.
                Err(ApiError::new(
                    save_error.code,
                    format!(
                        "the pvmove was started and its COPYING record reported a save error, \
                         but the durable record could not be re-read ({load_error}); treat \
                         the COPYING anchor as possibly durable — a retry or restart \
                         classifies the world and must not start a blind second pvmove: {}",
                        save_error.detail
                    ),
                ))
            }
        }
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
    /// - the LV verifiably absent: no `pvmove` can be active, so the
    ///   record resolves `FAILED` terminally (never a permanent
    ///   `IN_DOUBT` fence);
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
        let Some(observation) = self.observe_lv(vg_name, lv_name)? else {
            return self
                .resolve_absent_move(volume_id, vg_name, lv_name, record)
                .map(Some);
        };
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
        let mut state = self.lock_state_stable()?;
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
    /// - the LV is verifiably absent → the record resolves `FAILED`
    ///   terminally (no `pvmove` can be active on a missing LV);
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
                Ok(Some(observation)) => observation,
                Ok(None) => {
                    return self.resolve_absent_move(volume_id, vg_name, lv_name, record);
                }
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
                    //
                    // Re-check the live record under the lock: another
                    // actor may have landed `COMPLETE`/`FAILED`/parked
                    // the move between this pass's observation and the
                    // lock. Stamping the window detail onto a terminal
                    // record would misreport a resolved move.
                    let mut state = self.lock_state_stable()?;
                    let still_copying = state.move_record(volume_id).is_some_and(|stored| {
                        stored.state == MoveVolumeBackingState::Copying
                            && stored.source_pv == record.source_pv
                            && stored.target_pv == record.target_pv
                    });
                    if !still_copying {
                        return state
                            .move_record(volume_id)
                            .map(|stored| Self::move_response(&state, volume_id, stored))
                            .ok_or_else(|| not_found(volume_id));
                    }
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
                    self.persist(&mut state).inspect_err(|_| {
                        self.reconcile_failed_save(&mut state);
                    })?;
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

    /// Resolve a move's LV names, or drop the record when its volume is
    /// gone (deleted under the move): nothing is left to move.
    fn move_names_or_drop(
        &self,
        volume_id: &VolumeId,
        report: &mut MoveReconcileReport,
    ) -> Result<Option<(String, String)>, ApiError> {
        let names = {
            let state = self.lock_state_stable()?;
            state
                .volume(volume_id)
                .map(|stored| (stored.entry.vg_name.clone(), stored.entry.lv_name.clone()))
        };
        if names.is_some() {
            return Ok(names);
        }
        let mut state = self.lock_state_stable()?;
        state.remove_move(volume_id);
        self.persist(&mut state).inspect_err(|_| {
            self.reconcile_failed_save(&mut state);
        })?;
        self.clear_move_observe_failures(volume_id);
        report.dropped.push(volume_id.clone());
        Ok(None)
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
            let state = self.lock_state_stable()?;
            state
                .moves()
                .iter()
                .map(|(id, record)| (id.clone(), record.clone()))
                .collect()
        };
        for (volume_id, record) in snapshot {
            let Some((vg_name, lv_name)) = self.move_names_or_drop(&volume_id, &mut report)? else {
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
                Ok(Some(observation)) => {
                    self.clear_move_observe_failures(&volume_id);
                    observation
                }
                Ok(None) => {
                    // A successful `lvs` with no row for the LV is a
                    // proven absence, not an unobservable world: no
                    // pvmove can be active, the move can never
                    // complete, and parking would fence delete
                    // forever. Resolve the record terminally.
                    let resolved =
                        self.resolve_absent_move(&volume_id, &vg_name, &lv_name, &record)?;
                    if resolved.state == MoveVolumeBackingState::Failed
                        && record.state != MoveVolumeBackingState::Failed
                    {
                        report.resolved_absent.push(volume_id.clone());
                    }
                    self.clear_move_observe_failures(&volume_id);
                    continue;
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
                // so durably. The observation may still be stale — the
                // pass snapshotted before it observed without the lock —
                // so the live record is re-checked under the lock: a
                // move that completed (or terminally failed) while this
                // pass was observing must never be regressed to
                // `COPYING`. Regressing a `COMPLETE` would let the next
                // pass re-complete it and bump the generation again.
                if self.mark_move_copying_if_current(&volume_id, &record)? {
                    report.marked_copying.push(volume_id);
                }
                continue;
            }
            if observation.relocated_to(&record.source_pv, &record.target_pv) {
                // Completion is provable from the world — including
                // for an IN_DOUBT record ("rolls forward under
                // reconciled authority") — and verified against the
                // record's own target, never a foreign relocation.
                // A live grow defers this one record (its completion
                // would void the grow's generation anchor); the pass
                // records the deferral and keeps classifying the rest.
                match self.complete_move_unless_growing(&volume_id, &vg_name, &lv_name, &record)? {
                    MoveCompletion::Completed(_) => report.completed.push(volume_id),
                    MoveCompletion::Deferred(_) => {
                        report.deferred_while_growing.push(volume_id);
                    }
                }
                continue;
            }
            match record.state {
                MoveVolumeBackingState::Preparing if redrive => {
                    self.redrive_preparing_move(
                        &volume_id,
                        &vg_name,
                        &lv_name,
                        &record,
                        &mut report,
                    )?;
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

    /// Re-drive a PREPARING record whose `pvmove` verifiably never
    /// started.
    ///
    /// The pass's snapshot may be stale and the in-memory anchor may be
    /// memory-only (a failed PREPARING save whose re-read was unknown),
    /// so the record is re-confirmed under the lock and against the
    /// durable file before the world is touched: only a PREPARING record
    /// for this exact source→target pair with a **durable** anchor may
    /// be re-driven. A record that changed, vanished, or was never
    /// durable is left untouched and reported (`redrive_failed`) — a
    /// crash between the start and the COPYING save must never lose the
    /// anchor. A source-less record is malformed and must never reach
    /// `pvmove`.
    ///
    /// A foreign move on the source (invisible to the records-only
    /// guard) would make a scoped `pvmove` attach to it and ignore its
    /// arguments, so the same world scan a fresh start performs runs
    /// first. Any refusal or start failure leaves the record PREPARING
    /// for the next tick and is reported (`redrive_failed`), never
    /// swallowed; a successful start rolls the record to COPYING only
    /// while it still matches (a concurrent completion is never
    /// overwritten).
    fn redrive_preparing_move(
        &self,
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        record: &crate::state::MoveRecord,
        report: &mut MoveReconcileReport,
    ) -> Result<(), ApiError> {
        if record.source_pv.is_empty() || record.target_pv.is_empty() {
            report.redrive_failed.push(volume_id.clone());
            return Ok(());
        }
        // Re-confirm the snapshot under the lock: another path may have
        // completed, superseded, or dropped the record since the pass
        // took its snapshot.
        let current = {
            let state = self.lock_state_stable()?;
            state.move_record(volume_id).cloned()
        };
        let Some(current) = current else {
            return Ok(());
        };
        if current.state != MoveVolumeBackingState::Preparing
            || current.source_pv != record.source_pv
            || current.target_pv != record.target_pv
        {
            // Superseded: the current record's owner classifies it.
            return Ok(());
        }
        // The anchor must already be durable before a pvmove starts. A
        // failed re-read is fail-closed; a durable record that moved on
        // (a concurrent completion/roll) must never be overwritten from
        // this stale snapshot.
        let Ok(durable) = LvmState::load(&self.state_path) else {
            report.redrive_failed.push(volume_id.clone());
            return Ok(());
        };
        match durable.move_record(volume_id) {
            None => {
                // Provably memory-only: no re-drive (a crash
                // between start and the COPYING save would lose
                // the anchor). A restart drops it if it never
                // landed; a consumer re-attach persists it first.
                report.redrive_failed.push(volume_id.clone());
                return Ok(());
            }
            Some(stored)
                if stored.state == MoveVolumeBackingState::Preparing
                    && stored.source_pv == record.source_pv
                    && stored.target_pv == record.target_pv => {}
            Some(_) => {
                report.redrive_failed.push(volume_id.clone());
                return Ok(());
            }
        }
        if self
            .guard_fresh_move_source(vg_name, &record.source_pv)
            .is_err()
        {
            report.redrive_failed.push(volume_id.clone());
            return Ok(());
        }
        match self.start_pvmove(vg_name, lv_name, &record.source_pv, &record.target_pv) {
            Ok(()) => {
                let mut state = self.lock_state_stable()?;
                let still_matches = state.move_record(volume_id).is_some_and(|stored| {
                    stored.state == MoveVolumeBackingState::Preparing
                        && stored.source_pv == record.source_pv
                        && stored.target_pv == record.target_pv
                });
                if !still_matches {
                    // A concurrent operation changed the record while
                    // the pvmove was starting: never overwrite it.
                    return Ok(());
                }
                if let Some(stored) = state.move_record_mut(volume_id) {
                    stored.state = MoveVolumeBackingState::Copying;
                    stored.detail = None;
                }
                self.persist(&mut state).inspect_err(|_| {
                    self.reconcile_failed_save(&mut state);
                })?;
                report.redriven.push(volume_id.clone());
            }
            Err(_) => report.redrive_failed.push(volume_id.clone()),
        }
        Ok(())
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
            let state = self.lock_state_stable()?;
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
        let mut state = self.lock_state_stable()?;
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
                // the in-memory resolution matches it. If it still says
                // IN_DOUBT, restore the parked record (with its previous
                // detail) so memory matches the durable file.
                match LvmState::load(&self.state_path) {
                    Ok(durable) => {
                        let committed = durable.move_record(volume_id).is_some_and(|stored| {
                            stored.state == MoveVolumeBackingState::Failed
                                && stored.source_pv == record.source_pv
                                && stored.target_pv == record.target_pv
                        });
                        if committed {
                            return Err(ApiError::new(
                                save_error.code,
                                format!(
                                    "{}; the durable state already records the verified \
                                     rollback of volume {volume_id} as FAILED, so the in-memory \
                                     resolution was kept",
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
                                "{}; the verified rollback of volume {volume_id} could not be \
                                 recorded durably, so the durable IN_DOUBT record remains the \
                                 anchor",
                                save_error.detail
                            ),
                        ));
                    }
                    Err(load_error) => {
                        // Unknown: the resolution's rename may have
                        // landed. Keep the verified FAILED resolution in
                        // memory (it is the outcome this pass proved
                        // from the world) so a later save cannot erase a
                        // possibly-durable FAILED record; report it
                        // honestly so an operator does not assume the
                        // park remains.
                        return Err(ApiError::new(
                            save_error.code,
                            format!(
                                "{}; the rollback resolution's durable record could not be \
                                 re-read ({load_error}), so treat the verified FAILED \
                                 resolution of volume {volume_id} as possibly durable",
                                save_error.detail
                            ),
                        ));
                    }
                }
            }
            report.resolved_failed.push(volume_id.clone());
        }
        Ok(())
    }

    /// Resolve a move record whose LV is **verifiably absent** (a
    /// successful `lvs` query has no row for it) to the settled
    /// `FAILED` state.
    ///
    /// A `pvmove` cannot be active on an LV that does not exist, so the
    /// move can never complete. Leaving the record
    /// `PREPARING`/`COPYING`/`IN_DOUBT` would fence delete forever:
    /// [`Self::move_blocks_delete`] refuses an unsettled move, yet no
    /// reconcile pass can ever observe a placement for an absent LV (it
    /// would park `IN_DOUBT` at the observation bound). `FAILED` is
    /// settled and the volume is marked `Failed` in the same durable
    /// save (mirroring the startup volume pass), so a live delete then
    /// completes with nothing to remove — no restart is needed. The
    /// detail states exactly what was proven — the LV is gone — never
    /// that the relocation succeeded or the source was freed.
    ///
    /// Never overwrites a `COMPLETE` record or a record superseded under
    /// the lock (the stale-snapshot discipline [`Self::finish_complete`]
    /// uses); answering the stored truth is not a fresh completion. A
    /// failed save is classified against a re-read: the rename may have
    /// landed (keep the resolution), the file may prove the prior record
    /// is still the anchor (restore it in memory), or the re-read may be
    /// unknown (keep the resolution as possibly durable — never roll
    /// back).
    fn resolve_absent_move(
        &self,
        volume_id: &VolumeId,
        vg_name: &str,
        lv_name: &str,
        record: &crate::state::MoveRecord,
    ) -> Result<MoveVolumeBackingResponse, ApiError> {
        let mut state = self.lock_state_stable()?;
        let Some(stored) = state.move_record(volume_id).cloned() else {
            return Err(not_found(volume_id));
        };
        if stored.state == MoveVolumeBackingState::Complete {
            return Ok(Self::move_response(&state, volume_id, &stored));
        }
        if stored.source_pv != record.source_pv || stored.target_pv != record.target_pv {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "the move record for volume {volume_id} was superseded while absence was \
                     being classified; refusing to resolve a stale snapshot"
                ),
            ));
        }
        if stored.state != MoveVolumeBackingState::Failed {
            let previous = stored.clone();
            if let Some(stored) = state.move_record_mut(volume_id) {
                stored.state = MoveVolumeBackingState::Failed;
                stored.detail = Some(format!(
                    "the LV {vg_name}/{lv_name} is verifiably absent from a successful lvs \
                     query, so the move to {} can never complete and no pvmove is active; \
                     the move is settled FAILED and the volume is marked Failed — its data \
                     is gone (no LV was removed by this resolution; delete then completes \
                     without a destructive command)",
                    record.target_pv
                ));
            }
            // Mirror the startup volume pass in the same durable save: a
            // volume whose LV is verifiably absent is `Failed`. Without
            // this, a live delete would still refuse an absent LV on a
            // volume left `Ready` until the next restart, even though the
            // move record no longer fences it.
            if let Some(volume) = state.volume_mut(volume_id) {
                volume.runtime.state = VolumeLifecycle::Failed;
            }
            if let Err(save_error) = self.persist(&mut state) {
                match LvmState::load(&self.state_path) {
                    Ok(durable) => {
                        let committed = durable.move_record(volume_id).is_some_and(|stored| {
                            stored.state == MoveVolumeBackingState::Failed
                                && stored.source_pv == record.source_pv
                                && stored.target_pv == record.target_pv
                        });
                        if !committed {
                            state.insert_move(volume_id.clone(), previous);
                        }
                        return Err(ApiError::new(
                            save_error.code,
                            if committed {
                                format!(
                                    "{}; the durable state already records the absent-LV \
                                     resolution of volume {volume_id} as FAILED, so the \
                                     in-memory resolution was kept",
                                    save_error.detail
                                )
                            } else {
                                format!(
                                    "{}; the absent-LV resolution of volume {volume_id} could \
                                     not be recorded durably, so the durable move record \
                                     remains the anchor (do not delete it by hand)",
                                    save_error.detail
                                )
                            },
                        ));
                    }
                    Err(load_error) => {
                        // Unknown: the resolution's rename may have
                        // landed. Keep the verified FAILED resolution in
                        // memory as possibly durable (never roll back);
                        // report it honestly.
                        return Err(ApiError::new(
                            save_error.code,
                            format!(
                                "{}; the absent-LV resolution's durable record could not be \
                                 re-read ({load_error}), so treat the verified FAILED \
                                 resolution of volume {volume_id} as possibly durable",
                                save_error.detail
                            ),
                        ));
                    }
                }
            }
        }
        state
            .move_record(volume_id)
            .map(|stored| Self::move_response(&state, volume_id, stored))
            .ok_or_else(|| not_found(volume_id))
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
        // The create body is synchronous end to end: `vgs`, `lvcreate` and
        // the size-verifying `lvs` are blocking LVM CLI calls. Running them
        // inline pins an async worker for their full duration, so at high
        // in-flight enough concurrent creates can pin every worker and
        // starve reads, attaches, reports and detaches. `block_in_place`
        // hands this worker's core to the scheduler for the duration; see
        // `grow_volume` for the handoff and fallback rationale.
        run_blocking_body(|| self.create_volume_inner(req))
    }

    async fn inspect_volume(&self, id: &VolumeId) -> Result<InspectVolumeResponse, ApiError> {
        let state = self.state_snapshot();
        state
            .volume(id)
            .map(|stored| inspect_response(id, stored))
            .ok_or_else(|| not_found(id))
    }

    async fn list_volumes(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<InspectVolumeResponse>, ApiError> {
        let state = self.state_snapshot();
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
        self.attach_volume_inner(volume_id, req).await
    }

    async fn detach_volume(
        &self,
        volume_id: &VolumeId,
        attachment_id: &AttachmentId,
        req: &DetachVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError> {
        self.detach_volume_inner(volume_id, attachment_id, req)
            .await
    }

    async fn report_attachment_state(
        &self,
        volume_id: &VolumeId,
        req: &ReportAttachmentStateRequest,
    ) -> Result<ReportAttachmentStateResponse, ApiError> {
        self.report_attachment_state_inner(volume_id, req).await
    }

    async fn grow_volume(
        &self,
        volume_id: &VolumeId,
        req: &GrowVolumeRequest,
    ) -> Result<GrowVolumeResponse, ApiError> {
        // One live grow drive per volume. A duplicate request is refused
        // immediately, even before it can observe the durable intent.
        // Total in-flight grow bodies are bounded by the daemon's request
        // admission cap; the provider adds no second, lower bound. A
        // grow-specific bulkhead below that cap was tested in a
        // non-evidence A/B diagnostic of the parent commit (limit 8 versus
        // effectively unbounded): the bounded arm added a provider-side
        // queue without raising the LVM-bound grow completion rate, and
        // had lower c128 throughput and longer grow latency. The mechanism
        // beyond that added queue is not attributed.
        let _drive = self.begin_grow_drive(volume_id)?;
        // The grow body is synchronous end to end: `vgs`, `lvextend` and
        // the size-verifying `lvs` are blocking LVM CLI calls. Running
        // them inline pins an async worker for their full duration, so at
        // high in-flight enough concurrent grows can pin every worker and
        // starve reads, attaches, reports and detaches behind them. The
        // grows-on/off diagnostic (non-evidence) implicated the grow path
        // in the residual c128 tail but did not attribute the mechanism;
        // worker pinning is a hypothesis, not a measured cause.
        // `block_in_place` hands this worker's core to the scheduler for
        // the duration, so admission keeps running.
        //
        // This is a worker handoff, not a move to the blocking pool: the
        // calling thread still runs the blocking body (blocking mode) and
        // the runtime may transiently grow its worker set to cover the
        // handed-off core. That cost is not measured here.
        //
        // `block_in_place` panics on a current-thread runtime (test
        // runtimes, embedded callers) or inside a `LocalSet`, where there
        // is no other worker to hand off to; `run_blocking_body` falls
        // back to the inline call there. The daemon uses the multi-thread
        // runtime, which is the case this protects.
        run_blocking_body(|| self.grow_volume_inner(volume_id, req))
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
        // The delete body is synchronous end to end: the `lvs` check,
        // `blkdiscard` and `lvremove` are blocking LVM CLI calls. Running
        // them inline pins an async worker for their full duration, so at
        // high in-flight enough concurrent deletes can pin every worker and
        // starve reads, attaches, reports and detaches. `block_in_place`
        // hands this worker's core to the scheduler for the duration; see
        // `grow_volume` for the handoff and fallback rationale.
        run_blocking_body(|| self.delete_volume_inner(volume_id, req))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Whether the current Tokio runtime reports the multi-thread flavor.
///
/// This is only a fast-path predicate. It is **not sufficient** to prove
/// [`tokio::task::block_in_place`] is legal: a `LocalSet` on a
/// multi-thread runtime disallows blocking in place even though the
/// flavor says `MultiThread`, and `block_in_place` then panics. The
/// actual handoff goes through [`run_blocking_body`], which falls back to
/// the inline call if the handoff is refused.
fn on_multi_thread_runtime() -> bool {
    matches!(
        tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()),
        Ok(tokio::runtime::RuntimeFlavor::MultiThread)
    )
}

/// Run a synchronous blocking body with the current async worker's core
/// handed off to the scheduler, when the runtime permits it.
///
/// `block_in_place` panics (before running the closure) inside a
/// `LocalSet`, where there is no legal handoff. The panic is caught here and
/// the body runs inline instead. (A plain current-thread runtime takes the
/// early return above and never reaches `block_in_place`.) The `ran` flag
/// distinguishes a refused handoff (body never
/// started, safe to run it now) from a panic *inside* the body (re-raised
/// unchanged, never run twice).
///
/// The caught refusal still reaches the process-wide panic hook, so an
/// embedder that calls grow, create or delete on a `LocalSet` sees a "can
/// call blocking only when running on the multi-threaded runtime" message on
/// stderr for each call. `volvisord` uses the multi-thread runtime and never
/// takes this path. The fallback also
/// runs the synchronous body on the caller's thread, which is the worker
/// pinning this helper exists to avoid; it is a correctness fallback for
/// embedders, not a throughput path.
fn run_blocking_body<F, R>(body: F) -> R
where
    F: FnOnce() -> R,
{
    if !on_multi_thread_runtime() {
        return body();
    }
    let mut body = Some(body);
    let mut ran = false;
    let result = catch_unwind(AssertUnwindSafe(|| {
        tokio::task::block_in_place(|| {
            let Some(body) = body.take() else {
                unreachable!("the blocking body runs exactly once");
            };
            ran = true;
            body()
        })
    }));
    match result {
        Ok(value) => value,
        Err(payload) => {
            if ran {
                // The body itself panicked; preserve the original panic.
                resume_unwind(payload);
            }
            // `block_in_place` refused to hand off (LocalSet or another
            // disallowed context): the caller's body never ran.
            let Some(body) = body.take() else {
                unreachable!("a refused handoff leaves the body unexecuted");
            };
            body()
        }
    }
}

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

/// Refuse a second active move on one source PV.
///
/// Verified LVM behavior: a scoped `pvmove` whose source PV already carries
/// an active move attaches to it, ignores its arguments and exits 0 — a
/// silent no-op. Callers run this against the freshest move records they
/// have: admission against the published snapshot (pre-CLI) and
/// `journal_move_intent` again under the writer lock (post-CLI), so a
/// concurrent admission cannot widen the check-then-act window.
fn ensure_no_other_active_move_on_source(
    state: &LvmState,
    volume_id: &VolumeId,
    source: &str,
) -> Result<(), ApiError> {
    let occupied = state.moves().iter().any(|(other, record)| {
        other != volume_id
            && record.source_pv == source
            && matches!(
                record.state,
                MoveVolumeBackingState::Preparing | MoveVolumeBackingState::Copying
            )
    });
    if occupied {
        return Err(ApiError::new(
            ApiErrorCode::InvalidState,
            format!(
                "another volume is already evacuating source PV {source}; LVM attaches a \
                 second scoped pvmove to the first and ignores its arguments, so one move \
                 per source PV at a time"
            ),
        ));
    }
    Ok(())
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

/// Build the standalone grow answer.
///
/// The provider-layer status is a placeholder, honestly labeled: this
/// layer has no VMM to notify. When the daemon wires the P6-B
/// grow-notification engine (volvisor-provider's `grow` module — the LVM
/// daemon path always does), the API layer composes the real status over
/// this response, inside the journal's execute closure, so the recorded
/// outcome carries it and replays byte-compatibly. This placeholder
/// remains the honest standalone answer (the conformance kit,
/// provider-direct callers): an attached frontend still needs a
/// (retried) notification; a detached volume has nobody to notify.
/// Never `Notified` from here.
fn grow_response(has_attachment: bool, effective_size_bytes: u64) -> GrowVolumeResponse {
    GrowVolumeResponse {
        backing_resized: true,
        guest_notification_status: if has_attachment {
            GrowGuestNotification::RetryRequired
        } else {
            GrowGuestNotification::NotApplicable
        },
        effective_size_bytes,
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

#[cfg(test)]
mod read_snapshot_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    pub(super) fn seeded_provider(dir: &tempfile::TempDir) -> (Arc<LvmProvider>, VolumeId) {
        seeded_provider_with_state_path(dir, dir.path().join("state.json"))
    }

    fn seeded_provider_with_state_path(
        dir: &tempfile::TempDir,
        state_path: std::path::PathBuf,
    ) -> (Arc<LvmProvider>, VolumeId) {
        let volume_id = VolumeId::new("vol-snapshot").expect("valid id");
        let mut state = LvmState::default();
        state.insert_volume(
            volume_id.clone(),
            StoredVolume {
                entry: VolumeEntry {
                    vg_name: "vg-1".to_owned(),
                    lv_name: "vol-vol-snapshot-abcd1234".to_owned(),
                    size_bytes: 1024,
                    requested_size_bytes: 1024,
                    generation: 1,
                    data_epoch: 0,
                    project_id: ProjectId::new("tenant-a").expect("valid id"),
                    block_size: 4096,
                    creation_payload: "{}".to_owned(),
                },
                runtime: VolumeRuntime {
                    state: VolumeLifecycle::Ready,
                    attachment: None,
                    retired_attachments: RetiredAttachmentFilter::default(),
                },
            },
        );
        let store_crash = Arc::new(StoreCrashHooks::new());
        let read_snapshot = Arc::new(RwLock::new(Arc::new(state.clone())));
        let writer = Arc::new(WriterCore::new(
            &state_path,
            Arc::clone(&store_crash),
            Arc::clone(&read_snapshot),
        ));
        let provider = LvmProvider {
            runner: Arc::new(volvisor_provider::FakeRunner::with_queue(Vec::new())),
            writer,
            state_path,
            sysfs_root: dir.path().to_path_buf(),
            vg_prefix: "vg-volvisor".to_owned(),
            expected_auth_token: "test-token".to_owned(),
            store_crash,
            move_timing: MoveTiming::default(),
            move_observe_failures: Mutex::new(BTreeMap::new()),
            grow_drives: Mutex::new(BTreeSet::new()),
            volume_drives: Mutex::new(BTreeMap::new()),
            capacity_reservations: Mutex::new(BTreeMap::new()),
            capacity_epoch: AtomicU64::new(0),
            state: Mutex::new(state.clone()),
            read_snapshot,
        };
        (Arc::new(provider), volume_id)
    }

    /// Regression for the Phase C benchmark finding: a schema-v3 state
    /// file can carry a saturated legacy `retired_attachment_filter`.
    /// Consulting it (as the Phase A code did) refuses every fresh id as a
    /// false positive. The generation is the fence, so the attach must
    /// succeed and the legacy bits must be ignored.
    #[tokio::test]
    async fn a_saturated_legacy_retired_filter_does_not_refuse_fresh_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        {
            let mut state = provider.lock_state().expect("writer guard");
            let runtime = &mut state.volume_mut(&volume_id).expect("volume").runtime;
            for index in 0..100_000u32 {
                runtime
                    .retired_attachments
                    .insert(&AttachmentId::new(format!("legacy-att-{index}")).expect("valid id"));
            }
            assert!(
                runtime
                    .retired_attachments
                    .maybe_contains(&AttachmentId::new("fresh-att").expect("valid id")),
                "the legacy filter is saturated and would false-positive a fresh id"
            );
        }
        let fresh = AttachmentId::new("fresh-att").expect("valid id");
        let request = volvisor_types::request::AttachVolumeRequest {
            api_version: volvisor_types::API_VERSION.to_owned(),
            operation_id: volvisor_types::OperationId::new("op-attach-fresh").expect("valid id"),
            vm_id: "vm-fresh".to_owned(),
            host_id: volvisor_types::HostId::new("host-fresh").expect("valid id"),
            attachment_id: fresh.clone(),
            expected_volume_generation: 1,
            access_mode: volvisor_types::request::AccessModeRequest::SingleWriter,
            requested_frontend: None,
            vmm_disk_id: None,
        };
        let attached = provider
            .attach_volume(&volume_id, &request)
            .await
            .expect("a saturated legacy filter must not refuse a fresh id");
        assert_eq!(attached.attachment_id, fresh);
        assert_eq!(attached.attachment_generation, 1);
    }

    #[test]
    fn reads_do_not_wait_for_the_writer_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, _volume_id) = seeded_provider(&dir);
        let writer = provider.lock_state().expect("writer guard");

        // A reader in another thread must get the published snapshot
        // while the writer guard is held (the pre-#62 design took the
        // same mutex and would block here).
        let (tx, rx) = mpsc::channel();
        let reader = Arc::clone(&provider);
        std::thread::spawn(move || {
            let count = reader.state_snapshot().volumes().len();
            let _ = tx.send(count);
        });
        let count = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reader must not block behind the writer");
        assert_eq!(count, 1);
        drop(writer);
    }

    #[test]
    fn a_committed_writer_guard_publishes_the_new_read_view() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        {
            let mut state = provider.lock_state().expect("writer guard");
            state.volume_mut(&volume_id).expect("volume").runtime.state = VolumeLifecycle::Degraded;
        }
        let snapshot = provider.state_snapshot();
        assert_eq!(
            snapshot
                .volume(&volume_id)
                .expect("volume in snapshot")
                .runtime
                .state,
            VolumeLifecycle::Degraded,
            "dropping a mutably-used writer guard publishes the new view"
        );
    }

    #[tokio::test]
    async fn a_failed_save_publishes_nothing_and_keeps_the_committed_ready_view() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A regular file where the state directory should be makes every
        // save fail before the temporary file is even created.
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"x").expect("blocker file");
        let (provider, volume_id) =
            seeded_provider_with_state_path(&dir, blocker.join("state.json"));

        let request = AttachVolumeRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-failed-save").expect("valid id"),
            vm_id: "vm-1".to_owned(),
            host_id: HostId::new("host-1").expect("valid id"),
            attachment_id: AttachmentId::new("att-failed-save").expect("valid id"),
            expected_volume_generation: 1,
            access_mode: volvisor_types::request::AccessModeRequest::SingleWriter,
            requested_frontend: None,
            vmm_disk_id: None,
        };
        let error = provider
            .attach_volume_inner(&volume_id, &request)
            .await
            .expect_err("the durable save must fail");
        assert_eq!(error.code, ApiErrorCode::Internal);

        // The failed mutation must not be observable as committed: a read
        // still sees Ready with no attachment, and a same-id retry must
        // re-execute rather than replay a phantom in-memory success.
        let snapshot = provider.state_snapshot();
        let stored = snapshot.volume(&volume_id).expect("volume");
        assert_eq!(stored.runtime.state, VolumeLifecycle::Ready);
        assert!(stored.runtime.attachment.is_none());
        assert_eq!(stored.entry.generation, 1);
    }

    #[tokio::test]
    async fn a_poison_refusal_never_resurrects_an_uncommitted_attachment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        // Make the seeded volume durable so admission alignment has a
        // baseline to restore after the poisoned capture is refused.
        {
            let mut state = provider.lock_state().expect("writer guard");
            provider.persist(&mut state).expect("seed durable state");
        }
        let request = AttachVolumeRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-poison").expect("valid id"),
            vm_id: "vm-1".to_owned(),
            host_id: HostId::new("host-1").expect("valid id"),
            attachment_id: AttachmentId::new("att-poison").expect("valid id"),
            expected_volume_generation: 1,
            access_mode: volvisor_types::request::AccessModeRequest::SingleWriter,
            requested_frontend: None,
            vmm_disk_id: None,
        };
        provider.writer.poison(WRITER_POISON_DETAIL);

        let first = provider
            .attach_volume_inner(&volume_id, &request)
            .await
            .expect_err("a poisoned writer refuses the capture");
        assert_eq!(first.code, ApiErrorCode::Internal);
        assert!(first.detail.contains(WRITER_POISON_DETAIL), "{first}");
        assert!(
            provider.writer.needs_alignment(),
            "a poison refusal must require the next admission to align"
        );
        assert!(
            provider
                .state_snapshot()
                .volume(&volume_id)
                .expect("volume")
                .runtime
                .attachment
                .is_none(),
            "the refused mutation is not published"
        );

        // A same-id retry must re-execute and fail again. If the refused
        // mutation were still visible to admission, the idempotency replay
        // would answer success from memory while the durable file has no
        // attachment.
        let retry = provider
            .attach_volume_inner(&volume_id, &request)
            .await
            .expect_err("a retry must never replay a phantom in-memory success");
        assert_eq!(retry.code, ApiErrorCode::Internal);
        assert!(retry.detail.contains(WRITER_POISON_DETAIL), "{retry}");

        let durable = LvmState::load(&provider.state_path).expect("durable state");
        assert!(
            durable
                .volume(&volume_id)
                .expect("volume")
                .runtime
                .attachment
                .is_none(),
            "the durable file never contained the refused attachment"
        );
        assert!(
            provider
                .state_snapshot()
                .volume(&volume_id)
                .expect("volume")
                .runtime
                .attachment
                .is_none(),
            "the admission view is aligned to the durable file"
        );
    }

    #[tokio::test]
    async fn a_failed_save_after_the_rename_keeps_the_durable_attachment_published() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        // The injected failure lands after the rename: the new state file
        // IS the durable state even though the save returns an error.
        provider.store_crash_hooks().arm_save_failure(
            volvisor_types::crash::STORE_LVM_STATE,
            volvisor_types::crash::StoreSavePoint::AfterRenameBeforeDirFsync,
            0,
        );
        let request = AttachVolumeRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-post-rename").expect("valid id"),
            vm_id: "vm-1".to_owned(),
            host_id: HostId::new("host-1").expect("valid id"),
            attachment_id: AttachmentId::new("att-post-rename").expect("valid id"),
            expected_volume_generation: 1,
            access_mode: volvisor_types::request::AccessModeRequest::SingleWriter,
            requested_frontend: None,
            vmm_disk_id: None,
        };
        let error = provider
            .attach_volume_inner(&volume_id, &request)
            .await
            .expect_err("the directory fsync fails after the rename");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(
            error.detail.contains("AfterRenameBeforeDirFsync"),
            "{error}"
        );

        let durable = LvmState::load(&provider.state_path).expect("durable state");
        assert!(
            durable
                .volume(&volume_id)
                .expect("volume")
                .runtime
                .attachment
                .is_some(),
            "the rename committed the attachment"
        );
        let snapshot = provider.state_snapshot();
        let stored = snapshot.volume(&volume_id).expect("volume");
        assert_eq!(
            stored
                .runtime
                .attachment
                .as_ref()
                .expect("the read view follows the durable file")
                .id,
            request.attachment_id,
            "a post-rename failure must not revert below the durable state"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn commit_async_re_raises_the_armed_kill_payload_in_band() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        provider.store_crash_hooks().arm(
            volvisor_types::crash::STORE_LVM_STATE,
            volvisor_types::crash::StoreSavePoint::AfterTmpWrite,
        );
        let request = AttachVolumeRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-async-kill").expect("valid id"),
            vm_id: "vm-1".to_owned(),
            host_id: HostId::new("host-1").expect("valid id"),
            attachment_id: AttachmentId::new("att-async-kill").expect("valid id"),
            expected_volume_generation: 1,
            access_mode: volvisor_types::request::AccessModeRequest::SingleWriter,
            requested_frontend: None,
            vmm_disk_id: None,
        };
        let task_provider = Arc::clone(&provider);
        let joined = tokio::spawn(async move {
            task_provider
                .attach_volume_inner(&volume_id, &request)
                .await
        })
        .await;
        let join_error = joined.expect_err("the armed kill must panic the caller task");
        assert!(
            join_error.is_panic(),
            "the async leader must re-raise the panic, not return a typed error"
        );
        let payload = join_error.into_panic();
        let text = payload
            .downcast_ref::<String>()
            .expect("the crash payload is the marked string");
        assert!(
            text.starts_with(volvisor_types::crash::CRASH_PANIC_PREFIX),
            "the campaign rig must still see the in-band crash payload: {text}"
        );
        assert!(
            provider.writer.lock_writer().poisoned.is_some(),
            "the async leader panic poisons the writer"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    // The test deliberately holds the per-path write gate across awaits so
    // the first attach's physical write cannot complete; the gate is a
    // blocking leaf, not a lock the async code needs on this thread.
    #[allow(clippy::await_holding_lock)]
    #[allow(clippy::too_many_lines)]
    async fn a_same_id_attach_replay_waits_for_durability_and_reexecutes_after_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        // The seeded memory state must be durable first: a covering
        // failure aligns the next admission back to this baseline.
        {
            let mut state = provider.lock_state().expect("writer guard");
            provider.persist(&mut state).expect("seed durable state");
        }
        let request = AttachVolumeRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-replay-window").expect("valid id"),
            vm_id: "vm-1".to_owned(),
            host_id: HostId::new("host-1").expect("valid id"),
            attachment_id: AttachmentId::new("att-replay-window").expect("valid id"),
            expected_volume_generation: 1,
            access_mode: volvisor_types::request::AccessModeRequest::SingleWriter,
            requested_frontend: None,
            vmm_disk_id: None,
        };

        // Hold the per-path physical-write gate so the first attach's save
        // cannot complete until this test releases it, and make that first
        // write fail before the rename. The re-executed write then
        // succeeds.
        let gate = WriterCore::gate_for(&provider.state_path);
        let held = gate.lock().expect("hold the physical-write gate");
        let writes_before = provider.writer.physical_writes.load(Ordering::SeqCst);
        provider.store_crash_hooks().arm_save_failure(
            volvisor_types::crash::STORE_LVM_STATE,
            volvisor_types::crash::StoreSavePoint::AfterTmpWrite,
            0,
        );

        let first_provider = Arc::clone(&provider);
        let first_request = request.clone();
        let first_volume = volume_id.clone();
        let first = tokio::spawn(async move {
            first_provider
                .attach_volume_inner(&first_volume, &first_request)
                .await
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while provider.writer.physical_writes.load(Ordering::SeqCst) <= writes_before {
            assert!(
                std::time::Instant::now() < deadline,
                "the first attach never reached its physical write"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // The same-id replay arrives while the first capture is still
        // unproven. It must not answer success from the uncommitted
        // in-memory attachment.
        let second_provider = Arc::clone(&provider);
        let second_request = request.clone();
        let second_volume = volume_id.clone();
        let second = tokio::spawn(async move {
            second_provider
                .attach_volume_inner(&second_volume, &second_request)
                .await
        });

        // It must park rather than answer, and it must not hold the
        // provider state lock while it waits. Retry the lock probe so a
        // brief admission critical section is not a false failure.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                !second.is_finished(),
                "a same-id replay must wait for the covering capture's watermark"
            );
            if provider.state.try_lock().is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the replay must not hold the provider state lock while it waits"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let durable = LvmState::load(&provider.state_path).expect("durable state");
        assert!(
            durable
                .volume(&volume_id)
                .expect("volume")
                .runtime
                .attachment
                .is_none(),
            "the first attachment is not durable while the write is held"
        );

        drop(held);
        let first_result = first.await.expect("first attach task");
        assert!(
            first_result.is_err(),
            "the injected pre-rename failure must reach the first caller"
        );
        let second_result = second
            .await
            .expect("second attach task")
            .expect("the replay must re-execute after the failed covering capture");
        assert_eq!(second_result.attachment_id, request.attachment_id);

        let durable = LvmState::load(&provider.state_path).expect("durable state");
        assert_eq!(
            durable
                .volume(&volume_id)
                .expect("volume")
                .runtime
                .attachment
                .as_ref()
                .expect("the re-executed attach is durable")
                .id,
            request.attachment_id,
            "a replay must never turn a failed save into a durable-lie"
        );
        assert_eq!(
            provider.writer.physical_writes.load(Ordering::SeqCst),
            writes_before + 2,
            "one failed write and one successful re-execution"
        );
    }

    #[test]
    fn persisting_a_copying_record_never_regresses_a_landed_completion() {
        // A concurrent supervisor can complete the move between the
        // pvmove start and this save. Regressing COMPLETE to COPYING
        // would let the next reconcile pass complete it again and bump
        // the volume generation a second time.
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        let source = "/dev/pv-source";
        let target = "/dev/pv-target";
        let complete = crate::state::MoveRecord {
            operation_id: volvisor_types::OperationId::new("op-complete").expect("valid id"),
            source_pv: source.to_owned(),
            target_pv: target.to_owned(),
            state: MoveVolumeBackingState::Complete,
            detail: None,
        };
        {
            let mut state = provider.lock_state().expect("writer guard");
            state.insert_move(volume_id.clone(), complete);
            provider.persist(&mut state).expect("seed the completion");
        }

        let prior = crate::state::MoveRecord {
            operation_id: volvisor_types::OperationId::new("op-drive").expect("valid id"),
            source_pv: source.to_owned(),
            target_pv: target.to_owned(),
            state: MoveVolumeBackingState::Preparing,
            detail: None,
        };
        let request = MoveVolumeBackingRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-drive").expect("valid id"),
            target_pool_id: target.to_owned(),
            expected_generation: 1,
            max_copy_bytes_per_sec: None,
        };
        provider
            .persist_copying_record(&volume_id, &request, &prior)
            .expect("an already-complete move is answered, not regressed");

        let snapshot = provider.state_snapshot();
        let record = snapshot.move_record(&volume_id).expect("move record");
        assert_eq!(
            record.state,
            MoveVolumeBackingState::Complete,
            "the landed completion must never be regressed to COPYING"
        );
        assert_eq!(record.operation_id.as_str(), "op-complete");
    }

    #[test]
    fn persisting_a_copying_record_refuses_a_superseded_placement() {
        // The record was replaced by a newer move (different source or
        // target) while this drive was starting its pvmove. Clobbering
        // it would attach this drive's COPYING save to a move it did
        // not start.
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        let superseding = crate::state::MoveRecord {
            operation_id: volvisor_types::OperationId::new("op-newer").expect("valid id"),
            source_pv: "/dev/pv-other-source".to_owned(),
            target_pv: "/dev/pv-other-target".to_owned(),
            state: MoveVolumeBackingState::Preparing,
            detail: None,
        };
        {
            let mut state = provider.lock_state().expect("writer guard");
            state.insert_move(volume_id.clone(), superseding.clone());
            provider.persist(&mut state).expect("seed the newer move");
        }

        let prior = crate::state::MoveRecord {
            operation_id: volvisor_types::OperationId::new("op-drive").expect("valid id"),
            source_pv: "/dev/pv-source".to_owned(),
            target_pv: "/dev/pv-target".to_owned(),
            state: MoveVolumeBackingState::Preparing,
            detail: None,
        };
        let request = MoveVolumeBackingRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-drive").expect("valid id"),
            target_pool_id: "/dev/pv-target".to_owned(),
            expected_generation: 1,
            max_copy_bytes_per_sec: None,
        };
        let error = provider
            .persist_copying_record(&volume_id, &request, &prior)
            .expect_err("a superseded placement is refused");
        assert_eq!(error.code, ApiErrorCode::InvalidState);

        let snapshot = provider.state_snapshot();
        let record = snapshot.move_record(&volume_id).expect("move record");
        assert_eq!(record, &superseding, "the newer move is untouched");
    }

    #[test]
    fn journaling_a_move_rechecks_the_one_move_per_source_rule_under_the_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        let other_id = VolumeId::new("vol-other").expect("valid id");
        {
            let mut state = provider.lock_state().expect("writer guard");
            state.insert_volume(
                other_id.clone(),
                StoredVolume {
                    entry: VolumeEntry {
                        vg_name: "vg-1".to_owned(),
                        lv_name: "vol-vol-other-abcd1234".to_owned(),
                        size_bytes: 1024,
                        requested_size_bytes: 1024,
                        generation: 1,
                        data_epoch: 0,
                        project_id: ProjectId::new("tenant-a").expect("valid id"),
                        block_size: 4096,
                        creation_payload: "{}".to_owned(),
                    },
                    runtime: VolumeRuntime {
                        state: VolumeLifecycle::Ready,
                        attachment: None,
                        retired_attachments: RetiredAttachmentFilter::default(),
                    },
                },
            );
            // The concurrent admission's anchor: a move on the same source
            // PV committed by another request.
            state.insert_move(
                other_id,
                crate::state::MoveRecord {
                    operation_id: volvisor_types::OperationId::new("op-concurrent")
                        .expect("valid id"),
                    source_pv: "/dev/sda".to_owned(),
                    target_pv: "/dev/sdc".to_owned(),
                    state: MoveVolumeBackingState::Preparing,
                    detail: None,
                },
            );
        }

        let request = MoveVolumeBackingRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-second-move").expect("valid id"),
            target_pool_id: "/dev/sdb".to_owned(),
            expected_generation: 1,
            max_copy_bytes_per_sec: None,
        };
        let error = provider
            .journal_move_intent(&volume_id, &request, Some("/dev/sda"))
            .expect_err("the second move from one source PV must be refused");
        assert_eq!(error.code, ApiErrorCode::InvalidState);
        assert!(
            error.detail.contains("one move per source PV"),
            "the refusal names the LVM no-op hazard: {error}"
        );
    }

    #[test]
    fn parking_over_a_resolved_failure_answers_the_settled_record() {
        // A concurrent reconcile may resolve an IN_DOUBT record FAILED
        // between this caller's failed observation and its park. The
        // settled verdict is durable truth: re-parking would resurrect
        // the move to IN_DOUBT and destroy the resolution detail.
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        let resolved = crate::state::MoveRecord {
            operation_id: volvisor_types::OperationId::new("op-resolved").expect("valid id"),
            source_pv: "/dev/sda".to_owned(),
            target_pv: "/dev/sdb".to_owned(),
            state: MoveVolumeBackingState::Failed,
            detail: Some("resolved FAILED by reconciliation".to_owned()),
        };
        {
            let mut state = provider.lock_state().expect("writer guard");
            state.insert_move(volume_id.clone(), resolved.clone());
        }

        let response = provider
            .park_in_doubt(&volume_id, "a late observation failure".to_owned())
            .expect("a terminal record is answered, not parked over");
        assert_eq!(response.state, MoveVolumeBackingState::Failed);
        assert_eq!(response.source_pv, resolved.source_pv);
        let snapshot = provider.state_snapshot();
        assert_eq!(
            snapshot.move_record(&volume_id),
            Some(&resolved),
            "the settled FAILED record and its audit detail are untouched"
        );
    }

    #[test]
    fn journaling_a_fresh_move_refuses_a_record_parked_after_admission() {
        // Admission reads a published snapshot before this request's
        // lock-free CLI observations. A prior move can park IN_DOUBT in
        // that window; the fresh drive must not silently supersede the
        // park (the contract says it waits for reconciliation or an
        // operator).
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        let parked = crate::state::MoveRecord {
            operation_id: volvisor_types::OperationId::new("op-parked").expect("valid id"),
            source_pv: "/dev/sda".to_owned(),
            target_pv: "/dev/sdb".to_owned(),
            state: MoveVolumeBackingState::InDoubt,
            detail: Some("the pvmove ended without relocating".to_owned()),
        };
        {
            let mut state = provider.lock_state().expect("writer guard");
            state.insert_move(volume_id.clone(), parked.clone());
        }

        let request = MoveVolumeBackingRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-fresh").expect("valid id"),
            target_pool_id: "/dev/sdc".to_owned(),
            expected_generation: 1,
            max_copy_bytes_per_sec: None,
        };
        let error = provider
            .journal_move_intent(&volume_id, &request, Some("/dev/sda"))
            .expect_err("a parked record must not be superseded");
        assert_eq!(error.code, ApiErrorCode::InvalidState);
        assert!(
            error.detail.contains("IN_DOUBT"),
            "the refusal names the park contract: {error}"
        );
        let snapshot = provider.state_snapshot();
        assert_eq!(
            snapshot.move_record(&volume_id),
            Some(&parked),
            "the parked record is untouched"
        );
    }

    #[test]
    fn journaling_a_fresh_move_refuses_a_foreign_mirror_record() {
        // MIRROR_READY/PIVOTED are mirror-path states this same-VG
        // implementation never writes; a record holding one is foreign
        // and must be refused, not silently replaced.
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        let foreign = crate::state::MoveRecord {
            operation_id: volvisor_types::OperationId::new("op-foreign").expect("valid id"),
            source_pv: "/dev/sda".to_owned(),
            target_pv: "/dev/sdb".to_owned(),
            state: MoveVolumeBackingState::MirrorReady,
            detail: None,
        };
        {
            let mut state = provider.lock_state().expect("writer guard");
            state.insert_move(volume_id.clone(), foreign.clone());
        }

        let request = MoveVolumeBackingRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-fresh").expect("valid id"),
            target_pool_id: "/dev/sdc".to_owned(),
            expected_generation: 1,
            max_copy_bytes_per_sec: None,
        };
        let error = provider
            .journal_move_intent(&volume_id, &request, Some("/dev/sda"))
            .expect_err("a foreign record must not be superseded");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(
            error.detail.contains("MIRROR_READY"),
            "the refusal names the foreign state: {error}"
        );
        let snapshot = provider.state_snapshot();
        assert_eq!(snapshot.move_record(&volume_id), Some(&foreign));
    }

    #[test]
    fn journaling_a_fresh_move_refuses_a_same_target_move_completed_after_admission() {
        // A concurrent drive can complete the exact move this fresh
        // request targets between admission's published-snapshot read
        // and the writer lock. Superseding the landed COMPLETE with a
        // fresh PREPARING would run a no-op pvmove, bump the generation
        // again, and transiently re-erect the delete fence. The retry
        // instead observes admission's idempotent answer.
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        let completed = crate::state::MoveRecord {
            operation_id: volvisor_types::OperationId::new("op-completed").expect("valid id"),
            source_pv: "/dev/sda".to_owned(),
            target_pv: "/dev/sdb".to_owned(),
            state: MoveVolumeBackingState::Complete,
            detail: None,
        };
        {
            let mut state = provider.lock_state().expect("writer guard");
            state.insert_move(volume_id.clone(), completed.clone());
        }

        let request = MoveVolumeBackingRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-fresh").expect("valid id"),
            target_pool_id: "/dev/sdb".to_owned(),
            expected_generation: 1,
            max_copy_bytes_per_sec: None,
        };
        let error = provider
            .journal_move_intent(&volume_id, &request, Some("/dev/sda"))
            .expect_err("a landed same-target completion must not be superseded");
        assert_eq!(error.code, ApiErrorCode::InvalidState);
        assert!(
            error
                .detail
                .contains("completed while this request was being admitted"),
            "the refusal tells the caller to retry the idempotent answer: {error}"
        );
        let snapshot = provider.state_snapshot();
        assert_eq!(
            snapshot.move_record(&volume_id),
            Some(&completed),
            "the landed completion is untouched"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn capture_reservation_order_matches_state_lock_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, _volume_id) = seeded_provider(&dir);
        let mut handles = Vec::new();
        for index in 0..8 {
            let provider = Arc::clone(&provider);
            handles.push(tokio::spawn(async move {
                let (seq, observed, reservation) = {
                    let mut state = provider.lock_state().expect("writer guard");
                    let id = VolumeId::new(format!("vol-order-{index}")).expect("valid id");
                    state.insert_move(
                        id,
                        crate::state::MoveRecord {
                            operation_id: volvisor_types::OperationId::new(format!(
                                "op-order-{index}"
                            ))
                            .expect("valid id"),
                            source_pv: format!("/dev/source-{index}"),
                            target_pv: "/dev/target".to_owned(),
                            state: MoveVolumeBackingState::Preparing,
                            detail: None,
                        },
                    );
                    let observed = state.moves().len();
                    let reservation = state.capture_and_reserve().expect("capture");
                    (reservation.seq, observed, reservation)
                };
                provider
                    .writer
                    .commit_async(reservation)
                    .await
                    .expect("commit");
                (seq, observed)
            }));
        }
        let mut observed = Vec::new();
        for handle in handles {
            observed.push(handle.await.expect("task"));
        }
        observed.sort_by_key(|(seq, _)| *seq);
        for pair in observed.windows(2) {
            assert!(
                pair[1].1 > pair[0].1,
                "a later sequence must contain every earlier capture: {observed:?}"
            );
        }
    }

    #[tokio::test]
    async fn needs_alignment_blocks_captures_and_clears_after_admission() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        // Make the seeded state durable so admission has a baseline to
        // align to after the injected failure.
        {
            let mut state = provider.lock_state().expect("writer guard");
            provider.persist(&mut state).expect("seed durable state");
        }
        provider.store_crash_hooks().arm_save_failure(
            volvisor_types::crash::STORE_LVM_STATE,
            volvisor_types::crash::StoreSavePoint::AfterTmpWrite,
            0,
        );
        let request = AttachVolumeRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-align").expect("valid id"),
            vm_id: "vm-1".to_owned(),
            host_id: HostId::new("host-1").expect("valid id"),
            attachment_id: AttachmentId::new("att-align").expect("valid id"),
            expected_volume_generation: 1,
            access_mode: volvisor_types::request::AccessModeRequest::SingleWriter,
            requested_frontend: None,
            vmm_disk_id: None,
        };
        provider
            .attach_volume_inner(&volume_id, &request)
            .await
            .expect_err("the injected pre-rename failure is not durable");
        assert!(
            provider.writer.needs_alignment(),
            "a hard failure requires admission alignment"
        );

        // New captures are refused until admission aligns.
        let mut probe = LvmState::default();
        let encoded = probe.encode_for_save().expect("encode probe");
        let rejected = provider
            .writer
            .reserve(Arc::from(encoded), Arc::new(probe))
            .err()
            .expect("a capture before alignment must be refused");
        assert!(
            rejected.detail.contains(NEEDS_ALIGNMENT_DETAIL),
            "{rejected}"
        );

        {
            let state = provider.lock_state().expect("admission aligns");
            assert!(!provider.writer.needs_alignment());
            assert!(
                state.volume(&volume_id).is_some(),
                "admission loads the durable baseline"
            );
            assert!(
                state
                    .volume(&volume_id)
                    .expect("volume")
                    .runtime
                    .attachment
                    .is_none(),
                "the failed attachment is not resurrected"
            );
        }

        // The flag is clear, so the same attach now commits.
        let attached = provider
            .attach_volume_inner(&volume_id, &request)
            .await
            .expect("the retry after alignment succeeds");
        assert_eq!(attached.attachment_id, request.attachment_id);
        assert_eq!(
            provider
                .state_snapshot()
                .volume(&volume_id)
                .expect("volume")
                .runtime
                .attachment
                .as_ref()
                .expect("attachment")
                .id,
            request.attachment_id
        );
    }

    #[tokio::test]
    async fn an_unreadable_durable_file_fails_admission_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"x").expect("blocker file");
        let (provider, volume_id) =
            seeded_provider_with_state_path(&dir, blocker.join("state.json"));
        let request = AttachVolumeRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-unreadable").expect("valid id"),
            vm_id: "vm-1".to_owned(),
            host_id: HostId::new("host-1").expect("valid id"),
            attachment_id: AttachmentId::new("att-unreadable").expect("valid id"),
            expected_volume_generation: 1,
            access_mode: volvisor_types::request::AccessModeRequest::SingleWriter,
            requested_frontend: None,
            vmm_disk_id: None,
        };
        provider
            .attach_volume_inner(&volume_id, &request)
            .await
            .expect_err("the save fails and the durable file is unreadable");
        assert!(provider.writer.needs_alignment());
        let error = provider
            .lock_state()
            .err()
            .expect("admission must fail closed on an unreadable durable file");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(
            error.detail.contains("requires admission alignment"),
            "{error}"
        );
        assert!(
            provider.writer.needs_alignment(),
            "an unreadable durable file leaves the flag set for a retry"
        );
    }

    #[tokio::test]
    async fn a_readable_different_durable_file_is_adopted_at_admission() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, _volume_id) = seeded_provider(&dir);
        let mut durable = LvmState::default();
        let foreign = VolumeId::new("vol-adopted").expect("valid id");
        durable.insert_move(
            foreign.clone(),
            crate::state::MoveRecord {
                operation_id: volvisor_types::OperationId::new("op-adopted").expect("valid id"),
                source_pv: "/dev/sda".to_owned(),
                target_pv: "/dev/sdb".to_owned(),
                state: MoveVolumeBackingState::Preparing,
                detail: None,
            },
        );
        durable
            .save(&provider.state_path)
            .expect("write a different durable state");
        {
            provider.writer.lock_writer().needs_alignment = true;
        }
        {
            let state = provider.lock_state().expect("admission adopts durable");
            assert_eq!(
                state.move_record(&foreign),
                durable.move_record(&foreign),
                "the guard is aligned to the readable durable file"
            );
            assert!(
                !provider.writer.needs_alignment(),
                "a successful readable alignment clears the flag"
            );
        }
    }

    #[tokio::test]
    async fn a_report_no_op_does_not_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = seeded_provider(&dir);
        let attach = AttachVolumeRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-report-attach").expect("valid id"),
            vm_id: "vm-1".to_owned(),
            host_id: HostId::new("host-1").expect("valid id"),
            attachment_id: AttachmentId::new("att-report").expect("valid id"),
            expected_volume_generation: 1,
            access_mode: volvisor_types::request::AccessModeRequest::SingleWriter,
            requested_frontend: None,
            vmm_disk_id: None,
        };
        let attached = provider
            .attach_volume_inner(&volume_id, &attach)
            .await
            .expect("attach");
        let report = ReportAttachmentStateRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: volvisor_types::OperationId::new("op-report").expect("valid id"),
            attachment_id: attach.attachment_id.clone(),
            expected_attachment_generation: attached.attachment_generation,
            state: AttachmentState::Advertised,
            evidence: AttachmentEvidence::VmmConfigured,
            reporter: Some("rig".to_owned()),
            detail: None,
        };
        let first = provider
            .report_attachment_state_inner(&volume_id, &report)
            .await
            .expect("first report");
        assert!(first.applied, "the first report advances the state");
        let writes = provider.writer.physical_writes.load(Ordering::SeqCst);
        let second = provider
            .report_attachment_state_inner(&volume_id, &report)
            .await
            .expect("no-op report");
        assert!(!second.applied, "the repeated report is a no-op");
        assert_eq!(
            provider.writer.physical_writes.load(Ordering::SeqCst),
            writes,
            "a no-op report must not perform a physical save"
        );
    }
}

#[cfg(test)]
mod capacity_epoch_tests {
    use super::*;

    fn grow_request(new_size_bytes: u64) -> GrowVolumeRequest {
        GrowVolumeRequest {
            api_version: volvisor_types::API_VERSION.to_owned(),
            operation_id: volvisor_types::OperationId::new("op-epoch-grow").expect("valid id"),
            new_size_bytes,
            expected_generation: 1,
        }
    }

    fn capacity() -> VgCapacity {
        VgCapacity {
            free_bytes: 1 << 30,
            extent_bytes: 4096,
        }
    }

    /// The reservation is recorded before the epoch is re-read, so an
    /// allocation that completed after the `vgs` sample is still caught.
    ///
    /// This isolates the post-reservation check: the sample and the
    /// reservation insertion happen at the same epoch value, and only the
    /// counter bump (a completed `lvcreate`/`lvextend`) separates them. If
    /// the check were removed, this attempt would reserve and persist a
    /// grow intent against free space that no longer exists.
    #[tokio::test]
    async fn a_completed_allocation_invalidates_a_sampled_reservation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, volume_id) = super::read_snapshot_tests::seeded_provider(&dir);
        let mut state = provider.lock_state().expect("writer guard");
        let request = grow_request(2048);
        let sampled_epoch = provider.capacity_epoch();

        // A concurrent `lvcreate` or `lvextend` completed between the
        // sample and this admission.
        provider.note_capacity_change();

        let outcome = provider
            .prepare_grow(
                &mut state,
                &volume_id,
                &request,
                "vg-1",
                capacity(),
                1024,
                1,
                false,
                sampled_epoch,
            )
            .expect("prepare the stale grow");
        assert!(
            matches!(outcome, GrowPrepare::SampleStale),
            "a reservation derived from a stale sample must be released"
        );
        assert_eq!(
            provider.reserved_capacity("vg-1"),
            0,
            "the stale reservation must not leak"
        );
        assert!(
            state.grow_intent(&volume_id).is_none(),
            "no durable intent may be written from a stale sample"
        );

        // The same admission at the current epoch reserves exactly the
        // extent-rounded demand and becomes durable.
        let current_epoch = provider.capacity_epoch();
        let fresh = provider
            .prepare_grow(
                &mut state,
                &volume_id,
                &request,
                "vg-1",
                capacity(),
                1024,
                1,
                false,
                current_epoch,
            )
            .expect("prepare the fresh grow");
        assert!(
            matches!(fresh, GrowPrepare::Prepared { .. }),
            "the current epoch must be accepted"
        );
        let GrowPrepare::Prepared {
            grow_intent: intent,
            reservation,
        } = fresh
        else {
            return;
        };
        assert_eq!(intent.requested_size_bytes, 2048);
        assert!(state.grow_intent(&volume_id).is_some());
        assert_eq!(provider.reserved_capacity("vg-1"), 4096);
        drop(reservation);
        assert_eq!(
            provider.reserved_capacity("vg-1"),
            0,
            "dropping the reservation returns the promise"
        );
    }
}

#[cfg(test)]
mod admission_retry_tests {
    use super::*;

    /// A `vgs` that completes an allocation on every call makes every
    /// sample stale. The admission must stop after the cap with a typed
    /// refusal instead of looping forever. The refusal is journaled as
    /// terminal for that operation id (this test exercises the provider
    /// layer only; the journal contract is documented on the constant).
    #[test]
    fn sustained_epoch_churn_refuses_after_the_attempt_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("state.json");
        let volume_id = VolumeId::new("vol-retry").expect("valid id");
        let mut state = LvmState::default();
        state.insert_volume(
            volume_id.clone(),
            StoredVolume {
                entry: VolumeEntry {
                    vg_name: "vg-1".to_owned(),
                    lv_name: "vol-vol-retry-abcd1234".to_owned(),
                    size_bytes: 1024,
                    requested_size_bytes: 1024,
                    generation: 1,
                    data_epoch: 0,
                    project_id: ProjectId::new("tenant-a").expect("valid id"),
                    block_size: 4096,
                    creation_payload: "{}".to_owned(),
                },
                runtime: VolumeRuntime {
                    state: VolumeLifecycle::Ready,
                    attachment: None,
                    retired_attachments: RetiredAttachmentFilter::default(),
                },
            },
        );
        let store_crash = Arc::new(StoreCrashHooks::new());
        let read_snapshot = Arc::new(RwLock::new(Arc::new(state.clone())));
        let writer = Arc::new(WriterCore::new(
            &state_path,
            Arc::clone(&store_crash),
            Arc::clone(&read_snapshot),
        ));
        let provider: Arc<LvmProvider> = Arc::new_cyclic(|weak: &Weak<LvmProvider>| {
            let weak = weak.clone();
            let runner = Arc::new(volvisor_provider::FakeRunner::with_closure(
                move |program, _args| {
                    if program == "vgs" {
                        if let Some(provider) = weak.upgrade() {
                            provider.note_capacity_change();
                        }
                        return Some(CommandOutput::success(
                            r#"{"report":[{"vg":[{"vg_name":"vg-1","vg_free":"1099511627776","vg_size":"1099511627776","vg_extent_size":"4194304"}]}]}"#,
                        ));
                    }
                    None
                },
            ));
            LvmProvider {
                runner,
                writer,
                state_path: state_path.clone(),
                sysfs_root: dir.path().to_path_buf(),
                vg_prefix: "vg-volvisor".to_owned(),
                expected_auth_token: "test-token".to_owned(),
                store_crash,
                move_timing: MoveTiming::default(),
                move_observe_failures: Mutex::new(BTreeMap::new()),
                grow_drives: Mutex::new(BTreeSet::new()),
                volume_drives: Mutex::new(BTreeMap::new()),
                capacity_reservations: Mutex::new(BTreeMap::new()),
                capacity_epoch: AtomicU64::new(0),
                state: Mutex::new(state.clone()),
                read_snapshot,
            }
        });

        let request = GrowVolumeRequest {
            api_version: volvisor_types::API_VERSION.to_owned(),
            operation_id: volvisor_types::OperationId::new("op-retry").expect("valid id"),
            new_size_bytes: 2048,
            expected_generation: 1,
        };
        let outcome = provider.prepare_grow_admission(&volume_id, &request);
        assert!(
            outcome.is_err(),
            "continuous churn must exhaust the attempt cap"
        );
        let Err(err) = outcome else {
            return;
        };
        assert_eq!(err.code, ApiErrorCode::Overloaded, "{err}");
        assert!(err.detail.contains("attempts"), "{err}");
    }
}

#[cfg(test)]
mod blocking_body_tests {
    use super::*;

    /// A `LocalSet` on a multi-thread runtime makes `block_in_place`
    /// refuse the handoff even though the runtime flavor is multi-thread.
    /// This test proves the fallback runs the body exactly once and does
    /// not panic; it cannot observe the refusal itself, because the
    /// refusal reaches `run_blocking_body` as a panic that is caught (the
    /// default panic hook still prints to stderr). The `ran` flag is what
    /// distinguishes a refusal from a body panic; the panic test below
    /// covers the other branch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_local_set_runs_the_body_exactly_once() {
        let local = tokio::task::LocalSet::new();
        let runs = AtomicUsize::new(0);
        let value = local
            .run_until(async {
                run_blocking_body(|| {
                    runs.fetch_add(1, Ordering::SeqCst);
                    42
                })
            })
            .await;
        assert_eq!(value, 42);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    /// A panic inside the blocking body is the body's own; it must be
    /// re-raised unchanged and must not cause a second execution.
    #[allow(clippy::panic)] // the body's panic is the property under test
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_body_panic_is_propagated_and_runs_once() {
        let runs = AtomicUsize::new(0);
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            run_blocking_body(|| {
                runs.fetch_add(1, Ordering::SeqCst);
                std::panic::panic_any("blocking body panic");
            })
        }));
        assert!(outcome.is_err());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod durable_writer_tests {
    use super::*;
    use volvisor_types::crash::{CRASH_PANIC_PREFIX, StoreSavePoint};

    struct Fixture {
        _dir: tempfile::TempDir,
        path: PathBuf,
        hooks: Arc<StoreCrashHooks>,
        read: Arc<RwLock<Arc<LvmState>>>,
        core: Arc<WriterCore>,
    }

    fn core_for(
        path: &Path,
        hooks: Arc<StoreCrashHooks>,
    ) -> (Arc<WriterCore>, Arc<RwLock<Arc<LvmState>>>) {
        let read = Arc::new(RwLock::new(Arc::new(LvmState::default())));
        let core = Arc::new(WriterCore::new(path, hooks, Arc::clone(&read)));
        (core, read)
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let hooks = Arc::new(StoreCrashHooks::new());
        let (core, read) = core_for(&path, Arc::clone(&hooks));
        Fixture {
            _dir: dir,
            path,
            hooks,
            read,
            core,
        }
    }

    fn state_with_moves(count: usize) -> LvmState {
        let mut state = LvmState::default();
        for index in 0..count {
            let id = VolumeId::new(format!("vol-{index}")).expect("valid id");
            state.insert_move(
                id,
                crate::state::MoveRecord {
                    operation_id: volvisor_types::OperationId::new(format!("op-{index}"))
                        .expect("valid id"),
                    source_pv: format!("/dev/source-{index}"),
                    target_pv: "/dev/target".to_owned(),
                    state: MoveVolumeBackingState::Preparing,
                    detail: None,
                },
            );
        }
        state
    }

    fn capture(state: &LvmState) -> (Arc<[u8]>, Arc<LvmState>) {
        let mut state = state.clone();
        let encoded = state.encode_for_save().expect("encode test state");
        (Arc::from(encoded), Arc::new(state))
    }

    fn expect_leader(reservation: Reservation) -> (u64, LeaderToken) {
        let Reservation { seq, role } = reservation;
        let token = match role {
            ReservationRole::Leader(token) => Some(token),
            ReservationRole::Waiter => None,
        };
        (seq, token.expect("the first reservation must lead"))
    }

    fn reserve(core: &Arc<WriterCore>, state: &LvmState) -> Reservation {
        let (encoded, snapshot) = capture(state);
        core.reserve(encoded, snapshot).expect("reserve")
    }

    #[test]
    fn writer_gates_are_shared_only_for_the_same_canonical_state_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("state.json");
        let other_path = dir.path().join("other.json");

        let first = WriterCore::gate_for(&state_path);
        let same = WriterCore::gate_for(&state_path);
        let different = WriterCore::gate_for(&other_path);

        assert!(
            Arc::ptr_eq(&first, &same),
            "two writers on the same state path must share one gate"
        );
        assert!(
            !Arc::ptr_eq(&first, &different),
            "different state paths must not share a gate"
        );

        // The canonical parent makes lexically different spellings of
        // the same file share the gate too.
        let alias = dir.path().join(".").join("state.json");
        let aliased = WriterCore::gate_for(&alias);
        assert!(
            Arc::ptr_eq(&first, &aliased),
            "the canonical state-path identity must alias the same gate"
        );
    }

    #[test]
    fn a_dropped_writer_gate_is_not_reused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("state.json");

        let first = WriterCore::gate_for(&state_path);
        let old = Arc::downgrade(&first);
        let same = WriterCore::gate_for(&state_path);
        assert!(
            Arc::ptr_eq(&first, &same),
            "two live writers on the same path share one gate"
        );

        drop(same);
        drop(first);
        assert!(
            old.upgrade().is_none(),
            "the registry must not keep a strong reference to a dropped writer's gate"
        );

        let fresh = WriterCore::gate_for(&state_path);
        assert!(
            old.upgrade().is_none(),
            "a fresh writer after every strong reference dropped gets a new gate"
        );
        assert!(Arc::strong_count(&fresh) >= 1, "the fresh gate is usable");
    }

    #[test]
    #[allow(clippy::panic)] // the test must panic while holding the gate to poison it
    fn a_poisoned_gate_fails_the_save_and_poisons_the_writer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("state.json");
        let other_path = dir.path().join("other.json");

        let (core, _read) = core_for(&state_path, Arc::new(StoreCrashHooks::new()));
        // Poison this live writer's own gate: dropping every strong
        // reference would retire the gate (and its poison) instead.
        let gate = Arc::clone(&core.gate);
        let poisoned = std::thread::spawn(move || {
            let _guard = gate
                .lock()
                .expect("the gate is unpoisoned before this panic");
            panic!("poison the writer gate");
        })
        .join();
        assert!(poisoned.is_err(), "the helper thread panicked as intended");

        let reservation = reserve(&core, &state_with_moves(1));
        let (seq, mut token) = expect_leader(reservation);
        let error = core
            .run_leader(seq, &mut token)
            .expect_err("a poisoned shared gate must fail the save");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(
            error.detail.contains(GATE_POISON_DETAIL),
            "the typed INTERNAL error names the poisoned writer gate: {error}"
        );
        assert!(
            core.lock_writer().poisoned.is_some(),
            "a poisoned gate is a terminal writer poison"
        );

        let (other, _other_read) = core_for(&other_path, Arc::new(StoreCrashHooks::new()));
        let reservation = reserve(&other, &state_with_moves(1));
        let (seq, mut token) = expect_leader(reservation);
        other
            .run_leader(seq, &mut token)
            .expect("a different path's writer is unaffected by the poison");
        assert!(other_path.exists());
    }

    #[test]
    fn leadership_handoff_is_atomic_with_the_empty_pending_check() {
        let fx = fixture();
        let (leader_seq, mut token) = expect_leader(reserve(&fx.core, &state_with_moves(1)));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *fx.core
            .handoff_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(move || {
            entered_tx.send(()).expect("signal the quiesce decision");
            release_rx.recv().expect("wait for the reservation attempt");
        }));
        let core = Arc::clone(&fx.core);
        let leader = std::thread::spawn(move || core.run_leader(leader_seq, &mut token));
        entered_rx
            .recv()
            .expect("the leader must reach the quiesce decision");

        // Attempt a reservation while the leader is inside the quiesce
        // critical section. It blocks on the writer mutex; when it
        // proceeds, leadership must already be released, so it becomes
        // the next leader rather than a waiter with no leader.
        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let core = Arc::clone(&fx.core);
        let reserver = std::thread::spawn(move || {
            attempt_tx.send(()).expect("signal the reservation attempt");
            reserve(&core, &state_with_moves(2))
        });
        attempt_rx.recv().expect("the reserver is about to reserve");
        release_tx
            .send(())
            .expect("release the quiesce critical section");

        leader
            .join()
            .expect("leader thread")
            .expect("the first leader resolves its save");
        let reservation = reserver.join().expect("reserver thread");
        let (seq, mut token) = expect_leader(reservation);
        fx.core
            .run_leader(seq, &mut token)
            .expect("the next leader writes the reservation");
        fx.core.wait_sync(seq).expect("the reservation is durable");
        assert_eq!(
            fx.core.physical_writes.load(Ordering::SeqCst),
            2,
            "both leaders performed exactly one physical write each"
        );
    }

    #[test]
    fn capture_does_not_publish_until_the_leader_proves_durability() {
        let fx = fixture();
        let initial = Arc::clone(&*fx.read.read().expect("read view"));
        let (leader_seq, mut token) = expect_leader(reserve(&fx.core, &state_with_moves(1)));
        assert_eq!(
            *fx.read.read().expect("read view"),
            initial,
            "reserving a capture must not publish it"
        );
        assert!(
            !fx.path.exists(),
            "no durable file exists before the leader writes"
        );
        fx.core
            .run_leader(leader_seq, &mut token)
            .expect("the physical write succeeds");
        assert!(fx.path.exists(), "the leader writes before it publishes");
        assert_eq!(
            *fx.read.read().expect("read view"),
            Arc::new(state_with_moves(1)),
            "publication follows proven durability"
        );
    }

    #[test]
    fn n_pending_captures_coalesce_into_one_physical_write() {
        let fx = fixture();
        let (leader_seq, mut token) = expect_leader(reserve(&fx.core, &state_with_moves(1)));
        let mut waiters = Vec::new();
        for count in 2..=8 {
            let reservation = reserve(&fx.core, &state_with_moves(count));
            assert!(
                matches!(&reservation.role, ReservationRole::Waiter),
                "only the first reservation leads"
            );
            waiters.push(reservation.seq);
        }
        fx.core
            .run_leader(leader_seq, &mut token)
            .expect("the coalesced write succeeds");
        assert_eq!(
            fx.core.physical_writes.load(Ordering::SeqCst),
            1,
            "all pending captures are covered by one physical write"
        );
        for seq in waiters {
            fx.core
                .wait_sync(seq)
                .expect("every covered waiter succeeds");
        }
        let durable = LvmState::load(&fx.path).expect("durable state");
        assert_eq!(
            durable,
            state_with_moves(8),
            "the latest capture is durable"
        );
    }

    #[test]
    fn a_landed_failure_answers_covered_waiters_with_success() {
        let fx = fixture();
        fx.hooks.arm_save_failure(
            volvisor_types::crash::STORE_LVM_STATE,
            StoreSavePoint::AfterRenameBeforeDirFsync,
            0,
        );
        let (leader_seq, mut token) = expect_leader(reserve(&fx.core, &state_with_moves(1)));
        let waiter = reserve(&fx.core, &state_with_moves(2));
        fx.core
            .run_leader(leader_seq, &mut token)
            .expect_err("the leader caller still sees its save error");
        // The rename landed: seq 2 is durable and its waiter succeeds.
        fx.core
            .wait_sync(waiter.seq)
            .expect("a proven-landed covered waiter succeeds");
        let durable = LvmState::load(&fx.path).expect("durable state");
        assert_eq!(
            durable,
            state_with_moves(2),
            "the durable file contains the waiter's mutation"
        );
        let published = Arc::clone(&*fx.read.read().expect("read view"));
        assert_eq!(*published, state_with_moves(2));
        assert!(!fx.core.lock_writer().needs_alignment);
    }

    #[test]
    fn an_unproven_failure_fails_every_waiter_and_publishes_nothing() {
        let fx = fixture();
        fx.hooks.arm_save_failure(
            volvisor_types::crash::STORE_LVM_STATE,
            StoreSavePoint::AfterTmpWrite,
            0,
        );
        let (leader_seq, mut token) = expect_leader(reserve(&fx.core, &state_with_moves(1)));
        let waiter = reserve(&fx.core, &state_with_moves(2));
        let error = fx
            .core
            .run_leader(leader_seq, &mut token)
            .expect_err("the pre-rename failure is not durable");
        assert!(error.detail.contains("AfterTmpWrite"), "{error}");
        let waiter_error = fx
            .core
            .wait_sync(waiter.seq)
            .expect_err("an uncovered waiter must fail, never succeed");
        assert!(
            waiter_error.detail.contains("AfterTmpWrite"),
            "{waiter_error}"
        );
        assert!(
            !fx.path.exists(),
            "a failed pre-rename save publishes no durable state"
        );
        assert_eq!(
            *fx.read.read().expect("read view"),
            Arc::new(LvmState::default()),
            "a failed capture is never published"
        );
        assert!(
            fx.core.lock_writer().needs_alignment,
            "the next admission must align to durable"
        );
        let (encoded, snapshot) = capture(&state_with_moves(3));
        let rejected = fx
            .core
            .reserve(encoded, snapshot)
            .err()
            .expect("new captures are blocked until admission aligns");
        assert!(
            rejected.detail.contains(NEEDS_ALIGNMENT_DETAIL),
            "{rejected}"
        );
    }

    #[test]
    fn an_unreadable_durable_file_fails_closed_without_poison() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"x").expect("blocker file");
        let path = blocker.join("state.json");
        let (core, read) = core_for(&path, Arc::new(StoreCrashHooks::new()));
        let (leader_seq, mut token) = expect_leader(reserve(&core, &state_with_moves(1)));
        let waiter = reserve(&core, &state_with_moves(2));
        let error = core
            .run_leader(leader_seq, &mut token)
            .expect_err("the physical write and its classification both fail");
        assert_eq!(error.code, ApiErrorCode::Internal);
        let waiter_error = core
            .wait_sync(waiter.seq)
            .expect_err("an unreadable durable file fails every waiter");
        assert_eq!(waiter_error.code, ApiErrorCode::Internal);
        {
            let state = core.lock_writer();
            assert!(
                state.poisoned.is_none(),
                "an unreadable durable file must not poison the writer"
            );
            assert!(
                state.needs_alignment,
                "an unreadable durable file requires admission alignment"
            );
        }
        assert_eq!(
            *read.read().expect("read view"),
            Arc::new(LvmState::default())
        );
    }

    #[test]
    fn a_leader_panic_wakes_every_waiter_and_poisons() {
        let fx = fixture();
        fx.hooks.arm(
            volvisor_types::crash::STORE_LVM_STATE,
            StoreSavePoint::AfterTmpWrite,
        );
        let (leader_seq, mut token) = expect_leader(reserve(&fx.core, &state_with_moves(1)));
        let waiter = reserve(&fx.core, &state_with_moves(2));
        let payload = catch_unwind(AssertUnwindSafe(|| {
            fx.core.run_leader(leader_seq, &mut token)
        }))
        .expect_err("the armed crash seam must panic");
        let payload = payload
            .downcast_ref::<String>()
            .expect("the crash payload is the marked string");
        assert!(
            payload.starts_with(CRASH_PANIC_PREFIX),
            "the campaign rig must still see the in-band crash payload: {payload}"
        );
        let error = fx
            .core
            .wait_sync(waiter.seq)
            .expect_err("a leader panic wakes waiters with a typed failure");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(error.detail.contains("durable writer poisoned"), "{error}");
        assert!(
            fx.core.lock_writer().poisoned.is_some(),
            "a leader panic poisons the writer"
        );
    }

    #[test]
    fn an_abandoned_leader_wakes_every_waiter_and_poisons() {
        let fx = fixture();
        let (leader_seq, token) = expect_leader(reserve(&fx.core, &state_with_moves(1)));
        let waiter = reserve(&fx.core, &state_with_moves(2));
        assert_eq!(leader_seq, 1);
        drop(token);
        let error = fx
            .core
            .wait_sync(waiter.seq)
            .expect_err("an abandoned leader fails its waiters");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(fx.core.lock_writer().poisoned.is_some());
    }

    #[test]
    fn a_lower_sequence_never_replaces_a_higher_published_snapshot() {
        let fx = fixture();
        let higher = Arc::new(state_with_moves(5));
        let lower = Arc::new(state_with_moves(2));
        fx.core.publish_if_newer(5, &higher);
        fx.core.publish_if_newer(3, &lower);
        assert_eq!(
            *fx.read.read().expect("read view"),
            higher,
            "a lower sequence must never regress the published view"
        );
        fx.core.publish_if_newer(6, &lower);
        assert_eq!(
            *fx.read.read().expect("read view"),
            lower,
            "a newer sequence publishes"
        );
    }
}
