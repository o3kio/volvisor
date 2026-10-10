//! # volvisor-types
//!
//! Typed domain vocabulary for the Volvisor volume virtualization control plane.
//!
//! This crate is the single source of truth for opaque identities, canonical
//! state vocabularies, the typed error taxonomy, versioned provider
//! capabilities, domain records and the request/response shapes of the
//! [Volume API v2 contract](https://github.com/o3kio/volvisor) (P0 scope).
//!
//! Invariants enforced here (see AGENTS.md rules 1-21 and the v2 contracts):
//!
//! - identities are opaque and never derived from Linux device names, PCI BDF
//!   or backend friendly names (rule 7);
//! - state enums use exactly the canonical vocabularies: PascalCase common
//!   volume states (Volume API v2 section 7), SCREAMING_SNAKE migration
//!   states (nearline contract section 6) and online-move states
//!   (Volume API v2 section 4A);
//! - unknown or unproven conditions surface as `unknown`, never as a healthy
//!   default (observability truthfulness);
//! - every typed error code of Volume API v2 section 7 exists, maps to an
//!   HTTP status and round-trips through JSON.

#![deny(unsafe_code)]
// Tests may use expect/unwrap for invariant assertions; production code may not.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod admin;
pub mod authority;
pub mod capability;
pub mod crash;
pub mod domain;
pub mod error;
pub mod id;
pub mod request;
pub mod state;

pub use admin::{ClaimDeviceRequest, DeviceListResponse, ReleaseDeviceRequest};
pub use authority::{
    AuthoritySummary, AuthorityView, BarrierAttestation, EndpointBacking, EpochRetirement,
    FencingProof, LeaseId, LeaseState, LossBoundary, PromotionClassification, RecordedBarrier,
    RecordedMigrationBarrier, SafeCurrentEvidence, VolumeRegistration, WriterEpoch,
};
pub use capability::{Capability, CapabilitySet};
pub use domain::{
    AccessMode, Attachment, AttachmentState, DeviceRole, EffectiveProtection, FailureDomain,
    Frontend, Health, LocalProtectionAxis, Migration, PhysicalDevice, Pool, PoolProtection,
    RemoteProtectionAxis, Replica, Volume,
};
pub use error::{ApiError, ApiErrorBody, ApiErrorCode, INVALID_API_VERSION};
pub use id::{
    AttachmentId, DeviceId, HostId, ID_MAX_LEN, MigrationId, OperationId, PoolId, ProjectId,
    VolumeId,
};
pub use request::{
    AccessModeRequest, AdoptVolumeRequest, AdoptVolumeResponse, CreateVolumeRequest,
    DeleteVolumeRequest, DetachVolumeRequest, DrainProof, ErasurePolicy, GrowVolumeRequest,
    GrowVolumeResponse, InspectVolumeResponse, ListVolumesResponse, MoveVolumeBackingRequest,
    MoveVolumeBackingResponse, PROVIDER_API_VERSION,
};
pub use state::{GuestNotificationStatus, MigrationState, MoveVolumeBackingState, VolumeLifecycle};

/// Canonical API version string accepted in request envelopes.
pub const API_VERSION: &str = "volvisor.volume.v2";

/// Validate that a request envelope carries the supported `api_version`.
///
/// Fail-closed: an unknown version is rejected rather than best-effort parsed
/// (Volume API v2, "Purpose and compatibility").
pub fn validate_api_version(v: &str) -> Result<(), ApiError> {
    if v == API_VERSION {
        Ok(())
    } else {
        Err(ApiError::new(
            ApiErrorCode::UnsupportedClassOrPolicy,
            format!("unsupported api_version {v:?}; expected {API_VERSION:?}"),
        ))
    }
}
