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
use volvisor_provider::VolumeProvider;
use volvisor_types::domain::{
    AccessMode, EffectiveProtection, EvidenceStatus, FailureDomain, Frontend, Health, Provisioning,
    VolumeClass,
};
use volvisor_types::request::{
    AttachVolumeRequest, AttachVolumeResponse, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, ErasurePolicy, GrowGuestNotification, GrowVolumeRequest,
    GrowVolumeResponse, InspectVolumeResponse, LocalProtectionModeRequest,
};
use volvisor_types::{
    ApiError, ApiErrorCode, AttachmentId, AttachmentState, Capability, CapabilitySet, DeviceId,
    ProjectId, VolumeId, VolumeLifecycle, validate_api_version,
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
    state: Mutex<LvmState>,
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
            state: Mutex::new(state),
        };
        provider.reconcile()?;
        Ok(provider)
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
            state.save(&self.state_path)?;
        }
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
        state.save(&self.state_path)?;
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
        };
        stored.runtime.attachment = Some(record.clone());
        stored.runtime.state = VolumeLifecycle::Attached;
        stored.entry.generation += 1;
        let response = attach_response(&record, &stored.entry);
        state.save(&self.state_path)?;
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
        state.save(&self.state_path)?;
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
        state.save(&self.state_path)?;
        Ok(GrowVolumeResponse {
            backing_resized: true,
            // No VMM integration exists: an attached frontend still needs a
            // (retried) notification; a detached volume has nobody to
            // notify. Never `Notified`.
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
        state.save(&self.state_path)?;
        Ok(())
    }
}

#[async_trait]
impl VolumeProvider for LvmProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet::native_local_p0()
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
