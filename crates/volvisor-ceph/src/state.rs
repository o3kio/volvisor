//! Durable JSON state for the Ceph RBD provider.
//!
//! The state file is the provider-internal cross-reference store: which
//! volume identity maps to which RBD image name, the single recorded
//! attachment per volume (device mapping, host, attachment generation),
//! the requested vs. effective size, and the ownership generation. It is
//! never exposed to tenants (the API surface derives all responses from
//! it), and it stores **no credentials**: the keyring is daemon-side and
//! the provider only ever passes `--name <user>` to the CLI.
//!
//! Persistence is atomic: [`CephState::save`] writes `<path>.tmp`, fsyncs
//! the file, renames it over the target and fsyncs the parent directory,
//! so a crash can never leave a torn or half-renamed state file. The file
//! is created owner-only (`0600` on unix) because it names volume
//! placements. A missing file loads as an empty state (first start).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use volvisor_types::{
    AccessMode, ApiError, ApiErrorCode, AttachmentId, HostId, ProjectId, VolumeId, VolumeLifecycle,
};

/// The volume→image cross-reference plus the durable volume attributes.
///
/// `image_name` is derived from the `volume_id` (sanitized plus a hash
/// suffix over the full id, so uniqueness rests on that 32-bit suffix
/// rather than the sanitized segment itself); ownership is *additionally*
/// proven by the `volvisor.owner` image metadata key, which is verified
/// before every mutation — the name alone is never the ownership proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeEntry {
    /// RBD image name inside the configured pool.
    pub image_name: String,
    /// Effective (provisioned) size in bytes. RBD sizes are byte-granular
    /// (no extent rounding), so this equals what `rbd info` reported after
    /// create/grow.
    pub size_bytes: u64,
    /// The size the caller originally requested, in bytes. Idempotent-create
    /// replay compares *this* value against a replayed request, mirroring
    /// the LVM provider's requested-vs-effective distinction.
    pub requested_size_bytes: u64,
    /// Volume generation (optimistic concurrency fencing).
    pub generation: u64,
    /// Owning tenant project (needed to answer inspect/list).
    pub project_id: ProjectId,
    /// Logical block size in bytes.
    pub block_size: u32,
    /// Canonical creation payload with `operation_id` normalized out;
    /// used to detect idempotent replays vs. payload conflicts.
    pub creation_payload: String,
    /// Creation time as seconds since the unix epoch (diagnostics only).
    pub created_at: u64,
}

/// The mutable runtime portion of a stored volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeRuntime {
    /// Current lifecycle state.
    pub state: VolumeLifecycle,
    /// The single recorded attachment, if any (single-writer profile).
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
    /// Host the attachment (the `rbd map` device) is scoped to.
    pub host_id: HostId,
    /// Attachment generation.
    pub generation: u64,
    /// Granted access mode.
    pub access_mode: AccessMode,
    /// The host-scoped `/dev/rbd*` device this attachment mapped. It is a
    /// host-local, ephemeral handle — never a secret.
    pub device: String,
}

/// A state entry whose backing could not be verified this pass.
///
/// Carries the summarized error so the report is an honest unknown, not
/// a silent skip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnverifiableVolume {
    /// The volume whose verification failed.
    pub volume_id: VolumeId,
    /// Summarized error from the failed query.
    pub detail: String,
}

/// An untracked image whose ownership could not be classified this pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnverifiableImage {
    /// The image whose ownership query failed.
    pub image_name: String,
    /// Summarized error from the failed query.
    pub detail: String,
}

/// Result of reconciling provider state against the observed cluster.
///
/// Everything here is *reported*, never auto-fixed destructively: missing
/// and mismatched volumes are marked `Failed` in state, foreign images are
/// left untouched (AGENTS rule 7). The one deliberate bookkeeping heal is
/// [`ReconcileReport::healed_grown`](#structfield.healed_grown): a grow
/// that completed on the cluster but was never recorded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// State entries whose image is absent from a successful `rbd ls`
    /// (marked `Failed`).
    pub missing_volumes: Vec<VolumeId>,
    /// State entries whose image exists but whose `volvisor.owner`
    /// metadata is missing or names a different volume (marked `Failed`;
    /// never adopted).
    pub mismatched_volumes: Vec<VolumeId>,
    /// State entries whose image reports LESS than the recorded size —
    /// the image changed outside volvisor (marked `Failed`; the recorded
    /// size is never healed downward).
    pub shrunk_volumes: Vec<VolumeId>,
    /// State entries whose image is currently mapped but that carry no
    /// attachment record — a stale mapping from a previous incarnation.
    /// Reported honestly; never automatically unmapped (destructive).
    pub stale_mappings: Vec<VolumeId>,
    /// State entries whose recorded size was healed UP to the image's
    /// actual size: a completed-but-unrecorded grow (the crash window
    /// after `rbd resize` succeeded but the state save did not). The
    /// image's own report is the authority; only the bookkeeping lagged.
    pub healed_grown: Vec<VolumeId>,
    /// State entries whose backing could not be verified this pass (a
    /// transient query failure, e.g. a mon timeout). Their lifecycle is
    /// deliberately left untouched — an unknown is never recorded as
    /// `Failed` — and counted here instead.
    pub unverifiable_volumes: Vec<UnverifiableVolume>,
    /// Images in the pool without any `volvisor.owner` metadata: foreign.
    /// Reported, never touched.
    pub foreign_images: Vec<String>,
    /// Images carrying `volvisor.owner` metadata but absent from state
    /// (e.g. a crash between `rbd create` and the state save). Reported,
    /// never adopted.
    pub untracked_owned_images: Vec<String>,
    /// Untracked images whose ownership could not be classified (a
    /// transient query failure): neither foreign nor ours, counted
    /// honestly instead of guessed.
    pub unverifiable_images: Vec<UnverifiableImage>,
}

/// The whole durable provider state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CephState {
    volumes: BTreeMap<VolumeId, StoredVolume>,
}

impl CephState {
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
    /// `<path>.tmp` (mode `0600` on unix — the state names volume
    /// placements, so it is owner-only), fsync, rename over `path`, fsync
    /// the directory. If any step fails, the temporary file is removed and
    /// an `INTERNAL` error is returned; the previous state file remains
    /// intact.
    pub fn save(&mut self, path: &Path) -> Result<(), ApiError> {
        let tmp_path = sibling_tmp_path(path);
        let result = self.save_to(&tmp_path, path);
        if result.is_err() {
            // Best-effort cleanup: never leave a stale .tmp behind.
            drop(fs::remove_file(&tmp_path));
        }
        result
    }

    fn save_to(&mut self, tmp_path: &Path, path: &Path) -> Result<(), ApiError> {
        let internal = |detail: String| ApiError::new(ApiErrorCode::Internal, detail);
        let tmp_display = tmp_path.display();
        let path_display = path.display();
        let data = serde_json::to_vec_pretty(self)
            .map_err(|e| internal(format!("failed to serialize provider state: {e}")))?;
        let mut file = create_owner_only(tmp_path)
            .map_err(|e| internal(format!("failed to create {tmp_display}: {e}")))?;
        file.write_all(&data)
            .map_err(|e| internal(format!("failed to write {tmp_display}: {e}")))?;
        file.sync_all()
            .map_err(|e| internal(format!("failed to fsync {tmp_display}: {e}")))?;
        drop(file);
        fs::rename(tmp_path, path).map_err(|e| {
            internal(format!(
                "failed to rename {tmp_display} to {path_display}: {e}"
            ))
        })?;
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

/// Seconds since the unix epoch (best effort; diagnostics only).
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> CephState {
        let volume_id = VolumeId::new("vol-1").expect("valid id");
        let project_id = ProjectId::new("tenant-a").expect("valid id");
        let mut state = CephState::default();
        state.insert_volume(
            volume_id,
            StoredVolume {
                entry: VolumeEntry {
                    image_name: "vol-vol-1-abcd0123".to_owned(),
                    size_bytes: 1024,
                    requested_size_bytes: 1024,
                    generation: 3,
                    project_id,
                    block_size: 4096,
                    creation_payload: "{\"class\":\"ceph-rbd\"}".to_owned(),
                    created_at: 1_700_000_000,
                },
                runtime: VolumeRuntime {
                    state: VolumeLifecycle::Attached,
                    attachment: Some(AttachmentRecord {
                        id: AttachmentId::new("att-1").expect("valid id"),
                        vm_id: "vm-1".to_owned(),
                        host_id: HostId::new("host-1").expect("valid id"),
                        generation: 1,
                        access_mode: AccessMode::SingleWriter,
                        device: "/dev/rbd0".to_owned(),
                    }),
                },
            },
        );
        state
    }

    #[test]
    fn load_missing_file_is_empty_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nonexistent.json");
        let state = CephState::load(&path).expect("empty state");
        assert_eq!(state, CephState::default());
    }

    #[test]
    fn save_load_round_trip_is_lossless() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let mut state = sample_state();
        state.save(&path).expect("save");
        let loaded = CephState::load(&path).expect("load");
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
        let reloaded = CephState::load(&path).expect("reload");
        assert_eq!(reloaded, state);
    }

    #[test]
    fn load_rejects_corrupt_state_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"{ not json").expect("write corrupt state");
        let err = CephState::load(&path).expect_err("corrupt state must fail");
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

    #[test]
    fn state_serialization_carries_no_credentials() {
        // The state must never carry key material: the serialized sample
        // contains only identities, geometry and the device handle.
        let state = sample_state();
        let text = serde_json::to_string(&state).expect("serialize");
        assert!(!text.to_lowercase().contains("key"));
        assert!(!text.to_lowercase().contains("secret"));
        assert!(!text.to_lowercase().contains("token"));
    }
}
