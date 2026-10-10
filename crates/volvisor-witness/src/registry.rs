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
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use volvisor_journal::{IntentAppend, Journal, JournalRecord};
use volvisor_types::{
    ApiError, AuthorityView, EpochRetirement, FencingProof, HostId, LeaseId, LeaseState,
    OperationId, RecordedMigrationBarrier, VolumeId, VolumeRegistration, WriterEpoch,
};

use crate::proto::{
    CallerIdentity, GrantRequest, GrantResponse, GrantSetRequest, GrantSetResponse,
    RecordBarrierRequest, RecordBarrierResponse, RegisterRequest, RegisterResponse,
    RegistrationContent, RenewRequest, RenewResponse, RevokeRequest, RevokeResponse,
    RevokeSetRequest, RevokeSetResponse, VoidBarrierRequest, VoidBarrierResponse, WitnessError,
    request_hash, request_hash_batch,
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
        /// The acting host (the resolved caller identity) — journaled
        /// for the audit trail; the fold derives state from the lease,
        /// not from this field. `None` only in pre-v2 journals.
        #[serde(default)]
        actor: Option<HostId>,
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
        /// The acting host (the resolved caller identity) — journaled
        /// for the audit trail; the only recoverable record of **who**
        /// performed a forced revocation (the authorization names the
        /// human operator, not the daemon host). `None` only in
        /// pre-v2 journals.
        #[serde(default)]
        actor: Option<HostId>,
    },
    /// Recording of a migration barrier (P4b plan §4 W9): the witness
    /// stamps the entry into the volume's barrier log; the entry is
    /// immutable once journaled (a differing re-record under the same
    /// operation id is an idempotency conflict).
    RecordBarrier {
        /// The volume.
        volume_id: VolumeId,
        /// The stamped barrier entry (the witness's commit index at
        /// recording time is the ordering token).
        barrier: RecordedMigrationBarrier,
    },
    /// Voiding of a recorded barrier (P4b plan §4 W9): the abort
    /// path's evidence-hygiene step. Identifies the barrier uniquely
    /// by `(holder, epoch, boundary_commit_index)`; only journaled
    /// while the epoch is not yet retired.
    VoidBarrier {
        /// The volume.
        volume_id: VolumeId,
        /// The host that recorded the barrier (and is voiding it).
        holder: HostId,
        /// The epoch the barrier attests.
        epoch: WriterEpoch,
        /// The barrier's ordering token (uniquely identifies the log
        /// entry in the fold).
        boundary_commit_index: u64,
    },
    /// Batch self-release (P4b plan §4 W10): one host releasing every
    /// member lease in one journaled mutation — one commit-index bump
    /// for the set, all members' leases flipped by the fold.
    RevokeSet {
        /// The member releases (volume, proof, self-release flag).
        releases: Vec<RevokeSetEntry>,
        /// The acting host (the resolved caller identity — the whole
        /// set releases as one host) — journaled for the audit trail.
        /// `None` only in pre-v2 journals.
        #[serde(default)]
        actor: Option<HostId>,
    },
    /// Batch grant (P4b plan §4 W10): one host acquiring writer
    /// authority for every member volume in one journaled mutation —
    /// one commit-index bump for the set, each member minting its own
    /// epoch and lease.
    GrantSet {
        /// The member grants.
        grants: Vec<GrantRecord>,
    },
}

impl Mutation {
    /// The mutation's serde tag (the `mutation` field's snake_case
    /// value) — the identity the store-save crash seam (P5 plan
    /// §3.1) keys a witness-commit arm on: the witness journals
    /// timer-driven renewals too, so an arm must name the mutation
    /// kind it targets to stay deterministic.
    fn kind(&self) -> &'static str {
        match self {
            Mutation::Register { .. } => "register",
            Mutation::Grant { .. } => "grant",
            Mutation::Renew { .. } => "renew",
            Mutation::Revoke { .. } => "revoke",
            Mutation::RecordBarrier { .. } => "record_barrier",
            Mutation::VoidBarrier { .. } => "void_barrier",
            Mutation::RevokeSet { .. } => "revoke_set",
            Mutation::GrantSet { .. } => "grant_set",
        }
    }
}

/// One member of a journaled [`Mutation::RevokeSet`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeSetEntry {
    /// The released volume.
    volume_id: VolumeId,
    /// Durable proof of the member's retirement (a newly released
    /// member carries the batch's shared commit index; a
    /// state-idempotent member carries its originally recorded proof).
    proof: FencingProof,
    /// Whether the holder released itself (always true for a
    /// revoke-set member — the batch is a self-release set).
    self_released: bool,
}

/// One member of a journaled [`Mutation::GrantSet`]: everything the
/// fold needs to apply the member grant (the same fields a single
/// [`Mutation::Grant`] carries, per member).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantRecord {
    /// The granted volume.
    volume_id: VolumeId,
    /// The new writer.
    host_id: HostId,
    /// The granted epoch (the member's previous epoch + 1).
    epoch: WriterEpoch,
    /// The granted lease (fresh, never reused).
    lease_id: LeaseId,
    /// Witness-clock end of the lease (`grant time + ttl`).
    end_secs: u64,
    /// Fencing proof for the member's retired epoch (the batch's
    /// shared commit index).
    proof: FencingProof,
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
    /// The volume's barrier log (P4b W9), oldest first: every
    /// `RecordBarrier` mutation's stamped entry, with the voided flag
    /// flipped by `VoidBarrier` folds.
    barriers: Vec<RecordedMigrationBarrier>,
    /// The volume's retired epochs, each with the commit index that
    /// durably recorded the retirement (grant of a newer epoch, or an
    /// explicit/batch revocation). The classifier's ordering target.
    retirements: Vec<EpochRetirement>,
    /// The registry commit index of the last mutation that touched
    /// THIS authority (what `inspect` reports as the authority's
    /// commit index — the global watermark stays internal).
    last_commit: u64,
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
    /// The store-save crash seam (P5 plan §3.1): kills inside this
    /// core's own durable mutations — after the intent append, or
    /// between the in-memory apply and the outcome append (the W3b
    /// in-flight window). `None` in every production construction;
    /// the constructing rig attaches its instance.
    crash: Option<Arc<volvisor_types::crash::StoreCrashHooks>>,
}

impl WitnessCore {
    /// Attach the store-save crash seam (P5 plan §3.1) this core's
    /// commits consult — the witness's mid-save kill points, inside
    /// its own durable mutations. Test-rig plumbing only (the
    /// doc-gated trust class in `volvisor-types::crash`); must be
    /// called before the core serves (the rig constructs the core,
    /// attaches, then wraps it in the server state).
    pub fn attach_store_crash_hooks(&mut self, hooks: Arc<volvisor_types::crash::StoreCrashHooks>) {
        self.crash = Some(hooks);
    }

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
            crash: None,
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
    /// W8: the caller must present a host identity (the registering
    /// daemon acts for one of the endpoints). The registration content
    /// itself names no single asserting host, so any host credential is
    /// accepted — a legacy (shared-token) caller is refused typed.
    ///
    /// # Errors
    /// [`WitnessError::InvalidRequest`] on content validation,
    /// [`WitnessError::AlreadyRegistered`] on a diverging re-registration,
    /// [`WitnessError::IdentityRequired`] for a legacy caller.
    pub fn register(
        &mut self,
        volume_id: &VolumeId,
        request: &RegisterRequest,
        now_secs: u64,
        caller: &CallerIdentity,
    ) -> Result<RegisterResponse, WitnessError> {
        validate_content(&request.content)?;
        // Operation-id idempotency first: a journaled retry replays the
        // recorded response even if the registry state has since moved on
        // (strict replay before content comparison).
        let hash = request_hash("register", volume_id, &request.operation_id, request);
        if let Some(replayed) = self.replay(&request.operation_id, hash)? {
            return Ok(replayed);
        }
        if matches!(caller, CallerIdentity::Legacy) {
            return Err(WitnessError::IdentityRequired);
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
    /// response — the returned proof is that retirement. W8: the
    /// caller must be the host credential the grant is for.
    ///
    /// # Errors
    /// [`WitnessError::UnknownVolume`], [`WitnessError::LeaseHeld`],
    /// [`WitnessError::FencePending`], [`WitnessError::IdentityRequired`]
    /// — all fail-closed refusals.
    pub fn grant(
        &mut self,
        volume_id: &VolumeId,
        request: &GrantRequest,
        now_secs: u64,
        caller: &CallerIdentity,
    ) -> Result<GrantResponse, WitnessError> {
        let hash = request_hash("grant", volume_id, &request.operation_id, request);
        if let Some(replayed) = self.replay(&request.operation_id, hash)? {
            return Ok(replayed);
        }
        caller.require_holder(&request.host_id)?;
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
    /// (W5): renewal extends from the renewal time. W8: the caller must
    /// be the host credential the renewal asserts.
    ///
    /// # Errors
    /// [`WitnessError::StaleEpoch`] / [`WitnessError::LeaseHeld`] /
    /// [`WitnessError::UnknownVolume`] /
    /// [`WitnessError::IdentityRequired`].
    pub fn renew(
        &mut self,
        volume_id: &VolumeId,
        request: &RenewRequest,
        now_secs: u64,
        caller: &CallerIdentity,
    ) -> Result<RenewResponse, WitnessError> {
        let hash = request_hash("renew", volume_id, &request.operation_id, request);
        if let Some(replayed) = self.replay(&request.operation_id, hash)? {
            return Ok(replayed);
        }
        caller.require_holder(&request.host_id)?;
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
            actor: caller.host().cloned(),
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
    /// Trust model (W8, P4b plan §4): the caller must hold the host
    /// credential bound to `request.host_id` — the requester, whether
    /// self-releasing or presenting a W6 authorization for a forced
    /// revocation. The P4a residual (a shared token could impersonate
    /// any holder) is closed: there is no shared-token mutation path.
    ///
    /// # Errors
    /// [`WitnessError::StaleEpoch`] on epoch/lease mismatch,
    /// [`WitnessError::InvalidRequest`] when a required authorization or
    /// evidence ground is missing or empty,
    /// [`WitnessError::IdentityRequired`] for a caller not bound to the
    /// requesting host.
    pub fn revoke(
        &mut self,
        volume_id: &VolumeId,
        request: &RevokeRequest,
        now_secs: u64,
        caller: &CallerIdentity,
    ) -> Result<RevokeResponse, WitnessError> {
        let hash = request_hash("revoke", volume_id, &request.operation_id, request);
        if let Some(replayed) = self.replay(&request.operation_id, hash)? {
            return Ok(replayed);
        }
        caller.require_holder(&request.host_id)?;
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
            actor: caller.host().cloned(),
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

    /// Record a migration barrier (P4b plan §4 W9).
    ///
    /// The witness journals the caller's attestation verbatim, stamped
    /// with its own commit index (an **ordering token** in the journal's
    /// total order — no claim of being the epoch's final mutation:
    /// renewals after the barrier write no data and do not invalidate
    /// it) and the witness-clock recording time.
    ///
    /// Enforced: the volume is registered; its current lease exists,
    /// is not revoked and is live; the lease's holder is the asserted
    /// host; the asserted epoch is the current epoch; the caller is
    /// the host credential bound to the asserted holder (W8). A second
    /// barrier for the same epoch is allowed — it appends; the
    /// classifier picks evidence, the witness records.
    ///
    /// # Errors
    /// [`WitnessError::UnknownVolume`], [`WitnessError::StaleEpoch`]
    /// (no live current lease / stale epoch),
    /// [`WitnessError::LeaseHeld`] (another host holds the live
    /// lease), [`WitnessError::IdentityRequired`].
    pub fn record_barrier(
        &mut self,
        volume_id: &VolumeId,
        request: &RecordBarrierRequest,
        now_secs: u64,
        caller: &CallerIdentity,
    ) -> Result<RecordBarrierResponse, WitnessError> {
        let hash = request_hash("record-barrier", volume_id, &request.operation_id, request);
        if let Some(replayed) = self.replay(&request.operation_id, hash)? {
            return Ok(replayed);
        }
        caller.require_holder(&request.host_id)?;
        let vol = self
            .volumes
            .get(volume_id)
            .ok_or(WitnessError::UnknownVolume)?;
        let Some(lease) = vol.lease.as_ref() else {
            return Err(WitnessError::StaleEpoch {
                current_epoch: vol.current_epoch,
            });
        };
        // A barrier attests a serving boundary: the recording epoch
        // must be the current one and its lease must still be live.
        if lease.revoked || now_secs >= lease.end_secs {
            return Err(WitnessError::StaleEpoch {
                current_epoch: vol.current_epoch,
            });
        }
        if request.epoch != vol.current_epoch {
            return Err(WitnessError::StaleEpoch {
                current_epoch: vol.current_epoch,
            });
        }
        if lease.holder != request.host_id {
            return Err(WitnessError::LeaseHeld {
                current_epoch: vol.current_epoch,
            });
        }
        let barrier = RecordedMigrationBarrier {
            holder: request.host_id.clone(),
            epoch: request.epoch,
            boundary_commit_index: self.commit_index + 1,
            attestation: request.attestation,
            migration_id: request.migration_id.clone(),
            recorded_at: now_secs,
            voided: false,
        };
        let response = RecordBarrierResponse {
            barrier: barrier.clone(),
        };
        let mutation = Mutation::RecordBarrier {
            volume_id: volume_id.clone(),
            barrier,
        };
        self.commit(
            &request.operation_id,
            hash,
            "witness_record_barrier",
            &mutation,
            &response,
        )?;
        Ok(response)
    }

    /// Void a recorded barrier (P4b plan §4 W9): the abort path's
    /// evidence-hygiene step.
    ///
    /// Enforced: the caller is the host credential bound to the
    /// recording holder (W8); the target epoch is **not retired**
    /// (voiding after retirement would be retroactive tampering with
    /// what could later certify a `SAFE_CURRENT` classification); the
    /// latest non-voided barrier of the holder/epoch (and migration,
    /// when the request carries one) is voided. A void can only lose
    /// an unlock, never promote anything falsely — but it must not be
    /// possible after the epoch's facts are sealed.
    ///
    /// # Errors
    /// [`WitnessError::UnknownVolume`],
    /// [`WitnessError::InvalidRequest`] when no matching barrier
    /// exists or the epoch is already retired,
    /// [`WitnessError::IdentityRequired`].
    pub fn void_barrier(
        &mut self,
        volume_id: &VolumeId,
        request: &VoidBarrierRequest,
        // Kept for signature symmetry with the other core operations;
        // no time check applies here (the retirement check subsumes
        // it — a retired epoch stays retired).
        _now_secs: u64,
        caller: &CallerIdentity,
    ) -> Result<VoidBarrierResponse, WitnessError> {
        let hash = request_hash("void-barrier", volume_id, &request.operation_id, request);
        if let Some(replayed) = self.replay(&request.operation_id, hash)? {
            return Ok(replayed);
        }
        caller.require_holder(&request.host_id)?;
        let vol = self
            .volumes
            .get(volume_id)
            .ok_or(WitnessError::UnknownVolume)?;
        if vol
            .retirements
            .iter()
            .any(|retired| retired.epoch == request.epoch)
        {
            return Err(WitnessError::InvalidRequest(
                "refusing to void a barrier of a retired epoch (the retirement is \
                 sealed evidence)"
                    .to_owned(),
            ));
        }
        // Latest non-voided barrier of this holder/epoch (and
        // migration, when the request carries one). The witness stamps
        // and never invents: a barrier that was never recorded cannot
        // be voided.
        let found = vol.barriers.iter().rev().find(|barrier| {
            !barrier.voided
                && barrier.holder == request.host_id
                && barrier.epoch == request.epoch
                && request
                    .migration_id
                    .as_ref()
                    .is_none_or(|migration| barrier.migration_id.as_ref() == Some(migration))
        });
        let Some(found) = found else {
            return Err(WitnessError::InvalidRequest(
                "no matching recorded barrier".to_owned(),
            ));
        };
        let mut voided = found.clone();
        voided.voided = true;
        let response = VoidBarrierResponse { barrier: voided };
        let mutation = Mutation::VoidBarrier {
            volume_id: volume_id.clone(),
            holder: request.host_id.clone(),
            epoch: request.epoch,
            boundary_commit_index: found.boundary_commit_index,
        };
        self.commit(
            &request.operation_id,
            hash,
            "witness_void_barrier",
            &mutation,
            &response,
        )?;
        Ok(response)
    }

    /// Batch self-release (P4b plan §4 W10 `revoke-set`).
    ///
    /// A batch of self-releases by **one host** (the migration
    /// source). All-or-nothing: every member is pre-checked before
    /// anything is journaled — volume registered, lease exists at the
    /// current epoch held by the batch's host (a batch is never a
    /// forced revocation; a member held by another host refuses the
    /// whole batch), or the state-idempotent case (already revoked,
    /// self-released by this host — confirmed with the recorded
    /// proof). One journaled mutation carries every member's proof at
    /// the batch's shared commit index; one commit-index bump for the
    /// set. W8: the caller is the host credential bound to the
    /// batch's host.
    ///
    /// # Errors
    /// [`WitnessError::UnknownVolume`], [`WitnessError::StaleEpoch`],
    /// [`WitnessError::InvalidRequest`] (empty/duplicate members, a
    /// member held by another host),
    /// [`WitnessError::IdentityRequired`]. Any member failure refuses
    /// the whole batch with nothing journaled.
    pub fn revoke_set(
        &mut self,
        request: &RevokeSetRequest,
        // Kept for signature symmetry with the other core operations;
        // a self-release needs no liveness check (an expired lease is
        // as releasable as a live one — the holder demoted either
        // way), and the batch is never a forced revocation.
        _now_secs: u64,
        caller: &CallerIdentity,
    ) -> Result<RevokeSetResponse, WitnessError> {
        let hash = request_hash_batch("revoke-set", &request.operation_id, request);
        if let Some(replayed) = self.replay(&request.operation_id, hash)? {
            return Ok(replayed);
        }
        if request.releases.is_empty() {
            return Err(WitnessError::InvalidRequest(
                "revoke-set requires at least one release".to_owned(),
            ));
        }
        let mut seen = std::collections::HashSet::with_capacity(request.releases.len());
        for release in &request.releases {
            if !seen.insert(&release.volume_id) {
                return Err(WitnessError::InvalidRequest(format!(
                    "revoke-set lists volume {} more than once",
                    release.volume_id
                )));
            }
        }
        caller.require_holder(&request.host_id)?;
        // Pre-check every member before committing anything: the batch
        // is all-or-nothing (a partial release is never issued — D4).
        let mut entries = Vec::with_capacity(request.releases.len());
        for release in &request.releases {
            let vol = self
                .volumes
                .get(&release.volume_id)
                .ok_or(WitnessError::UnknownVolume)?;
            let Some(lease) = vol.lease.as_ref() else {
                return Err(WitnessError::StaleEpoch {
                    current_epoch: vol.current_epoch,
                });
            };
            if release.epoch != vol.current_epoch {
                return Err(WitnessError::StaleEpoch {
                    current_epoch: vol.current_epoch,
                });
            }
            if lease.revoked {
                // State-idempotent member: the batch's goal state
                // (this lease released by its holder) already holds —
                // confirm with the recorded proof.
                if lease.self_released && lease.holder == request.host_id {
                    let proof = vol.last_proof.clone().ok_or_else(|| {
                        WitnessError::Internal(
                            "recorded self-release is missing its fencing proof".to_owned(),
                        )
                    })?;
                    entries.push(RevokeSetEntry {
                        volume_id: release.volume_id.clone(),
                        proof,
                        self_released: true,
                    });
                    continue;
                }
                return Err(WitnessError::StaleEpoch {
                    current_epoch: vol.current_epoch,
                });
            }
            if lease.holder != request.host_id {
                return Err(WitnessError::InvalidRequest(format!(
                    "revoke-set is a self-release batch; volume {} is held by another host",
                    release.volume_id
                )));
            }
            entries.push(RevokeSetEntry {
                volume_id: release.volume_id.clone(),
                proof: FencingProof {
                    volume_id: release.volume_id.clone(),
                    retired_epoch: vol.current_epoch,
                    commit_index: self.commit_index + 1,
                },
                self_released: true,
            });
        }
        let response = RevokeSetResponse {
            releases: entries
                .iter()
                .map(|entry| crate::proto::BatchRevokeOutcome {
                    volume_id: entry.volume_id.clone(),
                    fencing_proof: entry.proof.clone(),
                })
                .collect(),
        };
        let mutation = Mutation::RevokeSet {
            releases: entries,
            actor: caller.host().cloned(),
        };
        self.commit(
            &request.operation_id,
            hash,
            "witness_revoke_set",
            &mutation,
            &response,
        )?;
        Ok(response)
    }

    /// Batch grant (P4b plan §4 W10 `grant-set`).
    ///
    /// Every member passes the single-grant W1/W7 checks (a live
    /// lease refuses; an unfenced retired lease delays unless
    /// self-released or power-off attested) or the whole batch is
    /// refused and nothing is journaled. Each member mints its own
    /// epoch (member current + 1) and a fresh lease; every member's
    /// fencing proof retires its previous epoch at the batch's shared
    /// commit index; one commit-index bump for the set. W8: the
    /// caller is the host credential bound to the batch's host.
    ///
    /// # Errors
    /// [`WitnessError::UnknownVolume`], [`WitnessError::LeaseHeld`],
    /// [`WitnessError::FencePending`], [`WitnessError::InvalidRequest`]
    /// (empty/duplicate members), [`WitnessError::IdentityRequired`].
    pub fn grant_set(
        &mut self,
        request: &GrantSetRequest,
        now_secs: u64,
        caller: &CallerIdentity,
    ) -> Result<GrantSetResponse, WitnessError> {
        let hash = request_hash_batch("grant-set", &request.operation_id, request);
        if let Some(replayed) = self.replay(&request.operation_id, hash)? {
            return Ok(replayed);
        }
        if request.requests.is_empty() {
            return Err(WitnessError::InvalidRequest(
                "grant-set requires at least one volume".to_owned(),
            ));
        }
        let mut seen = std::collections::HashSet::with_capacity(request.requests.len());
        for member in &request.requests {
            if !seen.insert(&member.volume_id) {
                return Err(WitnessError::InvalidRequest(format!(
                    "grant-set lists volume {} more than once",
                    member.volume_id
                )));
            }
        }
        caller.require_holder(&request.host_id)?;
        // Pre-check every member against the single-grant W1/W7 rules
        // before committing anything.
        let mut grants = Vec::with_capacity(request.requests.len());
        for member in &request.requests {
            let vol = self
                .volumes
                .get(&member.volume_id)
                .ok_or(WitnessError::UnknownVolume)?;
            if let Some(lease) = &vol.lease {
                if !lease.revoked && now_secs < lease.end_secs {
                    return Err(WitnessError::LeaseHeld {
                        current_epoch: vol.current_epoch,
                    });
                }
                if !lease.self_released && !lease.power_off_attested {
                    let fence_until = lease.end_secs.saturating_add(self.config.fence_wait_secs());
                    if now_secs < fence_until {
                        return Err(WitnessError::FencePending {
                            retry_after_secs: fence_until - now_secs,
                        });
                    }
                }
            }
            grants.push(GrantRecord {
                volume_id: member.volume_id.clone(),
                host_id: request.host_id.clone(),
                epoch: WriterEpoch(vol.current_epoch.0 + 1),
                lease_id: LeaseId(self.next_lease_id + grants.len() as u64),
                end_secs: now_secs.saturating_add(self.config.lease_ttl_secs),
                proof: FencingProof {
                    volume_id: member.volume_id.clone(),
                    retired_epoch: vol.current_epoch,
                    commit_index: self.commit_index + 1,
                },
            });
        }
        let response = GrantSetResponse {
            grants: grants
                .iter()
                .map(|record| crate::proto::BatchGrantOutcome {
                    volume_id: record.volume_id.clone(),
                    epoch: record.epoch,
                    lease_id: record.lease_id,
                    lease_ttl_secs: self.config.lease_ttl_secs,
                    fencing_proof: record.proof.clone(),
                })
                .collect(),
        };
        let mutation = Mutation::GrantSet { grants };
        self.commit(
            &request.operation_id,
            hash,
            "witness_grant_set",
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
            // The live lease's identity (the promote-under-granted-lease
            // path renews a lease the witness minted for this host, so
            // it must learn the id from the view; None without a lease).
            lease_id: vol.lease.as_ref().map(|lease| lease.lease_id),
            lease_remaining_secs,
            commit_index: vol.last_commit,
            registration: Some(vol.registration.clone()),
            barriers: vol.barriers.clone(),
            retirements: vol.retirements.clone(),
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
                // The store-save crash point (P5 plan §3.1, the
                // witness variant): the intent is durable, neither
                // the in-memory apply nor the outcome has happened —
                // a restart's replay re-derives the whole mutation.
                if let Some(crash) = &self.crash {
                    crash.consult_witness(
                        mutation.kind(),
                        volvisor_types::crash::StoreSavePoint::WitnessAfterIntentAppend,
                    );
                }
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
        // The store-save crash point (P5 plan §3.1, the witness
        // variant): the mutation is applied in memory, the outcome
        // is not yet durable — the W3b in-flight window a restart's
        // roll-forward completes from the recorded envelope.
        if let Some(crash) = &self.crash {
            crash.consult_witness(
                mutation.kind(),
                volvisor_types::crash::StoreSavePoint::WitnessAfterApplyBeforeOutcome,
            );
        }
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
    ///
    /// One commit-index bump per mutation — batch mutations (W10) bump
    /// once for the whole set, so every member's fencing proof shares
    /// the batch's index. The bump happens **before** the variant
    /// folds so retirement records (W9) can stamp the mutation's own
    /// (post-bump) commit index.
    fn apply(&mut self, mutation: &Mutation) {
        self.commit_index += 1;
        let commit_index = self.commit_index;
        match mutation {
            Mutation::Register {
                volume_id,
                registration,
            } => self.apply_register(volume_id, registration),
            Mutation::Grant {
                volume_id,
                host_id,
                epoch,
                lease_id,
                end_secs,
                proof,
            } => self.apply_grant(
                commit_index,
                volume_id,
                host_id,
                *epoch,
                *lease_id,
                *end_secs,
                proof,
            ),
            Mutation::Renew {
                volume_id,
                lease_id,
                end_secs,
                ..
            } => self.apply_renew(volume_id, *lease_id, *end_secs),
            Mutation::Revoke {
                volume_id,
                proof,
                self_released,
                power_off_attested,
                ..
            } => self.apply_revoke(
                commit_index,
                volume_id,
                proof,
                *self_released,
                *power_off_attested,
            ),
            Mutation::RecordBarrier { volume_id, barrier } => {
                self.apply_record_barrier(volume_id, barrier);
            }
            Mutation::VoidBarrier {
                volume_id,
                holder,
                epoch,
                boundary_commit_index,
            } => self.apply_void_barrier(volume_id, holder, *epoch, *boundary_commit_index),
            Mutation::RevokeSet { releases, .. } => self.apply_revoke_set(commit_index, releases),
            Mutation::GrantSet { grants } => self.apply_grant_set(commit_index, grants),
        }
        for volume_id in mutation.volume_ids() {
            if let Some(vol) = self.volumes.get_mut(volume_id) {
                vol.last_commit = commit_index;
            }
        }
    }

    /// `Mutation::Register` fold: a fresh authority at the
    /// pre-authority epoch with no holder, lease or W9 records.
    fn apply_register(&mut self, volume_id: &VolumeId, registration: &VolumeRegistration) {
        self.volumes.insert(
            volume_id.clone(),
            VolumeAuthority {
                registration: registration.clone(),
                current_epoch: WriterEpoch::pre_authority(),
                holder: None,
                lease: None,
                last_proof: None,
                barriers: Vec::new(),
                retirements: Vec::new(),
                last_commit: 0,
            },
        );
    }

    /// `Mutation::Grant` fold (W2: retires every older epoch, the
    /// pre-authority epoch's implicit retirement included).
    fn apply_grant(
        &mut self,
        commit_index: u64,
        volume_id: &VolumeId,
        host_id: &HostId,
        epoch: WriterEpoch,
        lease_id: LeaseId,
        end_secs: u64,
        proof: &FencingProof,
    ) {
        if let Some(vol) = self.volumes.get_mut(volume_id) {
            // The grant retires every epoch below the granted one —
            // recorded with the grant's own commit index so barriers
            // can be ordered against it.
            retire_epoch(vol, proof.retired_epoch, commit_index);
            vol.current_epoch = epoch;
            vol.holder = Some(host_id.clone());
            vol.lease = Some(LeaseRecord {
                lease_id,
                holder: host_id.clone(),
                epoch,
                end_secs,
                revoked: false,
                self_released: false,
                power_off_attested: false,
            });
            vol.last_proof = Some(proof.clone());
        }
        self.next_lease_id = self.next_lease_id.max(lease_id.0 + 1);
    }

    /// `Mutation::Renew` fold: extends the named lease's end.
    fn apply_renew(&mut self, volume_id: &VolumeId, lease_id: LeaseId, end_secs: u64) {
        if let Some(vol) = self.volumes.get_mut(volume_id) {
            if let Some(lease) = vol.lease.as_mut() {
                if lease.lease_id == lease_id {
                    lease.end_secs = end_secs;
                }
            }
        }
    }

    /// `Mutation::Revoke` fold: the revoked lease's epoch is the
    /// current epoch (W4 checked it at commit time); the retirement is
    /// sealed at this mutation's own commit index.
    fn apply_revoke(
        &mut self,
        commit_index: u64,
        volume_id: &VolumeId,
        proof: &FencingProof,
        self_released: bool,
        power_off_attested: bool,
    ) {
        if let Some(vol) = self.volumes.get_mut(volume_id) {
            retire_epoch(vol, proof.retired_epoch, commit_index);
            if let Some(lease) = vol.lease.as_mut() {
                lease.revoked = true;
                lease.self_released = self_released;
                lease.power_off_attested = power_off_attested;
            }
            vol.last_proof = Some(proof.clone());
        }
    }

    /// `Mutation::RecordBarrier` fold (W9): appends the stamped entry.
    fn apply_record_barrier(&mut self, volume_id: &VolumeId, barrier: &RecordedMigrationBarrier) {
        if let Some(vol) = self.volumes.get_mut(volume_id) {
            vol.barriers.push(barrier.clone());
        }
    }

    /// `Mutation::VoidBarrier` fold (W9): `(holder, epoch,
    /// boundary_commit_index)` identifies the entry uniquely (each
    /// record barrier is its own commit).
    fn apply_void_barrier(
        &mut self,
        volume_id: &VolumeId,
        holder: &HostId,
        epoch: WriterEpoch,
        boundary_commit_index: u64,
    ) {
        if let Some(vol) = self.volumes.get_mut(volume_id) {
            if let Some(barrier) = vol.barriers.iter_mut().find(|barrier| {
                barrier.holder == *holder
                    && barrier.epoch == epoch
                    && barrier.boundary_commit_index == boundary_commit_index
            }) {
                barrier.voided = true;
            }
        }
    }

    /// `Mutation::RevokeSet` fold (W10): every member lease flipped by
    /// the one batch mutation, all retirements sharing its commit
    /// index.
    fn apply_revoke_set(&mut self, commit_index: u64, releases: &[RevokeSetEntry]) {
        for entry in releases {
            // An unknown member is skipped, never invented: the live
            // path pre-validates every member (`UnknownVolume` before
            // anything is journaled) and a replayed batch was already
            // validated, so this arm is twice-defensive — it guards a
            // corrupted journal or a host bug writing an unregistered
            // member. A skipped member keeps its prior state, which
            // the caller's own reconcile re-checks externally.
            let Some(vol) = self.volumes.get_mut(&entry.volume_id) else {
                continue;
            };
            retire_epoch(vol, entry.proof.retired_epoch, commit_index);
            if let Some(lease) = vol.lease.as_mut() {
                lease.revoked = true;
                lease.self_released = entry.self_released;
            }
            vol.last_proof = Some(entry.proof.clone());
        }
    }

    /// `Mutation::GrantSet` fold (W10): every member grant applied by
    /// the one batch mutation, all retirements sharing its commit
    /// index.
    fn apply_grant_set(&mut self, commit_index: u64, grants: &[GrantRecord]) {
        for record in grants {
            // Unknown member: skipped, never invented — the live path
            // pre-validates every member before journaling, so this is
            // twice-defensive (see apply_revoke_set).
            let Some(vol) = self.volumes.get_mut(&record.volume_id) else {
                continue;
            };
            retire_epoch(vol, record.proof.retired_epoch, commit_index);
            vol.current_epoch = record.epoch;
            vol.holder = Some(record.host_id.clone());
            vol.lease = Some(LeaseRecord {
                lease_id: record.lease_id,
                holder: record.host_id.clone(),
                epoch: record.epoch,
                end_secs: record.end_secs,
                revoked: false,
                self_released: false,
                power_off_attested: false,
            });
            vol.last_proof = Some(record.proof.clone());
            self.next_lease_id = self.next_lease_id.max(record.lease_id.0 + 1);
        }
    }
}

/// Append one retirement record to a volume's authority (W9), unless
/// the epoch is already retired — a `GrantSet`/`Grant` following a
/// `Revoke` of the same epoch must not double-append (the earlier
/// record is the sealed evidence; the later mutation retires nothing
/// new).
fn retire_epoch(vol: &mut VolumeAuthority, epoch: WriterEpoch, commit_index: u64) {
    if vol.retirements.iter().all(|retired| retired.epoch != epoch) {
        vol.retirements.push(EpochRetirement {
            epoch,
            commit_index,
        });
    }
}

impl Mutation {
    /// The volumes this mutation touches: one for the volume-scoped
    /// variants, every member for the batch variants (W10).
    fn volume_ids(&self) -> Vec<&VolumeId> {
        match self {
            Mutation::Register { volume_id, .. }
            | Mutation::Grant { volume_id, .. }
            | Mutation::Renew { volume_id, .. }
            | Mutation::Revoke { volume_id, .. }
            | Mutation::RecordBarrier { volume_id, .. }
            | Mutation::VoidBarrier { volume_id, .. } => vec![volume_id],
            Mutation::RevokeSet { releases, .. } => {
                releases.iter().map(|entry| &entry.volume_id).collect()
            }
            Mutation::GrantSet { grants } => {
                grants.iter().map(|record| &record.volume_id).collect()
            }
        }
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

    /// The host credential `host(n)` would present (W8 tests).
    fn caller(n: u64) -> crate::proto::CallerIdentity {
        crate::proto::CallerIdentity::Host(host(n))
    }

    /// The legacy shared-token identity (read-only on a v2 witness).
    const LEGACY: crate::proto::CallerIdentity = crate::proto::CallerIdentity::Legacy;

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

    fn record_barrier_request(
        n: u64,
        host_n: u64,
        epoch: WriterEpoch,
    ) -> crate::proto::RecordBarrierRequest {
        crate::proto::RecordBarrierRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op(n),
            host_id: host(host_n),
            epoch,
            attestation: volvisor_types::BarrierAttestation {
                vm_paused_and_drained: true,
                data_path_suspended: true,
                peer_up_to_date: true,
            },
            migration_id: Some(
                volvisor_types::MigrationId::new(format!("mig-{n}")).expect("valid migration id"),
            ),
        }
    }

    /// A void request under operation id `void_op`, scoped to the
    /// barrier recorded by `record_barrier_request(record_op, ..)`
    /// (same migration).
    fn void_barrier_request(
        void_op: u64,
        record_op: u64,
        host_n: u64,
        epoch: WriterEpoch,
    ) -> crate::proto::VoidBarrierRequest {
        crate::proto::VoidBarrierRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op(void_op),
            host_id: host(host_n),
            epoch,
            migration_id: Some(
                volvisor_types::MigrationId::new(format!("mig-{record_op}"))
                    .expect("valid migration id"),
            ),
        }
    }

    fn batch_release(volume_n: u64, epoch: WriterEpoch) -> crate::proto::BatchRelease {
        crate::proto::BatchRelease {
            volume_id: volume(volume_n),
            epoch,
        }
    }

    fn revoke_set_request(
        n: u64,
        host_n: u64,
        releases: Vec<crate::proto::BatchRelease>,
    ) -> crate::proto::RevokeSetRequest {
        crate::proto::RevokeSetRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op(n),
            host_id: host(host_n),
            migration_id: Some(
                volvisor_types::MigrationId::new(format!("mig-{n}")).expect("valid migration id"),
            ),
            releases,
        }
    }

    fn grant_set_request(
        n: u64,
        host_n: u64,
        volumes: Vec<VolumeId>,
    ) -> crate::proto::GrantSetRequest {
        crate::proto::GrantSetRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op(n),
            host_id: host(host_n),
            migration_id: Some(
                volvisor_types::MigrationId::new(format!("mig-{n}")).expect("valid migration id"),
            ),
            requests: volumes
                .into_iter()
                .map(|volume_id| crate::proto::BatchGrantVolume { volume_id })
                .collect(),
        }
    }

    fn open_core(dir: &std::path::Path) -> WitnessCore {
        WitnessCore::open(dir, test_config()).expect("witness core opens")
    }

    fn registered(dir: &std::path::Path) -> WitnessCore {
        let mut core = open_core(dir);
        core.register(&volume(1), &register_request(1), 1_000, &caller(1))
            .expect("register");
        core
    }

    /// Both fixture volumes registered (the W10 batch tests).
    fn registered_pair(dir: &std::path::Path) -> WitnessCore {
        let mut core = registered(dir);
        core.register(&volume(2), &register_request(80), 1_000, &caller(1))
            .expect("register vol-2");
        core
    }

    fn granted(dir: &std::path::Path, at: u64) -> (WitnessCore, GrantResponse) {
        let mut core = registered(dir);
        let response = core
            .grant(&volume(1), &grant_request(2, 1), at, &caller(1))
            .expect("grant");
        (core, response)
    }

    #[test]
    fn w1_single_live_lease_blocks_competing_grants() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut core, _grant) = granted(dir.path(), 1_000);
        // Live for another host.
        let err = core
            .grant(&volume(1), &grant_request(3, 2), 1_050, &caller(2))
            .expect_err("competing grant refused");
        assert_eq!(
            err,
            WitnessError::LeaseHeld {
                current_epoch: WriterEpoch(1)
            }
        );
        // Live for the same host too: the holder must renew, not re-grant.
        let err = core
            .grant(&volume(1), &grant_request(4, 1), 1_050, &caller(1))
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
        core.revoke(
            &volume(1),
            &revoke_request(5, 1, grant.epoch),
            1_050,
            &caller(1),
        )
        .expect("self-release after restart");
        let second = core
            .grant(&volume(1), &grant_request(6, 2), 1_060, &caller(2))
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
        let orphan_hash = request_hash("grant", &volume(1), &op(91), &grant_request(91, 1));
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
            .grant(&volume(1), &grant_request(91, 1), 1_060, &caller(1))
            .expect("retry completes the rolled-forward grant");
        assert_eq!(replayed.epoch, WriterEpoch(1));
        // Fresh operation id: blocked by the (now durable) live lease —
        // W1, not a wedge.
        let err = core
            .grant(&volume(1), &grant_request(92, 2), 1_060, &caller(2))
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
                &caller(1),
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
                &caller(1),
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
                &caller(1),
            )
            .expect("renewal");
        assert_eq!(renewed.remaining_secs, 100);
        // Wrong holder.
        let err = core
            .renew(
                &volume(1),
                &renew_request(8, 2, grant.epoch, grant.lease_id),
                1_060,
                &caller(2),
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
            .grant(&volume(1), &grant_request(3, 2), 1_105, &caller(2))
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
            .revoke(&volume(1), &forced, 1_050, &caller(2))
            .expect("forced revocation journaled");
        assert_eq!(proof.fencing_proof.retired_epoch, WriterEpoch(1));
        // Fence window: recorded end 1_100 + grace 5 + budget 5 = 1_110.
        let err = core
            .grant(&volume(1), &grant_request(5, 2), 1_109, &caller(2))
            .expect_err("fence window keys on the recorded end, not the revoke time");
        assert_eq!(
            err,
            WitnessError::FencePending {
                retry_after_secs: 1
            }
        );
        let granted_late = core
            .grant(&volume(1), &grant_request(6, 2), 1_110, &caller(2))
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
            .revoke(
                &volume(1),
                &revoke_request(3, 2, grant.epoch),
                1_050,
                &caller(2),
            )
            .expect_err("forced revocation without authorization refused");
        assert!(matches!(err, WitnessError::InvalidRequest(_)));
        // Empty authorization fields: refused.
        let mut empty = revoke_request(4, 2, grant.epoch);
        empty.authorization = Some(RevocationAuthorization {
            operator: "  ".to_owned(),
            reason: "forced failover".to_owned(),
        });
        let err = core
            .revoke(&volume(1), &empty, 1_050, &caller(2))
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
        core.revoke(&volume(1), &forced, 1_050, &caller(2))
            .expect("power-off-attested revocation");
        core.grant(&volume(1), &grant_request(6, 2), 1_050, &caller(2))
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
            .revoke(&volume(1), &bare, 1_050, &caller(2))
            .expect("bare forced revocation journaled");
        let err = core2
            .grant(&volume(1), &grant_request(8, 2), 1_050, &caller(2))
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
        core.revoke(
            &volume(1),
            &revoke_request(3, 1, grant.epoch),
            1_050,
            &caller(1),
        )
        .expect("self-release");
        let immediate = core
            .grant(&volume(1), &grant_request(4, 2), 1_050, &caller(2))
            .expect("grant immediately after self-release");
        assert_eq!(immediate.epoch, WriterEpoch(2));
        // State-idempotent self-release retry under a fresh operation id
        // after the lease was already replaced: epoch mismatch teaches
        // the stale caller it is fenced.
        let err = core
            .revoke(
                &volume(1),
                &revoke_request(5, 1, grant.epoch),
                1_051,
                &caller(1),
            )
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
            .revoke(
                &volume(1),
                &revoke_request(6, 2, immediate.epoch),
                1_051,
                &caller(2),
            )
            .expect("self-release");
        let retry = core
            .revoke(
                &volume(1),
                &revoke_request(7, 2, immediate.epoch),
                1_052,
                &caller(2),
            )
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
            core.register(&volume(1), &unsorted, 1_000, &caller(1)),
            Err(WitnessError::InvalidRequest(_))
        ));
        // One endpoint.
        let mut one = register_request(2);
        one.content.endpoints.truncate(1);
        assert!(matches!(
            core.register(&volume(1), &one, 1_000, &caller(1)),
            Err(WitnessError::InvalidRequest(_))
        ));
        // Same-host endpoints.
        let mut same = register_request(3);
        same.content.endpoints[1].host_id = host(1);
        assert!(matches!(
            core.register(&volume(1), &same, 1_000, &caller(1)),
            Err(WitnessError::InvalidRequest(_))
        ));
        // Successful registration, then an identical re-registration
        // under a different operation id: idempotent.
        core.register(&volume(1), &register_request(4), 1_000, &caller(1))
            .expect("register");
        core.register(&volume(1), &register_request(5), 2_000, &caller(1))
            .expect("identical re-register is idempotent");
        // Diverging content: typed conflict.
        let mut diverging = register_request(6);
        diverging.content.lineage_uuids = vec!["0000000000000ABC".to_owned()];
        assert_eq!(
            core.register(&volume(1), &diverging, 2_000, &caller(1))
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
            core.grant(&volume(2), &grant_request(7, 1), 2_000, &caller(1))
                .expect_err("unregistered grant refused"),
            WitnessError::UnknownVolume
        );
    }

    #[test]
    fn grant_idempotency_replays_and_conflicts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut core = registered(dir.path());
        let first = core
            .grant(&volume(1), &grant_request(2, 1), 1_000, &caller(1))
            .expect("grant");
        // Same operation id + same request: byte-identical replay.
        let replay = core
            .grant(&volume(1), &grant_request(2, 1), 1_050, &caller(1))
            .expect("replay");
        assert_eq!(first, replay);
        // Same operation id + different request: typed conflict.
        let conflicting = core
            .grant(&volume(1), &grant_request(2, 2), 1_050, &caller(2))
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
            .grant(&volume(1), &grant_request(2, 1), 1_000, &caller(1))
            .expect_err("outcome append fails after the intent is durable");
        assert!(matches!(err, WitnessError::Internal(_)));
        // Lift the injected fault: the retry completes the durable
        // intent's outcome (runtime W3b).
        core.journal.inject_append_failures_after(u64::MAX);
        let completed = core
            .grant(&volume(1), &grant_request(2, 1), 1_010, &caller(1))
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
            core.revoke(
                &volume(1),
                &revoke_request(3, 1, WriterEpoch(1)),
                1_050,
                &caller(1),
            )
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
            .grant(&volume(1), &grant_request(4, 2), 1_060, &caller(2))
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

    #[test]
    fn the_same_operation_id_across_volumes_never_replays_the_wrong_lease() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut core = open_core(dir.path());
        core.register(&volume(1), &register_request(1), 1_000, &caller(1))
            .expect("register vol-1");
        core.register(&volume(2), &register_request(2), 1_000, &caller(1))
            .expect("register vol-2");
        // The same operation id and a byte-identical body, targeting a
        // different volume: the volume is folded into the request hash
        // (it travels in the URL path, not the body), so this is a
        // typed idempotency conflict — never volume 1's grant response
        // served for volume 2.
        core.grant(&volume(1), &grant_request(7, 1), 1_000, &caller(1))
            .expect("grant vol-1");
        let error = core
            .grant(&volume(2), &grant_request(7, 1), 1_000, &caller(1))
            .expect_err("cross-volume reuse of an operation id");
        assert!(
            matches!(error, WitnessError::IdempotencyConflict),
            "{error:?}"
        );
    }

    #[test]
    fn inspect_reports_the_authoritys_own_commit_index_not_the_global_watermark() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut core = open_core(dir.path());
        core.register(&volume(1), &register_request(1), 1_000, &caller(1))
            .expect("register vol-1");
        core.register(&volume(2), &register_request(2), 1_000, &caller(1))
            .expect("register vol-2");
        core.grant(&volume(1), &grant_request(3, 1), 1_000, &caller(1))
            .expect("grant vol-1");
        // vol-2's authority last changed at its own registration: the
        // global watermark (bumped by vol-1's grant) must not leak into
        // its view.
        let view_1 = core.inspect(&volume(1), 1_000).expect("inspect vol-1");
        let view_2 = core.inspect(&volume(2), 1_000).expect("inspect vol-2");
        assert_eq!(view_1.commit_index, 3, "vol-1's own grant index");
        assert_eq!(view_2.commit_index, 2, "vol-2's own registration index");
        // The global watermark equals vol-1's index only because its
        // grant was the last mutation; vol-2 proves the per-volume
        // tracking.
        assert_eq!(core.commit_index(), 3);
    }

    // ------------------------------------------------------ P4b W8

    /// The journaled mutations of a dropped core, oldest first — the
    /// audit trail (the journal enforces single-writer, so the core
    /// must be closed before its records are read).
    fn journaled_mutations(dir: &std::path::Path) -> Vec<Mutation> {
        let (journal, records) =
            Journal::open_with_records(dir).expect("reopen the journal read-only");
        drop(journal);
        records
            .into_iter()
            .filter_map(|record| {
                let JournalRecord::Intent(intent) = record else {
                    return None;
                };
                if !intent.op_kind.starts_with(OP_KIND_PREFIX) {
                    return None;
                }
                let envelope: MutationEnvelope =
                    serde_json::from_value(intent.payload.clone()).ok()?;
                Some(envelope.mutation)
            })
            .collect()
    }

    #[test]
    fn pre_v2_journal_payloads_without_the_actor_still_parse() {
        // Regression pin (round-2 review, NOTE 7): the W8 actor
        // annotation is `#[serde(default)]` so a P4a-era journal —
        // written before the field existed — must keep replaying.
        // `deny_unknown_fields` rejects unknown keys, not
        // missing-with-default; these hand-written pre-v2 payloads
        // pin that mechanism so removing the default breaks a test
        // instead of a stale journal's startup.
        let cases: [(&str, serde_json::Value); 3] = [
            (
                "renew",
                serde_json::json!({
                    "mutation": "renew",
                    "volume_id": "vol-1",
                    "lease_id": 1,
                    "end_secs": 1_100
                }),
            ),
            (
                "revoke",
                serde_json::json!({
                    "mutation": "revoke",
                    "volume_id": "vol-1",
                    "proof": {
                        "volume_id": "vol-1",
                        "retired_epoch": 1,
                        "commit_index": 3
                    },
                    "self_released": false,
                    "power_off_attested": false,
                    "authorization": null
                }),
            ),
            (
                "revoke_set",
                serde_json::json!({
                    "mutation": "revoke_set",
                    "releases": [{
                        "volume_id": "vol-1",
                        "proof": {
                            "volume_id": "vol-1",
                            "retired_epoch": 1,
                            "commit_index": 5
                        },
                        "self_released": true
                    }]
                }),
            ),
        ];
        for (label, mutation) in &cases {
            let payload = serde_json::json!({
                "mutation": mutation,
                "response": {}
            });
            let envelope: MutationEnvelope =
                serde_json::from_value(payload).expect("the pre-v2 payload parses");
            assert!(
                matches!(&envelope.mutation,
                    Mutation::Renew { actor, .. } if actor.is_none())
                    || matches!(&envelope.mutation,
                        Mutation::Revoke { actor, .. } if actor.is_none())
                    || matches!(&envelope.mutation,
                        Mutation::RevokeSet { actor, .. } if actor.is_none()),
                "{label}: the pre-v2 mutation parses with a None actor, got {:?}",
                envelope.mutation
            );
        }
    }

    #[test]
    fn w8_the_acting_host_is_journaled_on_every_mutation() {
        // The W8 audit-trail sentence: every journaled mutation records
        // the bound identity. `Grant`, `RecordBarrier` and the barrier
        // voids carry their host structurally; these three carry it
        // only through the actor annotation — the forced revoke most
        // importantly, where the W6 authorization names the human
        // operator, not the daemon host that presented the credential.
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut core, grant) = granted(dir.path(), 1_000);
        core.renew(
            &volume(1),
            &renew_request(3, 1, grant.epoch, grant.lease_id),
            1_020,
            &caller(1),
        )
        .expect("renew by the holder");
        let mut forced = revoke_request(4, 2, WriterEpoch(1));
        forced.authorization = Some(RevocationAuthorization {
            operator: "op@example".to_owned(),
            reason: "host-1 partitioned; forced failover".to_owned(),
        });
        core.revoke(&volume(1), &forced, 1_050, &caller(2))
            .expect("forced revocation by host-2");
        drop(core);
        let mutations = journaled_mutations(dir.path());
        let renew_actor = mutations
            .iter()
            .find_map(|mutation| match mutation {
                Mutation::Renew { actor, .. } => Some(actor.clone()),
                _ => None,
            })
            .expect("a journaled renew");
        assert_eq!(renew_actor, Some(host(1)));
        let revoke_actor = mutations
            .iter()
            .find_map(|mutation| match mutation {
                Mutation::Revoke { actor, .. } => Some(actor.clone()),
                _ => None,
            })
            .expect("a journaled revoke");
        assert_eq!(
            revoke_actor,
            Some(host(2)),
            "the forced revoke records the acting daemon host"
        );

        // The batch self-release journals the one host releasing the set.
        let set_dir = tempfile::tempdir().expect("tempdir");
        let mut pair = registered_pair(set_dir.path());
        pair.grant(&volume(1), &grant_request(10, 1), 1_100, &caller(1))
            .expect("grant vol-1");
        pair.grant(&volume(2), &grant_request(11, 1), 1_100, &caller(1))
            .expect("grant vol-2");
        pair.revoke_set(
            &revoke_set_request(12, 1, vec![batch_release(1, WriterEpoch(1))]),
            1_150,
            &caller(1),
        )
        .expect("revoke set");
        drop(pair);
        let set_actor = journaled_mutations(set_dir.path())
            .into_iter()
            .find_map(|mutation| match mutation {
                Mutation::RevokeSet { actor, .. } => Some(actor.clone()),
                _ => None,
            })
            .expect("a journaled revoke set");
        assert_eq!(set_actor, Some(host(1)));
    }

    #[test]
    fn w8_mutations_require_the_bound_host_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut core, grant) = granted(dir.path(), 1_000);
        let epoch = grant.epoch;

        // A legacy (shared-token) caller is refused on EVERY mutation —
        // there is no shared-token path that could forge a holder
        // assertion.
        assert_eq!(
            core.register(&volume(2), &register_request(10), 1_000, &LEGACY)
                .expect_err("legacy register refused"),
            WitnessError::IdentityRequired
        );
        assert_eq!(
            core.grant(&volume(1), &grant_request(11, 1), 1_050, &LEGACY)
                .expect_err("legacy grant refused"),
            WitnessError::IdentityRequired
        );
        assert_eq!(
            core.renew(
                &volume(1),
                &renew_request(12, 1, epoch, grant.lease_id),
                1_050,
                &LEGACY
            )
            .expect_err("legacy renew refused"),
            WitnessError::IdentityRequired
        );
        assert_eq!(
            core.revoke(&volume(1), &revoke_request(13, 1, epoch), 1_050, &LEGACY)
                .expect_err("legacy revoke refused"),
            WitnessError::IdentityRequired
        );
        assert_eq!(
            core.record_barrier(
                &volume(1),
                &record_barrier_request(14, 1, epoch),
                1_050,
                &LEGACY
            )
            .expect_err("legacy record-barrier refused"),
            WitnessError::IdentityRequired
        );
        assert_eq!(
            core.void_barrier(
                &volume(1),
                &void_barrier_request(15, 3, 1, epoch),
                1_050,
                &LEGACY
            )
            .expect_err("legacy void-barrier refused"),
            WitnessError::IdentityRequired
        );
        assert_eq!(
            core.revoke_set(
                &revoke_set_request(16, 1, vec![batch_release(1, epoch)]),
                1_050,
                &LEGACY
            )
            .expect_err("legacy revoke-set refused"),
            WitnessError::IdentityRequired
        );
        assert_eq!(
            core.grant_set(&grant_set_request(17, 1, vec![volume(1)]), 1_050, &LEGACY)
                .expect_err("legacy grant-set refused"),
            WitnessError::IdentityRequired
        );

        // A host credential asserting a DIFFERENT host is refused the
        // same way (the credential binds the assertion, not just the
        // route).
        assert_eq!(
            core.grant(&volume(1), &grant_request(18, 1), 1_050, &caller(2))
                .expect_err("cross-host grant refused"),
            WitnessError::IdentityRequired
        );
        assert_eq!(
            core.revoke(&volume(1), &revoke_request(19, 1, epoch), 1_050, &caller(2))
                .expect_err("cross-host self-revoke refused"),
            WitnessError::IdentityRequired
        );
        assert_eq!(
            core.record_barrier(
                &volume(1),
                &record_barrier_request(20, 1, epoch),
                1_050,
                &caller(2)
            )
            .expect_err("cross-host record-barrier refused"),
            WitnessError::IdentityRequired
        );

        // Nothing was journaled by any refused mutation.
        assert_eq!(core.commit_index(), 2, "register + grant only");

        // A host credential may register (the registering daemon acts
        // for one of the endpoints; the content names both).
        core.register(&volume(2), &register_request(21), 1_000, &caller(2))
            .expect("host-credential register");

        // Reads are open to both identity classes.
        assert!(core.inspect(&volume(1), 1_050).is_ok());
    }

    // ------------------------------------------------------ P4b W9

    #[test]
    fn w9_record_barrier_accepts_only_the_live_epoch_holder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut core, grant) = granted(dir.path(), 1_000);
        let epoch = grant.epoch;

        // Accepted from the epoch holder while live; the witness stamps
        // the boundary commit index (its own mutation's index) and the
        // recording time.
        let recorded = core
            .record_barrier(
                &volume(1),
                &record_barrier_request(3, 1, epoch),
                1_020,
                &caller(1),
            )
            .expect("record barrier");
        assert_eq!(recorded.barrier.holder, host(1));
        assert_eq!(recorded.barrier.epoch, epoch);
        assert_eq!(recorded.barrier.recorded_at, 1_020);
        assert!(!recorded.barrier.voided);
        assert_eq!(recorded.barrier.boundary_commit_index, core.commit_index());
        assert!(recorded.barrier.boundary_commit_index > grant.fencing_proof.commit_index);

        // Stale epoch: refused typed.
        assert_eq!(
            core.record_barrier(
                &volume(1),
                &record_barrier_request(4, 1, WriterEpoch(5)),
                1_020,
                &caller(1)
            )
            .expect_err("stale-epoch barrier refused"),
            WitnessError::StaleEpoch {
                current_epoch: epoch
            }
        );

        // Another host asserting itself as the recorder of a lease it
        // does not hold: LeaseHeld (it is not the holder), and the
        // identity check would refuse it besides.
        assert_eq!(
            core.record_barrier(
                &volume(1),
                &record_barrier_request(5, 2, epoch),
                1_020,
                &caller(2)
            )
            .expect_err("non-holder barrier refused"),
            WitnessError::LeaseHeld {
                current_epoch: epoch
            }
        );

        // An expired lease is not a live serving boundary: refused.
        assert_eq!(
            core.record_barrier(
                &volume(1),
                &record_barrier_request(6, 1, epoch),
                1_100,
                &caller(1)
            )
            .expect_err("expired-lease barrier refused"),
            WitnessError::StaleEpoch {
                current_epoch: epoch
            }
        );

        // An unregistered volume: typed not-found.
        assert_eq!(
            core.record_barrier(
                &volume(9),
                &record_barrier_request(7, 1, epoch),
                1_020,
                &caller(1)
            )
            .expect_err("unknown volume"),
            WitnessError::UnknownVolume
        );
    }

    #[test]
    fn w9_record_barrier_replays_conflicts_and_appends() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut core, grant) = granted(dir.path(), 1_000);
        let epoch = grant.epoch;

        let first = core
            .record_barrier(
                &volume(1),
                &record_barrier_request(3, 1, epoch),
                1_020,
                &caller(1),
            )
            .expect("record barrier");
        // Byte-identical retry: replays the recorded response.
        let replay = core
            .record_barrier(
                &volume(1),
                &record_barrier_request(3, 1, epoch),
                1_030,
                &caller(1),
            )
            .expect("barrier retry replays");
        assert_eq!(replay, first);
        // Same operation id, different content: typed conflict — a
        // journaled barrier is immutable.
        let mut conflicting = record_barrier_request(3, 1, epoch);
        conflicting.attestation.peer_up_to_date = false;
        assert_eq!(
            core.record_barrier(&volume(1), &conflicting, 1_030, &caller(1))
                .expect_err("differing re-record conflicts"),
            WitnessError::IdempotencyConflict
        );

        // A second barrier for the same epoch is allowed — it appends;
        // the classifier picks evidence, the witness records.
        let second = core
            .record_barrier(
                &volume(1),
                &record_barrier_request(4, 1, epoch),
                1_040,
                &caller(1),
            )
            .expect("second barrier appends");
        assert_ne!(
            second.barrier.boundary_commit_index,
            first.barrier.boundary_commit_index
        );
        let view = core.inspect(&volume(1), 1_040).expect("inspect");
        assert_eq!(view.barriers.len(), 2);
        assert!(view.barriers.iter().all(|barrier| !barrier.voided));
    }

    #[test]
    fn w9_void_barrier_only_before_retirement_and_only_by_the_recorder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut core, grant) = granted(dir.path(), 1_000);
        let epoch = grant.epoch;
        let recorded = core
            .record_barrier(
                &volume(1),
                &record_barrier_request(3, 1, epoch),
                1_020,
                &caller(1),
            )
            .expect("record barrier");

        // Void from a credential that is not the recording holder:
        // refused (W8).
        assert_eq!(
            core.void_barrier(
                &volume(1),
                &void_barrier_request(4, 3, 1, epoch),
                1_030,
                &caller(2)
            )
            .expect_err("cross-host void refused"),
            WitnessError::IdentityRequired
        );

        // A void with no matching barrier (wrong migration): refused.
        let mut unmatched = void_barrier_request(5, 3, 1, epoch);
        unmatched.migration_id =
            Some(volvisor_types::MigrationId::new("mig-other").expect("valid migration id"));
        assert!(matches!(
            core.void_barrier(&volume(1), &unmatched, 1_030, &caller(1))
                .expect_err("unmatched void refused"),
            WitnessError::InvalidRequest(_)
        ));

        // The rightful void before retirement: the log shows voided.
        let voided = core
            .void_barrier(
                &volume(1),
                &void_barrier_request(6, 3, 1, epoch),
                1_030,
                &caller(1),
            )
            .expect("void before retirement");
        assert_eq!(
            voided.barrier.boundary_commit_index,
            recorded.barrier.boundary_commit_index
        );
        assert!(voided.barrier.voided);
        let view = core.inspect(&volume(1), 1_030).expect("inspect");
        assert_eq!(view.barriers.len(), 1);
        assert!(view.barriers[0].voided);

        // Voiding again (no non-voided barrier remains): refused.
        assert!(matches!(
            core.void_barrier(
                &volume(1),
                &void_barrier_request(7, 3, 1, epoch),
                1_030,
                &caller(1)
            )
            .expect_err("double void refused"),
            WitnessError::InvalidRequest(_)
        ));

        // After the epoch retires (self-release + a new grant), a void
        // of the retired epoch is refused: evidence hygiene.
        core.revoke(&volume(1), &revoke_request(8, 1, epoch), 1_050, &caller(1))
            .expect("self-release");
        core.grant(&volume(1), &grant_request(9, 2), 1_050, &caller(2))
            .expect("new epoch");
        assert!(matches!(
            core.void_barrier(
                &volume(1),
                &void_barrier_request(10, 3, 1, epoch),
                1_060,
                &caller(1)
            )
            .expect_err("void after retirement refused"),
            WitnessError::InvalidRequest(_)
        ));
    }

    #[test]
    fn w9_barriers_and_retirements_survive_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let epoch = {
            let (mut core, grant) = granted(dir.path(), 1_000);
            core.record_barrier(
                &volume(1),
                &record_barrier_request(3, 1, grant.epoch),
                1_020,
                &caller(1),
            )
            .expect("record barrier");
            grant.epoch
        };
        // "Crash": reopen on the same directory — the barrier log and
        // the retirement records are fold-derived, so both survive.
        let mut core = open_core(dir.path());
        let view = core.inspect(&volume(1), 1_050).expect("inspect");
        assert_eq!(view.barriers.len(), 1);
        assert_eq!(view.barriers[0].epoch, epoch);
        assert!(!view.barriers[0].voided);
        assert_eq!(view.retirements.len(), 1);
        assert_eq!(view.retirements[0].epoch, WriterEpoch::pre_authority());

        // The reopened core keeps journaling on top of the folded
        // state: a void lands on the surviving entry.
        let voided = core
            .void_barrier(
                &volume(1),
                &void_barrier_request(4, 3, 1, epoch),
                1_050,
                &caller(1),
            )
            .expect("void after reopen");
        assert!(voided.barrier.voided);
    }

    // ------------------------------------------------------ P4b W10

    #[test]
    fn w10_revoke_set_refusals_journal_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut core = registered_pair(dir.path());
        core.grant(&volume(1), &grant_request(2, 1), 1_000, &caller(1))
            .expect("grant vol-1");
        core.grant(&volume(2), &grant_request(3, 1), 1_000, &caller(1))
            .expect("grant vol-2");
        let commit = core.commit_index();

        // One bad member (an unregistered volume): the whole batch is
        // refused and NOTHING is journaled — the commit index is
        // unchanged and no member's lease flipped.
        assert_eq!(
            core.revoke_set(
                &revoke_set_request(
                    4,
                    1,
                    vec![
                        batch_release(1, WriterEpoch(1)),
                        batch_release(9, WriterEpoch(1))
                    ],
                ),
                1_050,
                &caller(1)
            )
            .expect_err("unknown member refuses the batch"),
            WitnessError::UnknownVolume
        );
        assert_eq!(core.commit_index(), commit, "nothing journaled");
        assert_eq!(
            core.inspect(&volume(1), 1_050)
                .expect("inspect")
                .lease_state,
            LeaseState::Live
        );

        // A member held by another host is not a self-release: the
        // batch host's own credential is valid, the batch still
        // refuses typed.
        assert!(matches!(
            core.revoke_set(
                &revoke_set_request(5, 2, vec![batch_release(1, WriterEpoch(1))]),
                1_050,
                &caller(2)
            )
            .expect_err("foreign-held member refuses the batch"),
            WitnessError::InvalidRequest(_)
        ));

        // Empty and duplicate member sets are refused.
        assert!(matches!(
            core.revoke_set(&revoke_set_request(6, 1, vec![]), 1_050, &caller(1))
                .expect_err("empty batch refused"),
            WitnessError::InvalidRequest(_)
        ));
        assert!(matches!(
            core.revoke_set(
                &revoke_set_request(
                    7,
                    1,
                    vec![
                        batch_release(1, WriterEpoch(1)),
                        batch_release(1, WriterEpoch(1))
                    ],
                ),
                1_050,
                &caller(1)
            )
            .expect_err("duplicate member refused"),
            WitnessError::InvalidRequest(_)
        ));
        assert_eq!(core.commit_index(), commit, "nothing journaled");
    }

    #[test]
    fn w10_revoke_set_is_all_or_nothing_with_one_bump() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut core = registered_pair(dir.path());
        core.grant(&volume(1), &grant_request(2, 1), 1_000, &caller(1))
            .expect("grant vol-1");
        core.grant(&volume(2), &grant_request(3, 1), 1_000, &caller(1))
            .expect("grant vol-2");
        let commit = core.commit_index();

        // The good batch: every member's proof shares the single new
        // commit index; one bump for the set.
        let outcome = core
            .revoke_set(
                &revoke_set_request(
                    8,
                    1,
                    vec![
                        batch_release(1, WriterEpoch(1)),
                        batch_release(2, WriterEpoch(1)),
                    ],
                ),
                1_050,
                &caller(1),
            )
            .expect("revoke-set");
        assert_eq!(core.commit_index(), commit + 1, "one bump per batch");
        assert_eq!(outcome.releases.len(), 2);
        for member in &outcome.releases {
            assert_eq!(member.fencing_proof.commit_index, commit + 1);
            assert_eq!(member.fencing_proof.retired_epoch, WriterEpoch(1));
        }
        for member in [volume(1), volume(2)] {
            let view = core.inspect(&member, 1_050).expect("inspect");
            assert_eq!(view.lease_state, LeaseState::Revoked);
            assert_eq!(view.commit_index, commit + 1);
            // The retirement is recorded at the batch's own index.
            assert!(view.retirements.iter().any(
                |retired| retired.epoch == WriterEpoch(1) && retired.commit_index == commit + 1
            ));
        }

        // A byte-identical retry with the same operation id replays
        // the recorded outcome (no second bump).
        let replay = core
            .revoke_set(
                &revoke_set_request(
                    8,
                    1,
                    vec![
                        batch_release(1, WriterEpoch(1)),
                        batch_release(2, WriterEpoch(1)),
                    ],
                ),
                1_060,
                &caller(1),
            )
            .expect("batch retry replays");
        assert_eq!(replay, outcome);
        assert_eq!(core.commit_index(), commit + 1);
    }

    #[test]
    fn w10_revoke_set_member_idempotence_uses_the_recorded_proof() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut core = registered_pair(dir.path());
        core.grant(&volume(1), &grant_request(2, 1), 1_000, &caller(1))
            .expect("grant vol-1");
        core.grant(&volume(2), &grant_request(3, 1), 1_000, &caller(1))
            .expect("grant vol-2");
        // First batch releases only vol-1.
        core.revoke_set(
            &revoke_set_request(4, 1, vec![batch_release(1, WriterEpoch(1))]),
            1_050,
            &caller(1),
        )
        .expect("revoke-set");
        let recorded_proof = core
            .inspect(&volume(1), 1_050)
            .expect("inspect")
            .retirements
            .iter()
            .find(|retired| retired.epoch == WriterEpoch(1))
            .copied()
            .map(|retired| (retired.commit_index, retired.epoch))
            .expect("recorded retirement");
        let commit = core.commit_index();

        // A later batch including the already-released member confirms
        // it with its recorded proof (state-idempotent), while the new
        // member retires at the batch's shared index.
        let outcome = core
            .revoke_set(
                &revoke_set_request(
                    5,
                    1,
                    vec![
                        batch_release(1, WriterEpoch(1)),
                        batch_release(2, WriterEpoch(1)),
                    ],
                ),
                1_060,
                &caller(1),
            )
            .expect("mixed batch");
        let by_volume = |n: u64| {
            outcome
                .releases
                .iter()
                .find(|member| member.volume_id == volume(n))
                .cloned()
                .expect("member outcome")
        };
        assert_eq!(
            by_volume(1).fencing_proof.commit_index,
            recorded_proof.0,
            "recorded proof, not the batch index"
        );
        assert_eq!(by_volume(2).fencing_proof.commit_index, commit + 1);
        // A second retirement record was not appended for epoch 1.
        let view = core.inspect(&volume(1), 1_060).expect("inspect");
        assert_eq!(
            view.retirements
                .iter()
                .filter(|retired| retired.epoch == WriterEpoch(1))
                .count(),
            1
        );
    }

    #[test]
    fn w10_grant_set_refusals_journal_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut core = registered_pair(dir.path());
        core.grant(&volume(1), &grant_request(2, 1), 1_000, &caller(1))
            .expect("grant vol-1");
        core.grant(&volume(2), &grant_request(3, 1), 1_000, &caller(1))
            .expect("grant vol-2");
        // The source self-releases both (waives the W7 wait) so a
        // member's live lease is not what refuses the batch first.
        core.revoke_set(
            &revoke_set_request(
                4,
                1,
                vec![
                    batch_release(1, WriterEpoch(1)),
                    batch_release(2, WriterEpoch(1)),
                ],
            ),
            1_050,
            &caller(1),
        )
        .expect("revoke-set");
        let commit = core.commit_index();

        // One unregistered member: nothing journaled.
        assert_eq!(
            core.grant_set(
                &grant_set_request(5, 2, vec![volume(1), volume(9)]),
                1_060,
                &caller(2)
            )
            .expect_err("unknown member refuses the batch"),
            WitnessError::UnknownVolume
        );
        assert_eq!(core.commit_index(), commit, "nothing journaled");

        // One member with a live lease: the whole batch refuses (W1).
        core.grant(&volume(2), &grant_request(6, 2), 1_060, &caller(2))
            .expect("live grant on vol-2");
        assert_eq!(
            core.grant_set(
                &grant_set_request(7, 2, vec![volume(1), volume(2)]),
                1_060,
                &caller(2)
            )
            .expect_err("live member refuses the batch"),
            WitnessError::LeaseHeld {
                current_epoch: WriterEpoch(2)
            }
        );
        assert_eq!(core.commit_index(), commit + 1, "only the single grant");
    }

    #[test]
    fn w10_grant_set_is_all_or_nothing_and_retires_lingering_epochs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut core = registered_pair(dir.path());
        core.grant(&volume(1), &grant_request(2, 1), 1_000, &caller(1))
            .expect("grant vol-1");
        core.grant(&volume(2), &grant_request(3, 1), 1_000, &caller(1))
            .expect("grant vol-2");
        // The source self-releases both (waives the W7 wait).
        core.revoke_set(
            &revoke_set_request(
                4,
                1,
                vec![
                    batch_release(1, WriterEpoch(1)),
                    batch_release(2, WriterEpoch(1)),
                ],
            ),
            1_050,
            &caller(1),
        )
        .expect("revoke-set");
        let commit = core.commit_index();

        // vol-2 passes through a grant/release cycle for the W1 check;
        // undo its live lease so the batch can proceed.
        core.grant(&volume(2), &grant_request(6, 2), 1_060, &caller(2))
            .expect("live grant on vol-2");
        core.revoke(
            &volume(2),
            &revoke_request(8, 2, WriterEpoch(2)),
            1_060,
            &caller(2),
        )
        .expect("self-release vol-2");

        // The good batch: new epochs per member, one bump, retirements
        // recorded at the shared index. (vol-2 passed through an extra
        // grant/release cycle for the W1 check, so its new epoch is 3
        // and the batch retires epoch 2 there; vol-1 goes 1 → 2.)
        let outcome = core
            .grant_set(
                &grant_set_request(9, 2, vec![volume(1), volume(2)]),
                1_060,
                &caller(2),
            )
            .expect("grant-set");
        let shared = core.commit_index();
        assert_eq!(
            shared,
            commit + 3,
            "single grant + release + one batch bump"
        );
        assert_eq!(outcome.grants.len(), 2);
        let expected = [
            (volume(1), WriterEpoch(2), WriterEpoch(1)),
            (volume(2), WriterEpoch(3), WriterEpoch(2)),
        ];
        let mut lease_ids = Vec::new();
        for grant in &outcome.grants {
            let (_, new_epoch, retired_epoch) = expected
                .iter()
                .find(|(vol, _, _)| *vol == grant.volume_id)
                .expect("known member");
            assert_eq!(grant.epoch, *new_epoch);
            assert_eq!(grant.lease_ttl_secs, 100);
            assert_eq!(grant.fencing_proof.commit_index, shared);
            assert_eq!(grant.fencing_proof.retired_epoch, *retired_epoch);
            lease_ids.push(grant.lease_id);
            let view = core.inspect(&grant.volume_id, 1_060).expect("inspect");
            assert_eq!(view.current_epoch, *new_epoch);
            assert_eq!(view.holder, Some(host(2)));
            assert_eq!(view.lease_state, LeaseState::Live);
            assert_eq!(view.commit_index, shared);
            // The member's previous epoch is sealed in the retirement
            // log at its ORIGINAL retirement index (the revoke-set or
            // revoke that first retired it) — the grant-set's dedup
            // never re-appends, and never rewrites a sealed record.
            let sealed = view
                .retirements
                .iter()
                .find(|retired| retired.epoch == *retired_epoch)
                .copied()
                .expect("previous epoch retired");
            assert!(sealed.commit_index < shared);
        }
        // Fresh leases, never reused.
        assert_ne!(lease_ids[0], lease_ids[1]);

        // A byte-identical retry replays the recorded outcome.
        let replay = core
            .grant_set(
                &grant_set_request(9, 2, vec![volume(1), volume(2)]),
                1_070,
                &caller(2),
            )
            .expect("batch retry replays");
        assert_eq!(replay, outcome);
        assert_eq!(core.commit_index(), shared);
    }

    #[test]
    fn w10_grant_set_waits_out_the_fence_window_set_wide() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut core = registered_pair(dir.path());
        core.grant(&volume(1), &grant_request(2, 1), 1_000, &caller(1))
            .expect("grant vol-1");
        core.grant(&volume(2), &grant_request(3, 1), 1_000, &caller(1))
            .expect("grant vol-2");
        // Let both leases expire (end 1_100) but stay inside the fence
        // window (until 1_110): the batch refuses on the FIRST member
        // inside the window — all-or-nothing, W7 set-wide.
        assert_eq!(
            core.grant_set(
                &grant_set_request(4, 2, vec![volume(1), volume(2)]),
                1_105,
                &caller(2)
            )
            .expect_err("fence window holds the batch"),
            WitnessError::FencePending {
                retry_after_secs: 5
            }
        );
        // Past the window: the lingering epochs retire set-wide.
        let outcome = core
            .grant_set(
                &grant_set_request(5, 2, vec![volume(1), volume(2)]),
                1_110,
                &caller(2),
            )
            .expect("grant-set after the window");
        for grant in &outcome.grants {
            assert_eq!(grant.epoch, WriterEpoch(2));
            let view = core.inspect(&grant.volume_id, 1_110).expect("inspect");
            assert!(
                view.retirements
                    .iter()
                    .any(|retired| retired.epoch == WriterEpoch(1)
                        && retired.commit_index == grant.fencing_proof.commit_index)
            );
        }
    }

    #[test]
    fn retirement_records_never_double_append() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut core = registered(dir.path());
        // Grant retires the pre-authority epoch 0.
        core.grant(&volume(1), &grant_request(2, 1), 1_000, &caller(1))
            .expect("grant");
        // Revoke retires epoch 1.
        core.revoke(
            &volume(1),
            &revoke_request(3, 1, WriterEpoch(1)),
            1_050,
            &caller(1),
        )
        .expect("self-release");
        let view = core.inspect(&volume(1), 1_050).expect("inspect");
        assert_eq!(view.retirements.len(), 2);
        // A grant of the next epoch "retires" epoch 1 again — the
        // record must not double-append (the revoke already sealed it).
        core.grant(&volume(1), &grant_request(4, 2), 1_050, &caller(2))
            .expect("grant");
        let view = core.inspect(&volume(1), 1_050).expect("inspect");
        assert_eq!(view.retirements.len(), 2);
        assert_eq!(view.retirements[0].epoch, WriterEpoch::pre_authority());
        assert_eq!(view.retirements[1].epoch, WriterEpoch(1));
        // The sealed record keeps the revoke's commit index (3), not
        // the later grant's (4).
        assert_eq!(view.retirements[1].commit_index, 3);
    }
}
