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
use volvisor_types::domain::{
    AccessMode, EffectiveProtection, EvidenceStatus, FailureDomain, Frontend, Health, Provisioning,
    VolumeClass,
};

use volvisor_types::crash::StoreCrashHooks;
use volvisor_types::request::{
    AttachVolumeRequest, AttachVolumeResponse, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, ErasurePolicy, GrowGuestNotification, GrowVolumeRequest,
    GrowVolumeResponse, InspectVolumeResponse, LocalProtectionModeRequest,
    MoveVolumeBackingRequest, MoveVolumeBackingResponse,
};
use volvisor_types::{
    ApiError, ApiErrorCode, AttachmentId, AttachmentState, Capability, CapabilitySet, DeviceId,
    MoveVolumeBackingState, ProjectId, VolumeId, VolumeLifecycle, validate_api_version,
};

use crate::report::{LvRow, VgRow};
use crate::state::{AttachmentRecord, LvmState, StoredVolume, VolumeEntry, VolumeRuntime};
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
    /// save (every internal mutation routes through here, so the
    /// armed seam sees every commit boundary of the state file).
    fn persist(&self, state: &mut LvmState) -> Result<(), ApiError> {
        state.save_with_hooks(&self.state_path, Some(&self.store_crash))
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
    /// Two passes, both non-destructive towards volumes:
    ///
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
        let present: Vec<String> = self
            .list_lvs()?
            .into_iter()
            .filter_map(|row| row.vg_slash_lv())
            .collect();
        // None = the query failed (honest unknown): keep every claim.
        let observed_vgs: Option<Vec<String>> = match self.list_vgs() {
            Ok(rows) => Some(rows.into_iter().filter_map(|row| row.vg_name).collect()),
            Err(_) => None,
        };
        let mut state = self.lock_state()?;
        let mut changed = false;
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

    // -- Volume operations (sync bodies behind the async trait surface) --

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

        let vg_name = self.pick_pool_vg(&state, req.size_bytes)?;
        let lv_name = lv_name_for(&req.volume_id);
        let output = self.runner.run(
            "lvcreate",
            &[
                "--yes",
                "-L",
                &format!("{}B", req.size_bytes),
                "-n",
                &lv_name,
                &vg_name,
            ],
        )?;
        if !output.success {
            return Err(command_failed("lvcreate", &output));
        }
        // Verify against LVM's own report: a successful exit status is not
        // evidence of the requested geometry. Thick LVM rounds the size up
        // to whole physical extents, so anything at or above the request
        // is a success — the effective size is what gets persisted and
        // reported.
        let effective = self.verify_created_size(&vg_name, &lv_name, req.size_bytes)?;

        let stored = StoredVolume {
            entry: VolumeEntry {
                vg_name,
                lv_name,
                size_bytes: effective,
                requested_size_bytes: req.size_bytes,
                generation: 1,
                data_epoch: 0,
                project_id: req.project_id.clone(),
                block_size: req.logical_block_size.unwrap_or(DEFAULT_BLOCK_SIZE),
                creation_payload: payload,
            },
            runtime: VolumeRuntime {
                state: VolumeLifecycle::Ready,
                attachment: None,
            },
        };
        state.insert_volume(req.volume_id.clone(), stored.clone());
        self.persist(&mut state)?;
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

        // Crash-replay idempotency: an attachment id that is already
        // recorded replays the recorded response (even if the volume
        // generation has since moved) and never fabricates a second
        // attachment; the same id with a different payload conflicts.
        for (recorded_volume, stored) in state.volumes() {
            if let Some(record) = stored.runtime.attachment.as_ref() {
                if record.id == req.attachment_id {
                    if *recorded_volume == *volume_id
                        && record.vm_id == req.vm_id
                        && record.host_id == req.host_id
                        && record.access_mode == mode
                        && record.vmm_disk_id == req.vmm_disk_id
                    {
                        return Ok(attach_response(record, &stored.entry));
                    }
                    return Err(ApiError::idempotency_conflict(&req.attachment_id));
                }
            }
        }

        let stored = state
            .volume_mut(volume_id)
            .ok_or_else(|| not_found(volume_id))?;
        if req.expected_volume_generation != stored.entry.generation {
            return Err(ApiError::stale_generation(
                req.expected_volume_generation,
                stored.entry.generation,
            ));
        }
        if stored.runtime.attachment.is_some() {
            return Err(ApiError::new(
                ApiErrorCode::WriterAlreadyActive,
                format!("volume {volume_id} already has an active attachment"),
            ));
        }
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
        if req.expected_attachment_generation != record.generation {
            return Err(ApiError::stale_generation(
                req.expected_attachment_generation,
                record.generation,
            ));
        }
        // The drain proof (`vm_stopped_or_io_drained_proof`) is a
        // type-mandatory attestation: authority is never released without
        // one, though the provider cannot verify the claim itself
        // (Volume API v2 section 3).

        stored.runtime.attachment = None;
        if stored.runtime.state == VolumeLifecycle::Attached {
            stored.runtime.state = VolumeLifecycle::Ready;
        }
        stored.entry.generation += 1;
        stored.entry.data_epoch += 1;
        let response = inspect_response(volume_id, stored);
        self.persist(&mut state)?;
        Ok(response)
    }

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
        let (vg_name, lv_name, current_size, has_attachment) = {
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
                stored.runtime.attachment.is_some(),
            )
        };

        let capacity = self.vg_capacity(&vg_name)?;
        let delta = req.new_size_bytes - current_size;
        // Thick LVM rounds the grown LV up to whole physical extents, so
        // the space the grow actually consumes is the extent-rounded
        // delta (the current size is already extent-aligned). Comparing
        // the raw delta would under-cover the rounding and surface the
        // over-allocation later as a raw INTERNAL instead of the typed
        // NO_SAFE_CAPACITY.
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
        let output = self.runner.run(
            "lvextend",
            &[
                "--yes",
                "-L",
                &format!("{}B", req.new_size_bytes),
                &format!("{vg_name}/{lv_name}"),
            ],
        )?;
        if !output.success {
            return Err(command_failed("lvextend", &output));
        }
        // Verify the effective size from LVM's own report. Like create,
        // grow rounds up to whole extents: anything at or above the
        // requested size is a success, and the effective size is what
        // gets persisted and reported.
        let actual = self.lv_size(&vg_name, &lv_name)?.ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("lvs no longer reports {vg_name}/{lv_name} after lvextend"),
            )
        })?;
        if actual < req.new_size_bytes {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "LV size mismatch after lvextend of {vg_name}/{lv_name}: requested {} \
                     bytes, LVM reports {actual} bytes",
                    req.new_size_bytes
                ),
            ));
        }

        let stored = state
            .volume_mut(volume_id)
            .ok_or_else(|| not_found(volume_id))?;
        stored.entry.size_bytes = actual;
        stored.entry.generation += 1;
        self.persist(&mut state)?;
        Ok(GrowVolumeResponse {
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
            effective_size_bytes: actual,
        })
    }

    fn delete_volume_inner(
        &self,
        volume_id: &VolumeId,
        req: &DeleteVolumeRequest,
    ) -> Result<(), ApiError> {
        validate_api_version(&req.api_version)?;
        let mut state = self.lock_state()?;
        let (vg_name, lv_name, volume_state) = {
            let stored = state
                .volume_mut(volume_id)
                .ok_or_else(|| not_found(volume_id))?;
            if req.expected_generation != stored.entry.generation {
                return Err(ApiError::stale_generation(
                    req.expected_generation,
                    stored.entry.generation,
                ));
            }
            if stored.runtime.attachment.is_some() {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!("volume {volume_id} must be fully detached before delete"),
                ));
            }
            match stored.runtime.state {
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
            (
                stored.entry.vg_name.clone(),
                stored.entry.lv_name.clone(),
                stored.runtime.state,
            )
        };

        // A Failed volume whose LV is already absent has nothing to
        // remove: check LVM's own report first so a vanished LV does not
        // pin the pool forever (lvremove on a missing LV always fails).
        // For any *other* state an absent LV is unexpected: a Ready
        // volume whose LV is (perhaps only transiently) invisible must
        // never be deleted as if it had been erased — a ZeroDiscard
        // delete would silently skip erasure and leave a foreign LV
        // holding live data — so it fails loudly instead. LVs that exist
        // but fail to remove keep their typed-error path below.
        let lv_exists = self.lv_size(&vg_name, &lv_name)?.is_some();
        if !lv_exists && volume_state != VolumeLifecycle::Failed {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "LV {vg_name}/{lv_name} unexpectedly absent for volume in state \
                     {volume_state:?}"
                ),
            ));
        }
        if lv_exists {
            if matches!(req.data_erasure_policy, ErasurePolicy::ZeroDiscard) {
                let device = format!("/dev/{vg_name}/{lv_name}");
                let output = self.runner.run("blkdiscard", &["-f", &device])?;
                if !output.success {
                    // Never proceed to lvremove after a failed discard: the
                    // caller asked for erased data and must learn the truth.
                    return Err(command_failed("blkdiscard", &output));
                }
            }
            let output = self
                .runner
                .run("lvremove", &["--yes", &format!("{vg_name}/{lv_name}")])?;
            if !output.success {
                return Err(command_failed("lvremove", &output));
            }
        }
        state.remove_volume(volume_id);
        self.persist(&mut state)?;
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
    fn park_in_doubt(
        &self,
        volume_id: &VolumeId,
        detail: String,
    ) -> Result<MoveVolumeBackingResponse, ApiError> {
        let mut state = self.lock_state()?;
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
        let Some(stored) = state.volume_mut(volume_id) else {
            // The volume was deleted while the move ran (the delete
            // path's own lvremove would have refused a moving LV, so
            // this is the settled tail): nothing to complete.
            state.remove_move(volume_id);
            self.persist(&mut state)?;
            return Err(not_found(volume_id));
        };
        stored.entry.generation += 1;
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
        self.persist(&mut state)?;
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
    /// target or an `IN_DOUBT` park refuses). Nothing is journaled
    /// and nothing in the world is touched here.
    fn admit_move(
        &self,
        volume_id: &VolumeId,
        req: &MoveVolumeBackingRequest,
    ) -> Result<MoveAdmission, ApiError> {
        let state = self.lock_state()?;
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
                        // already completed.
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
                _ => Ok(MoveAdmission::Drive {
                    vg_name: names.0,
                    lv_name: names.1,
                    existing_target: None,
                }),
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

        // Phase 3 — journal the intent. The record is the
        // reconciliation anchor: after this save, a daemon death
        // anywhere below leaves a durable fact to classify against.
        // A re-attach reusing an unchanged record saves nothing —
        // an unchanged state has no durable boundary to write, and
        // the operation_id refresh lands with phase 4's COPYING
        // save.
        let (source_pv, record_for_drive) = {
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
                    let source = source_pv.clone().unwrap_or_default();
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
                self.persist(&mut state)?;
            }
            (source, record)
        };

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
            self.persist(&mut state)?;
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
        if let Some(record) = state.move_record(volume_id) {
            if record.state == MoveVolumeBackingState::Preparing {
                state.remove_move(volume_id);
                self.persist(&mut state)?;
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
    /// Never destructive: an unobservable record is skipped (retried
    /// next tick), an unrelocated `COPYING` record parks `IN_DOUBT`,
    /// and nothing but a verified relocation ever completes.
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
                report.dropped.push(volume_id);
                continue;
            };
            if record.state == MoveVolumeBackingState::Complete {
                // A completed record is a historical fact: the world
                // moving on afterwards is a new move for a new
                // consumer request, never a re-completion (and never
                // a second generation bump).
                continue;
            }
            let Ok(observation) = self.observe_lv(&vg_name, &lv_name) else {
                // Honest unknown: leave the record untouched,
                // retry on the next pass.
                report.unobservable.push(volume_id);
                continue;
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
                    // The move ended without relocating: park.
                    self.park_in_doubt(
                        &volume_id,
                        format!(
                            "the pvmove ended without relocating the extents to {} \
                             (aborted out-of-band or failed); the source PV {} is intact \
                             and serving",
                            record.target_pv, record.source_pv
                        ),
                    )?;
                    report.parked_in_doubt.push(volume_id);
                }
                _ => {}
            }
        }
        Ok(report)
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
        current_writer: attachment
            .filter(|a| a.access_mode == AccessMode::SingleWriter)
            .map(|a| a.id.clone()),
        backend_health: Health::Unknown,
        evidence_status: EvidenceStatus::PrototypeOnly,
        authority: None,
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
        // No VMM integration exists: evidence honestly starts at Prepared.
        state: AttachmentState::Prepared,
    }
}
