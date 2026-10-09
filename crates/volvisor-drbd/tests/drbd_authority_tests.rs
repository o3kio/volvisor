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

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::{
    FakeDrbd, NODE, PEER_NODE, SEED_MINOR, config_for_peer, fixture, flip_world_to_peer,
    provider_from_with_authority, seed_volume, seed_volume_with_protocol,
};
use volvisor_drbd::AuthorityContext;
use volvisor_drbd::provider::{DrbdProvider, resource_name_for};
use volvisor_drbd::report::Role;
use volvisor_drbd::state::{DrbdState, ReplicationMode};
use volvisor_provider::VolumeProvider;
use volvisor_types::request::{AccessModeRequest, AttachVolumeRequest, DetachVolumeRequest};
use volvisor_types::{
    ApiErrorCode, AttachmentId, DrainProof, HostId, LeaseState, LossBoundary, OperationId,
    PromotionClassification, RecordedBarrier, VolumeId,
};
use volvisor_witness::BlockingWitness;
use volvisor_witness::client::{HttpWitnessConnection, WitnessConnection};
use volvisor_witness::proto::{GrantRequest, WITNESS_PROTOCOL_VERSION};
use volvisor_witness::registry::{WitnessCore, WitnessCoreConfig};
use volvisor_witness::server::{WitnessServerState, router};

mod common;

/// One gibibyte (extent-aligned under the fixture's 4-MiB extents).
const GIB: u64 = 1 << 30;
/// The witness auth token both sides share.
const TOKEN: &str = "authority-test-token";
/// Deterministic knobs: ttl 100s, grace 5s, budget 5s (the W7 wait
/// ends 10s past a lease's recorded end), clocks starting at t=1000.
const TTL: u64 = 100;
const START: u64 = 1_000;
/// The writer's renewal cadence in these tests (well under ttl/2).
const INTERVAL: u64 = 20;

// ---------------------------------------------------------------- kit

struct Server {
    addr: SocketAddr,
    handle: tokio::task::JoinHandle<()>,
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
    let state = Arc::new(WitnessServerState::with_clock(
        core,
        Some(TOKEN.to_owned()),
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
    Server { addr, handle }
}

fn client_for(server: &Server) -> HttpWitnessConnection {
    HttpWitnessConnection::new(
        format!("http://{}", server.addr),
        Some(TOKEN.to_owned()),
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
    let server = spawn_witness(dir.path(), Arc::clone(&witness_clock)).await;
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
            Arc::new(client_for(&kit.server)),
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
/// setup: another writer holds or held authority).
async fn grant_to_peer(kit: &WitnessKit, volume_id: &VolumeId) {
    kit.client
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
    let kit = witness_kit().await;
    let f = fixture();
    seed_volume(&f.base, &f.world, "vol-down", GIB);
    let provider = authority_provider(&kit, &f.state_path, &f.world);
    let vol = volume("vol-down");
    provider.register_volume(&vol, None).expect("register");
    kit.server.handle.abort();
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
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-deadline").await;
    kit.server.handle.abort();
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
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-defer").await;
    kit.server.handle.abort();
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
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-stalled").await;
    kit.server.handle.abort();
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
