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

use serde::{Deserialize, Serialize};
use volvisor_types::crash::{STORE_LVM_STATE, StoreCrashHooks, StoreSavePoint};
use volvisor_types::{
    AccessMode, ApiError, ApiErrorCode, AttachmentId, DeviceId, DeviceRole, HostId,
    MoveVolumeBackingState, OperationId, ProjectId, VolumeId, VolumeLifecycle,
};

/// The volume→LV cross-reference plus the durable volume attributes.
///
/// `lv_name` is derived from the `volume_id` (sanitized plus a hash
/// suffix over the full id, so uniqueness rests on that 32-bit suffix
/// rather than the sanitized segment itself);
/// `vg_name` identifies the claimed pool the volume lives in (a
/// provider-internal reference, never exposed to tenants).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// The mutable runtime portion of a stored volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeRuntime {
    /// Current lifecycle state.
    pub state: VolumeLifecycle,
    /// The single recorded attachment, if any (single-writer P0 profile).
    pub attachment: Option<AttachmentRecord>,
}

/// A volume as stored in provider state: durable entry + runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredVolume {
    /// Durable cross-reference and attributes.
    pub entry: VolumeEntry,
    /// Lifecycle state and attachment record.
    pub runtime: VolumeRuntime,
}

/// A recorded attachment (crash-replay never fabricates a second one).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
}

/// A claimed physical device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
/// `COPYING`, `COMPLETE`, `IN_DOUBT` — never `MIRROR_READY`/
/// `PIVOTED` (mirror-path states; a same-VG extent move never
/// pivots, the LV's dm identity is stable) and never `FAILED` (an
/// unknown outcome is `IN_DOUBT`; deterministic rejections are typed
/// errors, not states).
///
/// Keyed by volume: at most one move record exists per volume. A
/// `COMPLETE` record for the same source/target answers re-issues
/// idempotently; an `IN_DOUBT` record parks further moves typed
/// until reconciliation proves completion or an operator resolves
/// it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// The whole durable provider state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LvmState {
    volumes: BTreeMap<VolumeId, StoredVolume>,
    devices: BTreeMap<DeviceId, DeviceEntry>,
    /// Online-move records, keyed by volume (at most one per
    /// volume). Defaulted so state files written before P6-C load
    /// unchanged.
    #[serde(default)]
    moves: BTreeMap<VolumeId, MoveRecord>,
}

impl LvmState {
    /// Load the state from `path`.
    ///
    /// A missing file is an empty state (first start); a corrupt file is an
    /// `INTERNAL` error — state is never guessed or reset silently.
    pub fn load(path: &Path) -> Result<Self, ApiError> {
        let path_display = path.display();
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!("failed to parse provider state {path_display}: {e}"),
                )
            }),
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
    /// after the rename) consult `crash` exactly like the grow
    /// store's saves, so a rig can kill the daemon at any commit
    /// split of a move-record mutation. The seam is inert unless the
    /// rig armed this store's points.
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
        let consult = |point: StoreSavePoint| {
            if let Some(crash) = crash {
                crash.consult(STORE_LVM_STATE, point);
            }
        };
        let data = serde_json::to_vec_pretty(self)
            .map_err(|e| internal(format!("failed to serialize provider state: {e}")))?;
        let mut file = create_owner_only(tmp_path)
            .map_err(|e| internal(format!("failed to create {tmp_display}: {e}")))?;
        file.write_all(&data)
            .map_err(|e| internal(format!("failed to write {tmp_display}: {e}")))?;
        consult(StoreSavePoint::AfterTmpWrite);
        file.sync_all()
            .map_err(|e| internal(format!("failed to fsync {tmp_display}: {e}")))?;
        drop(file);
        consult(StoreSavePoint::AfterFsyncBeforeRename);
        fs::rename(tmp_path, path).map_err(|e| {
            internal(format!(
                "failed to rename {tmp_display} to {path_display}: {e}"
            ))
        })?;
        consult(StoreSavePoint::AfterRename);
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
                    attachment: Some(AttachmentRecord {
                        id: AttachmentId::new("att-1").expect("valid id"),
                        vm_id: "vm-1".to_owned(),
                        host_id: HostId::new("host-1").expect("valid id"),
                        generation: 1,
                        access_mode: AccessMode::SingleWriter,
                        vmm_disk_id: None,
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
}
