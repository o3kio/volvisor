//! # Witness registry: the writer-authority core
//!
//! [`WitnessCore`] is the durable epoch/lease registry every witness
//! decision funnels through (P4a plan §2). It owns a
//! [`volvisor_journal::Journal`] and derives **all** state by folding the
//! journaled intents — nothing authority-relevant lives only in memory:
//!
//! - epoch/commit-index monotonicity across crash is W3 (journal replay);
//! - responses are returned only after the outcome record is fsynced
//!   (W3a), and an intent without an outcome is rolled forward at open
//!   from the intent's embedded, byte-identical computed response (W3b);
//! - time is an **explicit parameter** (`now_secs`, witness-clock unix
//!   seconds) on every operation, so every expiry and fence-window
//!   decision is deterministic and unit-testable; the HTTP surface
//!   injects the real clock.
//!
//! Concurrency: the core is single-threaded by construction (the HTTP
//! surface serializes through one mutex, as the journal requires `&mut`).
//! There is no interleaving between the semantic checks and the journaled
//! commit of an operation.

use std::collections::HashMap;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use volvisor_journal::{IntentAppend, Journal, JournalRecord};
use volvisor_types::{
    ApiError, AuthorityView, FencingProof, HostId, LeaseId, LeaseState, OperationId, VolumeId,
    VolumeRegistration, WriterEpoch,
};

use crate::proto::{
    GrantRequest, GrantResponse, RegisterRequest, RegisterResponse, RegistrationContent,
    RenewRequest, RenewResponse, RevokeRequest, RevokeResponse, WitnessError, request_hash,
};

/// Prefix of the journal `op_kind`s owned by this core. A journal
/// directory used by a witness contains only witness records; foreign
/// records are skipped, never interpreted.
const OP_KIND_PREFIX: &str = "witness_";

/// Tuning of the authority core. All three knobs are enforced by the
/// witness (they parameterize W5/W7), so they are witness-side
/// configuration (P4a plan §3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WitnessCoreConfig {
    /// Lease time-to-live in seconds. A writer must renew within this
    /// window or its authority lapses.
    pub lease_ttl_secs: u64,
    /// The response-latency bound the fence window assumes (W5): the only
    /// residual cross-host skew term, since deadlines travel as durations
    /// from responses.
    pub lease_grace_secs: u64,
    /// The assumed budget for a self-fencing writer to suspend I/O
    /// (`drbdsetup suspend-io`), which is fast — demotion of an open
    /// device is *not* budgeted because a suspended device is already
    /// write-frozen.
    pub suspend_budget_secs: u64,
}

impl WitnessCoreConfig {
    /// Validate: every knob must be positive (a zero TTL, grace or budget
    /// would make the fence window vacuous or the lease immortal).
    ///
    /// # Errors
    /// Typed invalid-request error naming the offending knob.
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.lease_ttl_secs == 0 {
            return Err(ApiError::invalid_request(
                "witness lease_ttl_secs must be positive",
            ));
        }
        if self.lease_grace_secs == 0 {
            return Err(ApiError::invalid_request(
                "witness lease_grace_secs must be positive",
            ));
        }
        if self.suspend_budget_secs == 0 {
            return Err(ApiError::invalid_request(
                "witness suspend_budget_secs must be positive",
            ));
        }
        Ok(())
    }

    /// The W7 fence-wait added to a retired lease's recorded end.
    #[must_use]
    pub fn fence_wait_secs(&self) -> u64 {
        self.lease_grace_secs + self.suspend_budget_secs
    }
}

/// One journaled authority mutation. Serialized into the intent payload
/// (inside a [`MutationEnvelope`]); the open-time fold applies these to
/// rebuild state, so the payload is the authoritative record — the
/// in-memory registry is a cache of exactly this fold.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mutation", rename_all = "snake_case", deny_unknown_fields)]
enum Mutation {
    /// First registration of a volume lineage.
    Register {
        /// The registered volume.
        volume_id: VolumeId,
        /// The stamped registration record.
        registration: VolumeRegistration,
    },
    /// Grant of a new writer epoch (W2: retires all older epochs).
    Grant {
        /// The volume.
        volume_id: VolumeId,
        /// The new writer.
        host_id: HostId,
        /// The granted epoch.
        epoch: WriterEpoch,
        /// The granted lease.
        lease_id: LeaseId,
        /// Witness-clock end of the lease (`grant time + ttl`).
        end_secs: u64,
        /// Fencing proof for the retired epochs.
        proof: FencingProof,
    },
    /// Renewal of the current lease (extends from the renewal time).
    Renew {
        /// The volume.
        volume_id: VolumeId,
        /// The renewed lease.
        lease_id: LeaseId,
        /// New witness-clock end (`renewal time + ttl`).
        end_secs: u64,
    },
    /// Revocation of the current lease (forced or self-release).
    Revoke {
        /// The volume.
        volume_id: VolumeId,
        /// Fencing proof for the retirement (carries the retired epoch).
        proof: FencingProof,
        /// Whether the holder released itself (the detach path: the
        /// releasing host demoted first — no W7 wait follows).
        self_released: bool,
        /// Whether a positive power-off attestation shortened the wait.
        power_off_attested: bool,
        /// The W6 authorization record, when one was required/received.
        authorization: Option<crate::proto::RevocationAuthorization>,
    },
}

/// Intent payload: the mutation plus the complete computed response
/// (W3b — the roll-forward outcome is byte-identical to the normal path
/// because it *is* the recorded response).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationEnvelope {
    mutation: Mutation,
    response: serde_json::Value,
}

/// The current lease of a volume, as derived from the journal fold.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LeaseRecord {
    lease_id: LeaseId,
    holder: HostId,
    epoch: WriterEpoch,
    end_secs: u64,
    revoked: bool,
    self_released: bool,
    power_off_attested: bool,
}

/// The authority state of one registered volume.
#[derive(Clone, Debug, PartialEq, Eq)]
struct VolumeAuthority {
    registration: VolumeRegistration,
    /// Highest epoch ever granted (pre-authority 0 until the first
    /// grant). Never decreases (W3).
    current_epoch: WriterEpoch,
    /// Holder of `current_epoch`, if it was ever granted.
    holder: Option<HostId>,
    /// The current (most recent) lease; older leases are superseded by
    /// construction (each grant replaces it).
    lease: Option<LeaseRecord>,
    /// The fencing proof of the latest retirement (grant or revoke) —
    /// kept for state-idempotent self-release retries.
    last_proof: Option<FencingProof>,
}

/// The durable writer-authority registry.
pub struct WitnessCore {
    journal: Journal,
    config: WitnessCoreConfig,
    volumes: HashMap<VolumeId, VolumeAuthority>,
    /// Monotonic count of applied mutations; the `authority_commit_index`
    /// embedded in fencing proofs (contract §1).
    commit_index: u64,
    /// Next lease id to allocate (never reused, monotonic across replay).
    next_lease_id: u64,
    /// Responses of operations whose intent is durable but whose outcome
    /// append failed or is being retried (runtime roll-forward support).
    /// Empty after a successful operation; rebuilt never — startup
    /// roll-forward reads the intents from the journal directly.
    in_flight: HashMap<OperationId, serde_json::Value>,
}

impl WitnessCore {
    /// Open (or create) the registry in `dir`, replaying the journal into
    /// state and rolling forward any intent whose outcome is missing
    /// (W3b).
    ///
    /// # Errors
    /// Typed internal error when the directory/journal cannot be opened,
    /// the config is invalid, or a journaled record cannot be decoded
    /// (corrupt registry state is never best-effort applied).
    pub fn open(dir: impl AsRef<Path>, config: WitnessCoreConfig) -> Result<Self, ApiError> {
        config.validate()?;
        let (journal, records) = Journal::open_with_records(dir)?;
        let mut core = Self {
            journal,
            config,
            volumes: HashMap::new(),
            commit_index: 0,
            next_lease_id: 1,
            in_flight: HashMap::new(),
        };
        for record in records {
            let JournalRecord::Intent(intent) = record else {
                // Outcomes and checkpoints carry no state beyond what
                // their intent already applied.
                continue;
            };
            if !intent.op_kind.starts_with(OP_KIND_PREFIX) {
                continue;
            }
            let envelope: MutationEnvelope = serde_json::from_value(intent.payload.clone())
                .map_err(|err| {
                    ApiError::new(
                        volvisor_types::ApiErrorCode::Internal,
                        format!("corrupt witness journal record: {err}"),
                    )
                })?;
            // The intent is durable: the mutation has happened (W3's
            // "state derives from intents"). Apply it, then make the
            // response replayable if the outcome is missing (W3b).
            core.apply(&envelope.mutation);
            let completed = core
                .journal
                .lookup(&intent.operation_id)
                .is_some_and(|entry| entry.has_outcome);
            if !completed {
                core.journal.append_outcome(
                    intent.operation_id.clone(),
                    true,
                    envelope.response.clone(),
                )?;
            }
        }
        Ok(core)
    }

    /// The witness configuration.
    #[must_use]
    pub fn config(&self) -> &WitnessCoreConfig {
        &self.config
    }

    /// The current commit index (test/diagnostic surface).
    #[must_use]
    pub fn commit_index(&self) -> u64 {
        self.commit_index
    }

    /// Register a volume lineage (P4a plan §3 `register`).
    ///
    /// The witness linearizes all **future** authority for the volume; no
    /// historical claims are made. Re-registering with identical content
    /// is idempotent; different content is a typed conflict.
    ///
    /// # Errors
    /// [`WitnessError::InvalidRequest`] on content validation,
    /// [`WitnessError::AlreadyRegistered`] on a diverging re-registration.
    pub fn register(
        &mut self,
        volume_id: &VolumeId,
        request: &RegisterRequest,
        now_secs: u64,
    ) -> Result<RegisterResponse, WitnessError> {
        validate_content(&request.content)?;
        // Operation-id idempotency first: a journaled retry replays the
        // recorded response even if the registry state has since moved on
        // (strict replay before content comparison).
        let hash = request_hash("register", &request.operation_id, request);
        if let Some(replayed) = self.replay(&request.operation_id, hash)? {
            return Ok(replayed);
        }
        // Content-idempotent re-registration (any operation id): the
        // registry state is identical, so the answer is.
        if let Some(vol) = self.volumes.get(volume_id) {
            if registration_content(&vol.registration) == request.content {
                return Ok(RegisterResponse {
                    volume_id: volume_id.clone(),
                    current_epoch: vol.current_epoch,
                });
            }
            return Err(WitnessError::AlreadyRegistered);
        }
        let registration = VolumeRegistration {
            volume_id: volume_id.clone(),
            lineage_uuids: request.content.lineage_uuids.clone(),
            endpoints: request.content.endpoints.clone(),
            barrier: request.content.barrier.clone(),
            registered_at: now_secs,
        };
        let response = RegisterResponse {
            volume_id: volume_id.clone(),
            current_epoch: WriterEpoch::pre_authority(),
        };
        let mutation = Mutation::Register {
            volume_id: volume_id.clone(),
            registration,
        };
        self.commit(
            &request.operation_id,
            hash,
            "witness_register",
            &mutation,
            &response,
        )?;
        Ok(response)
    }

    /// Grant writer authority for a volume (P4a plan §3 `grant`).
    ///
    /// W1: a live lease blocks the grant. W7: a retired-but-unfenced
    /// lease delays it until `recorded end + grace + suspend budget`,
    /// unless it was self-released or positively attested powered off.
    /// W2: the grant durably retires all older epochs before the
    /// response — the returned proof is that retirement.
    ///
    /// # Errors
    /// [`WitnessError::UnknownVolume`], [`WitnessError::LeaseHeld`],
    /// [`WitnessError::FencePending`] — all fail-closed refusals.
    pub fn grant(
        &mut self,
        volume_id: &VolumeId,
        request: &GrantRequest,
        now_secs: u64,
    ) -> Result<GrantResponse, WitnessError> {
        let hash = request_hash("grant", &request.operation_id, request);
        if let Some(replayed) = self.replay(&request.operation_id, hash)? {
            return Ok(replayed);
        }
        let vol = self
            .volumes
            .get(volume_id)
            .ok_or(WitnessError::UnknownVolume)?;
        if let Some(lease) = &vol.lease {
            // W1: exactly one live lease per volume. The current holder
            // re-granting while live is a client bug (it should renew),
            // refused the same way.
            if !lease.revoked && now_secs < lease.end_secs {
                return Err(WitnessError::LeaseHeld {
                    current_epoch: vol.current_epoch,
                });
            }
            // W7: wait out the fence window keyed on the lease's
            // recorded end. The same host re-acquiring after its own
            // lapse also waits: the previous process may not have
            // finished suspending.
            if !lease.self_released && !lease.power_off_attested {
                let fence_until = lease.end_secs.saturating_add(self.config.fence_wait_secs());
                if now_secs < fence_until {
                    return Err(WitnessError::FencePending {
                        retry_after_secs: fence_until - now_secs,
                    });
                }
            }
        }
        let epoch = WriterEpoch(vol.current_epoch.0 + 1);
        let lease_id = LeaseId(self.next_lease_id);
        let proof = FencingProof {
            volume_id: volume_id.clone(),
            retired_epoch: vol.current_epoch,
            commit_index: self.commit_index + 1,
        };
        let response = GrantResponse {
            epoch,
            lease_id,
            lease_ttl_secs: self.config.lease_ttl_secs,
            fencing_proof: proof.clone(),
        };
        let mutation = Mutation::Grant {
            volume_id: volume_id.clone(),
            host_id: request.host_id.clone(),
            epoch,
            lease_id,
            end_secs: now_secs.saturating_add(self.config.lease_ttl_secs),
            proof,
        };
        self.commit(
            &request.operation_id,
            hash,
            "witness_grant",
            &mutation,
            &response,
        )?;
        Ok(response)
    }

    /// Renew the current lease (P4a plan §3 `renew`).
    ///
    /// W4: any mismatch (retired epoch, wrong lease, expiry, revocation)
    /// is a typed `STALE_EPOCH` carrying the current epoch — the writer
    /// *learns* it is fenced. A wrong holder receives `LEASE_HELD`. The
    /// response carries the remaining duration **from this response**
    /// (W5): renewal extends from the renewal time.
    ///
    /// # Errors
    /// [`WitnessError::StaleEpoch`] / [`WitnessError::LeaseHeld`] /
    /// [`WitnessError::UnknownVolume`].
    pub fn renew(
        &mut self,
        volume_id: &VolumeId,
        request: &RenewRequest,
        now_secs: u64,
    ) -> Result<RenewResponse, WitnessError> {
        let hash = request_hash("renew", &request.operation_id, request);
        if let Some(replayed) = self.replay(&request.operation_id, hash)? {
            return Ok(replayed);
        }
        let vol = self
            .volumes
            .get(volume_id)
            .ok_or(WitnessError::UnknownVolume)?;
        let lease = vol.lease.as_ref().ok_or(WitnessError::StaleEpoch {
            current_epoch: vol.current_epoch,
        })?;
        // Everything that is not exactly "the live current lease held by
        // this host" teaches the writer it is fenced (W4).
        if request.epoch != vol.current_epoch
            || lease.revoked
            || lease.lease_id != request.lease_id
            || now_secs >= lease.end_secs
        {
            return Err(WitnessError::StaleEpoch {
                current_epoch: vol.current_epoch,
            });
        }
        if lease.holder != request.host_id {
            return Err(WitnessError::LeaseHeld {
                current_epoch: vol.current_epoch,
            });
        }
        let end_secs = now_secs.saturating_add(self.config.lease_ttl_secs);
        let response = RenewResponse {
            epoch: vol.current_epoch,
            remaining_secs: self.config.lease_ttl_secs,
        };
        let mutation = Mutation::Renew {
            volume_id: volume_id.clone(),
            lease_id: lease.lease_id,
            end_secs,
        };
        self.commit(
            &request.operation_id,
            hash,
            "witness_renew",
            &mutation,
            &response,
        )?;
        Ok(response)
    }

    /// Revoke the current lease (P4a plan §3 `revoke`).
    ///
    /// W6: revoking a **live** lease held by another host requires a
    /// recorded operator authorization. The holder self-releasing (the
    /// detach path — the releasing host demoted itself first) needs no
    /// authorization and starts **no** W7 wait. A positive power-off
    /// attestation shortens a forced revocation's wait to zero; a bare
    /// authorization never shortens it.
    ///
    /// # Errors
    /// [`WitnessError::StaleEpoch`] on epoch/lease mismatch,
    /// [`WitnessError::InvalidRequest`] when a required authorization or
    /// evidence ground is missing or empty.
    pub fn revoke(
        &mut self,
        volume_id: &VolumeId,
        request: &RevokeRequest,
        now_secs: u64,
    ) -> Result<RevokeResponse, WitnessError> {
        let hash = request_hash("revoke", &request.operation_id, request);
        if let Some(replayed) = self.replay(&request.operation_id, hash)? {
            return Ok(replayed);
        }
        let vol = self
            .volumes
            .get(volume_id)
            .ok_or(WitnessError::UnknownVolume)?;
        let Some(lease) = vol.lease.as_ref() else {
            return Err(WitnessError::StaleEpoch {
                current_epoch: vol.current_epoch,
            });
        };
        if request.epoch != vol.current_epoch {
            return Err(WitnessError::StaleEpoch {
                current_epoch: vol.current_epoch,
            });
        }
        // State-idempotent self-release retry under a fresh operation id:
        // the goal state (this lease released by its holder) already
        // holds, so confirm it with the recorded proof.
        if lease.revoked {
            if lease.self_released && lease.holder == request.host_id {
                let proof = vol.last_proof.clone().ok_or_else(|| {
                    WitnessError::Internal(
                        "recorded self-release is missing its fencing proof".to_owned(),
                    )
                })?;
                return Ok(RevokeResponse {
                    fencing_proof: proof,
                });
            }
            return Err(WitnessError::StaleEpoch {
                current_epoch: vol.current_epoch,
            });
        }
        let live = now_secs < lease.end_secs;
        let self_release = lease.holder == request.host_id;
        if live && !self_release {
            // W6: forced revocation of a live lease — recorded
            // authorization, never silent, never inferred.
            let Some(auth) = &request.authorization else {
                return Err(WitnessError::InvalidRequest(
                    "revoking a live lease held by another host requires operator \
                     authorization (operator + reason)"
                        .to_owned(),
                ));
            };
            if auth.operator.trim().is_empty() || auth.reason.trim().is_empty() {
                return Err(WitnessError::InvalidRequest(
                    "revocation authorization requires a non-empty operator and reason".to_owned(),
                ));
            }
        }
        // Power-off evidence ground (P4a plan §7): only positive
        // confirmation shortens the W7 wait.
        let power_off_attested = request
            .power_off
            .as_ref()
            .is_some_and(|attestation| !attestation.evidence.trim().is_empty());
        if request.power_off.is_some() && !power_off_attested {
            return Err(WitnessError::InvalidRequest(
                "power-off attestation requires positive evidence \
                 (fence-device/BMC confirmation)"
                    .to_owned(),
            ));
        }
        let proof = FencingProof {
            volume_id: volume_id.clone(),
            retired_epoch: vol.current_epoch,
            commit_index: self.commit_index + 1,
        };
        let response = RevokeResponse {
            fencing_proof: proof.clone(),
        };
        let mutation = Mutation::Revoke {
            volume_id: volume_id.clone(),
            proof,
            self_released: self_release,
            power_off_attested,
            authorization: request.authorization.clone(),
        };
        self.commit(
            &request.operation_id,
            hash,
            "witness_revoke",
            &mutation,
            &response,
        )?;
        Ok(response)
    }

    /// Read the authority view of a volume (P4a plan §3 `inspect`).
    ///
    /// Read-only: never journals, never mutates. Lease state is evaluated
    /// lazily against `now_secs` (witness-clock truth); a live lease's
    /// remaining duration is a **duration-from-response** (W5).
    ///
    /// # Errors
    /// [`WitnessError::UnknownVolume`] when the volume is not registered.
    pub fn inspect(
        &self,
        volume_id: &VolumeId,
        now_secs: u64,
    ) -> Result<AuthorityView, WitnessError> {
        let vol = self
            .volumes
            .get(volume_id)
            .ok_or(WitnessError::UnknownVolume)?;
        let (lease_state, lease_remaining_secs) = match &vol.lease {
            None => (LeaseState::None, None),
            Some(lease) if lease.revoked => (LeaseState::Revoked, None),
            Some(lease) if now_secs < lease.end_secs => {
                (LeaseState::Live, Some(lease.end_secs - now_secs))
            }
            Some(_) => (LeaseState::Expired, None),
        };
        Ok(AuthorityView {
            volume_id: volume_id.clone(),
            current_epoch: vol.current_epoch,
            holder: vol.holder.clone(),
            lease_state,
            lease_remaining_secs,
            commit_index: self.commit_index,
            registration: Some(vol.registration.clone()),
        })
    }

    /// Replay path shared by every mutating operation: resolve an
    /// operation the journal already knows (idempotent retry, hash
    /// conflict, or in-flight completion).
    fn replay<T: DeserializeOwned>(
        &mut self,
        operation_id: &OperationId,
        request_hash: [u8; 32],
    ) -> Result<Option<T>, WitnessError> {
        let Some(entry) = self.journal.lookup(operation_id) else {
            return Ok(None);
        };
        if entry.request_hash != request_hash {
            return Err(WitnessError::IdempotencyConflict);
        }
        if let Some(outcome) = entry.outcome {
            if !outcome.success {
                // This core journals outcomes only for successes; a
                // failure outcome is foreign state, refused rather than
                // reinterpreted.
                return Err(WitnessError::Internal(
                    "recorded outcome is a failure this core never writes".to_owned(),
                ));
            }
            let response = serde_json::from_value(outcome.response).map_err(|err| {
                WitnessError::Internal(format!("corrupt recorded outcome: {err}"))
            })?;
            return Ok(Some(response));
        }
        // Intent durable, outcome missing: complete it now from the
        // recorded envelope (runtime half of W3b; the startup half reads
        // the intents from the journal).
        let Some(response) = self.in_flight.remove(operation_id) else {
            return Err(WitnessError::Internal(
                "operation is in flight without a completable response".to_owned(),
            ));
        };
        self.journal
            .append_outcome(operation_id.clone(), true, response.clone())?;
        let decoded = serde_json::from_value(response)
            .map_err(|err| WitnessError::Internal(format!("corrupt in-flight response: {err}")))?;
        Ok(Some(decoded))
    }

    /// Journal one mutation, apply it, and record its outcome (W3a:
    /// the caller may only learn the response after the outcome is
    /// durable). If the outcome append fails, the in-flight response is
    /// retained for a retry to complete (W3b).
    fn commit(
        &mut self,
        operation_id: &OperationId,
        request_hash: [u8; 32],
        op_kind: &str,
        mutation: &Mutation,
        response: &impl Serialize,
    ) -> Result<(), WitnessError> {
        let response_value = serde_json::to_value(response).map_err(|err| {
            WitnessError::Internal(format!("failed to serialize witness response: {err}"))
        })?;
        let envelope = MutationEnvelope {
            mutation: mutation.clone(),
            response: response_value.clone(),
        };
        let payload = serde_json::to_value(&envelope).map_err(|err| {
            WitnessError::Internal(format!("failed to serialize witness mutation: {err}"))
        })?;
        match self
            .journal
            .append_intent(operation_id.clone(), request_hash, op_kind, payload)?
        {
            IntentAppend::New => {
                // The intent is durable: the mutation has happened.
                self.apply(mutation);
            }
            // Unreachable in practice — the replay prelude resolved any
            // known operation before commit — but handled fail-closed
            // rather than assumed.
            IntentAppend::Replayed { .. } | IntentAppend::AlreadyInFlight => {
                return Err(WitnessError::Internal(
                    "journal reported a known operation the replay prelude missed".to_owned(),
                ));
            }
        }
        self.in_flight.insert(operation_id.clone(), response_value);
        match self
            .journal
            .append_outcome(operation_id.clone(), true, envelope.response)
        {
            Ok(()) => {
                self.in_flight.remove(operation_id);
                Ok(())
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Fold one journaled mutation into the derived state (the replay
    /// path of every `apply` the live operations performed).
    fn apply(&mut self, mutation: &Mutation) {
        match mutation {
            Mutation::Register {
                volume_id,
                registration,
            } => {
                self.volumes.insert(
                    volume_id.clone(),
                    VolumeAuthority {
                        registration: registration.clone(),
                        current_epoch: WriterEpoch::pre_authority(),
                        holder: None,
                        lease: None,
                        last_proof: None,
                    },
                );
            }
            Mutation::Grant {
                volume_id,
                host_id,
                epoch,
                lease_id,
                end_secs,
                proof,
            } => {
                if let Some(vol) = self.volumes.get_mut(volume_id) {
                    vol.current_epoch = *epoch;
                    vol.holder = Some(host_id.clone());
                    vol.lease = Some(LeaseRecord {
                        lease_id: *lease_id,
                        holder: host_id.clone(),
                        epoch: *epoch,
                        end_secs: *end_secs,
                        revoked: false,
                        self_released: false,
                        power_off_attested: false,
                    });
                    vol.last_proof = Some(proof.clone());
                }
                self.next_lease_id = self.next_lease_id.max(lease_id.0 + 1);
            }
            Mutation::Renew {
                volume_id,
                lease_id,
                end_secs,
            } => {
                if let Some(vol) = self.volumes.get_mut(volume_id) {
                    if let Some(lease) = vol.lease.as_mut() {
                        if lease.lease_id == *lease_id {
                            lease.end_secs = *end_secs;
                        }
                    }
                }
            }
            Mutation::Revoke {
                volume_id,
                proof,
                self_released,
                power_off_attested,
                ..
            } => {
                if let Some(vol) = self.volumes.get_mut(volume_id) {
                    if let Some(lease) = vol.lease.as_mut() {
                        lease.revoked = true;
                        lease.self_released = *self_released;
                        lease.power_off_attested = *power_off_attested;
                    }
                    vol.last_proof = Some(proof.clone());
                }
            }
        }
        self.commit_index += 1;
    }
}

/// Extract the client-supplied content of a registration record (for
/// idempotent re-registration comparison).
fn registration_content(registration: &VolumeRegistration) -> RegistrationContent {
    RegistrationContent {
        lineage_uuids: registration.lineage_uuids.clone(),
        endpoints: registration.endpoints.clone(),
        barrier: registration.barrier.clone(),
    }
}

/// Validate registration content (P4a plan §3): non-empty sorted deduped
/// lineage set, exactly two endpoints on different hosts, a barrier
/// attestation that is non-empty when present.
fn validate_content(content: &RegistrationContent) -> Result<(), WitnessError> {
    if content.lineage_uuids.is_empty() {
        return Err(WitnessError::InvalidRequest(
            "registration requires a non-empty lineage identifier set".to_owned(),
        ));
    }
    let mut sorted = content.lineage_uuids.clone();
    sorted.sort();
    sorted.dedup();
    if sorted != content.lineage_uuids {
        return Err(WitnessError::InvalidRequest(
            "lineage identifiers must be sorted and deduplicated".to_owned(),
        ));
    }
    if content.endpoints.len() != 2 {
        return Err(WitnessError::InvalidRequest(
            "registration requires exactly two endpoints".to_owned(),
        ));
    }
    if content.endpoints[0].host_id == content.endpoints[1].host_id {
        return Err(WitnessError::InvalidRequest(
            "the two endpoints must be on different hosts (a same-host replica \
             is not an independent failure domain)"
                .to_owned(),
        ));
    }
    if let Some(barrier) = &content.barrier {
        if barrier.boundary.trim().is_empty() || barrier.attestation.trim().is_empty() {
            return Err(WitnessError::InvalidRequest(
                "a recorded barrier requires a non-empty boundary and attestation".to_owned(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{PowerOffAttestation, RevocationAuthorization, WITNESS_PROTOCOL_VERSION};
    use volvisor_types::EndpointBacking;

    /// Deterministic test knobs: ttl 100s, grace 5s, suspend budget 5s —
    /// the W7 wait is 10s past a lease's recorded end.
    fn test_config() -> WitnessCoreConfig {
        WitnessCoreConfig {
            lease_ttl_secs: 100,
            lease_grace_secs: 5,
            suspend_budget_secs: 5,
        }
    }

    fn volume(n: u64) -> VolumeId {
        VolumeId::new(format!("vol-{n}")).expect("valid volume id")
    }

    fn host(n: u64) -> HostId {
        HostId::new(format!("node-{n}")).expect("valid host id")
    }

    fn op(n: u64) -> OperationId {
        OperationId::new(format!("op-{n}")).expect("valid operation id")
    }

    fn endpoints() -> Vec<EndpointBacking> {
        vec![
            EndpointBacking {
                host_id: host(1),
                backing: "vg-near/vol-abc-00000001".to_owned(),
                volvisor_created: true,
            },
            EndpointBacking {
                host_id: host(2),
                backing: "vg-near/vol-abc-00000001".to_owned(),
                volvisor_created: false,
            },
        ]
    }

    fn content() -> RegistrationContent {
        RegistrationContent {
            lineage_uuids: vec!["0000000000000004".to_owned(), "0000000000000005".to_owned()],
            endpoints: endpoints(),
            barrier: None,
        }
    }

    fn register_request(n: u64) -> RegisterRequest {
        RegisterRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op(n),
            content: content(),
        }
    }

    fn grant_request(n: u64, host_n: u64) -> GrantRequest {
        GrantRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op(n),
            host_id: host(host_n),
        }
    }

    fn renew_request(n: u64, host_n: u64, epoch: WriterEpoch, lease_id: LeaseId) -> RenewRequest {
        RenewRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op(n),
            host_id: host(host_n),
            epoch,
            lease_id,
        }
    }

    fn revoke_request(n: u64, host_n: u64, epoch: WriterEpoch) -> RevokeRequest {
        RevokeRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op(n),
            host_id: host(host_n),
            epoch,
            authorization: None,
            power_off: None,
        }
    }

    fn open_core(dir: &std::path::Path) -> WitnessCore {
        WitnessCore::open(dir, test_config()).expect("witness core opens")
    }

    fn registered(dir: &std::path::Path) -> WitnessCore {
        let mut core = open_core(dir);
        core.register(&volume(1), &register_request(1), 1_000)
            .expect("register");
        core
    }

    fn granted(dir: &std::path::Path, at: u64) -> (WitnessCore, GrantResponse) {
        let mut core = registered(dir);
        let response = core
            .grant(&volume(1), &grant_request(2, 1), at)
            .expect("grant");
        (core, response)
    }

    #[test]
    fn w1_single_live_lease_blocks_competing_grants() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut core, _grant) = granted(dir.path(), 1_000);
        // Live for another host.
        let err = core
            .grant(&volume(1), &grant_request(3, 2), 1_050)
            .expect_err("competing grant refused");
        assert_eq!(
            err,
            WitnessError::LeaseHeld {
                current_epoch: WriterEpoch(1)
            }
        );
        // Live for the same host too: the holder must renew, not re-grant.
        let err = core
            .grant(&volume(1), &grant_request(4, 1), 1_050)
            .expect_err("re-grant refused");
        assert_eq!(
            err,
            WitnessError::LeaseHeld {
                current_epoch: WriterEpoch(1)
            }
        );
    }

    #[test]
    fn w2_grant_retires_the_past_before_the_response() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (core, grant) = granted(dir.path(), 1_000);
        let proof = &grant.fencing_proof;
        assert_eq!(proof.retired_epoch, WriterEpoch::pre_authority());
        // The proof names a commit index that exists only because the
        // retirement was journaled: it is the grant's own index.
        assert_eq!(proof.commit_index, core.commit_index());
        assert!(proof.commit_index > 0);
        // The granted response carries the TTL as a duration (W5 shape),
        // never an absolute deadline.
        assert_eq!(grant.lease_ttl_secs, 100);
    }

    #[test]
    fn w3_state_and_monotonicity_survive_crash_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (core, grant) = granted(dir.path(), 1_000);
        let commit = core.commit_index();
        drop(core);
        // "Crash": reopen on the same directory.
        let mut core = open_core(dir.path());
        let view = core.inspect(&volume(1), 1_050).expect("inspect");
        assert_eq!(view.current_epoch, WriterEpoch(1));
        assert_eq!(view.lease_state, LeaseState::Live);
        assert_eq!(view.commit_index, commit);
        // Epochs never shrink and lease ids are never reused.
        core.revoke(&volume(1), &revoke_request(5, 1, grant.epoch), 1_050)
            .expect("self-release after restart");
        let second = core
            .grant(&volume(1), &grant_request(6, 2), 1_060)
            .expect("grant after self-release (no W7 wait)");
        assert_eq!(second.epoch, WriterEpoch(2));
        assert_ne!(second.lease_id, grant.lease_id);
    }

    #[test]
    fn w3b_roll_forward_completes_intents_without_outcomes() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The orphaned grant's intent must carry the request hash a
        // retry with the same operation id computes, or the replay
        // prelude would (correctly) report a conflict instead of
        // completing it.
        let orphan_hash = request_hash("grant", &op(91), &grant_request(91, 1));
        // Build an orphaned intent directly in the journal: a grant whose
        // outcome was never appended (crash between append and response).
        {
            let mut journal = Journal::open(dir.path()).expect("journal opens");
            let envelope = MutationEnvelope {
                mutation: Mutation::Grant {
                    volume_id: volume(1),
                    host_id: host(1),
                    epoch: WriterEpoch(1),
                    lease_id: LeaseId(1),
                    end_secs: 1_100,
                    proof: FencingProof {
                        volume_id: volume(1),
                        retired_epoch: WriterEpoch::pre_authority(),
                        commit_index: 1,
                    },
                },
                response: serde_json::json!({
                    "epoch": 1,
                    "lease_id": 1,
                    "lease_ttl_secs": 100,
                    "fencing_proof": {
                        "volume_id": "vol-1",
                        "retired_epoch": 0,
                        "commit_index": 1,
                    }
                }),
            };
            let registration = Mutation::Register {
                volume_id: volume(1),
                registration: VolumeRegistration {
                    volume_id: volume(1),
                    lineage_uuids: content().lineage_uuids,
                    endpoints: endpoints(),
                    barrier: None,
                    registered_at: 1_000,
                },
            };
            // Registration first (with outcome), then the orphan grant.
            let reg_env = MutationEnvelope {
                mutation: registration,
                response: serde_json::json!({"volume_id": "vol-1", "current_epoch": 0}),
            };
            journal
                .append_intent(
                    op(90),
                    [1; 32],
                    "witness_register",
                    serde_json::to_value(&reg_env).expect("serialize"),
                )
                .expect("register intent");
            journal
                .append_outcome(op(90), true, reg_env.response)
                .expect("register outcome");
            journal
                .append_intent(
                    op(91),
                    orphan_hash,
                    "witness_grant",
                    serde_json::to_value(&envelope).expect("serialize"),
                )
                .expect("orphan grant intent");
        }
        // Reopen: the fold applies the orphan grant and appends its
        // outcome; a retry with the same operation id replays it, and a
        // retry with a fresh id is not blocked by an orphan lease.
        let mut core = open_core(dir.path());
        let view = core.inspect(&volume(1), 1_050).expect("inspect");
        assert_eq!(view.current_epoch, WriterEpoch(1));
        assert_eq!(view.lease_state, LeaseState::Live);
        // Same operation id, matching request hash: replays the outcome.
        let replayed = core
            .grant(&volume(1), &grant_request(91, 1), 1_060)
            .expect("retry completes the rolled-forward grant");
        assert_eq!(replayed.epoch, WriterEpoch(1));
        // Fresh operation id: blocked by the (now durable) live lease —
        // W1, not a wedge.
        let err = core
            .grant(&volume(1), &grant_request(92, 2), 1_060)
            .expect_err("fresh grant sees the rolled-forward state");
        assert_eq!(
            err,
            WitnessError::LeaseHeld {
                current_epoch: WriterEpoch(1)
            }
        );
    }

    #[test]
    fn w4_stale_renewal_carries_the_current_epoch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut core, grant) = granted(dir.path(), 1_000);
        // Renew with the wrong epoch.
        let err = core
            .renew(
                &volume(1),
                &renew_request(5, 1, WriterEpoch(7), grant.lease_id),
                1_050,
            )
            .expect_err("stale epoch refused");
        assert_eq!(
            err,
            WitnessError::StaleEpoch {
                current_epoch: WriterEpoch(1)
            }
        );
        // Renew after expiry: the writer learns it is fenced.
        let err = core
            .renew(
                &volume(1),
                &renew_request(6, 1, grant.epoch, grant.lease_id),
                1_100,
            )
            .expect_err("expired renewal refused");
        assert_eq!(
            err,
            WitnessError::StaleEpoch {
                current_epoch: WriterEpoch(1)
            }
        );
        // A correct renewal returns a duration, never a timestamp (W5
        // shape) and extends from the renewal time.
        let renewed = core
            .renew(
                &volume(1),
                &renew_request(7, 1, grant.epoch, grant.lease_id),
                1_050,
            )
            .expect("renewal");
        assert_eq!(renewed.remaining_secs, 100);
        // Wrong holder.
        let err = core
            .renew(
                &volume(1),
                &renew_request(8, 2, grant.epoch, grant.lease_id),
                1_060,
            )
            .expect_err("wrong holder refused");
        assert_eq!(
            err,
            WitnessError::LeaseHeld {
                current_epoch: WriterEpoch(1)
            }
        );
    }

    #[test]
    fn w7_wait_keys_on_the_lease_recorded_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut core, _grant) = granted(dir.path(), 1_000);
        // Lease end = 1_100 (1_000 + ttl 100). Fence window ends at
        // 1_100 + 10. Expiry alone does not clear it.
        let err = core
            .grant(&volume(1), &grant_request(3, 2), 1_105)
            .expect_err("fence window holds after expiry");
        assert_eq!(
            err,
            WitnessError::FencePending {
                retry_after_secs: 5
            }
        );
        // The same window applies to a forced revocation of a live lease
        // held by a partitioned writer: revoking at 1_050 (mid-lease)
        // still waits until the *recorded end* 1_100 + 10, because the
        // partitioned writer serves until its local deadline no matter
        // how often it fails to renew.
        let mut forced = revoke_request(4, 2, WriterEpoch(1));
        forced.authorization = Some(RevocationAuthorization {
            operator: "op@example".to_owned(),
            reason: "host-1 partitioned; forced failover".to_owned(),
        });
        let proof = core
            .revoke(&volume(1), &forced, 1_050)
            .expect("forced revocation journaled");
        assert_eq!(proof.fencing_proof.retired_epoch, WriterEpoch(1));
        // Fence window: recorded end 1_100 + grace 5 + budget 5 = 1_110.
        let err = core
            .grant(&volume(1), &grant_request(5, 2), 1_109)
            .expect_err("fence window keys on the recorded end, not the revoke time");
        assert_eq!(
            err,
            WitnessError::FencePending {
                retry_after_secs: 1
            }
        );
        let granted_late = core
            .grant(&volume(1), &grant_request(6, 2), 1_110)
            .expect("grant succeeds once the window has passed");
        assert_eq!(granted_late.epoch, WriterEpoch(2));
        // GrantResponse carries the TTL (duration-from-response), never
        // an absolute deadline.
        assert_eq!(granted_late.lease_ttl_secs, 100);
    }

    #[test]
    fn w6_forced_revocation_requires_recorded_authorization() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut core, grant) = granted(dir.path(), 1_000);
        // Live lease, another host, no authorization: refused.
        let err = core
            .revoke(&volume(1), &revoke_request(3, 2, grant.epoch), 1_050)
            .expect_err("forced revocation without authorization refused");
        assert!(matches!(err, WitnessError::InvalidRequest(_)));
        // Empty authorization fields: refused.
        let mut empty = revoke_request(4, 2, grant.epoch);
        empty.authorization = Some(RevocationAuthorization {
            operator: "  ".to_owned(),
            reason: "forced failover".to_owned(),
        });
        let err = core
            .revoke(&volume(1), &empty, 1_050)
            .expect_err("empty authorization refused");
        assert!(matches!(err, WitnessError::InvalidRequest(_)));
        // Positive power-off evidence shortens the wait to zero.
        let mut forced = revoke_request(5, 2, grant.epoch);
        forced.authorization = Some(RevocationAuthorization {
            operator: "op@example".to_owned(),
            reason: "host-1 powered off via BMC".to_owned(),
        });
        forced.power_off = Some(PowerOffAttestation {
            evidence: "bmc: chassis power off confirmed at 2026-10-09T12:00:00Z".to_owned(),
        });
        core.revoke(&volume(1), &forced, 1_050)
            .expect("power-off-attested revocation");
        core.grant(&volume(1), &grant_request(6, 2), 1_050)
            .expect("no fence wait after a positive power-off attestation");
        // A bare authorization without positive power-off evidence never
        // shortens (the wait stands, keyed on the recorded end). Fresh
        // registry: the first one still holds the journal flock.
        let bare_dir = tempfile::tempdir().expect("tempdir");
        let (mut core2, grant2) = granted(bare_dir.path(), 1_000);
        let mut bare = revoke_request(7, 2, grant2.epoch);
        bare.authorization = Some(RevocationAuthorization {
            operator: "op@example".to_owned(),
            reason: "forced failover".to_owned(),
        });
        core2
            .revoke(&volume(1), &bare, 1_050)
            .expect("bare forced revocation journaled");
        let err = core2
            .grant(&volume(1), &grant_request(8, 2), 1_050)
            .expect_err("bare authorization does not shorten the wait");
        assert_eq!(
            err,
            WitnessError::FencePending {
                retry_after_secs: 60
            }
        );
    }

    #[test]
    fn self_release_starts_no_wait_and_is_state_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut core, grant) = granted(dir.path(), 1_000);
        // The holder self-releases (detach path): no authorization, no
        // wait — the releasing host demoted itself.
        core.revoke(&volume(1), &revoke_request(3, 1, grant.epoch), 1_050)
            .expect("self-release");
        let immediate = core
            .grant(&volume(1), &grant_request(4, 2), 1_050)
            .expect("grant immediately after self-release");
        assert_eq!(immediate.epoch, WriterEpoch(2));
        // State-idempotent self-release retry under a fresh operation id
        // after the lease was already replaced: epoch mismatch teaches
        // the stale caller it is fenced.
        let err = core
            .revoke(&volume(1), &revoke_request(5, 1, grant.epoch), 1_051)
            .expect_err("old-epoch self-release retry is stale");
        assert_eq!(
            err,
            WitnessError::StaleEpoch {
                current_epoch: WriterEpoch(2)
            }
        );
        // The current holder's self-release retry with a fresh id
        // confirms idempotently.
        let again = core
            .revoke(&volume(1), &revoke_request(6, 2, immediate.epoch), 1_051)
            .expect("self-release");
        let retry = core
            .revoke(&volume(1), &revoke_request(7, 2, immediate.epoch), 1_052)
            .expect("self-release retry is state-idempotent");
        assert_eq!(again.fencing_proof, retry.fencing_proof);
    }

    #[test]
    fn register_validates_and_is_idempotent_by_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut core = open_core(dir.path());
        // Unsorted lineage set.
        let mut unsorted = register_request(1);
        unsorted.content.lineage_uuids =
            vec!["0000000000000005".to_owned(), "0000000000000004".to_owned()];
        assert!(matches!(
            core.register(&volume(1), &unsorted, 1_000),
            Err(WitnessError::InvalidRequest(_))
        ));
        // One endpoint.
        let mut one = register_request(2);
        one.content.endpoints.truncate(1);
        assert!(matches!(
            core.register(&volume(1), &one, 1_000),
            Err(WitnessError::InvalidRequest(_))
        ));
        // Same-host endpoints.
        let mut same = register_request(3);
        same.content.endpoints[1].host_id = host(1);
        assert!(matches!(
            core.register(&volume(1), &same, 1_000),
            Err(WitnessError::InvalidRequest(_))
        ));
        // Successful registration, then an identical re-registration
        // under a different operation id: idempotent.
        core.register(&volume(1), &register_request(4), 1_000)
            .expect("register");
        core.register(&volume(1), &register_request(5), 2_000)
            .expect("identical re-register is idempotent");
        // Diverging content: typed conflict.
        let mut diverging = register_request(6);
        diverging.content.lineage_uuids = vec!["0000000000000ABC".to_owned()];
        assert_eq!(
            core.register(&volume(1), &diverging, 2_000)
                .expect_err("diverging re-register refused"),
            WitnessError::AlreadyRegistered
        );
        // Unregistered volumes are unknown, never guessed.
        assert_eq!(
            core.inspect(&volume(2), 2_000).expect_err("unknown volume"),
            WitnessError::UnknownVolume
        );
        // Grant on an unregistered volume: typed refusal.
        assert_eq!(
            core.grant(&volume(2), &grant_request(7, 1), 2_000)
                .expect_err("unregistered grant refused"),
            WitnessError::UnknownVolume
        );
    }

    #[test]
    fn grant_idempotency_replays_and_conflicts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut core = registered(dir.path());
        let first = core
            .grant(&volume(1), &grant_request(2, 1), 1_000)
            .expect("grant");
        // Same operation id + same request: byte-identical replay.
        let replay = core
            .grant(&volume(1), &grant_request(2, 1), 1_050)
            .expect("replay");
        assert_eq!(first, replay);
        // Same operation id + different request: typed conflict.
        let conflicting = core
            .grant(&volume(1), &grant_request(2, 2), 1_050)
            .expect_err("hash conflict");
        assert_eq!(conflicting, WitnessError::IdempotencyConflict);
    }

    #[test]
    fn inspect_reports_lazy_lease_states_and_durations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (core, _grant) = granted(dir.path(), 1_000);
        // Live with remaining duration (1_100 - 1_050).
        let view = core.inspect(&volume(1), 1_050).expect("inspect");
        assert_eq!(view.lease_state, LeaseState::Live);
        assert_eq!(view.lease_remaining_secs, Some(50));
        assert_eq!(view.holder, Some(host(1)));
        assert_eq!(
            view.registration
                .expect("registration present")
                .lineage_uuids,
            content().lineage_uuids
        );
        // Expired (lazily evaluated).
        let view = core.inspect(&volume(1), 1_100).expect("inspect");
        assert_eq!(view.lease_state, LeaseState::Expired);
        assert_eq!(view.lease_remaining_secs, None);
        // Never-granted volume: no lease at all. Fresh registry: the
        // first one still holds the journal flock.
        let ungranted_dir = tempfile::tempdir().expect("tempdir");
        let fresh = registered(ungranted_dir.path());
        let view = fresh.inspect(&volume(1), 1_000).expect("inspect");
        assert_eq!(view.lease_state, LeaseState::None);
        assert_eq!(view.current_epoch, WriterEpoch::pre_authority());
    }

    #[test]
    fn outcome_append_failure_completes_on_retry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut core = registered(dir.path());
        // Fail the second append (the outcome) of the next operation.
        core.journal.inject_append_failures_after(1);
        let err = core
            .grant(&volume(1), &grant_request(2, 1), 1_000)
            .expect_err("outcome append fails after the intent is durable");
        assert!(matches!(err, WitnessError::Internal(_)));
        // Lift the injected fault: the retry completes the durable
        // intent's outcome (runtime W3b).
        core.journal.inject_append_failures_after(u64::MAX);
        let completed = core
            .grant(&volume(1), &grant_request(2, 1), 1_010)
            .expect("retry completes the in-flight grant");
        assert_eq!(completed.epoch, WriterEpoch(1));
        let view = core.inspect(&volume(1), 1_020).expect("inspect");
        assert_eq!(view.lease_state, LeaseState::Live);
    }

    #[test]
    fn torn_tail_tolerated_on_reopen() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let (mut core, _grant) = granted(dir.path(), 1_000);
            core.revoke(&volume(1), &revoke_request(3, 1, WriterEpoch(1)), 1_050)
                .expect("self-release");
        }
        // Simulate a torn tail: append garbage bytes to the log.
        let log = dir.path().join("journal.log");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .expect("open log");
        file.write_all(&[0xFF, 0xFF, 0xFF, 0xFF]).expect("garbage");
        drop(file);
        // Reopen: the torn tail is truncated; the durable state (grant +
        // self-release) survives.
        let mut core = open_core(dir.path());
        let view = core.inspect(&volume(1), 1_060).expect("inspect");
        assert_eq!(view.current_epoch, WriterEpoch(1));
        assert_eq!(view.lease_state, LeaseState::Revoked);
        let next = core
            .grant(&volume(1), &grant_request(4, 2), 1_060)
            .expect("grant after self-release and torn tail");
        assert_eq!(next.epoch, WriterEpoch(2));
    }

    #[test]
    fn config_validation_rejects_vacuous_knobs() {
        for config in [
            WitnessCoreConfig {
                lease_ttl_secs: 0,
                lease_grace_secs: 5,
                suspend_budget_secs: 5,
            },
            WitnessCoreConfig {
                lease_ttl_secs: 100,
                lease_grace_secs: 0,
                suspend_budget_secs: 5,
            },
            WitnessCoreConfig {
                lease_ttl_secs: 100,
                lease_grace_secs: 5,
                suspend_budget_secs: 0,
            },
        ] {
            assert!(config.validate().is_err(), "{config:?}");
        }
    }
}
