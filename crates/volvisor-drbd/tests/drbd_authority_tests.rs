//! Writer-authority behavior over the DRBD engine (P4a plan §8, the
//! "DRBD behavior" rows): every test drives the real provider code
//! through the real blocking witness boundary against a real loopback
//! witness server (real axum HTTP, real journal-backed registry in a
//! temp directory), with deterministic injected clocks on both sides.
//! The fake DRBD command surface (`tests/common`) provides the host
//! facts; the fence itself is the code under test, never faked.
//!
//! Covered rows:
//! - attach acquires the lease and persists the block **before** the
//!   promotion (a refused witness leaves the resource Secondary);
//! - typed refusals: `LEASE_HELD`, `FENCE_PENDING` inside the W7
//!   window, unregistered volume, unreachable witness;
//! - renewal passes: due leases renew, superseded epochs and passed
//!   W5 deadlines self-fence (suspend → demote → resume), unreachable
//!   witnesses defer until the deadline (the honest bound);
//! - detach releases the lease after the demotion;
//! - startup fail-closed: unproven primaries stay suspended; proven
//!   ones resume; a registered zombie without a block stays suspended;
//! - a pending fence completes when the device closes;
//! - the adopt-and-promote flow (plan §5): verification refusals
//!   (recreated lineage, live lease, unprovable disk, unregistered,
//!   already-held state), the `POSSIBLE_LOSS` authorization gate, the
//!   `SAFE_CURRENT` evidence row, and promotion under a fresh epoch
//!   after the fence window.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::{
    FakeDrbd, NODE, PEER_NODE, SEED_MINOR, SEED_PORT, Server, WitnessTokens, config_for,
    config_for_peer, fixture, flip_world_to_peer, provider_from_with_authority, seed_volume,
    seed_volume_with_identity, seed_volume_with_protocol, spawn_witness,
};
use volvisor_drbd::AuthorityContext;
use volvisor_drbd::provider::{DrbdProvider, resource_name_for};
use volvisor_drbd::report::Role;
use volvisor_drbd::state::{DrbdState, PendingFence, ReplicationMode, UnverifiableVolume};
use volvisor_drbd::{CommandOutput, CommandRunner, FakeRunner};
use volvisor_provider::VolumeProvider;
use volvisor_types::request::{
    AccessModeRequest, AttachVolumeRequest, DeleteVolumeRequest, DetachVolumeRequest, ErasurePolicy,
};
use volvisor_types::{
    ApiError, ApiErrorCode, AttachmentId, DrainProof, HostId, LeaseState, LossBoundary,
    OperationId, PromotionClassification, RecordedBarrier, VolumeId,
};
use volvisor_witness::BlockingWitness;
use volvisor_witness::client::{HttpWitnessConnection, WitnessConnection};
use volvisor_witness::proto::{GrantRequest, WITNESS_PROTOCOL_VERSION};

mod common;

/// One gibibyte (extent-aligned under the fixture's 4-MiB extents).
const GIB: u64 = 1 << 30;
/// The witness auth token both sides share (the legacy read-only
/// credential on a v2 witness: `kit.client` inspects with it).
const TOKEN: &str = "authority-test-token";
/// Per-host witness credentials (W8): each side's daemon mutates with
/// its own host's token.
const NODE_TOKEN: &str = "authority-test-node-a-token";
const PEER_TOKEN: &str = "authority-test-node-b-token";
/// Deterministic knobs: ttl 100s, grace 5s, budget 5s (the W7 wait
/// ends 10s past a lease's recorded end), clocks starting at t=1000.
const TTL: u64 = 100;
const START: u64 = 1_000;
/// The writer's renewal cadence in these tests (well under ttl/2).
const INTERVAL: u64 = 20;

// ---------------------------------------------------------------- kit
// The loopback witness server (the deterministic unreachability
// gate included) is shared with the handoff kit: `tests/common`.

/// The legacy shared-token client (read-only on a v2 witness: used for
/// witness-side inspection).
fn client_for(server: &Server) -> HttpWitnessConnection {
    HttpWitnessConnection::new(
        format!("http://{}", server.addr),
        Some(TOKEN.to_owned()),
        Duration::from_secs(5),
    )
}

/// The client presenting `host`'s W8 credential (the mutating surface).
fn host_client_for(server: &Server, host: &str) -> HttpWitnessConnection {
    let token = if host == NODE { NODE_TOKEN } else { PEER_TOKEN };
    HttpWitnessConnection::new(
        format!("http://{}", server.addr),
        Some(token.to_owned()),
        Duration::from_secs(5),
    )
}

/// The loopback witness plus both injected clocks and a direct client
/// for witness-side manipulation (peer grants, views).
struct WitnessKit {
    server: Server,
    client: HttpWitnessConnection,
    /// The witness's clock (lease expiry, fence windows).
    witness_clock: Arc<AtomicU64>,
    /// The writer's clock (W5 deadline anchoring).
    writer_clock: Arc<AtomicU64>,
    /// Keeps the journal directory alive for the server's lifetime.
    _dir: tempfile::TempDir,
}

async fn witness_kit() -> WitnessKit {
    let dir = tempfile::tempdir().expect("witness dir");
    let witness_clock = Arc::new(AtomicU64::new(START));
    let server = spawn_witness(
        dir.path(),
        Arc::clone(&witness_clock),
        WitnessTokens {
            shared: TOKEN,
            node: NODE_TOKEN,
            peer: PEER_TOKEN,
        },
    )
    .await;
    let client = client_for(&server);
    WitnessKit {
        server,
        client,
        witness_clock,
        writer_clock: Arc::new(AtomicU64::new(START)),
        _dir: dir,
    }
}

fn volume(volume_id: &str) -> VolumeId {
    VolumeId::new(volume_id).expect("valid volume id")
}

fn op(n: u64) -> OperationId {
    OperationId::new(format!("authority-op-{n}")).expect("valid operation id")
}

fn resource_of(volume_id: &str) -> String {
    resource_name_for(&volume(volume_id))
}

/// A witness-managed provider identity for `host` over the fixture's
/// state and world (the writer clock is shared with the kit).
fn authority_for(kit: &WitnessKit, host: &str, renewal_interval: u64) -> AuthorityContext {
    let connection: Arc<dyn volvisor_witness::BlockingWitnessConnection> =
        Arc::new(BlockingWitness::new(
            Arc::new(host_client_for(&kit.server, host)),
            tokio::runtime::Handle::current(),
            Duration::from_secs(5),
        ));
    let clock = Arc::clone(&kit.writer_clock);
    AuthorityContext::new(
        connection,
        HostId::new(host).expect("valid host id"),
        renewal_interval,
        Arc::new(move || clock.load(Ordering::SeqCst)),
    )
    .expect("authority context")
}

/// An [`AuthorityContext`] against an arbitrary witness URL — used to
/// point a provider at a deliberately broken witness.
fn authority_for_url(
    url: &str,
    host: &str,
    renewal_interval: u64,
    clock: Arc<AtomicU64>,
) -> AuthorityContext {
    let connection: Arc<dyn volvisor_witness::BlockingWitnessConnection> =
        Arc::new(BlockingWitness::new(
            Arc::new(HttpWitnessConnection::new(
                url.to_owned(),
                Some(TOKEN.to_owned()),
                Duration::from_secs(5),
            )),
            tokio::runtime::Handle::current(),
            Duration::from_secs(5),
        ));
    AuthorityContext::new(
        connection,
        HostId::new(host).expect("valid host id"),
        renewal_interval,
        Arc::new(move || clock.load(Ordering::SeqCst)),
    )
    .expect("authority context")
}

/// The primary host's witness-managed provider over a seeded volume.
fn authority_provider(
    kit: &WitnessKit,
    state_path: &Path,
    world: &Arc<Mutex<FakeDrbd>>,
) -> Arc<DrbdProvider> {
    provider_from_with_authority(state_path, world, authority_for(kit, NODE, INTERVAL))
}

/// The surviving (peer) host's provider: the peer-view configuration
/// over the same host directory and world, a fresh state file (it
/// holds none of the primary's volumes), and its own authority
/// identity. Call after [`flip_world_to_peer`].
fn peer_provider(kit: &WitnessKit, base: &Path, world: &Arc<Mutex<FakeDrbd>>) -> Arc<DrbdProvider> {
    DrbdProvider::with_authority(
        FakeDrbd::runner(world),
        config_for_peer(base),
        base.join("state-peer.json"),
        authority_for(kit, PEER_NODE, INTERVAL),
    )
    .map(Arc::new)
    .expect("peer provider construction")
}

/// A single-writer attach request.
fn attach_req(volume_id: &str, expected_generation: u64) -> AttachVolumeRequest {
    AttachVolumeRequest {
        api_version: "volvisor.volume.v2".to_owned(),
        operation_id: OperationId::new(format!("op-attach-{volume_id}")).expect("valid id"),
        vm_id: "authority-vm".to_owned(),
        host_id: HostId::new("authority-host").expect("valid id"),
        attachment_id: AttachmentId::new(format!("att-{volume_id}")).expect("valid id"),
        expected_volume_generation: expected_generation,
        access_mode: AccessModeRequest::SingleWriter,
        requested_frontend: None,
        vmm_disk_id: None,
    }
}

/// A drained detach request.
fn detach_req(attachment_id: &str, expected_generation: u64) -> DetachVolumeRequest {
    DetachVolumeRequest {
        api_version: "volvisor.volume.v2".to_owned(),
        operation_id: OperationId::new(format!("op-detach-{attachment_id}")).expect("valid id"),
        expected_attachment_generation: expected_generation,
        vm_stopped_or_io_drained_proof: DrainProof::VmStopped,
    }
}

/// The operator-attested barrier (plan §5: the only P4a
/// `SAFE_CURRENT` evidence source).
fn barrier() -> RecordedBarrier {
    RecordedBarrier {
        boundary: "boundary-1".to_owned(),
        attestation: "source committed, connection established at the boundary, no writes \
                      acknowledged past it"
            .to_owned(),
        recorded_at: START,
    }
}

/// Grant a lease to the PEER host directly at the witness (failover
/// setup: another writer holds or held authority). Presented with the
/// peer's own W8 credential.
async fn grant_to_peer(kit: &WitnessKit, volume_id: &VolumeId) {
    host_client_for(&kit.server, PEER_NODE)
        .grant(
            volume_id,
            GrantRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: op(0),
                host_id: HostId::new(PEER_NODE).expect("valid host id"),
            },
        )
        .await
        .expect("peer grant");
}

/// Seed, register and attach a volume on the primary host; returns the
/// fixture pieces for further assertions.
struct Attached {
    provider: Arc<DrbdProvider>,
    world: Arc<Mutex<FakeDrbd>>,
    state_path: std::path::PathBuf,
    resource: String,
    volume: VolumeId,
}

async fn attached(kit: &WitnessKit, volume_id: &str) -> Attached {
    let f = fixture();
    seed_volume(&f.base, &f.world, volume_id, GIB);
    let provider = authority_provider(kit, &f.state_path, &f.world);
    let vol = volume(volume_id);
    provider.register_volume(&vol, None).expect("register");
    provider
        .attach_volume(&vol, &attach_req(volume_id, 1))
        .await
        .expect("attach");
    Attached {
        provider,
        world: f.world,
        state_path: f.state_path,
        resource: resource_of(volume_id),
        volume: vol,
    }
}

fn role_of(world: &Arc<Mutex<FakeDrbd>>, resource: &str) -> Role {
    world
        .lock()
        .expect("world")
        .resources
        .get(resource)
        .expect("resource exists")
        .role
}

fn suspended(world: &Arc<Mutex<FakeDrbd>>, minor: u32) -> bool {
    world
        .lock()
        .expect("world")
        .suspended_minors
        .contains(&minor)
}

// ------------------------------------------------------------ attach

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_acquires_the_lease_and_persists_the_block() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-auth").await;
    // The promotion happened under the lease.
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
    // The witness holds a live lease for this host at a fresh epoch.
    let view = kit.client.inspect(&state.volume).await.expect("view");
    assert_eq!(view.lease_state, LeaseState::Live);
    assert_eq!(view.holder.as_ref().expect("holder").as_str(), NODE);
    assert!(view.current_epoch.0 > 0);
    // The inspect summary is the writer-side observation: live, with
    // the full TTL remaining against the W5 deadline.
    let inspect = state
        .provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    let summary = inspect.authority.expect("authority summary");
    assert_eq!(summary.lease_state, LeaseState::Live);
    assert_eq!(summary.lease_remaining_secs, Some(TTL));
    assert_eq!(summary.epoch, view.current_epoch);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_refused_by_a_live_lease_leaves_the_resource_secondary() {
    let kit = witness_kit().await;
    let f = fixture();
    seed_volume(&f.base, &f.world, "vol-held", GIB);
    let provider = authority_provider(&kit, &f.state_path, &f.world);
    let vol = volume("vol-held");
    provider.register_volume(&vol, None).expect("register");
    grant_to_peer(&kit, &vol).await;
    let error = provider
        .attach_volume(&vol, &attach_req("vol-held", 1))
        .await
        .expect_err("a live peer lease refuses the attach");
    assert_eq!(error.code, ApiErrorCode::LeaseHeld);
    // Fail-closed: no promotion, no persisted authority block.
    assert_eq!(role_of(&f.world, &resource_of("vol-held")), Role::Secondary);
    let inspect = provider.inspect_volume(&vol).await.expect("inspect");
    assert!(inspect.authority.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_inside_the_fence_wait_window_is_typed_fence_pending() {
    let kit = witness_kit().await;
    let f = fixture();
    seed_volume(&f.base, &f.world, "vol-window", GIB);
    let provider = authority_provider(&kit, &f.state_path, &f.world);
    let vol = volume("vol-window");
    provider.register_volume(&vol, None).expect("register");
    grant_to_peer(&kit, &vol).await;
    // The peer lease ended at START+TTL; the W7 wait runs to
    // end+grace+budget. Inside it, a grant is FENCE_PENDING.
    kit.witness_clock.store(START + TTL + 5, Ordering::SeqCst);
    let error = provider
        .attach_volume(&vol, &attach_req("vol-window", 1))
        .await
        .expect_err("inside the fence wait window");
    assert_eq!(error.code, ApiErrorCode::FencePending);
    assert_eq!(
        role_of(&f.world, &resource_of("vol-window")),
        Role::Secondary
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_without_registration_is_refused_typed() {
    let kit = witness_kit().await;
    let f = fixture();
    seed_volume(&f.base, &f.world, "vol-p3", GIB);
    let provider = authority_provider(&kit, &f.state_path, &f.world);
    let vol = volume("vol-p3");
    let error = provider
        .attach_volume(&vol, &attach_req("vol-p3", 1))
        .await
        .expect_err("unregistered volume");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert!(error.detail.contains("not registered"));
    assert_eq!(role_of(&f.world, &resource_of("vol-p3")), Role::Secondary);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_witness_refuses_attach_typed() {
    let mut kit = witness_kit().await;
    let f = fixture();
    seed_volume(&f.base, &f.world, "vol-down", GIB);
    let provider = authority_provider(&kit, &f.state_path, &f.world);
    let vol = volume("vol-down");
    provider.register_volume(&vol, None).expect("register");
    kit.server
        .stop()
        .await
        .expect("the witness drain completes");
    let error = provider
        .attach_volume(&vol, &attach_req("vol-down", 1))
        .await
        .expect_err("unreachable witness");
    assert_eq!(error.code, ApiErrorCode::UnknownFencingAuthority);
    assert_eq!(role_of(&f.world, &resource_of("vol-down")), Role::Secondary);
}

// ---------------------------------------------------------- renewal

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renewal_passes_renew_due_leases_only() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-renew").await;
    // Not due yet: the interval has not elapsed.
    let early = state.provider.renew_leases().expect("renewal pass");
    assert_eq!(early.renewed, Vec::<volvisor_types::VolumeId>::new());
    // Advance the writer clock past the interval: due.
    kit.writer_clock
        .store(START + INTERVAL + 1, Ordering::SeqCst);
    let report = state.provider.renew_leases().expect("renewal pass");
    assert_eq!(report.renewed, vec![state.volume.clone()]);
    assert_eq!(
        report.fenced,
        Vec::<volvisor_drbd::state::FencedVolume>::new()
    );
    assert_eq!(
        report.deferred,
        Vec::<volvisor_drbd::state::DeferredRenewal>::new()
    );
    // The witness still shows our live lease.
    let view = kit.client.inspect(&state.volume).await.expect("view");
    assert_eq!(view.lease_state, LeaseState::Live);
    assert_eq!(view.holder.as_ref().expect("holder").as_str(), NODE);
    // The W5 deadline moved with the renewal response.
    let inspect = state
        .provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    let summary = inspect.authority.expect("authority summary");
    assert_eq!(summary.lease_state, LeaseState::Live);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_superseded_epoch_self_fences() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-superseded").await;
    // Our lease lapses and the W7 window passes at the witness; the
    // peer then takes the epoch (the witness retired ours — W4).
    kit.witness_clock
        .store(START + TTL + 5 + 5 + 1, Ordering::SeqCst);
    grant_to_peer(&kit, &state.volume).await;
    kit.writer_clock
        .store(START + INTERVAL + 1, Ordering::SeqCst);
    let report = state.provider.renew_leases().expect("renewal pass");
    assert_eq!(report.renewed, Vec::<volvisor_types::VolumeId>::new());
    assert_eq!(report.fenced.len(), 1);
    assert!(
        report.fenced[0].demoted,
        "the device was not open: demotion completes"
    );
    // The writer is gone: Secondary, unsuspended, block cleared, Ready.
    assert_eq!(role_of(&state.world, &state.resource), Role::Secondary);
    assert!(!suspended(&state.world, SEED_MINOR));
    let inspect = state
        .provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    assert!(inspect.authority.is_none());
    assert_eq!(inspect.state, volvisor_types::VolumeLifecycle::Ready);
    // The witness shows the new holder.
    let view = kit.client.inspect(&state.volume).await.expect("view");
    assert_eq!(view.holder.as_ref().expect("holder").as_str(), PEER_NODE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_passed_deadline_self_fences_even_with_the_witness_down() {
    let mut kit = witness_kit().await;
    let state = attached(&kit, "vol-deadline").await;
    kit.server
        .stop()
        .await
        .expect("the witness drain completes");
    // Past the W5 deadline: the writer fences itself without asking.
    kit.writer_clock.store(START + TTL + 1, Ordering::SeqCst);
    let report = state.provider.renew_leases().expect("renewal pass");
    assert_eq!(report.fenced.len(), 1);
    assert!(report.fenced[0].demoted);
    assert_eq!(role_of(&state.world, &state.resource), Role::Secondary);
    assert!(!suspended(&state.world, SEED_MINOR));
    let inspect = state
        .provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    assert!(inspect.authority.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_witness_defers_renewal_until_the_deadline() {
    let mut kit = witness_kit().await;
    let state = attached(&kit, "vol-defer").await;
    kit.server
        .stop()
        .await
        .expect("the witness drain completes");
    // Renewal is due, the witness is down, but the W5 deadline has not
    // passed: keep serving (the deferred entry carries the bound).
    kit.writer_clock
        .store(START + INTERVAL + 1, Ordering::SeqCst);
    let report = state.provider.renew_leases().expect("renewal pass");
    assert_eq!(report.renewed, Vec::<volvisor_types::VolumeId>::new());
    assert_eq!(
        report.fenced,
        Vec::<volvisor_drbd::state::FencedVolume>::new()
    );
    assert_eq!(report.deferred.len(), 1);
    assert_eq!(report.deferred[0].deadline_at, START + TTL);
    assert_eq!(report.deferred[0].volume_id, state.volume);
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
    let inspect = state
        .provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    assert!(inspect.authority.is_some(), "the block survives a deferral");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_witness_is_deterministically_unreachable() {
    // The unreachability gate's own contract, pinned: after
    // `Server::stop` returns, NO request can complete against the
    // witness — through the provider's error mapping and over the
    // client's pooled keep-alive connection at the transport level
    // alike. The drain proof makes that a deterministic invariant,
    // not an assumption: the graceful-shutdown await returns only
    // after the listener is dropped and every already-accepted
    // connection task has exited, so no serviceable connection
    // remains. (The gate exists because the abort shape it replaced
    // closed lingering connections only asynchronously — the
    // recorded renewal-deadline flake; see `Server::stop`'s docs.)
    let mut kit = witness_kit().await;
    let state = attached(&kit, "vol-gate").await;
    kit.server
        .stop()
        .await
        .expect("the witness drain completes");
    // The provider path: a due renewal defers (the local W5 deadline
    // is the bound), never renews, never fences.
    kit.writer_clock
        .store(START + INTERVAL + 1, Ordering::SeqCst);
    let report = state.provider.renew_leases().expect("renewal pass");
    assert_eq!(report.renewed, Vec::<volvisor_types::VolumeId>::new());
    assert_eq!(
        report.fenced,
        Vec::<volvisor_drbd::state::FencedVolume>::new()
    );
    assert_eq!(report.deferred.len(), 1);

    // The client path: a direct request over the very connection the
    // attach pooled cannot complete either — the gate holds at the
    // transport level, not just through the provider's error mapping.
    let error = kit
        .client
        .inspect(&state.volume)
        .await
        .expect_err("a stopped witness answers nothing");
    assert!(
        matches!(error, volvisor_witness::proto::WitnessError::Unreachable(_)),
        "the direct request is a transport unreachability: {error:?}"
    );
}

// ----------------------------------------------------------- detach

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_demotes_then_releases_the_lease() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-detach").await;
    state
        .provider
        .detach_volume(
            &state.volume,
            &AttachmentId::new("att-vol-detach").expect("valid id"),
            &detach_req("att-vol-detach", 1),
        )
        .await
        .expect("detach");
    assert_eq!(role_of(&state.world, &state.resource), Role::Secondary);
    let inspect = state
        .provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    assert!(inspect.authority.is_none());
    assert_eq!(inspect.state, volvisor_types::VolumeLifecycle::Ready);
    // The witness no longer shows a live lease (the demoted holder
    // released it; no W7 wait was needed).
    let view = kit.client.inspect(&state.volume).await.expect("view");
    assert_ne!(view.lease_state, LeaseState::Live);
}

// --------------------------------------------------------- startup

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_validation_resumes_a_proven_primary() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-restart").await;
    // A daemon restart over the same on-disk state: every Primary is
    // unproven until validated — suspend first, then prove, then
    // resume.
    let provider = authority_provider(&kit, &state.state_path, &state.world);
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
    assert!(
        !suspended(&state.world, SEED_MINOR),
        "a proven primary resumes"
    );
    let report = provider
        .last_reconcile_report()
        .expect("report lock")
        .expect("startup report");
    assert_eq!(
        report.unvalidated_primaries,
        Vec::<volvisor_drbd::state::UnverifiableVolume>::new()
    );
    assert_eq!(
        report.zombie_primaries,
        Vec::<volvisor_types::VolumeId>::new()
    );
    // The attachment record survived: still Attached.
    let inspect = provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    assert_eq!(inspect.state, volvisor_types::VolumeLifecycle::Attached);
    assert!(inspect.authority.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_with_the_witness_down_stays_suspended_and_failed() {
    let mut kit = witness_kit().await;
    let state = attached(&kit, "vol-stalled").await;
    kit.server
        .stop()
        .await
        .expect("the witness drain completes");
    let provider = authority_provider(&kit, &state.state_path, &state.world);
    // Fail-closed: the unproven writer stays frozen, never resumed.
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
    assert!(suspended(&state.world, SEED_MINOR));
    let report = provider
        .last_reconcile_report()
        .expect("report lock")
        .expect("startup report");
    assert_eq!(report.unvalidated_primaries.len(), 1);
    let inspect = provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    assert_eq!(inspect.state, volvisor_types::VolumeLifecycle::Failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_primary_without_a_block_on_a_registered_volume_stays_suspended() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-noblock").await;
    // A rolled-back state file: the resource is Primary, the witness
    // is registered, but this host holds no lease block — an unproven
    // writer that can neither renew nor be validated.
    let mut disk = DrbdState::load(&state.state_path).expect("load state");
    disk.volume_mut(&state.volume)
        .expect("volume")
        .runtime
        .authority = None;
    disk.save(&state.state_path).expect("save state");
    let provider = authority_provider(&kit, &state.state_path, &state.world);
    assert!(suspended(&state.world, SEED_MINOR));
    let report = provider
        .last_reconcile_report()
        .expect("report lock")
        .expect("startup report");
    assert_eq!(report.zombie_primaries, vec![state.volume.clone()]);
    assert_eq!(report.unvalidated_primaries.len(), 1);
    let inspect = provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    assert_eq!(inspect.state, volvisor_types::VolumeLifecycle::Failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_fence_completes_when_the_device_closes() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-open").await;
    // The VM holds the device open: the fence suspends I/O but the
    // kernel refuses the demotion — the marker stays.
    state
        .world
        .lock()
        .expect("world")
        .open_devices
        .insert(SEED_MINOR);
    // Our lease lapses and the W7 window passes; the peer takes the
    // epoch while our (suspended) device is still open.
    kit.witness_clock
        .store(START + TTL + 5 + 5 + 1, Ordering::SeqCst);
    grant_to_peer(&kit, &state.volume).await;
    kit.writer_clock
        .store(START + INTERVAL + 1, Ordering::SeqCst);
    let report = state.provider.renew_leases().expect("renewal pass");
    assert_eq!(report.fenced.len(), 1);
    assert!(
        !report.fenced[0].demoted,
        "the open device refuses demotion"
    );
    assert!(suspended(&state.world, SEED_MINOR));
    let disk = DrbdState::load(&state.state_path).expect("load state");
    assert!(
        disk.volume(&state.volume)
            .expect("volume")
            .runtime
            .fence
            .is_some(),
        "the pending-fence marker is durable"
    );
    assert_eq!(
        disk.volume(&state.volume).expect("volume").runtime.state,
        volvisor_types::VolumeLifecycle::Failed
    );
    // The device closes (the VM died): reconcile completes the fence.
    state
        .world
        .lock()
        .expect("world")
        .open_devices
        .remove(&SEED_MINOR);
    let reconciled = state.provider.reconcile().expect("reconcile");
    assert_eq!(reconciled.completed_fences, vec![state.volume.clone()]);
    assert_eq!(role_of(&state.world, &state.resource), Role::Secondary);
    assert!(!suspended(&state.world, SEED_MINOR));
    let inspect = state
        .provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    assert_eq!(inspect.state, volvisor_types::VolumeLifecycle::Ready);
}

// ------------------------------------------------------------ adopt

/// The adopt fixture: a seeded volume registered by the (still alive
/// or dead) primary host, and the flipped peer-view world.
struct AdoptFixture {
    peer: Arc<DrbdProvider>,
    world: Arc<Mutex<FakeDrbd>>,
    resource: String,
    volume: VolumeId,
}

fn adopt_fixture(
    kit: &WitnessKit,
    volume_id: &str,
    protocol: ReplicationMode,
    barrier: Option<RecordedBarrier>,
) -> AdoptFixture {
    let f = fixture();
    seed_volume_with_protocol(&f.base, &f.world, volume_id, GIB, protocol);
    let primary = authority_provider(kit, &f.state_path, &f.world);
    let vol = volume(volume_id);
    primary.register_volume(&vol, barrier).expect("register");
    // The primary dies: the surviving host boots over the same host
    // directory with a state file of its own.
    flip_world_to_peer(&f.world);
    let peer = peer_provider(kit, &f.base, &f.world);
    AdoptFixture {
        peer,
        world: f.world,
        resource: resource_of(volume_id),
        volume: vol,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_refuses_an_unregistered_volume() {
    let kit = witness_kit().await;
    let f = fixture();
    seed_volume(&f.base, &f.world, "vol-foreign", GIB);
    let vol = volume("vol-foreign");
    flip_world_to_peer(&f.world);
    let peer = peer_provider(&kit, &f.base, &f.world);
    let error = peer
        .adopt_and_promote(&vol, false)
        .expect_err("unregistered volume");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert!(error.detail.contains("not registered"));
    assert_eq!(
        role_of(&f.world, &resource_of("vol-foreign")),
        Role::Secondary
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_refuses_while_the_old_lease_is_live() {
    let kit = witness_kit().await;
    let state = adopt_fixture(&kit, "vol-livelease", ReplicationMode::A, None);
    // The dead primary's lease is still live at the witness: the old
    // authority may still write — never a promotion, never a demotion.
    grant_to_peer(&kit, &state.volume).await;
    let response = state
        .peer
        .adopt_and_promote(&state.volume, false)
        .expect("classified");
    assert!(matches!(
        response.classification,
        PromotionClassification::Unsafe { .. }
    ));
    if let PromotionClassification::Unsafe { reasons } = response.classification {
        assert!(reasons.iter().any(|reason| reason.contains("live lease")));
    }
    assert!(response.volume.is_none());
    assert_eq!(role_of(&state.world, &state.resource), Role::Secondary);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_requires_explicit_loss_authorization() {
    let kit = witness_kit().await;
    let state = adopt_fixture(&kit, "vol-lossgate", ReplicationMode::A, None);
    // Protocol A without a barrier: the acknowledged tail cannot be
    // proved present — POSSIBLE_LOSS, and the promotion waits for the
    // operator's explicit authorization.
    let response = state
        .peer
        .adopt_and_promote(&state.volume, false)
        .expect("classified");
    assert!(matches!(
        response.classification,
        PromotionClassification::PossibleLoss {
            boundary: LossBoundary::Unknown,
            authorized: false
        }
    ));
    assert!(response.volume.is_none());
    assert_eq!(role_of(&state.world, &state.resource), Role::Secondary);
    // The same facts with the explicit authorization promote.
    let response = state
        .peer
        .adopt_and_promote(&state.volume, true)
        .expect("classified");
    assert!(matches!(
        response.classification,
        PromotionClassification::PossibleLoss {
            authorized: true,
            ..
        }
    ));
    let adopted = response.volume.expect("adopted");
    assert_eq!(adopted.state, volvisor_types::VolumeLifecycle::Ready);
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
    // The adopted entry is servable: a normal attach takes the
    // renewal path over the adopted lease.
    state
        .peer
        .attach_volume(&state.volume, &attach_req("vol-lossgate", 1))
        .await
        .expect("attach after adopt");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_promotes_safe_current_with_a_recorded_barrier() {
    let kit = witness_kit().await;
    let state = adopt_fixture(&kit, "vol-barrier", ReplicationMode::C, Some(barrier()));
    // Protocol C with a recorded operator barrier: the only P4a
    // SAFE_CURRENT evidence row — promoted without loss authorization.
    let response = state
        .peer
        .adopt_and_promote(&state.volume, false)
        .expect("classified");
    assert_eq!(
        response.classification,
        PromotionClassification::SafeCurrent
    );
    let adopted = response.volume.expect("adopted");
    assert_eq!(adopted.state, volvisor_types::VolumeLifecycle::Ready);
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
    // The survivor now holds the live lease.
    let view = kit.client.inspect(&state.volume).await.expect("view");
    assert_eq!(view.lease_state, LeaseState::Live);
    assert_eq!(view.holder.as_ref().expect("holder").as_str(), PEER_NODE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_refuses_a_recreated_lineage() {
    let kit = witness_kit().await;
    let state = adopt_fixture(&kit, "vol-recreated", ReplicationMode::A, None);
    // The resource on this host was rebuilt after a cluster loss: its
    // live data-generation identities are NOT the registered lineage.
    let generation = {
        let mut world = state.world.lock().expect("world");
        let generation = world.lineage_salt + 1;
        world.lineage.insert(
            state.resource.clone(),
            common::GiSet::for_resource_generation(&state.resource, generation),
        );
        generation
    };
    assert!(generation > 0);
    let error = state
        .peer
        .adopt_and_promote(&state.volume, true)
        .expect_err("a recreated lineage is refused even with loss authorization");
    assert_eq!(error.code, ApiErrorCode::ForeignDeviceState);
    assert!(error.detail.contains("registered lineage"));
    assert_eq!(role_of(&state.world, &state.resource), Role::Secondary);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_refuses_an_unprovable_disk() {
    let kit = witness_kit().await;
    let state = adopt_fixture(&kit, "vol-inconsistent", ReplicationMode::A, None);
    state
        .world
        .lock()
        .expect("world")
        .resources
        .get_mut(&state.resource)
        .expect("resource")
        .local_disk = volvisor_drbd::report::DiskState::Inconsistent;
    let response = state
        .peer
        .adopt_and_promote(&state.volume, true)
        .expect("classified");
    assert!(matches!(
        response.classification,
        PromotionClassification::Unsafe { .. }
    ));
    if let PromotionClassification::Unsafe { reasons } = response.classification {
        assert!(reasons.iter().any(|reason| reason.contains("Inconsistent")));
    }
    assert!(response.volume.is_none());
    assert_eq!(role_of(&state.world, &state.resource), Role::Secondary);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_after_the_fence_window_grants_a_fresh_epoch() {
    let kit = witness_kit().await;
    let state = adopt_fixture(&kit, "vol-window", ReplicationMode::A, None);
    // The dead primary's lease lapsed and the W7 fence window passed:
    // the witness itself proves the old authority retired.
    grant_to_peer(&kit, &state.volume).await;
    kit.witness_clock
        .store(START + TTL + 5 + 5 + 1, Ordering::SeqCst);
    let response = state
        .peer
        .adopt_and_promote(&state.volume, true)
        .expect("classified");
    assert!(matches!(
        response.classification,
        PromotionClassification::PossibleLoss {
            authorized: true,
            ..
        }
    ));
    assert!(response.volume.is_some());
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_refuses_a_volume_the_host_already_holds() {
    let kit = witness_kit().await;
    let state = adopt_fixture(&kit, "vol-twice", ReplicationMode::C, Some(barrier()));
    state
        .peer
        .adopt_and_promote(&state.volume, false)
        .expect("first adopt promotes");
    let error = state
        .peer
        .adopt_and_promote(&state.volume, false)
        .expect_err("a held volume is not adoptable");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert!(error.detail.contains("already exists"));
}

// -------------------------------------------------------- register

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_is_content_idempotent_and_refuses_divergence() {
    let kit = witness_kit().await;
    let f = fixture();
    seed_volume(&f.base, &f.world, "vol-reg", GIB);
    let provider = authority_provider(&kit, &f.state_path, &f.world);
    let vol = volume("vol-reg");
    provider
        .register_volume(&vol, None)
        .expect("first register");
    // Identical content: idempotent replay.
    provider
        .register_volume(&vol, None)
        .expect("identical re-register replays");
    // Diverging content (a recreated lineage under the same name):
    // refused typed, never overwritten.
    let resource = resource_of("vol-reg");
    let mut world = f.world.lock().expect("world");
    let generation = world.lineage_salt + 1;
    world.lineage.insert(
        resource.clone(),
        common::GiSet::for_resource_generation(&resource, generation),
    );
    drop(world);
    let error = provider
        .register_volume(&vol, None)
        .expect_err("diverging re-register is refused");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
}

// ------------------------------------------------- matrix completions

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_suspends_a_live_but_nearly_expired_lease() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-margin").await;
    // The lease is still live at the witness but its remaining duration
    // is under the renewal margin: a resumed writer would hold no
    // W5-conformant deadline until its first renewal — the resume gate
    // refuses exactly that.
    kit.witness_clock
        .store(START + TTL - (INTERVAL - 5), Ordering::SeqCst);
    let provider = authority_provider(&kit, &state.state_path, &state.world);
    // The fence completes on the spot here (no device is open in the
    // fixture): demoted, resumed, and recorded — never left serving.
    assert_eq!(
        role_of(&state.world, &state.resource),
        Role::Secondary,
        "a nearly-expired lease is not a safe resume"
    );
    assert!(
        !suspended(&state.world, SEED_MINOR),
        "the fence resumed after the clean demotion"
    );
    let report = provider
        .last_reconcile_report()
        .expect("report lock")
        .expect("startup report");
    assert_eq!(report.fenced_volumes.len(), 1);
    let inspect = provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    // The fence completed cleanly: the attachment is gone and the
    // volume is Ready (reattachable under a fresh lease), not Failed.
    assert_eq!(inspect.state, volvisor_types::VolumeLifecycle::Ready);
    assert_eq!(
        inspect.attachment_ids,
        Vec::<volvisor_types::AttachmentId>::new()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reattach_after_detach_is_not_fence_pending_blocked() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-reattach").await;
    state
        .provider
        .detach_volume(
            &state.volume,
            &AttachmentId::new("att-vol-reattach").expect("valid id"),
            &detach_req("att-vol-reattach", 1),
        )
        .await
        .expect("detach");
    // W7's self-release exclusion: the holder that demoted and released
    // itself waits no fence window on the next grant.
    state
        .provider
        .attach_volume(&state.volume, &attach_req("vol-reattach", 3))
        .await
        .expect("reattach after a clean release");
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_inside_the_fence_window_is_typed_then_succeeds_after_it() {
    let kit = witness_kit().await;
    let state = adopt_fixture(&kit, "vol-adoptwindow", ReplicationMode::A, None);
    // The dead primary's lease lapsed but the W7 window has not passed:
    // the adopt caller sees the typed wait, never a promotion.
    grant_to_peer(&kit, &state.volume).await;
    kit.witness_clock.store(START + TTL + 5, Ordering::SeqCst);
    let error = state
        .peer
        .adopt_and_promote(&state.volume, true)
        .expect_err("inside the fence window");
    assert_eq!(error.code, ApiErrorCode::FencePending);
    assert_eq!(role_of(&state.world, &state.resource), Role::Secondary);
    // The window passes: the same request succeeds.
    kit.witness_clock
        .store(START + TTL + 5 + 5 + 1, Ordering::SeqCst);
    let response = state
        .peer
        .adopt_and_promote(&state.volume, true)
        .expect("adopt after the window");
    assert!(response.volume.is_some());
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
}

/// The volvisor-created branch of the ownership check: the ORIGINAL
/// host, having lost its state file, re-adopts its own volume — its
/// endpoint is registered as volvisor-created, so the backing LV must
/// carry the matching `volvisor.owner` tag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_of_own_volvisor_created_backing_requires_the_ownership_tag() {
    let kit = witness_kit().await;
    let f = fixture();
    seed_volume(&f.base, &f.world, "vol-own", GIB);
    let primary = authority_provider(&kit, &f.state_path, &f.world);
    let vol = volume("vol-own");
    primary.register_volume(&vol, None).expect("register");
    // The host lost its state file (and nothing else): a fresh state
    // over the same world and authority identity.
    let lost = f.base.join("state-lost.json");
    let survivor = DrbdProvider::with_authority(
        FakeDrbd::runner(&f.world),
        common::config_for(&f.base),
        lost,
        authority_for(&kit, NODE, INTERVAL),
    )
    .map(Arc::new)
    .expect("fresh provider");
    let response = survivor
        .adopt_and_promote(&vol, true)
        .expect("the tagged own backing adopts");
    assert!(
        response.volume.is_some(),
        "protocol A + allow_loss promotes"
    );
    assert_eq!(role_of(&f.world, &resource_of("vol-own")), Role::Primary);
}

/// The same flow with the ownership tag stripped from the LV: the
/// volvisor-created branch refuses — foreign backing is never adopted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_refuses_a_stripped_ownership_tag_on_own_backing() {
    let kit = witness_kit().await;
    let f = fixture();
    seed_volume(&f.base, &f.world, "vol-stripped", GIB);
    let primary = authority_provider(&kit, &f.state_path, &f.world);
    let vol = volume("vol-stripped");
    primary.register_volume(&vol, None).expect("register");
    // Strip the volvisor.owner tag from the backing LV (the
    // reconstructed-disk case): only the lineage match remains, and it
    // does not carry the ownership proof this branch requires.
    {
        let mut world = f.world.lock().expect("world");
        let lv = world
            .lvs
            .get_mut(&format!("{}/{}", common::VG, resource_of("vol-stripped")))
            .expect("backing lv");
        lv.tags.retain(|tag| !tag.starts_with("volvisor.owner="));
    }
    let lost = f.base.join("state-lost.json");
    let survivor = DrbdProvider::with_authority(
        FakeDrbd::runner(&f.world),
        common::config_for(&f.base),
        lost,
        authority_for(&kit, NODE, INTERVAL),
    )
    .map(Arc::new)
    .expect("fresh provider");
    let error = survivor
        .adopt_and_promote(&vol, true)
        .expect_err("untagged own backing is foreign");
    assert_eq!(error.code, ApiErrorCode::ForeignDeviceState);
    assert!(error.detail.contains("volvisor.owner"));
    assert_eq!(
        role_of(&f.world, &resource_of("vol-stripped")),
        Role::Secondary
    );
}

// --------------------------------------------- review-round-1 additions

/// The adoption record is durable BEFORE the promotion (the same
/// crash-window discipline as attach): a failed `primary --force`
/// after the grant leaves a TRACKED `Failed` volume — never an
/// untracked Primary holding a live lease outside every fence path —
/// and the just-granted lease is released, not stranded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_promotion_after_the_grant_unwinds_tracked_and_released() {
    let kit = witness_kit().await;
    let state = adopt_fixture(&kit, "vol-unwind", ReplicationMode::A, None);
    {
        let mut world = state.world.lock().expect("world");
        world.fail_primary = true;
    }
    let error = state
        .peer
        .adopt_and_promote(&state.volume, true)
        .expect_err("the promotion fails");
    assert_eq!(error.code, ApiErrorCode::Internal);
    assert_eq!(role_of(&state.world, &state.resource), Role::Secondary);
    // The record is durable and tracked as Failed.
    let inspect = state.peer.inspect_volume(&state.volume).await;
    assert!(
        inspect.is_ok(),
        "the adopted volume must stay tracked: {inspect:?}"
    );
    if let Ok(inspect) = inspect {
        assert_eq!(inspect.state, volvisor_types::VolumeLifecycle::Failed);
    }
    // The just-granted lease was released (not stranded until lapse):
    // the witness shows it revoked by the holder's own unwind.
    let view = kit.client.inspect(&state.volume).await.expect("view");
    assert_eq!(view.lease_state, LeaseState::Revoked);
    // A retry after the fault clears is the typed already-exists
    // refusal: the tracked Failed entry is resolved through volume
    // management, never by a silent second adoption.
    {
        let mut world = state.world.lock().expect("world");
        world.fail_primary = false;
    }
    let error = state
        .peer
        .adopt_and_promote(&state.volume, true)
        .expect_err("already tracked");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
}

/// Outside `SAFE_CURRENT` the loss boundary is always `Unknown` (the
/// plan §5 table): a recorded barrier gates the safe row, it never
/// names a provable boundary for a volume that kept serving past it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_reports_an_unknown_boundary_outside_safe_current() {
    let kit = witness_kit().await;
    // Protocol A with a recorded barrier: the previously over-claiming
    // row — the boundary must still be reported unknown.
    let state = adopt_fixture(&kit, "vol-boundary", ReplicationMode::A, Some(barrier()));
    let response = state
        .peer
        .adopt_and_promote(&state.volume, false)
        .expect("classified");
    assert!(matches!(
        response.classification,
        PromotionClassification::PossibleLoss {
            boundary: LossBoundary::Unknown,
            authorized: false,
        }
    ));
    assert!(response.volume.is_none());
}

/// A crash mid-self-fence (after the suspend and demote, before the
/// completion save) leaves exactly the durable marker the fence
/// writes BEFORE demoting — and the reconciler completes it: resumed,
/// marker cleared, `Ready`. Never a silently frozen Secondary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_mid_fence_is_completed_by_the_reconciler() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-midfence").await;
    // Simulate the crash point: the device suspended, the resource
    // demoted, the durable state exactly the pre-demotion marker.
    {
        let mut world = state.world.lock().expect("world");
        let resource = world.resources.get_mut(&state.resource).expect("resource");
        resource.role = Role::Secondary;
        world.suspended_minors.insert(SEED_MINOR);
    }
    {
        let mut disk = DrbdState::load(&state.state_path).expect("load state");
        let volume = disk.volume_mut(&state.volume).expect("volume");
        volume.runtime.attachment = None;
        volume.runtime.authority = None;
        volume.runtime.fence = Some(PendingFence {
            reason: "writer authority lost".to_owned(),
            fenced_at: START,
        });
        volume.runtime.state = volvisor_types::VolumeLifecycle::Failed;
        disk.save(&state.state_path).expect("save state");
    }
    let provider = authority_provider(&kit, &state.state_path, &state.world);
    let report = provider
        .last_reconcile_report()
        .expect("report lock")
        .expect("startup report");
    assert_eq!(report.completed_fences, vec![state.volume.clone()]);
    assert!(
        !suspended(&state.world, SEED_MINOR),
        "the fence completion lifts the suspension"
    );
    let inspect = provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    assert_eq!(inspect.state, volvisor_types::VolumeLifecycle::Ready);
}

// --------------------------------------------- review-round-2 additions

/// A runner wrapper for fault injection: `drbdsetup status` for the
/// given resource FAILS TO EXECUTE once the resource is actually
/// Primary — the post-promotion verification path cannot observe the
/// role. Everything else forwards verbatim to the real fake surface.
struct StatusBlindWhenPrimary {
    inner: Arc<FakeRunner>,
    world: Arc<Mutex<FakeDrbd>>,
    resource: String,
}

impl CommandRunner for StatusBlindWhenPrimary {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ApiError> {
        if program == "drbdsetup"
            && args.first().copied() == Some("status")
            && args.get(1).copied() == Some(self.resource.as_str())
        {
            let primary = self
                .world
                .lock()
                .expect("world")
                .resources
                .get(&self.resource)
                .is_some_and(|state| state.role == Role::Primary);
            if primary {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    "drbdsetup status: simulated verification blindness",
                ));
            }
        }
        self.inner.run(program, args)
    }
}

/// The F1 invariant: when a promotion cannot be VERIFIED (the status
/// query fails with the resource actually Primary), the unwind fences
/// the residue but does NOT release the lease — a self-release would
/// waive the next grant's W7 wait while a writer might still be
/// serving. The lease stays live at the witness; the next grant waits
/// out the window keyed on its recorded end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unverifiable_promotion_never_releases_the_lease() {
    let kit = witness_kit().await;
    let f = fixture();
    seed_volume_with_protocol(&f.base, &f.world, "vol-blind", GIB, ReplicationMode::A);
    let vol = volume("vol-blind");
    let primary = authority_provider(&kit, &f.state_path, &f.world);
    primary.register_volume(&vol, None).expect("register");
    flip_world_to_peer(&f.world);
    let peer = DrbdProvider::with_authority(
        Arc::new(StatusBlindWhenPrimary {
            inner: FakeDrbd::runner(&f.world),
            world: Arc::clone(&f.world),
            resource: resource_of("vol-blind"),
        }),
        common::config_for_peer(&f.base),
        f.base.join("state-peer.json"),
        authority_for(&kit, PEER_NODE, INTERVAL),
    )
    .map(Arc::new)
    .expect("peer provider construction");
    let error = peer
        .adopt_and_promote(&vol, true)
        .expect_err("the promotion cannot be verified");
    assert_eq!(error.code, ApiErrorCode::Internal);
    // The residue is fenced and tracked: suspended, marker durable,
    // Failed — never an untracked Primary.
    let resource = resource_of("vol-blind");
    assert_eq!(role_of(&f.world, &resource), Role::Primary);
    assert!(suspended(&f.world, SEED_MINOR));
    let disk = DrbdState::load(&f.base.join("state-peer.json")).expect("load state");
    let entry = disk.volume(&vol).expect("tracked volume");
    assert!(entry.runtime.fence.is_some(), "the fence marker is durable");
    assert_eq!(entry.runtime.state, volvisor_types::VolumeLifecycle::Failed);
    // The F1 core: the lease is NOT released (a release here would
    // waive the W7 wait for the next grant while the suspended device
    // is still Primary).
    let view = kit.client.inspect(&vol).await.expect("view");
    assert_eq!(view.lease_state, LeaseState::Live);
    // A restart with a healthy runner completes the fence (demote,
    // resume, clear) — and the lease still holds until it lapses.
    let healed = DrbdProvider::with_authority(
        FakeDrbd::runner(&f.world),
        common::config_for_peer(&f.base),
        f.base.join("state-peer.json"),
        authority_for(&kit, PEER_NODE, INTERVAL),
    )
    .map(Arc::new)
    .expect("healed provider construction");
    let report = healed
        .last_reconcile_report()
        .expect("report lock")
        .expect("startup report");
    assert!(report.completed_fences.contains(&vol));
    assert_eq!(role_of(&f.world, &resource), Role::Secondary);
    assert!(!suspended(&f.world, SEED_MINOR));
    let view = kit.client.inspect(&vol).await.expect("view");
    assert_eq!(
        view.lease_state,
        LeaseState::Live,
        "the lease lapses on its own; it is never released un-demoted"
    );
}

/// The renewal pass's fence lane (F3): a busy-device fence is completed
/// by the NEXT renewal pass once the device closes — no restart
/// required.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_renewal_pass_completes_a_pending_fence_without_a_restart() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-lane").await;
    // The VM holds the device open: the fence suspends I/O but the
    // kernel refuses the demotion — the marker stays.
    state
        .world
        .lock()
        .expect("world")
        .open_devices
        .insert(SEED_MINOR);
    kit.witness_clock
        .store(START + TTL + 5 + 5 + 1, Ordering::SeqCst);
    grant_to_peer(&kit, &state.volume).await;
    kit.writer_clock
        .store(START + INTERVAL + 1, Ordering::SeqCst);
    let report = state.provider.renew_leases().expect("first pass");
    assert_eq!(report.fenced.len(), 1);
    assert!(
        !report.fenced[0].demoted,
        "the open device refuses demotion"
    );
    assert!(suspended(&state.world, SEED_MINOR));
    // The device closes; the next renewal pass (not a restart)
    // completes the fence through the lane.
    state
        .world
        .lock()
        .expect("world")
        .open_devices
        .remove(&SEED_MINOR);
    let report = state.provider.renew_leases().expect("second pass");
    assert_eq!(report.completed_fences, vec![state.volume.clone()]);
    assert_eq!(report.fence_failures, Vec::<UnverifiableVolume>::new());
    assert!(
        !suspended(&state.world, SEED_MINOR),
        "the fence completion lifts the suspension"
    );
    assert_eq!(role_of(&state.world, &state.resource), Role::Secondary);
    let disk = DrbdState::load(&state.state_path).expect("load state");
    let entry = disk.volume(&state.volume).expect("volume");
    assert!(entry.runtime.fence.is_none(), "the marker is cleared");
    assert_eq!(entry.runtime.state, volvisor_types::VolumeLifecycle::Ready);
}

// --------------------------------------------- review-round-3 additions

/// A retain delete request.
fn delete_req(volume_id: &str, expected_generation: u64) -> DeleteVolumeRequest {
    DeleteVolumeRequest {
        api_version: "volvisor.volume.v2".to_owned(),
        operation_id: OperationId::new(format!("op-delete-{volume_id}")).expect("valid id"),
        expected_generation,
        data_erasure_policy: ErasurePolicy::Retain,
    }
}

/// A TCP listener that accepts connections and never answers — a
/// witness that hangs every request for the full client timeout.
fn hanging_witness() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind hanging witness");
    let addr = listener.local_addr().expect("hanging witness address");
    std::thread::spawn(move || {
        // Hold every accepted connection open forever; the client
        // times out on its own.
        while let Ok((socket, _)) = listener.accept() {
            std::mem::forget(socket);
        }
    });
    addr
}

/// The round-3 ordering invariant: a past-deadline writer's fence is
/// executed BEFORE any blocking witness call runs — `self_fence` is
/// entirely local, so one volume's hanging renewal must never delay
/// another volume's deadline enforcement. (The pass is two-phase:
/// fences first, then renewals.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn past_deadline_fences_do_not_queue_behind_a_hanging_renewal() {
    let kit = witness_kit().await;
    let f = fixture();
    // vol-b-dead (deadlines: lease granted at witness-now START, W5
    // deadline START+TTL) sorts AFTER vol-a-fresh in the pass's
    // BTreeMap iteration — the exact shape that serialized the fence
    // behind vol-a's renewal before the phase split.
    seed_volume_with_identity(
        &f.base,
        &f.world,
        "vol-b-dead",
        GIB,
        ReplicationMode::C,
        SEED_MINOR,
        SEED_PORT,
    );
    seed_volume_with_identity(
        &f.base,
        &f.world,
        "vol-a-fresh",
        GIB,
        ReplicationMode::C,
        SEED_MINOR + 1,
        SEED_PORT + 1,
    );
    let vol_a = volume("vol-a-fresh");
    let vol_b = volume("vol-b-dead");
    let provider = authority_provider(&kit, &f.state_path, &f.world);
    provider.register_volume(&vol_b, None).expect("register b");
    provider.register_volume(&vol_a, None).expect("register a");
    provider
        .attach_volume(&vol_b, &attach_req("vol-b-dead", 1))
        .await
        .expect("attach b");
    // vol-a's lease is granted 50s later (both clocks: the W5 deadline
    // is receipt-anchored in the writer's own clock, so it sits past
    // vol-b's only when the writer clock advanced too).
    kit.witness_clock.store(START + 50, Ordering::SeqCst);
    kit.writer_clock.store(START + 50, Ordering::SeqCst);
    provider
        .attach_volume(&vol_a, &attach_req("vol-a-fresh", 1))
        .await
        .expect("attach a");
    // The renewal pass runs on a RESTARTED provider whose witness is a
    // hanging socket: the resources are demoted for its construction
    // (so startup validation makes no witness call), then promoted
    // back out of band — the pass under test sees two live Primaries.
    {
        let mut world = f.world.lock().expect("world");
        for resource in [resource_of("vol-a-fresh"), resource_of("vol-b-dead")] {
            world.resources.get_mut(&resource).expect("resource").role = Role::Secondary;
        }
    }
    {
        let mut disk = DrbdState::load(&f.state_path).expect("load state");
        for vol in [&vol_a, &vol_b] {
            let entry = disk.volume_mut(vol).expect("volume");
            entry.runtime.attachment = None;
            entry.runtime.state = volvisor_types::VolumeLifecycle::Ready;
        }
        disk.save(&f.state_path).expect("save state");
    }
    let hanging = hanging_witness();
    let clock = Arc::clone(&kit.writer_clock);
    let restarted = DrbdProvider::with_authority(
        FakeDrbd::runner(&f.world),
        config_for(&f.base),
        f.state_path.clone(),
        authority_for_url(&format!("http://{hanging}"), NODE, INTERVAL, clock),
    )
    .map(Arc::new)
    .expect("restarted provider construction");
    {
        let mut world = f.world.lock().expect("world");
        for resource in [resource_of("vol-a-fresh"), resource_of("vol-b-dead")] {
            world.resources.get_mut(&resource).expect("resource").role = Role::Primary;
        }
    }
    // Writer clock: past vol-b's deadline (START+TTL), before vol-a's
    // (START+50+TTL) and past vol-a's renewal interval.
    kit.writer_clock.store(START + TTL + 25, Ordering::SeqCst);
    let pass_provider = Arc::clone(&restarted);
    let pass = std::thread::spawn(move || pass_provider.renew_leases());
    // While vol-a's renewal hangs on the dead socket, vol-b's fence
    // must already have landed (suspend + demote + resume, authority
    // cleared) — one second in, with the renewal blocked for the full
    // five-second client timeout.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        role_of(&f.world, &resource_of("vol-b-dead")),
        Role::Secondary,
        "the past-deadline writer is fenced without waiting on any witness call"
    );
    assert_eq!(
        role_of(&f.world, &resource_of("vol-a-fresh")),
        Role::Primary,
        "the still-valid writer is untouched while its renewal hangs"
    );
    let disk = DrbdState::load(&f.state_path).expect("load state");
    assert!(
        disk.volume(&vol_b)
            .expect("volume")
            .runtime
            .authority
            .is_none(),
        "the fence cleared vol-b's authority durably"
    );
    let report = pass
        .join()
        .expect("renewal pass thread")
        .expect("renewal pass");
    assert_eq!(report.fenced.len(), 1);
    assert_eq!(report.fenced[0].volume_id, vol_b);
    assert_eq!(report.deferred.len(), 1);
    assert_eq!(report.deferred[0].volume_id, vol_a);
    // vol-a's lease was never released by the fence path (only the
    // witness's own expiry can end it).
    let view = kit.client.inspect(&vol_a).await.expect("view");
    assert_eq!(view.lease_state, LeaseState::Live);
}

/// Delete refuses a Primary resource (rule 17) WITHOUT releasing the
/// lease: a self-release would waive the next grant's W7 wait while
/// the resource might still be writing. The release is earned only on
/// the verified-not-Primary path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_refusing_a_primary_resource_releases_no_lease() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-delprim").await;
    // Roll the record back to the detached shape while the resource
    // stays Primary out of band: delete is admissible on the record
    // but must refuse on the role.
    {
        let mut disk = DrbdState::load(&state.state_path).expect("load state");
        let entry = disk.volume_mut(&state.volume).expect("volume");
        entry.runtime.attachment = None;
        entry.runtime.state = volvisor_types::VolumeLifecycle::Ready;
        disk.save(&state.state_path).expect("save state");
    }
    // A provider constructed over the rolled-back record (the
    // attaching provider's in-memory state still carries the
    // attachment; the restart is the shape an operator would see).
    let provider = authority_provider(&kit, &state.state_path, &state.world);
    let error = provider
        .delete_volume(&state.volume, &delete_req("vol-delprim", 2))
        .await
        .expect_err("delete refuses a Primary resource");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
    let view = kit.client.inspect(&state.volume).await.expect("view");
    assert_eq!(
        view.lease_state,
        LeaseState::Live,
        "no release while the resource may still be writing"
    );
    // The resource demotes out of band: delete now proceeds and the
    // verified-not-Primary path releases.
    state
        .world
        .lock()
        .expect("world")
        .resources
        .get_mut(&state.resource)
        .expect("resource")
        .role = Role::Secondary;
    provider
        .delete_volume(&state.volume, &delete_req("vol-delprim", 2))
        .await
        .expect("delete after the demotion");
    let view = kit.client.inspect(&state.volume).await.expect("view");
    assert_eq!(view.lease_state, LeaseState::Revoked);
}

/// A runner wrapper for fault injection: `drbdsetup suspend-io` for
/// one specific minor FAILS TO EXECUTE — that volume's self-fence
/// cannot even suspend. Everything else forwards verbatim.
struct SuspendFailsFor {
    inner: Arc<FakeRunner>,
    minor: u32,
}

impl CommandRunner for SuspendFailsFor {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ApiError> {
        let device = format!("/dev/drbd{}", self.minor);
        if program == "drbdsetup"
            && args.first().copied() == Some("suspend-io")
            && args.get(1).copied() == Some(device.as_str())
        {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                "drbdsetup suspend-io: simulated failure",
            ));
        }
        self.inner.run(program, args)
    }
}

/// The pass-continues invariant: a fence that itself fails is
/// reported (fence_failures) and retried next pass — it never aborts
/// the other volumes' fences. The failing volume keeps its authority
/// block (still Primary, lease live at the witness: the W7 window and
/// the witness's own expiry remain the bound).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_failed_fence_does_not_abort_the_renewal_pass() {
    let kit = witness_kit().await;
    let f = fixture();
    seed_volume_with_identity(
        &f.base,
        &f.world,
        "vol-a-stuck",
        GIB,
        ReplicationMode::C,
        SEED_MINOR,
        SEED_PORT,
    );
    seed_volume_with_identity(
        &f.base,
        &f.world,
        "vol-z-fenced",
        GIB,
        ReplicationMode::C,
        SEED_MINOR + 1,
        SEED_PORT + 1,
    );
    let vol_a = volume("vol-a-stuck");
    let vol_z = volume("vol-z-fenced");
    let provider = authority_provider(&kit, &f.state_path, &f.world);
    provider.register_volume(&vol_a, None).expect("register a");
    provider.register_volume(&vol_z, None).expect("register z");
    provider
        .attach_volume(&vol_a, &attach_req("vol-a-stuck", 1))
        .await
        .expect("attach a");
    provider
        .attach_volume(&vol_z, &attach_req("vol-z-fenced", 1))
        .await
        .expect("attach z");
    // The pass runs on a provider whose suspend-io fails for
    // vol-a's minor only (its construction reconcile reports vol-a
    // unverifiable and leaves it Primary; vol-z validates normally).
    let stuck = DrbdProvider::with_authority(
        Arc::new(SuspendFailsFor {
            inner: FakeDrbd::runner(&f.world),
            minor: SEED_MINOR,
        }),
        config_for(&f.base),
        f.state_path.clone(),
        authority_for(&kit, NODE, INTERVAL),
    )
    .map(Arc::new)
    .expect("stuck provider construction");
    // Both deadlines pass.
    kit.writer_clock.store(START + TTL + 1, Ordering::SeqCst);
    let report = stuck.renew_leases().expect("the pass completes");
    // vol-z's fence completed; vol-a's failed and was reported.
    assert_eq!(report.fenced.len(), 1);
    assert_eq!(report.fenced[0].volume_id, vol_z);
    assert_eq!(report.fence_failures.len(), 1);
    assert_eq!(report.fence_failures[0].volume_id, vol_a);
    assert_eq!(report.renewed, Vec::<VolumeId>::new());
    assert_eq!(
        role_of(&f.world, &resource_of("vol-z-fenced")),
        Role::Secondary
    );
    assert!(!suspended(&f.world, SEED_MINOR + 1));
    assert_eq!(
        role_of(&f.world, &resource_of("vol-a-stuck")),
        Role::Primary,
        "the stuck volume was not silently demoted"
    );
    // The stuck volume keeps its authority block and its live lease:
    // the witness's own expiry and the W7 window remain the bound.
    let disk = DrbdState::load(&f.state_path).expect("load state");
    assert!(
        disk.volume(&vol_a)
            .expect("volume")
            .runtime
            .authority
            .is_some(),
        "a failed fence retains the authority block for the retry"
    );
    let view = kit.client.inspect(&vol_a).await.expect("view");
    assert_eq!(view.lease_state, LeaseState::Live);
}

// --------------------------------------------- review-round-5 additions

/// Break the state save: the state file's path becomes a directory, so
/// the atomic write-then-rename fails (the rename onto a directory is
/// refused). Returns the backup copy for [`restore_state_save`].
fn break_state_save(state_path: &Path) -> std::path::PathBuf {
    let backup = state_path.with_extension("json.backup");
    std::fs::copy(state_path, &backup).expect("back the state file up");
    std::fs::remove_file(state_path).expect("remove the state file");
    std::fs::create_dir(state_path).expect("the state path is now a directory");
    backup
}

/// Undo [`break_state_save`].
fn restore_state_save(state_path: &Path, backup: &Path) {
    std::fs::remove_dir(state_path).expect("remove the directory placeholder");
    std::fs::copy(backup, state_path).expect("restore the state file");
    std::fs::remove_file(backup).expect("drop the backup");
}

/// A failed authority-block save on a FRESH grant releases the
/// just-acquired lease: a retry is not refused LEASE_HELD against our
/// own orphan until TTL + fence window (the adopt path's discipline,
/// now symmetric), and the in-memory record is restored to the
/// durable state's value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attach_save_failure_releases_a_fresh_grant() {
    let kit = witness_kit().await;
    let f = fixture();
    seed_volume(&f.base, &f.world, "vol-savefail", GIB);
    let vol = volume("vol-savefail");
    let provider = authority_provider(&kit, &f.state_path, &f.world);
    provider.register_volume(&vol, None).expect("register");
    let backup = break_state_save(&f.state_path);
    let error = provider
        .attach_volume(&vol, &attach_req("vol-savefail", 1))
        .await
        .expect_err("the authority-block save fails");
    assert_eq!(error.code, ApiErrorCode::Internal);
    // The resource was never promoted (the save precedes the
    // promotion) and the fresh grant was released.
    assert_eq!(
        role_of(&f.world, &resource_of("vol-savefail")),
        Role::Secondary
    );
    let view = kit.client.inspect(&vol).await.expect("view");
    assert_eq!(view.lease_state, LeaseState::Revoked);
    // The retry, with the save healthy again, attaches immediately —
    // no LEASE_HELD against our own orphan, no fence-window wait.
    restore_state_save(&f.state_path, &backup);
    provider
        .attach_volume(&vol, &attach_req("vol-savefail", 1))
        .await
        .expect("the retry attaches without an orphan lease");
    assert_eq!(
        role_of(&f.world, &resource_of("vol-savefail")),
        Role::Primary
    );
}

/// A failed authority-block save on a PLAIN RENEWAL (the recorded
/// block still matches the live lease) releases nothing: the lease
/// self-heals on the next renewal save, and releasing an attached
/// writer's lease would waive the next grant's W7 wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attach_save_failure_keeps_a_renewed_lease() {
    let kit = witness_kit().await;
    let f = fixture();
    seed_volume(&f.base, &f.world, "vol-savekeep", GIB);
    let vol = volume("vol-savekeep");
    let provider = authority_provider(&kit, &f.state_path, &f.world);
    provider.register_volume(&vol, None).expect("register");
    provider
        .attach_volume(&vol, &attach_req("vol-savekeep", 1))
        .await
        .expect("attach");
    // The interrupted-attach shape: the resource demoted back out of
    // band, the record rolled to detached-with-authority — a re-attach
    // renews the recorded lease rather than granting.
    {
        let mut world = f.world.lock().expect("world");
        world
            .resources
            .get_mut(&resource_of("vol-savekeep"))
            .expect("resource")
            .role = Role::Secondary;
    }
    {
        let mut disk = DrbdState::load(&f.state_path).expect("load state");
        let entry = disk.volume_mut(&vol).expect("volume");
        entry.runtime.attachment = None;
        entry.runtime.state = volvisor_types::VolumeLifecycle::Ready;
        disk.save(&f.state_path).expect("save state");
    }
    let reattacher = authority_provider(&kit, &f.state_path, &f.world);
    let backup = break_state_save(&f.state_path);
    let error = reattacher
        .attach_volume(&vol, &attach_req("vol-savekeep", 2))
        .await
        .expect_err("the authority-block save fails");
    assert_eq!(error.code, ApiErrorCode::Internal);
    // The renewed lease was NOT released: it still matches the
    // durable record and self-heals on the next save.
    let view = kit.client.inspect(&vol).await.expect("view");
    assert_eq!(view.lease_state, LeaseState::Live);
    restore_state_save(&f.state_path, &backup);
    reattacher
        .attach_volume(&vol, &attach_req("vol-savekeep", 2))
        .await
        .expect("the retry renews the same lease");
    let view = kit.client.inspect(&vol).await.expect("view");
    assert_eq!(view.lease_state, LeaseState::Live);
}
