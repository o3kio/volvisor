//! Domain records (SPEC-0002 section 2 data model).
//!
//! Stable identities are opaque, immutable and never based solely on Linux
//! device names, PCI BDF or Ceph friendly image names. The provider maintains
//! authoritative cross-references and reconciles observed state; a backend
//! private reference is provider-internal and never exposed to tenants.

use serde::{Deserialize, Serialize};

use crate::id::{AttachmentId, DeviceId, HostId, MigrationId, PoolId, ProjectId, VolumeId};
use crate::state::{MigrationState, MoveVolumeBackingState, VolumeLifecycle};

/// Volume storage class (Volume API v2 canonical values).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VolumeClass {
    /// Logical volumes on claimed local disks (LVM reference prototype).
    #[serde(rename = "native-local")]
    NativeLocal,
    /// Locally served replica with remote host replication.
    #[serde(rename = "nearline-replicated")]
    NearlineReplicated,
    /// RBD images on a Ceph cluster.
    #[serde(rename = "ceph-rbd")]
    CephRbd,
}

/// Health reporting axis. `Unknown` is the honest default for unproven
/// conditions; it never degrades to `Healthy` (observability truthfulness).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Health {
    /// The honest default: health not established.
    #[default]
    Unknown,
    /// Fully healthy per the owning component's definition.
    Healthy,
    /// Serving with reduced protection or partial failure.
    Degraded,
    /// Failed.
    Unhealthy,
}

/// Failure-domain granularity for placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureDomain {
    /// A single physical host.
    Host,
    /// A rack or equivalent grouping.
    Rack,
}

/// Requested or effective local media protection on the serving host
/// (Volume API v2 section 6: distinct from remote protection).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalProtectionAxis {
    /// No additional local media legs.
    #[default]
    None,
    /// Local mirror (e.g. qualified md/dm implementation).
    Mirror,
    /// Backend-specific local protection.
    ProviderSpecific,
}

/// Requested or effective remote protection axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProtectionAxis {
    /// No remote copy.
    #[default]
    None,
    /// Asynchronous peer replication (possible-RPO; not RPO=0).
    AsynchronousPeer,
    /// Ceph placement/replica or EC policy.
    CephPolicy,
}

/// Effective protection reported on inspect: two independent axes, never a
/// single boolean (Volume API v2 section 6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveProtection {
    /// Local media protection currently in effect.
    pub local: LocalProtectionAxis,
    /// Remote protection currently in effect.
    pub remote: RemoteProtectionAxis,
}

/// Provisioning mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provisioning {
    /// Thin provisioning (requires thin-pool safeguards).
    Thin,
    /// Thick allocation.
    #[default]
    Thick,
}

/// Physical-device role in a pool (SPEC-0002 section 3): a disk is claimed
/// for exactly one physical role, distinct from the volume class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceRole {
    /// Device backs a native-local pool.
    NativePool,
    /// Device backs a nearline replica pool.
    NearlinePool,
    /// Device is a Ceph OSD.
    CephOsd,
}

/// Monotonic ownership/consistency generation for optimistic concurrency.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Generation(pub u64);

impl Generation {
    /// The next generation in sequence.
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Attachment access mode (single-writer by default).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    /// Exactly one active writable attachment (default across all classes).
    #[default]
    SingleWriter,
    /// Multi-reader; requires an explicit safe multi-reader contract.
    ReadOnly,
}

/// VMM attachment evidence states (Volume API v2 section 3): prepared,
/// advertised and active must be distinguished honestly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentState {
    /// Backend handle prepared; not yet advertised to a VMM.
    #[default]
    Prepared,
    /// Advertised to a VMM; not yet observed active.
    Advertised,
    /// Observed active in the guest.
    Active,
}

/// Frontend used to expose a volume to a VM.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Frontend {
    /// Cloud Hypervisor virtio-blk on a host block path.
    VirtioBlk {
        /// Host-scoped, ephemeral backend device path (never a secret).
        host_device_path: String,
    },
    /// PCI passthrough attachment profile (separate qualification).
    PciPassthrough {
        /// Claimed physical device identity.
        device_id: DeviceId,
    },
}

/// Evidence status of a support claim (AGENTS rule 12): honest reporting,
/// never implying production support from prototype behavior.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    /// Control-plane prototype only; no conformance or real-host evidence.
    #[default]
    PrototypeOnly,
    /// Provider conformance kit passed at a recorded commit.
    ConformancePassed,
    /// Real-host failure-campaign evidence recorded (future gate).
    RealHostGated,
}

/// A claimed physical device (SPEC-0002 section 2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalDevice {
    /// Opaque stable device identity (never a Linux name or BDF).
    pub id: DeviceId,
    /// Owning host.
    pub host_id: HostId,
    /// Namespace identities when the device is multi-namespace.
    pub namespace_ids: Vec<String>,
    /// Capacity in bytes.
    pub capacity_bytes: u64,
    /// Device health (`Unknown` until proven).
    pub health: Health,
    /// Claimed role, if claimed.
    pub owner_role: Option<DeviceRole>,
    /// Ownership generation (monotonic).
    pub owner_generation: Generation,
}

/// Pool protection summary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolProtection {
    /// Local mirror configured on the pool.
    pub local_mirror: bool,
    /// Remote replication configured on the pool.
    pub remote_replication: bool,
}

/// A capacity pool (SPEC-0002 section 2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pool {
    /// Opaque pool identity.
    pub id: PoolId,
    /// Volume class served by this pool.
    pub backend_class: VolumeClass,
    /// Devices combined under an explicit validated topology.
    pub device_ids: Vec<DeviceId>,
    /// Owning host (or Ceph cluster identifier).
    pub host_or_ceph_cluster: String,
    /// Physical capacity in bytes.
    pub capacity_bytes: u64,
    /// Currently allocatable capacity in bytes.
    pub allocatable_bytes: u64,
    /// Pool protection summary.
    pub protection: PoolProtection,
    /// Pool health (`Unknown` until proven).
    pub health: Health,
}

/// A logical volume (SPEC-0002 section 2). `backend_private_ref` is
/// provider-internal and skipped in tenant-facing serialization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    /// Opaque volume identity.
    pub id: VolumeId,
    /// Owning tenant project.
    pub project_id: ProjectId,
    /// Storage class.
    pub class: VolumeClass,
    /// Owning pool.
    pub pool_ref: PoolId,
    /// Logical size in bytes.
    pub size_bytes: u64,
    /// Provisioning mode.
    pub provisioning: Provisioning,
    /// Logical block size in bytes.
    pub block_size: u32,
    /// Current generation (optimistic concurrency).
    pub generation: Generation,
    /// Lifecycle state.
    pub state: VolumeLifecycle,
    /// Effective protection (two axes).
    pub effective_protection: EffectiveProtection,
    /// Placement failure domain.
    pub failure_domain: FailureDomain,
    /// Volume health (`Unknown` until proven).
    pub health: Health,
    /// Active attachment identities.
    pub attachment_ids: Vec<AttachmentId>,
    /// Current writer attachment, if any.
    pub current_writer: Option<AttachmentId>,
    /// Writer epoch / data epoch for authority reasoning.
    pub data_epoch: u64,
    /// Provider-internal backend reference; never tenant-visible.
    #[serde(skip)]
    pub backend_private_ref: Option<String>,
    /// Honest evidence status of any support claim.
    pub evidence_status: EvidenceStatus,
}

/// A VM attachment (SPEC-0002 section 2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Opaque attachment identity.
    pub id: AttachmentId,
    /// Owning volume.
    pub volume_id: VolumeId,
    /// Consuming VM.
    pub vm_id: String,
    /// Host the attachment is scoped to.
    pub host_id: HostId,
    /// Attachment generation.
    pub generation: Generation,
    /// Frontend used.
    pub frontend: Frontend,
    /// Access mode.
    pub access_mode: AccessMode,
    /// Attachment evidence state.
    pub state: AttachmentState,
}

/// A remote replica record (nearline; present in the model from day one).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replica {
    /// Opaque replica identity.
    pub id: String,
    /// Owning volume.
    pub volume_id: VolumeId,
    /// Replica host.
    pub host_id: HostId,
    /// Backing disk or pool reference.
    pub disk_or_pool_ref: String,
    /// Replica generation.
    pub generation: Generation,
    /// Replica role (primary/secondary/witness vocabulary is backend-owned).
    pub role: String,
    /// Durable progress through the source barrier, in bytes.
    pub durable_progress: u64,
    /// Replica health (`Unknown` until proven).
    pub health: Health,
}

/// A migration transaction (nearline contract section 6).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Migration {
    /// Opaque migration identity.
    pub id: MigrationId,
    /// Migrating VM.
    pub vm_id: String,
    /// Source host.
    pub source_host: HostId,
    /// Target host.
    pub target_host: HostId,
    /// All participating volumes (single consistent cut across all disks).
    pub participating_volume_ids: Vec<VolumeId>,
    /// VM generation at cutover boundary.
    pub vm_generation: Generation,
    /// Writer epoch.
    pub epoch: u64,
    /// Canonical migration state.
    pub state: MigrationState,
    /// Cutover boundary description.
    pub cutover_boundary: Option<String>,
    /// Last error detail, if any (never secret material).
    pub last_error: Option<String>,
}

/// An online same-host backing relocation transaction (Volume API v2 4A).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveVolumeBacking {
    /// Opaque operation identity.
    pub operation_id: crate::id::OperationId,
    /// Owning volume.
    pub volume_id: VolumeId,
    /// Target pool.
    pub target_pool_id: PoolId,
    /// Canonical online-move state.
    pub state: MoveVolumeBackingState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_wire_format() {
        assert_eq!(
            serde_json::to_string(&VolumeClass::NativeLocal).expect("serialize"),
            "\"native-local\""
        );
        assert_eq!(
            serde_json::to_string(&VolumeClass::NearlineReplicated).expect("serialize"),
            "\"nearline-replicated\""
        );
        assert_eq!(
            serde_json::to_string(&VolumeClass::CephRbd).expect("serialize"),
            "\"ceph-rbd\""
        );
    }

    #[test]
    fn health_defaults_to_unknown() {
        assert_eq!(Health::default(), Health::Unknown);
        assert_eq!(
            serde_json::to_string(&Health::Unknown).expect("serialize"),
            "\"Unknown\""
        );
    }

    #[test]
    fn protection_axes_wire_format() {
        assert_eq!(
            serde_json::to_string(&LocalProtectionAxis::ProviderSpecific).expect("s"),
            "\"provider_specific\""
        );
        assert_eq!(
            serde_json::to_string(&RemoteProtectionAxis::AsynchronousPeer).expect("s"),
            "\"asynchronous_peer\""
        );
        assert_eq!(
            serde_json::to_string(&RemoteProtectionAxis::CephPolicy).expect("s"),
            "\"ceph_policy\""
        );
    }

    #[test]
    fn backend_private_ref_not_serialized() {
        let vol = Volume {
            id: VolumeId::new("vol-1").expect("id"),
            project_id: ProjectId::new("p").expect("id"),
            class: VolumeClass::NativeLocal,
            pool_ref: PoolId::new("pool-1").expect("id"),
            size_bytes: 1024,
            provisioning: Provisioning::Thick,
            block_size: 512,
            generation: Generation(1),
            state: VolumeLifecycle::Ready,
            effective_protection: EffectiveProtection::default(),
            failure_domain: FailureDomain::Host,
            health: Health::Unknown,
            attachment_ids: vec![],
            current_writer: None,
            data_epoch: 0,
            backend_private_ref: Some("/dev/vg/lv".to_owned()),
            evidence_status: EvidenceStatus::PrototypeOnly,
        };
        let json = serde_json::to_string(&vol).expect("serialize");
        assert!(!json.contains("backend_private_ref"));
        assert!(!json.contains("/dev/vg/lv"));
    }
}
