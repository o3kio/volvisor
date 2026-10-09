//! Canonical state vocabularies.
//!
//! Three distinct vocabularies exist and must never be conflated:
//!
//! - [`VolumeLifecycle`]: PascalCase common volume states of Volume API v2
//!   section 7;
//! - [`MigrationState`]: SCREAMING_SNAKE canonical migration states of the
//!   nearline contract section 6, including `IN_DOUBT` between
//!   `SOURCE_REVOKED` and `DESTINATION_AUTHORIZED`;
//! - [`MoveVolumeBackingState`]: online same-host backing relocation states of
//!   Volume API v2 section 4A, where `FAILED` is only legal before `PIVOTED`.

use serde::{Deserialize, Serialize};

/// Common volume lifecycle states (Volume API v2 section 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VolumeLifecycle {
    /// Mutation accepted, not yet provisioned.
    Requested,
    /// Backend allocation in progress.
    Provisioning,
    /// Provisioned and attachable.
    Ready,
    /// Serving with reduced protection; never silently reported healthy.
    Degraded,
    /// Attachment admission in progress.
    Attaching,
    /// Actively attached (at most one writable attachment).
    Attached,
    /// Attachment teardown / I/O drain in progress.
    Detaching,
    /// Deletion in progress.
    Deleting,
    /// Terminal failure; no implicit recovery.
    Failed,
    /// Foreign or ambiguous backend state; quarantined, never auto-adopted.
    Quarantined,
}

/// Canonical migration states (nearline contract section 6 plus the terminal
/// API outcomes `IN_DOUBT` and `ABORTED` of Volume API v2 section 5).
///
/// `SOURCE_REVOKED -> DESTINATION_AUTHORIZED` must never be collapsed into a
/// single atomic status; the gap is exactly where `IN_DOUBT` is observable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MigrationState {
    /// Destination replica and read-only endpoint prepared.
    Prepared,
    /// Source keeps writing while the replica catches up.
    Precopy,
    /// Guest I/O and host frontend drained.
    Quiesced,
    /// Source barrier B durable; target exact-prefix durable >= B.
    #[serde(rename = "BARRIER_DURABLE")]
    BarrierDurable,
    /// Old epoch can no longer admit writes.
    #[serde(rename = "SOURCE_REVOKED")]
    SourceRevoked,
    /// Legitimate fail-closed state after revocation, before authorization.
    #[serde(rename = "IN_DOUBT")]
    InDoubt,
    /// Strictly newer epoch admitted.
    #[serde(rename = "DESTINATION_AUTHORIZED")]
    DestinationAuthorized,
    /// Destination VM resumed.
    #[serde(rename = "VM_RESUMED")]
    VmResumed,
    /// Data and VMM completion both proven.
    Complete,
    /// Terminated before authority moved.
    Aborted,
}

impl MigrationState {
    /// Whether a generic `ABORTED` may be reported from this state.
    ///
    /// After `SOURCE_REVOKED`, an interrupted migration must be reported as
    /// `IN_DOUBT` or roll forward under reconciled authority; a generic
    /// `ABORTED` is never legal (Volume API v2 section 5).
    #[must_use]
    pub fn abort_legal(self) -> bool {
        !matches!(
            self,
            Self::SourceRevoked
                | Self::InDoubt
                | Self::DestinationAuthorized
                | Self::VmResumed
                | Self::Complete
        )
    }
}

/// Online same-host backing relocation states (Volume API v2 section 4A).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MoveVolumeBackingState {
    /// Validating layout and reserving target capacity.
    Preparing,
    /// Copying/mirroring extents to the target backing.
    Copying,
    /// Mirror established; pivot not yet performed.
    #[serde(rename = "MIRROR_READY")]
    MirrorReady,
    /// Authority moved to the target backing.
    Pivoted,
    /// Target authoritative, source safely reconciled and removed.
    Complete,
    /// Terminal failure; only legal strictly before `PIVOTED`.
    Failed,
    /// Post-pivot unknown outcome; rolls forward under reconciled authority.
    #[serde(rename = "IN_DOUBT")]
    InDoubt,
}

impl MoveVolumeBackingState {
    /// Whether `FAILED` may be reported in this state.
    ///
    /// Never report a generic `FAILED` after the pivot: the outcome is
    /// `IN_DOUBT` or rolls forward (Volume API v2 section 4A).
    #[must_use]
    pub fn failed_reportable(self) -> bool {
        !matches!(self, Self::Pivoted | Self::Complete | Self::InDoubt)
    }
}

/// Guest notification status of an online grow (Volume API v2 section 4A).
///
/// A failed guest notification after successful backing growth is a retryable
/// partial completion, never an instruction to shrink backing data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestNotificationStatus {
    /// Notification not yet attempted or not yet acknowledged.
    Pending,
    /// VMM acknowledged the new size.
    Notified,
    /// Notification failed; retry required. Backing is NOT shrunk.
    Failed,
    /// No running frontend exists to notify (e.g. detached volume).
    NotApplicable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_states_wire_format() {
        let cases = [
            (MigrationState::Prepared, "\"PREPARED\""),
            (MigrationState::Precopy, "\"PRECOPY\""),
            (MigrationState::Quiesced, "\"QUIESCED\""),
            (MigrationState::BarrierDurable, "\"BARRIER_DURABLE\""),
            (MigrationState::SourceRevoked, "\"SOURCE_REVOKED\""),
            (MigrationState::InDoubt, "\"IN_DOUBT\""),
            (
                MigrationState::DestinationAuthorized,
                "\"DESTINATION_AUTHORIZED\"",
            ),
            (MigrationState::VmResumed, "\"VM_RESUMED\""),
            (MigrationState::Complete, "\"COMPLETE\""),
            (MigrationState::Aborted, "\"ABORTED\""),
        ];
        for (state, wire) in cases {
            assert_eq!(serde_json::to_string(&state).expect("serialize"), wire);
            let back: MigrationState = serde_json::from_str(wire).expect("deserialize");
            assert_eq!(back, state);
        }
    }

    #[test]
    fn abort_never_legal_after_revocation() {
        assert!(MigrationState::Prepared.abort_legal());
        assert!(MigrationState::Precopy.abort_legal());
        assert!(MigrationState::Quiesced.abort_legal());
        assert!(MigrationState::BarrierDurable.abort_legal());
        assert!(!MigrationState::SourceRevoked.abort_legal());
        assert!(!MigrationState::InDoubt.abort_legal());
        assert!(!MigrationState::DestinationAuthorized.abort_legal());
        assert!(!MigrationState::VmResumed.abort_legal());
        assert!(!MigrationState::Complete.abort_legal());
    }

    #[test]
    fn failed_only_before_pivot() {
        assert!(MoveVolumeBackingState::Preparing.failed_reportable());
        assert!(MoveVolumeBackingState::Copying.failed_reportable());
        assert!(MoveVolumeBackingState::MirrorReady.failed_reportable());
        assert!(!MoveVolumeBackingState::Pivoted.failed_reportable());
        assert!(!MoveVolumeBackingState::Complete.failed_reportable());
        assert!(!MoveVolumeBackingState::InDoubt.failed_reportable());
    }

    #[test]
    fn volume_states_wire_format() {
        let cases = [
            (VolumeLifecycle::Requested, "\"Requested\""),
            (VolumeLifecycle::Provisioning, "\"Provisioning\""),
            (VolumeLifecycle::Ready, "\"Ready\""),
            (VolumeLifecycle::Degraded, "\"Degraded\""),
            (VolumeLifecycle::Attaching, "\"Attaching\""),
            (VolumeLifecycle::Attached, "\"Attached\""),
            (VolumeLifecycle::Detaching, "\"Detaching\""),
            (VolumeLifecycle::Deleting, "\"Deleting\""),
            (VolumeLifecycle::Failed, "\"Failed\""),
            (VolumeLifecycle::Quarantined, "\"Quarantined\""),
        ];
        for (state, wire) in cases {
            assert_eq!(serde_json::to_string(&state).expect("serialize"), wire);
            let back: VolumeLifecycle = serde_json::from_str(wire).expect("deserialize");
            assert_eq!(back, state);
        }
    }
}
