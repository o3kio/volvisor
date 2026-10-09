//! The DRBD9 nearline-replicated provider (ADR-0007, P3 baseline).
//!
//! [`DrbdProvider`] implements [`VolumeProvider`] for
//! [`VolumeClass::NearlineReplicated`] on the local host (ADR-0007
//! deployment variant A): host-kernel DRBD over an operator-designated
//! LVM volume group, driven through the unmodified `drbdadm` /
//! `drbdsetup` / `blockdev` / LVM CLIs via the shell-free
//! [`CommandRunner`](volvisor_provider::runner) — no new replication
//! engine exists here (AGENTS rule 13).
//!
//! Volvisor owns exactly the **local end** of every resource: the local
//! backing LV (created with `volvisor.owner`/`volvisor.generation` tags
//! in the configured VG), the generated single-resource `.res` file
//! (0600, atomic write, carrying the peer shared secret read from a
//! path reference), the resource lifecycle (`create-md`, `up`, `down`)
//! and the local role (`primary`/`secondary`). Every `drbdadm`
//! invocation is scoped through `-c <our own file>`, so a foreign
//! resource can never be touched (rule 7). The **peer** is
//! operator-provisioned out of band and deploys the identical
//! definition; volvisor never writes to it and never adopts peer state
//! beyond what `drbdsetup status` observes.
//!
//! Honesty rules that shape this implementation:
//!
//! - a successful exit status is never evidence: the local role is
//!   re-read from `drbdsetup status` after every promotion/demotion,
//!   LV geometry from `lvs`, the effective device size from
//!   `blockdev --getsize64`;
//! - single-writer maps to DRBD single-primary: attach promotes and
//!   records the `/dev/drbdN` handle at `prepared` evidence; detach
//!   demotes, and the kernel's refusal while the device is open is
//!   surfaced as the typed `INVALID_STATE` it is — demotion is never
//!   forced (rule 17), dual-primary has no code path, and a
//!   read-only/shared-reader attach is a typed rejection;
//! - seeding (`primary --force`) runs **only** over a provably fresh
//!   resource (local `Inconsistent`); a peer observed holding data
//!   (`UpToDate`/`Consistent`/`Outdated`) or serving as Primary is
//!   foreign data and is never overwritten;
//! - Protocol A is possible-RPO: the remote protection axis only ever
//!   states that a remote replica is currently established and observed
//!   `UpToDate` — it is never a durability claim (rule 16);
//! - grow-only resize: the local LV grows, `drbdadm resize` follows,
//!   and a device that did not reach the request (the peer backing was
//!   not grown) fails closed with the honest boundary and the observed
//!   size persisted;
//! - health is observed, not configured: `drbdsetup status` facts map
//!   conservatively (`Healthy` only when connected with both disks
//!   `UpToDate` and no resync), and `evidence_status` stays
//!   `PrototypeOnly` (rule 12).

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use volvisor_provider::{AdoptionSurface, VolumeProvider};
use volvisor_types::domain::{
    AccessMode, EffectiveProtection, EvidenceStatus, FailureDomain, Frontend, Health,
    LocalProtectionAxis, Provisioning, RemoteProtectionAxis, VolumeClass,
};
use volvisor_types::request::{
    AccessModeRequest, AttachVolumeRequest, AttachVolumeResponse, CreateVolumeRequest,
    DeleteVolumeRequest, DetachVolumeRequest, ErasurePolicy, GrowGuestNotification,
    GrowVolumeRequest, GrowVolumeResponse, InspectVolumeResponse, LocalProtectionModeRequest,
    ReplicationModeRequest,
};
use volvisor_types::{
    AdoptVolumeResponse, ApiError, ApiErrorCode, AttachmentId, AttachmentState, AuthoritySummary,
    AuthorityView, Capability, CapabilitySet, EndpointBacking, HostId, LeaseState, LossBoundary,
    ProjectId, PromotionClassification, RecordedBarrier, VolumeId, VolumeLifecycle,
    validate_api_version,
};

use crate::authority::{AuthorityContext, LeaseValidity, witness_error};
use crate::report::{
    DiskState, LvRow, ResourceStatus, Role, VgRow, parse_blockdev_size, parse_drbdsetup_show_gi,
    parse_drbdsetup_status, parse_report,
};
use crate::resgen::{
    ParsedNode, ParsedResource, ResourceDefinition, is_ipv4_literal, parse_resource_file,
    read_shared_secret, res_file_path,
};
use crate::state::{
    AttachmentRecord, ClearedAttachment, ClearedAttachmentReason, DeferredRenewal, DrbdState,
    FencedVolume, PendingFence, ReconcileReport, RenewalReport, ReplicationMode, StoredVolume,
    UnverifiableVolume, VolumeAuthorityBlock, VolumeEntry, VolumeRuntime, unix_now,
};
use crate::{CommandOutput, CommandRunner};
use volvisor_witness::proto::{RegisterResponse, RegistrationContent, WitnessError};

/// Stable provider name for diagnostics (never a secret).
pub const PROVIDER_NAME: &str = "drbd9-nearline-prototype";

/// LVM tag holding the owning volume identity on the backing LV.
pub const OWNER_TAG: &str = "volvisor.owner";

/// LVM tag holding the volume generation at create time.
pub const GENERATION_TAG: &str = "volvisor.generation";

/// The only volume class served by this provider.
static SUPPORTED_CLASSES: &[VolumeClass] = &[VolumeClass::NearlineReplicated];

/// Logical block size assumed when a create request omits one.
const DEFAULT_BLOCK_SIZE: u32 = 4096;

/// VG headroom kept unallocated so the nearline VG can always absorb
/// metadata churn (`NO_SAFE_CAPACITY` is reported before the VG fills).
/// 4 MiB — one default LVM extent, mirroring the LVM provider.
pub const VG_HEADROOM_BYTES: u64 = 4 << 20;

/// Extent size assumed when `vgs` does not report one (LVM's own
/// default).
const DEFAULT_EXTENT_BYTES: u64 = 4 << 20;

/// The only `preferred_host_id` this single-host provider honors.
const LOCAL_HOST_ID: &str = "local";

/// The size-agreement outcome for an owned backing LV.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backing {
    /// The LV is verifiably absent from a successful `lvs` query.
    Absent,
    /// The LV exists, carries this volume's ownership tag and reports
    /// the carried size in bytes.
    Owned {
        /// Effective LV size in bytes.
        size_bytes: u64,
    },
    /// The LV exists but its ownership tag is missing or names a
    /// different volume: never adopted, never destroyed by us.
    Mismatch,
}

/// The verification outcome for a state entry's resource file.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ResFileCheck {
    /// The file exists and matches the recorded resource (name, minor,
    /// protocol, disk, node set, secret presence).
    Matches,
    /// The file does not exist.
    Missing,
    /// The file exists but no longer matches volvisor's record
    /// (carrying the mismatch detail).
    Mismatch(String),
}

/// What the peer's observed state means for seeding a fresh local disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerFreshness {
    /// The peer is connected with an `Inconsistent` (never-seeded) disk
    /// and is not Primary: provably fresh, safe to seed.
    Fresh,
    /// The peer holds data or is Primary: foreign, never overwritten.
    Foreign,
    /// The peer is absent or disk-unknown: nothing to seed against yet.
    Absent,
}

/// The outcome of reclaiming a crashed predecessor's create: the
/// verified LV size, whether the LV still counts as freshly created by
/// this call (no predecessor state to preserve), and the minor/port
/// adopted from the crashed predecessor's resource file (mandatory when
/// the resource is verifiably up).
type ReclaimOutcome = (u64, bool, Option<(u32, u16)>);

/// Free space and extent size of the configured nearline VG.
#[derive(Clone, Copy, Debug)]
struct VgCapacity {
    /// Free bytes in the VG.
    free_bytes: u64,
    /// Physical extent size in bytes (LVM rounds allocations up to
    /// whole extents).
    extent_bytes: u64,
}

/// Operator-provided deployment parameters for the DRBD provider.
///
/// The peer host (name and address) and its backing LV are
/// operator-provisioned out of band (plan §7): the generated resource
/// definition describes both ends and the operator deploys the
/// identical definition on the peer. The shared secret is referenced by
/// path only — it is read at generation time, written into the 0600
/// resource file, and never logged or persisted in provider state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrbdProviderConfig {
    /// The operator-designated volume group holding the nearline
    /// backing LVs (must exist; distinct from any `native-local` VG).
    pub vg_name: String,
    /// Directory the generated `volvisor-<resource>.res` files live in
    /// (must exist; the DRBD convention is `/etc/drbd.d`).
    pub config_dir: PathBuf,
    /// The local `on` node name (validated against `uname -n` at
    /// startup).
    pub node_name: String,
    /// The local replication address (IPv4 dotted quad, no port).
    pub local_address: String,
    /// The peer's `on` node name.
    pub peer_name: String,
    /// The peer's replication address as `<ipv4>:<port>`; the port is
    /// the peer's fixed listening port, written into every generated
    /// definition.
    pub peer_address: String,
    /// Path of the peer shared secret (owner-only file; operator
    /// responsibility).
    pub shared_secret_file: PathBuf,
    /// Inclusive lower bound of the local replication port range.
    pub port_min: u16,
    /// Inclusive upper bound of the local replication port range.
    pub port_max: u16,
    /// Inclusive lower bound of the DRBD minor range.
    ///
    /// The configured `[minor_min, minor_max]` range must be
    /// **exclusively reserved for volvisor**: the allocator never
    /// probes the kernel's minor space, so a minor already in use by a
    /// foreign resource is only discovered as a create-time failure
    /// that tears the half-created volume back down.
    pub minor_min: u32,
    /// Inclusive upper bound of the DRBD minor range (see
    /// [`Self::minor_min`]: the range must be exclusively reserved for
    /// volvisor).
    pub minor_max: u32,
    /// Root of the procfs mount used for the module check
    /// (`<proc_root>/drbd` must be readable; `/proc` on a real host, a
    /// fixture directory in tests).
    pub proc_root: PathBuf,
}

impl DrbdProviderConfig {
    /// Validate the configuration shape (this never touches the host).
    ///
    /// # Errors
    /// Returns an `INVALID_REQUEST` [`ApiError`] naming the offending
    /// field: empty names, a non-IPv4 `local_address`, a malformed
    /// `peer_address`, an empty secret path or an empty/inverted
    /// minor/port range.
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.vg_name.trim().is_empty() {
            return Err(ApiError::invalid_request("vg_name must not be empty"));
        }
        if self.node_name.trim().is_empty() {
            return Err(ApiError::invalid_request("node_name must not be empty"));
        }
        if self.node_name.chars().count() > NODE_NAME_MAX_CHARS {
            return Err(ApiError::invalid_request(format!(
                "node_name must not exceed {NODE_NAME_MAX_CHARS} characters (the \
                 drbdsetup status connection-line wrap budget)"
            )));
        }
        if !is_ipv4_literal(&self.local_address) {
            return Err(ApiError::invalid_request(
                "local_address must be an IPv4 dotted quad (e.g. 10.0.0.1)",
            ));
        }
        if self.peer_name.trim().is_empty() {
            return Err(ApiError::invalid_request("peer_name must not be empty"));
        }
        if self.peer_name.chars().count() > NODE_NAME_MAX_CHARS {
            return Err(ApiError::invalid_request(format!(
                "peer_name must not exceed {NODE_NAME_MAX_CHARS} characters (the \
                 drbdsetup status connection-line wrap budget)"
            )));
        }
        split_peer_address(&self.peer_address)?;
        if self.shared_secret_file.as_os_str().is_empty() {
            return Err(ApiError::invalid_request(
                "shared_secret_file must not be empty",
            ));
        }
        if self.port_min > self.port_max {
            return Err(ApiError::invalid_request(
                "port range is empty: port_min exceeds port_max",
            ));
        }
        if self.minor_min > self.minor_max {
            return Err(ApiError::invalid_request(
                "minor range is empty: minor_min exceeds minor_max",
            ));
        }
        Ok(())
    }
}

/// Split a configured peer address `<ipv4>:<port>` into its parts.
///
/// # Errors
/// `INVALID_REQUEST` when the address is not shaped
/// `<ipv4-dotted-quad>:<port 1..=65535>`.
pub fn split_peer_address(address: &str) -> Result<(String, u16), ApiError> {
    let invalid = || {
        ApiError::invalid_request(format!(
            "peer_address must be shaped <ipv4>:<port>, got {address:?}"
        ))
    };
    let (ip, port) = address.rsplit_once(':').ok_or_else(invalid)?;
    if !is_ipv4_literal(ip) {
        return Err(invalid());
    }
    let port: u16 = port.parse().map_err(|_| invalid())?;
    if port == 0 {
        // Port 0 is never a valid peer listening port.
        return Err(invalid());
    }
    Ok((ip.to_owned(), port))
}

/// The DRBD9 nearline-replicated provider.
///
/// All durable state lives in the JSON state file (see [`DrbdState`]);
/// the in-memory `Mutex` only serializes access within this daemon.
/// The constructor runs the fail-closed startup verification
/// (`drbdadm --version` answers, the DRBD kernel module is loaded, the
/// configured VG is queryable, `uname -n` matches `node_name`, the
/// secret file is readable and non-empty, the config directory exists)
/// and then a reconciliation pass; on any failure nothing is written.
pub struct DrbdProvider {
    /// Shell-free command executor for the drbd-utils/LVM toolchain.
    runner: Arc<dyn CommandRunner>,
    /// Verified deployment parameters.
    config: DrbdProviderConfig,
    /// Path of the durable JSON state file.
    state_path: PathBuf,
    state: Mutex<DrbdState>,
    /// The most recent successful reconcile report (the startup pass or
    /// the last explicit [`reconcile`](Self::reconcile) call that
    /// returned `Ok`), kept so the audit trail of destructive-looking
    /// bookkeeping — attachment records cleared because their backing
    /// vanished, turned foreign or their resource went down — survives
    /// past the state change it describes. Read it with
    /// [`last_reconcile_report`](Self::last_reconcile_report).
    last_reconcile: Mutex<Option<ReconcileReport>>,
    /// The writer-authority context (P4a), when this deployment is
    /// witness-managed. `None` keeps exactly the P3 behavior
    /// (pre-authority, epoch 0) for every code path below.
    authority: Option<AuthorityContext>,
}

/// Verified adoption facts (P4a plan §5 step 1): everything the
/// authority check, the classification and the promotion need,
/// proven against this host and the witness registration.
/// The nearline adopt surface (P4a plan §6): the admin route calls the
/// engine's [`Self::adopt_and_promote`] through the trait object the
/// daemon wires into the API state.
#[async_trait::async_trait]
impl AdoptionSurface for DrbdProvider {
    async fn adopt_volume(
        &self,
        volume_id: &VolumeId,
        allow_loss: bool,
    ) -> Result<AdoptVolumeResponse, ApiError> {
        self.adopt_and_promote(volume_id, allow_loss)
    }
}

struct AdoptionFacts {
    /// The derived resource name (the scheme match is part of the
    /// verification).
    resource: String,
    /// The backing LV's volume group (from the definition's disk
    /// path).
    vg_name: String,
    /// The backing LV's name.
    lv_name: String,
    /// The resource's DRBD minor (from the definition).
    minor: u32,
    /// This host's replication port (from the definition's own
    /// on-node address).
    port: u16,
    /// The replication protocol re-verified from the definition
    /// (the classification branches on it).
    replication_mode: ReplicationMode,
    /// The observed local disk state.
    local_disk: DiskState,
    /// The operator-attested barrier, when the registration
    /// recorded one.
    barrier: Option<RecordedBarrier>,
    /// The witness view (authority check + registration).
    view: AuthorityView,
}

impl DrbdProvider {
    /// Construct the provider in pre-authority (P3) mode: no witness,
    /// no leases — every authority field stays `None` and the zombie
    /// and reconciliation rules behave exactly as in P3.
    ///
    /// See [`Self::with_authority`] for the witness-managed mode and
    /// `Self::construct` for the shared fail-closed startup
    /// sequence.
    ///
    /// # Errors
    /// See `Self::construct`.
    pub fn new(
        runner: Arc<dyn CommandRunner>,
        config: DrbdProviderConfig,
        state_path: PathBuf,
    ) -> Result<Self, ApiError> {
        Self::construct(runner, config, state_path, None)
    }

    /// Construct the provider in witness-managed mode (P4a plan §4):
    /// every promotion must hold a lease acquired from the witness
    /// first, renewal runs on the daemon's cadence against W5 local
    /// deadlines, and the startup reconciliation fail-closes on any
    /// Primary whose lease cannot be proven live (suspended, never
    /// silently resumed).
    ///
    /// The context must have been built inside the runtime that will
    /// serve the witness connection (the blocking adapter captured its
    /// handle at construction).
    ///
    /// # Errors
    /// See `Self::construct`; additionally, the startup
    /// reconciliation now validates witness-managed primaries, so an
    /// unreachable witness can fail construction with a typed error
    /// naming the volumes that stayed suspended.
    pub fn with_authority(
        runner: Arc<dyn CommandRunner>,
        config: DrbdProviderConfig,
        state_path: PathBuf,
        authority: AuthorityContext,
    ) -> Result<Self, ApiError> {
        Self::construct(runner, config, state_path, Some(authority))
    }

    /// The shared fail-closed construction sequence (plan §8):
    /// `drbdadm --version` must answer, `<proc_root>/drbd` must be
    /// readable (the kernel module is loaded), `vgs` must report the
    /// configured VG, `uname -n` must equal `config.node_name`, the
    /// shared secret file must be readable and non-empty, and
    /// `config_dir` must exist. Only then is the durable state loaded
    /// and reconciled; on any failure nothing is written.
    ///
    /// # Errors
    /// Returns a typed [`ApiError`]: `INTERNAL` when the toolchain or
    /// module check fails, `NOT_FOUND` when the VG is missing,
    /// `FOREIGN_DEVICE_STATE` on a node-name mismatch (this provider
    /// would be running on the wrong host), `INVALID_REQUEST` on an
    /// unusable secret file or config directory, plus whatever the
    /// initial reconciliation surfaces.
    fn construct(
        runner: Arc<dyn CommandRunner>,
        config: DrbdProviderConfig,
        state_path: PathBuf,
        authority: Option<AuthorityContext>,
    ) -> Result<Self, ApiError> {
        config.validate()?;
        let provider = Self {
            runner,
            config,
            state_path,
            state: Mutex::new(DrbdState::default()),
            last_reconcile: Mutex::new(None),
            authority,
        };
        provider.verify_startup()?;
        let state = DrbdState::load(&provider.state_path)?;
        *provider.lock_state()? = state;
        provider.reconcile()?;
        Ok(provider)
    }

    /// Lock the in-memory state, mapping poisoning to `INTERNAL`.
    fn lock_state(&self) -> Result<MutexGuard<'_, DrbdState>, ApiError> {
        self.state.lock().map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "drbd provider state lock poisoned by a previous failure",
            )
        })
    }

    /// Lock the last-reconcile slot, mapping poisoning to `INTERNAL`.
    fn lock_last_reconcile(&self) -> Result<MutexGuard<'_, Option<ReconcileReport>>, ApiError> {
        self.last_reconcile.lock().map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "drbd provider last-reconcile lock poisoned by a previous failure",
            )
        })
    }

    /// The most recent *successful* reconcile report: the startup pass
    /// performed at construction, or the last explicit
    /// [`reconcile`](Self::reconcile) call that returned `Ok` (a failed
    /// pass leaves the previous report in place).
    ///
    /// # Errors
    /// Returns a typed [`ApiError`] (`INTERNAL`) only when the
    /// reporting slot's lock is poisoned.
    pub fn last_reconcile_report(&self) -> Result<Option<ReconcileReport>, ApiError> {
        Ok(self.lock_last_reconcile()?.clone())
    }

    // -- Command plumbing (argv arrays; every drbdadm scoped to -c) --

    /// Run `drbdadm -c <our res file> <tail...> <resource>`.
    ///
    /// The `-c` scoping is the rule-7 guarantee: the invocation can
    /// only ever address volvisor's own single-resource file, never a
    /// foreign resource or the global `/etc/drbd.conf`.
    fn run_drbdadm_args(&self, resource: &str, tail: &[&str]) -> Result<CommandOutput, ApiError> {
        let res_file = res_file_path(&self.config.config_dir, resource);
        let mut args = vec!["-c".to_owned(), res_file.to_string_lossy().into_owned()];
        args.extend(tail.iter().map(|arg| (*arg).to_owned()));
        args.push(resource.to_owned());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner.run("drbdadm", &refs)
    }

    /// Run one scoped `drbdadm <action> <resource>`.
    fn run_drbdadm(&self, action: &str, resource: &str) -> Result<CommandOutput, ApiError> {
        self.run_drbdadm_args(resource, &[action])
    }

    /// Run the seeding promotion `drbdadm -c <file> primary --force
    /// <resource>`. Only ever called on a resource whose local disk is
    /// provably fresh (see [`Self::establish_replica`]).
    ///
    /// `--force` placed between the action and the resource name is the
    /// drbd-utils spelling (verified against the drbd-utils source:
    /// `config_flags.c` builds the primary command context with the
    /// force flag, and the kernel accepts the forced promotion of a
    /// fresh local disk via `CS_FP_LOCAL_UP_TO_DATE`). The role is
    /// always re-verified from `drbdsetup status` afterwards, so any
    /// deviation on a real toolchain surfaces as `INTERNAL`, never as
    /// silent success.
    fn run_drbdadm_seed(&self, resource: &str) -> Result<CommandOutput, ApiError> {
        self.run_drbdadm_args(resource, &["primary", "--force"])
    }

    // -- Host queries --

    /// Run `lvs` and return its report rows (the invocation explicitly
    /// requests the `lv_tags` column so ownership can be verified).
    fn list_lvs(&self) -> Result<Vec<LvRow>, ApiError> {
        let args = [
            "--reportformat",
            "json",
            "--units",
            "b",
            "--nosuffix",
            "-o",
            "vg_name,lv_name,lv_size,lv_tags",
        ];
        let output = self.runner.run("lvs", &args)?;
        if !output.success {
            return Err(command_failed("lvs", &output));
        }
        parse_report(&output.stdout, "lv")
    }

    /// Run `vgs` and return its report rows (`vg_extent_size`
    /// explicitly requested — it is not part of `vgs`' default
    /// columns, and capacity checks must round the demand to whole
    /// extents).
    fn list_vgs(&self) -> Result<Vec<VgRow>, ApiError> {
        let args = [
            "--reportformat",
            "json",
            "--units",
            "b",
            "--nosuffix",
            "-o",
            "vg_name,vg_free,vg_size,vg_extent_size",
        ];
        let output = self.runner.run("vgs", &args)?;
        if !output.success {
            return Err(command_failed("vgs", &output));
        }
        parse_report(&output.stdout, "vg")
    }

    /// Free space and extent size of the configured nearline VG.
    ///
    /// # Errors
    /// `INTERNAL` when `vgs` does not report the group — capacity is
    /// never guessed.
    fn vg_capacity(&self) -> Result<VgCapacity, ApiError> {
        let row = self
            .list_vgs()?
            .into_iter()
            .find(|row| row.vg_name.as_deref() == Some(self.config.vg_name.as_str()))
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "volume group {:?} is not reported by vgs (it existed at startup)",
                        self.config.vg_name
                    ),
                )
            })?;
        Ok(VgCapacity {
            free_bytes: row.free_bytes().ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!("vgs did not report vg_free for {:?}", self.config.vg_name),
                )
            })?,
            extent_bytes: row.extent_bytes().unwrap_or(DEFAULT_EXTENT_BYTES),
        })
    }

    /// Actual size of one LV as reported by `lvs`, when it exists.
    fn lv_size(&self, vg_name: &str, lv_name: &str) -> Result<Option<u64>, ApiError> {
        let wanted = format!("{vg_name}/{lv_name}");
        Ok(self
            .list_lvs()?
            .into_iter()
            .find(|row| row.vg_slash_lv().as_deref() == Some(wanted.as_str()))
            .and_then(|row| row.size_bytes()))
    }

    /// The status of one running resource from `drbdsetup status`.
    ///
    /// `Ok(None)` means *verifiably down*: the command failed with the
    /// real "No such resource" stderr spelling. Any other failure is a
    /// typed `INTERNAL` error (an honest unknown, never a silent
    /// down).
    fn resource_status(&self, resource: &str) -> Result<Option<ResourceStatus>, ApiError> {
        let output = self.runner.run("drbdsetup", &["status", resource])?;
        if output.success {
            let status = parse_drbdsetup_status(&output.stdout)?;
            if status.name != resource {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "drbdsetup status answered for resource {:?} instead of {resource:?}",
                        status.name
                    ),
                ));
            }
            return Ok(Some(status));
        }
        if output.stderr.contains("No such resource") {
            return Ok(None);
        }
        Err(command_failed("drbdsetup status", &output))
    }

    /// The effective size of the DRBD device, from `blockdev
    /// --getsize64 /dev/drbd<minor>`.
    ///
    /// # Errors
    /// `INTERNAL` when the query fails or the output is not a plain
    /// decimal — the device size is never guessed.
    fn device_size(&self, minor: u32) -> Result<u64, ApiError> {
        let device = format!("/dev/drbd{minor}");
        let output = self.runner.run("blockdev", &["--getsize64", &device])?;
        if !output.success {
            return Err(command_failed("blockdev --getsize64", &output));
        }
        parse_blockdev_size(&output.stdout)
    }

    // -- Writer-authority self-fencing (P4a plan §4) --

    /// Suspend I/O on the resource's device: `drbdsetup suspend-io
    /// /dev/drbd<minor>`.
    ///
    /// The argv form is verified against the drbd-utils 9.29.0 sources
    /// (suspend-io is a CTX_MINOR command; a bare resource name is not
    /// resolvable, the device node is — see
    /// `tests/drbd_authority_shapes.rs`). Suspension freezes the data
    /// path — the enforcement point a bypassing guest cannot escape;
    /// only root on the host can, which is the documented residual.
    /// Idempotent in the kernel (suspending a suspended resource is a
    /// no-op).
    ///
    /// # Errors
    /// `INTERNAL` when the command fails.
    fn suspend_io(&self, minor: u32) -> Result<(), ApiError> {
        let device = format!("/dev/drbd{minor}");
        let output = self.runner.run("drbdsetup", &["suspend-io", &device])?;
        if !output.success {
            return Err(command_failed("drbdsetup suspend-io", &output));
        }
        Ok(())
    }

    /// Resume I/O on the resource's device (`drbdsetup resume-io
    /// /dev/drbd<minor>`) — the counterpart of [`Self::suspend_io`],
    /// used only on the validated-resume path (a live lease for our
    /// epoch covering the renewal margin); never on the fence path.
    ///
    /// # Errors
    /// `INTERNAL` when the command fails.
    fn resume_io(&self, minor: u32) -> Result<(), ApiError> {
        let device = format!("/dev/drbd{minor}");
        let output = self.runner.run("drbdsetup", &["resume-io", &device])?;
        if !output.success {
            return Err(command_failed("drbdsetup resume-io", &output));
        }
        Ok(())
    }

    /// Self-fence a witness-managed volume (plan §4): suspend I/O, then
    /// attempt the demotion. The pending-fence marker is durably
    /// recorded **before** the demotion is attempted, so a crash
    /// between the two routes the restart through reconcile's
    /// fence-completion path instead of leaving a suspended Secondary
    /// nothing resumes. A busy device (the kernel refuses demotion
    /// while open — the P3 rule) stays suspended with the attachment
    /// record cleared, the [`PendingFence`] marker recorded and the
    /// lifecycle `Failed`; reconcile completes the demotion once the
    /// device closes. A clean demotion returns the volume to `Ready`.
    /// Never a silent resume.
    ///
    /// The lease block is always cleared: authority is provably lost
    /// and must never be renewed again.
    ///
    /// # Errors
    /// `INTERNAL` when a command or the state save fails (the fence is
    /// then retried by the next renewal pass and, after a restart, by
    /// the startup reconcile — the suspension is already durable in
    /// the kernel whenever the suspend itself succeeded; a suspended
    /// data path is guaranteed only while the local `drbdsetup` path
    /// is healthy, and the witness's W7 window — keyed on the lease's
    /// recorded end — is the bound that covers a suspension that is
    /// slow or failing).
    /// The fence-completion core shared by the startup reconcile and
    /// the renewal pass's fence lane: demote a still-Primary marked
    /// resource (a busy refusal returns `Ok(false)` — the device is
    /// still open, retried by the next pass), lift the suspension once
    /// demoted, clear the marker and return the volume to `Ready`.
    /// Never a silent resume. The caller owns the state save and the
    /// reporting.
    ///
    /// # Errors
    /// `INTERNAL` when the demotion or the resume command fails.
    fn try_complete_fence(
        &self,
        state: &mut DrbdState,
        id: &VolumeId,
        entry: &VolumeEntry,
        is_primary: bool,
    ) -> Result<bool, ApiError> {
        let mut demoted = !is_primary;
        if !demoted {
            let output = self.run_drbdadm("secondary", &entry.resource_name)?;
            // A busy refusal (the device is still open) is the expected
            // retry-later case; any OTHER failure is reported by the
            // caller instead of being folded into the silent skip —
            // the same distinction `self_fence` itself makes.
            if !output.success && !is_device_busy(&output.stderr) {
                return Err(command_failed("drbdadm secondary", &output));
            }
            demoted = output.success;
        }
        if !demoted {
            return Ok(false);
        }
        self.resume_io(entry.minor)?;
        if let Some(volume) = state.volume_mut(id) {
            volume.runtime.fence = None;
            volume.runtime.state = VolumeLifecycle::Ready;
        }
        Ok(true)
    }

    /// The renewal pass's fence lane: for every entry carrying a
    /// [`PendingFence`] marker, attempt the shared completion (demote
    /// if the device closed, resume, clear, `Ready`). Query and
    /// completion failures are reported (never hidden) and retried by
    /// the next pass; they never abort the renewal pass — the lease
    /// deadlines of the other volumes must not depend on one stuck
    /// fence. Without this lane, a busy-device fence would stay
    /// suspended until a restart. A downed resource is left to the
    /// startup reconcile's downed-volume handling.
    fn complete_pending_fences(&self, state: &mut DrbdState, report: &mut RenewalReport) {
        let ids: Vec<VolumeId> = state.volumes().keys().cloned().collect();
        for id in ids {
            let Some(snapshot) = state.volume(&id).cloned() else {
                continue;
            };
            if snapshot.runtime.fence.is_none() {
                continue;
            }
            let entry = snapshot.entry;
            let status = match self.resource_status(&entry.resource_name) {
                Ok(Some(status)) => status,
                Ok(None) => continue,
                Err(error) => {
                    report.fence_failures.push(UnverifiableVolume {
                        volume_id: id,
                        detail: format!(
                            "querying {} for fence completion failed: {}",
                            entry.resource_name, error.detail
                        ),
                    });
                    continue;
                }
            };
            match self.try_complete_fence(state, &id, &entry, status.role == Role::Primary) {
                Ok(true) => match state.save(&self.state_path) {
                    Ok(()) => report.completed_fences.push(id),
                    Err(error) => {
                        report.fence_failures.push(UnverifiableVolume {
                            volume_id: id,
                            detail: format!(
                                "saving the completed fence of {} failed: {}",
                                entry.resource_name, error.detail
                            ),
                        });
                    }
                },
                Ok(false) => {}
                Err(error) => {
                    report.fence_failures.push(UnverifiableVolume {
                        volume_id: id,
                        detail: format!(
                            "completing the fence of {} failed: {}",
                            entry.resource_name, error.detail
                        ),
                    });
                }
            }
        }
    }

    fn self_fence(
        &self,
        state: &mut DrbdState,
        volume_id: &VolumeId,
        entry: &VolumeEntry,
        reasons: Vec<String>,
    ) -> Result<FencedVolume, ApiError> {
        self.suspend_io(entry.minor)?;
        // The fence marker is durable BEFORE the demotion is attempted
        // (the plan's crash-window discipline): a crash anywhere past
        // this point leaves the marker that routes the restart through
        // reconcile's fence-completion path (which demotes, resumes and
        // clears it) — never a suspended Secondary that nothing resumes
        // while the state claims it is healthy. A failed marker save
        // does NOT stop the fence: the suspension is already durable
        // in the kernel and the demotion is safe regardless of what
        // the on-disk record says; the save error is returned at the
        // end so the caller reports it.
        let fenced = FencedVolume {
            volume_id: volume_id.clone(),
            reasons,
            demoted: false,
        };
        let mut save_error = None;
        if let Some(volume) = state.volume_mut(volume_id) {
            volume.runtime.attachment = None;
            volume.runtime.authority = None;
            volume.runtime.fence = Some(PendingFence {
                reason: fenced
                    .reasons
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "writer authority lost".to_owned()),
                fenced_at: unix_now(),
            });
            volume.runtime.state = VolumeLifecycle::Failed;
            volume.entry.generation += 1;
            if let Err(error) = state.save(&self.state_path) {
                save_error = Some(error);
            }
        }
        let demoted = match self.resource_status(&entry.resource_name)? {
            Some(status) if status.role == Role::Primary => {
                let output = self.run_drbdadm("secondary", &entry.resource_name)?;
                if output.success {
                    true
                } else if is_device_busy(&output.stderr) {
                    false
                } else {
                    return Err(command_failed("drbdadm secondary", &output));
                }
            }
            // Already Secondary (e.g. an interrupted detach caught by
            // the renewal loop) or down: the fence needs no demotion.
            _ => true,
        };
        // Lift the suspension once the demotion completed: the writer
        // is provably gone and a suspended Secondary would silently
        // freeze the next attachment's I/O. A failed resume keeps the
        // pending-fence marker (the renewal pass and the startup
        // reconcile retry the completion) — never a silent freeze,
        // never a silent resume.
        let mut resume_error = None;
        if demoted {
            if let Err(error) = self.resume_io(entry.minor) {
                resume_error = Some(error);
            }
        }
        let complete = demoted && resume_error.is_none();
        if complete {
            // The fence finished: clear the durable marker. (A crash
            // before this save leaves the marker set — reconcile
            // re-runs the idempotent completion.)
            if let Some(volume) = state.volume_mut(volume_id) {
                volume.runtime.fence = None;
                volume.runtime.state = VolumeLifecycle::Ready;
                if let Err(error) = state.save(&self.state_path) {
                    save_error = save_error.or(Some(error));
                }
            }
        }
        let fenced = FencedVolume { demoted, ..fenced };
        if let Some(error) = save_error {
            return Err(error);
        }
        if let Some(error) = resume_error {
            return Err(error);
        }
        Ok(fenced)
    }

    // -- Ownership verification --

    /// Verify a state entry's backing LV: existence (from a successful
    /// `lvs`), size and the `volvisor.owner` tag.
    fn verify_backing(
        &self,
        entry: &VolumeEntry,
        volume_id: &VolumeId,
    ) -> Result<Backing, ApiError> {
        backing_from_rows(&self.list_lvs()?, entry, volume_id)
    }

    /// The ownership proof required before a mutation.
    ///
    /// `absent_code` selects the typed error for a verifiably absent
    /// LV (`INVALID_STATE` on attach/detach, `INTERNAL` on grow — the
    /// state entry demands reconciliation either way); an LV whose
    /// ownership tag is missing or foreign is always refused with
    /// `FOREIGN_DEVICE_STATE` and never adopted.
    fn require_owned_backing(
        &self,
        entry: &VolumeEntry,
        volume_id: &VolumeId,
        absent_code: ApiErrorCode,
    ) -> Result<(), ApiError> {
        let wanted = format!("{}/{}", entry.vg_name, entry.lv_name);
        match self.verify_backing(entry, volume_id)? {
            Backing::Owned { .. } => Ok(()),
            Backing::Absent => Err(ApiError::new(
                absent_code,
                format!(
                    "backing LV {wanted} is absent; volume {volume_id} requires reconciliation"
                ),
            )),
            Backing::Mismatch => Err(ApiError::new(
                ApiErrorCode::ForeignDeviceState,
                format!(
                    "backing LV {wanted} does not carry the ownership tag of volume {volume_id}; \
                     foreign state is never adopted"
                ),
            )),
        }
    }

    /// Verify a state entry's resource file against the record.
    ///
    /// The file must still name our resource, minor, protocol letter,
    /// disk path and both `on` nodes (with the recorded local port and
    /// the configured peer address), and carry a shared secret.
    fn check_res_file(&self, entry: &VolumeEntry) -> ResFileCheck {
        let path = res_file_path(&self.config.config_dir, &entry.resource_name);
        let Ok(content) = fs::read_to_string(&path) else {
            return ResFileCheck::Missing;
        };
        let Ok(parsed) = parse_resource_file(&content) else {
            return ResFileCheck::Mismatch("the file is unparsable".to_owned());
        };
        let mismatch = |detail: String| ResFileCheck::Mismatch(detail);
        if parsed.name != entry.resource_name {
            return mismatch(format!(
                "names resource {:?} instead of {:?}",
                parsed.name, entry.resource_name
            ));
        }
        if parsed.minor != Some(entry.minor) {
            return mismatch(format!(
                "carries minor {:?} instead of {}",
                parsed.minor, entry.minor
            ));
        }
        if parsed.protocol.as_deref() != Some(entry.replication_mode.as_str()) {
            return mismatch(format!(
                "carries protocol {:?} instead of {:?}",
                parsed.protocol,
                entry.replication_mode.as_str()
            ));
        }
        let disk_path = format!("/dev/{}/{}", entry.vg_name, entry.lv_name);
        if !parsed.disks.iter().any(|disk| disk == &disk_path) {
            return mismatch(format!("does not name the backing disk {disk_path}"));
        }
        if !parsed.has_shared_secret {
            return mismatch("configures no shared secret".to_owned());
        }
        let local_address = format!("{}:{}", self.config.local_address, entry.port);
        let local_ok = parsed.nodes.iter().any(|node| {
            node.name == self.config.node_name && node.address.as_deref() == Some(&local_address)
        });
        let peer_ok = match split_peer_address(&self.config.peer_address) {
            Ok((peer_ip, peer_port)) => {
                let peer_address = format!("{peer_ip}:{peer_port}");
                parsed.nodes.iter().any(|node| {
                    node.name == self.config.peer_name
                        && node.address.as_deref() == Some(&peer_address)
                })
            }
            Err(_) => false,
        };
        if !local_ok || !peer_ok {
            return mismatch("the on-node set no longer matches the deployment".to_owned());
        }
        ResFileCheck::Matches
    }

    /// The resource-file proof required before a mutation.
    ///
    /// # Errors
    /// `INTERNAL` when the file is missing (the state entry demands
    /// reconciliation); `FOREIGN_DEVICE_STATE` when it no longer
    /// matches volvisor's record (never adopted).
    fn require_entry_res_file(
        &self,
        entry: &VolumeEntry,
        volume_id: &VolumeId,
    ) -> Result<(), ApiError> {
        match self.check_res_file(entry) {
            ResFileCheck::Matches => Ok(()),
            ResFileCheck::Missing => Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "resource file for {} is missing; volume {volume_id} requires reconciliation",
                    entry.resource_name
                ),
            )),
            ResFileCheck::Mismatch(detail) => Err(ApiError::new(
                ApiErrorCode::ForeignDeviceState,
                format!(
                    "resource file for {} no longer matches volvisor's record ({detail}); \
                     foreign state is never adopted",
                    entry.resource_name
                ),
            )),
        }
    }
}

impl DrbdProvider {
    /// The fail-closed startup verification (see [`Self::new`]).
    ///
    /// Order matters for the typed error: the toolchain first (an
    /// unanswerable `drbdadm --version` is `INTERNAL`), then the kernel
    /// module (`<proc_root>/drbd` readable, `INTERNAL`), then the VG
    /// (`vgs` must report it, `NOT_FOUND`), then the host identity
    /// (`uname -n` must equal `node_name`; a mismatch is
    /// `FOREIGN_DEVICE_STATE` — this provider would be running on the
    /// wrong host), then the secret file (`INVALID_REQUEST`), then the
    /// config directory (`INVALID_REQUEST`).
    fn verify_startup(&self) -> Result<(), ApiError> {
        let output = self.runner.run("drbdadm", &["--version"])?;
        if !output.success {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "drbdadm --version failed: {} (is drbd-utils installed?)",
                    output.stderr_excerpt()
                ),
            ));
        }
        let proc_file = self.config.proc_root.join("drbd");
        fs::read_to_string(&proc_file).map_err(|e| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "cannot read {} ({e}): is the DRBD kernel module loaded?",
                    proc_file.display()
                ),
            )
        })?;
        if !self
            .list_vgs()?
            .iter()
            .any(|row| row.vg_name.as_deref() == Some(self.config.vg_name.as_str()))
        {
            return Err(ApiError::not_found(format!(
                "configured volume group {:?} is not reported by vgs",
                self.config.vg_name
            )));
        }
        let output = self.runner.run("uname", &["-n"])?;
        if !output.success {
            return Err(command_failed("uname -n", &output));
        }
        let observed_node = output.stdout.trim();
        if observed_node != self.config.node_name {
            return Err(ApiError::new(
                ApiErrorCode::ForeignDeviceState,
                format!(
                    "node_name mismatch: the configuration names {:?} but `uname -n` reports \
                     {observed_node:?}; this provider would be running on the wrong host",
                    self.config.node_name
                ),
            ));
        }
        // The secret is read here only as a startup proof (readable and
        // non-empty); the value itself is consumed at resource-generation
        // time and never stored or logged.
        read_shared_secret(&self.config.shared_secret_file)
            .map_err(|error| ApiError::new(ApiErrorCode::InvalidRequest, error.detail))?;
        if !self.config.config_dir.is_dir() {
            return Err(ApiError::invalid_request(format!(
                "config_dir {} is not a directory",
                self.config.config_dir.display()
            )));
        }
        Ok(())
    }

    /// Seed a provably-fresh resource: `primary --force` (the only
    /// sanctioned use of `--force` — the local disk has been observed
    /// `Inconsistent`, never seeded), verify the role from
    /// `drbdsetup status`, demote with `secondary`, verify again.
    ///
    /// # Errors
    /// `INTERNAL` when any step fails or its verification contradicts
    /// the command's exit status — a half-seeded resource is never
    /// recorded as seeded (a fresh one is torn down by the caller).
    fn seed_resource(&self, resource: &str) -> Result<(), ApiError> {
        let output = self.run_drbdadm_seed(resource)?;
        if !output.success {
            return Err(command_failed("drbdadm primary --force", &output));
        }
        let Some(status) = self.resource_status(resource)? else {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!("resource {resource} is not up after a successful primary --force"),
            ));
        };
        if status.role != Role::Primary {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "drbdadm primary --force reported success but the role of {resource} is \
                     not Primary"
                ),
            ));
        }
        let output = self.run_drbdadm("secondary", resource)?;
        if !output.success {
            return Err(command_failed("drbdadm secondary", &output));
        }
        let Some(status) = self.resource_status(resource)? else {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!("resource {resource} is not up after a successful secondary"),
            ));
        };
        if status.role != Role::Secondary {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "drbdadm secondary reported success but the role of {resource} is not \
                     Secondary"
                ),
            ));
        }
        Ok(())
    }

    /// Tear down a half-created resource after a failed create.
    ///
    /// Best-effort by design (the caller is already on an error path; a
    /// failing cleanup must not mask the original failure): `down`
    /// (which also demotes a resource a half-failed seed left Primary),
    /// remove the resource file, and remove the backing LV **only when
    /// this very call created it** (`fresh_lv`) — a crashed
    /// predecessor's LV is never destroyed by us.
    fn teardown_after_failed_create(&self, entry: &VolumeEntry, fresh_lv: bool) {
        drop(self.run_drbdadm("down", &entry.resource_name));
        drop(fs::remove_file(res_file_path(
            &self.config.config_dir,
            &entry.resource_name,
        )));
        if fresh_lv {
            let spec = format!("{}/{}", entry.vg_name, entry.lv_name);
            drop(self.runner.run("lvremove", &["--yes", &spec]));
        }
    }

    /// Bring a resource up: `create-md` (internal metadata), then `up`,
    /// then verify the resource answers `drbdsetup status`.
    ///
    /// Real `drbdmeta` semantics (verified against the drbdmeta
    /// source): with a non-tty stdin the move prompt auto-declines and
    /// `md_initialize` then RE-INITIALIZES (wipes and rewrites) the
    /// metadata whenever the data-area start is all-zero — so on a
    /// fresh all-zero LV, even one carrying a crashed predecessor's
    /// valid metadata, `create-md` SUCCEEDS and rewrites. Only
    /// non-zero data at the data-area start makes it refuse ("Operation
    /// refused"). A `create-md` refusal is therefore not fatal by
    /// itself: the LV may hold data under valid metadata that `up`
    /// adopts (the crash-recovery decider is the status
    /// [`Self::establish_replica`] reads, not the create-md exit), and
    /// if `up` also fails, the original `create-md` error surfaces.
    ///
    /// Real-cluster confirmation of these create-md semantics is
    /// outstanding (see the integration test's gating note) — no CI
    /// slice runs the gated real-cluster tests.
    ///
    /// # Errors
    /// `INTERNAL` when the commands fail or the resource does not answer
    /// `drbdsetup status` afterwards.
    fn bring_up_resource(&self, entry: &VolumeEntry) -> Result<ResourceStatus, ApiError> {
        let create_md = self.run_drbdadm("create-md", &entry.resource_name)?;
        let up = self.run_drbdadm("up", &entry.resource_name)?;
        if !up.success {
            if !create_md.success {
                return Err(command_failed("drbdadm create-md", &create_md));
            }
            return Err(command_failed("drbdadm up", &up));
        }
        let Some(status) = self.resource_status(&entry.resource_name)? else {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "drbdadm up reported success but drbdsetup status does not report {}",
                    entry.resource_name
                ),
            ));
        };
        Ok(status)
    }

    /// Establish the replica of a freshly brought-up resource and return
    /// whether it is seeded.
    ///
    /// Seeding (`primary --force`) runs **only** over a provably fresh
    /// resource: local disk `Inconsistent`. The peer decides:
    ///
    /// - `Inconsistent` peer, not Primary (provably fresh too): seed
    ///   through the verified promote/demote sequence;
    /// - peer holding data (`UpToDate`/`Consistent`/`Outdated`) or
    ///   Primary: **foreign data, never overwritten** — a typed
    ///   `FOREIGN_DEVICE_STATE` refusal (a resource this call created
    ///   is torn down first so no half-state survives);
    /// - peer absent (or diskless/unknown): nothing to seed against. With
    ///   `allow_degraded_create` the volume is honestly left unseeded
    ///   (split-brain avoidance: a later peer appearance is reconciled
    ///   before any seeding); without it the create fails typed.
    ///
    /// A local disk already holding data (`UpToDate`/`Consistent`/
    /// `Outdated`) on a resource this volume owns is a state-loss
    /// replay — a crashed predecessor seeded it after `lvcreate` but
    /// before the state save — and is adopted as seeded.
    ///
    /// # Errors
    /// Typed as described; a resource this call created is torn down
    /// before any error is returned.
    fn establish_replica(
        &self,
        entry: &VolumeEntry,
        status: &ResourceStatus,
        allow_degraded: bool,
        fresh_lv: bool,
    ) -> Result<bool, ApiError> {
        let resource = entry.resource_name.as_str();
        let teardown = |fresh: bool| {
            if fresh {
                self.teardown_after_failed_create(entry, fresh);
            }
        };
        match &status.local_disk {
            DiskState::Inconsistent => match peer_freshness(status) {
                PeerFreshness::Fresh => match self.seed_resource(resource) {
                    Ok(()) => Ok(true),
                    Err(error) => {
                        teardown(fresh_lv);
                        Err(error)
                    }
                },
                PeerFreshness::Foreign => {
                    teardown(fresh_lv);
                    Err(ApiError::new(
                        ApiErrorCode::ForeignDeviceState,
                        format!(
                            "the peer of {resource} holds data (peer disk {:?}, peer role {:?}); \
                             foreign peer state is never overwritten",
                            status.peer_disk, status.peer_role
                        ),
                    ))
                }
                PeerFreshness::Absent => {
                    if allow_degraded {
                        // Deliberately NOT seeded: seeding without seeing
                        // the peer's state would risk overwriting peer
                        // data when it appears (split-brain avoidance);
                        // reconcile seeds once the peer is observed fresh.
                        Ok(false)
                    } else {
                        teardown(fresh_lv);
                        Err(unsupported(
                            "the peer replica is not established and allow_degraded_create is \
                             false; create refused (no replica, no silent degradation)",
                        ))
                    }
                }
            },
            DiskState::UpToDate | DiskState::Consistent | DiskState::Outdated => {
                // State-loss replay: the ownership tag plus the injective
                // resource name prove the resource is this volume's, and a
                // data-holding local disk proves a predecessor's seed ran.
                Ok(true)
            }
            other => {
                teardown(fresh_lv);
                Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!("unexpected local disk state {other:?} after bring-up of {resource}"),
                ))
            }
        }
    }

    /// Mark a volume `Failed` (idempotently; the caller persists when
    /// `changed` says something moved).
    fn fail_volume(
        state: &mut DrbdState,
        id: &VolumeId,
        snapshot: &StoredVolume,
        changed: &mut bool,
    ) {
        if snapshot.runtime.state != VolumeLifecycle::Failed {
            if let Some(volume) = state.volume_mut(id) {
                volume.runtime.state = VolumeLifecycle::Failed;
                *changed = true;
            }
        }
    }

    /// Mark a volume `Failed` and clear its attachment record, preserving
    /// the cleared record in the report as the audit trail.
    ///
    /// Used when the backing is verifiably absent or no longer provably
    /// ours, or the resource is verifiably down: the record's authority
    /// claim is void, and keeping it would wedge the volume forever
    /// (detach refuses on the absent device, delete refuses on "must be
    /// fully detached"; the documented restart remedy relies on this
    /// clear). Only the RECORD is dropped — an actual zombie device is
    /// left to an operator and never touched.
    fn fail_and_clear(
        state: &mut DrbdState,
        id: &VolumeId,
        snapshot: &StoredVolume,
        reason: ClearedAttachmentReason,
        report: &mut ReconcileReport,
        changed: &mut bool,
    ) {
        Self::fail_volume(state, id, snapshot, changed);
        // The lease block is cleared too: the resource is verifiably
        // gone (backing vanished/foreign or the resource down), so its
        // writer is not serving; an orphaned lease would only be
        // renewed pointlessly. It lapses at its recorded end, bounded
        // harmlessly by W1/W7.
        if snapshot.runtime.authority.is_some() {
            if let Some(volume) = state.volume_mut(id) {
                volume.runtime.authority = None;
            }
            *changed = true;
        }
        if let Some(record) = snapshot.runtime.attachment.as_ref() {
            if let Some(volume) = state.volume_mut(id) {
                volume.runtime.attachment = None;
            }
            *changed = true;
            report.cleared_attachments.push(ClearedAttachment {
                volume_id: id.clone(),
                device: record.device.clone(),
                reason,
            });
        }
    }

    /// Reconcile provider state against the observed host.
    ///
    /// Non-destructive by construction (no device is demoted, removed or
    /// adopted here), per volume and in this order:
    ///
    /// - backing LV verifiably absent (from a successful `lvs`):
    ///   `Failed`, attachment record cleared ([`ClearedAttachmentReason::VanishedBacking`]);
    /// - LV present but its `volvisor.owner` tag missing or foreign:
    ///   `Failed`, record cleared ([`ClearedAttachmentReason::OwnershipMismatch`])
    ///   — never adopted;
    /// - resource file missing, unparsable or no longer matching the
    ///   record: `Failed`, record **kept** (the resource may still be
    ///   live; the operator restores the file or removes the volume out
    ///   of band);
    /// - resource status query failure: an honest unknown — the entry is
    ///   left completely untouched and counted as unverifiable (an
    ///   outage is never persisted as `Failed`);
    /// - resource verifiably down: `Failed`, record cleared
    ///   ([`ClearedAttachmentReason::ResourceDown`]) — the `/dev/drbdN`
    ///   device the record named no longer exists;
    /// - resource Primary without a record (a zombie promotion from a
    ///   crashed prior life): `Failed`, reported, **never auto-demoted**
    ///   (rule 17: demotion requires releasing an in-use source device,
    ///   an operator action);
    /// - record present but the resource is up and Secondary (an
    ///   interrupted detach: the demotion succeeded, the state save did
    ///   not): record cleared
    ///   ([`ClearedAttachmentReason::InterruptedDetach`]), volume back
    ///   to `Ready`;
    /// - unseeded volume whose local disk holds data: the seeding flag
    ///   is healed (a predecessor's seed completed before its state
    ///   save);
    /// - unseeded volume, local `Inconsistent`, peer just appeared fresh
    ///   (`Inconsistent`, not Primary): seeded this pass through the
    ///   verified sequence; a peer holding data is foreign — `Failed`,
    ///   never overwritten; an absent peer is waited for;
    /// - effective device size above the record: a completed-but-
    ///   unrecorded grow, healed up; below the record: the backing
    ///   changed outside volvisor — `Failed`, never healed downward.
    ///
    /// Untracked LVs in the nearline VG are classified without being
    /// touched (AGENTS rule 7): carrying a `volvisor.owner` tag without a
    /// state entry (e.g. a crash between `lvcreate` and the state save)
    /// or foreign (no tag).
    ///
    /// # Errors
    /// Returns an [`ApiError`] when the `lvs` query cannot be executed
    /// (reconciliation then never ran — an honest unknown, never an
    /// empty report implying consistency) or when a mutation cannot be
    /// persisted.
    // One pass per volume whose step ORDER is the safety argument
    // (absent → foreign → unverifiable → down → zombie → interrupted
    // detach → seeding → size boundary); splitting it would scatter
    // those ordering invariants across helpers.
    #[allow(clippy::too_many_lines)]
    pub fn reconcile(&self) -> Result<ReconcileReport, ApiError> {
        let rows = self.list_lvs()?;
        let mut state = self.lock_state()?;
        let mut report = ReconcileReport::default();
        let mut changed = false;
        let ids: Vec<VolumeId> = state.volumes().keys().cloned().collect();
        for id in ids {
            let Some(snapshot) = state.volume(&id).cloned() else {
                continue;
            };
            let entry = snapshot.entry.clone();
            // 1. The backing LV decides everything else.
            match backing_from_rows(&rows, &entry, &id)? {
                Backing::Absent => {
                    Self::fail_and_clear(
                        &mut state,
                        &id,
                        &snapshot,
                        ClearedAttachmentReason::VanishedBacking,
                        &mut report,
                        &mut changed,
                    );
                    report.missing_volumes.push(id);
                    continue;
                }
                Backing::Mismatch => {
                    Self::fail_and_clear(
                        &mut state,
                        &id,
                        &snapshot,
                        ClearedAttachmentReason::OwnershipMismatch,
                        &mut report,
                        &mut changed,
                    );
                    report.mismatched_volumes.push(id);
                    continue;
                }
                Backing::Owned { .. } => {}
            }
            // 2. The resource file must still be verifiably ours.
            if !matches!(self.check_res_file(&entry), ResFileCheck::Matches) {
                // Failed, but the attachment record is KEPT: the resource
                // may still be live and the record is the only authority
                // trail; the operator restores the file or deletes the
                // volume out of band.
                Self::fail_volume(&mut state, &id, &snapshot, &mut changed);
                report.resource_file_mismatches.push(id);
                continue;
            }
            // 3. The resource status (a query failure is an honest
            //    unknown: the entry is left completely untouched).
            let status = match self.resource_status(&entry.resource_name) {
                Ok(status) => status,
                Err(error) => {
                    report.unverifiable_volumes.push(UnverifiableVolume {
                        volume_id: id,
                        detail: error.detail,
                    });
                    continue;
                }
            };
            let Some(status) = status else {
                Self::fail_and_clear(
                    &mut state,
                    &id,
                    &snapshot,
                    ClearedAttachmentReason::ResourceDown,
                    &mut report,
                    &mut changed,
                );
                report.downed_volumes.push(id);
                continue;
            };
            // 4. Complete a pending self-fence (P4a plan §4): the
            //    resource is volvisor's own suspended device — the
            //    demotion was refused only because it was still open.
            //    It completes once the device closes (and the
            //    suspension this host imposed is lifted with it); a
            //    resource carrying the marker is never confused with a
            //    foreign zombie promotion (which is never
            //    auto-demoted, rule 17). Never a silent resume. The
            //    completion core is shared with the renewal pass's
            //    fence lane ([`Self::try_complete_fence`]).
            if snapshot.runtime.fence.is_some() {
                match self.try_complete_fence(&mut state, &id, &entry, status.role == Role::Primary)
                {
                    Ok(true) => {
                        changed = true;
                        report.completed_fences.push(id);
                    }
                    Ok(false) => {}
                    Err(error) => {
                        report.unverifiable_volumes.push(UnverifiableVolume {
                            volume_id: id,
                            detail: format!(
                                "completing the fence of {} failed: {}",
                                entry.resource_name, error.detail
                            ),
                        });
                    }
                }
                continue;
            }
            // 5. Writer-authority validation (P4a plan §4): in
            //    witness-managed mode every Primary found on this host
            //    is unproven until validated — keyed on the role and
            //    the lease, NOT the attachment record (a crash between
            //    promote and record save leaves a Primary with no
            //    record; it is still an unvalidated writer). I/O is
            //    suspended first (fail-closed ordering: a stalled
            //    witness must leave the writer frozen, never serving),
            //    the lease is validated via inspect, and the device is
            //    resumed only on a live lease for our epoch whose
            //    remaining duration covers the renewal margin.
            //    Pre-authority (P3) mode keeps exactly the P3 rules.
            if status.role == Role::Primary {
                if let Some(authority) = &self.authority {
                    if let Err(error) = self.suspend_io(entry.minor) {
                        report.unverifiable_volumes.push(UnverifiableVolume {
                            volume_id: id,
                            detail: format!(
                                "suspending the primary {} failed: {}",
                                entry.resource_name, error.detail
                            ),
                        });
                        continue;
                    }
                    match snapshot.runtime.authority.as_ref() {
                        Some(block) => match authority.validate(&id, block) {
                            Ok(LeaseValidity::Valid { .. }) => {
                                // Proven: resume. A volume that a previous
                                // pass failed only because the witness was
                                // unreachable heals back to Attached (its
                                // attachment record survived); a valid
                                // Primary without an attachment record
                                // falls through to the zombie report
                                // below — reported exactly as in P3,
                                // never auto-demoted, serving under its
                                // live lease.
                                if let Err(error) = self.resume_io(entry.minor) {
                                    report.unverifiable_volumes.push(UnverifiableVolume {
                                        volume_id: id,
                                        detail: format!(
                                            "resuming the validated primary {} failed: {}",
                                            entry.resource_name, error.detail
                                        ),
                                    });
                                    continue;
                                }
                                if snapshot.runtime.attachment.is_some()
                                    && snapshot.runtime.state == VolumeLifecycle::Failed
                                {
                                    if let Some(volume) = state.volume_mut(&id) {
                                        volume.runtime.state = VolumeLifecycle::Attached;
                                    }
                                    changed = true;
                                }
                            }
                            Ok(LeaseValidity::Invalid { reasons }) => {
                                match self.self_fence(&mut state, &id, &entry, reasons) {
                                    Ok(fenced) => report.fenced_volumes.push(fenced),
                                    Err(error) => {
                                        report.unverifiable_volumes.push(UnverifiableVolume {
                                            volume_id: id,
                                            detail: format!(
                                                "self-fencing {} failed: {}",
                                                entry.resource_name, error.detail
                                            ),
                                        });
                                    }
                                }
                                continue;
                            }
                            Err(error) => {
                                // Unreachable or otherwise unvalidatable
                                // witness: stay suspended, fail-closed. The
                                // attachment record is KEPT (the writer
                                // resumes once the witness answers again);
                                // the lifecycle records the refusal.
                                Self::fail_volume(&mut state, &id, &snapshot, &mut changed);
                                report.unvalidated_primaries.push(UnverifiableVolume {
                                    volume_id: id,
                                    detail: error.detail,
                                });
                                continue;
                            }
                        },
                        None => {
                            // A Primary with no persisted lease on a
                            // witness-managed host: unproven. The witness
                            // decides which kind: an UNREGISTERED volume
                            // is pre-authority (P3-era) and keeps exactly
                            // its P3 handling; a registered volume without
                            // a block (e.g. a rolled-back state file)
                            // stays suspended — its writer cannot renew
                            // or prove anything.
                            match authority.inspect(&id) {
                                // Unregistered (UnknownVolume is mapped
                                // to INVALID_STATE): pre-authority.
                                Err(error) if error.code == ApiErrorCode::InvalidState => {
                                    if let Err(error) = self.resume_io(entry.minor) {
                                        report.unverifiable_volumes.push(UnverifiableVolume {
                                            volume_id: id,
                                            detail: format!(
                                                "resuming the pre-authority primary {} \
                                                 failed: {}",
                                                entry.resource_name, error.detail
                                            ),
                                        });
                                        continue;
                                    }
                                }
                                // Registered: suspended zombie, reported
                                // with the reason.
                                Ok(_) => {
                                    Self::fail_volume(&mut state, &id, &snapshot, &mut changed);
                                    report.zombie_primaries.push(id.clone());
                                    report.unvalidated_primaries.push(UnverifiableVolume {
                                        volume_id: id,
                                        detail: format!(
                                            "primary {} holds no persisted authority block \
                                             on a witness-registered volume: suspended as \
                                             unproven; re-acquire authority out of band",
                                            entry.resource_name
                                        ),
                                    });
                                    continue;
                                }
                                // Unreachable witness: stay suspended.
                                Err(error) => {
                                    Self::fail_volume(&mut state, &id, &snapshot, &mut changed);
                                    report.unvalidated_primaries.push(UnverifiableVolume {
                                        volume_id: id,
                                        detail: error.detail,
                                    });
                                    continue;
                                }
                            }
                        }
                    }
                }
            }
            // 6. A zombie promotion is reported and Failed, never
            //    auto-demoted (pre-authority volumes, unregistered
            //    primaries and pre-authority mode).
            if status.role == Role::Primary && snapshot.runtime.attachment.is_none() {
                Self::fail_volume(&mut state, &id, &snapshot, &mut changed);
                report.zombie_primaries.push(id);
                continue;
            }
            // 7. An interrupted detach: the demotion succeeded but the
            //    state save did not. Clear the record (and any orphaned
            //    lease block — the lease lapses at its recorded end,
            //    bounded harmlessly by W1/W7), return to Ready, and
            //    keep reconciling the (now detached) volume.
            if snapshot.runtime.attachment.is_some() && status.role == Role::Secondary {
                if let Some(volume) = state.volume_mut(&id) {
                    volume.runtime.attachment = None;
                    volume.runtime.authority = None;
                    if volume.runtime.state == VolumeLifecycle::Attached {
                        volume.runtime.state = VolumeLifecycle::Ready;
                    }
                }
                changed = true;
                if let Some(record) = snapshot.runtime.attachment.as_ref() {
                    report.cleared_attachments.push(ClearedAttachment {
                        volume_id: id.clone(),
                        device: record.device.clone(),
                        reason: ClearedAttachmentReason::InterruptedDetach,
                    });
                }
            }
            // 6. Seeding: heal the flag on a data-holding local disk,
            //    seed a provably-fresh resource whose peer just appeared
            //    fresh, refuse foreign peer data, wait for an absent one.
            if !snapshot.runtime.seeded {
                match &status.local_disk {
                    DiskState::UpToDate | DiskState::Consistent | DiskState::Outdated => {
                        if let Some(volume) = state.volume_mut(&id) {
                            volume.runtime.seeded = true;
                        }
                        changed = true;
                    }
                    DiskState::Inconsistent => match peer_freshness(&status) {
                        PeerFreshness::Fresh => match self.seed_resource(&entry.resource_name) {
                            Ok(()) => {
                                if let Some(volume) = state.volume_mut(&id) {
                                    volume.runtime.seeded = true;
                                }
                                changed = true;
                                report.seeded_volumes.push(id.clone());
                            }
                            Err(error) => {
                                // A half-failed seed is an honest unknown;
                                // the volume is left untouched.
                                report.unverifiable_volumes.push(UnverifiableVolume {
                                    volume_id: id,
                                    detail: error.detail,
                                });
                                continue;
                            }
                        },
                        PeerFreshness::Foreign => {
                            Self::fail_volume(&mut state, &id, &snapshot, &mut changed);
                            report.foreign_peer_volumes.push(id);
                            continue;
                        }
                        PeerFreshness::Absent => {}
                    },
                    _ => {}
                }
            }
            // 7. Size agreement from the device itself.
            match self.device_size(entry.minor) {
                Ok(device) if device > snapshot.entry.size_bytes => {
                    if let Some(volume) = state.volume_mut(&id) {
                        volume.entry.size_bytes = device;
                    }
                    changed = true;
                    report.healed_grown.push(id);
                }
                Ok(device) if device < snapshot.entry.size_bytes => {
                    Self::fail_volume(&mut state, &id, &snapshot, &mut changed);
                    report.shrunk_volumes.push(id);
                }
                Ok(_) => {}
                Err(error) => {
                    report.unverifiable_volumes.push(UnverifiableVolume {
                        volume_id: id,
                        detail: error.detail,
                    });
                }
            }
        }

        // Untracked-LV pass: LVs in the nearline VG without a state
        // entry, classified without being touched (AGENTS rule 7).
        let tracked: Vec<&str> = state
            .volumes()
            .values()
            .map(|volume| volume.entry.lv_name.as_str())
            .collect();
        for row in &rows {
            if row.vg_name.as_deref() != Some(self.config.vg_name.as_str()) {
                continue;
            }
            let Some(lv) = row.lv_name.as_deref() else {
                continue;
            };
            if tracked.contains(&lv) {
                continue;
            }
            if row.tag(OWNER_TAG).is_some() {
                report.untracked_owned_lvs.push(lv.to_owned());
            } else {
                report.foreign_lvs.push(lv.to_owned());
            }
        }

        if changed {
            state.save(&self.state_path)?;
        }
        // Retain the report (audit trail) before handing it to the
        // caller: construction's startup pass discards the return value,
        // and records it cleared must stay observable via
        // `last_reconcile_report`.
        *self.lock_last_reconcile()? = Some(report.clone());
        Ok(report)
    }

    /// One writer-authority renewal pass (P4a plan §4): for every
    /// volume holding a lease — attached or not, `Failed` included —
    /// fence writers past their W5 local deadline, renew due leases,
    /// and defer failures that do not prove authority lost (an
    /// unreachable witness: keep serving until the local deadline, the
    /// honest bound). The renewal predicate is deliberately "holds an
    /// authority block", not "is Attached": the lease is never dropped
    /// unilaterally — a `Failed` or zombie volume keeps renewing until
    /// the WITNESS ends the lease (expiry after a failed renewal, a
    /// recorded W6 forced revocation, or W4 epoch retirement), because
    /// a deliberately lapped lease would auto-demote a zombie, which
    /// P3's rules forbid, and the witness is the arbiter.
    ///
    /// The daemon's background task calls this on its renewal cadence.
    /// The deadline check runs on **every** call, so the task may (and
    /// should) tick faster than the renewal interval — the plan's §2
    /// timing analysis assumes the fence lands within the grace
    /// budget, which requires a small enforcement granularity (e.g. a
    /// one-second tick; the witness's `lease_grace_secs` +
    /// `suspend_budget_secs` must cover response latency plus the
    /// tick).
    ///
    /// Each pass also runs the **fence lane** (a private
    /// completion pass shared with the startup reconcile's step 4):
    /// pending self-fences are
    /// completed when their device has closed, so a busy-device fence
    /// does not stay suspended until a restart (query and completion
    /// failures there are reported, never fatal to the pass).
    ///
    /// The pass is two-phase: **fences first** (suspension and demotion
    /// are entirely local — a past-deadline writer's enforcement never
    /// queues behind another volume's blocking witness call), then
    /// renewals. A fence that itself fails is reported in
    /// [`RenewalReport::fence_failures`] and retried by the next pass
    /// — it never aborts the other volumes' fences or renewals.
    ///
    /// Pre-authority (P3) mode is a no-op returning an empty report.
    ///
    /// # Errors
    /// `INTERNAL` when a renewed lease's state save fails (the next
    /// pass retries). Fence failures are reported in the report, not
    /// returned — the suspension is already durable in the kernel
    /// whenever the suspend itself succeeded.
    pub fn renew_leases(&self) -> Result<RenewalReport, ApiError> {
        let Some(authority) = &self.authority else {
            return Ok(RenewalReport::default());
        };
        let now = authority.now_secs();
        let mut state = self.lock_state()?;
        let mut report = RenewalReport::default();
        // The fence lane first: completing a fence can free the device
        // (and the volume's authority residue) before the lease logic
        // looks at it.
        self.complete_pending_fences(&mut state, &mut report);
        let ids: Vec<VolumeId> = state.volumes().keys().cloned().collect();
        // Phase 1 — fences first, and fence-only: every past-deadline
        // writer is suspended and demoted BEFORE any blocking witness
        // call runs. `self_fence` is entirely local (the lease is left
        // to the witness's own expiry — it is never released here), so
        // a past-deadline volume's enforcement must not queue behind
        // another volume's slow renewal: the W7 budget covers response
        // latency plus the tick, not N × the witness timeout. A fence
        // that itself fails is reported (and retried next pass)
        // without aborting the other volumes' fences.
        for id in &ids {
            let Some(snapshot) = state.volume(id).cloned() else {
                continue;
            };
            let Some(block) = snapshot.runtime.authority.clone() else {
                continue;
            };
            // Past the W5 local deadline the writer self-fences no
            // matter why the renewal failed: the deadline is the bound
            // the writer promised (a witness that never answers is not
            // a license to keep writing).
            if now < block.deadline_at {
                continue;
            }
            let reasons = vec![format!(
                "the W5 local deadline {} passed (lease acquired {})",
                block.deadline_at, block.acquired_at
            )];
            match self.self_fence(&mut state, id, &snapshot.entry, reasons) {
                Ok(fenced) => report.fenced.push(fenced),
                Err(error) => {
                    report.fence_failures.push(UnverifiableVolume {
                        volume_id: id.clone(),
                        detail: format!(
                            "self-fencing {} failed: {}",
                            snapshot.entry.resource_name, error.detail
                        ),
                    });
                }
            }
        }
        // Phase 2 — renewals (blocking witness calls; a slow witness
        // can only delay other renewals, never a fence).
        for id in ids {
            let Some(snapshot) = state.volume(&id).cloned() else {
                continue;
            };
            let Some(block) = snapshot.runtime.authority.clone() else {
                continue;
            };
            if now >= block.deadline_at {
                continue;
            }
            // Renewal is due only after a full interval since the last
            // response-anchored acquisition (a fast-ticking task must
            // not hammer the witness).
            if now
                < block
                    .acquired_at
                    .saturating_add(authority.renewal_interval_secs())
            {
                continue;
            }
            match authority.renew_lease(&id, &block) {
                Ok(refreshed) => {
                    if let Some(volume) = state.volume_mut(&id) {
                        volume.runtime.authority = Some(refreshed);
                    }
                    state.save(&self.state_path)?;
                    report.renewed.push(id);
                }
                Err(err) if err.code == ApiErrorCode::StaleEpoch => {
                    // The witness retired the epoch: this writer is
                    // fenced, provably (W4).
                    let reasons = vec![format!("the witness retired the lease: {}", err.detail)];
                    match self.self_fence(&mut state, &id, &snapshot.entry, reasons) {
                        Ok(fenced) => report.fenced.push(fenced),
                        Err(error) => {
                            report.fence_failures.push(UnverifiableVolume {
                                volume_id: id.clone(),
                                detail: format!(
                                    "self-fencing {} after a stale epoch failed: {}",
                                    snapshot.entry.resource_name, error.detail
                                ),
                            });
                        }
                    }
                }
                Err(err) => {
                    // Not proven lost: keep serving until the deadline
                    // — the deferred entry carries the bound.
                    report.deferred.push(DeferredRenewal {
                        volume_id: id,
                        detail: err.detail,
                        deadline_at: block.deadline_at,
                    });
                }
            }
        }
        Ok(report)
    }

    // -- Registration and unplanned promotion (P4a plan §3/§5) --

    /// Read and parse the resource's own definition file. The caller
    /// is expected to have verified the file matches the record
    /// ([`Self::check_res_file`]) where a record exists.
    ///
    /// # Errors
    /// `INTERNAL` when the file cannot be read or parsed.
    fn parsed_definition(&self, resource: &str) -> Result<ParsedResource, ApiError> {
        let path = res_file_path(&self.config.config_dir, resource);
        let content = fs::read_to_string(&path).map_err(|error| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "failed to read the resource file {}: {error}",
                    path.display()
                ),
            )
        })?;
        parse_resource_file(&content)
    }

    /// The definition's backing disk path — the P3 operator model
    /// deploys the identical single-volume definition on both hosts,
    /// so exactly one distinct path may appear; anything else is not a
    /// volvisor resource and is never guessed.
    ///
    /// # Errors
    /// `INTERNAL` when no disk path is present or several distinct
    /// paths appear.
    fn definition_disk(resource: &str, definition: &ParsedResource) -> Result<String, ApiError> {
        let disk = definition.disks.first().cloned().ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("the resource definition for {resource} carries no disk path"),
            )
        })?;
        if definition.disks.iter().any(|path| *path != disk) {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "the resource definition for {resource} carries several distinct disk \
                     paths: not the single-volume operator model"
                ),
            ));
        }
        Ok(disk)
    }

    /// The resource's live data-generation identifiers
    /// (`drbdsetup show-gi <resource> <peer-node-id> 0` — a
    /// CTX_PEER_DEVICE command; the argv and output shapes are pinned
    /// by `tests/drbd_authority_shapes.rs`). The peer node id comes
    /// from the resource definition's peer section — never guessed.
    /// Returned sorted and deduplicated (the registration's set form).
    ///
    /// # Errors
    /// `INTERNAL` when the definition carries no peer `node-id`, the
    /// command fails or the output has no UUID line — the lineage is
    /// never guessed.
    fn lineage_uuids(&self, resource: &str) -> Result<Vec<String>, ApiError> {
        let definition = self.parsed_definition(resource)?;
        let peer_node_id = definition
            .nodes
            .iter()
            .find(|node| node.name != self.config.node_name)
            .and_then(|node| node.node_id)
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!("resource definition for {resource} carries no peer node-id"),
                )
            })?;
        let peer_node_id = peer_node_id.to_string();
        // Every volvisor resource is single-volume (volume 0).
        let output = self
            .runner
            .run("drbdsetup", &["show-gi", resource, &peer_node_id, "0"])?;
        if !output.success {
            return Err(command_failed("drbdsetup show-gi", &output));
        }
        let mut uuids = parse_drbdsetup_show_gi(&output.stdout)?.lineage_uuids;
        uuids.sort();
        uuids.dedup();
        Ok(uuids)
    }

    /// Register a volume lineage with the witness (P4a plan §3): the
    /// explicit, out-of-band provisioning step that makes a volume
    /// witness-managed (nothing registers automatically — the optional
    /// operator-attested barrier is an operator decision, and an
    /// auto-registration without one would permanently foreclose
    /// `SAFE_CURRENT` for that lineage). Verifies the volume is ours
    /// first (rule 7: never register foreign state), then records the
    /// live lineage identifiers and both endpoints' backing identities
    /// as derived from the resource definition — the same definition
    /// file is deployed on both hosts (the P3 operator model), so the
    /// adopt flow on the surviving host can compare them verbatim
    /// after losing the primary entirely.
    ///
    /// # Errors
    /// `INVALID_STATE` in pre-authority mode or for an unknown
    /// volume; typed ownership/resource verification failures; the
    /// witness's own refusals (content-identical re-registration is
    /// idempotent; divergence is refused typed).
    pub fn register_volume(
        &self,
        volume_id: &VolumeId,
        barrier: Option<RecordedBarrier>,
    ) -> Result<RegisterResponse, ApiError> {
        let Some(authority) = &self.authority else {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                "this provider is not witness-managed (no authority context)",
            ));
        };
        let state = self.lock_state()?;
        let stored = state
            .volume(volume_id)
            .ok_or_else(|| not_found(volume_id))?;
        let entry = stored.entry.clone();
        // Ownership proofs before anything is attested to the witness.
        self.require_owned_backing(&entry, volume_id, ApiErrorCode::InvalidState)?;
        self.require_entry_res_file(&entry, volume_id)?;
        let lineage = self.lineage_uuids(&entry.resource_name)?;
        let definition = self.parsed_definition(&entry.resource_name)?;
        let disk = Self::definition_disk(&entry.resource_name, &definition)?;
        authority.register(
            volume_id,
            RegistrationContent {
                lineage_uuids: lineage,
                endpoints: vec![
                    EndpointBacking {
                        host_id: host_id_of(&self.config.node_name)?,
                        backing: endpoint_backing_identity(
                            &self.config.node_name,
                            &entry.resource_name,
                            &disk,
                        ),
                        volvisor_created: true,
                    },
                    EndpointBacking {
                        host_id: host_id_of(&self.config.peer_name)?,
                        backing: endpoint_backing_identity(
                            &self.config.peer_name,
                            &entry.resource_name,
                            &disk,
                        ),
                        volvisor_created: false,
                    },
                ],
                barrier,
            },
        )
    }

    /// The adopt-and-promote admin operation (P4a plan §5): the
    /// surviving host takes over a volume it does **not** hold in its
    /// state after an unplanned failover. Adoption verification first
    /// (rule 7 — never adopt foreign state), then the witness-side
    /// authority check, then the honest classification from observed
    /// facts; only `safe_current`, or an explicitly authorized
    /// `possible_loss`, promotes — under a fresh witness epoch, with
    /// `drbdadm primary --force` (the second and last justified
    /// `--force` path: the kernel's unforced promotion gate is
    /// expected to refuse promotion against a `DUnknown`/`Outdated`
    /// peer, and this is exactly that case; the gate is never relied
    /// upon as the fence). Refusals return the classification with no
    /// adoption.
    ///
    /// # Errors
    /// Typed verification failures (`INVALID_STATE`,
    /// `FOREIGN_DEVICE_STATE`), `UNKNOWN_FENCING_AUTHORITY` when the
    /// witness is unreachable, `FENCE_PENDING` while the witness is
    /// still inside the W7 fence-wait window for the retired lease.
    /// Adoption verification, host side (P4a plan §5 step 1): the
    /// derived resource name must name an up Secondary resource whose
    /// definition file names this host — never someone's live writer.
    ///
    /// # Errors
    /// `INVALID_STATE` when the resource is down, Primary, or not this
    /// host's; `INTERNAL` when the definition cannot be parsed.
    fn verify_adoption_resource(
        &self,
        volume_id: &VolumeId,
    ) -> Result<(String, ParsedResource, DiskState), ApiError> {
        let resource = resource_name_for(volume_id);
        let status = self.resource_status(&resource)?.ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::InvalidState,
                format!("resource {resource} is down; a downed resource cannot be adopted"),
            )
        })?;
        if status.role != Role::Secondary {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "resource {resource} is {:?}; adoption requires a Secondary resource \
                     (a Primary one is someone's live writer)",
                    status.role
                ),
            ));
        }
        let definition = self.parsed_definition(&resource)?;
        if !definition
            .nodes
            .iter()
            .any(|node| node.name == self.config.node_name)
        {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "the resource definition for {resource} does not name this host ({}); \
                     it is not this volume's resource",
                    self.config.node_name
                ),
            ));
        }
        Ok((resource, definition, status.local_disk))
    }

    /// Adoption verification (P4a plan §5 step 1, rule 7 — never adopt
    /// foreign state): the backing LV must exist, the live lineage
    /// must match the registration (this closes the recreated-volume
    /// hole), the registration's endpoint for this host must match the
    /// resource-definition identity verbatim, and — in the
    /// volvisor-created branch — the LV must carry the ownership tag.
    ///
    /// # Errors
    /// Typed verification failures (`INVALID_STATE`,
    /// `FOREIGN_DEVICE_STATE`); witness errors (an unregistered volume
    /// is `INVALID_STATE`, an unreachable witness
    /// `UNKNOWN_FENCING_AUTHORITY`).
    // One linear proof chain over the same facts; the extraction
    // boundary would cross seven locals (same convention as
    // `attach_volume_inner`).
    #[allow(clippy::too_many_lines)]
    fn verify_adoption(
        &self,
        authority: &AuthorityContext,
        volume_id: &VolumeId,
    ) -> Result<AdoptionFacts, ApiError> {
        let (resource, definition, local_disk) = self.verify_adoption_resource(volume_id)?;
        let disk = Self::definition_disk(&resource, &definition)?;
        let (vg_name, lv_name) = disk
            .strip_prefix("/dev/")
            .and_then(|path| path.split_once('/'))
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "the resource definition for {resource} names an unusable backing \
                         path {disk:?}"
                    ),
                )
            })?;
        let (vg_name, lv_name) = (vg_name.to_owned(), lv_name.to_owned());
        // The backing LV must exist (its tag matters only in the
        // volvisor-created branch below).
        let rows = self.list_lvs()?;
        let backing_row = rows.iter().find(|row| {
            row.vg_name.as_deref() == Some(vg_name.as_str())
                && row.lv_name.as_deref() == Some(lv_name.as_str())
        });
        if backing_row.is_none() {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!("the backing LV {vg_name}/{lv_name} of {resource} does not exist"),
            ));
        }
        let replication_mode = match definition.protocol.as_deref() {
            Some("A") => ReplicationMode::A,
            Some("B") => ReplicationMode::B,
            Some("C") => ReplicationMode::C,
            other => {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "the resource definition for {resource} carries an unusable protocol \
                         {other:?}: the replication contract is never guessed"
                    ),
                ));
            }
        };
        let live_lineage = self.lineage_uuids(&resource)?;
        // The witness must hold a registration for the volume (an
        // unregistered volume is pre-authority: not adoptable under
        // this flow — the authority check has nothing to compare).
        let view = authority.inspect(volume_id)?;
        let registration = view.registration.as_ref().ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "the witness holds no registration for {volume_id}; an unregistered \
                     (pre-authority) volume is not adoptable through the authority flow"
                ),
            )
        })?;
        let mut registered_lineage = registration.lineage_uuids.clone();
        registered_lineage.sort();
        registered_lineage.dedup();
        if registered_lineage != live_lineage {
            return Err(ApiError::new(
                ApiErrorCode::ForeignDeviceState,
                format!(
                    "the live lineage of {resource} does not match the registered lineage \
                     of {volume_id}: the resource is not the registered lineage (a recreated \
                     same-named volume is exactly this refusal)"
                ),
            ));
        }
        let our_host = host_id_of(&self.config.node_name)?;
        let our_endpoint = registration
            .endpoints
            .iter()
            .find(|endpoint| endpoint.host_id == our_host)
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "the registration of {volume_id} has no endpoint for this host \
                         ({}); this host is not a replication end of the registered lineage",
                        self.config.node_name
                    ),
                )
            })?;
        if our_endpoint.backing
            != endpoint_backing_identity(&self.config.node_name, &resource, &disk)
        {
            return Err(ApiError::new(
                ApiErrorCode::ForeignDeviceState,
                "the registered endpoint identity for this host does not match the \
                 resource definition: the backing is not the registered one"
                    .to_owned(),
            ));
        }
        let minor = definition.minor.ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("the resource definition for {resource} carries no minor"),
            )
        })?;
        let port = definition
            .nodes
            .iter()
            .find(|node| node.name == self.config.node_name)
            .and_then(ParsedNode::port)
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "the resource definition for {resource} carries no address port \
                         for this host"
                    ),
                )
            })?;
        // The volvisor-created branch: the surviving host's own LV must
        // carry the matching ownership tag. The operator-provisioned
        // branch (the P3 peer side) carries its rule-7 weight through
        // the lineage match and the endpoint identity above.
        if our_endpoint.volvisor_created {
            let tag = backing_row.and_then(|row| row.tag(OWNER_TAG));
            if tag.as_deref() != Some(volume_id.as_str()) {
                return Err(ApiError::new(
                    ApiErrorCode::ForeignDeviceState,
                    format!(
                        "the backing LV {vg_name}/{lv_name} does not carry the \
                         volvisor.owner tag of {volume_id}; foreign backing is never adopted"
                    ),
                ));
            }
        }
        Ok(AdoptionFacts {
            resource,
            vg_name,
            lv_name,
            minor,
            port,
            replication_mode,
            local_disk,
            barrier: registration.barrier.clone(),
            view,
        })
    }

    /// The plan §5 classification from observed facts only.
    /// `SAFE_CURRENT` is evidence-gated, never protocol-gated: the
    /// only P4a evidence is a recorded barrier whose attestation
    /// covers the last-acknowledged-boundary property (over protocol
    /// C's synchronous completion). Every non-`SAFE_CURRENT` row
    /// reports `POSSIBLE_LOSS` with an **unknown** boundary — a
    /// recorded barrier names no provable boundary for a volume that
    /// kept serving past it (the plan's table mandates `Unknown` for
    /// all four cells; `Known` boundaries arrive only with P4b's
    /// `BARRIER_DURABLE`, never from a registration-time attestation).
    fn classify_adoption(
        local_disk: DiskState,
        replication_mode: ReplicationMode,
        barrier: Option<&RecordedBarrier>,
        allow_loss: bool,
    ) -> PromotionClassification {
        match local_disk {
            DiskState::UpToDate if replication_mode == ReplicationMode::C && barrier.is_some() => {
                PromotionClassification::SafeCurrent
            }
            DiskState::UpToDate | DiskState::Consistent | DiskState::Outdated => {
                PromotionClassification::PossibleLoss {
                    boundary: LossBoundary::Unknown,
                    authorized: allow_loss,
                }
            }
            other => PromotionClassification::Unsafe {
                reasons: vec![format!(
                    "the local disk is {other:?}: integrity is unprovable (mid-resync \
                     loss); never a partial promotion"
                )],
            },
        }
    }

    /// The state entry a successful adoption records: identity from
    /// the verified facts, sizes from the device itself, the adopt
    /// classification as the creation payload (the honest provenance
    /// of this entry), and the reserved `adopted` project grouping
    /// (the original project identity died with the lost host's state
    /// and is never invented).
    ///
    /// # Errors
    /// `INTERNAL` when the reserved project id is rejected.
    fn adopted_volume_entry(
        facts: &AdoptionFacts,
        classification: &PromotionClassification,
        allow_loss: bool,
        size: u64,
    ) -> Result<VolumeEntry, ApiError> {
        let project_id = ProjectId::new("adopted").map_err(|error| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("reserved project id: {error}"),
            )
        })?;
        Ok(VolumeEntry {
            resource_name: facts.resource.clone(),
            vg_name: facts.vg_name.clone(),
            lv_name: facts.lv_name.clone(),
            minor: facts.minor,
            port: facts.port,
            size_bytes: size,
            requested_size_bytes: size,
            generation: 1,
            project_id,
            block_size: 4096,
            replication_mode: facts.replication_mode,
            creation_payload: format!(
                "{{\"adopted\":true,\"classification\":{classification:?},\"allow_loss\":{allow_loss}}}"
            ),
            created_at: unix_now(),
        })
    }

    /// The adopt-and-promote admin operation (P4a plan §5): the
    /// surviving host takes over a volume it does **not** hold in its
    /// state after an unplanned failover. Adoption verification first
    /// (rule 7 — never adopt foreign state), then the witness-side
    /// authority check, then the honest classification from observed
    /// facts; only `safe_current`, or an explicitly authorized
    /// `possible_loss`, promotes — under a fresh witness epoch, with
    /// `drbdadm primary --force` (the second and last justified
    /// `--force` path: the kernel's unforced promotion gate is
    /// expected to refuse promotion against a `DUnknown`/`Outdated`
    /// peer, and this is exactly that case; the gate is never relied
    /// upon as the fence). Refusals return the classification with no
    /// adoption.
    ///
    /// # Errors
    /// Typed verification failures (`INVALID_STATE`,
    /// `FOREIGN_DEVICE_STATE`), `UNKNOWN_FENCING_AUTHORITY` when the
    /// witness is unreachable, `FENCE_PENDING` while the witness is
    /// still inside the W7 fence-wait window for the retired lease.
    // Adopt is a fail-closed promotion sequence like attach
    // (verification → authority check → classification gate → grant →
    // durable record → promote → verify → unwind); splitting it would
    // scatter the crash-window invariants.
    #[allow(clippy::too_many_lines)]
    pub fn adopt_and_promote(
        &self,
        volume_id: &VolumeId,
        allow_loss: bool,
    ) -> Result<AdoptVolumeResponse, ApiError> {
        let Some(authority) = &self.authority else {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                "this provider is not witness-managed (no authority context)",
            ));
        };
        let mut state = self.lock_state()?;
        if state.volume(volume_id).is_some() {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "volume {volume_id} already exists in this host's state; adoption is for \
                     volumes the surviving host does not hold (plan §5)"
                ),
            ));
        }
        // 1. Adoption verification (rule 7).
        let facts = self.verify_adoption(authority, volume_id)?;
        // 2. Authority check: the witness must show no live lease — a
        //    proof the witness supplies, never one the caller brings.
        if facts.view.lease_state == LeaseState::Live {
            let holder = facts.view.holder.as_ref().map_or_else(
                || "no holder".to_owned(),
                |holder| holder.as_str().to_owned(),
            );
            return Ok(AdoptVolumeResponse {
                classification: PromotionClassification::Unsafe {
                    reasons: vec![format!(
                        "the witness holds a live lease for epoch {} (holder {holder}); \
                         the old authority may still write",
                        facts.view.current_epoch.0
                    )],
                },
                volume: None,
            });
        }
        // 3. Classification from observed facts only (plan §5 table).
        let classification = Self::classify_adoption(
            facts.local_disk.clone(),
            facts.replication_mode,
            facts.barrier.as_ref(),
            allow_loss,
        );
        // 4. Promotion gate: UNSAFE never promotes; POSSIBLE_LOSS
        //    requires the explicit recorded authorization.
        let authorized = match &classification {
            PromotionClassification::Unsafe { .. } => {
                return Ok(AdoptVolumeResponse {
                    classification,
                    volume: None,
                });
            }
            PromotionClassification::PossibleLoss { authorized, .. } => *authorized,
            PromotionClassification::SafeCurrent => true,
        };
        if !authorized {
            return Ok(AdoptVolumeResponse {
                classification,
                volume: None,
            });
        }
        // Promotion: a fresh epoch from the witness (the grant record
        // is the durable FencingProof that retired the old epoch — W2;
        // W7 has already waited out the fence window or the grant
        // itself refuses with FENCE_PENDING), then the durable
        // adoption record, then `primary --force` (see the method
        // docs), then verification. The record is persisted BEFORE the
        // promotion — the same crash-window discipline as attach: a
        // crash between the two leaves a TRACKED volume the
        // reconciler validates or fences, never an untracked Primary
        // holding a live lease outside every fence path.
        let size = self.device_size(facts.minor)?;
        if size == 0 {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "blockdev reports a zero-sized device for {}",
                    facts.resource
                ),
            ));
        }
        let entry = Self::adopted_volume_entry(&facts, &classification, allow_loss, size)?;
        let block = authority.acquire(volume_id, None)?;
        let stored = StoredVolume {
            entry,
            runtime: VolumeRuntime {
                state: VolumeLifecycle::Ready,
                attachment: None,
                seeded: true,
                authority: Some(block.clone()),
                fence: None,
            },
        };
        let entry = stored.entry.clone();
        state.insert_volume(volume_id.clone(), stored);
        // Keep the monotonic allocators ahead of the adopted resource
        // so a later allocation can never collide with it.
        state.observe_minor(facts.minor);
        state.observe_port(facts.port);
        if let Err(error) = state.save(&self.state_path) {
            // The durable record is absent: drop the in-memory entry
            // so the renewal loop cannot keep renewing a lease for a
            // volume this host does not durably hold, then release the
            // just-granted lease best-effort (the save precedes the
            // promotion, so the resource is provably Secondary and the
            // self-release is safe).
            state.remove_volume(volume_id);
            let _ = authority.release(volume_id, &block);
            return Err(error);
        }
        let promoted = self
            .run_drbdadm_seed(&facts.resource)
            .and_then(|output| {
                if output.success {
                    Ok(())
                } else {
                    Err(command_failed("drbdadm primary --force", &output))
                }
            })
            .and_then(|()| self.verify_promotion(&entry));
        if let Err(error) = promoted {
            let detail = error.detail.clone();
            self.unwind_failed_adoption(&mut state, volume_id, &entry, &block, &detail)?;
            return Err(error);
        }
        let Some(stored) = state.volume(volume_id) else {
            // Structurally unreachable (the lock is held and the entry
            // was saved above), but if the invariant ever broke, the
            // residue is fenced and released exactly like a failed
            // promotion — never left serving.
            self.unwind_failed_adoption(
                &mut state,
                volume_id,
                &entry,
                &block,
                "the adoption record vanished mid-operation",
            )?;
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!("the adoption record for {volume_id} vanished mid-operation"),
            ));
        };
        let response = self.verified_inspect_response(volume_id, stored)?;
        Ok(AdoptVolumeResponse {
            classification,
            volume: Some(response),
        })
    }

    /// Unwind an adoption whose promotion failed after the durable
    /// record: the volume is **fenced first** (suspend, durable
    /// pending-fence marker, demote, resume — [`Self::self_fence`]),
    /// because the resource may still be Primary. The just-granted
    /// lease is released **only on a completed fence**: a
    /// proven-demoted holder self-releasing waives the next grant's
    /// W7 wait safely, while an incomplete fence (a busy device)
    /// leaves the lease to expire at the witness, where the W7 window
    /// then guards the next grant — never a release while a writer
    /// might still be serving. The entry stays tracked and `Failed`
    /// (reconcile and the renewal pass own the residue; the entry is
    /// never silently removed, because an untracked Primary is exactly
    /// the hole the pre-promotion save exists to prevent).
    ///
    /// # Errors
    /// The fence error itself, or `INTERNAL` from the final state
    /// save. The witness lease is only ever released after a completed
    /// fence.
    fn unwind_failed_adoption(
        &self,
        state: &mut DrbdState,
        volume_id: &VolumeId,
        entry: &VolumeEntry,
        block: &VolumeAuthorityBlock,
        detail: &str,
    ) -> Result<(), ApiError> {
        let fenced = self.self_fence(
            state,
            volume_id,
            entry,
            vec![format!("the adoption promotion failed: {detail}")],
        );
        match fenced {
            Ok(fenced) => {
                // A failed adoption stays Failed (a completed
                // `self_fence` returns the volume to Ready for
                // reattachment; this is not that) — and the override is
                // durable BEFORE the release: if this save fails, the
                // error returns with the lease unreleased (it lapses at
                // the witness under the W7 window — safe), never a
                // released lease over an on-disk record that still
                // claims Ready.
                if let Some(volume) = state.volume_mut(volume_id) {
                    volume.runtime.state = VolumeLifecycle::Failed;
                    state.save(&self.state_path)?;
                }
                if fenced.demoted {
                    if let Some(authority) = &self.authority {
                        let _ = authority.release(volume_id, block);
                    }
                }
                Ok(())
            }
            Err(error) => {
                // The fence itself failed: the marker (when its save
                // succeeded) routes the restart through completion,
                // and the unreleased lease expires at the witness
                // under the W7 window. The failure is recorded
                // honestly; the lease is never released un-demoted.
                if let Some(volume) = state.volume_mut(volume_id) {
                    volume.runtime.state = VolumeLifecycle::Failed;
                }
                let _ = state.save(&self.state_path);
                Err(error)
            }
        }
    }

    /// Build the inspect response for one stored volume, verifying the
    /// backing LV, the resource and the device, and reflecting **observed
    /// facts only**.
    ///
    /// A `Failed` volume is `Unhealthy` on both axes without any query
    /// (the persisted verdict is the fact; recovery is manual). A
    /// verifiably absent/mismatched backing, a backing smaller than the
    /// record, or a device smaller than the record is reported `Failed`
    /// in the response — the persisted state is left alone so the next
    /// reconcile owns the transition. When the resource is up, the
    /// provisioned size is the device's own report and the health axes
    /// map the observed role/connection/disk states (see
    /// [`observed_health`] and [`backend_health_of`]); the remote
    /// protection axis states only that a remote replica is currently
    /// established and observed `UpToDate`, classified by the
    /// replication protocol read back from the resource's own
    /// definition file ([`remote_axis_of`]) — an observed fact, never
    /// an unconditional durability claim (rule 16). When the status
    /// cannot be observed, the health axes honestly stay `Unknown` and
    /// the sizes come from `lvs`.
    fn verified_inspect_response(
        &self,
        volume_id: &VolumeId,
        stored: &StoredVolume,
    ) -> Result<InspectVolumeResponse, ApiError> {
        let mut response = inspect_response(volume_id, stored);
        // The authority section is the WRITER's observation, not a
        // witness claim: the last response-anchored W5 deadline,
        // evaluated at the local clock. `Live` means "within the
        // deadline the writer promised to fence by", never "the
        // witness currently agrees" (an inspect against the witness is
        // a separate, explicit operation).
        if let Some(authority) = &self.authority {
            if let Some(block) = stored.runtime.authority.as_ref() {
                let now = authority.now_secs();
                let live = now < block.deadline_at;
                response.authority = Some(AuthoritySummary {
                    epoch: block.epoch,
                    lease_state: if live {
                        LeaseState::Live
                    } else {
                        LeaseState::Expired
                    },
                    holder: Some(authority.host_id().clone()),
                    lease_remaining_secs: live.then(|| block.deadline_at - now),
                });
            }
        }
        if stored.runtime.state == VolumeLifecycle::Failed {
            response.health = Health::Unhealthy;
            response.backend_health = Health::Unhealthy;
            return Ok(response);
        }
        let lv_size = match backing_from_rows(&self.list_lvs()?, &stored.entry, volume_id)? {
            Backing::Owned { size_bytes } => size_bytes,
            Backing::Absent | Backing::Mismatch => {
                response.state = VolumeLifecycle::Failed;
                response.health = Health::Unhealthy;
                response.backend_health = Health::Unhealthy;
                return Ok(response);
            }
        };
        if lv_size < stored.entry.size_bytes {
            // The backing changed outside volvisor: Failed in the
            // response; the recorded size is kept (never healed down).
            response.state = VolumeLifecycle::Failed;
            response.health = Health::Unhealthy;
            response.backend_health = Health::Unhealthy;
            return Ok(response);
        }
        response.allocated_bytes = lv_size;
        if lv_size > stored.entry.size_bytes {
            // The LV outgrew the record (a crash window after lvextend):
            // report the observed size; reconcile heals the record.
            response.provisioned_bytes = lv_size;
        }
        // Observed resource facts. A status query failure or a downed
        // resource is an honest unknown: the axes stay `Unknown` and the
        // sizes come from `lvs` (never a fabricated healthy status).
        let Some(status) = self
            .resource_status(&stored.entry.resource_name)
            .ok()
            .flatten()
        else {
            return Ok(response);
        };
        let Ok(device) = self.device_size(stored.entry.minor) else {
            return Ok(response);
        };
        if device < stored.entry.size_bytes {
            response.state = VolumeLifecycle::Failed;
            response.health = Health::Unhealthy;
            response.backend_health = Health::Unhealthy;
            return Ok(response);
        }
        response.provisioned_bytes = device;
        response.health = observed_health(&status);
        response.backend_health = backend_health_of(&status);
        response.effective_protection.remote =
            remote_axis_of(&status, self.observed_protocol(&stored.entry));
        Ok(response)
    }

    /// The replication protocol read back from the resource's own
    /// definition file (the observed fact the remote protection axis is
    /// classified by, never the recorded create-time intent).
    ///
    /// `None` when the file is missing, unparsable or carries no
    /// recognized protocol letter — an honest unknown that reports no
    /// remote protection instead of guessing.
    fn observed_protocol(&self, entry: &VolumeEntry) -> Option<ReplicationMode> {
        let path = res_file_path(&self.config.config_dir, &entry.resource_name);
        let content = fs::read_to_string(path).ok()?;
        let parsed = parse_resource_file(&content).ok()?;
        match parsed.protocol.as_deref() {
            Some("A") => Some(ReplicationMode::A),
            Some("B") => Some(ReplicationMode::B),
            Some("C") => Some(ReplicationMode::C),
            _ => None,
        }
    }
}

impl DrbdProvider {
    // -- Volume operations (sync bodies behind the async trait surface) --

    /// Run `lvcreate --yes -L <n>B --addtag <owner> --addtag <generation>
    /// -n <lv> <vg>`: the ownership tags ride the creation itself, so an
    /// LV either exists with volvisor's markers or does not exist at all
    /// — there is no untagged window a crash could leave behind.
    fn lvcreate_tagged(
        &self,
        vg_name: &str,
        lv_name: &str,
        volume_id: &VolumeId,
        size_bytes: u64,
    ) -> Result<CommandOutput, ApiError> {
        let owner = format!("{OWNER_TAG}={}", volume_id.as_str());
        let generation = format!("{GENERATION_TAG}=1");
        let size = format!("{size_bytes}B");
        self.runner.run(
            "lvcreate",
            &[
                "--yes",
                "-L",
                &size,
                "--addtag",
                &owner,
                "--addtag",
                &generation,
                "-n",
                lv_name,
                vg_name,
            ],
        )
    }

    /// Reclaim our own half-created LV after a crash window, or fail
    /// typed.
    ///
    /// A create retry whose `lvcreate` fails would otherwise be wedged
    /// forever; the escape hatch is the ownership tag: an LV whose
    /// `volvisor.owner` tag equals THIS volume id was created by a
    /// previous incarnation of this very create, so adopting it touches
    /// no foreign state (AGENTS rule 7). The LV must be at least as
    /// large as the request (the actual size becomes the effective
    /// size).
    ///
    /// Returns the LV size, whether this call created the LV (always
    /// `false` here), and the minor/port adopted from the crashed
    /// predecessor's resource file. Adoption is mandatory when the
    /// resource is verifiably up (the running kernel resource owns its
    /// minor and port; a fresh allocation would describe a different
    /// resource) and refused `INTERNAL` when the file is unparsable or
    /// its minor/port is claimed by another state entry. A downed
    /// resource keeps the fresh allocation (the stale file is
    /// overwritten by the new definition).
    // Crash-recovery decision tree over three observation sources (lvs,
    // status, res file); each branch is a distinct fail-closed rule.
    #[allow(clippy::too_many_lines)]
    fn reclaim_crashed_create(
        &self,
        state: &mut DrbdState,
        volume_id: &VolumeId,
        vg_name: &str,
        resource: &str,
        requested_bytes: u64,
        create_output: &CommandOutput,
    ) -> Result<ReclaimOutcome, ApiError> {
        let wanted = format!("{vg_name}/{resource}");
        let rows = self.list_lvs()?;
        let Some(row) = rows
            .iter()
            .find(|row| row.vg_slash_lv().as_deref() == Some(wanted.as_str()))
        else {
            // The LV does not exist: the create failure was real.
            return Err(command_failed("lvcreate", create_output));
        };
        let Some(size) = row.size_bytes() else {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!("lvs did not report a size for {wanted}"),
            ));
        };
        if row.tag(OWNER_TAG).as_deref() != Some(volume_id.as_str()) {
            return Err(ApiError::new(
                ApiErrorCode::ForeignDeviceState,
                format!(
                    "logical volume {wanted} exists without the ownership tag of volume \
                     {volume_id}; foreign state is never adopted"
                ),
            ));
        }
        if size < requested_bytes {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "orphaned LV {wanted} carries volume {volume_id}'s ownership tag but is \
                     smaller than the request ({size} < {requested_bytes} bytes); refusing to \
                     adopt it"
                ),
            ));
        }
        let adopted = match self.resource_status(resource)? {
            Some(_) => {
                let path = res_file_path(&self.config.config_dir, resource);
                let internal = |detail: String| {
                    ApiError::new(
                        ApiErrorCode::Internal,
                        format!(
                            "cannot adopt the crashed predecessor's resource file {}: {detail}",
                            path.display()
                        ),
                    )
                };
                let content = fs::read_to_string(&path).map_err(|e| {
                    internal(format!(
                        "the resource is up but the file cannot be read ({e})"
                    ))
                })?;
                let parsed =
                    parse_resource_file(&content).map_err(|error| internal(error.detail))?;
                if parsed.name != resource {
                    return Err(internal(format!(
                        "it names resource {:?} instead of {resource:?}",
                        parsed.name
                    )));
                }
                let disk_path = format!("/dev/{wanted}");
                if !parsed.disks.iter().any(|disk| disk == &disk_path) {
                    return Err(internal(format!(
                        "it does not name the backing disk {disk_path}"
                    )));
                }
                let minor = parsed
                    .minor
                    .ok_or_else(|| internal("it carries no minor".to_owned()))?;
                let port = parsed
                    .nodes
                    .iter()
                    .find(|node| node.name == self.config.node_name)
                    .and_then(ParsedNode::port)
                    .ok_or_else(|| {
                        internal(format!(
                            "it carries no address for the local node {:?}",
                            self.config.node_name
                        ))
                    })?;
                for (other_id, other) in state.volumes() {
                    if other_id == volume_id {
                        continue;
                    }
                    if other.entry.minor == minor {
                        return Err(internal(format!(
                            "its minor {minor} is claimed by volume {other_id}"
                        )));
                    }
                    if other.entry.port == port {
                        return Err(internal(format!(
                            "its local port {port} is claimed by volume {other_id}"
                        )));
                    }
                    if other.entry.resource_name == resource {
                        return Err(internal(format!(
                            "resource {resource:?} is claimed by volume {other_id}"
                        )));
                    }
                }
                state.observe_minor(minor);
                state.observe_port(port);
                Some((minor, port))
            }
            None => None,
        };
        Ok((size, false, adopted))
    }

    // The create state machine (idempotent replay → policy gate →
    // capacity → backing → bring-up → seeding → verify → persist);
    // splitting it would scatter the crash-window invariants.
    #[allow(clippy::too_many_lines)]
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
        // `validate` already requires a replication policy for this
        // class; the binding is restated so the compiler keeps it.
        let Some(policy) = req.replication.as_ref() else {
            return Err(unsupported(
                "nearline-replicated volumes require a replication policy (engine drbd9)",
            ));
        };
        let mode = requested_replication_mode(policy.mode);
        let payload = canonical_create_payload(req)?;
        let mut state = self.lock_state()?;
        // Idempotent replay: same volume_id + same payload returns the
        // current state through the same verified path as a fresh
        // inspect; a different payload is a typed conflict. The
        // comparison uses the *requested* size, never the
        // extent-rounded effective size.
        if let Some(existing) = state.volume(&req.volume_id) {
            if existing.entry.creation_payload == payload
                && existing.entry.requested_size_bytes == req.size_bytes
            {
                return self.verified_inspect_response(&req.volume_id, existing);
            }
            return Err(ApiError::idempotency_conflict(&req.volume_id));
        }

        // Capacity envelope: the extent-rounded demand plus headroom
        // must fit into the nearline VG's free space (typed
        // NO_SAFE_CAPACITY before the VG fills; thick LVM rounds every
        // allocation up to whole extents).
        let capacity = self.vg_capacity()?;
        let demand = extent_rounded(req.size_bytes, capacity.extent_bytes);
        if demand.saturating_add(VG_HEADROOM_BYTES) > capacity.free_bytes {
            return Err(ApiError::new(
                ApiErrorCode::NoSafeCapacity,
                format!(
                    "no safe capacity: create needs {demand} bytes (extent-rounded from {}), \
                     {} free in {} (headroom {})",
                    req.size_bytes, capacity.free_bytes, self.config.vg_name, VG_HEADROOM_BYTES
                ),
            ));
        }

        let resource = resource_name_for(&req.volume_id);
        let vg_name = self.config.vg_name.clone();
        let mut minor = state.allocate_minor(self.config.minor_min, self.config.minor_max)?;
        let mut port = state.allocate_port(self.config.port_min, self.config.port_max)?;
        let output = self.lvcreate_tagged(&vg_name, &resource, &req.volume_id, req.size_bytes)?;
        let (lv_size, fresh_lv, adopted) = if output.success {
            // Verify against LVM's own report: a successful exit status
            // is not evidence of the requested geometry.
            let size = self.lv_size(&vg_name, &resource)?.ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!("lvcreate reported success but lvs does not list {vg_name}/{resource}"),
                )
            })?;
            if size < req.size_bytes {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "LV size mismatch after lvcreate of {vg_name}/{resource}: requested {} \
                         bytes, LVM reports {size} bytes",
                        req.size_bytes
                    ),
                ));
            }
            (size, true, None)
        } else {
            // Maybe our own half-created orphan from a crash between
            // lvcreate and the state save: reclaim it when the ownership
            // tag proves it, fail typed otherwise.
            self.reclaim_crashed_create(
                &mut state,
                &req.volume_id,
                &vg_name,
                &resource,
                req.size_bytes,
                &output,
            )?
        };
        if let Some((adopted_minor, adopted_port)) = adopted {
            minor = adopted_minor;
            port = adopted_port;
        }

        let entry = VolumeEntry {
            resource_name: resource,
            vg_name,
            lv_name: resource_name_for(&req.volume_id),
            minor,
            port,
            // Refined to the device's own report below.
            size_bytes: lv_size,
            requested_size_bytes: req.size_bytes,
            generation: 1,
            project_id: req.project_id.clone(),
            block_size: req.logical_block_size.unwrap_or(DEFAULT_BLOCK_SIZE),
            replication_mode: mode,
            creation_payload: payload,
            created_at: unix_now(),
        };
        // The resource definition: the local port is this resource's
        // allocated port; the peer port is the peer's FIXED listening
        // port from the configuration (the operator deploys the
        // identical definition on the peer).
        let (peer_ip, peer_port) = split_peer_address(&self.config.peer_address)?;
        let definition = ResourceDefinition {
            resource_name: entry.resource_name.clone(),
            minor: entry.minor,
            protocol: entry.replication_mode,
            local_node: self.config.node_name.clone(),
            local_address: self.config.local_address.clone(),
            local_port: entry.port,
            peer_node: self.config.peer_name.clone(),
            peer_address: peer_ip,
            peer_port,
            disk_path: format!("/dev/{}/{}", entry.vg_name, entry.lv_name),
            shared_secret: read_shared_secret(&self.config.shared_secret_file)?,
        };
        if let Err(error) = definition.write(&self.config.config_dir) {
            if fresh_lv {
                self.teardown_after_failed_create(&entry, fresh_lv);
            }
            return Err(error);
        }
        let status = match self.bring_up_resource(&entry) {
            Ok(status) => status,
            Err(error) => {
                if fresh_lv {
                    self.teardown_after_failed_create(&entry, fresh_lv);
                }
                return Err(error);
            }
        };
        // establish_replica tears a fresh resource down on its own
        // error paths.
        let seeded =
            self.establish_replica(&entry, &status, policy.allow_degraded_create, fresh_lv)?;
        let device = match self.device_size(entry.minor) {
            Ok(device) => device,
            Err(error) => {
                if fresh_lv {
                    self.teardown_after_failed_create(&entry, fresh_lv);
                }
                return Err(error);
            }
        };
        if device < req.size_bytes {
            if fresh_lv {
                self.teardown_after_failed_create(&entry, fresh_lv);
            }
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "the DRBD device for {} reports {device} bytes, below the requested {}; \
                     the peer backing is likely smaller than the request",
                    entry.resource_name, req.size_bytes
                ),
            ));
        }
        let stored = StoredVolume {
            entry: VolumeEntry {
                size_bytes: device,
                ..entry
            },
            runtime: VolumeRuntime {
                state: VolumeLifecycle::Ready,
                attachment: None,
                seeded,
                authority: None,
                fence: None,
            },
        };
        state.insert_volume(req.volume_id.clone(), stored.clone());
        state.save(&self.state_path)?;
        Ok(inspect_response(&req.volume_id, &stored))
    }

    /// Verify a promotion really took: the role re-read from
    /// `drbdsetup status` must be Primary. On contradiction the resource
    /// is demoted back best-effort (no orphaned writer is left behind)
    /// Release the writer's lease after a demotion (P4a plan §4
    /// detach): the witness starts no W7 wait. A lease that is already
    /// retired (`STALE_EPOCH` — authority was superseded earlier) is
    /// released in effect; any other failure is returned typed and
    /// leaves the record in place (the demotion is durable, the
    /// interrupted-detach reconciliation completes the transition, and
    /// the lease lapses at its recorded end, bounded harmlessly by
    /// W1/W7) — never a silent skip.
    ///
    /// # Errors
    /// The typed witness refusal, for the caller to report.
    fn release_after_demote(
        &self,
        volume_id: &VolumeId,
        stored: &StoredVolume,
    ) -> Result<(), ApiError> {
        if let (Some(authority), Some(block)) = (&self.authority, stored.runtime.authority.as_ref())
        {
            if let Err(err) = authority.release(volume_id, block) {
                if !matches!(err, WitnessError::StaleEpoch { .. }) {
                    return Err(witness_error(err));
                }
            }
        }
        Ok(())
    }

    /// and an `INTERNAL` error names the inconsistency.
    fn verify_promotion(&self, entry: &VolumeEntry) -> Result<(), ApiError> {
        let status = self.resource_status(&entry.resource_name)?;
        match status {
            Some(status) if status.role == Role::Primary => Ok(()),
            other => {
                drop(self.run_drbdadm("secondary", &entry.resource_name));
                Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "drbdadm primary reported success but the role of {} is not Primary \
                         (observed {other:?}); the resource was demoted back best-effort",
                        entry.resource_name
                    ),
                ))
            }
        }
    }

    // Attach is a fail-closed promotion sequence (crash replay →
    // lifecycle/zombie/seed gates → promote → verify → rollback);
    // splitting it would scatter the rollback invariants.
    #[allow(clippy::too_many_lines)]
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
        // Contract §3 honesty: the only attachment this prototype grants
        // is a writable single-primary promotion. A read-only
        // (shared-reader) request is rejected typed BEFORE any mutation
        // (no promotion, no state change): a read-only claim over a
        // writable primary would be a fail-open lie, and dual-primary
        // has no code path (rule 17).
        if matches!(req.access_mode, AccessModeRequest::ReadOnly) {
            return Err(unsupported(
                "read-only (shared-reader) attachments require a safe multi-reader contract \
                 the drbd9 prototype has not qualified (dual-primary is forbidden by default); \
                 only read-write single-writer attachments are supported",
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
        if !stored.runtime.seeded {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "volume {volume_id} is not seeded: its replica is not established (an \
                     allow_degraded_create volume seeds once its peer appears and reconcile \
                     observes it fresh)"
                ),
            ));
        }
        let entry = stored.entry.clone();
        // Ownership proofs before the mutation.
        self.require_owned_backing(&entry, volume_id, ApiErrorCode::InvalidState)?;
        self.require_entry_res_file(&entry, volume_id)?;
        // The resource must be up and Secondary. A Primary without a
        // record is a zombie promotion — never adopted, never demoted
        // here (rule 17) — with one P4a exception: a Primary this host
        // holds a recorded lease for is our own promoted volume (an
        // adopted volume awaiting its attachment, or a crash between
        // promote and record save); the acquisition below re-validates
        // the lease before anything is served.
        let status = self.resource_status(&entry.resource_name)?;
        let mut suspended = false;
        match status {
            None => {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "resource {} is down; volume {volume_id} requires reconciliation",
                        entry.resource_name
                    ),
                ));
            }
            Some(status) if status.role == Role::Primary => {
                if self.authority.is_none() || stored.runtime.authority.is_none() {
                    return Err(ApiError::new(
                        ApiErrorCode::InvalidState,
                        format!(
                            "resource {} is Primary without a recorded attachment (a zombie \
                             promotion from a previous life); demote it out of band and restart \
                             the daemon so reconciliation records the state",
                            entry.resource_name
                        ),
                    ));
                }
                // Our own suspended promoted volume (e.g. the startup
                // validation froze it and the witness came back): it
                // resumes only after a fresh lease is proven below.
                suspended = status.suspended.is_some();
            }
            // A Secondary resource — including one an operator suspended
            // by hand — keeps exactly its P3 handling: this provider
            // never suspends a Secondary and never silently undoes an
            // operator suspension.
            Some(_) => {}
        }

        // Writer authority before promotion (P4a plan §4): acquire (or
        // renew) the lease, persist the authority block, and only then
        // promote — a new writer is never admitted without authority.
        // The block is saved BEFORE the promotion so an interrupted
        // attach resumes through the renewal path with a fresh W5
        // deadline instead of replaying a stale grant response. A
        // witness refusal is typed; an unreachable witness is
        // `UNKNOWN_FENCING_AUTHORITY`.
        if let Some(authority) = &self.authority {
            let prior_authority = stored.runtime.authority.clone();
            let block = authority.acquire(volume_id, prior_authority.as_ref())?;
            let acquired = block.clone();
            {
                // The save needs the state exclusively, so the record
                // borrow ends here and the tail re-acquires it.
                let volume = state
                    .volume_mut(volume_id)
                    .ok_or_else(|| not_found(volume_id))?;
                volume.runtime.authority = Some(block);
            }
            if let Err(error) = state.save(&self.state_path) {
                // A failed save leaves no durable record of a FRESH
                // grant: release it best-effort so a retry is not
                // refused with LEASE_HELD against our own orphan lease
                // (the adopt path's discipline). Only the
                // provably-not-writing case releases: a renewed lease
                // still matches the durable record (same epoch,
                // extended end) and self-heals on the next renewal
                // save, and a suspended Primary must never have its
                // lease released from under it — the next grant's W7
                // wait would be waived while the device is only
                // kernel-suspended.
                if prior_authority.is_none() && !suspended {
                    let _ = authority.release(volume_id, &acquired);
                }
                return Err(error);
            }
        }

        let output = self.run_drbdadm("primary", &entry.resource_name)?;
        if !output.success {
            // e.g. the local disk is inconsistent (unseeded) or the peer
            // is Primary (dual-primary refused): no attachment recorded.
            return Err(command_failed("drbdadm primary", &output));
        }
        // A successful exit status is not evidence: verify the role,
        // then the device itself.
        self.verify_promotion(&entry)?;
        // A suspended resource resumes only now that authority is
        // proven (the startup validation left it frozen).
        if suspended {
            self.resume_io(entry.minor)?;
        }
        let device = format!("/dev/drbd{}", entry.minor);
        let device_size = self.device_size(entry.minor)?;
        if device_size == 0 {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "blockdev reports a zero-sized device for {}",
                    entry.resource_name
                ),
            ));
        }

        let record = AttachmentRecord {
            id: req.attachment_id.clone(),
            vm_id: req.vm_id.clone(),
            host_id: req.host_id.clone(),
            generation: 1,
            access_mode: mode,
            device,
        };
        // Re-acquire the record: the authority acquisition above ended
        // the earlier borrow when it saved the block.
        let stored = state
            .volume_mut(volume_id)
            .ok_or_else(|| not_found(volume_id))?;
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
        // type-mandatory attestation: authority is never released
        // without one, though the provider cannot verify the claim
        // itself (Volume API v2 section 3).
        let entry = stored.entry.clone();
        let status = self.resource_status(&entry.resource_name)?;
        let Some(status) = status else {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "resource {} is down while the attachment is still recorded; restart the \
                     daemon so reconciliation clears the record",
                    entry.resource_name
                ),
            ));
        };
        match status.role {
            Role::Primary => {}
            Role::Secondary => {
                // The demotion already succeeded but the state save did
                // not (an interrupted detach). Reconcile owns that
                // transition; this call refuses rather than replaying a
                // demotion that already happened.
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "resource {} is already Secondary while the attachment is still \
                         recorded (an interrupted detach); restart the daemon so \
                         reconciliation completes it",
                        entry.resource_name
                    ),
                ));
            }
        }

        let output = self.run_drbdadm("secondary", &entry.resource_name)?;
        if !output.success {
            // The kernel refuses the demotion while the device is open:
            // the honest, enforcement-point-level single-writer release.
            // Never forced (rule 17).
            if is_device_busy(&output.stderr) {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "drbdadm secondary refused to demote {}: the device {} is still open \
                         (busy); stop the consuming VM or drain its I/O first — demotion is \
                         never forced",
                        entry.resource_name, record.device
                    ),
                ));
            }
            return Err(command_failed("drbdadm secondary", &output));
        }
        // A successful exit status is not evidence: verify the demotion.
        let status = self.resource_status(&entry.resource_name)?;
        match status {
            Some(status) if status.role == Role::Secondary => {}
            Some(_) => {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "drbdadm secondary reported success but {} is still Primary",
                        entry.resource_name
                    ),
                ));
            }
            None => {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "drbdadm secondary reported success but {} is no longer up",
                        entry.resource_name
                    ),
                ));
            }
        }

        // Release the writer's lease after the durable demotion (the
        // helper reports every failure that is not already-released).
        self.release_after_demote(volume_id, stored)?;

        stored.runtime.attachment = None;
        stored.runtime.authority = None;
        if stored.runtime.state == VolumeLifecycle::Attached {
            stored.runtime.state = VolumeLifecycle::Ready;
        }
        stored.entry.generation += 1;
        let response = inspect_response(volume_id, stored);
        state.save(&self.state_path)?;
        Ok(response)
    }

    // Grow is a fail-closed sequence (ownership → capacity → extend →
    // verify → resize → verify → honest boundary persist); each step's
    // failure mode is distinct and ordered.
    //
    // `drbdadm resize` is the standard supported path in BOTH roles:
    // drbdadm's resize carries no role gate and the kernel executes a
    // cluster-wide size transaction (drbd_nl.c), so an ATTACHED grow
    // runs resize while the resource is Primary — the ordinary,
    // guest-visible online-grow case whose notification status below
    // honestly stays RetryRequired — and a detached grow runs the same
    // resize over a Secondary resource. The only role refusal is the
    // zombie gate above (Primary without a recorded attachment). When
    // the peer backing was not grown, the device stays below the
    // request: that honest boundary is persisted before the typed
    // failure so a retry continues from observed reality.
    #[allow(clippy::too_many_lines)]
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
        let (entry, current_size, has_attachment) = {
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
            // CURRENT size under the state lock (Volume API v2 4A).
            req.validate(stored.entry.size_bytes)?;
            (
                stored.entry.clone(),
                stored.entry.size_bytes,
                stored.runtime.attachment.is_some(),
            )
        };

        // Ownership proofs before the mutation. A resource found Primary
        // without an attachment record is a zombie promotion — never
        // resized under it.
        self.require_owned_backing(&entry, volume_id, ApiErrorCode::Internal)?;
        self.require_entry_res_file(&entry, volume_id)?;
        let status = self.resource_status(&entry.resource_name)?;
        let Some(status) = status else {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "resource {} is down; volume {volume_id} requires reconciliation",
                    entry.resource_name
                ),
            ));
        };
        if status.role == Role::Primary && !has_attachment {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "resource {} is Primary without a recorded attachment (a zombie \
                     promotion); grow is refused",
                    entry.resource_name
                ),
            ));
        }

        // Capacity envelope for the extent-rounded growth delta.
        let capacity = self.vg_capacity()?;
        let delta = req.new_size_bytes.saturating_sub(current_size);
        let demand = extent_rounded(delta, capacity.extent_bytes);
        // Only a grow that actually needs new bytes consumes capacity: a
        // target already met must not be refused on a near-full VG.
        if demand > 0 && demand.saturating_add(VG_HEADROOM_BYTES) > capacity.free_bytes {
            return Err(ApiError::new(
                ApiErrorCode::NoSafeCapacity,
                format!(
                    "grow needs {demand} more bytes (extent-rounded from {delta}), {} free in \
                     {} (headroom {})",
                    capacity.free_bytes, entry.vg_name, VG_HEADROOM_BYTES
                ),
            ));
        }

        // A device already at or beyond the target (an unrecorded grow
        // met it) needs no resize: the target is met, only the record
        // catches up. `backing_resized` stays honest: false.
        let actual_device = self.device_size(entry.minor)?;
        if actual_device >= req.new_size_bytes {
            let stored = state
                .volume_mut(volume_id)
                .ok_or_else(|| not_found(volume_id))?;
            stored.entry.size_bytes = actual_device;
            stored.entry.generation += 1;
            state.save(&self.state_path)?;
            return Ok(GrowVolumeResponse {
                backing_resized: false,
                guest_notification_status: notification_status(has_attachment),
                effective_size_bytes: actual_device,
            });
        }

        let output = self.runner.run(
            "lvextend",
            &[
                "--yes",
                "-L",
                &format!("{}B", req.new_size_bytes),
                &format!("{}/{}", entry.vg_name, entry.lv_name),
            ],
        )?;
        if !output.success {
            return Err(command_failed("lvextend", &output));
        }
        // Verify the LV geometry from LVM's own report: thick LVM rounds
        // up to whole extents, so anything at or above the request is a
        // success.
        let actual_lv = self
            .lv_size(&entry.vg_name, &entry.lv_name)?
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "lvs no longer reports {}/{} after lvextend",
                        entry.vg_name, entry.lv_name
                    ),
                )
            })?;
        if actual_lv < req.new_size_bytes {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "LV size mismatch after lvextend of {}/{}: requested {} bytes, LVM reports \
                     {actual_lv} bytes",
                    entry.vg_name, entry.lv_name, req.new_size_bytes
                ),
            ));
        }
        let output = self.run_drbdadm("resize", &entry.resource_name)?;
        if !output.success {
            return Err(command_failed("drbdadm resize", &output));
        }
        // The effective size proof comes from the device itself. A
        // device below the request is the honest boundary of a peer
        // backing that was not grown: the observed size and the
        // generation bump are persisted BEFORE the typed failure, so a
        // retry continues from observed reality instead of wedging.
        let device = self.device_size(entry.minor)?;
        if device < req.new_size_bytes {
            let stored = state
                .volume_mut(volume_id)
                .ok_or_else(|| not_found(volume_id))?;
            stored.entry.size_bytes = device;
            stored.entry.generation += 1;
            state.save(&self.state_path)?;
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "grow stopped at an honest boundary: the DRBD device for {} reports \
                     {device} bytes, below the requested {}; the peer backing was likely not \
                     grown — grow the peer backing first, then retry",
                    entry.resource_name, req.new_size_bytes
                ),
            ));
        }

        let stored = state
            .volume_mut(volume_id)
            .ok_or_else(|| not_found(volume_id))?;
        stored.entry.size_bytes = device;
        stored.entry.generation += 1;
        state.save(&self.state_path)?;
        Ok(GrowVolumeResponse {
            backing_resized: true,
            guest_notification_status: notification_status(has_attachment),
            effective_size_bytes: device,
        })
    }

    // Delete is a fail-closed teardown sequence (lifecycle gates →
    // verified down → res-file ownership cases → backing honesty);
    // splitting it would scatter the never-demote/never-touch-foreign
    // invariants.
    #[allow(clippy::too_many_lines)]
    fn delete_volume_inner(
        &self,
        volume_id: &VolumeId,
        req: &DeleteVolumeRequest,
    ) -> Result<(), ApiError> {
        validate_api_version(&req.api_version)?;
        let mut state = self.lock_state()?;
        let (entry, volume_state, block) = {
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
                VolumeLifecycle::Ready | VolumeLifecycle::Degraded | VolumeLifecycle::Failed => {}
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
                // DRBD/LVM discard does not guarantee block-level
                // zeroing: fail-closed policy negotiation instead of a
                // false claim (Volume API v2 section 4D).
                ErasurePolicy::ZeroDiscard => {
                    return Err(unsupported(
                        "the drbd9 prototype does not implement verified block zeroing; use \
                         Retain (resource down + file removed + LV retained) or a provider \
                         with a proven zeroing path",
                    ));
                }
                ErasurePolicy::Cryptographic => {
                    return Err(unsupported(
                        "cryptographic erasure requires a separate evidence gate",
                    ));
                }
                ErasurePolicy::Retain => {}
            }
            (
                stored.entry.clone(),
                stored.runtime.state,
                stored.runtime.authority.clone(),
            )
        };

        // A Primary resource is never force-demoted by delete (rule 17)
        // — and this check comes BEFORE the lease release: a
        // self-release waives the next grant's W7 wait, so it is
        // earned only once the resource is verifiably not writing
        // (the same ordering every authority-clearing path follows).
        let status = self.resource_status(&entry.resource_name)?;
        if let Some(status) = &status {
            if status.role == Role::Primary {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "resource {} is Primary; demote (detach) before delete — demotion is \
                         never forced",
                        entry.resource_name
                    ),
                ));
            }
        }
        // Best-effort lease release before the resource is torn down
        // (P4a): a failure here is deliberately tolerated — delete must
        // not be wedged on witness availability — and is safe now: the
        // volume is detached and verifiably not Primary (the check
        // above), the lease lapses at its recorded end if the release
        // fails (bounded by W1/W7), and the witness-side record of a
        // deleted lineage is retained as lineage evidence (witness
        // garbage collection is out of P4a scope).
        if let (Some(authority), Some(block)) = (&self.authority, block.as_ref()) {
            let _ = authority.release(volume_id, block);
        }
        // Bring the resource down through our own scoped file, then
        // remove the file — but only when it is verifiably ours. A
        // mismatched file is foreign state: never adopted, never
        // destroyed (the remedy is restoring the file or removing the
        // volume out of band).
        let res_path = res_file_path(&self.config.config_dir, &entry.resource_name);
        match self.check_res_file(&entry) {
            ResFileCheck::Matches => {
                if status.is_some() {
                    let output = self.run_drbdadm("down", &entry.resource_name)?;
                    if !output.success {
                        return Err(command_failed("drbdadm down", &output));
                    }
                    // A successful exit status is not evidence.
                    if self.resource_status(&entry.resource_name)?.is_some() {
                        return Err(ApiError::new(
                            ApiErrorCode::Internal,
                            format!(
                                "drbdadm down reported success but {} is still up",
                                entry.resource_name
                            ),
                        ));
                    }
                }
                fs::remove_file(&res_path).map_err(|e| {
                    ApiError::new(
                        ApiErrorCode::Internal,
                        format!(
                            "failed to remove the resource file {}: {e}",
                            res_path.display()
                        ),
                    )
                })?;
            }
            ResFileCheck::Missing => {
                if status.is_some() {
                    return Err(ApiError::new(
                        ApiErrorCode::Internal,
                        format!(
                            "resource {} is up but its resource file is missing; volvisor \
                             cannot scope a `down` without it — bring the resource down out \
                             of band and retry",
                            entry.resource_name
                        ),
                    ));
                }
                // Down and no file: nothing to bring down or remove.
            }
            ResFileCheck::Mismatch(detail) => {
                return Err(ApiError::new(
                    ApiErrorCode::ForeignDeviceState,
                    format!(
                        "resource file for {} no longer matches volvisor's record ({detail}); \
                         foreign state is never adopted — restoring the file to the recorded \
                         definition makes an in-band delete possible, or remove the volume's \
                         state entry out of band once you have confirmed it is truly foreign",
                        entry.resource_name
                    ),
                ));
            }
        }

        // The backing LV is RETAINED (recoverable erasure, mirroring the
        // LVM provider's Retain) — but its ownership must still be
        // provably ours for the state entry to be dropped: a Failed
        // volume whose LV already vanished deletes cleanly; a non-Failed
        // volume without its LV fails loudly; a mismatched tag is
        // foreign, never adopted.
        let wanted = format!("{}/{}", entry.vg_name, entry.lv_name);
        match self.verify_backing(&entry, volume_id)? {
            Backing::Owned { .. } => {}
            Backing::Absent if volume_state == VolumeLifecycle::Failed => {}
            Backing::Absent => {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "backing LV {wanted} unexpectedly absent for volume {volume_id} in \
                         state {volume_state:?}; if this follows an out-of-band removal, \
                         restart the daemon so reconciliation records the loss"
                    ),
                ));
            }
            Backing::Mismatch => {
                return Err(ApiError::new(
                    ApiErrorCode::ForeignDeviceState,
                    format!(
                        "backing LV {wanted} does not carry the ownership tag of volume \
                         {volume_id}; a foreign LV is never destroyed by volvisor — restoring \
                         the LV's `{OWNER_TAG}` tag to `{volume_id}` makes an in-band delete \
                         possible, or remove the LV and the volume's state entry manually (out \
                         of band)"
                    ),
                ));
            }
        }
        state.remove_volume(volume_id);
        state.save(&self.state_path)?;
        Ok(())
    }
}

#[async_trait]
impl VolumeProvider for DrbdProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet::of([
            Capability::Create,
            Capability::Attach,
            Capability::Resize,
            // The nearline class marker: this provider establishes a
            // remote replica. Mobility surfaces stay unadvertised
            // (unproven; the fencing/handoff phase owns them).
            Capability::Replicate,
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

/// The access mode granted for a requested mode.
///
/// `ReadOnly` never reaches this mapping on the attach path (it is
/// rejected typed in [`DrbdProvider::attach_volume_inner`] before any
/// mutation); the mapping is kept total so a recorded mode always
/// round-trips.
fn requested_mode(mode: AccessModeRequest) -> AccessMode {
    match mode {
        AccessModeRequest::SingleWriter => AccessMode::SingleWriter,
        AccessModeRequest::ReadOnly => AccessMode::ReadOnly,
    }
}

/// The replication protocol for a requested mode.
fn requested_replication_mode(mode: ReplicationModeRequest) -> ReplicationMode {
    match mode {
        ReplicationModeRequest::Async => ReplicationMode::A,
        ReplicationModeRequest::SemiSync => ReplicationMode::B,
        ReplicationModeRequest::Sync => ReplicationMode::C,
    }
}

/// The honest guest-notification status of a grow: no VMM integration
/// exists, so an attached frontend still needs a (retried) notification
/// and a detached volume has nobody to notify. Never `Notified`.
fn notification_status(has_attachment: bool) -> GrowGuestNotification {
    if has_attachment {
        GrowGuestNotification::RetryRequired
    } else {
        GrowGuestNotification::NotApplicable
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
    // Nearline volumes require a replication policy; this provider is
    // the drbd9 engine with exactly one peer replica (the P3
    // deployment shape — two nodes).
    let Some(replication) = req.replication.as_ref() else {
        return Err(unsupported(
            "nearline-replicated volumes require a replication policy (engine drbd9, one \
             remote replica)",
        ));
    };
    match replication.engine.as_deref() {
        None | Some("drbd9") => {}
        Some(engine) => {
            return Err(unsupported(format!(
                "replication engine {engine:?}: this provider implements drbd9 only"
            )));
        }
    }
    if replication.remote_replicas != 1 {
        return Err(unsupported(format!(
            "remote_replicas {}: the drbd9 prototype serves exactly one peer replica (two-node \
             deployment)",
            replication.remote_replicas
        )));
    }
    // DRBD backing LVs are thick: LVM allocates the whole extent-rounded
    // size up front.
    if req.provisioning == Some(Provisioning::Thin) {
        return Err(unsupported(
            "thin provisioning: DRBD backing LVs here are thick (extent-rounded at create)",
        ));
    }
    if req.encryption.is_some() {
        return Err(unsupported("encryption: this provider implements none"));
    }
    if req.migration_policy.is_some() {
        return Err(unsupported(
            "migration policy: no VMM/storage handoff semantics exist in this prototype \
             (dual-primary is forbidden by default)",
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
                    "local mirror protection is not implemented: the P3 DRBD baseline puts no \
                     mirror legs under the backing LV",
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
                        "cannot honor preferred_host_id {preferred}: this provider serves the \
                         single host {LOCAL_HOST_ID:?}"
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

/// The space `size` bytes effectively occupy on a VG with `extent`-byte
/// physical extents: `size` rounded up to a whole multiple of `extent`.
///
/// A zero or absent extent is treated as "no rounding" (guarded, never a
/// division by zero). Mirrors `volvisor-lvm`'s helper (deliberately
/// duplicated, not factored out, so each crate stays independently
/// reviewable).
fn extent_rounded(size: u64, extent: u64) -> u64 {
    if extent == 0 {
        return size;
    }
    size.div_ceil(extent) * extent
}

/// The maximum length of the sanitized identity segment of a resource
/// (and LV) name.
///
/// The cap exists for a DRBD-specific reason: `drbdsetup status`
/// renders through the column-oriented `wrap_printf`, which wraps any
/// line past 80 columns when piped (non-tty). The resource line is
/// `<name> role:<Secondary>` — 15 characters past the name — so a
/// name longer than 65 would push `role:` onto a wrapped continuation
/// line the status parser (correctly) rejects. Capping the sanitized
/// segment at 50 keeps the full name (`vol-` + segment + `-` + 8 hex)
/// at 63 characters, 2 under that wrap budget. This is deliberately
/// NOT the LVM/Ceph providers' 113-character budget: those backends
/// have no status-line width constraint, DRBD does.
///
/// The remaining wrap exposure: resource-line qualifiers that appear
/// only while the resource is degraded — the operator's out-of-band
/// `drbdsetup suspend-io` (`suspended:user`), and, importantly NOT an
/// operator action, the kernel's automatic no-data-access suspension
/// (`suspended:no-data`) after local data-access loss, i.e. the very
/// disk-failure observation path. ` name(48..63) + role:Secondary +
/// " suspended:no-data"` exceeds 80 columns, so on a genuine
/// failed-disk event with a longer name the qualifier wraps onto a
/// continuation line and the status parse fails closed (`INTERNAL`,
/// the volume is left untouched and reported unverifiable) instead of
/// reporting the documented `Unhealthy` verdict. That is safe (never
/// mis-parsed) but lossy for names >= 48; a future parser that merges
/// wrap continuations would close it.
const RESOURCE_NAME_SANITIZED_MAX_CHARS: usize = 50;

/// The maximum length of a node name (`on <host>` in the generated
/// resource file, `node_name`/`peer_name` in the provider config).
///
/// Same `wrap_printf` budget as the resource-name cap
/// (`RESOURCE_NAME_SANITIZED_MAX_CHARS`, private to this module),
/// applied to the connection
/// line: `  <peer> connection:<State>` — 2 indent + name + up to 26
/// characters for the longest real connection state
/// (`connection:WFReportParams`) — wraps past 80 columns when the
/// name exceeds 52. Both the local and the peer node name are bound:
/// each appears as the peer-line prefix on the *other* node's status
/// output.
pub const NODE_NAME_MAX_CHARS: usize = 52;

/// The DRBD resource (and backing LV) name for a volume identity.
///
/// This mirrors the LVM provider's `lv_name_for` scheme (the two are
/// deliberately duplicated, not factored out, so each crate stays
/// independently reviewable): `.` and `:` are replaced with `-` (the ID
/// charset otherwise consists of `[A-Za-z0-9_.:-]`), the sanitized
/// segment is truncated to 50 characters
/// (`RESOURCE_NAME_SANITIZED_MAX_CHARS`, private to this module — the
/// DRBD status-line wrap budget, see there), and the first 8 hex
/// characters of SHA-256 over the **full** volume id are appended.
/// Sanitization alone is **not injective** (`vol.a`, `vol:a` and `vol-a`
/// all map to `vol-a`), so uniqueness rests on the 32-bit hash suffix
/// (collision probability <= 2^-32 per distinct pair). The `vol-` prefix
/// guarantees the name is never dash-leading, and the full name is at
/// most `4 + 50 + 1 + 8 = 63` characters. Ownership is *additionally*
/// proven by the `volvisor.owner` LV tag before every mutation — the
/// name alone is never the ownership proof.
#[must_use]
pub fn resource_name_for(volume_id: &VolumeId) -> String {
    let sanitized: String = volume_id
        .as_str()
        .chars()
        .map(|c| if matches!(c, '.' | ':') { '-' } else { c })
        .take(RESOURCE_NAME_SANITIZED_MAX_CHARS)
        .collect();
    let digest = Sha256::digest(volume_id.as_str().as_bytes());
    format!("vol-{sanitized}-{}", hex_prefix(&digest, 4))
}

/// Build a [`HostId`] from a node name (the endpoint identities use
/// the resource-definition node names, which `verify_startup` proves
/// name real hosts).
fn host_id_of(node: &str) -> Result<HostId, ApiError> {
    HostId::new(node).map_err(|error| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("node name {node:?} is not a valid host identity: {error}"),
        )
    })
}

/// The endpoint backing identity recorded at registration and compared
/// verbatim by the adopt flow (P4a plan §5): derived from the resource
/// definition alone — host, resource, backing disk path — so the
/// surviving host can reconstruct it after losing the primary host
/// (and its state) entirely. The same definition file is deployed on
/// both ends (the P3 operator model).
fn endpoint_backing_identity(host: &str, resource: &str, disk: &str) -> String {
    format!("host={host};resource={resource};disk={disk}")
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

/// The ownership outcome for a state entry's backing LV from `lvs` rows.
fn backing_from_rows(
    rows: &[LvRow],
    entry: &VolumeEntry,
    volume_id: &VolumeId,
) -> Result<Backing, ApiError> {
    let wanted = format!("{}/{}", entry.vg_name, entry.lv_name);
    let Some(row) = rows
        .iter()
        .find(|row| row.vg_slash_lv().as_deref() == Some(wanted.as_str()))
    else {
        return Ok(Backing::Absent);
    };
    let size = row.size_bytes().ok_or_else(|| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("lvs did not report a size for {wanted}"),
        )
    })?;
    if row.tag(OWNER_TAG).as_deref() == Some(volume_id.as_str()) {
        Ok(Backing::Owned { size_bytes: size })
    } else {
        Ok(Backing::Mismatch)
    }
}

/// What the peer's observed state means for seeding a fresh local disk.
fn peer_freshness(status: &ResourceStatus) -> PeerFreshness {
    if !status.connected {
        return PeerFreshness::Absent;
    }
    if status.peer_role == Some(Role::Primary) {
        return PeerFreshness::Foreign;
    }
    match status.peer_disk {
        Some(DiskState::Inconsistent) => PeerFreshness::Fresh,
        Some(DiskState::UpToDate | DiskState::Consistent | DiskState::Outdated) => {
            PeerFreshness::Foreign
        }
        // Diskless/DUnknown peers — and a connected peer whose
        // peer-disk line is not printed at all — have nothing to seed
        // against yet. Real drbdsetup omits the entire peer-device
        // block while the peer's device state is un-exchanged
        // (replication Off + DUnknown), a transient during connection
        // establishment: `None` here means "not yet observable", never
        // "peer holds data", so it must wait (`Absent`), not destroy
        // (`Foreign` would tear a fresh resource down on that false
        // verdict). An unrecognized spelling is likewise never treated
        // as fresh (fail-closed against overwriting data it might
        // describe).
        Some(DiskState::Diskless | DiskState::DUnknown) | None => PeerFreshness::Absent,
        // Failed/unknown-spelling peer disks conservatively count as
        // data-holding: never seeded over.
        Some(DiskState::Failed | DiskState::Other(_)) => PeerFreshness::Foreign,
    }
}

/// The observed volume health from a resource status (plan §6).
///
/// `Healthy` only when connected with both disks `UpToDate` and no
/// resync in progress; a local disk failure is `Unhealthy`; resync,
/// peer inconsistency or a lost connection is `Degraded`; an
/// unrecognized local disk state is an honest `Unknown`, never a guess.
fn observed_health(status: &ResourceStatus) -> Health {
    // `force-io-failures:yes` is the disk-failure / fencing emulation
    // path: local I/O is being failed on purpose, which is the
    // unhealthiest observable state short of a parse failure.
    if status.force_io_failures == Some(true) {
        return Health::Unhealthy;
    }
    let base = match &status.local_disk {
        DiskState::Failed | DiskState::Diskless => Health::Unhealthy,
        DiskState::DUnknown | DiskState::Other(_) => Health::Unknown,
        DiskState::UpToDate => {
            if status.connected
                && status.peer_disk == Some(DiskState::UpToDate)
                && status.replication.is_none()
            {
                Health::Healthy
            } else {
                Health::Degraded
            }
        }
        DiskState::Inconsistent | DiskState::Outdated | DiskState::Consistent => Health::Degraded,
    };
    // A suspended resource (operator `drbdsetup suspend-io`, or the
    // kernel's no-data-access suspension after local data-access
    // loss) freezes I/O: no data is lost, but a Healthy verdict would
    // claim serving capability the resource does not have. Degraded
    // is the honest ceiling.
    if status.suspended.is_some() && base == Health::Healthy {
        return Health::Degraded;
    }
    base
}

/// The observed backend (local disk) health from a resource status.
fn backend_health_of(status: &ResourceStatus) -> Health {
    match &status.local_disk {
        DiskState::UpToDate => Health::Healthy,
        DiskState::Failed | DiskState::Diskless => Health::Unhealthy,
        DiskState::Outdated | DiskState::Consistent => Health::Degraded,
        DiskState::Inconsistent | DiskState::DUnknown | DiskState::Other(_) => Health::Unknown,
    }
}

/// The remote protection axis from a resource status plus the
/// replication protocol read back from the resource's own definition
/// file (an observed fact, not the recorded create-time intent).
///
/// The axis states only that a remote replica is **currently
/// established and observed `UpToDate`** outside resync; the protocol
/// read back from the resource selects which *class* of remote
/// protection that replica provides (AGENTS rule 16):
///
/// - Protocol A → [`RemoteProtectionAxis::AsynchronousPeer`]:
///   possible-RPO; the peer's arrival is never acknowledged, so the
///   axis is never a durability claim.
/// - Protocol B → `AsynchronousPeer` as well: semi-synchronous B
///   acknowledges write arrival in the peer's **memory** only — the
///   peer can still lose acknowledged writes on peer loss, so B is
///   deliberately NOT the synchronous axis.
/// - Protocol C → [`RemoteProtectionAxis::SynchronousPeer`]: writes
///   are acknowledged after both durable media completed. RPO=0 only
///   under the protocol's own conditions — both backings durable, the
///   connection in the correct state — never a simultaneous-failure
///   claim.
///
/// Everything else (no connection, peer not `UpToDate`, resync in
/// progress, or a protocol that could not be read back) reports `None`:
/// remote protection not established.
fn remote_axis_of(
    status: &ResourceStatus,
    protocol: Option<ReplicationMode>,
) -> RemoteProtectionAxis {
    let Some(protocol) = protocol else {
        return RemoteProtectionAxis::None;
    };
    if status.connected
        && status.peer_disk == Some(DiskState::UpToDate)
        && status.replication.is_none()
    {
        match protocol {
            ReplicationMode::A | ReplicationMode::B => RemoteProtectionAxis::AsynchronousPeer,
            ReplicationMode::C => RemoteProtectionAxis::SynchronousPeer,
        }
    } else {
        RemoteProtectionAxis::None
    }
}

/// Whether a `drbdadm secondary` stderr means "the device is still
/// open" (the kernel's refusal to demote an in-use source device).
///
/// Real drbd-utils surfaces the kernel's `SS_DEVICE_IN_USE` (`-EBUSY`)
/// as `State change failed: (-12) Device is held open by someone`,
/// followed by the kernel's opener info (`open_cnt:` and the
/// `<dev> opened by <proc> (pid N) ...` list); those are the PRIMARY
/// patterns. Other drbd-utils generations spell the same refusal
/// "Device or resource busy" / "device is in use", so those stay as
/// fallbacks. The match is deliberately case-insensitive and
/// substring-based. Any OTHER failure is an honest `INTERNAL`, never a
/// mistyped busy refusal.
fn is_device_busy(stderr: &str) -> bool {
    let lowered = stderr.to_lowercase();
    lowered.contains("held open")
        || lowered.contains("open_cnt")
        || lowered.contains("opened by")
        // Fallback spellings from other drbd-utils versions.
        || lowered.contains("busy")
        || lowered.contains("in use")
}

/// Build the contract-shaped inspect response from stored state.
///
/// The health axes start `Unknown` and the remote protection axis starts
/// `None`: observed facts are filled in only by
/// [`DrbdProvider::verified_inspect_response`], never from configured
/// intent. `evidence_status` is `PrototypeOnly` (AGENTS rule 12).
fn inspect_response(volume_id: &VolumeId, stored: &StoredVolume) -> InspectVolumeResponse {
    let attachment = stored.runtime.attachment.as_ref();
    InspectVolumeResponse {
        volume_id: volume_id.clone(),
        backend_class: VolumeClass::NearlineReplicated,
        project_id: stored.entry.project_id.clone(),
        generation: stored.entry.generation,
        state: stored.runtime.state,
        // Thick backing: provisioning equals allocation, both at the
        // effective size the DRBD device actually serves (extent-rounded
        // at create; possibly capped by a smaller peer backing).
        provisioned_bytes: stored.entry.size_bytes,
        allocated_bytes: stored.entry.size_bytes,
        effective_protection: EffectiveProtection {
            // No local mirror legs exist under a DRBD backing LV here.
            local: LocalProtectionAxis::None,
            remote: RemoteProtectionAxis::None,
        },
        // Two hosts (local + the operator-provisioned peer): the failure
        // domain of the placement is the host.
        failure_domain: FailureDomain::Host,
        health: Health::Unknown,
        attachment_ids: attachment.map(|a| vec![a.id.clone()]).unwrap_or_default(),
        current_writer: attachment
            .filter(|a| a.access_mode == AccessMode::SingleWriter)
            .map(|a| a.id.clone()),
        backend_health: Health::Unknown,
        evidence_status: EvidenceStatus::PrototypeOnly,
        // Filled by `verified_inspect_response` from witness facts for
        // witness-managed volumes; `None` (pre-authority/P3-era) is the
        // honest value — never a fabricated authority claim.
        authority: None,
    }
}

/// Build the attach response for a recorded attachment.
fn attach_response(record: &AttachmentRecord, entry: &VolumeEntry) -> AttachVolumeResponse {
    AttachVolumeResponse {
        attachment_id: record.id.clone(),
        attachment_generation: record.generation,
        volume_generation: entry.generation,
        // Host-scoped, ephemeral backend handle (the DRBD device);
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
    use volvisor_types::HostId;
    use volvisor_types::OperationId;
    use volvisor_types::request::{Placement, ReplicationPolicyRequest};

    fn volume_id(raw: &str) -> VolumeId {
        VolumeId::new(raw).expect("valid fixture volume id")
    }

    #[test]
    fn resource_names_are_injective_across_the_id_charset() {
        // `vol.a`, `vol:a` and `vol-a` all sanitize to `vol-a`; the hash
        // suffix keeps them distinct (the scheme mirrors the LVM one).
        let names = [
            resource_name_for(&volume_id("vol.a")),
            resource_name_for(&volume_id("vol:a")),
            resource_name_for(&volume_id("vol-a")),
        ];
        assert_ne!(names[0], names[1]);
        assert_ne!(names[0], names[2]);
        assert_ne!(names[1], names[2]);
        for name in &names {
            assert!(name.starts_with("vol-"), "{name}: stable prefix");
            assert!(!name.starts_with('-'), "{name}: never dash-leading");
        }
        // Deterministic for the same identity.
        assert_eq!(names[0], resource_name_for(&volume_id("vol.a")));
    }

    #[test]
    fn resource_names_for_max_length_ids_fit_the_status_wrap_budget() {
        let long_id = volume_id(&"v".repeat(128));
        let name = resource_name_for(&long_id);
        // 63 = `vol-` + 50 sanitized + `-` + 8 hex; the resource line
        // `name role:Secondary` must stay within wrap_printf's 80-column
        // line budget on piped `drbdsetup status` output (63 + 15 <= 80).
        assert!(name.chars().count() <= 63, "{name} is too long");
        assert!(name.starts_with("vol-"));
        assert!(!name.starts_with('-'));

        // Two distinct 128-char ids sharing a 50-char sanitized prefix
        // still produce distinct names: uniqueness rests on the hash
        // suffix computed over the FULL volume id.
        let shared_prefix = "p".repeat(50);
        let first = resource_name_for(&volume_id(&format!("{shared_prefix}{}", "a".repeat(78))));
        let second = resource_name_for(&volume_id(&format!("{shared_prefix}{}", "b".repeat(78))));
        assert_ne!(first, second);
        let (first_prefix, _) = first.rsplit_once('-').expect("hash suffix delimited");
        let (second_prefix, _) = second.rsplit_once('-').expect("hash suffix delimited");
        assert_eq!(
            first_prefix, second_prefix,
            "the sanitized segments are identical after truncation"
        );
    }

    fn base_config() -> DrbdProviderConfig {
        DrbdProviderConfig {
            vg_name: "vgdrbd".to_owned(),
            config_dir: PathBuf::from("/etc/drbd.d"),
            node_name: "node-a".to_owned(),
            local_address: "10.0.0.1".to_owned(),
            peer_name: "node-b".to_owned(),
            peer_address: "10.0.0.2:7800".to_owned(),
            shared_secret_file: PathBuf::from("/etc/volvisor/peer.secret"),
            port_min: 7900,
            port_max: 7999,
            minor_min: 10,
            minor_max: 99,
            proc_root: PathBuf::from("/proc"),
        }
    }

    #[test]
    fn config_validation_rejects_malformed_shapes() {
        let base = base_config();
        assert!(base.validate().is_ok());

        let mut config = base.clone();
        config.vg_name = "  ".to_owned();
        assert_eq!(
            config.validate().unwrap_err().code,
            ApiErrorCode::InvalidRequest
        );

        let mut config = base.clone();
        config.node_name = String::new();
        assert!(config.validate().is_err());

        // Node names beyond the drbdsetup status connection-line wrap
        // budget are rejected at validation time, not discovered as
        // status parse failures at runtime.
        let mut config = base.clone();
        config.node_name = "n".repeat(NODE_NAME_MAX_CHARS + 1);
        assert!(config.validate().is_err());

        let mut config = base.clone();
        config.local_address = "10.0.0".to_owned();
        assert!(config.validate().is_err());

        let mut config = base.clone();
        config.local_address = "node-a".to_owned();
        assert!(config.validate().is_err());

        let mut config = base.clone();
        config.peer_name = String::new();
        assert!(config.validate().is_err());

        let mut config = base.clone();
        config.peer_name = "p".repeat(NODE_NAME_MAX_CHARS + 1);
        assert!(config.validate().is_err());

        let mut config = base.clone();
        config.peer_address = "10.0.0.2".to_owned();
        assert!(config.validate().is_err());

        let mut config = base.clone();
        config.peer_address = "10.0.0.2:0".to_owned();
        assert!(config.validate().is_err());

        let mut config = base.clone();
        config.peer_address = "10.0.0.2:70000".to_owned();
        assert!(config.validate().is_err());

        let mut config = base.clone();
        config.shared_secret_file = PathBuf::new();
        assert!(config.validate().is_err());

        let mut config = base.clone();
        config.port_min = 7999;
        config.port_max = 7900;
        assert!(config.validate().is_err());

        let mut config = base;
        config.minor_min = 100;
        config.minor_max = 99;
        assert!(config.validate().is_err());
    }

    #[test]
    fn split_peer_address_accepts_only_ipv4_port_shapes() {
        assert_eq!(
            split_peer_address("10.0.0.2:7800").expect("valid"),
            ("10.0.0.2".to_owned(), 7800)
        );
        for malformed in [
            "10.0.0.2",
            "10.0.0:7800",
            "node-b:7800",
            "10.0.0.2:abc",
            "10.0.0.2:0",
            "10.0.0.2:65536",
            "",
        ] {
            assert!(
                split_peer_address(malformed).is_err(),
                "{malformed:?} must be rejected"
            );
        }
    }

    #[test]
    fn capabilities_advertise_the_drbd_profile() {
        let provider = DrbdProvider {
            runner: Arc::new(volvisor_provider::FakeRunner::with_queue(Vec::new())),
            config: base_config(),
            state_path: std::env::temp_dir().join("unused-drbd-state.json"),
            state: Mutex::new(DrbdState::default()),
            last_reconcile: Mutex::new(None),
            authority: None,
        };
        let capabilities = provider.capabilities();
        assert!(capabilities.contains(Capability::Create));
        assert!(capabilities.contains(Capability::Attach));
        assert!(capabilities.contains(Capability::Resize));
        assert!(capabilities.contains(Capability::Replicate));
        // Unproven mobility surfaces stay unadvertised.
        assert!(!capabilities.contains(Capability::LiveMigrate));
        assert!(!capabilities.contains(Capability::SameHostLiveBackingMove));
        assert_eq!(
            provider.supported_classes(),
            &[VolumeClass::NearlineReplicated]
        );
        assert_eq!(provider.name(), PROVIDER_NAME);
    }

    #[test]
    fn extent_rounded_rounds_up_to_whole_extents() {
        const MIB: u64 = 1 << 20;
        assert_eq!(extent_rounded(MIB, 4 * MIB), 4 * MIB);
        assert_eq!(extent_rounded(4 * MIB, 4 * MIB), 4 * MIB);
        assert_eq!(extent_rounded(5 * MIB, 4 * MIB), 8 * MIB);
        // A zero extent means "no rounding" (guarded).
        assert_eq!(extent_rounded(123, 0), 123);
    }

    #[test]
    fn is_device_busy_matches_busy_spellings_only() {
        // The PRIMARY patterns: the real refused-demotion stderr of
        // drbd-utils printing the kernel's SS_DEVICE_IN_USE.
        for busy in [
            "drbdadm: /dev/drbd7: Device or resource busy",
            "error: device is BUSY",
            "refusing: device in use by another process",
        ] {
            assert!(is_device_busy(busy), "{busy:?} must read as busy");
        }
        for other in [
            "connection refused",
            "no such resource",
            "internal error 42",
            "",
        ] {
            assert!(!is_device_busy(other), "{other:?} must not read as busy");
        }
    }

    #[test]
    fn is_device_busy_matches_the_verbatim_real_refused_demotion_stderr() {
        // Verbatim shape of a refused `drbdadm secondary` on a real
        // cluster (drbdsetup.c printing SS_DEVICE_IN_USE plus the
        // kernel's opener info).
        let stderr = "drbd0: State change failed: (-12) Device is held open by someone\n\
                      additional info from kernel:\n\
                      \x20/dev/drbd0 open_cnt:1, writable:1; list of openers follows\n\
                      drbd0 opened by qemu (pid 1234) at 2026-10-09 12:34:56\n";
        assert!(is_device_busy(stderr), "the real refusal must read as busy");
        // The opener-info fragments alone must classify too (stderr
        // truncation must not lose the busy classification).
        assert!(is_device_busy(
            "drbd0: State change failed: (-12) Device is held open by someone"
        ));
        assert!(is_device_busy(
            "/dev/drbd0 open_cnt:1, writable:1; list of openers follows"
        ));
        assert!(is_device_busy(
            "drbd0 opened by qemu (pid 1234) at 2026-10-09 12:34:56"
        ));
    }

    /// A minimal status for the mapping tests.
    fn status(
        local_disk: DiskState,
        connected: bool,
        peer_disk: Option<DiskState>,
        replication: Option<&str>,
        peer_role: Option<Role>,
    ) -> ResourceStatus {
        ResourceStatus {
            name: "vol-r".to_owned(),
            role: Role::Secondary,
            local_disk,
            connected,
            connection: None,
            peer_role,
            peer_disk,
            replication: replication.map(str::to_owned),
            resync_done: None,
            local_open: None,
            quorum: None,
            suspended: None,
            force_io_failures: None,
        }
    }

    #[test]
    fn observed_health_surfaces_the_suspension_qualifiers() {
        use Health::{Degraded, Healthy, Unhealthy};
        // A fully-established, open, suspended resource (operator
        // suspend-io): I/O is frozen — never Healthy.
        let mut suspended = status(
            DiskState::UpToDate,
            true,
            Some(DiskState::UpToDate),
            None,
            Some(Role::Secondary),
        );
        suspended.suspended = Some("user".to_owned());
        assert_eq!(observed_health(&suspended), Degraded);
        // The kernel's no-data-access suspension (local data path
        // lost) degrades the same way — an honest "not serving", not
        // an Unhealthy data-loss claim.
        suspended.suspended = Some("no-data".to_owned());
        assert_eq!(observed_health(&suspended), Degraded);
        // force-io-failures:yes (disk-failure emulation) is Unhealthy.
        let mut failing = status(
            DiskState::UpToDate,
            true,
            Some(DiskState::UpToDate),
            None,
            Some(Role::Secondary),
        );
        failing.force_io_failures = Some(true);
        assert_eq!(observed_health(&failing), Unhealthy);
        // And the unsuspended baseline of the same facts is Healthy,
        // so the qualifiers are what makes the difference.
        assert_eq!(
            observed_health(&status(
                DiskState::UpToDate,
                true,
                Some(DiskState::UpToDate),
                None,
                Some(Role::Secondary)
            )),
            Healthy
        );
    }

    #[test]
    fn observed_health_maps_status_facts_conservatively() {
        use Health::{Degraded, Healthy, Unhealthy, Unknown};
        // Healthy ONLY on connected + both UpToDate + no resync.
        assert_eq!(
            observed_health(&status(
                DiskState::UpToDate,
                true,
                Some(DiskState::UpToDate),
                None,
                Some(Role::Secondary)
            )),
            Healthy
        );
        // Resync in progress: degraded, never healthy.
        assert_eq!(
            observed_health(&status(
                DiskState::UpToDate,
                true,
                Some(DiskState::Inconsistent),
                Some("SyncTarget"),
                Some(Role::Secondary)
            )),
            Degraded
        );
        // Peer not UpToDate: degraded.
        assert_eq!(
            observed_health(&status(
                DiskState::UpToDate,
                true,
                Some(DiskState::Inconsistent),
                None,
                Some(Role::Secondary)
            )),
            Degraded
        );
        // Connection lost: degraded.
        assert_eq!(
            observed_health(&status(DiskState::UpToDate, false, None, None, None)),
            Degraded
        );
        // Local disk failure: unhealthy.
        assert_eq!(
            observed_health(&status(
                DiskState::Failed,
                true,
                Some(DiskState::UpToDate),
                None,
                Some(Role::Secondary)
            )),
            Unhealthy
        );
        assert_eq!(
            observed_health(&status(DiskState::Diskless, false, None, None, None)),
            Unhealthy
        );
        // Local disk holding stale/unseeded data: degraded.
        assert_eq!(
            observed_health(&status(
                DiskState::Outdated,
                true,
                Some(DiskState::UpToDate),
                None,
                Some(Role::Secondary)
            )),
            Degraded
        );
        assert_eq!(
            observed_health(&status(
                DiskState::Inconsistent,
                true,
                Some(DiskState::Inconsistent),
                None,
                Some(Role::Secondary)
            )),
            Degraded
        );
        // Unrecognized local state: an honest unknown, never a guess.
        assert_eq!(
            observed_health(&status(
                DiskState::Other("SomeFutureState".to_owned()),
                true,
                Some(DiskState::UpToDate),
                None,
                Some(Role::Secondary)
            )),
            Unknown
        );
        assert_eq!(
            observed_health(&status(DiskState::DUnknown, false, None, None, None)),
            Unknown
        );
    }

    #[test]
    fn backend_health_and_remote_axis_mappings() {
        use Health::{Degraded, Healthy, Unhealthy, Unknown};
        let up_to_date_peer = |disk: DiskState| {
            status(
                disk,
                true,
                Some(DiskState::UpToDate),
                None,
                Some(Role::Secondary),
            )
        };
        assert_eq!(
            backend_health_of(&up_to_date_peer(DiskState::UpToDate)),
            Healthy
        );
        assert_eq!(
            backend_health_of(&up_to_date_peer(DiskState::Failed)),
            Unhealthy
        );
        assert_eq!(
            backend_health_of(&up_to_date_peer(DiskState::Diskless)),
            Unhealthy
        );
        assert_eq!(
            backend_health_of(&up_to_date_peer(DiskState::Outdated)),
            Degraded
        );
        assert_eq!(
            backend_health_of(&up_to_date_peer(DiskState::Consistent)),
            Degraded
        );
        assert_eq!(
            backend_health_of(&up_to_date_peer(DiskState::Inconsistent)),
            Unknown
        );
        assert_eq!(
            backend_health_of(&up_to_date_peer(DiskState::DUnknown)),
            Unknown
        );

        // The remote axis is an observed fact, never a durability claim:
        // established only when connected with an UpToDate peer outside
        // resync, and CLASSIFIED by the protocol read back from the
        // resource's own definition file.
        let established = up_to_date_peer(DiskState::UpToDate);
        // Protocol A: possible-RPO asynchronous arrival — the
        // asynchronous axis.
        assert_eq!(
            remote_axis_of(&established, Some(ReplicationMode::A)),
            RemoteProtectionAxis::AsynchronousPeer
        );
        // Protocol B: semi-synchronous arrival in remote MEMORY only —
        // still possible-RPO on peer loss, so B is NOT the synchronous
        // axis.
        assert_eq!(
            remote_axis_of(&established, Some(ReplicationMode::B)),
            RemoteProtectionAxis::AsynchronousPeer
        );
        // Protocol C: both durable media acknowledge — the synchronous
        // axis (RPO=0 only under the protocol's own conditions).
        assert_eq!(
            remote_axis_of(&established, Some(ReplicationMode::C)),
            RemoteProtectionAxis::SynchronousPeer
        );
        // A protocol that could not be read back (missing/unparsable
        // resource file) is an honest unknown: no remote claim.
        assert_eq!(
            remote_axis_of(&established, None),
            RemoteProtectionAxis::None
        );
        // Not established (or resyncing): no remote claim regardless of
        // the protocol.
        assert_eq!(
            remote_axis_of(
                &status(DiskState::UpToDate, false, None, None, None),
                Some(ReplicationMode::C)
            ),
            RemoteProtectionAxis::None
        );
        assert_eq!(
            remote_axis_of(
                &status(
                    DiskState::UpToDate,
                    true,
                    Some(DiskState::Inconsistent),
                    Some("SyncTarget"),
                    Some(Role::Secondary)
                ),
                Some(ReplicationMode::C)
            ),
            RemoteProtectionAxis::None
        );
        assert_eq!(
            remote_axis_of(
                &status(
                    DiskState::UpToDate,
                    true,
                    Some(DiskState::Inconsistent),
                    None,
                    Some(Role::Secondary)
                ),
                Some(ReplicationMode::C)
            ),
            RemoteProtectionAxis::None
        );
    }

    #[test]
    fn peer_freshness_classifications() {
        // Fresh: connected, peer Inconsistent, peer not Primary.
        assert_eq!(
            peer_freshness(&status(
                DiskState::Inconsistent,
                true,
                Some(DiskState::Inconsistent),
                None,
                Some(Role::Secondary)
            )),
            PeerFreshness::Fresh
        );
        // Foreign: peer holds data.
        for foreign in [
            DiskState::UpToDate,
            DiskState::Consistent,
            DiskState::Outdated,
        ] {
            assert_eq!(
                peer_freshness(&status(
                    DiskState::Inconsistent,
                    true,
                    Some(foreign.clone()),
                    None,
                    Some(Role::Secondary)
                )),
                PeerFreshness::Foreign,
                "{foreign:?} peer must be foreign"
            );
        }
        // Foreign: peer is Primary.
        assert_eq!(
            peer_freshness(&status(
                DiskState::Inconsistent,
                true,
                Some(DiskState::Inconsistent),
                None,
                Some(Role::Primary)
            )),
            PeerFreshness::Foreign
        );
        // Foreign: unrecognized peer spelling (fail-closed).
        assert_eq!(
            peer_freshness(&status(
                DiskState::Inconsistent,
                true,
                Some(DiskState::Other("Weird".to_owned())),
                None,
                Some(Role::Secondary)
            )),
            PeerFreshness::Foreign
        );
        // Absent: no connection.
        assert_eq!(
            peer_freshness(&status(DiskState::Inconsistent, false, None, None, None)),
            PeerFreshness::Absent
        );
        // Absent: diskless/unknown peer has nothing to seed against.
        assert_eq!(
            peer_freshness(&status(
                DiskState::Inconsistent,
                true,
                Some(DiskState::Diskless),
                None,
                Some(Role::Secondary)
            )),
            PeerFreshness::Absent
        );
        assert_eq!(
            peer_freshness(&status(
                DiskState::Inconsistent,
                true,
                Some(DiskState::DUnknown),
                None,
                Some(Role::Secondary)
            )),
            PeerFreshness::Absent
        );
        // Absent: connected but no peer-disk line at all — real
        // drbdsetup omits the peer-device block while the peer's
        // device state is un-exchanged (replication Off + DUnknown,
        // the transient during connection establishment). This must
        // wait, never classify as Foreign (which would tear a fresh
        // resource down on a false "peer holds data" verdict).
        assert_eq!(
            peer_freshness(&status(
                DiskState::Inconsistent,
                true,
                None,
                None,
                Some(Role::Secondary)
            )),
            PeerFreshness::Absent
        );
        // Foreign: a Failed peer disk conservatively counts as
        // data-holding (never seeded over).
        assert_eq!(
            peer_freshness(&status(
                DiskState::Inconsistent,
                true,
                Some(DiskState::Failed),
                None,
                Some(Role::Secondary)
            )),
            PeerFreshness::Foreign
        );
    }

    /// A nearline create request body for the policy tests.
    fn policy_request(
        engine: Option<&str>,
        remote_replicas: u32,
        provisioning: Option<Provisioning>,
    ) -> CreateVolumeRequest {
        CreateVolumeRequest {
            api_version: "volvisor.volume.v2".to_owned(),
            operation_id: OperationId::new("op-policy").expect("valid id"),
            project_id: ProjectId::new("p").expect("valid id"),
            volume_id: volume_id("policy-vol"),
            volume_class: VolumeClass::NearlineReplicated,
            size_bytes: 1 << 30,
            logical_block_size: None,
            provisioning,
            placement: None,
            local_protection: None,
            replication: Some(ReplicationPolicyRequest {
                engine: engine.map(str::to_owned),
                mode: ReplicationModeRequest::Async,
                remote_replicas,
                allow_degraded_create: false,
            }),
            migration_policy: None,
            encryption: None,
        }
    }

    #[test]
    fn check_policies_rejections() {
        // Baseline: drbd9 with one replica is accepted.
        assert!(check_policies(&policy_request(Some("drbd9"), 1, None)).is_ok());
        assert!(check_policies(&policy_request(None, 1, None)).is_ok());
        // Wrong engine, wrong replica count, thin provisioning.
        assert_eq!(
            check_policies(&policy_request(Some("ceph"), 1, None))
                .unwrap_err()
                .code,
            ApiErrorCode::UnsupportedClassOrPolicy
        );
        assert_eq!(
            check_policies(&policy_request(Some("drbd9"), 2, None))
                .unwrap_err()
                .code,
            ApiErrorCode::UnsupportedClassOrPolicy
        );
        assert_eq!(
            check_policies(&policy_request(Some("drbd9"), 1, Some(Provisioning::Thin)))
                .unwrap_err()
                .code,
            ApiErrorCode::UnsupportedClassOrPolicy
        );
        // Missing replication policy entirely.
        let mut none = policy_request(Some("drbd9"), 1, None);
        none.replication = None;
        assert_eq!(
            check_policies(&none).unwrap_err().code,
            ApiErrorCode::UnsupportedClassOrPolicy
        );
        // Placement: only the local host and the host failure domain.
        let mut rack = policy_request(Some("drbd9"), 1, None);
        rack.placement = Some(Placement {
            preferred_host_id: None,
            failure_domain: Some(FailureDomain::Rack),
        });
        assert_eq!(
            check_policies(&rack).unwrap_err().code,
            ApiErrorCode::InsufficientFailureDomains
        );
        let mut remote_host = policy_request(Some("drbd9"), 1, None);
        remote_host.placement = Some(Placement {
            preferred_host_id: Some(HostId::new("other-host").expect("valid id")),
            failure_domain: None,
        });
        assert_eq!(
            check_policies(&remote_host).unwrap_err().code,
            ApiErrorCode::InsufficientFailureDomains
        );
    }
}
