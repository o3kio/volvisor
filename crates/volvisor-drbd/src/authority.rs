//! # Writer-authority context (the provider's witness boundary)
//!
//! [`AuthorityContext`] is everything the DRBD engine needs from the
//! witness (P4a plan §4): lease acquisition before promotion, renewal
//! with W5 **duration-from-response** local deadlines, self-initiated
//! release on detach, and lease validation for the fail-closed startup
//! and reconcile paths. It is deliberately synchronous (over a
//! [`BlockingWitnessConnection`]) because the engine's control paths
//! hold their state lock end-to-end; the blocking adapter bounds every
//! wait by the connection's request timeout (see
//! `volvisor_witness::blocking`).
//!
//! ## Deadline discipline (W5)
//!
//! The writer never interprets absolute witness timestamps: every
//! deadline it keeps is `local time at response receipt + the duration
//! the response carried`. The residual skew is bounded by response
//! latency, which the witness's `lease_grace_secs` budget covers (the
//! plan's documented timing assumption). A grant response received via
//! an idempotent *replay* would anchor a deadline later than the
//! witness's lease end, so the provider persists the authority block
//! **before** promoting and resumes interrupted attaches through the
//! renewal path — a replayed grant response is never used to anchor a
//! deadline the writer serves under.
//!
//! ## Renewal-interval guard (availability, not a fence)
//!
//! `renewal_interval_secs < ttl / 2` is checked against the TTL every
//! grant/renew response carries (plan §6) — lazily and continuously,
//! never at startup (where no response exists). A violation refuses the
//! acquisition/renewal so a writer never attaches with a renewal cadence
//! that could let its lease lapse between renewals. Fencing correctness
//! never depends on this check (the W7 wait is computed from the lease's
//! recorded end at the witness regardless).
//!
//! ## Honesty
//!
//! `evidence_status: PrototypeOnly`. The lease bounds — but cannot
//! prove — the absence of a second writer; see the plan's dual-write
//! window analysis (§2). This module never claims otherwise.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use volvisor_types::ID_MAX_LEN;
use volvisor_types::error::{ApiError, ApiErrorCode};
use volvisor_types::{AuthorityView, HostId, LeaseState, OperationId, VolumeId};
use volvisor_witness::BlockingWitnessConnection;
use volvisor_witness::proto::{
    GrantRequest, RegisterRequest, RegistrationContent, RenewRequest, RevokeRequest,
    WITNESS_PROTOCOL_VERSION, WitnessError,
};

use crate::state::VolumeAuthorityBlock;

/// The writer-side authority context.
pub struct AuthorityContext {
    connection: Arc<dyn BlockingWitnessConnection>,
    host_id: HostId,
    renewal_interval_secs: u64,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Freshness nonce for witness operation ids (unique per attempt by
    /// construction: real-clock nanos + a process-local counter).
    operation_nonce: AtomicU64,
}

/// Outcome of validating a lease against the witness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseValidity {
    /// The lease is live, held by this host, at the recorded epoch, with
    /// a remaining duration covering the renewal margin.
    Valid {
        /// Remaining lease seconds as of the inspect response.
        remaining_secs: u64,
    },
    /// The lease is provably not ours to serve under (expired, revoked,
    /// superseded, or held by another host). The caller self-fences.
    Invalid {
        /// Stable, human-readable reasons (diagnostic).
        reasons: Vec<String>,
    },
}

impl AuthorityContext {
    /// Build the context. `now` is the local clock (unix seconds) used
    /// for W5 deadline anchoring; tests inject a controllable one.
    ///
    /// # Errors
    /// Typed invalid-request error when the renewal interval is not
    /// positive.
    pub fn new(
        connection: Arc<dyn BlockingWitnessConnection>,
        host_id: HostId,
        renewal_interval_secs: u64,
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<Self, ApiError> {
        if renewal_interval_secs == 0 {
            return Err(ApiError::invalid_request(
                "renewal_interval_secs must be positive",
            ));
        }
        Ok(Self {
            connection,
            host_id,
            renewal_interval_secs,
            now,
            operation_nonce: AtomicU64::new(0),
        })
    }

    /// The host identity all leases are acquired for.
    #[must_use]
    pub fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// The configured renewal cadence (seconds).
    #[must_use]
    pub fn renewal_interval_secs(&self) -> u64 {
        self.renewal_interval_secs
    }

    /// Local unix seconds (the W5 anchor clock).
    #[must_use]
    pub fn now_secs(&self) -> u64 {
        (self.now)()
    }

    /// A fresh witness operation id (unique per attempt — see the
    /// module docs on why grant responses are never replayed to anchor
    /// deadlines). The volume-identity portion is trimmed so the id
    /// fits `ID_MAX_LEN` for any valid volume id; the unique nonce
    /// suffix is never trimmed.
    ///
    /// # Errors
    /// `INTERNAL` when no candidate validates — never a panic, never a
    /// reused id (a collision would surface as the witness's typed
    /// `IDEMPOTENCY_CONFLICT`, which is safe; a reused id is not).
    fn operation_id(&self, kind: &str, volume_id: &VolumeId) -> Result<OperationId, ApiError> {
        let nonce = self.operation_nonce.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        // 48 chars cover "-{nanos(20)}-{nonce(20)}" with slack.
        let prefix: String = format!("drbd-{kind}-{}", volume_id.as_str())
            .chars()
            .take(ID_MAX_LEN - 48)
            .collect();
        OperationId::new(format!("{prefix}-{nanos}-{nonce}"))
            .or_else(|_| OperationId::new(format!("drbd-{kind}-{nanos}-{nonce}")))
            .map_err(|error| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!("failed to mint a unique witness operation id: {error}"),
                )
            })
    }

    /// Acquire writer authority for a promotion (plan §4 attach): renew
    /// a recorded lease when one exists (fresh W5 deadline), otherwise
    /// grant a new epoch. A recorded lease that is no longer renewable
    /// (expired, revoked, superseded — typed `STALE_EPOCH`) falls
    /// through to a grant, which surfaces `LEASE_HELD` or
    /// `FENCE_PENDING` refusals from the witness as appropriate.
    ///
    /// The returned block must be persisted **before** the promotion it
    /// authorizes.
    ///
    /// # Errors
    /// Typed refusals: [`ApiErrorCode::LeaseHeld`],
    /// [`ApiErrorCode::FencePending`], [`ApiErrorCode::StaleEpoch`],
    /// [`ApiErrorCode::UnknownFencingAuthority`] (witness unreachable),
    /// [`ApiErrorCode::InvalidState`] (volume not registered), and the
    /// renewal-interval guard's [`ApiErrorCode::InvalidRequest`].
    pub fn acquire(
        &self,
        volume_id: &VolumeId,
        prior: Option<&VolumeAuthorityBlock>,
    ) -> Result<VolumeAuthorityBlock, ApiError> {
        if let Some(prior) = prior {
            match self.renew_lease(volume_id, prior) {
                Ok(block) => return Ok(block),
                Err(err) if matches!(err.code, ApiErrorCode::StaleEpoch) => {
                    // The recorded lease is gone; a fresh grant is the
                    // honest next step (it may in turn be refused).
                }
                Err(err) => return Err(err),
            }
        }
        let response = self
            .connection
            .grant(
                volume_id,
                GrantRequest {
                    protocol_version: WITNESS_PROTOCOL_VERSION,
                    operation_id: self.operation_id("grant", volume_id)?,
                    host_id: self.host_id.clone(),
                },
            )
            .map_err(witness_error)?;
        // The availability guard runs on every response (plan §6).
        guard_renewal_interval(self.renewal_interval_secs, response.lease_ttl_secs)?;
        let now = self.now_secs();
        Ok(VolumeAuthorityBlock {
            epoch: response.epoch,
            lease_id: response.lease_id,
            lease_proof_ref: response.fencing_proof.commit_index,
            authority_commit_index: response.fencing_proof.commit_index,
            acquired_at: now,
            deadline_at: now.saturating_add(response.lease_ttl_secs),
        })
    }

    /// Renew a recorded lease, returning the block with the refreshed
    /// W5 deadline (anchored at this response).
    ///
    /// # Errors
    /// Typed refusals; in particular [`ApiErrorCode::StaleEpoch`] when
    /// the lease is no longer renewable (the caller of [`Self::acquire`]
    /// falls through to a grant; the renewal loop self-fences).
    pub fn renew_lease(
        &self,
        volume_id: &VolumeId,
        block: &VolumeAuthorityBlock,
    ) -> Result<VolumeAuthorityBlock, ApiError> {
        let response = self
            .connection
            .renew(
                volume_id,
                RenewRequest {
                    protocol_version: WITNESS_PROTOCOL_VERSION,
                    operation_id: self.operation_id("renew", volume_id)?,
                    host_id: self.host_id.clone(),
                    epoch: block.epoch,
                    lease_id: block.lease_id,
                },
            )
            .map_err(witness_error)?;
        // The shrink-after-grant case is first observable here, on an
        // already-attached volume: a violating renew response refuses
        // the renewal (plan §6) and the caller follows the local
        // deadline self-fence path.
        guard_renewal_interval(self.renewal_interval_secs, response.remaining_secs)?;
        let now = self.now_secs();
        Ok(VolumeAuthorityBlock {
            epoch: response.epoch,
            deadline_at: now.saturating_add(response.remaining_secs),
            ..block.clone()
        })
    }

    /// Release a lease the holder no longer needs (the detach path —
    /// the releasing host has already demoted, so the witness starts no
    /// W7 wait). Best-effort by policy: a failure leaves the lease to
    /// lapse at its recorded end, which W1/W7 bound harmlessly (plan
    /// §4); the caller records the failure, never hides it.
    ///
    /// # Errors
    /// The typed witness refusal, for the caller to report.
    pub fn release(
        &self,
        volume_id: &VolumeId,
        block: &VolumeAuthorityBlock,
    ) -> Result<(), WitnessError> {
        self.connection.revoke(
            volume_id,
            RevokeRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: self
                    .operation_id("release", volume_id)
                    .map_err(|error| WitnessError::Internal(error.detail))?,
                host_id: self.host_id.clone(),
                epoch: block.epoch,
                authorization: None,
                power_off: None,
            },
        )?;
        Ok(())
    }

    /// Validate a recorded lease against the witness (the startup
    /// resume gate and the reconcile check, plan §4): live, held by this
    /// host, at the recorded epoch, and with a remaining duration
    /// covering the renewal margin — anything shorter could lapse
    /// before the next renewal lands, leaving the writer without a
    /// W5-conformant deadline.
    ///
    /// # Errors
    /// [`ApiErrorCode::UnknownFencingAuthority`] when the witness
    /// cannot be reached (the caller fail-closes: stays suspended).
    pub fn validate(
        &self,
        volume_id: &VolumeId,
        block: &VolumeAuthorityBlock,
    ) -> Result<LeaseValidity, ApiError> {
        let view = self.connection.inspect(volume_id).map_err(witness_error)?;
        Ok(self.validity_of(&view, block))
    }

    /// Inspect the witness view of a volume (adopt flow authority
    /// check; an unregistered volume surfaces as a typed
    /// `INVALID_STATE` error).
    ///
    /// # Errors
    /// [`ApiErrorCode::UnknownFencingAuthority`] when the witness
    /// cannot be reached; [`ApiErrorCode::InvalidState`] when the
    /// volume is not registered.
    pub fn inspect(&self, volume_id: &VolumeId) -> Result<AuthorityView, ApiError> {
        self.connection.inspect(volume_id).map_err(witness_error)
    }

    /// Register a volume lineage (P4a plan §3) — the explicit,
    /// out-of-band provisioning step that makes a volume
    /// witness-managed (nothing automatic registers: the optional
    /// operator-attested barrier is exactly an operator decision, and
    /// an auto-registration without one would permanently foreclose
    /// `SAFE_CURRENT` for that lineage).
    ///
    /// The witness is idempotent by **content**: an identical
    /// re-registration replays; diverging content (e.g. a recreated
    /// lineage) is refused typed. The operation id is fresh per call —
    /// a registration carries no lease, so a lost response retried
    /// with identical content still succeeds.
    ///
    /// # Errors
    /// Typed witness refusals ([`ApiErrorCode::InvalidState`] for
    /// `ALREADY_REGISTERED` divergence), or
    /// [`ApiErrorCode::UnknownFencingAuthority`] when the witness
    /// cannot be reached.
    pub fn register(
        &self,
        volume_id: &VolumeId,
        content: RegistrationContent,
    ) -> Result<volvisor_witness::proto::RegisterResponse, ApiError> {
        self.connection
            .register(
                volume_id,
                RegisterRequest {
                    protocol_version: WITNESS_PROTOCOL_VERSION,
                    operation_id: self.operation_id("register", volume_id)?,
                    content,
                },
            )
            .map_err(witness_error)
    }

    /// Classify a witness view against a recorded block (shared by
    /// [`Self::validate`] and reconcile).
    #[must_use]
    pub fn validity_of(&self, view: &AuthorityView, block: &VolumeAuthorityBlock) -> LeaseValidity {
        let mut reasons = Vec::new();
        if view.current_epoch != block.epoch {
            reasons.push(format!(
                "recorded epoch {} is retired (witness epoch {})",
                block.epoch.0, view.current_epoch.0
            ));
        }
        match view.lease_state {
            LeaseState::Live => {}
            LeaseState::Expired => reasons.push("the lease has expired".to_owned()),
            LeaseState::Revoked => reasons.push("the lease was revoked".to_owned()),
            LeaseState::None => reasons.push("no lease exists for the epoch".to_owned()),
        }
        if view.holder.as_ref() != Some(&self.host_id) {
            reasons.push(format!(
                "the lease is held by {}",
                view.holder.as_ref().map_or_else(
                    || "no holder".to_owned(),
                    |holder| holder.as_str().to_owned()
                )
            ));
        }
        if reasons.is_empty() {
            match view.lease_remaining_secs {
                Some(remaining) if remaining >= self.renewal_interval_secs => {
                    LeaseValidity::Valid {
                        remaining_secs: remaining,
                    }
                }
                Some(remaining) => LeaseValidity::Invalid {
                    reasons: vec![format!(
                        "remaining lease {remaining}s is shorter than the renewal margin {}s",
                        self.renewal_interval_secs
                    )],
                },
                None => LeaseValidity::Invalid {
                    reasons: vec!["a live lease without a remaining duration".to_owned()],
                },
            }
        } else {
            LeaseValidity::Invalid { reasons }
        }
    }
}

/// The plan §6 availability guard: `renewal_interval < ttl / 2`.
fn guard_renewal_interval(renewal_interval_secs: u64, lease_secs: u64) -> Result<(), ApiError> {
    if renewal_interval_secs.saturating_mul(2) >= lease_secs {
        return Err(ApiError::invalid_request(format!(
            "witness lease duration {lease_secs}s does not admit the configured renewal \
             interval {renewal_interval_secs}s (requires renewal_interval < ttl/2); refusing \
             so the lease cannot lapse between renewals"
        )));
    }
    Ok(())
}

/// Map a typed witness refusal onto the Volume API error shape (plan
/// §4: witness refusals are typed, never swallowed). Takes the error
/// by value: the primary use is `map_err(witness_error)`.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn witness_error(err: WitnessError) -> ApiError {
    let detail = err.detail();
    match err {
        WitnessError::LeaseHeld { .. } => ApiError::new(ApiErrorCode::LeaseHeld, detail),
        WitnessError::StaleEpoch { .. } => ApiError::new(ApiErrorCode::StaleEpoch, detail),
        WitnessError::FencePending { .. } => ApiError::new(ApiErrorCode::FencePending, detail),
        WitnessError::UnknownVolume => ApiError::new(
            ApiErrorCode::InvalidState,
            "volume is not registered with the witness (register the lineage before \
             witness-managed attach)"
                .to_owned(),
        ),
        WitnessError::AlreadyRegistered => ApiError::new(ApiErrorCode::InvalidState, detail),
        // An unreachable witness is exactly "fencing authority cannot be
        // established": a new writer is never admitted without it.
        WitnessError::Unreachable(_) => {
            ApiError::new(ApiErrorCode::UnknownFencingAuthority, detail)
        }
        WitnessError::Unauthorized => ApiError::new(
            ApiErrorCode::Internal,
            "witness rejected the daemon's credentials (check witness_token)",
        ),
        WitnessError::InvalidRequest(_) => ApiError::new(ApiErrorCode::InvalidRequest, detail),
        WitnessError::IdempotencyConflict => {
            ApiError::new(ApiErrorCode::IdempotencyConflict, detail)
        }
        WitnessError::Internal(_) => ApiError::new(ApiErrorCode::Internal, detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use volvisor_types::{FencingProof, LeaseId, WriterEpoch};
    use volvisor_witness::proto::GrantResponse;

    /// A connection double the validity/guard tests never call.
    struct NoConnection;
    impl BlockingWitnessConnection for NoConnection {
        fn register(
            &self,
            _volume_id: &VolumeId,
            _request: volvisor_witness::proto::RegisterRequest,
        ) -> Result<volvisor_witness::proto::RegisterResponse, WitnessError> {
            Err(WitnessError::Internal("not used".to_owned()))
        }
        fn grant(
            &self,
            _volume_id: &VolumeId,
            _request: GrantRequest,
        ) -> Result<GrantResponse, WitnessError> {
            Err(WitnessError::Internal("not used".to_owned()))
        }
        fn renew(
            &self,
            _volume_id: &VolumeId,
            _request: RenewRequest,
        ) -> Result<volvisor_witness::proto::RenewResponse, WitnessError> {
            Err(WitnessError::Internal("not used".to_owned()))
        }
        fn revoke(
            &self,
            _volume_id: &VolumeId,
            _request: RevokeRequest,
        ) -> Result<volvisor_witness::proto::RevokeResponse, WitnessError> {
            Err(WitnessError::Internal("not used".to_owned()))
        }
        fn inspect(&self, _volume_id: &VolumeId) -> Result<AuthorityView, WitnessError> {
            Err(WitnessError::Internal("not used".to_owned()))
        }
    }

    fn context(renewal_interval: u64) -> AuthorityContext {
        AuthorityContext::new(
            Arc::new(NoConnection),
            HostId::new("node-a").expect("valid host id"),
            renewal_interval,
            Arc::new(|| 1_000),
        )
        .expect("authority context")
    }

    fn block(epoch: u64, deadline_at: u64) -> VolumeAuthorityBlock {
        VolumeAuthorityBlock {
            epoch: WriterEpoch(epoch),
            lease_id: LeaseId(1),
            lease_proof_ref: 1,
            authority_commit_index: 1,
            acquired_at: 1_000,
            deadline_at,
        }
    }

    fn view(
        epoch: u64,
        holder: Option<&HostId>,
        state: LeaseState,
        remaining: Option<u64>,
    ) -> AuthorityView {
        AuthorityView {
            volume_id: VolumeId::new("vol-1").expect("valid volume id"),
            current_epoch: WriterEpoch(epoch),
            holder: holder.cloned(),
            lease_state: state,
            lease_remaining_secs: remaining,
            commit_index: 7,
            registration: None,
        }
    }

    #[test]
    fn guard_rejects_vacuous_renewal_cadence() {
        // ttl 100, interval 50: 2*50 >= 100 — refused.
        assert!(guard_renewal_interval(50, 100).is_err());
        // ttl 100, interval 49: admitted.
        assert!(guard_renewal_interval(49, 100).is_ok());
        // A tiny witness-side ttl shrink is caught lazily (plan §6).
        assert!(guard_renewal_interval(20, 39).is_err());
        assert!(guard_renewal_interval(20, 41).is_ok());
        // Saturating arithmetic never refuses a viable cadence.
        assert!(guard_renewal_interval(u64::MAX / 4, u64::MAX).is_ok());
    }

    #[test]
    fn context_rejects_a_zero_renewal_interval() {
        let error = AuthorityContext::new(
            Arc::new(NoConnection),
            HostId::new("node-a").expect("valid host id"),
            0,
            Arc::new(|| 1_000),
        )
        .err()
        .expect("zero interval refused");
        assert_eq!(error.code, ApiErrorCode::InvalidRequest);
    }

    #[test]
    fn operation_ids_are_fresh_per_attempt() {
        let ctx = context(20);
        let volume = VolumeId::new("vol-1").expect("valid volume id");
        let first = ctx.operation_id("grant", &volume).expect("mint");
        let second = ctx.operation_id("grant", &volume).expect("mint");
        assert_ne!(
            first, second,
            "a replayed grant response must never anchor a deadline"
        );
    }

    #[test]
    fn witness_error_maps_every_refusal_typed() {
        assert_eq!(
            witness_error(WitnessError::LeaseHeld {
                current_epoch: WriterEpoch(3)
            })
            .code,
            ApiErrorCode::LeaseHeld
        );
        assert_eq!(
            witness_error(WitnessError::StaleEpoch {
                current_epoch: WriterEpoch(4)
            })
            .code,
            ApiErrorCode::StaleEpoch
        );
        assert_eq!(
            witness_error(WitnessError::FencePending {
                retry_after_secs: 7
            })
            .code,
            ApiErrorCode::FencePending
        );
        assert_eq!(
            witness_error(WitnessError::UnknownVolume).code,
            ApiErrorCode::InvalidState
        );
        assert_eq!(
            witness_error(WitnessError::AlreadyRegistered).code,
            ApiErrorCode::InvalidState
        );
        assert_eq!(
            witness_error(WitnessError::InvalidRequest("x".to_owned())).code,
            ApiErrorCode::InvalidRequest
        );
        assert_eq!(
            witness_error(WitnessError::IdempotencyConflict).code,
            ApiErrorCode::IdempotencyConflict
        );
        assert_eq!(
            witness_error(WitnessError::Unauthorized).code,
            ApiErrorCode::Internal
        );
        assert_eq!(
            witness_error(WitnessError::Unreachable("timeout".to_owned())).code,
            ApiErrorCode::UnknownFencingAuthority
        );
        assert_eq!(
            witness_error(WitnessError::Internal("boom".to_owned())).code,
            ApiErrorCode::Internal
        );
    }

    #[test]
    fn validity_of_accepts_only_a_live_lease_for_this_host_with_margin() {
        let ctx = context(20);
        let holder = HostId::new("node-a").expect("valid host id");
        let recorded = block(3, 1_100);
        // Live, ours, at our epoch, with margin: valid.
        assert_eq!(
            ctx.validity_of(
                &view(3, Some(&holder), LeaseState::Live, Some(100)),
                &recorded
            ),
            LeaseValidity::Valid {
                remaining_secs: 100
            }
        );
        // Retired epoch: the reasons say so.
        assert!(matches!(
            ctx.validity_of(
                &view(4, Some(&holder), LeaseState::Live, Some(100)),
                &recorded
            ),
            LeaseValidity::Invalid { .. }
        ));
        if let LeaseValidity::Invalid { reasons } = ctx.validity_of(
            &view(4, Some(&holder), LeaseState::Live, Some(100)),
            &recorded,
        ) {
            assert!(reasons.iter().any(|reason| reason.contains("retired")));
        }
        // Another holder.
        let other = HostId::new("node-b").expect("valid host id");
        assert!(matches!(
            ctx.validity_of(
                &view(3, Some(&other), LeaseState::Live, Some(100)),
                &recorded
            ),
            LeaseValidity::Invalid { .. }
        ));
        if let LeaseValidity::Invalid { reasons } = ctx.validity_of(
            &view(3, Some(&other), LeaseState::Live, Some(100)),
            &recorded,
        ) {
            assert!(
                reasons
                    .iter()
                    .any(|reason| reason.contains("held by node-b"))
            );
        }
        // Expired / revoked / no lease.
        for state in [LeaseState::Expired, LeaseState::Revoked, LeaseState::None] {
            assert!(matches!(
                ctx.validity_of(&view(3, Some(&holder), state, None), &recorded),
                LeaseValidity::Invalid { .. }
            ));
        }
        // Live but under the renewal margin: the writer would serve
        // without a conformant deadline.
        assert!(matches!(
            ctx.validity_of(
                &view(3, Some(&holder), LeaseState::Live, Some(19)),
                &recorded
            ),
            LeaseValidity::Invalid { .. }
        ));
        if let LeaseValidity::Invalid { reasons } = ctx.validity_of(
            &view(3, Some(&holder), LeaseState::Live, Some(19)),
            &recorded,
        ) {
            assert!(
                reasons
                    .iter()
                    .any(|reason| reason.contains("renewal margin"))
            );
        }
        // A live lease without a remaining duration is a protocol
        // violation, not a pass.
        assert!(matches!(
            ctx.validity_of(&view(3, Some(&holder), LeaseState::Live, None), &recorded),
            LeaseValidity::Invalid { .. }
        ));
    }

    /// A scripted connection for the deadline-anchoring tests: fixed
    /// grant/renew responses, everything else refused.
    struct Scripted {
        grant_ttl: u64,
        renew_remaining: u64,
    }
    impl BlockingWitnessConnection for Scripted {
        fn register(
            &self,
            _volume_id: &VolumeId,
            _request: RegisterRequest,
        ) -> Result<volvisor_witness::proto::RegisterResponse, WitnessError> {
            Err(WitnessError::Internal("not used".to_owned()))
        }
        fn grant(
            &self,
            _volume_id: &VolumeId,
            _request: GrantRequest,
        ) -> Result<GrantResponse, WitnessError> {
            Ok(GrantResponse {
                epoch: WriterEpoch(2),
                lease_id: LeaseId(9),
                lease_ttl_secs: self.grant_ttl,
                fencing_proof: FencingProof {
                    volume_id: VolumeId::new("vol-1").expect("valid volume id"),
                    retired_epoch: WriterEpoch(1),
                    commit_index: 42,
                },
            })
        }
        fn renew(
            &self,
            _volume_id: &VolumeId,
            _request: RenewRequest,
        ) -> Result<volvisor_witness::proto::RenewResponse, WitnessError> {
            Ok(volvisor_witness::proto::RenewResponse {
                epoch: WriterEpoch(2),
                remaining_secs: self.renew_remaining,
            })
        }
        fn revoke(
            &self,
            _volume_id: &VolumeId,
            _request: RevokeRequest,
        ) -> Result<volvisor_witness::proto::RevokeResponse, WitnessError> {
            Err(WitnessError::Internal("not used".to_owned()))
        }
        fn inspect(&self, _volume_id: &VolumeId) -> Result<AuthorityView, WitnessError> {
            Err(WitnessError::Internal("not used".to_owned()))
        }
    }

    fn scripted_context(
        scripted: Scripted,
        clock: Arc<AtomicU64>,
        renewal_interval: u64,
    ) -> AuthorityContext {
        let host_id = HostId::new("node-a").expect("valid host id");
        AuthorityContext::new(
            Arc::new(scripted),
            host_id,
            renewal_interval,
            Arc::new(move || clock.load(Ordering::SeqCst)),
        )
        .expect("authority context")
    }

    #[test]
    fn acquire_anchors_the_deadline_at_the_response_time() {
        let clock = Arc::new(AtomicU64::new(1_000));
        let ctx = scripted_context(
            Scripted {
                grant_ttl: 100,
                renew_remaining: 60,
            },
            Arc::clone(&clock),
            20,
        );
        let volume = VolumeId::new("vol-1").expect("valid volume id");
        let fresh = ctx.acquire(&volume, None).expect("grant");
        assert_eq!(fresh.epoch, WriterEpoch(2));
        assert_eq!(fresh.lease_id, LeaseId(9));
        // W5: the deadline is a DURATION from the response, and the
        // durable proof refs are the witness commit index.
        assert_eq!(fresh.acquired_at, 1_000);
        assert_eq!(fresh.deadline_at, 1_100);
        assert_eq!(fresh.lease_proof_ref, 42);
        assert_eq!(fresh.authority_commit_index, 42);
        // A recorded block takes the renewal path first (fresh
        // deadline, same lease identity).
        clock.store(1_050, Ordering::SeqCst);
        let renewed = ctx.acquire(&volume, Some(&fresh)).expect("renew");
        assert_eq!(renewed.epoch, WriterEpoch(2));
        assert_eq!(renewed.lease_id, LeaseId(9));
        // Renewal response carried 60s remaining, anchored at 1_050.
        assert_eq!(renewed.deadline_at, 1_110);
    }

    #[test]
    fn acquire_refuses_a_vacuous_cadence_from_the_response() {
        let clock = Arc::new(AtomicU64::new(1_000));
        // interval 50, ttl 100: 2*50 >= 100 — the grant response
        // itself is refused (plan §6 lazy check).
        let ctx = scripted_context(
            Scripted {
                grant_ttl: 100,
                renew_remaining: 60,
            },
            Arc::clone(&clock),
            50,
        );
        let volume = VolumeId::new("vol-1").expect("valid volume id");
        let error = ctx.acquire(&volume, None).expect_err("guard refuses");
        assert_eq!(error.code, ApiErrorCode::InvalidRequest);
        // The shrink-after-grant case: a renew response whose remaining
        // duration no longer admits the interval is refused the same
        // way (the caller follows the local-deadline self-fence path).
        let ctx = scripted_context(
            Scripted {
                grant_ttl: 100,
                renew_remaining: 30,
            },
            clock,
            20,
        );
        let error = ctx
            .renew_lease(&volume, &block(2, 1_100))
            .expect_err("guard refuses the shrink");
        assert_eq!(error.code, ApiErrorCode::InvalidRequest);
    }
}
