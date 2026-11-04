//! Durable JSON state for the LVM provider.
//!
//! The state file is the provider-internal cross-reference store: which
//! volume identity maps to which `vg/lv`, the single recorded attachment
//! per volume, and which devices have been claimed into which volume
//! groups. It is never exposed to tenants (the API surface derives all
//! responses from it).
//!
//! Persistence is atomic: [`LvmState::save`] writes `<path>.tmp`, fsyncs
//! the file, renames it over the target and fsyncs the parent directory,
//! so a crash can never leave a torn or half-renamed state file. A missing
//! file loads as an empty state (first start).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};
use volvisor_types::crash::{STORE_LVM_STATE, StoreCrashHooks, StoreSavePoint};
use volvisor_types::request::ErasurePolicy;
use volvisor_types::schema::{SchemaVersionProbe, ensure_supported_schema_version};
use volvisor_types::{
    AccessMode, ApiError, ApiErrorCode, AttachmentEvidence, AttachmentId, AttachmentState,
    DeviceId, DeviceRole, HostId, MoveVolumeBackingState, OperationId, ProjectId, VolumeId,
    VolumeLifecycle,
};

/// Current durable LVM-state schema version.
///
/// Version 1 is the baseline (including the pre-versioning format; a
/// missing `schema_version` decodes as
/// [`volvisor_types::BASELINE_SCHEMA_VERSION`]). Version 2 adds the
/// durable `pending_creates`/`pending_grows`/`pending_deletes` intent
/// maps introduced for issue #70. Because that is an additive
/// serialized-shape change, the version is bumped per the
/// [durable-state schema compatibility policy](../../../docs/schema-compatibility.md):
/// v1 files still load (the maps default empty) and are upgraded on
/// the first successful save, while an older binary refuses a v2 file
/// with the typed unsupported-version error instead of failing on an
/// unknown field.
pub const LVM_STATE_SCHEMA_VERSION: u32 = 2;

/// The volume→LV cross-reference plus the durable volume attributes.
///
/// `lv_name` is derived from the `volume_id` (sanitized plus a hash
/// suffix over the full id, so uniqueness rests on that 32-bit suffix
/// rather than the sanitized segment itself);
/// `vg_name` identifies the claimed pool the volume lives in (a
/// provider-internal reference, never exposed to tenants).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VolumeEntry {
    /// Volume group holding the logical volume.
    pub vg_name: String,
    /// Logical volume name (the sanitized volume identity).
    pub lv_name: String,
    /// Effective (provisioned) size in bytes: what `lvs` reported after
    /// LVM rounded the request up to whole physical extents.
    pub size_bytes: u64,
    /// The size the caller originally requested, in bytes. Thick LVM
    /// rounds sizes up to the physical extent, so this can be smaller
    /// than `size_bytes`; idempotent-create replay compares *this*
    /// value (never the rounded one) against a replayed request.
    ///
    /// The initial P1 format had no requested-vs-effective distinction.
    /// It decodes with this field equal to `size_bytes` (the only size
    /// that file recorded), never with a guessed or zero value.
    pub requested_size_bytes: u64,
    /// Volume generation (optimistic concurrency fencing).
    pub generation: u64,
    /// Writer epoch / data epoch for authority reasoning.
    pub data_epoch: u64,
    /// Owning tenant project (needed to answer inspect/list).
    pub project_id: ProjectId,
    /// Logical block size in bytes.
    pub block_size: u32,
    /// Canonical creation payload with `operation_id` normalized out;
    /// used to detect idempotent replays vs. payload conflicts.
    pub creation_payload: String,
}

/// Wire shape for [`VolumeEntry`]; differs only in the one legacy field.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VolumeEntryWire {
    vg_name: String,
    lv_name: String,
    size_bytes: u64,
    #[serde(default)]
    requested_size_bytes: Option<u64>,
    generation: u64,
    data_epoch: u64,
    project_id: ProjectId,
    block_size: u32,
    creation_payload: String,
}

impl<'de> Deserialize<'de> for VolumeEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = VolumeEntryWire::deserialize(deserializer)?;
        Ok(Self {
            vg_name: wire.vg_name,
            lv_name: wire.lv_name,
            size_bytes: wire.size_bytes,
            // P1 files recorded only the effective size. That is the
            // honest requested size for them; an absent field never
            // becomes a fabricated zero.
            requested_size_bytes: wire.requested_size_bytes.unwrap_or(wire.size_bytes),
            generation: wire.generation,
            data_epoch: wire.data_epoch,
            project_id: wire.project_id,
            block_size: wire.block_size,
            creation_payload: wire.creation_payload,
        })
    }
}

/// The mutable runtime portion of a stored volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeRuntime {
    /// Current lifecycle state.
    pub state: VolumeLifecycle,
    /// The single recorded attachment, if any (single-writer P0 profile).
    pub attachment: Option<AttachmentRecord>,
    /// Attachment ids this volume consumed earlier in its lifetime. A
    /// report-capable provider refuses to reuse one: per-id evidence
    /// (`state_revision`) must never be applied to a different instance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired_attachment_ids: Vec<AttachmentId>,
}

/// A volume as stored in provider state: durable entry + runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredVolume {
    /// Durable cross-reference and attributes.
    pub entry: VolumeEntry,
    /// Lifecycle state and attachment record.
    pub runtime: VolumeRuntime,
}

/// A recorded attachment (crash-replay never fabricates a second one).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentRecord {
    /// Attachment identity (caller-chosen, opaque).
    pub id: AttachmentId,
    /// Consuming VM identity.
    pub vm_id: String,
    /// Host the attachment is scoped to.
    pub host_id: HostId,
    /// Attachment generation.
    pub generation: u64,
    /// Granted access mode.
    pub access_mode: AccessMode,
    /// The VMM-side disk id the consumer configured for this
    /// frontend, when it configured one (P6-B): the durable mapping
    /// a grow's capacity notification addresses the VMM's
    /// resize-disk call with. Absent on an attached volume is a
    /// recorded, fail-closed refusal reason at notification time —
    /// never a guess. Optional and defaulted so state files written
    /// before P6-B load unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vmm_disk_id: Option<String>,
    /// Current evidence state (contract section 3). Optional/defaulted so
    /// state files written before consumer reporting load as `prepared`.
    #[serde(default)]
    pub state: AttachmentState,
    /// Monotonic evidence revision; advances only on an applied report.
    /// Defaulted to the initial revision for pre-report state files.
    #[serde(default = "volvisor_types::domain::initial_state_revision")]
    pub state_revision: u64,
    /// Evidence basis of the current state; absent for `prepared`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<AttachmentEvidence>,
    /// Consumer that reported the current state (untrusted label).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reporter: Option<String>,
    /// Server-assigned receipt time of the current state, Unix ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at_ms: Option<u64>,
}

/// A claimed physical device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceEntry {
    /// Stable hardware identity (WWN/serial) recorded at claim time.
    pub stable_identity: String,
    /// Device path used for `pvcreate`/`pvremove` at claim time.
    pub path: String,
    /// Volume group created on this device.
    pub vg_name: String,
    /// Role the device is claimed for (exactly one, SPEC-0002 section 3).
    pub role: DeviceRole,
    /// Monotonic ownership generation.
    pub owner_generation: u64,
}

/// One journaled online move of a volume's backing extents
/// (contract section 4A, `same_vg_extent_move` scope).
///
/// The record is the **reconciliation anchor**: the daemon can die
/// at any boundary of the move, and the kernel-side `pvmove` mirror
/// plus this record are what survive. The record's state is the
/// honest subset of [`MoveVolumeBackingState`] — `PREPARING`,
/// `COPYING`, `COMPLETE`, `IN_DOUBT`, and `FAILED` written by the
/// retry reconcile **alone** (the verified-rollback resolution of a
/// parked `IN_DOUBT`: the extents verifiably back on the source, the
/// relocation did not happen) — never `MIRROR_READY`/`PIVOTED`
/// (mirror-path states; a same-VG extent move never pivots, the
/// LV's dm identity is stable) and never `FAILED` from a drive (an
/// unknown outcome is `IN_DOUBT`; deterministic rejections are
/// typed errors, not states).
///
/// Keyed by volume: at most one move record exists per volume. A
/// `COMPLETE` record for the same source/target answers re-issues
/// idempotently; an `IN_DOUBT` record parks further moves typed
/// until reconciliation proves completion, resolves the verified
/// rollback, or an operator resolves it; a `FAILED` record admits a
/// fresh drive (the new record supersedes it at the drive's
/// `PREPARING` journal write).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveRecord {
    /// The operation that started (or most recently re-attached to)
    /// this move — diagnostics only; the API-layer idempotency
    /// journal is the replay authority.
    pub operation_id: OperationId,
    /// The PV the extents are being evacuated from.
    pub source_pv: String,
    /// The PV the extents are being evacuated to.
    pub target_pv: String,
    /// The move's current state (the honest subset — see the type
    /// documentation).
    pub state: MoveVolumeBackingState,
    /// Honest detail for non-complete states (the `IN_DOUBT` reason,
    /// or the supervision note for `COPYING`).
    pub detail: Option<String>,
}

/// A durable intent for a create whose `lvcreate` may already have
/// run (the kill-9 window between the CLI success and the volume
/// record's commit).
///
/// Written and fsynced **before** `lvcreate`. If the daemon dies after
/// the CLI created the LV but before the volume record is durable,
/// startup recovery adopts the LV only when it is present and at
/// least the recorded requested size; a missing LV drops the intent,
/// and a smaller LV fails startup rather than adopting a wrong
/// geometry. The payload and requested size are the idempotent-replay
/// comparison for a retried create.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateIntent {
    /// Volume group the LV was created in.
    pub vg_name: String,
    /// Logical volume name (must equal the id-derived name).
    pub lv_name: String,
    /// The size the caller requested, in bytes (pre-extent-rounding).
    pub requested_size_bytes: u64,
    /// Owning tenant project.
    pub project_id: ProjectId,
    /// Logical block size in bytes.
    pub block_size: u32,
    /// Canonical creation payload with `operation_id` normalized out;
    /// a retried create must match this and the requested size.
    pub creation_payload: String,
}

/// A durable intent for an online grow between the capacity check and
/// the final volume-record commit.
///
/// Written and fsynced **before** `lvextend`. Recovery rolls the grow
/// forward: an LV at or above the requested size completes the record
/// (generation bump) and an LV still at the from-size drops the
/// intent (the grow never landed). An LV in between fails startup
/// rather than guessing. The provider never shrinks an LV as
/// compensation — data may already have been written into the new
/// extents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrowIntent {
    /// The operation that authorized the grow.
    pub operation_id: OperationId,
    /// The volume size before the grow, in bytes.
    pub from_size_bytes: u64,
    /// The size the grow requested, in bytes.
    pub requested_size_bytes: u64,
    /// The volume generation the grow was authorized against.
    pub expected_generation: u64,
}

/// A durable intent for a delete between the non-destructive
/// validation and the volume record's removal.
///
/// Written and fsynced **before** `blkdiscard`/`lvremove`. Recovery
/// completes the delete when the stored generation still matches the
/// authorization; a stale-generation intent is dropped without any
/// deletion (never delete on a stale authorization). Foreign LVs are
/// never touched — recovery only acts on the intent's own recorded
/// volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteIntent {
    /// The operation that authorized the delete.
    pub operation_id: OperationId,
    /// The volume generation the delete was authorized against.
    pub expected_generation: u64,
    /// The erasure policy the delete must honor.
    pub erasure_policy: ErasurePolicy,
}

/// The whole durable provider state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LvmState {
    /// Durable schema version; absent in pre-versioning files (baseline 1).
    #[serde(default = "volvisor_types::baseline_schema_version")]
    schema_version: u32,
    volumes: BTreeMap<VolumeId, StoredVolume>,
    devices: BTreeMap<DeviceId, DeviceEntry>,
    /// Online-move records, keyed by volume (at most one per
    /// volume). Defaulted so state files written before P6-C load
    /// unchanged.
    #[serde(default)]
    moves: BTreeMap<VolumeId, MoveRecord>,
    /// Durable create intents, keyed by volume. Added in schema v2;
    /// defaulted so v1 files load unchanged and skipped when empty so
    /// a state with no pending create serializes without the map.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pending_creates: BTreeMap<VolumeId, CreateIntent>,
    /// Durable grow intents, keyed by volume. Added in schema v2;
    /// defaulted so v1 files load unchanged and skipped when empty so
    /// a state with no pending grow serializes without the map.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pending_grows: BTreeMap<VolumeId, GrowIntent>,
    /// Durable delete intents, keyed by volume. Added in schema v2;
    /// defaulted so v1 files load unchanged and skipped when empty so
    /// a state with no pending delete serializes without the map.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pending_deletes: BTreeMap<VolumeId, DeleteIntent>,
}

impl Default for LvmState {
    fn default() -> Self {
        Self {
            schema_version: LVM_STATE_SCHEMA_VERSION,
            volumes: BTreeMap::new(),
            devices: BTreeMap::new(),
            moves: BTreeMap::new(),
            pending_creates: BTreeMap::new(),
            pending_grows: BTreeMap::new(),
            pending_deletes: BTreeMap::new(),
        }
    }
}

impl LvmState {
    /// Load the state from `path`.
    ///
    /// A missing file is an empty state (first start). A corrupt file, an
    /// unknown field at a supported version, or a version newer than
    /// [`LVM_STATE_SCHEMA_VERSION`] is a typed `INTERNAL` error — state is
    /// never guessed, partially decoded or reset silently.
    pub fn load(path: &Path) -> Result<Self, ApiError> {
        let path_display = path.display();
        match fs::read_to_string(path) {
            Ok(text) => {
                // Probe the version with unknown fields tolerated, so a file
                // from a newer build gets the precise unsupported-version
                // refusal rather than a generic parse error.
                let probe: SchemaVersionProbe = serde_json::from_str(&text).map_err(|e| {
                    ApiError::new(
                        ApiErrorCode::Internal,
                        format!("failed to parse provider state {path_display}: {e}"),
                    )
                })?;
                ensure_supported_schema_version(
                    "lvm",
                    path,
                    probe.schema_version,
                    LVM_STATE_SCHEMA_VERSION,
                )?;
                serde_json::from_str(&text).map_err(|e| {
                    ApiError::new(
                        ApiErrorCode::Internal,
                        format!("failed to parse provider state {path_display}: {e}"),
                    )
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ApiError::new(
                ApiErrorCode::Internal,
                format!("failed to read provider state {path_display}: {e}"),
            )),
        }
    }

    /// Persist the state atomically.
    ///
    /// Takes `&mut self` to mark the intent to persist a mutation: write
    /// `<path>.tmp` (mode `0600` on unix — the state names devices and
    /// volume placements, so it is owner-only), fsync, rename over
    /// `path`, fsync the directory. If any step fails, the temporary
    /// file is removed and an `INTERNAL` error is returned; the previous
    /// state file remains intact.
    pub fn save(&mut self, path: &Path) -> Result<(), ApiError> {
        self.save_with_hooks(path, None)
    }

    /// [`Self::save`] with the store-save crash seam armed: the
    /// boundaries (after the tmp content write, after its fsync,
    /// after the rename, and after the rename before the directory
    /// fsync) consult `crash` exactly like the grow store's saves, so
    /// a rig can kill the daemon at any commit split of a
    /// move-record mutation, or inject a non-kill save error at the
    /// directory-fsync boundary. The seam is inert unless the rig
    /// armed this store's points.
    pub fn save_with_hooks(
        &mut self,
        path: &Path,
        crash: Option<&StoreCrashHooks>,
    ) -> Result<(), ApiError> {
        let tmp_path = sibling_tmp_path(path);
        let result = self.save_to(&tmp_path, path, crash);
        if result.is_err() {
            // Best-effort cleanup: never leave a stale .tmp behind.
            drop(fs::remove_file(&tmp_path));
        }
        result
    }

    fn save_to(
        &mut self,
        tmp_path: &Path,
        path: &Path,
        crash: Option<&StoreCrashHooks>,
    ) -> Result<(), ApiError> {
        let internal = |detail: String| ApiError::new(ApiErrorCode::Internal, detail);
        let tmp_display = tmp_path.display();
        let path_display = path.display();
        let consult = |point: StoreSavePoint| -> Result<(), ApiError> {
            if let Some(crash) = crash {
                // The kill seam first (it never returns), then the
                // rig-only injected error, which makes the save return
                // a typed error instead of dying.
                crash.consult(STORE_LVM_STATE, point);
                if crash.take_save_failure(STORE_LVM_STATE, point) {
                    return Err(internal(format!(
                        "injected state save failure for {STORE_LVM_STATE} at {point:?}"
                    )));
                }
            }
            Ok(())
        };
        // A successful open may have decoded an older file; every save
        // writes the current version so an upgrade becomes durable on the
        // first mutation (or the startup reconcile's first save).
        self.schema_version = LVM_STATE_SCHEMA_VERSION;
        let data = serde_json::to_vec_pretty(self)
            .map_err(|e| internal(format!("failed to serialize provider state: {e}")))?;
        let mut file = create_owner_only(tmp_path)
            .map_err(|e| internal(format!("failed to create {tmp_display}: {e}")))?;
        file.write_all(&data)
            .map_err(|e| internal(format!("failed to write {tmp_display}: {e}")))?;
        consult(StoreSavePoint::AfterTmpWrite)?;
        file.sync_all()
            .map_err(|e| internal(format!("failed to fsync {tmp_display}: {e}")))?;
        drop(file);
        consult(StoreSavePoint::AfterFsyncBeforeRename)?;
        fs::rename(tmp_path, path).map_err(|e| {
            internal(format!(
                "failed to rename {tmp_display} to {path_display}: {e}"
            ))
        })?;
        consult(StoreSavePoint::AfterRename)?;
        // The directory-fsync boundary: the rename has landed, so a
        // rig-injected failure here leaves the NEW file as the durable
        // state while still exercising the caller's error path.
        consult(StoreSavePoint::AfterRenameBeforeDirFsync)?;
        // fsync the directory so the rename itself is durable.
        let dir = fs::File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
        .map_err(|e| internal(format!("failed to open parent of {path_display}: {e}")))?;
        dir.sync_all()
            .map_err(|e| internal(format!("failed to fsync parent of {path_display}: {e}")))?;
        Ok(())
    }

    /// All stored volumes, ordered by volume identity.
    #[must_use]
    pub fn volumes(&self) -> &BTreeMap<VolumeId, StoredVolume> {
        &self.volumes
    }

    /// Look up one stored volume.
    #[must_use]
    pub fn volume(&self, id: &VolumeId) -> Option<&StoredVolume> {
        self.volumes.get(id)
    }

    /// Look up one stored volume for mutation.
    pub fn volume_mut(&mut self, id: &VolumeId) -> Option<&mut StoredVolume> {
        self.volumes.get_mut(id)
    }

    /// Insert or replace a stored volume.
    pub fn insert_volume(&mut self, id: VolumeId, volume: StoredVolume) -> Option<StoredVolume> {
        self.volumes.insert(id, volume)
    }

    /// Remove a stored volume.
    pub fn remove_volume(&mut self, id: &VolumeId) -> Option<StoredVolume> {
        self.volumes.remove(id)
    }

    /// Iterate all stored volumes mutably.
    pub fn volumes_mut(&mut self) -> impl Iterator<Item = &mut StoredVolume> {
        self.volumes.values_mut()
    }

    /// All claimed devices, ordered by device identity.
    #[must_use]
    pub fn devices(&self) -> &BTreeMap<DeviceId, DeviceEntry> {
        &self.devices
    }

    /// Look up one claimed device.
    #[must_use]
    pub fn device(&self, id: &DeviceId) -> Option<&DeviceEntry> {
        self.devices.get(id)
    }

    /// Insert or replace a claimed device.
    pub fn insert_device(&mut self, id: DeviceId, entry: DeviceEntry) -> Option<DeviceEntry> {
        self.devices.insert(id, entry)
    }

    /// Remove a claimed device.
    pub fn remove_device(&mut self, id: &DeviceId) -> Option<DeviceEntry> {
        self.devices.remove(id)
    }

    /// All online-move records, ordered by volume identity.
    #[must_use]
    pub fn moves(&self) -> &BTreeMap<VolumeId, MoveRecord> {
        &self.moves
    }

    /// Look up one volume's move record.
    #[must_use]
    pub fn move_record(&self, id: &VolumeId) -> Option<&MoveRecord> {
        self.moves.get(id)
    }

    /// Look up one volume's move record for mutation.
    pub fn move_record_mut(&mut self, id: &VolumeId) -> Option<&mut MoveRecord> {
        self.moves.get_mut(id)
    }

    /// Insert or replace one volume's move record.
    pub fn insert_move(&mut self, id: VolumeId, record: MoveRecord) -> Option<MoveRecord> {
        self.moves.insert(id, record)
    }

    /// Remove one volume's move record (a failed start with a
    /// verified-untouched world leaves nothing to reconcile).
    pub fn remove_move(&mut self, id: &VolumeId) -> Option<MoveRecord> {
        self.moves.remove(id)
    }

    /// All pending create intents, ordered by volume identity.
    #[must_use]
    pub fn create_intents(&self) -> &BTreeMap<VolumeId, CreateIntent> {
        &self.pending_creates
    }

    /// Look up one volume's pending create intent.
    #[must_use]
    pub fn create_intent(&self, id: &VolumeId) -> Option<&CreateIntent> {
        self.pending_creates.get(id)
    }

    /// Insert or replace one volume's pending create intent.
    pub fn insert_create_intent(
        &mut self,
        id: VolumeId,
        intent: CreateIntent,
    ) -> Option<CreateIntent> {
        self.pending_creates.insert(id, intent)
    }

    /// Remove one volume's pending create intent.
    pub fn remove_create_intent(&mut self, id: &VolumeId) -> Option<CreateIntent> {
        self.pending_creates.remove(id)
    }

    /// All pending grow intents, ordered by volume identity.
    #[must_use]
    pub fn grow_intents(&self) -> &BTreeMap<VolumeId, GrowIntent> {
        &self.pending_grows
    }

    /// Look up one volume's pending grow intent.
    #[must_use]
    pub fn grow_intent(&self, id: &VolumeId) -> Option<&GrowIntent> {
        self.pending_grows.get(id)
    }

    /// Insert or replace one volume's pending grow intent.
    pub fn insert_grow_intent(&mut self, id: VolumeId, intent: GrowIntent) -> Option<GrowIntent> {
        self.pending_grows.insert(id, intent)
    }

    /// Remove one volume's pending grow intent.
    pub fn remove_grow_intent(&mut self, id: &VolumeId) -> Option<GrowIntent> {
        self.pending_grows.remove(id)
    }

    /// All pending delete intents, ordered by volume identity.
    #[must_use]
    pub fn delete_intents(&self) -> &BTreeMap<VolumeId, DeleteIntent> {
        &self.pending_deletes
    }

    /// Look up one volume's pending delete intent.
    #[must_use]
    pub fn delete_intent(&self, id: &VolumeId) -> Option<&DeleteIntent> {
        self.pending_deletes.get(id)
    }

    /// Insert or replace one volume's pending delete intent.
    pub fn insert_delete_intent(
        &mut self,
        id: VolumeId,
        intent: DeleteIntent,
    ) -> Option<DeleteIntent> {
        self.pending_deletes.insert(id, intent)
    }

    /// Remove one volume's pending delete intent.
    pub fn remove_delete_intent(&mut self, id: &VolumeId) -> Option<DeleteIntent> {
        self.pending_deletes.remove(id)
    }
}

/// The `<path>.tmp` sibling used for atomic saves.
fn sibling_tmp_path(path: &Path) -> PathBuf {
    let mut os_name = path.as_os_str().to_owned();
    os_name.push(".tmp");
    PathBuf::from(os_name)
}

/// Create (or truncate) `path` for writing with owner-only permissions.
///
/// On unix the file is created with mode `0600`; other platforms fall
/// back to the platform default for [`fs::File::create`].
fn create_owner_only(path: &Path) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        fs::File::create(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N1_FIXTURE: &str = include_str!("../../../tests/fixtures/schema/lvm-state-v1-n1.json");
    const N2_FIXTURE: &str = include_str!("../../../tests/fixtures/schema/lvm-state-v1-n2.json");
    const TOMBSTONE_FIXTURE: &str =
        include_str!("../../../tests/fixtures/schema/lvm-state-v1-tombstones.json");
    /// The last released v1 shape (written by the pre-#70 binary at
    /// `ee739bc`) used as the N-1 fixture for schema v2.
    const V2_N1_FIXTURE: &str = include_str!("../../../tests/fixtures/schema/lvm-state-v2-n1.json");

    fn write_fixture(dir: &tempfile::TempDir, name: &str, text: &str) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, text).expect("write fixture");
        path
    }

    fn sample_state() -> LvmState {
        let volume_id = VolumeId::new("vol-1").expect("valid id");
        let project_id = ProjectId::new("tenant-a").expect("valid id");
        let device_id = DeviceId::new("dev-0123456789abcdef").expect("valid id");
        let mut state = LvmState::default();
        state.insert_volume(
            volume_id,
            StoredVolume {
                entry: VolumeEntry {
                    vg_name: "vg-1".to_owned(),
                    lv_name: "vol-1".to_owned(),
                    size_bytes: 1024,
                    requested_size_bytes: 512,
                    generation: 3,
                    data_epoch: 1,
                    project_id,
                    block_size: 4096,
                    creation_payload: "{\"class\":\"native-local\"}".to_owned(),
                },
                runtime: VolumeRuntime {
                    state: VolumeLifecycle::Ready,
                    retired_attachment_ids: Vec::new(),
                    attachment: Some(AttachmentRecord {
                        id: AttachmentId::new("att-1").expect("valid id"),
                        vm_id: "vm-1".to_owned(),
                        host_id: HostId::new("host-1").expect("valid id"),
                        generation: 1,
                        access_mode: AccessMode::SingleWriter,
                        vmm_disk_id: None,
                        state: volvisor_types::AttachmentState::Prepared,
                        state_revision: volvisor_types::domain::initial_state_revision(),
                        evidence: None,
                        reporter: None,
                        recorded_at_ms: None,
                    }),
                },
            },
        );
        state.insert_device(
            device_id,
            DeviceEntry {
                stable_identity: "wwn-0xabc".to_owned(),
                path: "/dev/disk/by-id/wwn-0xabc".to_owned(),
                vg_name: "vg-1".to_owned(),
                role: DeviceRole::NativePool,
                owner_generation: 1,
            },
        );
        state
    }

    #[test]
    fn load_missing_file_is_empty_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nonexistent.json");
        let state = LvmState::load(&path).expect("empty state");
        assert_eq!(state, LvmState::default());
    }

    #[test]
    fn empty_state_serializes_without_intent_maps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let mut state = LvmState::default();
        state.save(&path).expect("save");
        let text = fs::read_to_string(&path).expect("read state");
        assert!(
            text.contains("\"schema_version\": 2"),
            "the current schema version is stamped: {text}"
        );
        assert!(
            !text.contains("pending_creates"),
            "an empty state must not gain a create-intent map: {text}"
        );
        assert!(
            !text.contains("pending_grows"),
            "an empty state must not gain a grow-intent map: {text}"
        );
        assert!(
            !text.contains("pending_deletes"),
            "an empty state must not gain a delete-intent map: {text}"
        );
        assert_eq!(LvmState::load(&path).expect("reload"), state);
    }

    #[test]
    fn intent_maps_round_trip_losslessly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let volume_id = VolumeId::new("vol-intent").expect("valid id");
        let mut state = sample_state();
        state.insert_create_intent(
            volume_id.clone(),
            CreateIntent {
                vg_name: "vg-1".to_owned(),
                lv_name: "vol-intent".to_owned(),
                requested_size_bytes: 2048,
                project_id: ProjectId::new("tenant-a").expect("valid id"),
                block_size: 4096,
                creation_payload: "{\"class\":\"native-local\"}".to_owned(),
            },
        );
        state.insert_grow_intent(
            volume_id.clone(),
            GrowIntent {
                operation_id: OperationId::new("op-grow").expect("valid id"),
                from_size_bytes: 1024,
                requested_size_bytes: 4096,
                expected_generation: 3,
            },
        );
        state.insert_delete_intent(
            volume_id.clone(),
            DeleteIntent {
                operation_id: OperationId::new("op-delete").expect("valid id"),
                expected_generation: 3,
                erasure_policy: ErasurePolicy::ZeroDiscard,
            },
        );
        state.save(&path).expect("save");
        let loaded = LvmState::load(&path).expect("load");
        assert_eq!(loaded, state);
        assert_eq!(
            loaded
                .create_intent(&volume_id)
                .expect("create intent")
                .requested_size_bytes,
            2048
        );
        assert_eq!(
            loaded
                .grow_intent(&volume_id)
                .expect("grow intent")
                .expected_generation,
            3
        );
        assert_eq!(
            loaded
                .delete_intent(&volume_id)
                .expect("delete intent")
                .erasure_policy,
            ErasurePolicy::ZeroDiscard
        );

        // Removing every intent restores the compact shape.
        state.remove_create_intent(&volume_id);
        state.remove_grow_intent(&volume_id);
        state.remove_delete_intent(&volume_id);
        state.save(&path).expect("save cleared");
        let text = fs::read_to_string(&path).expect("read state");
        assert!(!text.contains("pending_creates"), "{text}");
        assert!(!text.contains("pending_grows"), "{text}");
        assert!(!text.contains("pending_deletes"), "{text}");
    }

    #[test]
    fn save_load_round_trip_is_lossless() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let mut state = sample_state();
        state.save(&path).expect("save");
        let loaded = LvmState::load(&path).expect("load");
        assert_eq!(loaded, state);
    }

    #[test]
    fn save_is_atomic_and_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let mut state = sample_state();
        state.save(&path).expect("save");
        assert!(path.exists());
        assert!(!sibling_tmp_path(&path).exists(), "no .tmp residue");

        // Overwriting an existing state also leaves no residue.
        state.save(&path).expect("save again");
        assert!(!sibling_tmp_path(&path).exists());
        let reloaded = LvmState::load(&path).expect("reload");
        assert_eq!(reloaded, state);
    }

    #[test]
    fn injected_save_failure_after_rename_keeps_new_state_and_before_rename_keeps_old() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let volume_id = VolumeId::new("vol-1").expect("valid id");
        let hooks = StoreCrashHooks::new();
        let mut state = sample_state();
        state.save(&path).expect("initial save");

        // Fail at the directory-fsync boundary, after the rename has
        // landed: the save reports the injected error, but a reload
        // must read the NEW state (the rename is the commit).
        state
            .volume_mut(&volume_id)
            .expect("volume")
            .entry
            .generation = 4;
        hooks.arm_save_failure(
            STORE_LVM_STATE,
            StoreSavePoint::AfterRenameBeforeDirFsync,
            0,
        );
        let err = state
            .save_with_hooks(&path, Some(&hooks))
            .expect_err("the armed save must fail");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("injected state save failure"), "{err}");
        assert!(err.detail.contains("AfterRenameBeforeDirFsync"), "{err}");
        assert!(
            !sibling_tmp_path(&path).exists(),
            "no .tmp residue after the injected failure"
        );
        let loaded = LvmState::load(&path).expect("load the post-rename state");
        assert_eq!(
            loaded.volume(&volume_id).expect("volume").entry.generation,
            4,
            "the rename landed before the injected failure"
        );

        // A pre-rename failure leaves the OLD durable state readable.
        state
            .volume_mut(&volume_id)
            .expect("volume")
            .entry
            .generation = 5;
        hooks.arm_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterFsyncBeforeRename, 0);
        let err = state
            .save_with_hooks(&path, Some(&hooks))
            .expect_err("the armed pre-rename save must fail");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("injected state save failure"), "{err}");
        assert!(
            !sibling_tmp_path(&path).exists(),
            "the tmp cleanup removes the unrenamed file"
        );
        let loaded = LvmState::load(&path).expect("load the old state");
        assert_eq!(
            loaded.volume(&volume_id).expect("volume").entry.generation,
            4,
            "a pre-rename failure leaves the old file untouched"
        );
    }

    #[test]
    fn load_rejects_corrupt_state_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"{ not json").expect("write corrupt state");
        let err = LvmState::load(&path).expect_err("corrupt state must fail");
        assert_eq!(err.code, ApiErrorCode::Internal);
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_created_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let mut state = sample_state();
        state.save(&path).expect("save");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "state file must be owner-only");
        // An overwritten state keeps the restrictive mode as well.
        state.save(&path).expect("save again");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // A failed save never leaves a .tmp residue behind.
        assert!(!sibling_tmp_path(&path).exists());
    }

    // -----------------------------------------------------------------
    // Golden historical fixtures (real pre-tombstone / initial formats
    // from git history; see docs/schema-compatibility.md).
    // -----------------------------------------------------------------

    #[test]
    fn n_minus_one_fixture_loads_attachment_evidence_and_move_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(&dir, "lvm-state-v1-n1.json", N1_FIXTURE);
        let state = LvmState::load(&path).expect("N-1 state loads");
        let volume_id = VolumeId::new("vol-upgrade-n1").expect("valid id");
        let volume = state.volume(&volume_id).expect("volume preserved");
        assert_eq!(volume.runtime.state, VolumeLifecycle::Attached);
        assert_eq!(volume.entry.generation, 7);
        assert_eq!(volume.entry.data_epoch, 3);
        let attachment = volume.runtime.attachment.as_ref().expect("attachment");
        assert_eq!(attachment.vm_id, "vm-upgrade");
        assert_eq!(attachment.generation, 5);
        assert_eq!(attachment.vmm_disk_id.as_deref(), Some("disk-n1"));
        assert_eq!(attachment.state, AttachmentState::Active);
        assert_eq!(attachment.state_revision, 3);
        assert_eq!(
            attachment.evidence,
            Some(AttachmentEvidence::GuestConfirmed)
        );
        assert_eq!(attachment.reporter.as_deref(), Some("consumer-upgrade"));
        assert_eq!(attachment.recorded_at_ms, Some(1_700_000_000_123));
        assert_eq!(
            volume.runtime.retired_attachment_ids,
            Vec::<AttachmentId>::new()
        );

        let device_id = DeviceId::new("dev-upgrade01").expect("valid id");
        assert_eq!(
            state.device(&device_id).expect("device").owner_generation,
            3
        );
        let movement = state.move_record(&volume_id).expect("move preserved");
        assert_eq!(movement.state, MoveVolumeBackingState::InDoubt);
        assert_eq!(movement.target_pv, "/dev/disk/by-id/wwn-0x5000c500eeff0011");
    }

    #[test]
    fn n_minus_two_fixture_defaults_pre_report_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(&dir, "lvm-state-v1-n2.json", N2_FIXTURE);
        let state = LvmState::load(&path).expect("N-2 state loads");
        let volume_id = VolumeId::new("vol-upgrade-n2").expect("valid id");
        let volume = state.volume(&volume_id).expect("volume preserved");
        let attachment = volume.runtime.attachment.as_ref().expect("attachment");
        // Pre-report fields decode through their documented defaults.
        assert_eq!(attachment.state, AttachmentState::Prepared);
        assert_eq!(
            attachment.state_revision,
            volvisor_types::domain::initial_state_revision()
        );
        assert_eq!(attachment.vmm_disk_id, None);
        assert_eq!(attachment.evidence, None);
        assert_eq!(attachment.reporter, None);
        assert_eq!(attachment.recorded_at_ms, None);
        assert!(state.moves().is_empty(), "pre-P6-C fixture has no moves");
        assert!(
            state.create_intents().is_empty(),
            "a pre-intent fixture has no create intents"
        );
        assert!(
            state.grow_intents().is_empty(),
            "a pre-intent fixture has no grow intents"
        );
        assert!(
            state.delete_intents().is_empty(),
            "a pre-intent fixture has no delete intents"
        );
        assert_eq!(
            volume.runtime.retired_attachment_ids,
            Vec::<AttachmentId>::new()
        );
        // The initial format had only the effective size; the requested
        // size backfills from it rather than defaulting to zero.
        assert_eq!(volume.entry.requested_size_bytes, volume.entry.size_bytes);
    }

    #[test]
    fn tombstone_fixture_preserves_retired_attachment_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(&dir, "lvm-state-v1-tombstones.json", TOMBSTONE_FIXTURE);
        let state = LvmState::load(&path).expect("tombstone state loads");
        let volume_id = VolumeId::new("vol-upgrade-tombstone").expect("valid id");
        let volume = state.volume(&volume_id).expect("volume preserved");
        let retired: Vec<&str> = volume
            .runtime
            .retired_attachment_ids
            .iter()
            .map(AsRef::as_ref)
            .collect();
        assert_eq!(retired, vec!["att-retired-oldest", "att-retired-newer"]);
    }

    #[test]
    fn v2_n_minus_one_fixture_loads_and_upgrades_to_v2() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = write_fixture(&dir, "lvm-state-v2-n1.json", V2_N1_FIXTURE);
        let mut state = LvmState::load(&source).expect("v1 state loads in v2");
        let volume_id = VolumeId::new("vol-upgrade-n1").expect("valid id");
        let volume = state.volume(&volume_id).expect("v1 volume preserved");
        assert_eq!(volume.entry.generation, 7);
        assert_eq!(
            volume
                .runtime
                .attachment
                .as_ref()
                .expect("v1 attachment")
                .state_revision,
            3
        );
        assert_eq!(
            state.move_record(&volume_id).expect("v1 move record").state,
            MoveVolumeBackingState::InDoubt
        );
        assert!(
            state.create_intents().is_empty()
                && state.grow_intents().is_empty()
                && state.delete_intents().is_empty(),
            "a v1 file has no intent maps"
        );

        let target = dir.path().join("upgraded-v2.json");
        state.save(&target).expect("upgrade-on-write");
        let text = fs::read_to_string(&target).expect("read upgraded");
        assert!(
            text.contains("\"schema_version\": 2"),
            "the first save stamps schema v2: {text}"
        );
        assert!(
            !text.contains("pending_creates")
                && !text.contains("pending_grows")
                && !text.contains("pending_deletes"),
            "empty intent maps stay omitted: {text}"
        );
        assert_eq!(
            LvmState::load(&target).expect("reload"),
            state,
            "no durable field is lost by the v1→v2 upgrade"
        );
    }

    #[test]
    fn old_fixture_upgrades_on_save_and_round_trips_losslessly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = write_fixture(&dir, "n1.json", N1_FIXTURE);
        let mut state = LvmState::load(&source).expect("load");
        let target = dir.path().join("upgraded.json");
        state.save(&target).expect("save");
        let text = fs::read_to_string(&target).expect("read upgraded");
        assert!(
            text.contains("\"schema_version\": 2"),
            "upgrade-on-write stamps the current schema version: {text}"
        );
        assert_eq!(
            LvmState::load(&target).expect("reload"),
            state,
            "no durable field is lost by load/save"
        );
    }

    #[test]
    fn future_schema_version_is_refused_typed_without_rewriting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let future = N1_FIXTURE.replacen(
            '{',
            "{\n  \"schema_version\": 3,\n  \"future_field\": true,",
            1,
        );
        let path = write_fixture(&dir, "future.json", &future);
        let err = LvmState::load(&path).expect_err("future version must be refused");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.is_unsupported_schema_version(), "{err}");
        assert!(err.detail.contains("schema_version 3"), "{err}");
        assert_eq!(
            fs::read_to_string(&path).expect("read refused file"),
            future,
            "a refused future-version file is never truncated or rewritten"
        );
    }

    #[test]
    fn zero_schema_version_is_refused_typed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let zero = N1_FIXTURE.replacen('{', "{\n  \"schema_version\": 0,", 1);
        let path = write_fixture(&dir, "zero.json", &zero);
        let err = LvmState::load(&path).expect_err("zero version must be refused");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(!err.is_unsupported_schema_version());
        assert!(err.detail.contains("invalid schema_version 0"), "{err}");
    }

    #[test]
    fn unknown_fields_at_a_supported_version_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Top-level unknown field.
        let top = N1_FIXTURE.replacen('{', "{\n  \"future_field\": true,", 1);
        let path = write_fixture(&dir, "top.json", &top);
        let err = LvmState::load(&path).expect_err("top-level unknown field");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(!err.is_unsupported_schema_version(), "{err}");

        // Nested unknown field inside a stored volume.
        let nested = N1_FIXTURE.replacen(
            "\"runtime\": {",
            "\"runtime\": {\n        \"future_field\": true,",
            1,
        );
        let path = write_fixture(&dir, "nested.json", &nested);
        let err = LvmState::load(&path).expect_err("nested unknown field");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("unknown field"), "{err}");
    }
}
