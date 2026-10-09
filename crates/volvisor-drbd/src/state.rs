//! Durable JSON state for the DRBD nearline provider.
//!
//! The state file is the provider-internal cross-reference store: which
//! volume identity maps to which DRBD resource (and its backing logical
//! volume, minor number and peer port), the single recorded attachment
//! per volume, the requested vs. effective size, the replication
//! protocol fixed at create, whether the resource was ever seeded, and
//! the monotonic minor/port allocation counters. It is never exposed to
//! tenants, and it stores **no credentials**: the peer shared secret
//! lives only in the operator-provisioned secret file and the generated
//! (0600) resource definition, never in this state.
//!
//! Persistence is atomic: [`DrbdState::save`] writes `<path>.tmp`, fsyncs
//! the file, renames it over the target and fsyncs the parent directory,
//! so a crash can never leave a torn or half-renamed state file. The file
//! is created owner-only (`0600` on unix) because it names volume
//! placements. A missing file loads as an empty state (first start).
//!
//! Minor numbers and peer ports are allocated from persisted monotonic
//! counters inside operator-declared ranges; exhaustion is a typed
//! `NO_SAFE_CAPACITY` refusal, never a silent reuse (a crash window may
//! skip values — adopting a crashed predecessor's resource instead
//! derives its minor and port from the surviving resource file, see the
//! provider).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use volvisor_types::{
    AccessMode, ApiError, ApiErrorCode, AttachmentId, HostId, LeaseId, ProjectId, VolumeId,
    VolumeLifecycle, WriterEpoch,
};

/// The DRBD replication protocol of a resource, fixed at create time.
///
/// Distinct durability contracts (AGENTS rule 16): `A` is possible-RPO
/// asynchronous, `B` semi-synchronous (remote memory arrival), `C`
/// synchronous (local and remote disk completion). The letter is written
/// into the resource definition and re-verified from the file before
/// every mutation; runtime switching is a recorded follow-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplicationMode {
    /// Protocol A: asynchronous, possible RPO (the v2 default).
    #[serde(rename = "A")]
    A,
    /// Protocol B: semi-synchronous (remote memory arrival).
    #[serde(rename = "B")]
    B,
    /// Protocol C: synchronous (local and remote disk completion).
    #[serde(rename = "C")]
    C,
}

impl ReplicationMode {
    /// The single-letter spelling used in resource definitions.
    #[must_use]
    pub fn as_letter(self) -> char {
        match self {
            Self::A => 'A',
            Self::B => 'B',
            Self::C => 'C',
        }
    }

    /// The letter as a string (for comparisons against parsed files).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
        }
    }
}

/// The volume→resource cross-reference plus the durable volume attributes.
///
/// `resource_name` doubles as the LV name (the injective
/// `vol-<sanitized>-<hash8>` scheme shared with the LVM/Ceph providers);
/// ownership is *additionally* proven by the `volvisor.owner` LV tag,
/// which is verified before every mutation — the name alone is never the
/// ownership proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeEntry {
    /// DRBD resource name (== LV name; injective per volume identity).
    pub resource_name: String,
    /// Volume group holding the backing LV (operator-designated).
    pub vg_name: String,
    /// Backing logical volume name (== resource name).
    pub lv_name: String,
    /// Allocated DRBD minor number (`/dev/drbd<minor>`).
    pub minor: u32,
    /// Allocated local replication port.
    pub port: u16,
    /// Effective (provisioned) size in bytes: the device size the DRBD
    /// resource actually serves (thick LVM rounds LV sizes up to whole
    /// extents; the device may additionally be capped by a smaller peer
    /// backing after a boundary-failed grow).
    pub size_bytes: u64,
    /// The size the caller originally requested, in bytes. Idempotent
    /// create replay compares *this* value, never the effective size.
    pub requested_size_bytes: u64,
    /// Volume generation (optimistic concurrency fencing).
    pub generation: u64,
    /// Owning tenant project (needed to answer inspect/list).
    pub project_id: ProjectId,
    /// Logical block size in bytes.
    pub block_size: u32,
    /// Replication protocol fixed at create.
    pub replication_mode: ReplicationMode,
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
    /// Whether the resource was ever seeded (`primary --force` over a
    /// provably-fresh local disk). An unseeded volume cannot attach: its
    /// replica is not established.
    pub seeded: bool,
    /// The writer-authority lease this host holds for the volume, if
    /// any (P4a). `None` for pre-authority (P3-era) volumes and for any
    /// volume whose lease was released or fenced. Persisted **before**
    /// promotion, so an interrupted attach resumes through the renewal
    /// path with a fresh W5 deadline instead of replaying a stale grant
    /// response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<VolumeAuthorityBlock>,
    /// A self-fence that suspended I/O but could not finish demoting
    /// the resource (the device was still open — the kernel refuses
    /// demotion of an open device). Reconcile completes the demotion
    /// once the device closes and clears this marker; a resource
    /// carrying it is **volvisor's own suspended resource**, distinct
    /// from a foreign zombie promotion (which is never auto-demoted,
    /// AGENTS rule 17). Never a silent resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence: Option<PendingFence>,
}

/// A recorded, incomplete self-fence (see
/// [`VolumeRuntime::fence`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingFence {
    /// Why the writer fenced (stale epoch, local deadline passed,
    /// failed validation) — recorded, never inferred later.
    pub reason: String,
    /// Local unix time the fence started (diagnostics).
    pub fenced_at: u64,
}

/// The persisted writer-authority block (contract §1's required durable
/// authority state; P4a plan §4): everything needed to recover a
/// writer's authority bookkeeping after a crash — and nothing that
/// would let a restarted writer *infer* authority it cannot prove (the
/// lease itself is validated against the witness, never trusted from
/// this record).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeAuthorityBlock {
    /// The granted writer epoch (never 0: a block exists only for a
    /// witnessed lease).
    pub epoch: WriterEpoch,
    /// The granted lease identity (needed to renew; W4-checked).
    pub lease_id: LeaseId,
    /// The witness commit index that durably recorded the grant — the
    /// lease proof reference (W2).
    pub lease_proof_ref: u64,
    /// The authority commit index of the grant (`authority_commit_index`
    /// in contract §1; equal to [`Self::lease_proof_ref`] in P4a, where
    /// only the grant establishes the recorded authority).
    pub authority_commit_index: u64,
    /// Local unix time the lease response was received — the W5 anchor.
    pub acquired_at: u64,
    /// Local unix deadline (`acquired_at` + the duration the lease
    /// response carried). The writer self-fences at this deadline; it
    /// never re-derives deadlines from absolute witness timestamps.
    pub deadline_at: u64,
}

/// A volume as stored in provider state: durable entry + runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredVolume {
    /// Durable cross-reference and attributes.
    pub entry: VolumeEntry,
    /// Lifecycle state, seeding flag and attachment record.
    pub runtime: VolumeRuntime,
}

/// A recorded attachment (crash-replay never fabricates a second one).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentRecord {
    /// Attachment identity (caller-chosen, opaque).
    pub id: AttachmentId,
    /// Consuming VM identity.
    pub vm_id: String,
    /// Host the attachment (the DRBD device) is scoped to.
    pub host_id: HostId,
    /// Attachment generation.
    pub generation: u64,
    /// Granted access mode.
    pub access_mode: AccessMode,
    /// The host-scoped `/dev/drbdN` device this attachment promoted. It
    /// is a host-local, ephemeral handle — never a secret.
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

/// Result of reconciling provider state against the observed host.
///
/// Everything here is *reported*; the only mutations reconcile performs
/// are the documented, non-destructive bookkeeping ones (marking volumes
/// `Failed`, clearing attachment records whose backing is provably gone
/// or no longer ours, healing a completed-but-unrecorded grow, and
/// seeding a provably-fresh resource whose peer just appeared
/// `Inconsistent`). Foreign LVs are reported and never touched
/// (AGENTS rule 7); an actual primary resource is never auto-demoted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// State entries whose backing LV is absent from a successful `lvs`
    /// query (marked `Failed`; any attachment record is cleared because
    /// the backing it referenced no longer exists).
    pub missing_volumes: Vec<VolumeId>,
    /// State entries whose LV exists but whose `volvisor.owner` tag is
    /// missing or names a different volume (marked `Failed`, attachment
    /// record cleared — never adopted).
    pub mismatched_volumes: Vec<VolumeId>,
    /// State entries whose resource definition file is missing, unparsable
    /// or no longer matches the recorded resource name, minor, protocol,
    /// disk or node set (marked `Failed`; the attachment record is kept
    /// because the resource may still be live — the operator restores
    /// the file or deletes the volume out of band).
    pub resource_file_mismatches: Vec<VolumeId>,
    /// State entries whose resource is verifiably down (a successful
    /// `drbdsetup status` answered "No such resource"). A downed resource
    /// with an attachment record is `Failed` with the record cleared
    /// (the `/dev/drbdN` device it named is gone); a downed resource
    /// without a record is `Failed` — the volume claims a lifecycle its
    /// backend does not support.
    pub downed_volumes: Vec<VolumeId>,
    /// State entries whose resource is Primary without an attachment
    /// record — a zombie promotion from a crashed prior life. Reported
    /// and marked `Failed`; **never auto-demoted** (rule 17: demotion
    /// requires releasing an in-use source device, an operator action).
    pub zombie_primaries: Vec<VolumeId>,
    /// Unseeded volumes whose peer just appeared with an `Inconsistent`
    /// disk (a fresh, never-seeded peer): seeded this pass through the
    /// verified `primary --force` / `secondary` sequence.
    pub seeded_volumes: Vec<VolumeId>,
    /// Unseeded volumes whose peer was observed holding data
    /// (`UpToDate`/`Consistent`/`Outdated`) — foreign data on a provably
    /// fresh local disk; marked `Failed`, never overwritten.
    pub foreign_peer_volumes: Vec<VolumeId>,
    /// State entries whose effective device size was healed UP to the
    /// observed size: a completed-but-unrecorded grow (the crash window
    /// after `drbdadm resize` succeeded but the state save did not).
    pub healed_grown: Vec<VolumeId>,
    /// State entries whose effective device size is BELOW the recorded
    /// size — the backing changed outside volvisor (marked `Failed`;
    /// the recorded size is never healed downward).
    pub shrunk_volumes: Vec<VolumeId>,
    /// The audit trail for attachment records cleared this pass (see
    /// [`ClearedAttachment`]).
    pub cleared_attachments: Vec<ClearedAttachment>,
    /// State entries whose status could not be verified this pass (a
    /// transient query failure). Their lifecycle is deliberately left
    /// untouched — an unknown is never recorded as `Failed`.
    pub unverifiable_volumes: Vec<UnverifiableVolume>,
    /// LVs in the nearline VG without a `volvisor.owner` tag: foreign.
    /// Reported, never touched.
    pub foreign_lvs: Vec<String>,
    /// LVs carrying a `volvisor.owner` tag but absent from state (e.g. a
    /// crash between `lvcreate` and the state save). Reported, never
    /// adopted.
    pub untracked_owned_lvs: Vec<String>,
    /// Witness-managed volumes found Primary that could not be
    /// validated because the witness was unreachable: left **suspended**
    /// (fail-closed — a restarted daemon never silently resumes a writer
    /// it cannot prove, P4a plan §4).
    pub unvalidated_primaries: Vec<UnverifiableVolume>,
    /// Witness-managed volumes whose self-fence this pass completed
    /// (the suspended resource finished demoting once its device
    /// closed).
    pub completed_fences: Vec<VolumeId>,
    /// Leases self-fenced this pass (stale epoch, local deadline
    /// passed, or a failed validation) — see [`FencedVolume`].
    pub fenced_volumes: Vec<FencedVolume>,
}

/// A lease this host self-fenced (writer authority provably lost).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FencedVolume {
    /// The volume whose writer was fenced.
    pub volume_id: VolumeId,
    /// Why (stale epoch, local deadline passed, failed validation) —
    /// recorded, never inferred.
    pub reasons: Vec<String>,
    /// Whether the resource finished demoting (`false` = still
    /// suspended, demotion completes once the device closes).
    pub demoted: bool,
}

/// Result of one `renew_leases` pass (P4a plan §4): every outcome is
/// reported; nothing is silently dropped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenewalReport {
    /// Volumes whose lease was renewed this pass.
    pub renewed: Vec<VolumeId>,
    /// Volumes that self-fenced this pass, with reasons.
    pub fenced: Vec<FencedVolume>,
    /// Volumes whose renewal failed *without* proving authority lost
    /// (e.g. the witness is unreachable): the writer keeps serving
    /// until its W5 local deadline, which is carried here — the honest
    /// bound, never a guess.
    pub deferred: Vec<DeferredRenewal>,
}

/// A renewal that failed without proving authority lost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeferredRenewal {
    /// The volume whose renewal failed.
    pub volume_id: VolumeId,
    /// The summarized failure (reported, never hidden).
    pub detail: String,
    /// The W5 local deadline (unix seconds) after which the writer
    /// self-fences even if the witness stays unreachable.
    pub deadline_at: u64,
}

/// An attachment record cleared by reconcile.
///
/// The record's details are preserved in the report as the audit trail:
/// the device it named and why it was cleared. The device itself is
/// never touched — a zombie `/dev/drbdN` (a device still serving over a
/// gone or foreign backing) is left to an operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClearedAttachment {
    /// The volume whose record was cleared.
    pub volume_id: VolumeId,
    /// The device the cleared record named.
    pub device: String,
    /// Why the record was cleared.
    pub reason: ClearedAttachmentReason,
}

/// Why reconcile cleared an attachment record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClearedAttachmentReason {
    /// The backing LV is verifiably absent from a successful `lvs`.
    VanishedBacking,
    /// The LV exists but its `volvisor.owner` tag is missing or names a
    /// different volume (never adopted).
    OwnershipMismatch,
    /// The resource is verifiably down; the `/dev/drbdN` device the
    /// record named no longer exists.
    ResourceDown,
    /// The resource is up and Secondary — the demotion succeeded but
    /// the state save did not (an interrupted detach). The record is
    /// cleared and the volume returns to `Ready`.
    InterruptedDetach,
}

/// The whole durable provider state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrbdState {
    volumes: BTreeMap<VolumeId, StoredVolume>,
    /// Next DRBD minor to hand out (monotonic; see [`Self::allocate_minor`]).
    #[serde(default)]
    next_minor: u32,
    /// Next local replication port to hand out (monotonic).
    #[serde(default)]
    next_port: u16,
}

impl DrbdState {
    /// Load the state from `path`.
    ///
    /// A missing file is an empty state (first start); a corrupt file is
    /// an `INTERNAL` error — state is never guessed or reset silently.
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
    /// the directory. If any step fails, the temporary file is removed
    /// and an `INTERNAL` error is returned; the previous state file
    /// remains intact.
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

    /// Allocate the next DRBD minor inside `[min, max]`.
    ///
    /// Monotonic and persisted: a value is never handed out twice while
    /// the state survives (a crash window may skip values — adoption of
    /// a crashed predecessor's resource derives its minor from the
    /// resource file instead). Exhaustion of the operator-declared range
    /// is a typed `NO_SAFE_CAPACITY` refusal, never a silent reuse.
    ///
    /// The operator-declared range must be **exclusively reserved for
    /// volvisor**: this allocator never probes the kernel's minor
    /// space, so a minor already claimed by a foreign resource surfaces
    /// only later, as a create-time failure that tears the half-created
    /// volume back down.
    ///
    /// # Errors
    /// `NO_SAFE_CAPACITY` when the range is exhausted.
    pub fn allocate_minor(&mut self, min: u32, max: u32) -> Result<u32, ApiError> {
        if self.next_minor < min {
            self.next_minor = min;
        }
        if self.next_minor > max {
            return Err(ApiError::new(
                ApiErrorCode::NoSafeCapacity,
                format!("no safe capacity: the DRBD minor range [{min}, {max}] is exhausted"),
            ));
        }
        let minor = self.next_minor;
        self.next_minor += 1;
        Ok(minor)
    }

    /// Allocate the next local replication port inside `[min, max]`
    /// (same monotonic semantics as [`Self::allocate_minor`]).
    ///
    /// # Errors
    /// `NO_SAFE_CAPACITY` when the range is exhausted.
    pub fn allocate_port(&mut self, min: u16, max: u16) -> Result<u16, ApiError> {
        if self.next_port < min {
            self.next_port = min;
        }
        if self.next_port > max {
            return Err(ApiError::new(
                ApiErrorCode::NoSafeCapacity,
                format!("no safe capacity: the DRBD port range [{min}, {max}] is exhausted"),
            ));
        }
        let port = self.next_port;
        self.next_port += 1;
        Ok(port)
    }

    /// Keep the minor counter ahead of an adopted resource's minor so a
    /// later allocation can never collide with it.
    pub fn observe_minor(&mut self, minor: u32) {
        if minor >= self.next_minor {
            self.next_minor = minor + 1;
        }
    }

    /// Keep the port counter ahead of an adopted resource's port.
    pub fn observe_port(&mut self, port: u16) {
        if port >= self.next_port {
            self.next_port = port + 1;
        }
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

/// Create (or truncate) `path` for writing with owner-only permissions
/// (resource-definition files share the state module's atomic-write
/// helper shape; see `resgen`).
pub(crate) fn create_owner_only_file(path: &Path) -> std::io::Result<fs::File> {
    create_owner_only(path)
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

    fn sample_state() -> DrbdState {
        let volume_id = VolumeId::new("vol-1").expect("valid id");
        let project_id = ProjectId::new("tenant-a").expect("valid id");
        let mut state = DrbdState::default();
        state.insert_volume(
            volume_id,
            StoredVolume {
                entry: VolumeEntry {
                    resource_name: "vol-vol-1-abcd0123".to_owned(),
                    vg_name: "vgdrbd".to_owned(),
                    lv_name: "vol-vol-1-abcd0123".to_owned(),
                    minor: 7,
                    port: 7901,
                    size_bytes: 1024,
                    requested_size_bytes: 1024,
                    generation: 3,
                    project_id,
                    block_size: 4096,
                    replication_mode: ReplicationMode::A,
                    creation_payload: "{\"class\":\"nearline-replicated\"}".to_owned(),
                    created_at: 1_700_000_000,
                },
                runtime: VolumeRuntime {
                    state: VolumeLifecycle::Attached,
                    seeded: true,
                    authority: None,
                    fence: None,
                    attachment: Some(AttachmentRecord {
                        id: AttachmentId::new("att-1").expect("valid id"),
                        vm_id: "vm-1".to_owned(),
                        host_id: HostId::new("host-1").expect("valid id"),
                        generation: 1,
                        access_mode: AccessMode::SingleWriter,
                        device: "/dev/drbd7".to_owned(),
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
        let state = DrbdState::load(&path).expect("empty state");
        assert_eq!(state, DrbdState::default());
    }

    #[test]
    fn save_load_round_trip_is_lossless() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let mut state = sample_state();
        state.save(&path).expect("save");
        let loaded = DrbdState::load(&path).expect("load");
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
        state.save(&path).expect("save again");
        assert!(!sibling_tmp_path(&path).exists());
    }

    #[test]
    fn load_rejects_corrupt_state_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"{ not json").expect("write corrupt state");
        let err = DrbdState::load(&path).expect_err("corrupt state must fail");
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
    }

    #[test]
    fn state_serialization_carries_no_credentials() {
        let state = sample_state();
        let text = serde_json::to_string(&state).expect("serialize");
        assert!(!text.to_lowercase().contains("secret"));
        assert!(!text.to_lowercase().contains("key"));
        assert!(!text.to_lowercase().contains("token"));
    }

    #[test]
    fn allocation_is_monotonic_and_exhausts_typed() {
        let mut state = DrbdState::default();
        assert_eq!(state.allocate_minor(10, 12).expect("minor"), 10);
        assert_eq!(state.allocate_minor(10, 12).expect("minor"), 11);
        assert_eq!(state.allocate_minor(10, 12).expect("minor"), 12);
        let err = state.allocate_minor(10, 12).expect_err("exhausted");
        assert_eq!(err.code, ApiErrorCode::NoSafeCapacity);

        let mut state = DrbdState::default();
        assert_eq!(state.allocate_port(100, 101).expect("port"), 100);
        assert_eq!(state.allocate_port(100, 101).expect("port"), 101);
        let err = state.allocate_port(100, 101).expect_err("exhausted");
        assert_eq!(err.code, ApiErrorCode::NoSafeCapacity);
    }

    #[test]
    fn counters_survive_round_trips_and_observation_bumps_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let mut state = DrbdState::default();
        assert!(state.allocate_minor(5, 100).is_ok());
        assert!(state.allocate_port(7000, 7100).is_ok());
        state.save(&path).expect("save");
        let mut loaded = DrbdState::load(&path).expect("load");
        // The persisted counters continue where they left off.
        assert_eq!(loaded.allocate_minor(5, 100).expect("minor"), 6);
        assert_eq!(loaded.allocate_port(7000, 7100).expect("port"), 7001);
        // Adopting an existing resource keeps the counters ahead of it.
        loaded.observe_minor(90);
        loaded.observe_port(7090);
        assert_eq!(loaded.allocate_minor(5, 100).expect("minor"), 91);
        assert_eq!(loaded.allocate_port(7000, 7100).expect("port"), 7091);
    }

    #[test]
    fn replication_mode_wire_format_is_the_protocol_letter() {
        assert_eq!(
            serde_json::to_string(&ReplicationMode::A).expect("serialize"),
            "\"A\""
        );
        assert_eq!(ReplicationMode::B.as_letter(), 'B');
        assert_eq!(ReplicationMode::C.as_str(), "C");
        let back: ReplicationMode = serde_json::from_str("\"B\"").expect("deserialize");
        assert_eq!(back, ReplicationMode::B);
    }
}
