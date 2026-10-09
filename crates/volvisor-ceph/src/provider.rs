//! The external-cluster Ceph RBD provider (ADR-0005 Phase A).
//!
//! [`CephRbdProvider`] implements [`VolumeProvider`] for
//! [`VolumeClass::CephRbd`]: volumes are RBD images in one configured pool
//! of an **existing, externally operated** Ceph cluster, driven through
//! the `ceph`/`rbd` CLIs via the shell-free
//! [`CommandRunner`](volvisor_provider::runner). Every invocation carries
//! `-m <mons>` and `--name <user>` (never an environment-variable or
//! config-file side channel); the ceph CLI resolves credentials itself,
//! so this provider never reads or stores key material.
//!
//! Ownership is proven, not assumed: image names are injective
//! (`vol-<sanitized>-<hash8>`, mirroring the LVM provider's scheme) and
//! every image this provider creates additionally carries
//! [`OWNER_META_KEY`]/[`GENERATION_META_KEY`] metadata that is verified
//! read-back before state is persisted and re-verified before every
//! mutation. Images without our metadata are foreign and never touched
//! (AGENTS rule 7).
//!
//! Honesty rules that shape this implementation:
//!
//! - a successful exit status is never evidence: sizes are read back from
//!   `rbd info`, trash placement from `rbd trash ls`, mappings from
//!   `rbd showmapped`;
//! - attach evidence starts at `Prepared` (no VMM integration exists),
//!   volume health axes stay `Unknown` until proven, `evidence_status`
//!   is `PrototypeOnly`, and Ceph's health is reported as Ceph's own
//!   policy facts — never converted into a Volvisor-made durability
//!   guarantee (ADR-0005);
//! - single-writer fencing uses the RBD `exclusive-lock` image feature;
//!   a pre-existing (crash-leftover) mapping is rejected fail-closed, and
//!   reconcile never unmaps automatically (destructive).

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use volvisor_provider::VolumeProvider;
use volvisor_types::domain::{
    AccessMode, EffectiveProtection, EvidenceStatus, FailureDomain, Frontend, Health,
    LocalProtectionAxis, PoolProtection, Provisioning, RemoteProtectionAxis, VolumeClass,
};
use volvisor_types::request::{
    AttachVolumeRequest, AttachVolumeResponse, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, ErasurePolicy, GrowGuestNotification, GrowVolumeRequest,
    GrowVolumeResponse, InspectVolumeResponse, LocalProtectionModeRequest,
};
use volvisor_types::{
    ApiError, ApiErrorCode, AttachmentId, AttachmentState, Capability, CapabilitySet, Pool, PoolId,
    ProjectId, VolumeId, VolumeLifecycle, validate_api_version,
};

use crate::report::{
    CephDfPool, HealthDetail, MappedDevice, RbdInfo, parse_ceph_df, parse_fsid,
    parse_health_detail, parse_image_list, parse_pool_policy_value, parse_rbd_info,
    parse_showmapped, parse_trash_list,
};
use crate::state::{
    AttachmentRecord, CephState, ClearedAttachment, ClearedAttachmentReason, StoredVolume,
    UnverifiableImage, UnverifiableVolume, VolumeEntry, VolumeRuntime, unix_now,
};
use crate::{CommandOutput, CommandRunner, ReconcileReport};

/// Stable provider name for diagnostics (never a secret).
pub const PROVIDER_NAME: &str = "ceph-rbd-prototype";

/// RBD image metadata key holding the owning volume identity.
pub const OWNER_META_KEY: &str = "volvisor.owner";

/// RBD image metadata key holding the volume generation at create time.
pub const GENERATION_META_KEY: &str = "volvisor.generation";

/// The only volume class served by this provider.
static SUPPORTED_CLASSES: &[VolumeClass] = &[VolumeClass::CephRbd];

/// The size-agreement outcome for an owned image, from `rbd info`.
#[derive(Clone, Debug)]
enum SizeAgreement {
    /// The image reports exactly the recorded size.
    Agree,
    /// The image reports MORE than recorded: a completed-but-unrecorded
    /// grow (the crash window after `rbd resize`), carrying the actual
    /// size so the record can be healed.
    Grown(u64),
    /// The image reports LESS than recorded: it changed outside volvisor
    /// (a shrink) — a violation, never healed.
    Shrunk,
    /// The size could not be verified (a transient read failure): an
    /// honest unknown carrying the summarized error.
    Unknown(String),
}

/// Logical block size assumed when a create request omits one.
const DEFAULT_BLOCK_SIZE: u32 = 4096;

/// Pool headroom kept unallocated so the cluster can always write metadata
/// and absorb recovery churn (`NO_SAFE_CAPACITY` is reported before the
/// pool fills). 1 GiB: a conservative slice of a Ceph pool, deliberately
/// independent of pool size because `ceph df` reports no metadata reserve.
pub const CEPH_HEADROOM_BYTES: u64 = 1 << 30;

/// The image features every volvisor-owned image is created with.
///
/// `exclusive-lock` makes the single-writer mechanism *available* to
/// clients (it is acquired lazily on first write, not at `rbd map`
/// time — the cluster does not refuse a second map), and `layering` is
/// the standard snapshot/clone basis. Volvisor's single-writer
/// guarantee does not rest on the lock: it comes from the recorded
/// attachment (a second attach is a typed `WRITER_ALREADY_ACTIVE`
/// rejection) plus the `showmapped` scan. Verifying that an existing
/// image actually carries these features before adopting it is a
/// recorded follow-up, not implemented in P2.
const IMAGE_FEATURES: &str = "exclusive-lock,layering";

/// The outcome of verifying a state entry against the observed cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backing {
    /// The image is verifiably absent from a successful `rbd ls` query.
    Absent,
    /// The image exists and its `volvisor.owner` metadata matches the
    /// volume identity (carrying the size `rbd info` reported).
    Owned {
        /// Effective image size in bytes.
        size_bytes: u64,
    },
    /// The image exists but its ownership metadata is missing or names a
    /// different volume: never adopted, never destroyed by us.
    Mismatch,
}

/// The replication policy facts of the configured pool (from
/// `ceph osd pool get`).
#[derive(Clone, Copy, Debug)]
struct PoolReplicationPolicy {
    /// Replication factor (`size`): how many copies the pool keeps.
    size: u64,
    /// Minimum healthy replicas the pool still accepts I/O at
    /// (`min_size`).
    min_size: u64,
}

/// The usable-capacity picture of the configured pool (from `ceph df`).
///
/// Deliberately carries no replication facts: real `ceph df` per-pool
/// stats have `stored`/`objects`/`kb_used`/`bytes_used`/
/// `percent_used`/`max_avail` and nothing else. Replication policy is
/// queried separately through `ceph osd pool get` (see
/// [`CephRbdProvider::discover_pools`]).
#[derive(Clone, Copy, Debug)]
struct PoolCapacity {
    /// Bytes used in the pool as reported by `ceph df`.
    bytes_used: u64,
    /// Bytes still allocatable in the pool as reported by `ceph df`.
    max_avail: u64,
}

/// Verified connection parameters for one external Ceph cluster.
///
/// The constructor fail-closes unless the cluster answers with exactly
/// `cluster_fsid`, the pool exists, and a health query succeeds; a
/// mis-pointed cluster is never adopted. No keyring lives here: the
/// daemon-side keyring is resolved by the ceph CLI through
/// `--name <user>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CephProviderConfig {
    /// The cluster FSID this provider may operate on (exact match).
    pub cluster_fsid: String,
    /// Monitor addresses (`host:port`), 1..=9 entries, joined into the
    /// `-m` flag of every invocation.
    pub mon_hosts: Vec<String>,
    /// The single pool volumes are created in.
    pub pool: String,
    /// The full Ceph entity name, e.g. `client.volvisor`, passed via
    /// `--name` on every invocation (`--id` takes a bare id and would
    /// double-prefix a full entity name into a nonexistent user).
    pub user: String,
}

impl CephProviderConfig {
    /// Validate the shape (non-empty fsid/pool/user, 1..=9 non-empty
    /// monitor entries).
    ///
    /// # Errors
    /// Returns an `INVALID_REQUEST` [`ApiError`] naming the offending
    /// field; this never touches the cluster.
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.cluster_fsid.trim().is_empty() {
            return Err(ApiError::invalid_request("cluster_fsid must not be empty"));
        }
        if self.mon_hosts.is_empty() || self.mon_hosts.len() > 9 {
            return Err(ApiError::invalid_request(
                "mon_hosts must contain between 1 and 9 entries",
            ));
        }
        if self.mon_hosts.iter().any(|mon| mon.trim().is_empty()) {
            return Err(ApiError::invalid_request(
                "mon_hosts entries must not be empty",
            ));
        }
        if self.pool.trim().is_empty() {
            return Err(ApiError::invalid_request("pool must not be empty"));
        }
        if self.user.trim().is_empty() {
            return Err(ApiError::invalid_request("user must not be empty"));
        }
        Ok(())
    }
}

/// The external-cluster Ceph RBD provider.
///
/// All state lives in the durable JSON state file (see [`CephState`]);
/// the in-memory `Mutex` only serializes access within this daemon. The
/// constructor runs the fail-closed startup verification (FSID match,
/// pool existence, health query) and then a reconciliation pass that
/// marks vanished/mismatched volumes `Failed` (clearing their stale
/// attachment records so detach/delete are not wedged forever) and
/// reports foreign images without touching them.
pub struct CephRbdProvider {
    /// Shell-free command executor for the ceph/rbd toolchain.
    runner: Arc<dyn CommandRunner>,
    /// Verified cluster parameters.
    config: CephProviderConfig,
    /// Path of the durable JSON state file.
    state_path: PathBuf,
    state: Mutex<CephState>,
    /// The most recent reconcile report (the startup pass or the last
    /// explicit [`reconcile`](Self::reconcile) call), kept so the audit
    /// trail of destructive-looking bookkeeping — e.g. attachment
    /// records cleared because their backing vanished or turned
    /// foreign — survives past the state change it describes. Read it
    /// with [`last_reconcile_report`](Self::last_reconcile_report).
    last_reconcile: Mutex<Option<ReconcileReport>>,
}

impl CephRbdProvider {
    /// Construct the provider.
    ///
    /// Fail-closed startup verification first (Volume API v2 / ADR-0005:
    /// a mis-pointed cluster must never be adopted): `ceph fsid` must
    /// report exactly `config.cluster_fsid`, `ceph df` must list
    /// `config.pool`, and a `ceph health detail` query must succeed. Only
    /// then is the durable state loaded and reconciled; on any failure
    /// nothing is written.
    ///
    /// # Errors
    /// Returns a typed [`ApiError`]: `FOREIGN_DEVICE_STATE` on an FSID
    /// mismatch, `NOT_FOUND` on a missing pool, `CEPH_CLUSTER_UNHEALTHY`
    /// on an unusable health/fsid query, `INTERNAL` on command or state
    /// failures, `INVALID_REQUEST` on a malformed config.
    pub fn new(
        runner: Arc<dyn CommandRunner>,
        config: CephProviderConfig,
        state_path: PathBuf,
    ) -> Result<Self, ApiError> {
        config.validate()?;
        let provider = Self {
            runner,
            config,
            state_path,
            state: Mutex::new(CephState::default()),
            last_reconcile: Mutex::new(None),
        };
        provider.verify_startup()?;
        let state = CephState::load(&provider.state_path)?;
        *provider.lock_state()? = state;
        provider.reconcile()?;
        Ok(provider)
    }

    /// Lock the in-memory state, mapping poisoning to `INTERNAL`.
    fn lock_state(&self) -> Result<MutexGuard<'_, CephState>, ApiError> {
        self.state.lock().map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "ceph provider state lock poisoned by a previous failure",
            )
        })
    }

    /// Lock the last-reconcile slot, mapping poisoning to `INTERNAL`.
    fn lock_last_reconcile(&self) -> Result<MutexGuard<'_, Option<ReconcileReport>>, ApiError> {
        self.last_reconcile.lock().map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "ceph provider last-reconcile lock poisoned by a previous failure",
            )
        })
    }

    /// The most recent reconcile report: the startup pass performed at
    /// construction, or the last explicit
    /// [`reconcile`](Self::reconcile) call.
    ///
    /// The startup report is retained precisely because construction
    /// reconciles before anything can observe it: without this
    /// accessor, the audit trail of records the startup pass cleared
    /// (see
    /// [`ReconcileReport::cleared_attachments`](crate::state::ReconcileReport::cleared_attachments))
    /// would be lost.
    ///
    /// # Errors
    /// Returns a typed [`ApiError`] (`INTERNAL`) only when the
    /// reporting slot's lock is poisoned.
    pub fn last_reconcile_report(&self) -> Result<Option<ReconcileReport>, ApiError> {
        Ok(self.lock_last_reconcile()?.clone())
    }

    // -- Command plumbing (argv arrays; -m and --name on every invocation) --

    /// The shared argv prefix of every `ceph`/`rbd` invocation.
    ///
    /// The user is passed via `--name` because the config carries the
    /// FULL entity name (e.g. `client.volvisor`); `--id` takes a bare id
    /// and would authenticate as the nonexistent
    /// `client.client.volvisor`.
    fn base_args(&self) -> Vec<String> {
        vec![
            "-m".to_owned(),
            self.config.mon_hosts.join(","),
            "--name".to_owned(),
            self.config.user.clone(),
        ]
    }

    /// Run `program` with the shared prefix plus `tail`.
    fn run_with(&self, program: &str, tail: &[&str]) -> Result<CommandOutput, ApiError> {
        let mut args = self.base_args();
        args.extend(tail.iter().map(|arg| (*arg).to_owned()));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner.run(program, &refs)
    }

    /// Run a `ceph` CLI command.
    fn run_ceph(&self, tail: &[&str]) -> Result<CommandOutput, ApiError> {
        self.run_with("ceph", tail)
    }

    /// Run an `rbd` CLI command.
    fn run_rbd(&self, tail: &[&str]) -> Result<CommandOutput, ApiError> {
        self.run_with("rbd", tail)
    }

    /// The `pool/image` spec for an image name.
    fn image_spec(&self, image_name: &str) -> String {
        format!("{}/{}", self.config.pool, image_name)
    }

    // -- Cluster queries --

    /// Run `ceph health detail --format json`.
    ///
    /// # Errors
    /// `CEPH_CLUSTER_UNHEALTHY` when the query cannot be executed or the
    /// CLI exits non-zero; `INTERNAL` on unparseable output.
    fn run_health(&self) -> Result<HealthDetail, ApiError> {
        let output = self.run_ceph(&["health", "detail", "--format", "json"])?;
        if !output.success {
            return Err(ApiError::new(
                ApiErrorCode::CephClusterUnhealthy,
                format!("ceph health query failed: {}", output.stderr_excerpt()),
            ));
        }
        parse_health_detail(&output.stdout)
    }

    /// The pool's capacity picture from one `ceph df` query.
    ///
    /// # Errors
    /// `INTERNAL` when the query fails or the configured pool (or its
    /// `max_avail`) is not reported — capacity is never guessed.
    fn pool_stats(&self) -> Result<PoolCapacity, ApiError> {
        let output = self.run_ceph(&["df", "--format", "json"])?;
        if !output.success {
            return Err(command_failed("ceph df", &output));
        }
        let df = parse_ceph_df(&output.stdout)?;
        let pool = find_pool(&df.pools, &self.config.pool).ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "pool {:?} is not reported by ceph df (it existed at startup)",
                    self.config.pool
                ),
            )
        })?;
        let max_avail = pool.max_avail().ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "ceph df did not report max_avail for pool {:?}",
                    self.config.pool
                ),
            )
        })?;
        Ok(PoolCapacity {
            // bytes_used is informational only; absence is not a capacity
            // fact worth failing over.
            bytes_used: pool.bytes_used().unwrap_or_default(),
            max_avail,
        })
    }

    /// The image names in the configured pool, from `rbd ls`.
    fn list_images(&self) -> Result<Vec<String>, ApiError> {
        let output = self.run_rbd(&["ls", "--pool", &self.config.pool, "--format", "json"])?;
        if !output.success {
            return Err(command_failed("rbd ls", &output));
        }
        parse_image_list(&output.stdout)
    }

    /// The trash contents of the configured pool, from `rbd trash ls`.
    fn list_trash(&self) -> Result<Vec<String>, ApiError> {
        let output = self.run_rbd(&[
            "trash",
            "ls",
            "--pool",
            &self.config.pool,
            "--format",
            "json",
        ])?;
        if !output.success {
            return Err(command_failed("rbd trash ls", &output));
        }
        parse_trash_list(&output.stdout)
    }

    /// The `rbd info` record of one image.
    fn image_info(&self, image_name: &str) -> Result<RbdInfo, ApiError> {
        let spec = self.image_spec(image_name);
        let output = self.run_rbd(&["info", "--format", "json", &spec])?;
        if !output.success {
            return Err(command_failed("rbd info", &output));
        }
        parse_rbd_info(&output.stdout)
    }

    /// Read one RBD image metadata key.
    ///
    /// `Ok(None)` means *genuine key absence* only: a non-zero exit whose
    /// stderr carries an ENOENT-flavored message (real `rbd image-meta
    /// get` on an absent key prints e.g. `failed to get metadata <key> of
    /// image : (2) No such file or directory`; some builds spell the
    /// errno name or `No such attribute` instead). Every OTHER non-zero
    /// exit — a transient mon timeout, a permission failure — is a typed
    /// `INTERNAL` error, never a silent `None`: conflating the two would
    /// persist `Failed` on healthy volumes out of a mere outage.
    fn image_meta_get(&self, image_name: &str, key: &str) -> Result<Option<String>, ApiError> {
        let spec = self.image_spec(image_name);
        let output = self.run_rbd(&["image-meta", "get", &spec, key])?;
        if output.success {
            return Ok(Some(output.stdout.trim().to_owned()));
        }
        if is_key_absent(&output.stderr) {
            Ok(None)
        } else {
            Err(command_failed("rbd image-meta get", &output))
        }
    }

    /// Write one RBD image metadata key.
    fn image_meta_set(
        &self,
        image_name: &str,
        key: &str,
        value: &str,
    ) -> Result<CommandOutput, ApiError> {
        let spec = self.image_spec(image_name);
        self.run_rbd(&["image-meta", "set", &spec, key, value])
    }

    /// The current `rbd showmapped` mappings.
    fn showmapped(&self) -> Result<Vec<MappedDevice>, ApiError> {
        let output = self.run_rbd(&["showmapped", "--format", "json"])?;
        if !output.success {
            return Err(command_failed("rbd showmapped", &output));
        }
        parse_showmapped(&output.stdout)
    }

    /// Verify a state entry's backing image: existence (from a successful
    /// `rbd ls`), size (from `rbd info`) and ownership metadata.
    fn verify_backing(&self, image_name: &str, volume_id: &VolumeId) -> Result<Backing, ApiError> {
        if !self.list_images()?.iter().any(|name| name == image_name) {
            return Ok(Backing::Absent);
        }
        let info = self.image_info(image_name)?;
        let size = info.size_bytes().ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "rbd info did not report a size for {}",
                    self.image_spec(image_name)
                ),
            )
        })?;
        match self.image_meta_get(image_name, OWNER_META_KEY)? {
            Some(owner) if owner == volume_id.as_str() => Ok(Backing::Owned { size_bytes: size }),
            _ => Ok(Backing::Mismatch),
        }
    }

    /// Best-effort `rbd rm` used to clean up a failed create.
    ///
    /// The outcome is deliberately swallowed: the caller is already on an
    /// error path, and a failing cleanup must not mask the original
    /// failure (no orphaned half-created image is left behind on the
    /// happy cleanup path).
    fn remove_image_best_effort(&self, image_name: &str) {
        let spec = self.image_spec(image_name);
        drop(self.run_rbd(&["rm", &spec]));
    }

    /// The fail-closed startup verification (see [`Self::new`]).
    fn verify_startup(&self) -> Result<(), ApiError> {
        let output = self.run_ceph(&["fsid"])?;
        if !output.success {
            return Err(ApiError::new(
                ApiErrorCode::CephClusterUnhealthy,
                format!("ceph fsid query failed: {}", output.stderr_excerpt()),
            ));
        }
        let fsid = parse_fsid(&output.stdout)?;
        if fsid != self.config.cluster_fsid {
            return Err(ApiError::new(
                ApiErrorCode::ForeignDeviceState,
                format!(
                    "cluster fsid mismatch; refusing to adopt a different cluster (configured \
                     {}, observed {fsid})",
                    self.config.cluster_fsid
                ),
            ));
        }
        let output = self.run_ceph(&["df", "--format", "json"])?;
        if !output.success {
            return Err(command_failed("ceph df", &output));
        }
        let df = parse_ceph_df(&output.stdout)?;
        if find_pool(&df.pools, &self.config.pool).is_none() {
            return Err(ApiError::not_found(format!(
                "configured pool {:?} does not exist in the cluster",
                self.config.pool
            )));
        }
        self.run_health()?;
        Ok(())
    }

    // -- Reconciliation and read-only discovery --

    /// The size-agreement outcome for a verifiably owned image.
    fn size_agreement(&self, image_name: &str, recorded: u64) -> SizeAgreement {
        match self.image_info(image_name) {
            Ok(info) => match info.size_bytes() {
                Some(actual) if actual > recorded => SizeAgreement::Grown(actual),
                Some(actual) if actual < recorded => SizeAgreement::Shrunk,
                Some(_) => SizeAgreement::Agree,
                None => SizeAgreement::Unknown(format!(
                    "rbd info did not report a size for {image_name}"
                )),
            },
            Err(error) => SizeAgreement::Unknown(error.detail),
        }
    }

    /// Mark a volume `Failed` and clear its attachment record.
    ///
    /// Used when the backing is verifiably absent or no longer provably
    /// ours: the attachment record's authority claim is void, and
    /// keeping it would wedge the volume forever (detach refuses on the
    /// absent device, delete refuses on "must be fully detached"; the
    /// documented restart remedy relies on this clear). Only the RECORD
    /// is dropped — the mapping itself is never auto-unmapped, an
    /// actual zombie device is left to an operator. The caller persists
    /// when `changed` says something moved.
    fn fail_and_clear_attachment(
        state: &mut CephState,
        id: &VolumeId,
        snapshot: &StoredVolume,
        mappings: Option<&[MappedDevice]>,
        reason: ClearedAttachmentReason,
        report: &mut ReconcileReport,
        changed: &mut bool,
    ) {
        if let Some(volume) = state.volume_mut(id) {
            if snapshot.runtime.state != VolumeLifecycle::Failed {
                volume.runtime.state = VolumeLifecycle::Failed;
                *changed = true;
            }
            if snapshot.runtime.attachment.is_some() {
                volume.runtime.attachment = None;
                *changed = true;
            }
        }
        // Preserve the cleared record in the report as the audit
        // trail: the device it named, and whether a live mapping still
        // exists over the gone/foreign backing (a zombie left for an
        // operator — never auto-unmapped).
        if let Some(record) = snapshot.runtime.attachment.as_ref() {
            report.cleared_attachments.push(ClearedAttachment {
                volume_id: id.clone(),
                device: record.device.clone(),
                zombie_mapping: mappings.map(|maps| {
                    maps.iter()
                        .any(|m| m.name.as_deref() == Some(snapshot.entry.image_name.as_str()))
                }),
                reason,
            });
        }
    }

    /// Reconcile provider state against the observed cluster.
    ///
    /// Non-destructive by construction (nothing is unmapped, adopted or
    /// removed here):
    ///
    /// - a state entry whose image is verifiably absent (from a
    ///   successful `rbd ls`) is marked `Failed` and persisted — never
    ///   silently recreated or dropped — and its attachment record is
    ///   cleared: the backing that record referenced no longer exists,
    ///   so keeping it would wedge the volume forever (detach refuses
    ///   on the absent device, delete refuses on "must be fully
    ///   detached"; the documented restart remedy now actually works).
    ///   The mapping itself is never auto-unmapped — an actual zombie
    ///   device, if any, is left to an operator and reported (with the
    ///   device the record named) in
    ///   [`ReconcileReport::cleared_attachments`];
    /// - a state entry whose `volvisor.owner` metadata is missing or
    ///   names a different volume is marked `Failed` with its
    ///   attachment record cleared for the same reason (the backing is
    ///   no longer provably ours) — never adopted;
    /// - a state entry whose image reports LESS than the recorded size
    ///   (a shrink outside volvisor) is marked `Failed`; MORE is a
    ///   completed-but-unrecorded grow and the recorded size is healed
    ///   up to the image's actual report (counted in
    ///   [`ReconcileReport::healed_grown`]);
    /// - a volume whose image is mapped while no attachment record exists
    ///   (a stale mapping from a previous incarnation) is marked `Failed`
    ///   and reported — the mapping is never unmapped automatically
    ///   (destructive); `Failed` is terminal, so unmaps stay manual;
    /// - an attachment record whose device is *verifiably absent* from a
    ///   successful `rbd showmapped` (an interrupted detach: unmap
    ///   succeeded, the state save did not) is cleared and the volume
    ///   returns to `Ready` — state matches observed reality, mirroring
    ///   the LVM release reconciliation;
    /// - a volume whose verification could not complete (a transient
    ///   query failure, e.g. a mon timeout) is left COMPLETELY untouched
    ///   and counted as unverifiable — an unknown is never persisted as
    ///   `Failed`;
    /// - images without our metadata are foreign: reported, never touched.
    ///
    /// # Errors
    /// Returns an [`ApiError`] when the `rbd ls` query cannot be executed
    /// (reconciliation then never ran — an honest unknown, never an empty
    /// report implying consistency).
    pub fn reconcile(&self) -> Result<ReconcileReport, ApiError> {
        let listed = self.list_images()?;
        // None = the query failed (honest unknown): keep every attachment.
        let mappings: Option<Vec<MappedDevice>> = self.showmapped().ok();

        let mut state = self.lock_state()?;
        let mut report = ReconcileReport::default();
        let mut changed = false;
        let ids: Vec<VolumeId> = state.volumes().keys().cloned().collect();
        for id in ids {
            let Some(snapshot) = state.volume(&id).cloned() else {
                continue;
            };
            let image_name = snapshot.entry.image_name.as_str();
            if !listed.iter().any(|name| name == image_name) {
                // Verifiably absent image: Failed, and the attachment
                // record (whose backing no longer exists) is cleared so
                // the volume is not wedged forever.
                Self::fail_and_clear_attachment(
                    &mut state,
                    &id,
                    &snapshot,
                    mappings.as_deref(),
                    ClearedAttachmentReason::VanishedImage,
                    &mut report,
                    &mut changed,
                );
                report.missing_volumes.push(id);
                continue;
            }
            // The image exists: ownership decides everything else.
            let owner = self.image_meta_get(image_name, OWNER_META_KEY);
            match owner {
                Ok(Some(owner)) if owner == id.as_str() => {
                    // Size agreement against the image's own report.
                    match self.size_agreement(image_name, snapshot.entry.size_bytes) {
                        SizeAgreement::Grown(actual) => {
                            // A completed-but-unrecorded grow (the crash
                            // window after `rbd resize`): heal the
                            // bookkeeping up to the image's report.
                            if let Some(volume) = state.volume_mut(&id) {
                                volume.entry.size_bytes = actual;
                                changed = true;
                            }
                            report.healed_grown.push(id.clone());
                        }
                        SizeAgreement::Shrunk => {
                            // The image changed outside volvisor: Failed,
                            // never healed downward.
                            if snapshot.runtime.state != VolumeLifecycle::Failed {
                                if let Some(volume) = state.volume_mut(&id) {
                                    volume.runtime.state = VolumeLifecycle::Failed;
                                    changed = true;
                                }
                            }
                            report.shrunk_volumes.push(id.clone());
                        }
                        SizeAgreement::Unknown(detail) => {
                            // Transient read failure: an honest unknown,
                            // never a Failed from an outage.
                            report.unverifiable_volumes.push(UnverifiableVolume {
                                volume_id: id.clone(),
                                detail,
                            });
                        }
                        SizeAgreement::Agree => {}
                    }
                    Self::reconcile_owned_attachment(
                        &mut state,
                        &id,
                        &snapshot,
                        image_name,
                        mappings.as_deref(),
                        &mut report,
                        &mut changed,
                    );
                }
                Ok(_) => {
                    // Missing or foreign owner metadata: never adopted.
                    // Failed, and the attachment record (whose backing
                    // is no longer provably ours) is cleared so the
                    // volume is not wedged forever; the image itself is
                    // never touched.
                    Self::fail_and_clear_attachment(
                        &mut state,
                        &id,
                        &snapshot,
                        mappings.as_deref(),
                        ClearedAttachmentReason::OwnershipMismatch,
                        &mut report,
                        &mut changed,
                    );
                    report.mismatched_volumes.push(id);
                }
                Err(error) => {
                    // Honest unknown (e.g. a transient mon timeout): leave
                    // the entry untouched and count it — never persist
                    // Failed from an outage.
                    report.unverifiable_volumes.push(UnverifiableVolume {
                        volume_id: id,
                        detail: error.detail,
                    });
                }
            }
        }

        // Foreign / untracked pass: images we have no state for. Never
        // touched, only reported (AGENTS rule 7).
        self.reconcile_untracked_images(&state, &listed, &mut report);

        if changed {
            state.save(&self.state_path)?;
        }
        // Retain the report (audit trail) before handing it to the
        // caller: construction's startup pass discards the return
        // value, and records it cleared must stay observable via
        // `last_reconcile_report`.
        *self.lock_last_reconcile()? = Some(report.clone());
        Ok(report)
    }

    /// The attachment-record pass of [`Self::reconcile`] for a volume
    /// whose image exists and is verifiably owned.
    ///
    /// A record whose device is *verifiably absent* from a successful
    /// `rbd showmapped` (an interrupted detach: unmap succeeded, the
    /// state save did not) is cleared and the volume returns to
    /// `Ready`; a mapping without a record (a stale mapping from a
    /// previous incarnation) marks the volume `Failed` and is reported
    /// — never unmapped automatically (destructive).
    #[allow(clippy::too_many_arguments)]
    fn reconcile_owned_attachment(
        state: &mut CephState,
        id: &VolumeId,
        snapshot: &StoredVolume,
        image_name: &str,
        mappings: Option<&[MappedDevice]>,
        report: &mut ReconcileReport,
        changed: &mut bool,
    ) {
        let mapped =
            mappings.is_some_and(|maps| maps.iter().any(|m| m.name.as_deref() == Some(image_name)));
        if let Some(record) = snapshot.runtime.attachment.as_ref() {
            // Record exists: a verifiably absent device means an
            // interrupted detach — reconcile forward.
            let device_present = mappings.is_some_and(|maps| {
                maps.iter()
                    .any(|m| m.device.as_deref() == Some(record.device.as_str()))
            });
            if mappings.is_some() && !device_present {
                if let Some(volume) = state.volume_mut(id) {
                    volume.runtime.attachment = None;
                    if volume.runtime.state == VolumeLifecycle::Attached {
                        volume.runtime.state = VolumeLifecycle::Ready;
                    }
                    *changed = true;
                }
            }
        } else if mapped {
            // Stale mapping without an attachment record: visible
            // and Failed, never auto-unmapped (destructive).
            if snapshot.runtime.state != VolumeLifecycle::Failed {
                if let Some(volume) = state.volume_mut(id) {
                    volume.runtime.state = VolumeLifecycle::Failed;
                    *changed = true;
                }
            }
            report.stale_mappings.push(id.clone());
        }
    }

    /// The untracked-images pass of [`Self::reconcile`]: classify images
    /// we have no state entry for (foreign / owned-but-untracked /
    /// unverifiable) without touching any of them (AGENTS rule 7).
    fn reconcile_untracked_images(
        &self,
        state: &CephState,
        listed: &[String],
        report: &mut ReconcileReport,
    ) {
        let our_images: Vec<String> = state
            .volumes()
            .values()
            .map(|volume| volume.entry.image_name.clone())
            .collect();
        for image_name in listed {
            if our_images.contains(image_name) {
                continue;
            }
            match self.image_meta_get(image_name, OWNER_META_KEY) {
                Ok(Some(_)) => report.untracked_owned_images.push(image_name.clone()),
                Ok(None) => report.foreign_images.push(image_name.clone()),
                Err(error) => {
                    // A transient read failure must not classify the
                    // image as foreign: count it as unverifiable.
                    report.unverifiable_images.push(UnverifiableImage {
                        image_name: image_name.clone(),
                        detail: error.detail,
                    });
                }
            }
        }
    }

    /// Read-only pool discovery: the configured pool as a [`Pool`], with
    /// capacity from `ceph df`, health reflected from `ceph health`
    /// (query failure → `Unknown`, never fabricated) and replication
    /// reported from the pool's own policy (`ceph osd pool get`). No
    /// cluster mutation of any kind; foreign images are invisible here
    /// (they are a reconcile concern, not a pool fact).
    ///
    /// # Errors
    /// Returns an [`ApiError`] when `ceph df` cannot establish the pool's
    /// capacity or `ceph osd pool get` cannot establish its replication
    /// policy (the pool list is then unknown, not guessed — replication
    /// is never asserted from absent facts).
    pub fn discover_pools(&self) -> Result<Vec<Pool>, ApiError> {
        let stats = self.pool_stats()?;
        let policy = self.pool_replication_policy()?;
        let health = self
            .run_health()
            .map_or(Health::Unknown, |detail| detail.health());
        let pool_id = PoolId::new(format!("ceph-{}", self.config.cluster_fsid)).map_err(|e| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("derived an invalid pool identity: {e}"),
            )
        })?;
        Ok(vec![Pool {
            id: pool_id,
            backend_class: VolumeClass::CephRbd,
            // External cluster: no local devices under volvisor's claim.
            device_ids: Vec::new(),
            host_or_ceph_cluster: self.config.cluster_fsid.clone(),
            // Estimate from the same statistics the capacity checks use;
            // bytes_used is 0 when ceph df does not report it.
            capacity_bytes: stats.bytes_used.saturating_add(stats.max_avail),
            allocatable_bytes: stats.max_avail.saturating_sub(CEPH_HEADROOM_BYTES),
            protection: PoolProtection {
                // Never claim a local mirror beneath Ceph (contract §6).
                local_mirror: false,
                // Remote replication is a pool POLICY fact, established
                // only from the policy query (`osd pool get size`), never
                // inferred from capacity output: a replicated pool
                // (size >= 2) keeps redundancy across failure domains; a
                // size-1 pool has no remote copy at all.
                remote_replication: policy.size >= 2,
            },
            health,
        }])
    }

    /// The pool's replication policy facts, from `ceph osd pool get`.
    ///
    /// `ceph df` does not report replication; the true (read-only)
    /// policy query is `ceph osd pool get <pool> size|min_size
    /// --format json` (real output: `{"size":"3"}`). Used only by
    /// [`Self::discover_pools`] — the per-volume inspect path stays
    /// cheap and makes no policy queries.
    ///
    /// # Errors
    /// `INTERNAL` when either query fails or its output is garbage — a
    /// policy fact is never guessed.
    fn pool_replication_policy(&self) -> Result<PoolReplicationPolicy, ApiError> {
        let policy = PoolReplicationPolicy {
            size: self.pool_policy_value("size")?,
            min_size: self.pool_policy_value("min_size")?,
        };
        // A policy whose min_size exceeds its size cannot exist on a
        // healthy cluster: garbage data is never adopted as a fact.
        if policy.min_size > policy.size {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "ceph osd pool get reported min_size {} above size {} for pool {:?}: \
                     garbage policy data is never adopted",
                    policy.min_size, policy.size, self.config.pool
                ),
            ));
        }
        Ok(policy)
    }

    /// One numeric pool-policy field from `ceph osd pool get`.
    fn pool_policy_value(&self, key: &str) -> Result<u64, ApiError> {
        let output = self.run_ceph(&[
            "osd",
            "pool",
            "get",
            &self.config.pool,
            key,
            "--format",
            "json",
        ])?;
        if !output.success {
            return Err(command_failed("ceph osd pool get", &output));
        }
        parse_pool_policy_value(&output.stdout, key)
    }

    // -- Volume operations (sync bodies behind the async trait surface) --

    /// Create the RBD image for `volume_id`, stamp the ownership record
    /// and verify it read-back.
    ///
    /// Returns the effective size `rbd info` reported. On any
    /// verification failure the half-created image is removed
    /// (best-effort `rbd rm`) and a typed `INTERNAL` error is returned,
    /// so neither an orphaned image nor a state entry survives a failed
    /// create.
    ///
    /// Crash-window recovery: if `rbd create` fails because the image
    /// already exists — the signature of a crash after
    /// [`Self::create_owned_image`] but before the caller's state save —
    /// the existing image is RECLAIMED when (and only when) its
    /// `volvisor.owner` metadata names this very volume: the ownership
    /// record is the proof that the image is ours, so adopting it cannot
    /// touch foreign state (see [`Self::reclaim_owned_image`]).
    fn create_owned_image(
        &self,
        volume_id: &VolumeId,
        requested_bytes: u64,
    ) -> Result<u64, ApiError> {
        let image_name = image_name_for(volume_id);
        let spec = self.image_spec(&image_name);
        let output = self.run_rbd(&[
            "create",
            "--image-feature",
            IMAGE_FEATURES,
            "-s",
            &format!("{requested_bytes}B"),
            &spec,
        ])?;
        if !output.success {
            // The image may be our own half-created orphan from a crash
            // between create and the state save: reclaim it when the
            // ownership metadata proves it, fail typed otherwise.
            return self.reclaim_owned_image(volume_id, requested_bytes, &output);
        }
        // Stamp the ownership record and verify it read-back before any
        // state is persisted: an image whose ownership cannot be proven is
        // removed (best-effort rbd rm) and the create fails honestly.
        for (key, value) in [
            (OWNER_META_KEY, volume_id.as_str()),
            (GENERATION_META_KEY, "1"),
        ] {
            let output = self.image_meta_set(&image_name, key, value)?;
            if !output.success {
                self.remove_image_best_effort(&image_name);
                return Err(command_failed("rbd image-meta set", &output));
            }
        }
        let effective = match self.verify_backing(&image_name, volume_id)? {
            Backing::Owned { size_bytes } if size_bytes >= requested_bytes => size_bytes,
            Backing::Owned { size_bytes } => {
                self.remove_image_best_effort(&image_name);
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "image size mismatch after rbd create of {spec}: requested \
                         {requested_bytes} bytes, rbd info reports {size_bytes} bytes"
                    ),
                ));
            }
            Backing::Absent => {
                self.remove_image_best_effort(&image_name);
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!("rbd create reported success but rbd ls does not list {spec}"),
                ));
            }
            Backing::Mismatch => {
                self.remove_image_best_effort(&image_name);
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!("ownership metadata read-back mismatch for {spec}"),
                ));
            }
        };
        if self
            .image_meta_get(&image_name, GENERATION_META_KEY)?
            .as_deref()
            != Some("1")
        {
            self.remove_image_best_effort(&image_name);
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!("generation metadata read-back mismatch for {spec}"),
            ));
        }
        Ok(effective)
    }

    /// Reclaim our own half-created image after a crash window, or fail
    /// typed.
    ///
    /// A create retry that hits "already exists" would otherwise be
    /// wedged forever; the escape hatch is the ownership record itself:
    /// an image whose `volvisor.owner` metadata equals THIS volume id
    /// was created by a previous incarnation of this very create, so
    /// adopting it touches no foreign state (AGENTS rule 7). The image
    /// must be at least as large as the request (the actual size becomes
    /// the effective size) and a missing `volvisor.generation` record is
    /// re-stamped. An image whose ownership metadata is absent, differs
    /// or cannot be read is NEVER adopted — a typed
    /// `FOREIGN_DEVICE_STATE` conflict (or the original create error,
    /// when the image does not exist at all).
    fn reclaim_owned_image(
        &self,
        volume_id: &VolumeId,
        requested_bytes: u64,
        create_output: &CommandOutput,
    ) -> Result<u64, ApiError> {
        let image_name = image_name_for(volume_id);
        let spec = self.image_spec(&image_name);
        match self.verify_backing(&image_name, volume_id)? {
            Backing::Owned { size_bytes } => {
                if size_bytes < requested_bytes {
                    return Err(ApiError::new(
                        ApiErrorCode::Internal,
                        format!(
                            "orphaned image {spec} carries volume {volume_id}'s ownership \
                             record but is smaller than the request ({size_bytes} < \
                             {requested_bytes} bytes); refusing to adopt it"
                        ),
                    ));
                }
                // Re-stamp the generation record when the crash window
                // closed before it was written.
                if self
                    .image_meta_get(&image_name, GENERATION_META_KEY)?
                    .is_none()
                {
                    let output = self.image_meta_set(&image_name, GENERATION_META_KEY, "1")?;
                    if !output.success {
                        return Err(command_failed("rbd image-meta set", &output));
                    }
                }
                Ok(size_bytes)
            }
            // The create failure was real (the image does not exist):
            // surface the original error untouched.
            Backing::Absent => Err(command_failed("rbd create", create_output)),
            // An existing image without our ownership record is foreign:
            // never adopted, never destroyed.
            Backing::Mismatch => Err(ApiError::new(
                ApiErrorCode::ForeignDeviceState,
                format!(
                    "image {spec} already exists and does not carry the ownership record of \
                     volume {volume_id}; foreign state is never adopted"
                ),
            )),
        }
    }

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
        // comparison uses the *requested* size (RBD is byte-granular, so
        // requested and effective coincide, but the distinction is kept
        // mirroring the LVM provider).
        if let Some(existing) = state.volume(&req.volume_id) {
            if existing.entry.creation_payload == payload
                && existing.entry.requested_size_bytes == req.size_bytes
            {
                // The replay routes through the same backing verification
                // as a fresh inspect: a replay must never report `Ready`
                // for an image that was manually removed behind the
                // provider's back (the observed health is reflected, the
                // persisted state is left to reconcile).
                return self.verified_inspect_response(&req.volume_id, existing);
            }
            return Err(ApiError::idempotency_conflict(&req.volume_id));
        }

        // Capacity envelope: the request plus headroom must fit into the
        // pool's max_avail (typed NO_SAFE_CAPACITY before the pool fills).
        //
        // Advisory by design: `max_avail` from `ceph df` is an ESTIMATE
        // that races with concurrent writers on a shared external
        // cluster, so this check only refuses obviously-unsafe requests
        // up front — over-commitment is not prevented, it surfaces as
        // Ceph's own ENOSPC at write time (RBD images are
        // thin-provisioned and allocate lazily). The P2 plan already
        // records quota-based enforcement as the follow-up.
        let capacity = self.pool_stats()?;
        if req.size_bytes.saturating_add(CEPH_HEADROOM_BYTES) > capacity.max_avail {
            return Err(ApiError::new(
                ApiErrorCode::NoSafeCapacity,
                format!(
                    "no safe capacity: pool {} has {} bytes available, request needs {} plus \
                     {} headroom",
                    self.config.pool, capacity.max_avail, req.size_bytes, CEPH_HEADROOM_BYTES
                ),
            ));
        }

        let image_name = image_name_for(&req.volume_id);
        let effective = self.create_owned_image(&req.volume_id, req.size_bytes)?;

        let stored = StoredVolume {
            entry: VolumeEntry {
                image_name,
                size_bytes: effective,
                requested_size_bytes: req.size_bytes,
                generation: 1,
                project_id: req.project_id.clone(),
                block_size: req.logical_block_size.unwrap_or(DEFAULT_BLOCK_SIZE),
                creation_payload: payload,
                created_at: unix_now(),
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

    /// Build the inspect response for one stored volume, verifying the
    /// backing image exists and is still ours.
    ///
    /// A verifiably absent or mismatched image is reported as `Failed`
    /// (honest observation) — the persisted state is left alone so the
    /// next reconcile owns the transition. A volume already stored as
    /// `Failed` stays `Failed` (no implicit recovery). Size is reflected
    /// from the image's own report: an image LARGER than recorded is a
    /// completed-but-unrecorded grow (the crash window after
    /// `rbd resize`), so the observed size is reported while reconcile
    /// heals the record; an image SMALLER than recorded changed outside
    /// volvisor and is reported `Failed`.
    ///
    /// The remote protection axis of the response is
    /// [`RemoteProtectionAxis::None`]: on this cheap per-volume path
    /// that means "remote protection not established **by this path**"
    /// (no policy query runs here) — it does NOT assert the absence of
    /// remote copies. Query [`CephRbdProvider::discover_pools`] for the
    /// pool's actual replication policy.
    fn verified_inspect_response(
        &self,
        volume_id: &VolumeId,
        stored: &StoredVolume,
    ) -> Result<InspectVolumeResponse, ApiError> {
        let mut response = inspect_response(volume_id, stored);
        if stored.runtime.state == VolumeLifecycle::Failed {
            return Ok(response);
        }
        match self.verify_backing(&stored.entry.image_name, volume_id)? {
            Backing::Owned { size_bytes } => {
                if size_bytes > stored.entry.size_bytes {
                    // Observed reality outranks the stale record; the
                    // persisted heal belongs to reconcile.
                    response.provisioned_bytes = size_bytes;
                    response.allocated_bytes = size_bytes;
                } else if size_bytes < stored.entry.size_bytes {
                    // The response keeps the RECORDED size fields: the
                    // recorded size is the last volvisor-provisioned
                    // truth, and the shrunk actual is a violation being
                    // surfaced as `Failed` — not a new size to adopt
                    // (a shrink is never healed downward).
                    response.state = VolumeLifecycle::Failed;
                }
                Ok(response)
            }
            Backing::Absent | Backing::Mismatch => {
                response.state = VolumeLifecycle::Failed;
                Ok(response)
            }
        }
    }

    /// Fail-closed on a stale pre-existing mapping (crash leftover).
    ///
    /// The mapping is evidence of a possible foreign writer, so it is
    /// never silently adopted or auto-unmapped: the attach is refused
    /// until an operator unmaps the device.
    fn ensure_not_mapped(&self, spec: &str) -> Result<(), ApiError> {
        let mappings = self.showmapped()?;
        if let Some(existing) = mappings
            .iter()
            .find(|mapping| mapping.pool_slash_image().as_deref() == Some(spec))
        {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "image {spec} is already mapped as {}: stale mapping from a previous \
                     incarnation; unmap the rbd device first",
                    existing.device.as_deref().unwrap_or("an unknown device"),
                ),
            ));
        }
        Ok(())
    }

    /// The ownership proof required before a mutation.
    ///
    /// `absent_code` selects the typed error for a verifiably absent
    /// image (`INVALID_STATE` on attach, `INTERNAL` on grow — the state
    /// entry demands reconciliation either way); an image whose
    /// ownership metadata is missing or foreign is always refused with
    /// `FOREIGN_DEVICE_STATE` and never adopted.
    fn require_owned_backing(
        &self,
        image_name: &str,
        volume_id: &VolumeId,
        spec: &str,
        absent_code: ApiErrorCode,
    ) -> Result<(), ApiError> {
        match self.verify_backing(image_name, volume_id)? {
            Backing::Owned { .. } => Ok(()),
            Backing::Absent => Err(ApiError::new(
                absent_code,
                format!("image {spec} is absent; volume {volume_id} requires reconciliation"),
            )),
            Backing::Mismatch => Err(ApiError::new(
                ApiErrorCode::ForeignDeviceState,
                format!(
                    "image {spec} does not carry the ownership record of volume {volume_id}; \
                     foreign state is never adopted"
                ),
            )),
        }
    }

    /// Ownership proof **and size agreement** before a grow mutation.
    ///
    /// The image must carry this volume's ownership record and report at
    /// LEAST the size the state records; the actual size is returned so
    /// the grow proceeds from observed reality. `actual < recorded` is
    /// the one violation (the image changed outside volvisor — a shrink
    /// — and the grow is refused); `actual > recorded` is a
    /// completed-but-unrecorded grow from the crash window after
    /// `rbd resize` (the state save never ran): the grow continues from
    /// the ACTUAL size and reconciliation heals the record, rather than
    /// wedging the volume behind a one-way door.
    fn verify_owned_size(
        &self,
        image_name: &str,
        volume_id: &VolumeId,
        current_size: u64,
    ) -> Result<u64, ApiError> {
        let spec = self.image_spec(image_name);
        match self.verify_backing(image_name, volume_id)? {
            Backing::Owned { size_bytes } if size_bytes >= current_size => Ok(size_bytes),
            Backing::Owned { size_bytes } => Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "image {spec} reports {size_bytes} bytes but state records {current_size}; \
                     the image changed outside volvisor (shrunk)"
                ),
            )),
            Backing::Absent => Err(ApiError::new(
                ApiErrorCode::Internal,
                format!("image {spec} is absent; volume {volume_id} requires reconciliation"),
            )),
            Backing::Mismatch => Err(ApiError::new(
                ApiErrorCode::ForeignDeviceState,
                format!("image {spec} does not carry the ownership record of volume {volume_id}"),
            )),
        }
    }

    /// Verify a fresh mapping really serves exactly this image before
    /// the handle is handed out.
    ///
    /// On mismatch the mapping is undone best-effort (no orphaned writer
    /// is left behind) and an `INTERNAL` error names the inconsistency.
    fn verify_mapping_serves(&self, device: &str, spec: &str) -> Result<(), ApiError> {
        let mappings = self.showmapped()?;
        let serves_this_image = mappings.iter().any(|mapping| {
            mapping.device.as_deref() == Some(device)
                && mapping.pool_slash_image().as_deref() == Some(spec)
        });
        if !serves_this_image {
            drop(self.run_rbd(&["unmap", device]));
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "rbd showmapped does not confirm that {device} maps {spec}; the mapping \
                     was undone best-effort"
                ),
            ));
        }
        Ok(())
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
        // Contract §3 honesty: the only mapping this prototype makes is a
        // writable, exclusive-lock-owning `rbd map` — granting a ReadOnly
        // request would hand out a writable device under a read-only
        // record (fail-open), and two ReadOnly attaches would collide on
        // the lock. Reject typed BEFORE any mutation (no rbd map, no
        // state change): a shared-reader attachment requires an explicit
        // multi-reader contract this prototype has not qualified.
        if matches!(
            req.access_mode,
            volvisor_types::request::AccessModeRequest::ReadOnly
        ) {
            return Err(unsupported(
                "read-only (shared-reader) attachments require an explicit multi-reader \
                 contract that the ceph-rbd prototype has not qualified; only read-write \
                 single-writer attachments are supported",
            ));
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
        let image_name = stored.entry.image_name.clone();
        let spec = self.image_spec(&image_name);
        self.ensure_not_mapped(&spec)?;
        self.require_owned_backing(&image_name, volume_id, &spec, ApiErrorCode::InvalidState)?;

        let output = self.run_rbd(&["map", "--image", &image_name, "--pool", &self.config.pool])?;
        if !output.success {
            // e.g. the image lacks exclusive-lock, or the cluster refuses
            // the mapping: no attachment is recorded.
            return Err(command_failed("rbd map", &output));
        }
        let device = output.stdout.trim().to_owned();
        if device.is_empty() {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!("rbd map reported success but printed no device for {spec}"),
            ));
        }
        self.verify_mapping_serves(&device, &spec)?;

        let record = AttachmentRecord {
            id: req.attachment_id.clone(),
            vm_id: req.vm_id.clone(),
            host_id: req.host_id.clone(),
            generation: 1,
            access_mode: mode,
            device,
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
        let device = record.device.clone();

        // The mapping must exist and belong to this volume's image;
        // an already-unmapped device is never silently treated as success.
        let spec = self.image_spec(&stored.entry.image_name);
        let mappings = self.showmapped()?;
        let mapping = mappings
            .iter()
            .find(|mapping| mapping.device.as_deref() == Some(device.as_str()));
        match mapping {
            None => {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "attachment {attachment_id} records device {device}, but rbd showmapped \
                         does not list it (already unmapped); if this follows an interrupted \
                         detach, restart the daemon so reconciliation clears the record"
                    ),
                ));
            }
            Some(mapping) => {
                if mapping.pool_slash_image().as_deref() != Some(spec.as_str()) {
                    return Err(ApiError::new(
                        ApiErrorCode::ForeignDeviceState,
                        format!(
                            "device {device} no longer maps {spec}; the mapping changed under \
                             the attachment and is never adopted"
                        ),
                    ));
                }
            }
        }
        let output = self.run_rbd(&["unmap", &device])?;
        if !output.success {
            return Err(command_failed("rbd unmap", &output));
        }
        // A successful exit status is not evidence: verify the mapping is
        // gone before authority is released.
        if self
            .showmapped()?
            .iter()
            .any(|mapping| mapping.device.as_deref() == Some(device.as_str()))
        {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!("rbd unmap reported success but rbd showmapped still lists {device}"),
            ));
        }

        stored.runtime.attachment = None;
        if stored.runtime.state == VolumeLifecycle::Attached {
            stored.runtime.state = VolumeLifecycle::Ready;
        }
        stored.entry.generation += 1;
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
        let (image_name, current_size, has_attachment) = {
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
            // Grow-only plus 512-alignment, fail-closed against the
            // CURRENT EFFECTIVE size under the state lock (Volume API v2
            // section 4A).
            req.validate(stored.entry.size_bytes)?;
            (
                stored.entry.image_name.clone(),
                stored.entry.size_bytes,
                stored.runtime.attachment.is_some(),
            )
        };

        // Ownership proof and size agreement before the mutation: the
        // grow proceeds from the image's ACTUAL size, which may exceed
        // the recorded one (a completed-but-unrecorded grow).
        let actual_size = self.verify_owned_size(&image_name, volume_id, current_size)?;

        // Capacity envelope for the growth delta.
        //
        // Advisory by design (same reasoning as the create check):
        // `max_avail` is an estimate that races with concurrent writers
        // on a shared external cluster; over-commitment surfaces as
        // Ceph's own ENOSPC at write time, never as a fabricated
        // durability claim here.
        let capacity = self.pool_stats()?;
        let delta = req.new_size_bytes.saturating_sub(actual_size);
        if delta.saturating_add(CEPH_HEADROOM_BYTES) > capacity.max_avail {
            return Err(ApiError::new(
                ApiErrorCode::NoSafeCapacity,
                format!(
                    "grow needs {delta} more bytes, pool {} has {} available (headroom {})",
                    self.config.pool, capacity.max_avail, CEPH_HEADROOM_BYTES
                ),
            ));
        }
        let spec = self.image_spec(&image_name);
        // An image already at or beyond the target (the unrecorded grow
        // was at least this large) needs no resize: the target is met,
        // the actual size is what gets recorded.
        let mut effective = actual_size;
        // Honest resize accounting: `backing_resized` is true only when
        // an `rbd resize` actually ran, never when the unrecorded-grown
        // size already met the target and the resize was skipped.
        let backing_resized = actual_size < req.new_size_bytes;
        if backing_resized {
            // No `--allow-shrink` flag: it is a boost::program_options
            // bool switch, which rejects the `--allow-shrink=false` argv
            // form outright ("does not take any arguments"), and shrink is
            // already impossible here — the branch runs only when
            // actual < new, and rbd independently refuses a smaller size
            // without the flag.
            let output =
                self.run_rbd(&["resize", "-s", &format!("{}B", req.new_size_bytes), &spec])?;
            if !output.success {
                return Err(command_failed("rbd resize", &output));
            }
            // Verify the effective size from rbd's own report; RBD is
            // byte-granular, so anything below the request is a failure.
            effective = self.image_info(&image_name)?.size_bytes().ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!("rbd info did not report a size for {spec} after rbd resize"),
                )
            })?;
            if effective < req.new_size_bytes {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "image size mismatch after rbd resize of {spec}: requested {} bytes, rbd \
                         info reports {effective} bytes",
                        req.new_size_bytes
                    ),
                ));
            }
        }

        let stored = state
            .volume_mut(volume_id)
            .ok_or_else(|| not_found(volume_id))?;
        stored.entry.size_bytes = effective;
        stored.entry.generation += 1;
        state.save(&self.state_path)?;
        Ok(GrowVolumeResponse {
            backing_resized,
            // No VMM integration exists: an attached frontend still needs
            // a (retried) notification; a detached volume has nobody to
            // notify. Never `Notified`.
            guest_notification_status: if has_attachment {
                GrowGuestNotification::RetryRequired
            } else {
                GrowGuestNotification::NotApplicable
            },
            effective_size_bytes: effective,
        })
    }

    fn delete_volume_inner(
        &self,
        volume_id: &VolumeId,
        req: &DeleteVolumeRequest,
    ) -> Result<(), ApiError> {
        validate_api_version(&req.api_version)?;
        let mut state = self.lock_state()?;
        let (image_name, volume_state) = {
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
            match req.data_erasure_policy {
                // Ceph reclaim does not guarantee block-level zeroing:
                // fail-closed policy negotiation instead of a false claim
                // (Volume API v2 section 4D).
                ErasurePolicy::ZeroDiscard => {
                    return Err(unsupported(
                        "ceph-rbd prototype does not implement verified block zeroing; use \
                         Retain (trash) or a provider with a proven zeroing path",
                    ));
                }
                ErasurePolicy::Cryptographic => {
                    return Err(unsupported(
                        "cryptographic erasure requires a separate evidence gate",
                    ));
                }
                ErasurePolicy::Retain => {}
            }
            (stored.entry.image_name.clone(), stored.runtime.state)
        };

        // A Failed volume whose image is already absent has nothing to
        // remove: check rbd's own listing first so a vanished image does
        // not pin the state forever (rbd trash move on a missing image
        // always fails). For any *other* state an absent image is
        // unexpected and fails loudly — a Ready volume whose image is
        // (perhaps only transiently) invisible must never be deleted as
        // if it had been erased.
        let spec = self.image_spec(&image_name);
        match self.verify_backing(&image_name, volume_id)? {
            Backing::Absent if volume_state == VolumeLifecycle::Failed => {}
            Backing::Absent => {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "image {spec} unexpectedly absent for volume in state {volume_state:?}"
                    ),
                ));
            }
            Backing::Mismatch => {
                return Err(ApiError::new(
                    ApiErrorCode::ForeignDeviceState,
                    format!(
                        "image {spec} does not carry the ownership record of volume {volume_id}; \
                         a foreign image is never destroyed by volvisor — restore the image's \
                         `volvisor.owner` metadata to `{volume_id}` to make the volume operable \
                         again, or, once you have confirmed the image is truly foreign, remove \
                         it and the volume's state entry manually (out of band)"
                    ),
                ));
            }
            Backing::Owned { .. } => {
                let output = self.run_rbd(&["trash", "move", &spec])?;
                if !output.success {
                    return Err(command_failed("rbd trash move", &output));
                }
                // Verify the image landed in the trash (recoverable) — a
                // successful exit status alone is not evidence.
                if !self.list_trash()?.contains(&image_name) {
                    return Err(ApiError::new(
                        ApiErrorCode::Internal,
                        format!(
                            "rbd trash move reported success but rbd trash ls does not list {image_name}"
                        ),
                    ));
                }
            }
        }
        state.remove_volume(volume_id);
        state.save(&self.state_path)?;
        Ok(())
    }
}

#[async_trait]
impl VolumeProvider for CephRbdProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet::of([
            Capability::Create,
            Capability::Attach,
            Capability::Resize,
            // The class marker for "adapter for an existing external Ceph
            // cluster" (Volume API v2 section 8). Same-host online backing
            // moves and live migration stay unadvertised (unproven).
            Capability::RbdClusterAdapter,
        ])
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
        let stored = state.volume(id).ok_or_else(|| not_found(id))?;
        self.verified_inspect_response(id, stored)
    }

    async fn list_volumes(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<InspectVolumeResponse>, ApiError> {
        let state = self.lock_state()?;
        let mut responses = Vec::new();
        for (id, stored) in state.volumes() {
            if project.is_none_or(|p| &stored.entry.project_id == p) {
                responses.push(self.verified_inspect_response(id, stored)?);
            }
        }
        Ok(responses)
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

/// Find one pool by name in a `ceph df` pool list.
fn find_pool<'a>(pools: &'a [CephDfPool], name: &str) -> Option<&'a CephDfPool> {
    pools.iter().find(|pool| pool.name.as_deref() == Some(name))
}

/// An `INTERNAL` error carrying the command name and a stderr excerpt.
fn command_failed(program: &str, output: &CommandOutput) -> ApiError {
    ApiError::new(
        ApiErrorCode::Internal,
        format!("{program} failed: {}", output.stderr_excerpt()),
    )
}

/// Whether an `rbd image-meta get` stderr means "the key is absent".
///
/// Real rbd reports a missing metadata key as an ENOENT-class failure
/// (e.g. `(2) No such file or directory`, the errno name, or `No such
/// attribute` depending on the build). Only these spellings count as
/// absence; anything else is a failure to *read* and must surface as a
/// typed error.
fn is_key_absent(stderr: &str) -> bool {
    stderr.contains("No such attribute")
        || stderr.contains("ENOENT")
        || stderr.contains("(2) No such file or directory")
}

/// An `UNSUPPORTED_CLASS_OR_POLICY` rejection (fail-closed negotiation).
fn unsupported(detail: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorCode::UnsupportedClassOrPolicy, detail)
}

/// A `NOT_FOUND` rejection for a missing volume.
fn not_found(volume_id: &VolumeId) -> ApiError {
    ApiError::not_found(format!("volume {volume_id} not found"))
}

/// The maximum length of the sanitized identity segment of an image name.
///
/// Ceph rejects image names beyond ~4 KiB but the same scheme is shared
/// with the LVM provider for cross-backend consistency; capping the
/// sanitized segment at 100 keeps the full name (`vol-` + segment + `-` +
/// 8 hex) at 113 characters even for a maximum-length (128-byte) volume
/// id.
const IMAGE_NAME_SANITIZED_MAX_CHARS: usize = 100;

/// The RBD image name for a volume identity.
///
/// This mirrors the LVM provider's `lv_name_for` scheme exactly (the two
/// are deliberately duplicated, not factored out, so each crate stays
/// independently reviewable): `.` and `:` are replaced with `-` (the ID
/// charset otherwise consists of `[A-Za-z0-9_.:-]`), the sanitized segment
/// is truncated to 100 characters (the
/// `IMAGE_NAME_SANITIZED_MAX_CHARS` constant), and the
/// first 8 hex characters of SHA-256 over the **full** volume id are
/// appended. Sanitization alone is **not injective** (`vol.a`, `vol:a` and
/// `vol-a` all map to `vol-a`), so uniqueness rests on the 32-bit hash
/// suffix (collision probability <= 2^-32 per distinct pair). The `vol-`
/// prefix guarantees the name is never dash-leading, and the full name is
/// at most `4 + 100 + 1 + 8 = 113` characters.
#[must_use]
pub fn image_name_for(volume_id: &VolumeId) -> String {
    let sanitized: String = volume_id
        .as_str()
        .chars()
        .map(|c| if matches!(c, '.' | ':') { '-' } else { c })
        .take(IMAGE_NAME_SANITIZED_MAX_CHARS)
        .collect();
    let digest = Sha256::digest(volume_id.as_str().as_bytes());
    format!("vol-{sanitized}-{}", hex_prefix(&digest, 4))
}

/// The first `bytes * 2` hex characters of a digest, without `format!`.
fn hex_prefix(digest: &[u8], bytes: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes * 2);
    for byte in digest.iter().take(bytes) {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// The access mode granted for a requested mode.
///
/// `ReadOnly` never reaches this mapping on the attach path (it is
/// rejected typed in [`CephRbdProvider::attach_volume_inner`] before any
/// mutation); the mapping is kept total so a recorded mode always
/// round-trips.
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

/// Fail-closed policy negotiation for create.
fn check_policies(req: &CreateVolumeRequest) -> Result<(), ApiError> {
    // RBD images are thin-provisioned by construction; explicit thin is
    // the honest match, thick preallocation is not implemented.
    if req.provisioning == Some(Provisioning::Thick) {
        return Err(unsupported(
            "thick provisioning: RBD images here are thin; thick preallocation is not implemented",
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
                    "local mirror protection is not implemented: ceph-rbd protection is the \
                     cluster's own placement policy, never a Volvisor local mirror",
                ));
            }
        }
    }
    if req.placement.is_some() {
        return Err(unsupported(
            "placement constraints: this prototype does not negotiate CRUSH placement; the \
             configured pool's policy applies",
        ));
    }
    Ok(())
}

/// Build the contract-shaped inspect response from stored state.
fn inspect_response(volume_id: &VolumeId, stored: &StoredVolume) -> InspectVolumeResponse {
    let attachment = stored.runtime.attachment.as_ref();
    InspectVolumeResponse {
        volume_id: volume_id.clone(),
        backend_class: VolumeClass::CephRbd,
        project_id: stored.entry.project_id.clone(),
        generation: stored.entry.generation,
        state: stored.runtime.state,
        // RBD sizes are byte-granular, so provisioned equals the effective
        // size; allocation is reported at the logical size (actual thin
        // usage is not measured in this prototype).
        provisioned_bytes: stored.entry.size_bytes,
        allocated_bytes: stored.entry.size_bytes,
        // Honest protection axes (contract section 6): no local mirror is
        // ever claimed for ceph-rbd, and the remote axis is reported as
        // NOT ESTABLISHED here — deliberately, not as an assertion that
        // no replication exists. Reasoning: whether the pool's placement
        // policy actually keeps remote copies is a POOL policy fact
        // (`ceph osd pool get <pool> size`), and the per-volume inspect
        // path must stay cheap, so it makes no policy queries; asserting
        // `ceph_policy` unconditionally would claim replication from a
        // heuristic (a size-1 pool has no remote copy at all), and the
        // axis type has no `unknown` variant. `None` on this path
        // therefore means "remote protection not established by this
        // (cheap, per-volume) path — query discover_pools for the pool's
        // actual replication policy"; it does NOT assert the absence of
        // remote copies (which would be a conservative falsehood on a
        // replicated pool). The authoritative policy facts are reported
        // on the read-only pool discovery surface (`discover_pools`);
        // per-volume health stays `Unknown` until proven, and
        // `evidence_status` says `PrototypeOnly`.
        effective_protection: EffectiveProtection {
            local: LocalProtectionAxis::None,
            remote: RemoteProtectionAxis::None,
        },
        // Ceph's default replicated-pool failure domain is the host; the
        // pool's actual CRUSH rule is not queried in this prototype.
        failure_domain: FailureDomain::Host,
        // Unknown until proven; never a healthy default. Cluster health is
        // reflected on the read-only pool discovery surface instead.
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
        // Host-scoped, ephemeral backend handle (the rbd mapping device);
        // never a secret.
        frontend: Frontend::VirtioBlk {
            host_device_path: record.device.clone(),
        },
        // No VMM integration exists: evidence honestly starts at Prepared.
        state: AttachmentState::Prepared,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume_id(raw: &str) -> VolumeId {
        VolumeId::new(raw).expect("valid fixture volume id")
    }

    #[test]
    fn image_names_are_injective_across_the_id_charset() {
        // `vol.a`, `vol:a` and `vol-a` all sanitize to `vol-a`; the hash
        // suffix keeps them distinct (the scheme mirrors the LVM one).
        let names = [
            image_name_for(&volume_id("vol.a")),
            image_name_for(&volume_id("vol:a")),
            image_name_for(&volume_id("vol-a")),
        ];
        assert_ne!(names[0], names[1]);
        assert_ne!(names[0], names[2]);
        assert_ne!(names[1], names[2]);
        for name in &names {
            assert!(name.starts_with("vol-"), "{name}: stable prefix");
            assert!(!name.starts_with('-'), "{name}: never dash-leading");
        }
        // Deterministic for the same identity.
        assert_eq!(names[0], image_name_for(&volume_id("vol.a")));
    }

    #[test]
    fn image_names_for_max_length_ids_fit_the_113_char_budget() {
        let long_id = volume_id(&"v".repeat(128));
        let name = image_name_for(&long_id);
        assert!(name.chars().count() <= 113, "{name} is too long");
        assert!(name.starts_with("vol-"));
        assert!(!name.starts_with('-'));

        // Two distinct 128-char ids sharing a 100-char sanitized prefix
        // still produce distinct names: uniqueness rests on the hash
        // suffix computed over the FULL volume id.
        let shared_prefix = "p".repeat(100);
        let first = image_name_for(&volume_id(&format!("{shared_prefix}{}", "a".repeat(28))));
        let second = image_name_for(&volume_id(&format!("{shared_prefix}{}", "b".repeat(28))));
        assert_ne!(first, second);
        let (first_prefix, _) = first.rsplit_once('-').expect("hash suffix delimited");
        let (second_prefix, _) = second.rsplit_once('-').expect("hash suffix delimited");
        assert_eq!(
            first_prefix, second_prefix,
            "the sanitized segments are identical after truncation"
        );
    }

    #[test]
    fn config_validation_rejects_malformed_shapes() {
        let base = CephProviderConfig {
            cluster_fsid: "fsid-1".to_owned(),
            mon_hosts: vec!["mon1:6789".to_owned()],
            pool: "rbd".to_owned(),
            user: "client.volvisor".to_owned(),
        };
        assert!(base.validate().is_ok());

        let mut config = base.clone();
        config.cluster_fsid = "  ".to_owned();
        assert_eq!(
            config.validate().unwrap_err().code,
            ApiErrorCode::InvalidRequest
        );

        let mut config = base.clone();
        config.mon_hosts = Vec::new();
        assert!(config.validate().is_err());

        let mut config = base.clone();
        config.mon_hosts = (0..10).map(|i| format!("mon{i}:6789")).collect();
        assert!(config.validate().is_err());

        let mut config = base.clone();
        config.mon_hosts = vec![String::new()];
        assert!(config.validate().is_err());

        let mut config = base.clone();
        config.pool = String::new();
        assert!(config.validate().is_err());

        let mut config = base;
        config.user = String::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn capabilities_advertise_the_ceph_adapter_profile() {
        let provider = {
            let runner: Arc<dyn CommandRunner> =
                Arc::new(volvisor_provider::FakeRunner::with_queue(Vec::new()));
            CephRbdProvider {
                runner,
                config: CephProviderConfig {
                    cluster_fsid: "fsid-1".to_owned(),
                    mon_hosts: vec!["mon1:6789".to_owned()],
                    pool: "rbd".to_owned(),
                    user: "client.volvisor".to_owned(),
                },
                state_path: std::env::temp_dir().join("unused-ceph-state.json"),
                state: Mutex::new(CephState::default()),
                last_reconcile: Mutex::new(None),
            }
        };
        let capabilities = provider.capabilities();
        assert!(capabilities.contains(Capability::Create));
        assert!(capabilities.contains(Capability::Attach));
        assert!(capabilities.contains(Capability::Resize));
        assert!(capabilities.contains(Capability::RbdClusterAdapter));
        // Unproven mobility surfaces stay unadvertised.
        assert!(!capabilities.contains(Capability::SameHostLiveBackingMove));
        assert!(!capabilities.contains(Capability::LiveMigrate));
        assert_eq!(provider.supported_classes(), &[VolumeClass::CephRbd]);
    }
}
