//! # The daemon's consumer-facing mobility surface (P4b plan §6,
//! stage B2)
//!
//! The five consumer-facing mobility routes
//! (`POST /v2/vms/{vm_id}/check-mobility`, `POST /v2/migrations`,
//! `POST /v2/migrations/{id}/transfer`, `GET /v2/migrations/{id}`,
//! `POST /v2/migrations/{id}/abort`) are served through this seam:
//! a small, engine-neutral vocabulary the daemon implements over the
//! [`MigrationCoordinator`](crate::MigrationCoordinator) plus the
//! provider surfaces, and the API layer journals and authenticates
//! like every other privileged mutation.
//!
//! Two deliberate boundaries:
//!
//! - **`check-mobility` is not here.** Eligibility is a provider
//!   observation (VM-wide, rule 6); the route reads it from the
//!   provider's `HandoffSurface::handoff_eligibility` (in
//!   `volvisor-provider`) through the daemon's provider surface —
//!   there is nothing for a migration record to add to a read-only
//!   eligibility answer.
//! - **This trait is not implemented generically for
//!   `MigrationCoordinator<D>`** (a documented stage-B2 deviation).
//!   `prepare` needs the provider-local participant facts — the DRBD
//!   resource and minor each volume's writer identity lives in —
//!   which the engine-neutral coordinator cannot derive from a
//!   [`MobilityRequest`]; the daemon's wrapper enriches the request
//!   by asking the provider first (a typed refusal when a volume is
//!   unknown or its generation is stale), then hands the completed
//!   [`PrepareHandoffRequest`](crate::PrepareHandoffRequest) to the
//!   coordinator. Keeping the enrichment in the daemon keeps this
//!   crate free of any provider dependency (the stage-B1 boundary).
//!
//! The stage-B2 daemon implementation is additionally responsible for
//! serializing concurrent consumer operations per migration (the
//! coordinator is not internally serialized; plan §3).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use volvisor_types::id::{HostId, MigrationId, VolumeId};
use volvisor_types::{ApiError, ApiErrorCode};

use crate::types::MigrationSummary;

/// The request shape of the journaled `PrepareNearlineHandoff` (plan
/// §6): the consumer names the migration, the VM whose whole writable
/// set participates, the destination and the expected generation of
/// every participating volume.
///
/// The engine-neutral projection of the coordinator's
/// [`PrepareHandoffRequest`](crate::PrepareHandoffRequest): the
/// provider-local participant facts (resource, minor) are derived by
/// the daemon from the provider state, not asserted by the consumer —
/// `vm_id` is therefore part of this request even though the plan's
/// route table renders it as the VM the volume set was computed for
/// (an additive field: the daemon must know which VM's attachments
/// to verify, and `check-mobility` is not a precondition the API can
/// rely on).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MobilityRequest {
    /// Consumer-supplied, unique per migration (the `OperationId`
    /// validation rules; idempotency key).
    pub migration_id: MigrationId,
    /// The VM whose whole writable set participates (rule 6: one cut
    /// owns a VM at a time).
    pub vm_id: String,
    /// The destination host (the peer that will be granted).
    pub target_host: HostId,
    /// The participating volumes (non-empty, unique).
    pub volume_ids: Vec<VolumeId>,
    /// The expected generation per volume, positionally aligned with
    /// `volume_ids` (fail-closed on a stale generation: a volume the
    /// consumer has not seen lately refuses the whole preparation).
    pub expected_generations: Vec<u64>,
}

impl MobilityRequest {
    /// Validate the request shape before any journaling: a non-empty
    /// vm id, a non-empty, duplicate-free volume set, and a
    /// positionally aligned `expected_generations` list.
    ///
    /// # Errors
    /// `INVALID_REQUEST` for every shape violation (typed, before the
    /// journal sees the payload).
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.vm_id.is_empty() {
            return Err(ApiError::invalid_request("vm_id must not be empty"));
        }
        if self.volume_ids.is_empty() {
            return Err(ApiError::invalid_request(
                "a migration needs at least one participating volume",
            ));
        }
        if self.volume_ids.len() != self.expected_generations.len() {
            return Err(ApiError::invalid_request(format!(
                "expected_generations has {} entries for {} volumes (positionally aligned lists)",
                self.expected_generations.len(),
                self.volume_ids.len()
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for volume_id in &self.volume_ids {
            if !seen.insert(volume_id.clone()) {
                return Err(ApiError::invalid_request(format!(
                    "duplicate participant volume {volume_id}"
                )));
            }
        }
        Ok(())
    }
}

/// The consumer-facing mobility surface (plan §6): what the five
/// journaled mobility routes drive. Implemented by the stage-B2
/// daemon over its coordinator (see the module docs for why not
/// generically here) and by a scripted fake in the API tests.
///
/// Every method is idempotent by `migration_id` (prepare) or by the
/// durable record (transfer/abort), and every refusal is typed — the
/// API journals the intent and resolves a replay exactly like any
/// other privileged mutation.
#[async_trait]
pub trait MigrationSurface: Send + Sync {
    /// `PrepareNearlineHandoff`: verify the participant set against
    /// the provider (existence, generation, attachment) and the
    /// destination (peer `prepare`), then persist the `PREPARED`
    /// record. Idempotent by `migration_id`: identical content
    /// returns the existing record; different content is a typed
    /// conflict.
    ///
    /// # Errors
    /// The surface's typed error (shape violations, unknown or
    /// stale-generation volumes, a conflicting `migration_id`, a
    /// destination refusal).
    async fn prepare(&self, request: MobilityRequest) -> Result<MigrationSummary, ApiError>;

    /// `BarrierAndTransfer`: record the consumer's proof as
    /// **corroboration** (plan §6: recorded, never trusted — the
    /// coordinator performs and verifies its own pause and its own
    /// durability proof) and start the long-running drive. Returns
    /// the observation at drive start — the drive continues in the
    /// background and progress is read through [`Self::observe`].
    ///
    /// A transfer on a TERMINAL record also answers `202` with the
    /// record: the route's contract is 202-always (the proof is
    /// recorded durably either way); the drive itself refuses
    /// terminal records internally and logs the refusal — it never
    /// surfaces as a route error.
    ///
    /// # Errors
    /// `NOT_FOUND` for an unknown migration; the surface's typed
    /// error when the proof cannot be recorded durably.
    async fn transfer(
        &self,
        migration_id: &MigrationId,
        proof: serde_json::Value,
    ) -> Result<MigrationSummary, ApiError>;

    /// `ObserveHandoff` (read-only): the durable observation, or
    /// `None` when the migration does not exist.
    ///
    /// # Errors
    /// The surface's typed error when the durable store cannot be
    /// read (never a silent `None`).
    fn observe(&self, migration_id: &MigrationId) -> Result<Option<MigrationSummary>, ApiError>;

    /// Abort a migration — **only before the cut** (the total refusal
    /// for cut-or-later states is the coordinator's, surfaced here
    /// typed). The G5-ordered rollback voids every recorded barrier
    /// before anything is resumed, failing closed into terminal
    /// `IN_DOUBT` when the void cannot be confirmed.
    ///
    /// # Errors
    /// `NOT_FOUND` for an unknown migration; `INVALID_STATE` for
    /// every cut-or-later state; the surface's typed error when a
    /// rollback act fails (the record keeps its pre-cut state).
    async fn abort(&self, migration_id: &MigrationId) -> Result<MigrationSummary, ApiError>;
}

/// The typed refusal for a mobility route on a daemon where the
/// migration surface is not enabled: the route exists (the contract
/// is uniform) but the deployment has not opted in — a 404-shaped
/// `NOT_FOUND`, never a silent no-op (plan §6's optional-surface
/// discipline, the same reading as `AdoptionSurface`).
#[must_use]
pub fn migration_not_enabled() -> ApiError {
    ApiError::new(
        ApiErrorCode::NotFound,
        "the mobility surface is not enabled on this deployment",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> MobilityRequest {
        MobilityRequest {
            migration_id: MigrationId::new("mig-1").expect("valid id"),
            vm_id: "vm-1".to_owned(),
            target_host: HostId::new("dst-host").expect("valid id"),
            volume_ids: vec![
                VolumeId::new("vol-a").expect("valid id"),
                VolumeId::new("vol-b").expect("valid id"),
            ],
            expected_generations: vec![3, 4],
        }
    }

    #[test]
    fn mobility_request_validates_shape() {
        assert!(request().validate().is_ok());

        let mut empty_vm = request();
        empty_vm.vm_id.clear();
        assert_eq!(
            empty_vm.validate().expect_err("empty vm").code,
            ApiErrorCode::InvalidRequest
        );

        let mut no_volumes = request();
        no_volumes.volume_ids.clear();
        no_volumes.expected_generations.clear();
        assert_eq!(
            no_volumes.validate().expect_err("no volumes").code,
            ApiErrorCode::InvalidRequest
        );

        let mut mismatched = request();
        mismatched.expected_generations.pop();
        assert!(mismatched.validate().is_err());

        let mut duplicate = request();
        duplicate.volume_ids[1] = duplicate.volume_ids[0].clone();
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn mobility_request_round_trips_and_denies_unknown_fields() {
        let json = serde_json::to_string(&request()).expect("serialize");
        assert_eq!(
            serde_json::from_str::<MobilityRequest>(&json).expect("deserialize"),
            request()
        );
        // Unknown fields are refused, never silently mapped.
        let extended = json.replace("\"vm_id\"", "\"extra\":1,\"vm_id\"");
        assert!(serde_json::from_str::<MobilityRequest>(&extended).is_err());
    }
}
