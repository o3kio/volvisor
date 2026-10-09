//! Versioned provider capabilities (Volume API v2 section 8).
//!
//! Capabilities are tied to implementation/VMM version and evidence, never
//! inferred from a backend product name. Absent capabilities cause fail-closed
//! `UNSUPPORTED_CLASS_OR_POLICY` rejections, never silent degradation.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

/// A versioned provider capability token (wire spelling: snake_case).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Create volumes.
    Create,
    /// Attach/detach volumes.
    Attach,
    /// Resize (grow) volumes.
    Resize,
    /// Snapshot volumes.
    Snapshot,
    /// Clone volumes.
    Clone,
    /// Optional local mirror (separate from remote replication).
    LocalMirror,
    /// Provider-managed encryption.
    Encryption,
    /// Remote replication.
    Replicate,
    /// Cross-host live migration participation.
    LiveMigrate,
    /// Offline copy workflows.
    OfflineCopy,
    /// Same-VG physical-extent evacuation (LVM pvmove scope).
    SameVgExtentMove,
    /// Same-host online whole-volume backing migration.
    SameHostLiveBackingMove,
    /// Adapter for an existing external Ceph cluster.
    RbdClusterAdapter,
    /// Volvisor-managed Ceph OSD placement (incl. gated rook-cell mode).
    ManagedCephOsd,
    /// Exclusive PCI passthrough attachment profile.
    PciPassthrough,
}

/// An owned set of capability tokens.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet(BTreeSet<Capability>);

impl CapabilitySet {
    /// Build a set from an iterator of capabilities.
    #[must_use]
    pub fn of<I: IntoIterator<Item = Capability>>(items: I) -> Self {
        Self(items.into_iter().collect())
    }

    /// The P0 native-local baseline: create, attach and grow only.
    #[must_use]
    pub fn native_local_p0() -> Self {
        Self::of([Capability::Create, Capability::Attach, Capability::Resize])
    }

    /// Whether the set contains the given capability.
    #[must_use]
    pub fn contains(&self, cap: Capability) -> bool {
        self.0.contains(&cap)
    }

    /// Whether every capability in `required` is present.
    #[must_use]
    pub fn contains_all(&self, required: &CapabilitySet) -> bool {
        required.0.is_subset(&self.0)
    }

    /// Insert a capability.
    pub fn insert(&mut self, cap: Capability) {
        self.0.insert(cap);
    }

    /// Iterate the contained capabilities in stable order.
    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        self.0.iter().copied()
    }
}

impl fmt::Display for CapabilitySet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for cap in &self.0 {
            if !first {
                f.write_str(",")?;
            }
            first = false;
            f.write_str(cap.wire_name())?;
        }
        Ok(())
    }
}

impl Capability {
    /// Wire (snake_case) spelling of this capability token.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Attach => "attach",
            Self::Resize => "resize",
            Self::Snapshot => "snapshot",
            Self::Clone => "clone",
            Self::LocalMirror => "local_mirror",
            Self::Encryption => "encryption",
            Self::Replicate => "replicate",
            Self::LiveMigrate => "live_migrate",
            Self::OfflineCopy => "offline_copy",
            Self::SameVgExtentMove => "same_vg_extent_move",
            Self::SameHostLiveBackingMove => "same_host_live_backing_move",
            Self::RbdClusterAdapter => "rbd_cluster_adapter",
            Self::ManagedCephOsd => "managed_ceph_osd",
            Self::PciPassthrough => "pci_passthrough",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&Capability::SameVgExtentMove).expect("serialize"),
            "\"same_vg_extent_move\""
        );
        assert_eq!(
            serde_json::to_string(&Capability::PciPassthrough).expect("serialize"),
            "\"pci_passthrough\""
        );
    }

    #[test]
    fn containment_and_subset() {
        let base = CapabilitySet::native_local_p0();
        assert!(base.contains(Capability::Create));
        assert!(!base.contains(Capability::Snapshot));
        let required = CapabilitySet::of([Capability::Create, Capability::Attach]);
        assert!(base.contains_all(&required));
        let too_much = CapabilitySet::of([Capability::Replicate]);
        assert!(!base.contains_all(&too_much));
    }
}
