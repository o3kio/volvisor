//! The external-cluster Ceph RBD provider (ADR-0005 Phase A).
//!
//! [`CephRbdProvider`] implements [`VolumeProvider`] for
//! [`VolumeClass::CephRbd`]: volumes are RBD images in one configured pool
//! of an **existing, externally operated** Ceph cluster, driven through
//! the `ceph`/`rbd` CLIs via the shell-free
//! [`CommandRunner`](volvisor_provider::runner). Every invocation carries
//! `-m <mons>` and `--id <user>` (never an environment-variable or
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
    parse_health_detail, parse_image_list, parse_rbd_info, parse_showmapped, parse_trash_list,
};
use crate::state::{
    AttachmentRecord, CephState, StoredVolume, VolumeEntry, VolumeRuntime, unix_now,
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

/// Logical block size assumed when a create request omits one.
const DEFAULT_BLOCK_SIZE: u32 = 4096;

/// Pool headroom kept unallocated so the cluster can always write metadata
/// and absorb recovery churn (`NO_SAFE_CAPACITY` is reported before the
/// pool fills). 1 GiB: a conservative slice of a Ceph pool, deliberately
/// independent of pool size because `ceph df` reports no metadata reserve.
pub const CEPH_HEADROOM_BYTES: u64 = 1 << 30;

/// The image features every volvisor-owned image is created with.
///
/// `exclusive-lock` is the single-writer mechanism (a second `rbd map`
/// writer is refused by the cluster); `layering` is the standard
/// snapshot/clone basis. Images without `exclusive-lock` are not adopted.
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

/// The usable-capacity picture of the configured pool (from `ceph df`).
#[derive(Clone, Copy, Debug)]
struct PoolCapacity {
    /// Bytes used in the pool as reported by `ceph df`.
    bytes_used: u64,
    /// Bytes still allocatable in the pool as reported by `ceph df`.
    max_avail: u64,
    /// Pool replication `size`, when reported (protection policy fact).
    size: Option<u64>,
}

/// Verified connection parameters for one external Ceph cluster.
///
/// The constructor fail-closes unless the cluster answers with exactly
/// `cluster_fsid`, the pool exists, and a health query succeeds; a
/// mis-pointed cluster is never adopted. No keyring lives here: the
/// daemon-side keyring is resolved by the ceph CLI through `--id user`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CephProviderConfig {
    /// The cluster FSID this provider may operate on (exact match).
    pub cluster_fsid: String,
    /// Monitor addresses (`host:port`), 1..=9 entries, joined into the
    /// `-m` flag of every invocation.
    pub mon_hosts: Vec<String>,
    /// The single pool volumes are created in.
    pub pool: String,
    /// The Ceph user id passed as `--id` (e.g. `client.volvisor`).
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
/// marks vanished/mismatched volumes `Failed` and reports foreign images
/// without touching them.
pub struct CephRbdProvider {
    /// Shell-free command executor for the ceph/rbd toolchain.
    runner: Arc<dyn CommandRunner>,
    /// Verified cluster parameters.
    config: CephProviderConfig,
    /// Path of the durable JSON state file.
    state_path: PathBuf,
    state: Mutex<CephState>,
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

    // -- Command plumbing (argv arrays; -m and --id on every invocation) --

    /// The shared argv prefix of every `ceph`/`rbd` invocation.
    fn base_args(&self) -> Vec<String> {
        vec![
            "-m".to_owned(),
            self.config.mon_hosts.join(","),
            "--id".to_owned(),
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
            size: pool.size(),
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
    /// Returns `Ok(None)` when the CLI exits non-zero, which — for an
    /// image whose existence was just confirmed through `rbd ls` — means
    /// the key is absent (a cluster-wide failure would have failed the
    /// `rbd ls` query too). Only `Err` on execution failure.
    fn image_meta_get(&self, image_name: &str, key: &str) -> Result<Option<String>, ApiError> {
        let spec = self.image_spec(image_name);
        let output = self.run_rbd(&["image-meta", "get", &spec, key])?;
        if output.success {
            Ok(Some(output.stdout.trim().to_owned()))
        } else {
            Ok(None)
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

    /// Reconcile provider state against the observed cluster.
    ///
    /// Non-destructive by construction (nothing is unmapped, adopted or
    /// removed here):
    ///
    /// - a state entry whose image is verifiably absent (from a
    ///   successful `rbd ls`) is marked `Failed` and persisted — never
    ///   silently recreated or dropped;
    /// - a state entry whose `volvisor.owner` metadata is missing or
    ///   names a different volume is marked `Failed` (never adopted);
    /// - a volume whose image is mapped while no attachment record exists
    ///   (a stale mapping from a previous incarnation) is marked `Failed`
    ///   and reported — the mapping is never unmapped automatically
    ///   (destructive); `Failed` is terminal, so unmaps stay manual;
    /// - an attachment record whose device is *verifiably absent* from a
    ///   successful `rbd showmapped` (an interrupted detach: unmap
    ///   succeeded, the state save did not) is cleared and the volume
    ///   returns to `Ready` — state matches observed reality, mirroring
    ///   the LVM release reconciliation;
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
                if snapshot.runtime.state != VolumeLifecycle::Failed {
                    if let Some(volume) = state.volume_mut(&id) {
                        volume.runtime.state = VolumeLifecycle::Failed;
                        changed = true;
                    }
                }
                report.missing_volumes.push(id);
                continue;
            }
            // The image exists: ownership decides everything else.
            let owner = self.image_meta_get(image_name, OWNER_META_KEY);
            match owner {
                Ok(Some(owner)) if owner == id.as_str() => {
                    let mapped = mappings.as_ref().is_some_and(|maps| {
                        maps.iter().any(|m| m.name.as_deref() == Some(image_name))
                    });
                    if let Some(record) = snapshot.runtime.attachment.as_ref() {
                        // Record exists: a verifiably absent device means an
                        // interrupted detach — reconcile forward.
                        let device_present = mappings.as_ref().is_some_and(|maps| {
                            maps.iter()
                                .any(|m| m.device.as_deref() == Some(record.device.as_str()))
                        });
                        if mappings.is_some() && !device_present {
                            if let Some(volume) = state.volume_mut(&id) {
                                volume.runtime.attachment = None;
                                if volume.runtime.state == VolumeLifecycle::Attached {
                                    volume.runtime.state = VolumeLifecycle::Ready;
                                }
                                changed = true;
                            }
                        }
                    } else if mapped {
                        // Stale mapping without an attachment record: visible
                        // and Failed, never auto-unmapped (destructive).
                        if snapshot.runtime.state != VolumeLifecycle::Failed {
                            if let Some(volume) = state.volume_mut(&id) {
                                volume.runtime.state = VolumeLifecycle::Failed;
                                changed = true;
                            }
                        }
                        report.stale_mappings.push(id);
                    }
                }
                Ok(_) => {
                    // Missing or foreign owner metadata: never adopt.
                    if snapshot.runtime.state != VolumeLifecycle::Failed {
                        if let Some(volume) = state.volume_mut(&id) {
                            volume.runtime.state = VolumeLifecycle::Failed;
                            changed = true;
                        }
                    }
                    report.mismatched_volumes.push(id);
                }
                Err(_) => {
                    // Honest unknown: leave the entry untouched.
                }
            }
        }

        // Foreign / untracked pass: images we have no state for. Never
        // touched, only reported (AGENTS rule 7).
        let our_images: Vec<String> = state
            .volumes()
            .values()
            .map(|volume| volume.entry.image_name.clone())
            .collect();
        for image_name in &listed {
            if our_images.contains(image_name) {
                continue;
            }
            match self.image_meta_get(image_name, OWNER_META_KEY) {
                Ok(Some(_)) => report.untracked_owned_images.push(image_name.clone()),
                Ok(None) => report.foreign_images.push(image_name.clone()),
                Err(_) => {}
            }
        }

        if changed {
            state.save(&self.state_path)?;
        }
        Ok(report)
    }

    /// Read-only pool discovery: the configured pool as a [`Pool`], with
    /// capacity from `ceph df` and health reflected from `ceph health`
    /// (query failure → `Unknown`, never fabricated). No cluster mutation
    /// of any kind; foreign images are invisible here (they are a
    /// reconcile concern, not a pool fact).
    ///
    /// # Errors
    /// Returns an [`ApiError`] when `ceph df` cannot establish the pool's
    /// capacity (the pool list is then unknown, not empty).
    pub fn discover_pools(&self) -> Result<Vec<Pool>, ApiError> {
        let stats = self.pool_stats()?;
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
                // Remote replication is a pool policy fact: claimed only
                // when ceph df reports a replication size >= 2; otherwise
                // not established (the volume-level protection axis is the
                // authoritative ceph_policy statement).
                remote_replication: stats.size.is_some_and(|size| size >= 2),
            },
            health,
        }])
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
            return Err(command_failed("rbd create", &output));
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
                return Ok(inspect_response(&req.volume_id, existing));
            }
            return Err(ApiError::idempotency_conflict(&req.volume_id));
        }

        // Capacity envelope: the request plus headroom must fit into the
        // pool's max_avail (typed NO_SAFE_CAPACITY before the pool fills).
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
    /// `Failed` stays `Failed` (no implicit recovery).
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
            Backing::Owned { .. } => Ok(response),
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
    /// The image must carry this volume's ownership record and report
    /// exactly the size the state records: an image that changed outside
    /// volvisor is never resized on assumption.
    fn verify_owned_size(
        &self,
        image_name: &str,
        volume_id: &VolumeId,
        current_size: u64,
    ) -> Result<(), ApiError> {
        let spec = self.image_spec(image_name);
        match self.verify_backing(image_name, volume_id)? {
            Backing::Owned { size_bytes } if size_bytes == current_size => Ok(()),
            Backing::Owned { size_bytes } => Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "image {spec} reports {size_bytes} bytes but state records {current_size}; \
                     the image changed outside volvisor"
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

        // Ownership proof and size agreement before the mutation.
        self.verify_owned_size(&image_name, volume_id, current_size)?;

        // Capacity envelope for the growth delta.
        let capacity = self.pool_stats()?;
        let delta = req.new_size_bytes - current_size;
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
        let output = self.run_rbd(&[
            "resize",
            "--allow-shrink=false",
            "-s",
            &format!("{}B", req.new_size_bytes),
            &spec,
        ])?;
        if !output.success {
            return Err(command_failed("rbd resize", &output));
        }
        // Verify the effective size from rbd's own report; RBD is
        // byte-granular, so anything below the request is a failure.
        let actual = self.image_info(&image_name)?.size_bytes().ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("rbd info did not report a size for {spec} after rbd resize"),
            )
        })?;
        if actual < req.new_size_bytes {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "image size mismatch after rbd resize of {spec}: requested {} bytes, rbd \
                     info reports {actual} bytes",
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
            // No VMM integration exists: an attached frontend still needs
            // a (retried) notification; a detached volume has nobody to
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
                         a foreign image is never destroyed by volvisor"
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
        // Honest protection axes (contract section 6): the cluster's
        // placement/replication policy is the remote axis; no local mirror
        // is ever claimed for ceph-rbd.
        effective_protection: EffectiveProtection {
            local: LocalProtectionAxis::None,
            remote: RemoteProtectionAxis::CephPolicy,
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
