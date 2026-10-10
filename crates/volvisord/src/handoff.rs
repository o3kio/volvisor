//! # The daemon's handoff wiring (P4b plan §6, stage B2)
//!
//! Stage B1 built the engine-neutral
//! [`MigrationCoordinator`] over
//! the [`HandoffDriver`] seam; this module implements that seam over the
//! real surfaces and composes the daemon's two migration roles:
//!
//! - **the source role** — [`DaemonHandoffDriver`]: the coordinator's
//!   external world. Witness mutations run over this host's W8
//!   credential; VMM acts run over the local [`VmmController`]; source
//!   storage acts run over the provider's
//!   [`HandoffSurface`];
//!   every destination act (prepare, grant, restore, discard) is a
//!   call to the **peer daemon's** internal API through
//!   [`PeerClient`].
//! - **the consumer surface** — [`MigrationHandle`]: the
//!   [`MigrationSurface`] behind
//!   the five `/v2/migrations` routes, wrapping the coordinator with
//!   the prepare enrichment (the provider-local participant facts the
//!   engine-neutral coordinator cannot derive) and the one-drive-at-a
//!   time lock plan §3 requires.
//!
//! ## The driver mapping (normative, plan §3/§6)
//!
//! | `HandoffDriver` method | Real surface |
//! |---|---|
//! | `witness_view` | witness `inspect` (this host's credential, read) |
//! | `vm_present` / `vm_paused` | local `VmmController::state` |
//! | `source_secondary` | `HandoffSurface::role_secondary` |
//! | `target_granted` | witness `inspect`: lease live **and** held by the target host |
//! | `witness_reachable` | a bounded TCP connect to the witness endpoint (below) |
//! | `prepare_target` / `discard_target` | peer `prepare` / `discard` |
//! | `replica_caught_up` | `HandoffSurface::replica_caught_up`, the same bounded retry — the pre-quiesce `PRECOPY` convergence observation (no proof minted; the D2 proof stays `track_sync`'s) |
//! | `pause_vm` / `snapshot_vm` / `destroy_vm` | local `VmmController` (verified adapter) |
//! | `quiesce_source` | `HandoffSurface::quiesce_for_barrier` |
//! | `track_sync` | `HandoffSurface::track_sync`, bounded retry over the retryable `REPLICA_NOT_DURABLE` refusal |
//! | `record_barrier` | witness `RecordBarrier` (the coordinator's deterministic op id, the inspected current epoch, the all-true attestation of the already-proven pause/suspension/sync chain) |
//! | `void_barriers` | per **participant**: witness log → void every unvoided barrier of this migration under the deterministic `void-barrier` op id → **re-inspect confirm** (a recorded proof the witness cannot present refuses typed; a proof-less participant with no barrier is the crash window's nothing-to-void) |
//! | `unsuspend_source` | `HandoffSurface::abort_prepare` |
//! | `resume_vm` | dual meaning by the observed local VM state (below) |
//! | `demote_source` | `HandoffSurface::release_source` |
//! | `revoke_set` | witness `RevokeSet` (the coordinator's op id, per-participant epochs from `inspect`) |
//! | `grant_set` / `promote_target` / `restore_vm` | **the peer daemon** — never this host's witness credential |
//! | `clear_cut_marker` | `HandoffSurface::clear_cut_marker` (no fencing proof: the completion tail is Secondary-gated) |
//! | `fence_source` | `HandoffSurface::fail_closed_fence` |
//!
//! ### W8 on the grant path
//!
//! This host **never** asks the witness to grant the target: a
//! `GrantSet` for the destination is issued by the destination itself
//! (the peer `grant` route, under the peer's own W8 credential, over
//! the same deterministic batch operation id the coordinator derived).
//! The driver's `grant_set`, `promote_target` and `restore_vm` are all
//! peer calls; the only witness mutations the source issues are
//! `RecordBarrier`, `VoidBarrier` and `RevokeSet` — each
//! holder-asserting **this** host.
//!
//! ### `witness_reachable`
//!
//! The trait contract is synchronous by design: it gates the
//! terminal-`InDoubt` abort re-attempt, which "must not await (and
//! half-execute) a flaky connection". The implementation is a bounded
//! (2 s) TCP connect to the witness endpoint: it answers transport
//! reachability only, never half-executes protocol state, and never
//! blocks an async worker on a protocol round-trip. Its failure
//! direction is fail-closed — an unreachable endpoint answers `false`
//! and the record stays `IN_DOUBT` for the next pass. A witness that
//! accepts connections but cannot answer is caught one step later, by
//! the void attempt itself, which fails closed into the fence path
//! exactly as G5 requires.
//!
//! ### `resume_vm`'s dual meaning
//!
//! The same `vm_id` names the VM on both hosts, and the coordinator
//! calls `resume_vm` on both paths: the rollback resumes the **source**
//! VM (paused at quiesce), the forward path completes the
//! **destination** VM's resume. The driver disambiguates by the
//! observed local state: `Paused` → a local verified resume (the
//! rollback); `Running` → already resumed (idempotent); `Absent` → the
//! forward path — the destination VM's restore **and** resume are one
//! peer act (`restore-vm {resume: true}`, verified by the response's
//! observed state), re-driven idempotently by the `restore_vm` call
//! that always precedes this one in the same drive; `Created` → a
//! typed refusal (a defined-but-not-running local VM is never resumed
//! blindly).
//!
//! ### Already-resolved refusals
//!
//! Two provider refusals are the **idempotent re-drive answer**, not
//! failures, and the driver maps them to `Ok`: `abort_prepare`'s
//! "no cut to abort" (a rollback of a participant that was never
//! marked — the abort at `PREPARED`, or a partial-quiesce crash
//! window) and `clear_cut_marker`'s "nothing to clear" (the completion
//! and rollback tails re-clear markers that `release_source` /
//! `abort_prepare` already cleared as their own last step). The
//! mappings are narrow — the exact refusal details — so a genuine
//! refusal never silently becomes a success.
//!
//! ## Honesty
//!
//! Everything here is wiring over verified seams: the `ch-remote`
//! adapter's command surface is verified against the upstream docs but
//! not exercised against a real cloud-hypervisor in CI, and the
//! two-host R4 evidence campaign remains the production gate (plan
//! §8/§9). No production-support claim is made by this module.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Method;
use hyper::Request;
use hyper::StatusCode;
use hyper::Uri;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::Serialize;
use serde::de::DeserializeOwned;
use volvisor_api::peer::{
    PeerDiscardRequest, PeerDiscardResponse, PeerGrantOutcome, PeerGrantRequest, PeerGrantResponse,
    PeerPrepareRequest, PeerPrepareResponse, PeerRestoreVmRequest, PeerRestoreVmResponse,
    PeerRouteContext, TargetPreparationStore,
};
use volvisor_handoff::{
    BarrierProof, Clock, HandoffDriver, MigrationCoordinator, MigrationRecord, MigrationStore,
    MigrationSummary, MigrationSurface, MobilityRequest, Participant, PrepareHandoffRequest,
    batch_operation_id, void_barrier_operation_id,
};
use volvisor_provider::handoff::HandoffSurface;
use volvisor_provider::vmm::{DiskMapping, VmState, VmmController};
use volvisor_types::error::ApiErrorBody;
use volvisor_types::{
    ApiError, ApiErrorCode, AuthorityView, BarrierAttestation, HostId, LeaseState, MigrationId,
    VolumeId,
};
use volvisor_witness::client::WitnessConnection;
use volvisor_witness::proto::{
    BatchRelease, RecordBarrierRequest, RevokeSetRequest, VoidBarrierRequest,
    WITNESS_PROTOCOL_VERSION, WitnessError,
};
use volvisor_witness::{BlockingWitness, BlockingWitnessConnection};

use crate::DaemonError;

/// Per-request timeout for the peer daemon's internal API. The peer's
/// `grant` route runs a witness batch **and** the promote-under-lease
/// tail before answering, so the bound is deliberately looser than the
/// witness's own. Public for the runtime's peer-client construction.
pub const PEER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The bound of the synchronous witness reachability probe (the
/// `witness_reachable` contract; see the module docs).
const WITNESS_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The bounded wait `track_sync` owns before surfacing the retryable
/// refusal as the drive's typed error (plan §9 row 9: the driver
/// waits, never assumes).
const TRACK_SYNC_BOUND: Duration = Duration::from_secs(60);

/// The delay between two `track_sync` observations inside the bound.
const TRACK_SYNC_RETRY_DELAY: Duration = Duration::from_millis(500);

/// The retry task's tick (plan §3's periodic reconcile). A pass skips
/// itself when a consumer drive holds the surface; the next tick
/// re-attempts.
const MIGRATION_RETRY_TICK: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// The peer client
// ---------------------------------------------------------------------------

/// The destination daemon's internal API, as the source daemon's
/// driver calls it (plan §6: daemon-to-daemon, the peer credential —
/// a peer call is never a consumer call and never a witness call).
///
/// Implemented by [`HttpPeerClient`] over real HTTP/JSON and by test
/// doubles that mirror the same semantics (the destination's own acts
/// against a real loopback witness).
#[async_trait]
pub trait PeerClient: Send + Sync {
    /// The destination's target preparation (verify the participant
    /// set against its provider, probe the shared snapshot dir,
    /// persist the preparation).
    ///
    /// # Errors
    /// The peer's typed refusal (unknown volumes, stale generations,
    /// an unusable snapshot dir, a conflicting preparation), or
    /// `INTERNAL` when the peer daemon cannot be reached.
    async fn prepare(&self, request: PeerPrepareRequest) -> Result<PeerPrepareResponse, ApiError>;

    /// The destination's `GrantSet` + promote-under-granted-lease act
    /// (W8: the witness mutation is the peer's own, never the
    /// source's).
    ///
    /// # Errors
    /// The peer's typed refusal, or `INTERNAL` when the peer daemon
    /// cannot be reached.
    async fn grant(&self, request: PeerGrantRequest) -> Result<PeerGrantResponse, ApiError>;

    /// The destination's VM restore (and, per the request's `resume`
    /// flag, resume) from the migration's snapshot.
    ///
    /// # Errors
    /// The peer's typed refusal, or `INTERNAL` when the peer daemon
    /// cannot be reached.
    async fn restore_vm(
        &self,
        request: PeerRestoreVmRequest,
    ) -> Result<PeerRestoreVmResponse, ApiError>;

    /// The destination's pre-cut abort tail (drop the target
    /// preparation; idempotent).
    ///
    /// # Errors
    /// The peer's typed refusal, or `INTERNAL` when the peer daemon
    /// cannot be reached.
    async fn discard(&self, request: PeerDiscardRequest) -> Result<PeerDiscardResponse, ApiError>;
}

/// HTTP implementation of [`PeerClient`] (the
/// [`HttpWitnessConnection`](volvisor_witness::client::HttpWitnessConnection)
/// pattern over the API crate's peer routes): hyper legacy client,
/// JSON bodies, the daemon-to-daemon bearer token, per-request
/// timeout.
///
/// Error mapping: a typed peer refusal decodes through the contract
/// error shape back into a typed [`ApiError`]
/// ([`ApiError::from_wire`]); every transport failure — connect,
/// timeout, torn body — is `INTERNAL` with a "peer daemon
/// unreachable" detail (the drive treats it like any external-act
/// failure: the record keeps its durable state and the retry task
/// re-drives).
pub struct HttpPeerClient {
    base_url: String,
    token: String,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>>,
    timeout: Duration,
}

impl HttpPeerClient {
    /// Build a client for the peer daemon at `base_url` (e.g.
    /// `http://10.0.0.2:7780`) with the daemon-to-daemon credential.
    #[must_use]
    pub fn new(base_url: impl Into<String>, token: impl Into<String>, timeout: Duration) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            client: Client::builder(TokioExecutor::new()).build_http(),
            timeout,
        }
    }

    /// Issue one request and decode the typed result.
    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &(impl Serialize + Sync),
    ) -> Result<T, ApiError> {
        let unreachable = |detail: String| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("peer daemon unreachable: {detail}"),
            )
        };
        let uri = format!("{}{path}", self.base_url)
            .parse::<Uri>()
            .map_err(|err| unreachable(format!("invalid peer base URL: {err}")))?;
        let payload = serde_json::to_vec(body).map_err(|err| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("peer request serialization failure: {err}"),
            )
        })?;
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(payload)))
            .map_err(|err| unreachable(format!("request build failure: {err}")))?;
        let response = tokio::time::timeout(self.timeout, self.client.request(request))
            .await
            .map_err(|_| {
                unreachable(format!(
                    "request timed out after {} ms",
                    self.timeout.as_millis()
                ))
            })?
            .map_err(|err| unreachable(format!("transport failure: {err}")))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|err| unreachable(format!("body failure: {err}")))?
            .to_bytes();
        if status.is_success() {
            return serde_json::from_slice(&bytes).map_err(|err| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!("unparseable peer response: {err}"),
                )
            });
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(ApiError::new(
                ApiErrorCode::Forbidden,
                "the peer daemon rejected the daemon-to-daemon credential",
            ));
        }
        let body = serde_json::from_slice::<ApiErrorBody>(&bytes).map_err(|err| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("unparseable peer error body (status {status}): {err}"),
            )
        })?;
        Err(ApiError::from_wire(&body))
    }
}

#[async_trait]
impl PeerClient for HttpPeerClient {
    async fn prepare(&self, request: PeerPrepareRequest) -> Result<PeerPrepareResponse, ApiError> {
        self.request(Method::POST, "/v2/internal/peer/prepare", &request)
            .await
    }

    async fn grant(&self, request: PeerGrantRequest) -> Result<PeerGrantResponse, ApiError> {
        self.request(Method::POST, "/v2/internal/peer/grant", &request)
            .await
    }

    async fn restore_vm(
        &self,
        request: PeerRestoreVmRequest,
    ) -> Result<PeerRestoreVmResponse, ApiError> {
        self.request(Method::POST, "/v2/internal/peer/restore-vm", &request)
            .await
    }

    async fn discard(&self, request: PeerDiscardRequest) -> Result<PeerDiscardResponse, ApiError> {
        self.request(Method::POST, "/v2/internal/peer/discard", &request)
            .await
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Map a witness refusal onto the API error taxonomy (the same
/// mapping the peer routes use).
fn witness_api_error(error: &WitnessError) -> ApiError {
    error.to_api_error()
}

/// Whether `vm_id` is safe to join into a filesystem path (the same
/// guard the peer's `prepare` route applies; the driver re-checks
/// because the record's `vm_id` is consumer free text and the join
/// happens on this host too — defense in depth, never a traversal).
fn safe_path_segment(vm_id: &str) -> bool {
    !vm_id.is_empty()
        && vm_id != "."
        && vm_id != ".."
        && !vm_id.contains('/')
        && !vm_id.contains('\0')
}

/// Run one synchronous VMM operation on the blocking thread pool (the
/// `VmmController` seam is synchronous; calling it inline would pin an
/// async worker for the command's full duration).
async fn vmm_call<T, F>(vmm: &Arc<dyn VmmController>, operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&dyn VmmController) -> Result<T, ApiError> + Send + 'static,
{
    let vmm = Arc::clone(vmm);
    tokio::task::spawn_blocking(move || operation(vmm.as_ref()))
        .await
        .map_err(|join_error| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("VMM operation failed to run: {join_error}"),
            )
        })?
}

/// Resolve a witness base URL to the socket address the
/// reachability probe connects to (`http://host:port`, port
/// defaulting to 80; names resolve through the system resolver).
///
/// # Errors
/// [`DaemonError::Config`] for a non-HTTP scheme, a missing host or a
/// name that cannot be resolved — the same failures the config
/// validation treats as colocation-check inputs, refused here because
/// a witness the daemon cannot address is unusable for the probe.
fn witness_probe_addr(url: &str) -> Result<SocketAddr, DaemonError> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        DaemonError::Config(format!(
            "witness_url {url} must use the http:// scheme (the witness surface is plain HTTP)"
        ))
    })?;
    let authority = rest.split('/').next().unwrap_or(rest);
    // A whole literal (IPv4, or a bracketed IPv6 literal with its
    // port) needs no resolution.
    if let Ok(addr) = authority.parse::<SocketAddr>() {
        return Ok(addr);
    }
    let (host, port) = split_authority(url, authority)?;
    // A name (or a portless literal): resolve through the system
    // resolver against the resolved port.
    (host, port)
        .to_socket_addrs()
        .map_err(|err| {
            DaemonError::Config(format!(
                "witness_url {url} cannot be resolved for the probe: {err}"
            ))
        })?
        .next()
        .ok_or_else(|| {
            DaemonError::Config(format!(
                "witness_url {url} resolved to no address on port {port}"
            ))
        })
}

/// Split an authority into its host and port (the port defaulting to
/// 80), honoring the bracketed IPv6 literal form.
///
/// # Errors
/// [`DaemonError::Config`] for a malformed port or an unterminated
/// bracketed literal.
fn split_authority<'a>(url: &str, authority: &'a str) -> Result<(&'a str, u16), DaemonError> {
    let bad_port = || {
        DaemonError::Config(format!(
            "witness_url {url} carries a malformed port in {authority}"
        ))
    };
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(|| {
            DaemonError::Config(format!(
                "witness_url {url} carries an unterminated bracketed host in {authority}"
            ))
        })?;
        let port = tail.strip_prefix(':').unwrap_or("80");
        return port
            .parse::<u16>()
            .map_err(|_| bad_port())
            .map(|port| (host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => port
            .parse::<u16>()
            .map_err(|_| bad_port())
            .map(|port| (host, port)),
        None => Ok((authority, 80)),
    }
}

// ---------------------------------------------------------------------------
// The source-role driver
// ---------------------------------------------------------------------------

/// The coordinator's external world on the source host (the module
/// docs' mapping table): the witness (this host's W8 credential), the
/// local VMM, the provider's handoff surface and the peer daemon.
pub struct DaemonHandoffDriver {
    /// This host's identity (the holder every witness mutation
    /// asserts; the source of the migration).
    host_id: HostId,
    /// The witness connection, authenticated as this host (W8).
    witness: Arc<dyn WitnessConnection>,
    /// The reachability probe's target (see the module docs).
    witness_probe: SocketAddr,
    /// The local VMM controller (the source VM).
    vmm: Arc<dyn VmmController>,
    /// The provider's source-side handoff surface.
    handoff: Arc<dyn HandoffSurface>,
    /// The destination daemon's internal API.
    peer: Arc<dyn PeerClient>,
    /// The configured snapshot root (the migration's directory is
    /// `{snapshot_root}/{vm_id}`).
    snapshot_root: PathBuf,
    /// The `track_sync` bounded wait (tests tighten it).
    track_sync_bound: Duration,
    /// The delay between two `track_sync` observations (tests tighten
    /// it).
    track_sync_retry_delay: Duration,
}

impl DaemonHandoffDriver {
    /// Build the driver over the real surfaces.
    #[must_use]
    pub fn new(
        host_id: HostId,
        witness: Arc<dyn WitnessConnection>,
        witness_probe: SocketAddr,
        vmm: Arc<dyn VmmController>,
        handoff: Arc<dyn HandoffSurface>,
        peer: Arc<dyn PeerClient>,
        snapshot_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            host_id,
            witness,
            witness_probe,
            vmm,
            handoff,
            peer,
            snapshot_root: snapshot_root.into(),
            track_sync_bound: TRACK_SYNC_BOUND,
            track_sync_retry_delay: TRACK_SYNC_RETRY_DELAY,
        }
    }

    /// Tighten the `track_sync` bounded wait (tests only: a 60 s bound
    /// would make convergence-wait tests slow, not more honest).
    #[must_use]
    pub fn with_track_sync_bounds(mut self, bound: Duration, retry_delay: Duration) -> Self {
        self.track_sync_bound = bound;
        self.track_sync_retry_delay = retry_delay;
        self
    }

    /// The migration's snapshot directory:
    /// `{snapshot_root}/{vm_id}` (the same key the destination's
    /// `prepare` probed — one VM, one directory, shared storage).
    ///
    /// # Errors
    /// `INVALID_REQUEST` when `vm_id` is not a safe path segment
    /// (consumer free text; the join happens only behind this guard).
    fn snapshot_dir(&self, vm_id: &str) -> Result<PathBuf, ApiError> {
        if !safe_path_segment(vm_id) {
            return Err(ApiError::invalid_request(
                "vm_id must be a non-empty path segment (no '/', not '.' or '..')",
            ));
        }
        Ok(self.snapshot_root.join(vm_id))
    }

    /// This host's witness view of one volume.
    async fn view(&self, volume_id: &VolumeId) -> Result<AuthorityView, ApiError> {
        self.witness
            .inspect(volume_id)
            .await
            .map_err(|error| witness_api_error(&error))
    }

    /// The observed local state of one VM.
    async fn local_state(&self, vm_id: &str) -> Result<VmState, ApiError> {
        let vm_id = vm_id.to_owned();
        vmm_call(&self.vmm, move |vmm| vmm.state(&vm_id)).await
    }

    /// The peer grant outcomes of one migration, re-fetched (the
    /// promote/restore acts re-call `grant` first: the route is
    /// idempotent and its outcomes carry the promoted device paths the
    /// restore's disk mappings need).
    async fn peer_grants(
        &self,
        migration_id: &MigrationId,
    ) -> Result<Vec<PeerGrantOutcome>, ApiError> {
        let grants = self
            .peer
            .grant(PeerGrantRequest {
                migration_id: migration_id.clone(),
            })
            .await?;
        Ok(grants.grants)
    }
}

#[async_trait]
impl HandoffDriver for DaemonHandoffDriver {
    async fn witness_view(&self, volume_id: &VolumeId) -> Result<AuthorityView, ApiError> {
        self.view(volume_id).await
    }

    async fn vm_present(&self, vm_id: &str) -> Result<bool, ApiError> {
        Ok(self.local_state(vm_id).await? != VmState::Absent)
    }

    async fn vm_paused(&self, vm_id: &str) -> Result<bool, ApiError> {
        Ok(self.local_state(vm_id).await? == VmState::Paused)
    }

    async fn source_secondary(&self, volume_id: &VolumeId) -> Result<bool, ApiError> {
        self.handoff.role_secondary(volume_id).await
    }

    async fn target_granted(
        &self,
        volume_id: &VolumeId,
        target: &HostId,
    ) -> Result<bool, ApiError> {
        let view = self.view(volume_id).await?;
        Ok(view.lease_state == LeaseState::Live && view.holder.as_ref() == Some(target))
    }

    fn witness_reachable(&self) -> bool {
        // The module docs' honesty boundary: transport reachability,
        // bounded, never a protocol round-trip from this synchronous
        // contract.
        TcpStream::connect_timeout(&self.witness_probe, WITNESS_PROBE_TIMEOUT).is_ok()
    }

    async fn prepare_target(&self, record: &MigrationRecord) -> Result<(), ApiError> {
        // The source's live data-generation lineage per participant
        // (P5 plan §5.2): the honest expected set the destination's
        // replica gate compares against — read from this host's own
        // device, never from the request input.
        let mut expected_lineages = Vec::with_capacity(record.participants.len());
        for participant in &record.participants {
            let lineage = self.handoff.source_lineage(&participant.volume_id).await?;
            expected_lineages.push(lineage);
        }
        let request = PeerPrepareRequest {
            migration_id: record.migration_id.clone(),
            vm_id: record.vm_id.clone(),
            source_host: self.host_id.clone(),
            volume_ids: record
                .participants
                .iter()
                .map(|p| p.volume_id.clone())
                .collect(),
            expected_generations: record
                .participants
                .iter()
                .map(|p| p.expected_generation)
                .collect(),
            expected_lineages,
        };
        let response = self.peer.prepare(request).await?;
        // The destination must have verified exactly the participant
        // set this record carries — same order, same generations —
        // never a subset or a reordering (the grant act trusts the
        // preparation's participant set).
        let verified: Vec<(&VolumeId, u64)> = response
            .participants
            .iter()
            .map(|p| (&p.volume_id, p.expected_generation))
            .collect();
        let expected: Vec<(&VolumeId, u64)> = record
            .participants
            .iter()
            .map(|p| (&p.volume_id, p.expected_generation))
            .collect();
        if verified != expected {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "the peer verified a different participant set for migration {} \
                     (response {:?}, record {:?})",
                    record.migration_id, verified, expected
                ),
            ));
        }
        Ok(())
    }

    async fn discard_target(&self, record: &MigrationRecord) -> Result<(), ApiError> {
        self.peer
            .discard(PeerDiscardRequest {
                migration_id: record.migration_id.clone(),
            })
            .await?;
        Ok(())
    }

    async fn replica_caught_up(&self, volume_id: &VolumeId) -> Result<(), ApiError> {
        // The pre-quiesce convergence observation (the contract's
        // `PRECOPY` step: the source is still the writer, no boundary
        // exists yet) shares the driver-owned bounded wait: the
        // retryable REPLICA_NOT_DURABLE refusal is retried inside the
        // bound and only the bound's expiry surfaces as the drive's
        // typed error — surfaced at `PREPARED`, before any suspension
        // or barrier, so the abort path is fully intact.
        let deadline = std::time::Instant::now() + self.track_sync_bound;
        loop {
            match self.handoff.replica_caught_up(volume_id).await {
                Ok(()) => return Ok(()),
                Err(error) if error.code == ApiErrorCode::ReplicaNotDurable => {
                    if std::time::Instant::now() >= deadline {
                        return Err(ApiError::new(
                            ApiErrorCode::ReplicaNotDurable,
                            format!(
                                "the peer of {volume_id} did not converge before the quiesce \
                                 within {} s (last refusal: {error})",
                                self.track_sync_bound.as_secs()
                            ),
                        ));
                    }
                    tokio::time::sleep(self.track_sync_retry_delay).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn pause_vm(&self, vm_id: &str) -> Result<(), ApiError> {
        let vm_id = vm_id.to_owned();
        // The adapter proves the pause itself (a `vm.info` observation,
        // never the command's exit status).
        vmm_call(&self.vmm, move |vmm| vmm.pause(&vm_id).map(|_| ())).await
    }

    async fn quiesce_source(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<(), ApiError> {
        self.handoff
            .quiesce_for_barrier(volume_id, migration_id)
            .await
            .map(|_| ())
    }

    async fn track_sync(&self, volume_id: &VolumeId) -> Result<(), ApiError> {
        // The driver owns the bounded wait (plan §9 row 9): the
        // provider's observation is a single snapshot, so the
        // retryable REPLICA_NOT_DURABLE refusal is retried inside the
        // bound and only the bound's expiry surfaces as the drive's
        // typed error.
        let deadline = std::time::Instant::now() + self.track_sync_bound;
        loop {
            match self.handoff.track_sync(volume_id).await {
                Ok(_) => return Ok(()),
                Err(error) if error.code == ApiErrorCode::ReplicaNotDurable => {
                    if std::time::Instant::now() >= deadline {
                        return Err(ApiError::new(
                            ApiErrorCode::ReplicaNotDurable,
                            format!(
                                "the peer of {volume_id} did not converge through the barrier \
                                 within {} s (last refusal: {error})",
                                self.track_sync_bound.as_secs()
                            ),
                        ));
                    }
                    tokio::time::sleep(self.track_sync_retry_delay).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn record_barrier(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
        op_id: &volvisor_types::OperationId,
    ) -> Result<BarrierProof, ApiError> {
        // The current epoch is inspected, never remembered: the
        // witness is the authority and refuses the recording unless
        // this host holds it live (W8/W9).
        let view = self.view(volume_id).await?;
        // The attestation is volvisor's own ordered observation chain —
        // the verified pause proof, the observed suspension, the
        // post-suspension sync proof — each returned by the steps the
        // coordinator ran before this one; the driver records them as
        // attested only on that chain, never on the consumer's
        // corroboration.
        let request = RecordBarrierRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op_id.clone(),
            host_id: self.host_id.clone(),
            epoch: view.current_epoch,
            migration_id: Some(migration_id.clone()),
            attestation: BarrierAttestation {
                vm_paused_and_drained: true,
                data_path_suspended: true,
                peer_up_to_date: true,
            },
        };
        let response = self
            .witness
            .record_barrier(volume_id, request)
            .await
            .map_err(|error| witness_api_error(&error))?;
        let barrier = response.barrier;
        Ok(BarrierProof {
            volume_id: volume_id.clone(),
            boundary_commit_index: barrier.boundary_commit_index,
            attestation: barrier.attestation,
            recorded_at: barrier.recorded_at,
        })
    }

    async fn void_barriers(&self, record: &MigrationRecord) -> Result<(), ApiError> {
        // Confirm by state, never by fresh-mutation success (the B2
        // seam contract) — and enumerate the PARTICIPANTS, not the
        // recorded proofs. A crash inside the barrier drive can leave
        // the durable record with an empty or partial proof set while
        // the witness already holds live barriers of this migration;
        // iterating the proofs alone would return vacuous success and
        // the rollback would resume the source over unvoided barriers
        // — exactly the false-SAFE_CURRENT shape G5 exists to exclude.
        for participant in &record.participants {
            let view = self.view(&participant.volume_id).await?;
            let mut unvoided = Vec::new();
            let mut present = false;
            for barrier in &view.barriers {
                if barrier.migration_id.as_ref() != Some(&record.migration_id) {
                    continue;
                }
                present = true;
                if !barrier.voided {
                    unvoided.push(barrier.clone());
                }
            }
            if !present {
                // Belt-and-suspenders for the proof-present case only:
                // a recorded proof the witness cannot show means a
                // re-recorded journal or a foreign witness — never
                // guess, refuse typed so the rollback fails closed
                // into the fence path. Without a proof (the crash
                // window) an empty set simply means nothing was
                // recorded before the crash: nothing to void.
                if let Some(proof) = record
                    .barrier_proofs
                    .iter()
                    .find(|proof| proof.volume_id == participant.volume_id)
                {
                    return Err(ApiError::new(
                        ApiErrorCode::Internal,
                        format!(
                            "the witness holds no barrier of migration {} for {} (proof \
                             references commit index {})",
                            record.migration_id, participant.volume_id, proof.boundary_commit_index
                        ),
                    ));
                }
                continue;
            }
            for barrier in &unvoided {
                let op_id =
                    void_barrier_operation_id(&record.migration_id, &participant.volume_id)?;
                let request = VoidBarrierRequest {
                    protocol_version: WITNESS_PROTOCOL_VERSION,
                    operation_id: op_id,
                    host_id: self.host_id.clone(),
                    epoch: barrier.epoch,
                    migration_id: Some(record.migration_id.clone()),
                };
                let response = self
                    .witness
                    .void_barrier(&participant.volume_id, request)
                    .await
                    .map_err(|error| witness_api_error(&error))?;
                if !response.barrier.voided {
                    return Err(ApiError::new(
                        ApiErrorCode::Internal,
                        format!(
                            "the witness voided the barrier of {} (migration {}) without \
                             reporting it voided",
                            participant.volume_id, record.migration_id
                        ),
                    ));
                }
            }
            // The seam contract's confirmation is state, not the
            // mutation's response: after voiding, every barrier of
            // this migration must read voided (G5's hard gate on any
            // source resume).
            let rechecked = self.view(&participant.volume_id).await?;
            if rechecked.barriers.iter().any(|barrier| {
                barrier.migration_id.as_ref() == Some(&record.migration_id) && !barrier.voided
            }) {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "an unvoided barrier of migration {} for {} survives the void (G5: \
                         the source cannot resume)",
                        record.migration_id, participant.volume_id
                    ),
                ));
            }
        }
        Ok(())
    }

    async fn unsuspend_source(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<(), ApiError> {
        match self.handoff.abort_prepare(volume_id, migration_id).await {
            Ok(()) => Ok(()),
            // The idempotent re-drive answer: this participant was
            // never marked (the abort at `PREPARED`, or a crash inside
            // the quiesce loop). Nothing to unsuspend is success.
            Err(error)
                if error.code == ApiErrorCode::InvalidState
                    && error.detail.contains("no cut to abort") =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn resume_vm(&self, vm_id: &str) -> Result<(), ApiError> {
        // The module docs' dual meaning, disambiguated by the observed
        // local state — never a guessed resume.
        match self.local_state(vm_id).await? {
            VmState::Paused => {
                let vm_id = vm_id.to_owned();
                vmm_call(&self.vmm, move |vmm| vmm.resume(&vm_id)).await
            }
            VmState::Running => Ok(()),
            VmState::Absent => {
                // The forward path: the destination VM's restore and
                // resume are one verified peer act, re-driven
                // idempotently by the `restore_vm` call that always
                // precedes this one in the same drive (the coordinator
                // runs restore → resume in sequence; a crash re-drive
                // re-runs `restore_vm` first). Nothing local to resume.
                Ok(())
            }
            VmState::Created => Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "VM {vm_id} is defined but not running on this host; refusing to resume it \
                     (a defined local VM is never the migration's resume target)"
                ),
            )),
        }
    }

    async fn snapshot_vm(&self, vm_id: &str) -> Result<(), ApiError> {
        let dir = self.snapshot_dir(vm_id)?;
        let vm_id = vm_id.to_owned();
        vmm_call(&self.vmm, move |vmm| vmm.snapshot(&vm_id, &dir)).await
    }

    async fn destroy_vm(&self, vm_id: &str) -> Result<(), ApiError> {
        let vm_id = vm_id.to_owned();
        // Re-drive-safe by the adapter's contract: an already-absent VM
        // is a verified no-op.
        vmm_call(&self.vmm, move |vmm| vmm.destroy(&vm_id)).await
    }

    async fn demote_source(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<(), ApiError> {
        self.handoff.release_source(volume_id, migration_id).await
    }

    async fn revoke_set(
        &self,
        record: &MigrationRecord,
        op_id: &volvisor_types::OperationId,
    ) -> Result<(), ApiError> {
        // Per-participant epochs are inspected, never remembered: the
        // witness's W4 check refuses a release against a moved epoch,
        // which is exactly the fail-closed answer a stale view
        // deserves.
        let mut releases = Vec::with_capacity(record.participants.len());
        for participant in &record.participants {
            let view = self.view(&participant.volume_id).await?;
            releases.push(BatchRelease {
                volume_id: participant.volume_id.clone(),
                epoch: view.current_epoch,
            });
        }
        let request = RevokeSetRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: op_id.clone(),
            host_id: self.host_id.clone(),
            migration_id: Some(record.migration_id.clone()),
            releases,
        };
        self.witness
            .revoke_set(request)
            .await
            .map_err(|error| witness_api_error(&error))?;
        Ok(())
    }

    async fn grant_set(
        &self,
        record: &MigrationRecord,
        _op_id: &volvisor_types::OperationId,
    ) -> Result<(), ApiError> {
        // W8 (the module docs): the GrantSet is the destination's own
        // witness mutation, issued under the peer's credential over
        // the same deterministic batch operation id this coordinator
        // derived — this host never grants for the target. The
        // coordinator's op id is deliberately unused here.
        self.peer
            .grant(PeerGrantRequest {
                migration_id: record.migration_id.clone(),
            })
            .await?;
        Ok(())
    }

    async fn promote_target(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<(), ApiError> {
        // The peer's `grant` route is the promote-under-granted-lease
        // act (idempotent per migration); re-calling it re-verifies
        // the granted lease and completes the promotion tail.
        let grants = self.peer_grants(migration_id).await?;
        if !grants.iter().any(|outcome| outcome.volume_id == *volume_id) {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "the peer's grant of migration {migration_id} carries no outcome for \
                     {volume_id}"
                ),
            ));
        }
        Ok(())
    }

    async fn restore_vm(&self, record: &MigrationRecord) -> Result<(), ApiError> {
        // The promoted device paths come from the (idempotent) peer
        // grant; the declared paths are the source devices the
        // snapshot's config carries — `/dev/drbd{minor}`, the DRBD
        // handle the source VM held open.
        let grants = self.peer_grants(&record.migration_id).await?;
        let mut disks = Vec::with_capacity(record.participants.len());
        for participant in &record.participants {
            let Some(outcome) = grants
                .iter()
                .find(|outcome| outcome.volume_id == participant.volume_id)
            else {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "the peer's grant of migration {} carries no outcome for {}",
                        record.migration_id, participant.volume_id
                    ),
                ));
            };
            disks.push(DiskMapping {
                declared_path: format!("/dev/drbd{}", participant.minor),
                device_path: outcome.device_path.clone(),
            });
        }
        let snapshot_dir = self.snapshot_dir(&record.vm_id)?;
        // Restore **and** resume in one act (the peer route's `resume`
        // flag; the module docs' dual-meaning note): the response's
        // observed state is the proof, never the command's exit.
        let response = self
            .peer
            .restore_vm(PeerRestoreVmRequest {
                migration_id: record.migration_id.clone(),
                snapshot_dir: snapshot_dir.to_string_lossy().into_owned(),
                disks,
                resume: true,
            })
            .await?;
        if response.vm_state != VmState::Running {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "the destination VM of migration {} reports state {} after the \
                     restore-and-resume act (Running is the only acceptable answer)",
                    record.migration_id, response.vm_state
                ),
            ));
        }
        Ok(())
    }

    async fn clear_cut_marker(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<(), ApiError> {
        // No fencing proof: the completion and rollback tails run after
        // the source is Secondary (the coordinator verifies), which is
        // the provider's own gate for a proofless clear.
        let _migration_id = migration_id;
        match self.handoff.clear_cut_marker(volume_id, None).await {
            Ok(_) => Ok(()),
            // The idempotent re-drive answer: `release_source` /
            // `abort_prepare` cleared the marker as their own last
            // step, so this tail's clear finds nothing left — success.
            Err(error)
                if error.code == ApiErrorCode::InvalidState
                    && error.detail.contains("nothing to clear") =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn fence_source(
        &self,
        volume_id: &VolumeId,
        migration_id: &MigrationId,
    ) -> Result<(), ApiError> {
        let reason = format!(
            "migration {migration_id}: the barrier void could not be confirmed; fail-closed \
             fence (never a resume)"
        );
        self.handoff.fail_closed_fence(volume_id, &reason).await
    }
}

// ---------------------------------------------------------------------------
// The consumer surface
// ---------------------------------------------------------------------------

/// The daemon's prepare enrichment: `(volume, vm, expected
/// generation) → (resource, minor)` — the provider-local facts each
/// volume's writer identity lives in, derived from provider state,
/// never asserted by the consumer (the `MigrationSurface` module
/// docs' documented deviation: the engine-neutral coordinator cannot
/// derive them).
pub type ParticipantFacts =
    Arc<dyn Fn(&VolumeId, &str, u64) -> Result<(String, u32), ApiError> + Send + Sync>;

/// One slot of the drive-task registry: `transfer` records the
/// spawned task's `JoinHandle` only after `tokio::spawn` returns, so
/// a fail-fast task may run its final act first — the slot records
/// which side of that spawn/finish race it is in.
enum DriveTaskSlot {
    /// The id is allocated; the spawner is between `spawn` and the
    /// record step.
    Starting,
    /// The recorded handle, live until the task's own final act
    /// removes the slot.
    Tracked(tokio::task::JoinHandle<()>),
    /// The task finished before its handle was recorded — the record
    /// step drops the handle instead of tracking a dead task.
    FinishedEarly,
    /// The kill group fired while the slot was still [`Starting`]
    /// (P5 plan §3.3: the group must be COMPLETE — a drive between
    /// spawn and record is a mutation engine the kill missed). The
    /// handle is not abortable yet, so the record step aborts it the
    /// moment it arrives.
    AbortPending,
}

/// The transfer drive tasks this surface spawned (P5 plan §3.3's
/// tracked registry): the failure-campaign supervisor's kill group
/// must enumerate the detached drive, so its `JoinHandle` is recorded
/// here. The task's own final act removes its slot, so the registry
/// never grows on graceful paths — behavior-neutral by construction
/// (the drive's future is unchanged).
#[derive(Default)]
struct DriveTaskRegistry {
    /// The next task id (monotonic, never reused within the process).
    next_id: u64,
    /// The slots by task id.
    slots: BTreeMap<u64, DriveTaskSlot>,
}

impl DriveTaskRegistry {
    /// Allocate the next task id (the slot starts [`Starting`]: the
    /// spawner records the handle right after `tokio::spawn`).
    fn alloc(&mut self) -> u64 {
        self.next_id += 1;
        self.slots.insert(self.next_id, DriveTaskSlot::Starting);
        self.next_id
    }

    /// Record the spawned task's handle (the spawner's half of the
    /// spawn/finish race): a task that already finished marked its
    /// slot, so the handle is dropped — a dead task is never tracked.
    /// A slot the kill group marked [`DriveTaskSlot::AbortPending`]
    /// aborts the handle here — the drive dies at the record step
    /// instead of escaping the kill. Returns whether the handle was
    /// aborted (the kill-window test's deterministic assertion).
    fn record(&mut self, id: u64, handle: tokio::task::JoinHandle<()>) -> bool {
        match self.slots.get(&id) {
            Some(DriveTaskSlot::FinishedEarly) => {
                self.slots.remove(&id);
                false
            }
            Some(DriveTaskSlot::AbortPending) => {
                handle.abort();
                self.slots.remove(&id);
                true
            }
            _ => {
                self.slots.insert(id, DriveTaskSlot::Tracked(handle));
                false
            }
        }
    }

    /// The task's final act (the task's half of the race): remove the
    /// slot, or mark it when the handle is not recorded yet.
    fn finish(&mut self, id: u64) {
        match self.slots.get(&id) {
            Some(DriveTaskSlot::Starting | DriveTaskSlot::AbortPending) => {
                self.slots.insert(id, DriveTaskSlot::FinishedEarly);
            }
            _ => {
                self.slots.remove(&id);
            }
        }
    }

    /// The number of live drive tasks (the kill group's enumeration
    /// input).
    fn live(&self) -> usize {
        self.slots
            .values()
            .filter(|slot| matches!(slot, DriveTaskSlot::Tracked(_)))
            .count()
    }

    /// Take every recorded handle out (the abort path's first half —
    /// an aborted task cannot run its final act, so the registry must
    /// reap it wholesale). A slot still [`Starting`] (the spawner
    /// between spawn and record) is marked [`AbortPending`] — the
    /// group is complete only if this window is covered too.
    fn take_live(&mut self) -> Vec<tokio::task::JoinHandle<()>> {
        let ids: Vec<u64> = self.slots.keys().copied().collect();
        let mut handles = Vec::with_capacity(ids.len());
        for id in ids {
            match self.slots.get(&id) {
                Some(DriveTaskSlot::Tracked(_)) => {
                    if let Some(DriveTaskSlot::Tracked(handle)) = self.slots.remove(&id) {
                        handles.push(handle);
                    }
                }
                Some(DriveTaskSlot::Starting) => {
                    self.slots.insert(id, DriveTaskSlot::AbortPending);
                }
                _ => {}
            }
        }
        handles
    }
}

/// Lock the registry without panicking on poison: every critical
/// section is a pure map operation, so a poisoned lock can only mean
/// another thread panicked in unrelated code while holding it — the
/// map is still structurally valid, and task bookkeeping must not
/// crash the daemon (the coordinator's own store maps poison to a
/// typed error; this registry has no caller to refuse).
fn lock_registry(registry: &Arc<Mutex<DriveTaskRegistry>>) -> MutexGuard<'_, DriveTaskRegistry> {
    registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The daemon's [`MigrationSurface`]: the coordinator wrapped with the
/// prepare enrichment and the one-drive-at-a-time lock (plan §3: the
/// coordinator is not internally serialized; the daemon serializes
/// the drive, the reconcile and the abort against each other).
pub struct MigrationHandle {
    coordinator: Arc<MigrationCoordinator<DaemonHandoffDriver>>,
    facts: ParticipantFacts,
    host_id: HostId,
    /// The drive lock: held by a live consumer drive, an abort or a
    /// retry pass; a shared `Arc` so the spawned drive task and the
    /// surface take the same lock.
    drive: Arc<tokio::sync::Mutex<()>>,
    /// The transfer drive tasks `transfer` spawned (plan §3.3's
    /// tracked registry): the supervisor's kill group aborts them as
    /// a group; each task's own final act removes its slot, so the
    /// registry never grows on graceful paths.
    drive_tasks: Arc<Mutex<DriveTaskRegistry>>,
}

impl MigrationHandle {
    /// The coordinator this handle wraps (the retry task's entry
    /// point).
    #[must_use]
    pub fn coordinator(&self) -> &Arc<MigrationCoordinator<DaemonHandoffDriver>> {
        &self.coordinator
    }

    /// The drive lock (the retry task's try_lock target).
    #[must_use]
    pub fn drive_lock(&self) -> &Arc<tokio::sync::Mutex<()>> {
        &self.drive
    }

    /// The migration store's store-save crash seam (P5 plan §3.1):
    /// the armed table a campaign rig aims and the kill switch fires
    /// into. Inert unless a rig arms it; no route or input reaches
    /// it (the doc-gated trust class in `volvisor-types::crash`).
    /// Fresh per `wire_migration` call - a rig re-registers its kill
    /// switch on every daemon restart.
    #[must_use]
    pub fn store_crash_hooks(&self) -> Arc<volvisor_types::crash::StoreCrashHooks> {
        self.coordinator.store_crash_hooks()
    }

    /// The number of live transfer drive tasks (plan §3.3: the
    /// supervisor's kill-group enumeration input — the detached drive
    /// is part of the task group the rig aborts as one unit).
    #[must_use]
    pub fn live_drive_tasks(&self) -> usize {
        lock_registry(&self.drive_tasks).live()
    }

    /// Abort and drain every tracked transfer drive task (plan §3.3:
    /// the supervisor's kill act). An aborted task cannot run its own
    /// final act, so the registry reaps the handle here; a task that
    /// already finished resolves immediately. The handles are taken
    /// out under the lock first — the lock is never held across an
    /// await. Aborting the drive *task* is not aborting the
    /// *migration*: the record keeps its durable state.
    pub async fn abort_drive_tasks(&self) {
        // The handles are taken out under the lock first — the lock
        // is never held across an await.
        let handles = lock_registry(&self.drive_tasks).take_live();
        for handle in handles {
            handle.abort();
            // Drain: the abort resolves the task (a kill that leaves
            // the drive mutating witness and migration state is not a
            // kill).
            let _ = handle.await;
        }
    }
}

#[async_trait]
impl MigrationSurface for MigrationHandle {
    async fn prepare(&self, request: MobilityRequest) -> Result<MigrationSummary, ApiError> {
        request.validate()?;
        // Enrich against the provider first: a typed refusal when a
        // volume is unknown, its generation stale or its attachment
        // names a different VM (rule 6) — before the peer is called
        // and before anything is journaled by the coordinator.
        let mut participants = Vec::with_capacity(request.volume_ids.len());
        for (volume_id, expected_generation) in
            request.volume_ids.iter().zip(&request.expected_generations)
        {
            let (resource, minor) = (self.facts)(volume_id, &request.vm_id, *expected_generation)?;
            participants.push(Participant {
                volume_id: volume_id.clone(),
                expected_generation: *expected_generation,
                resource,
                minor,
            });
        }
        let record = self
            .coordinator
            .prepare(PrepareHandoffRequest {
                migration_id: request.migration_id,
                vm_id: request.vm_id,
                source_host: self.host_id.clone(),
                target_host: request.target_host,
                participants,
            })
            .await?;
        Ok(record.observe())
    }

    async fn transfer(
        &self,
        migration_id: &MigrationId,
        proof: serde_json::Value,
    ) -> Result<MigrationSummary, ApiError> {
        // The proof is durable before the drive starts (crash between
        // the 202 and the task loses nothing): corroboration, recorded
        // verbatim, never an input.
        let record = self
            .coordinator
            .record_consumer_proof(migration_id, proof)?;
        let at_start = record.observe();
        let coordinator = Arc::clone(&self.coordinator);
        let drive = Arc::clone(&self.drive);
        let drive_tasks = Arc::clone(&self.drive_tasks);
        let migration_id = migration_id.clone();
        let task_id = lock_registry(&self.drive_tasks).alloc();
        let task = tokio::spawn(async move {
            // One drive at a time: a retry pass in flight is awaited,
            // never raced.
            let _guard = drive.lock().await;
            match coordinator.transfer(&migration_id).await {
                Ok(record) => tracing::info!(
                    kind = "migration_drive",
                    migration_id = %migration_id,
                    state = %record.state,
                    "migration drive landed"
                ),
                Err(error) => tracing::error!(
                    kind = "migration_drive",
                    migration_id = %migration_id,
                    error = %error,
                    "migration drive failed; the record keeps its durable state and the \
                     retry task re-drives it"
                ),
            }
            // The final act (plan §3.3): the task removes its own
            // registry slot, so the registry never grows on graceful
            // paths.
            lock_registry(&drive_tasks).finish(task_id);
        });
        // Record-after-spawn (plan §3.3): the supervisor's kill group
        // includes this drive. A fail-fast task may already have
        // finished — `record` drops the handle in that case.
        lock_registry(&self.drive_tasks).record(task_id, task);
        Ok(at_start)
    }

    fn observe(&self, migration_id: &MigrationId) -> Result<Option<MigrationSummary>, ApiError> {
        self.coordinator.observe(migration_id)
    }

    async fn abort(&self, migration_id: &MigrationId) -> Result<MigrationSummary, ApiError> {
        // Under the drive lock: the G5-ordered rollback never
        // interleaves with a live drive of the same surface.
        let _guard = self.drive.lock().await;
        let record = self.coordinator.abort(migration_id).await?;
        Ok(record.observe())
    }
}

/// Spawn the migration retry task (plan §3's periodic reconcile, the
/// renewal-task pattern): an immediate startup pass, then one pass
/// every `MIGRATION_RETRY_TICK`. A pass takes the drive lock with
/// `try_lock` — a live consumer drive owns the surface and the pass
/// skips itself — and drives `resolve` over every record that is
/// neither `Complete` nor `Aborted`, including terminal `InDoubt`
/// (whose re-attempt `resolve` itself gates on witness
/// reachability). Every outcome is a structured event; the task never
/// panics and never crashes the daemon.
///
/// Note the committed B1 semantics this wiring inherits: a pre-cut
/// record with no cut is **rolled back** by the reconcile (the
/// `AutoBeforeCut` policy) — an un-transferred preparation does not
/// linger; the consumer re-issues it.
#[must_use]
pub fn spawn_migration_retry_task(handle: Arc<MigrationHandle>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        retry_pass(&handle).await;
        loop {
            tokio::time::sleep(MIGRATION_RETRY_TICK).await;
            retry_pass(&handle).await;
        }
    })
}

/// One reconcile pass (see [`spawn_migration_retry_task`]).
async fn retry_pass(handle: &MigrationHandle) {
    let Ok(_guard) = handle.drive.try_lock() else {
        // A consumer drive (or an abort) holds the surface: skip, the
        // next tick re-attempts.
        return;
    };
    let ids = match handle.coordinator.list_ids() {
        Ok(ids) => ids,
        Err(error) => {
            tracing::error!(kind = "migration_resolve", error = %error, "retry pass could not list migrations");
            return;
        }
    };
    for migration_id in ids {
        match handle.coordinator.resolve(&migration_id).await {
            Ok(record) => tracing::info!(
                kind = "migration_resolve",
                migration_id = %migration_id,
                state = %record.state,
                "migration reconcile pass"
            ),
            Err(error) => tracing::error!(
                kind = "migration_resolve",
                migration_id = %migration_id,
                error = %error,
                "migration reconcile failed; retried next pass"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// The wiring
// ---------------------------------------------------------------------------

/// Compose the daemon's migration roles from the real surfaces: the
/// source-role driver + coordinator + consumer handle, and the
/// destination-side [`PeerRouteContext`] for the internal peer routes
/// (both roles share the witness connection, the VMM controller seam
/// and the host identity — one daemon serves whichever role the
/// migration assigns it).
///
/// Generic over the concrete witness connection type because the
/// blocking adapter ([`BlockingWitness`]) wraps a sized `C` (the
/// `HttpWitnessConnection` in the runtime, a test double in the
/// driver tests); the source-role driver receives the same connection
/// behind the async trait object.
///
/// Must be called from an async context (the blocking witness adapter
/// captures the runtime handle, the `BlockingWitness` discipline).
///
/// # Errors
/// [`DaemonError::Config`] when either durable store cannot be opened
/// (a corrupt record file is a typed startup error, never silently
/// dropped).
#[allow(clippy::too_many_arguments)]
pub fn wire_migration<C>(
    host_id: HostId,
    witness: Arc<C>,
    witness_probe: SocketAddr,
    vmm: Arc<dyn VmmController>,
    handoff: Arc<dyn HandoffSurface>,
    provider: Arc<dyn volvisor_provider::VolumeProvider>,
    peer: Arc<dyn PeerClient>,
    facts: ParticipantFacts,
    snapshot_root: PathBuf,
    records_dir: impl Into<PathBuf>,
    preparations_dir: impl Into<PathBuf>,
    clock: Clock,
) -> Result<(Arc<MigrationHandle>, Arc<PeerRouteContext>), DaemonError>
where
    C: WitnessConnection + 'static,
{
    let store = MigrationStore::open(records_dir)
        .map_err(|error| DaemonError::Config(format!("migration store failed to open: {error}")))?;
    let driver_witness: Arc<dyn WitnessConnection> = witness.clone();
    let driver = Arc::new(DaemonHandoffDriver::new(
        host_id.clone(),
        driver_witness,
        witness_probe,
        Arc::clone(&vmm),
        Arc::clone(&handoff),
        peer,
        snapshot_root.clone(),
    ));
    let coordinator = Arc::new(MigrationCoordinator::new(Arc::clone(&driver), store, clock));
    let handle = Arc::new(MigrationHandle {
        coordinator,
        facts,
        host_id: host_id.clone(),
        drive: Arc::new(tokio::sync::Mutex::new(())),
        drive_tasks: Arc::new(Mutex::new(DriveTaskRegistry::default())),
    });
    // The destination-side half: the same witness connection behind
    // the blocking adapter, the same VMM seam and handoff surface the
    // peer routes drive on the destination host.
    let blocking: Arc<dyn BlockingWitnessConnection> = Arc::new(BlockingWitness::new(
        witness,
        tokio::runtime::Handle::current(),
        PEER_REQUEST_TIMEOUT.min(Duration::from_secs(5)),
    ));
    let preparations = TargetPreparationStore::open(preparations_dir).map_err(|error| {
        DaemonError::Config(format!("peer-preparation store failed to open: {error}"))
    })?;
    let peer_context = Arc::new(PeerRouteContext::new(
        blocking,
        vmm,
        handoff,
        provider,
        host_id,
        preparations,
        snapshot_root,
    ));
    Ok((handle, peer_context))
}

/// The witness probe target of a configured URL (the
/// [`DaemonHandoffDriver`] constructor wants it; exposed for the
/// runtime's wiring).
///
/// # Errors
/// The same failures as the private `witness_probe_addr` (a non-HTTP
/// scheme, a malformed port, an unresolvable name) — all typed
/// `DaemonError::Config`.
pub fn migration_witness_probe(url: &str) -> Result<SocketAddr, DaemonError> {
    witness_probe_addr(url)
}

/// The migration records directory under the journal directory
/// (`{journal_dir}/migrations`, the `drbd-state.json` precedent: state
/// files live beside the journal, the journal's own lock untouched).
#[must_use]
pub fn migration_records_dir(journal_dir: &Path) -> PathBuf {
    journal_dir.join("migrations")
}

/// The destination-side preparation store directory under the journal
/// directory (`{journal_dir}/peer-preparations`).
#[must_use]
pub fn peer_preparations_dir(journal_dir: &Path) -> PathBuf {
    journal_dir.join("peer-preparations")
}

/// The deterministic batch operation id of a migration's grant step
/// (exported for the driver tests: the peer's id-only derivation must
/// equal the coordinator's full-participant one — the hash input is
/// the ordered volume set either way).
///
/// # Errors
/// `INTERNAL` only if the derived string failed identity validation
/// (unreachable for the fixed tag and hex alphabet).
pub fn grant_set_operation_id(
    migration_id: &MigrationId,
    participants: &[Participant],
) -> Result<volvisor_types::OperationId, ApiError> {
    batch_operation_id(
        migration_id,
        volvisor_handoff::BatchStep::GrantSet,
        participants,
    )
}

#[cfg(test)]
mod tests {
    // Test-kit code: `expect`/`unwrap` are allowed here by convention
    // (the drbd handoff tests' rule).
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::net::SocketAddr;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Duration;

    use volvisor_api::peer::{
        PeerGrantOutcome, PeerPrepareResponse, PeerPreparedVolume, PeerRestoreVmResponse,
    };
    use volvisor_handoff::{AbortPolicy, BatchStep, HandoffState};
    use volvisor_provider::vmm::{FakeVmm, VmState, VmmController};
    use volvisor_provider::{
        EligibilityParticipant, EligibilityReport, FakeProvider, HandoffSurface, QuiesceProof,
        SyncProof,
    };
    use volvisor_types::domain::{
        EffectiveProtection, EvidenceStatus, FailureDomain, Health, VolumeClass,
    };
    use volvisor_types::id::ProjectId;
    use volvisor_types::request::{AttachVolumeRequest, InspectVolumeResponse};
    use volvisor_types::state::VolumeLifecycle;
    use volvisor_types::{
        ApiErrorCode, EndpointBacking, HostId, LeaseState, MigrationId, OperationId, VolumeId,
    };
    use volvisor_witness::client::{HttpWitnessConnection, WitnessConnection};
    use volvisor_witness::proto::{
        GrantRequest, RegisterRequest, RegistrationContent, WITNESS_PROTOCOL_VERSION,
    };
    use volvisor_witness::registry::{WitnessCore, WitnessCoreConfig};
    use volvisor_witness::server::{WitnessServerState, router};

    use super::*;

    /// The source host of every driver test (the holder the driver's
    /// witness mutations assert).
    const NODE: &str = "driver-node-a";
    /// The destination host (the peer the GrantSet would belong to).
    const PEER: &str = "driver-node-b";
    /// The legacy shared token (read-only witness inspection).
    const TOKEN: &str = "driver-test-token";
    /// The source host's W8 credential.
    const NODE_TOKEN: &str = "driver-test-node-a-token";
    /// Deterministic witness knobs (ttl 100s; clocks at t=1000).
    const TTL: u64 = 100;
    const START: u64 = 1_000;

    // ------------------------------------------------------------- kit

    fn host(raw: &str) -> HostId {
        HostId::new(raw).expect("valid host id")
    }

    fn volume(raw: &str) -> VolumeId {
        VolumeId::new(raw).expect("valid volume id")
    }

    fn migration(raw: &str) -> MigrationId {
        MigrationId::new(raw).expect("valid migration id")
    }

    fn op(name: &str) -> OperationId {
        OperationId::new(format!("driver-op-{name}")).expect("valid operation id")
    }

    /// A participant with the full provider-local facts.
    fn participant(raw: &str, generation: u64, minor: u32) -> Participant {
        Participant {
            volume_id: volume(raw),
            expected_generation: generation,
            resource: format!("drbd-{raw}"),
            minor,
        }
    }

    /// A minimal migration record for the driver's set-shaped methods.
    fn record_for(raw: &str, participants: Vec<Participant>) -> MigrationRecord {
        MigrationRecord {
            migration_id: migration(raw),
            vm_id: "vm-1".to_owned(),
            source_host: host(NODE),
            target_host: host(PEER),
            participants,
            state: HandoffState::BarrierDurable,
            cut: None,
            state_history: Vec::new(),
            barrier_proofs: Vec::new(),
            abort_policy: AbortPolicy::AutoBeforeCut,
            consumer_proof: None,
            created_at: 0,
            updated_at: 0,
            cut_started_at: None,
            cut_completed_at: None,
        }
    }

    struct Server {
        addr: SocketAddr,
        /// Detached server task (kept for symmetry with the kit
        /// pattern; the task outlives the handle).
        _handle: tokio::task::JoinHandle<()>,
    }

    async fn spawn_witness(dir: &Path, clock: Arc<AtomicU64>) -> Server {
        let core = WitnessCore::open(
            dir,
            WitnessCoreConfig {
                lease_ttl_secs: TTL,
                lease_grace_secs: 5,
                suspend_budget_secs: 5,
            },
        )
        .expect("witness core opens");
        let mut host_tokens = std::collections::BTreeMap::new();
        host_tokens.insert(NODE.to_owned(), NODE_TOKEN.to_owned());
        let state = Arc::new(WitnessServerState::with_clock(
            core,
            Some(TOKEN.to_owned()),
            host_tokens,
            Arc::new(move || clock.load(Ordering::SeqCst)),
        ));
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server serves");
        });
        Server {
            addr,
            _handle: handle,
        }
    }

    /// A client presenting `host_credential` as its bearer token (a
    /// host's W8 credential for the mutating surface, the legacy
    /// shared token for read-only inspection).
    fn client_for(server: &Server, host_credential: Option<&str>) -> HttpWitnessConnection {
        HttpWitnessConnection::new(
            format!("http://{}", server.addr),
            host_credential.map(str::to_owned),
            Duration::from_secs(5),
        )
    }

    /// The loopback witness with its injected clock.
    struct WitnessKit {
        server: Server,
        /// Keeps the journal directory alive for the server's lifetime.
        _dir: tempfile::TempDir,
    }

    async fn witness_kit() -> WitnessKit {
        let dir = tempfile::tempdir().expect("witness dir");
        let clock = Arc::new(AtomicU64::new(START));
        let server = spawn_witness(dir.path(), clock).await;
        WitnessKit { server, _dir: dir }
    }

    /// Register a volume at the witness with both replication ends.
    async fn register_volume(kit: &WitnessKit, volume_id: &VolumeId) {
        client_for(&kit.server, Some(NODE_TOKEN))
            .register(
                volume_id,
                RegisterRequest {
                    protocol_version: WITNESS_PROTOCOL_VERSION,
                    operation_id: op("register"),
                    content: RegistrationContent {
                        lineage_uuids: vec![
                            "0000000000000004".to_owned(),
                            "0000000000000005".to_owned(),
                        ],
                        endpoints: vec![
                            EndpointBacking {
                                host_id: host(NODE),
                                backing: "vg/nlv-vol".to_owned(),
                                volvisor_created: true,
                            },
                            EndpointBacking {
                                host_id: host(PEER),
                                backing: "vg/nlv-vol".to_owned(),
                                volvisor_created: false,
                            },
                        ],
                        barrier: None,
                    },
                },
            )
            .await
            .expect("register");
    }

    /// Grant a live lease to the NODE host (the epoch the driver's
    /// barrier recording must assert).
    async fn grant_to_node(kit: &WitnessKit, volume_id: &VolumeId) {
        client_for(&kit.server, Some(NODE_TOKEN))
            .grant(
                volume_id,
                GrantRequest {
                    protocol_version: WITNESS_PROTOCOL_VERSION,
                    operation_id: op("grant-node"),
                    host_id: host(NODE),
                },
            )
            .await
            .expect("grant");
    }

    /// A scripted source-side handoff surface: counts and scripts the
    /// acts the driver maps onto it.
    struct FakeSurface {
        /// How many `track_sync` calls answer the retryable
        /// `REPLICA_NOT_DURABLE` refusal before converging.
        track_sync_lag: AtomicU64,
        /// `track_sync` never converges (the bound-expiry path).
        track_sync_stuck: AtomicBool,
        /// The scripted `abort_prepare` refusal (None → success).
        abort_refusal: Mutex<Option<ApiError>>,
        /// The scripted `clear_cut_marker` refusal (None → success).
        clear_refusal: Mutex<Option<ApiError>>,
        /// The `fail_closed_fence` reasons (the fence path's evidence).
        fences: Mutex<Vec<String>>,
    }

    impl FakeSurface {
        fn new() -> Self {
            Self {
                track_sync_lag: AtomicU64::new(0),
                track_sync_stuck: AtomicBool::new(false),
                abort_refusal: Mutex::new(None),
                clear_refusal: Mutex::new(None),
                fences: Mutex::new(Vec::new()),
            }
        }
    }

    fn not_scripted() -> ApiError {
        ApiError::not_found("not scripted")
    }

    fn inspect_response(volume_id: &VolumeId) -> InspectVolumeResponse {
        InspectVolumeResponse {
            volume_id: volume_id.clone(),
            backend_class: VolumeClass::NearlineReplicated,
            project_id: ProjectId::new("proj").expect("valid project id"),
            generation: 1,
            state: VolumeLifecycle::Ready,
            provisioned_bytes: 0,
            allocated_bytes: 0,
            effective_protection: EffectiveProtection::default(),
            failure_domain: FailureDomain::Host,
            health: Health::Unknown,
            attachment_ids: Vec::new(),
            current_writer: None,
            backend_health: Health::Unknown,
            evidence_status: EvidenceStatus::default(),
            authority: None,
        }
    }

    #[async_trait]
    impl HandoffSurface for FakeSurface {
        async fn handoff_eligibility(&self, vm_id: &str) -> Result<EligibilityReport, ApiError> {
            Ok(EligibilityReport {
                vm_id: vm_id.to_owned(),
                eligible: true,
                participants: vec![EligibilityParticipant {
                    volume_id: volume("vol-e"),
                    eligible: true,
                    reasons: Vec::new(),
                }],
            })
        }

        async fn quiesce_for_barrier(
            &self,
            volume_id: &VolumeId,
            migration_id: &MigrationId,
        ) -> Result<QuiesceProof, ApiError> {
            Ok(QuiesceProof {
                volume_id: volume_id.clone(),
                migration_id: migration_id.clone(),
                observed_suspended: true,
                cut_marker_durable: true,
                suspended_at: 0,
            })
        }

        async fn replica_caught_up(&self, volume_id: &VolumeId) -> Result<(), ApiError> {
            // The pre-quiesce observation shares the fake's
            // convergence model (the lag knobs) but mints no proof —
            // mirroring the real split from `track_sync`.
            self.track_sync(volume_id).await.map(|_| ())
        }

        async fn track_sync(&self, volume_id: &VolumeId) -> Result<SyncProof, ApiError> {
            let lag = self.track_sync_lag.fetch_sub(1, Ordering::SeqCst);
            if self.track_sync_stuck.load(Ordering::SeqCst) || lag > 0 {
                return Err(ApiError::new(
                    ApiErrorCode::ReplicaNotDurable,
                    format!("the peer of {volume_id} has not converged (fake lag)"),
                ));
            }
            Ok(SyncProof {
                volume_id: volume_id.clone(),
                peer_up_to_date: true,
                resync_active: false,
                connection_established: true,
                observed_after_suspension: true,
                observed_at: 0,
            })
        }

        async fn release_source(
            &self,
            _volume_id: &VolumeId,
            _migration_id: &MigrationId,
        ) -> Result<(), ApiError> {
            Ok(())
        }

        async fn abort_prepare(
            &self,
            _volume_id: &VolumeId,
            _migration_id: &MigrationId,
        ) -> Result<(), ApiError> {
            match self.abort_refusal.lock().expect("abort refusal").take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        async fn clear_cut_marker(
            &self,
            volume_id: &VolumeId,
            _proof: Option<&volvisor_types::FencingProof>,
        ) -> Result<InspectVolumeResponse, ApiError> {
            match self.clear_refusal.lock().expect("clear refusal").take() {
                Some(error) => Err(error),
                None => Ok(inspect_response(volume_id)),
            }
        }

        async fn promote_target(
            &self,
            _volume_id: &VolumeId,
            _migration_id: &MigrationId,
            _attach: &AttachVolumeRequest,
        ) -> Result<volvisor_types::request::AttachVolumeResponse, ApiError> {
            Err(not_scripted())
        }

        async fn verify_target_replica(
            &self,
            _volume_id: &VolumeId,
            _expected_lineage: &[String],
        ) -> Result<(), ApiError> {
            // The daemon-wiring tests reach this surface only through
            // the driver, never the peer routes (StubPeer owns the
            // route answers); every call is an honest `Ok`.
            Ok(())
        }

        async fn role_secondary(&self, _volume_id: &VolumeId) -> Result<bool, ApiError> {
            Ok(true)
        }

        async fn source_lineage(&self, _volume_id: &VolumeId) -> Result<Vec<String>, ApiError> {
            // The daemon-wiring tests never assert the lineage's
            // content (StubPeer owns the route answers).
            Ok(vec!["stub-lineage".to_owned()])
        }

        async fn fail_closed_fence(
            &self,
            _volume_id: &VolumeId,
            reason: &str,
        ) -> Result<(), ApiError> {
            self.fences.lock().expect("fences").push(reason.to_owned());
            Ok(())
        }
    }

    /// A scripted peer daemon: records every call, answers from the
    /// scripted outcomes.
    struct StubPeer {
        /// The participant set `prepare` verifies (order included).
        prepare_participants: Mutex<Vec<Participant>>,
        /// The grant outcomes (empty → no grants for anyone).
        grants: Vec<PeerGrantOutcome>,
        /// The `restore-vm` response's observed VM state.
        restore_state: Mutex<VmState>,
        /// The recorded call kinds, in order.
        calls: Mutex<Vec<&'static str>>,
        /// The observed `restore-vm` requests (disks, resume flag,
        /// snapshot dir), in order.
        restores: Mutex<Vec<(Vec<DiskMapping>, bool, String)>>,
    }

    impl StubPeer {
        fn new(grants: Vec<PeerGrantOutcome>) -> Self {
            Self {
                prepare_participants: Mutex::new(Vec::new()),
                grants,
                restore_state: Mutex::new(VmState::Running),
                calls: Mutex::new(Vec::new()),
                restores: Mutex::new(Vec::new()),
            }
        }

        fn record(&self, kind: &'static str) {
            self.calls.lock().expect("calls").push(kind);
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().expect("calls").clone()
        }
    }

    fn grant_outcome(raw: &str, device_path: &str) -> PeerGrantOutcome {
        PeerGrantOutcome {
            volume_id: volume(raw),
            device_path: device_path.to_owned(),
            epoch: 2,
            lease_id: 9,
            lease_ttl_secs: TTL,
        }
    }

    #[async_trait]
    impl PeerClient for StubPeer {
        async fn prepare(
            &self,
            request: volvisor_api::peer::PeerPrepareRequest,
        ) -> Result<PeerPrepareResponse, ApiError> {
            self.record("prepare");
            Ok(PeerPrepareResponse {
                migration_id: request.migration_id,
                vm_id: request.vm_id,
                participants: self
                    .prepare_participants
                    .lock()
                    .expect("prepare participants")
                    .clone()
                    .into_iter()
                    .map(|p| PeerPreparedVolume {
                        volume_id: p.volume_id,
                        expected_generation: p.expected_generation,
                    })
                    .collect(),
            })
        }

        async fn grant(
            &self,
            request: volvisor_api::peer::PeerGrantRequest,
        ) -> Result<volvisor_api::peer::PeerGrantResponse, ApiError> {
            self.record("grant");
            Ok(volvisor_api::peer::PeerGrantResponse {
                migration_id: request.migration_id,
                grants: self.grants.clone(),
            })
        }

        async fn restore_vm(
            &self,
            request: volvisor_api::peer::PeerRestoreVmRequest,
        ) -> Result<PeerRestoreVmResponse, ApiError> {
            self.record("restore_vm");
            self.restores.lock().expect("restores").push((
                request.disks,
                request.resume,
                request.snapshot_dir,
            ));
            Ok(PeerRestoreVmResponse {
                migration_id: request.migration_id,
                vm_state: *self.restore_state.lock().expect("restore state"),
            })
        }

        async fn discard(
            &self,
            request: volvisor_api::peer::PeerDiscardRequest,
        ) -> Result<volvisor_api::peer::PeerDiscardResponse, ApiError> {
            self.record("discard");
            Ok(volvisor_api::peer::PeerDiscardResponse {
                migration_id: request.migration_id,
                discarded: true,
            })
        }
    }

    /// The driver over a loopback witness, a fake VMM, a scripted
    /// surface and a scripted peer.
    struct DriverKit {
        witness: WitnessKit,
        vmm: Arc<FakeVmm>,
        surface: Arc<FakeSurface>,
        peer: Arc<StubPeer>,
        driver: Arc<DaemonHandoffDriver>,
        snapshot_dir: tempfile::TempDir,
    }

    async fn driver_kit() -> DriverKit {
        driver_kit_with(Vec::new()).await
    }

    async fn driver_kit_with(grants: Vec<PeerGrantOutcome>) -> DriverKit {
        let witness = witness_kit().await;
        let snapshot_dir = tempfile::tempdir().expect("snapshot dir");
        let vmm = Arc::new(FakeVmm::new(snapshot_dir.path()));
        let surface = Arc::new(FakeSurface::new());
        let peer = Arc::new(StubPeer::new(grants));
        let driver = Arc::new(
            DaemonHandoffDriver::new(
                host(NODE),
                Arc::new(client_for(&witness.server, Some(NODE_TOKEN))),
                witness.server.addr,
                vmm.clone(),
                surface.clone(),
                peer.clone(),
                snapshot_dir.path().to_owned(),
            )
            .with_track_sync_bounds(Duration::from_secs(1), Duration::from_millis(10)),
        );
        DriverKit {
            witness,
            vmm,
            surface,
            peer,
            driver,
            snapshot_dir,
        }
    }

    // ------------------------------------------------- unit-level tests

    #[test]
    fn witness_probe_addr_accepts_literals_and_names_and_refuses_other_schemes() {
        let literal = witness_probe_addr("http://127.0.0.1:9101").expect("literal");
        assert_eq!(literal.to_string(), "127.0.0.1:9101");
        let bracketed = witness_probe_addr("http://[::1]:9101").expect("bracketed literal");
        assert_eq!(bracketed.to_string(), "[::1]:9101");
        // A portless authority defaults to 80.
        let defaulted = witness_probe_addr("http://127.0.0.1").expect("default port");
        assert_eq!(defaulted.port(), 80);
        // A path after the authority is ignored.
        let with_path = witness_probe_addr("http://127.0.0.1:9101/witness").expect("path");
        assert_eq!(with_path.port(), 9101);
        // A name resolves through the system resolver.
        let named = witness_probe_addr("http://localhost:9101").expect("name");
        assert!(
            named.ip().is_loopback(),
            "localhost resolves to loopback: {named}"
        );
        // Only the plain-HTTP scheme is the witness surface.
        let scheme =
            witness_probe_addr("https://127.0.0.1:9101").expect_err("https must be refused");
        assert!(scheme.to_string().contains("http:// scheme"), "{scheme}");
        let garbage = witness_probe_addr("not a url").expect_err("garbage must be refused");
        assert!(garbage.to_string().contains("http:// scheme"), "{garbage}");
    }

    #[test]
    fn migration_stores_live_under_the_journal_dir() {
        let journal = Path::new("/j");
        assert_eq!(migration_records_dir(journal), Path::new("/j/migrations"));
        assert_eq!(
            peer_preparations_dir(journal),
            Path::new("/j/peer-preparations")
        );
    }

    #[test]
    fn grant_set_operation_id_hashes_only_the_ordered_volume_set() {
        let mig = migration("mig-op-id");
        // The peer's id-only derivation (empty facts) equals the
        // coordinator's full-participant one: the hash input is the
        // ordered volume set either way.
        let full = vec![participant("vol-a", 3, 7), participant("vol-b", 4, 8)];
        let id_only = vec![
            Participant {
                volume_id: volume("vol-a"),
                expected_generation: 0,
                resource: String::new(),
                minor: 0,
            },
            Participant {
                volume_id: volume("vol-b"),
                expected_generation: 0,
                resource: String::new(),
                minor: 0,
            },
        ];
        assert_eq!(
            grant_set_operation_id(&mig, &full).expect("full id"),
            batch_operation_id(&mig, BatchStep::GrantSet, &id_only).expect("id-only"),
            "the peer and the coordinator must derive the same batch id"
        );
        // Determinism: the same input derives the same id.
        assert_eq!(
            grant_set_operation_id(&mig, &full).expect("first"),
            grant_set_operation_id(&mig, &full).expect("second")
        );
        // Order is part of the input: a reordered set is a different
        // migration transaction.
        let reversed = vec![participant("vol-b", 4, 8), participant("vol-a", 3, 7)];
        assert_ne!(
            grant_set_operation_id(&mig, &full).expect("ordered"),
            grant_set_operation_id(&mig, &reversed).expect("reordered")
        );
    }

    // ------------------------------------------------ driver-mapping tests

    #[tokio::test]
    async fn grant_set_goes_to_the_peer_never_this_hosts_witness() {
        let kit = driver_kit().await;
        let vol = volume("vol-grant");
        register_volume(&kit.witness, &vol).await;
        let record = record_for("mig-grant", vec![participant("vol-grant", 1, 100)]);

        kit.driver
            .grant_set(&record, &op("grant-set"))
            .await
            .expect("grant set");

        // The peer daemon was called (W8: the GrantSet is the
        // destination's own witness mutation).
        assert_eq!(kit.peer.calls(), vec!["grant"]);
        // This host's witness was NOT asked to grant: the volume has
        // no lease and no holder.
        let view = client_for(&kit.witness.server, Some(TOKEN))
            .inspect(&vol)
            .await
            .expect("view");
        assert_ne!(
            view.lease_state,
            LeaseState::Live,
            "no live lease: {view:?}"
        );
        assert_eq!(view.holder, None, "no holder was granted: {view:?}");
    }

    #[tokio::test]
    async fn record_barrier_uses_the_inspected_epoch_and_the_all_true_attestation() {
        let kit = driver_kit().await;
        let vol = volume("vol-barrier");
        register_volume(&kit.witness, &vol).await;
        grant_to_node(&kit.witness, &vol).await;
        let mig = migration("mig-barrier");

        let proof = kit
            .driver
            .record_barrier(&vol, &mig, &op("record-barrier"))
            .await
            .expect("record barrier");
        assert_eq!(proof.volume_id, vol);
        assert_eq!(
            proof.attestation,
            BarrierAttestation {
                vm_paused_and_drained: true,
                data_path_suspended: true,
                peer_up_to_date: true,
            }
        );

        let view = client_for(&kit.witness.server, Some(TOKEN))
            .inspect(&vol)
            .await
            .expect("view");
        assert_eq!(view.barriers.len(), 1, "one recorded barrier: {view:?}");
        let barrier = &view.barriers[0];
        assert_eq!(barrier.holder, host(NODE));
        assert_eq!(barrier.epoch, view.current_epoch);
        assert_eq!(barrier.migration_id, Some(mig));
        assert!(barrier.attestation.vm_paused_and_drained);
        assert!(barrier.attestation.data_path_suspended);
        assert!(barrier.attestation.peer_up_to_date);
        assert_eq!(barrier.boundary_commit_index, proof.boundary_commit_index);
    }

    #[tokio::test]
    async fn void_barriers_confirm_by_state_and_replay_the_deterministic_op_id() {
        let kit = driver_kit().await;
        let vol = volume("vol-void");
        register_volume(&kit.witness, &vol).await;
        grant_to_node(&kit.witness, &vol).await;
        let mig = migration("mig-void");
        let proof = kit
            .driver
            .record_barrier(&vol, &mig, &op("record-barrier"))
            .await
            .expect("record barrier");
        let mut record = record_for("mig-void", vec![participant("vol-void", 1, 100)]);
        record.barrier_proofs = vec![proof.clone()];

        // The first void act voids the unvoided entry.
        kit.driver.void_barriers(&record).await.expect("void");
        let view = client_for(&kit.witness.server, Some(TOKEN))
            .inspect(&vol)
            .await
            .expect("view");
        assert!(
            view.barriers[0].voided,
            "barrier voided: {:?}",
            view.barriers
        );

        // The re-drive (a lost response, a crash) confirms by state:
        // already voided is success, with no fresh mutation.
        kit.driver
            .void_barriers(&record)
            .await
            .expect("re-drive confirms by state");

        // A proof the witness holds no barrier for never resolves as
        // success — the rollback fails closed into the fence path.
        let mut foreign = record_for("mig-other", vec![participant("vol-void", 1, 100)]);
        foreign.barrier_proofs = vec![BarrierProof {
            volume_id: vol,
            boundary_commit_index: proof.boundary_commit_index,
            attestation: proof.attestation,
            recorded_at: proof.recorded_at,
        }];
        let error = kit
            .driver
            .void_barriers(&foreign)
            .await
            .expect_err("a proof the witness does not hold must refuse");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(
            error.detail.contains("holds no barrier"),
            "typed refusal names the mismatch: {error}"
        );
    }

    #[tokio::test]
    async fn revoke_set_replays_the_recorded_outcome_under_the_same_op_id() {
        let kit = driver_kit().await;
        let vol = volume("vol-revoke");
        register_volume(&kit.witness, &vol).await;
        grant_to_node(&kit.witness, &vol).await;
        let record = record_for("mig-revoke", vec![participant("vol-revoke", 1, 100)]);
        let revoke_op = op("revoke-set");

        kit.driver
            .revoke_set(&record, &revoke_op)
            .await
            .expect("revoke set");
        let view = client_for(&kit.witness.server, Some(TOKEN))
            .inspect(&vol)
            .await
            .expect("view");
        assert_ne!(
            view.lease_state,
            LeaseState::Live,
            "lease released: {view:?}"
        );

        // The re-drive replays the journaled outcome under the same
        // deterministic op id (a fresh id would mint a redundant
        // epoch retirement attempt).
        kit.driver
            .revoke_set(&record, &revoke_op)
            .await
            .expect("same op id replays");
    }

    #[tokio::test]
    async fn witness_refusals_surface_typed_never_as_panics() {
        // A witness nothing answers: every observation maps onto a
        // typed INTERNAL error ("witness unreachable"), never a panic
        // or a silent success.
        let dir = tempfile::tempdir().expect("snapshot dir");
        let vmm = Arc::new(FakeVmm::new(dir.path()));
        let surface = Arc::new(FakeSurface::new());
        let peer = Arc::new(StubPeer::new(Vec::new()));
        let dead = SocketAddr::from(([127, 0, 0, 1], 1));
        let driver = DaemonHandoffDriver::new(
            host(NODE),
            Arc::new(HttpWitnessConnection::new(
                "http://127.0.0.1:1",
                Some(NODE_TOKEN.to_owned()),
                Duration::from_millis(200),
            )),
            dead,
            vmm,
            surface,
            peer,
            dir.path().to_owned(),
        );

        let error = driver
            .witness_view(&volume("vol-dead"))
            .await
            .expect_err("unreachable witness must refuse typed");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(
            error.detail.contains("witness unreachable"),
            "the detail names the transport failure: {error}"
        );
        // The synchronous probe answers false, fail-closed: the
        // record would stay IN_DOUBT for the next pass.
        assert!(!driver.witness_reachable());
    }

    #[tokio::test]
    async fn resume_vm_disambiguates_by_the_observed_local_state() {
        let kit = driver_kit().await;

        // Paused → the verified local resume (the rollback path).
        kit.vmm
            .create("vm-resume", &["/dev/drbd100"])
            .expect("create");
        kit.vmm.start("vm-resume").expect("start");
        kit.vmm.pause("vm-resume").expect("pause");
        kit.driver.resume_vm("vm-resume").await.expect("resume");
        assert_eq!(
            kit.vmm.vm_state("vm-resume").expect("state"),
            VmState::Running
        );

        // Running → already resumed, idempotent.
        kit.driver.resume_vm("vm-resume").await.expect("idempotent");

        // Absent → the forward path (the destination's restore+resume
        // is one peer act; nothing local to resume).
        kit.driver.resume_vm("vm-absent").await.expect("absent");

        // Created → a defined-but-not-running local VM is never the
        // migration's resume target.
        kit.vmm
            .create("vm-created", &["/dev/drbd100"])
            .expect("create");
        let error = kit
            .driver
            .resume_vm("vm-created")
            .await
            .expect_err("created must refuse typed");
        assert_eq!(error.code, ApiErrorCode::InvalidState);
        assert!(
            error.detail.contains("never the migration's resume target"),
            "the refusal names the rule: {error}"
        );
    }

    #[tokio::test]
    async fn track_sync_waits_inside_the_bound_and_expires_typed() {
        let kit = driver_kit().await;
        let vol = volume("vol-sync");

        // Two retryable refusals, then convergence: the driver waits,
        // never assumes (plan §9 row 9).
        kit.surface.track_sync_lag.store(2, Ordering::SeqCst);
        kit.driver
            .track_sync(&vol)
            .await
            .expect("convergence inside the bound");
        // The lag counter is saturated at 0 by now (the third call
        // observed success); the waits happened between them.

        // A peer that never converges surfaces the typed, retryable
        // refusal once the bound expires — never an infinite wait.
        kit.surface.track_sync_stuck.store(true, Ordering::SeqCst);
        let error = kit
            .driver
            .track_sync(&vol)
            .await
            .expect_err("the bound must expire typed");
        assert_eq!(error.code, ApiErrorCode::ReplicaNotDurable);
        assert!(
            error.detail.contains("did not converge"),
            "the expiry names the bound: {error}"
        );
    }

    #[tokio::test]
    async fn already_resolved_refusals_map_to_success_only_on_the_exact_details() {
        let kit = driver_kit().await;
        let vol = volume("vol-idem");
        let mig = migration("mig-idem");

        // The provider's exact "no cut to abort" refusal.
        *kit.surface.abort_refusal.lock().expect("abort") = Some(ApiError::new(
            ApiErrorCode::InvalidState,
            "volume vol-idem carries no migration-cut marker: there is no cut to abort",
        ));
        kit.driver
            .unsuspend_source(&vol, &mig)
            .await
            .expect("no cut to abort is the idempotent answer");

        // The provider's exact "nothing to clear" refusal.
        *kit.surface.clear_refusal.lock().expect("clear") = Some(ApiError::new(
            ApiErrorCode::InvalidState,
            "volume vol-idem carries no migration-cut marker: there is nothing to clear",
        ));
        kit.driver
            .clear_cut_marker(&vol, &mig)
            .await
            .expect("nothing to clear is the idempotent answer");

        // A genuine INVALID_STATE refusal never silently becomes a
        // success (the mapping is narrow, not code-wide).
        *kit.surface.abort_refusal.lock().expect("abort") = Some(ApiError::new(
            ApiErrorCode::InvalidState,
            "resource drbd-vol is already Secondary while the cut marker is still present",
        ));
        let error = kit
            .driver
            .unsuspend_source(&vol, &mig)
            .await
            .expect_err("a genuine refusal passes through");
        assert_eq!(error.code, ApiErrorCode::InvalidState);
        assert!(
            error.detail.contains("already Secondary"),
            "the genuine refusal is preserved verbatim: {error}"
        );
    }

    #[tokio::test]
    async fn prepare_target_refuses_a_peer_verified_participant_set_mismatch() {
        let kit = driver_kit().await;
        let record = record_for(
            "mig-prepare",
            vec![participant("vol-a", 3, 7), participant("vol-b", 4, 8)],
        );

        // A matching verification (same order, same generations).
        *kit.peer.prepare_participants.lock().expect("participants") =
            vec![participant("vol-a", 3, 7), participant("vol-b", 4, 8)];
        kit.driver
            .prepare_target(&record)
            .await
            .expect("matching participant set");

        // A reordering is a different transaction — never trusted.
        *kit.peer.prepare_participants.lock().expect("participants") =
            vec![participant("vol-b", 4, 8), participant("vol-a", 3, 7)];
        let error = kit
            .driver
            .prepare_target(&record)
            .await
            .expect_err("a reordered set must refuse");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(
            error.detail.contains("different participant set"),
            "the refusal names the mismatch: {error}"
        );
    }

    #[tokio::test]
    async fn promote_and_restore_use_the_peer_grant_outcomes() {
        let kit = driver_kit_with(vec![grant_outcome("vol-promote", "/dev/drbd-by-target")]).await;
        let vol = volume("vol-promote");
        let mig = migration("mig-promote");
        let record = record_for("mig-promote", vec![participant("vol-promote", 1, 100)]);

        // Promote re-calls the idempotent peer grant and requires the
        // participant's outcome.
        kit.driver
            .promote_target(&vol, &mig)
            .await
            .expect("promote via the peer grant");
        let other = volume("vol-absent-from-grants");
        let error = kit
            .driver
            .promote_target(&other, &mig)
            .await
            .expect_err("a participant without a grant outcome must refuse");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(
            error.detail.contains("no outcome"),
            "the refusal names the missing outcome: {error}"
        );

        // Restore maps the source-declared path to the promoted
        // device path and demands the Running observation.
        kit.driver.restore_vm(&record).await.expect("restore");
        let (disks, resume, snapshot_dir) = kit
            .peer
            .restores
            .lock()
            .expect("restores")
            .last()
            .cloned()
            .expect("one restore call");
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].declared_path, "/dev/drbd100");
        assert_eq!(disks[0].device_path, "/dev/drbd-by-target");
        assert!(resume, "restore and resume are one peer act");
        assert_eq!(
            snapshot_dir,
            kit.snapshot_dir.path().join("vm-1").to_string_lossy(),
            "the snapshot dir is keyed by the vm_id"
        );

        // A destination that reports anything but Running is a typed
        // refusal, never an assumed resume.
        *kit.peer.restore_state.lock().expect("restore state") = VmState::Paused;
        let error = kit
            .driver
            .restore_vm(&record)
            .await
            .expect_err("a non-Running observation must refuse");
        assert_eq!(error.code, ApiErrorCode::InvalidState);
        assert!(
            error
                .detail
                .contains("Running is the only acceptable answer"),
            "the refusal names the required observation: {error}"
        );
    }

    #[tokio::test]
    async fn retry_pass_skips_when_the_drive_lock_is_held() {
        // The full composition (the runtime's wiring over fakes): both
        // roles from one `wire_migration` call.
        let witness = witness_kit().await;
        let snapshot_dir = tempfile::tempdir().expect("snapshot dir");
        let dirs = tempfile::tempdir().expect("store dir");
        let vmm: Arc<dyn VmmController> = Arc::new(FakeVmm::new(snapshot_dir.path()));
        let surface: Arc<dyn HandoffSurface> = Arc::new(FakeSurface::new());
        let stub = Arc::new(StubPeer::new(Vec::new()));
        // The destination verifies exactly the participant set the
        // preparation names (same order, same generations).
        *stub.prepare_participants.lock().expect("participants") =
            vec![participant("vol-retry", 1, 100)];
        let peer: Arc<dyn PeerClient> = stub;
        let facts: ParticipantFacts =
            Arc::new(|_volume, _vm, _generation| Ok(("drbd-res".to_owned(), 100_u32)));
        // The volume is registered at the witness: the reconcile's
        // external-facts fold inspects every participant before it
        // chooses a direction.
        register_volume(&witness, &volume("vol-retry")).await;
        let (handle, _peer_ctx) = wire_migration(
            host(NODE),
            Arc::new(client_for(&witness.server, Some(NODE_TOKEN))),
            witness.server.addr,
            vmm,
            surface,
            Arc::new(FakeProvider::new()),
            peer,
            facts,
            snapshot_dir.path().to_owned(),
            dirs.path().join("migrations"),
            dirs.path().join("peer-preparations"),
            Arc::new(|| 0_u64),
        )
        .expect("wiring");

        // A PREPARED record exists (the stub peer verified the
        // participant set).
        let mig = migration("mig-retry");
        handle
            .coordinator()
            .prepare(PrepareHandoffRequest {
                migration_id: mig.clone(),
                vm_id: "vm-1".to_owned(),
                source_host: host(NODE),
                target_host: host(PEER),
                participants: vec![participant("vol-retry", 1, 100)],
            })
            .await
            .expect("prepare");
        assert_eq!(
            handle
                .coordinator()
                .observe(&mig)
                .expect("observe")
                .expect("record")
                .state,
            HandoffState::Prepared
        );

        // A live consumer drive holds the surface: the pass skips
        // itself, and the record is untouched.
        {
            let _guard = handle.drive_lock().lock().await;
            retry_pass(&handle).await;
        }
        assert_eq!(
            handle
                .coordinator()
                .observe(&mig)
                .expect("observe")
                .expect("record")
                .state,
            HandoffState::Prepared,
            "a skipped pass must not roll the record back"
        );

        // Without the lock, the same pass reconciles: the pre-cut
        // no-cut record is rolled back (the committed B1
        // AutoBeforeCut semantics this wiring inherits).
        retry_pass(&handle).await;
        let reconciled = handle
            .coordinator()
            .observe(&mig)
            .expect("observe")
            .expect("the record persists");
        assert!(
            matches!(reconciled.state, HandoffState::Aborted { .. }),
            "the reconcile rolls an un-transferred preparation back: {}",
            reconciled.state
        );
    }

    /// Plan §3.3's tracked registry, the spawn/finish race included:
    /// `record` runs after `tokio::spawn` returns, so every ordering
    /// of alloc/record/finish must leave the registry empty once the
    /// task is done — a dead handle is never tracked, and the abort
    /// path takes the live handles out wholesale.
    #[tokio::test]
    async fn the_drive_task_registry_survives_the_spawn_finish_race() {
        let mut registry = DriveTaskRegistry::default();

        // The graceful ordering: record first, the task's final act
        // second — the slot drops with the handle.
        let graceful = registry.alloc();
        registry.record(graceful, tokio::spawn(async {}));
        assert_eq!(registry.live(), 1, "the recorded task is live");
        registry.finish(graceful);
        assert_eq!(registry.live(), 0, "the final act removes the slot");

        // The fail-fast ordering: the task finishes before the spawner
        // records the handle — the record step drops the dead handle
        // instead of tracking it (the registry never grows).
        let fail_fast = registry.alloc();
        registry.finish(fail_fast);
        assert_eq!(registry.live(), 0);
        registry.record(fail_fast, tokio::spawn(async {}));
        assert_eq!(
            registry.live(),
            0,
            "a handle recorded after the task finished is never tracked"
        );

        // The abort ordering: a recorded handle is taken out wholesale
        // (an aborted task cannot run its final act).
        let aborted = registry.alloc();
        registry.record(aborted, tokio::spawn(async {}));
        let taken = registry.take_live();
        assert_eq!(taken.len(), 1, "the abort path drains the registry");
        assert_eq!(registry.live(), 0);
        for handle in taken {
            handle.abort();
            let _ = handle.await;
        }

        // The kill-inside-the-spawn-window ordering (P5 plan §3.3's
        // complete-group rule): the kill fires while the slot is
        // still Starting — the handle does not exist to take, so the
        // slot is marked AbortPending and the record step aborts the
        // drive the moment the handle arrives. The window cannot
        // leak a mutation engine past the kill.
        let window = registry.alloc();
        let taken = registry.take_live();
        assert!(taken.is_empty(), "a Starting slot has no handle to take");
        assert_eq!(registry.live(), 0, "nothing is live through the window");
        let aborted_at_record = registry.record(
            window,
            tokio::spawn(async { tokio::time::sleep(Duration::from_secs(3600)).await }),
        );
        assert!(
            aborted_at_record,
            "the record step aborts the handle of an AbortPending slot"
        );
        assert_eq!(
            registry.live(),
            0,
            "the AbortPending slot never becomes a tracked live task"
        );
    }

    /// Plan §3.3's registry over the real spawn path: a `transfer`
    /// drive is tracked (the supervisor's kill-group enumeration
    /// input), removes itself when it finishes, and is aborted and
    /// drained by [`MigrationHandle::abort_drive_tasks`] — and
    /// aborting the drive *task* is not aborting the *migration*.
    #[tokio::test]
    async fn transfer_drive_tasks_are_tracked_reaped_and_abortable() {
        // The full composition (the runtime's wiring over fakes): both
        // roles from one `wire_migration` call.
        let witness = witness_kit().await;
        let snapshot_dir = tempfile::tempdir().expect("snapshot dir");
        let dirs = tempfile::tempdir().expect("store dir");
        let vmm: Arc<dyn VmmController> = Arc::new(FakeVmm::new(snapshot_dir.path()));
        let surface: Arc<dyn HandoffSurface> = Arc::new(FakeSurface::new());
        let stub = Arc::new(StubPeer::new(Vec::new()));
        // The destination verifies exactly the participant set the
        // preparation names (same order, same generations).
        *stub.prepare_participants.lock().expect("participants") =
            vec![participant("vol-drive", 1, 100)];
        let peer: Arc<dyn PeerClient> = stub;
        let facts: ParticipantFacts =
            Arc::new(|_volume, _vm, _generation| Ok(("drbd-res".to_owned(), 100_u32)));
        // The volume is registered at the witness: the drive's
        // external-facts fold inspects every participant before it
        // cuts.
        register_volume(&witness, &volume("vol-drive")).await;
        let (handle, _peer_ctx) = wire_migration(
            host(NODE),
            Arc::new(client_for(&witness.server, Some(NODE_TOKEN))),
            witness.server.addr,
            vmm,
            surface,
            Arc::new(FakeProvider::new()),
            peer,
            facts,
            snapshot_dir.path().to_owned(),
            dirs.path().join("migrations"),
            dirs.path().join("peer-preparations"),
            Arc::new(|| 0_u64),
        )
        .expect("wiring");

        // Two PREPARED records: one whose drive runs to its outcome,
        // one whose drive is aborted while parked.
        let driven = migration("mig-drive-live");
        let parked = migration("mig-drive-abort");
        for mig in [&driven, &parked] {
            handle
                .coordinator()
                .prepare(PrepareHandoffRequest {
                    migration_id: mig.clone(),
                    vm_id: "vm-1".to_owned(),
                    source_host: host(NODE),
                    target_host: host(PEER),
                    participants: vec![participant("vol-drive", 1, 100)],
                })
                .await
                .expect("prepare");
        }

        // A parked drive is deterministically tracked: the test holds
        // the surface, so the spawned task cannot pass its first await
        // and cannot finish — after `transfer` returns, the recorded
        // handle is live.
        let guard = handle.drive_lock().lock().await;
        handle
            .transfer(&driven, serde_json::json!({"kind": "unit"}))
            .await
            .expect("transfer");
        assert_eq!(
            handle.live_drive_tasks(),
            1,
            "the parked drive task is tracked"
        );

        // Released, the drive runs to its outcome — over this kit the
        // stub peer grants nothing, so the drive fails fast and keeps
        // the record's durable state — and its final act removes it.
        drop(guard);
        let mut removed = false;
        for _ in 0..1_000 {
            if handle.live_drive_tasks() == 0 {
                removed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            removed,
            "the drive task's final act must remove its own slot"
        );

        // The kill act: a second parked drive is aborted and drained
        // by the registry (an aborted task cannot run its final act),
        // and the record keeps its PREPARED state — the task abort is
        // not a migration abort.
        let _guard = handle.drive_lock().lock().await;
        handle
            .transfer(&parked, serde_json::json!({"kind": "unit"}))
            .await
            .expect("transfer 2");
        assert_eq!(
            handle.live_drive_tasks(),
            1,
            "the second parked drive task is tracked"
        );
        handle.abort_drive_tasks().await;
        assert_eq!(
            handle.live_drive_tasks(),
            0,
            "the registry reaped the aborted task"
        );
        assert_eq!(
            handle
                .coordinator()
                .observe(&parked)
                .expect("observe")
                .expect("record")
                .state,
            HandoffState::Prepared,
            "aborting the drive task must not abort the migration"
        );
    }
}
