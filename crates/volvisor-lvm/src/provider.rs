//! The native-local LVM provider: thick logical volumes on claimed disks.
//!
//! [`LvmProvider`] implements [`VolumeProvider`] for
//! [`VolumeClass::NativeLocal`]: volumes are thick LVs created with
//! `lvcreate` on volume groups established by an explicit
//! [`claim_device`](crate::admin) under a destructive-authorization token.
//! Every LVM interaction goes through the shell-free
//! [`CommandRunner`](crate::runner); every mutation is persisted to the
//! durable JSON state after the backend confirmed it, and sizes are always
//! verified against `lvs` output — a successful exit status alone is never
//! trusted as evidence (honest reporting).
//!
//! P0 honesty constraints: attach returns a `Prepared` (never `Active`)
//! frontend handle because no VMM integration exists; health and backend
//! health are `Unknown` until proven; `evidence_status` is
//! `PrototypeOnly`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
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
    ApiError, ApiErrorCode, AttachmentId, AttachmentState, Capability, CapabilitySet, ProjectId,
    VolumeId, VolumeLifecycle, validate_api_version,
};

use crate::report::{LvRow, VgRow};
use crate::runner::{CommandOutput, CommandRunner};
use crate::state::{AttachmentRecord, LvmState, StoredVolume, VolumeEntry, VolumeRuntime};

/// Stable provider name for diagnostics (never a secret).
pub const PROVIDER_NAME: &str = "lvm-native-local";

/// The only volume class served by this provider.
static SUPPORTED_CLASSES: &[VolumeClass] = &[VolumeClass::NativeLocal];

/// Logical block size assumed when a create request omits one.
const DEFAULT_BLOCK_SIZE: u32 = 4096;

/// Free-space headroom kept in every volume group so LVM metadata can
/// always be written (`NO_SAFE_CAPACITY` is reported before the VG fills).
pub(crate) const VG_HEADROOM_BYTES: u64 = 4 << 20;

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
    /// immediately reconciles it against real LVM state, marking volumes
    /// whose LV vanished as `Failed`. `vg_prefix` namespaces the volume
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
    pub(crate) fn list_vgs(&self) -> Result<Vec<VgRow>, ApiError> {
        let output = self.runner.run("vgs", lvm_json_args())?;
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

    /// Free bytes per volume group name, from one `vgs` query.
    fn vg_free_map(&self) -> Result<BTreeMap<String, u64>, ApiError> {
        let mut map = BTreeMap::new();
        for row in self.list_vgs()? {
            if let (Some(name), Some(free)) = (row.vg_name.clone(), row.free_bytes()) {
                map.insert(name, free);
            }
        }
        Ok(map)
    }

    /// Free bytes of one volume group; `INTERNAL` (honest) when `vgs` does
    /// not report the group at all.
    fn vg_free(&self, vg_name: &str) -> Result<u64, ApiError> {
        self.vg_free_map()?.get(vg_name).copied().ok_or_else(|| {
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

    /// Pick the first claimed native-pool volume group with enough free
    /// space (headroom included); `NO_SAFE_CAPACITY` when none qualifies.
    fn pick_pool_vg(&self, state: &LvmState, size_bytes: u64) -> Result<String, ApiError> {
        let free_map = self.vg_free_map()?;
        for entry in state.devices().values() {
            if entry.role != volvisor_types::DeviceRole::NativePool {
                continue;
            }
            if let Some(free) = free_map.get(&entry.vg_name) {
                if size_bytes.saturating_add(VG_HEADROOM_BYTES) <= *free {
                    return Ok(entry.vg_name.clone());
                }
            }
        }
        Err(ApiError::new(
            ApiErrorCode::NoSafeCapacity,
            format!(
                "no safe capacity: no claimed native-local pool has {size_bytes} bytes \
                 (plus headroom) free"
            ),
        ))
    }

    /// Startup reconciliation.
    ///
    /// A state entry whose LV is absent from `lvs` output is marked
    /// `Failed` and persisted (its data is gone or the VG was removed —
    /// the volume is never silently recreated). Foreign LVs under our
    /// volume groups are not touched here; they are surfaced by
    /// [`reconcile_report`](crate::admin).
    fn reconcile(&self) -> Result<(), ApiError> {
        let present: Vec<String> = self
            .list_lvs()?
            .into_iter()
            .filter_map(|row| row.vg_slash_lv())
            .collect();
        let mut state = self.lock_state()?;
        let mut changed = false;
        for volume in state.volumes_mut() {
            let key = format!("{}/{}", volume.entry.vg_name, volume.entry.lv_name);
            if !present.contains(&key) && volume.runtime.state != VolumeLifecycle::Failed {
                volume.runtime.state = VolumeLifecycle::Failed;
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
        // current state; a different payload is a typed conflict.
        if let Some(existing) = state.volume(&req.volume_id) {
            if existing.entry.creation_payload == payload {
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
        // evidence of the requested geometry.
        let actual = self.lv_size(&vg_name, &lv_name)?.ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("lvcreate reported success but lvs does not list {vg_name}/{lv_name}"),
            )
        })?;
        if actual != req.size_bytes {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "LV size mismatch after lvcreate of {vg_name}/{lv_name}: requested {} \
                     bytes, LVM reports {actual} bytes",
                    req.size_bytes
                ),
            ));
        }

        let stored = StoredVolume {
            entry: VolumeEntry {
                vg_name,
                lv_name,
                size_bytes: req.size_bytes,
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

        let free = self.vg_free(&vg_name)?;
        let delta = req.new_size_bytes - current_size;
        if delta.saturating_add(VG_HEADROOM_BYTES) > free {
            return Err(ApiError::new(
                ApiErrorCode::NoSafeCapacity,
                format!("grow needs {delta} more bytes, {free} free in {vg_name}"),
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
        // Verify the effective size from LVM's own report.
        let actual = self.lv_size(&vg_name, &lv_name)?.ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("lvs no longer reports {vg_name}/{lv_name} after lvextend"),
            )
        })?;
        if actual != req.new_size_bytes {
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
        stored.entry.size_bytes = req.new_size_bytes;
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
        let (vg_name, lv_name) = {
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
            (stored.entry.vg_name.clone(), stored.entry.lv_name.clone())
        };

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

/// The LV name for a volume identity: `.` and `:` are not safe in all LVM
/// tooling contexts, so they are replaced with `-` (the ID charset
/// otherwise consists of `[A-Za-z0-9_.:-]`).
fn lv_name_for(volume_id: &VolumeId) -> String {
    volume_id
        .as_str()
        .chars()
        .map(|c| if matches!(c, '.' | ':') { '-' } else { c })
        .collect()
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
        provisioned_bytes: stored.entry.size_bytes,
        // Thick LVs: allocation equals provisioning.
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
