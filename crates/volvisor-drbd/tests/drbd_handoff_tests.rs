//! Coordinated-handoff source-side behavior over the DRBD engine
//! (P4b plan §9, stage-B1 rows 8, 9, 10, 12 and 16a): every test
//! drives the real provider code through the real blocking witness
//! boundary against a real loopback witness server, exactly like the
//! P4a authority tests. The fake DRBD command surface
//! (`tests/common`) provides the host facts — including the
//! peer-apply-lag knob that models asynchronous peer apply through
//! the REAL `drbdsetup status` tokens, so the convergence proof
//! (`track_sync`) is proven to *wait*, never assume (plan §8 item 3).
//!
//! Covered rows:
//! - `quiesce_for_barrier`: suspension observed + the durable cut
//!   marker stamped write-ahead; idempotent per migration, one cut
//!   owner at a time (row 8);
//! - D6a while marked: the startup reconcile does NOT resume a
//!   migration-suspended Primary (and suspends fail-closed in the
//!   marker's write-ahead crash window), the renewal pass keeps
//!   renewing, attach/detach are refused typed (row 8);
//! - peer lag makes the barrier wait (typed, retryable
//!   `REPLICA_NOT_DURABLE` through the real status tokens), and a
//!   pre-freeze observation is refused — the boundary is fixed before
//!   it is proven (row 9, D2);
//! - `release_source`: refuses typed while the device is open (rule
//!   17, never forced), completes after the VM destroy closes it, and
//!   performs no witness call — the caller batches the W10 `RevokeSet`
//!   (row 10);
//! - `abort_prepare`: unsuspends and clears the marker pre-cut, gated
//!   on every recorded barrier of the epoch being voided (G5) and
//!   fail-closed on an unreachable witness;
//! - eligibility is VM-wide: one unprepared participant refuses the
//!   whole migration (row 12);
//! - the clear-cut-marker admin operation: refuses a Primary/writer,
//!   clears + reconciles when Secondary, and accepts only a
//!   witness-corroborated fencing proof of the volume's own retired
//!   epoch otherwise (row 16a).
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
    FakeDrbd, NODE, PEER_NODE, SEED_MINOR, SEED_PORT, fixture, provider_from_with_authority,
    seed_volume_with_identity, set_peer_lag,
};
use volvisor_drbd::AuthorityContext;
use volvisor_drbd::provider::{DrbdProvider, resource_name_for};
use volvisor_drbd::report::Role;
use volvisor_drbd::state::{DrbdState, MigrationCut, ReplicationMode};
use volvisor_provider::{HandoffSurface, VolumeProvider};
use volvisor_types::request::{AccessModeRequest, AttachVolumeRequest, DetachVolumeRequest};
use volvisor_types::{
    ApiErrorCode, AttachmentId, BarrierAttestation, DrainProof, HostId, LeaseState, MigrationId,
    OperationId, VolumeId,
};
use volvisor_witness::BlockingWitness;
use volvisor_witness::client::{HttpWitnessConnection, WitnessConnection};
use volvisor_witness::proto::{
    GrantRequest, RecordBarrierRequest, VoidBarrierRequest, WITNESS_PROTOCOL_VERSION,
};
use volvisor_witness::registry::{WitnessCore, WitnessCoreConfig};
use volvisor_witness::server::{WitnessServerState, router};

mod common;

/// One gibibyte (extent-aligned under the fixture's 4-MiB extents).
const GIB: u64 = 1 << 30;
/// The witness auth token both sides share (the legacy read-only
/// credential on a v2 witness: `kit.client` inspects with it).
const TOKEN: &str = "handoff-test-token";
/// Per-host witness credentials (W8): each side's daemon mutates with
/// its own host's token.
const NODE_TOKEN: &str = "handoff-test-node-a-token";
const PEER_TOKEN: &str = "handoff-test-node-b-token";
/// Deterministic knobs: ttl 100s, grace 5s, budget 5s (the W7 wait
/// ends 10s past a lease's recorded end), clocks starting at t=1000.
const TTL: u64 = 100;
const START: u64 = 1_000;
/// The writer's renewal cadence in these tests (well under ttl/2).
const INTERVAL: u64 = 20;
/// The minor/port of the second seeded volume (multi-volume worlds).
const SECOND_MINOR: u32 = 12;
/// See [`SECOND_MINOR`].
const SECOND_PORT: u16 = 7901;

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
    let mut host_tokens = std::collections::BTreeMap::new();
    host_tokens.insert(NODE.to_owned(), NODE_TOKEN.to_owned());
    host_tokens.insert(PEER_NODE.to_owned(), PEER_TOKEN.to_owned());
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
    Server { addr, handle }
}

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
/// for witness-side manipulation (peer grants, views, barriers).
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

fn migration(raw: &str) -> MigrationId {
    MigrationId::new(raw).expect("valid migration id")
}

fn op(name: &str) -> OperationId {
    OperationId::new(format!("handoff-op-{name}")).expect("valid operation id")
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

/// The primary host's witness-managed provider over a seeded volume.
fn authority_provider(
    kit: &WitnessKit,
    state_path: &Path,
    world: &Arc<Mutex<FakeDrbd>>,
) -> Arc<DrbdProvider> {
    provider_from_with_authority(state_path, world, authority_for(kit, NODE, INTERVAL))
}

/// A single-writer attach request for `vm`.
fn attach_req(volume_id: &str, expected_generation: u64, vm: &str) -> AttachVolumeRequest {
    AttachVolumeRequest {
        api_version: "volvisor.volume.v2".to_owned(),
        operation_id: OperationId::new(format!("op-attach-{volume_id}")).expect("valid id"),
        vm_id: vm.to_owned(),
        host_id: HostId::new("handoff-host").expect("valid id"),
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

/// Grant a lease to the PEER host directly at the witness (retiring
/// our epoch — W4). Presented with the peer's own W8 credential.
async fn grant_to_peer(
    kit: &WitnessKit,
    volume_id: &VolumeId,
) -> volvisor_witness::proto::GrantResponse {
    host_client_for(&kit.server, PEER_NODE)
        .grant(
            volume_id,
            GrantRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: op("peer-grant"),
                host_id: HostId::new(PEER_NODE).expect("valid host id"),
            },
        )
        .await
        .expect("peer grant")
}

/// Record a migration barrier at the witness with the NODE holder's
/// own credential (W8/W9).
async fn record_barrier(kit: &WitnessKit, volume_id: &VolumeId, migration_id: &MigrationId) {
    let view = kit.client.inspect(volume_id).await.expect("view");
    host_client_for(&kit.server, NODE)
        .record_barrier(
            volume_id,
            RecordBarrierRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: op("record-barrier"),
                host_id: HostId::new(NODE).expect("valid host id"),
                epoch: view.current_epoch,
                attestation: BarrierAttestation {
                    vm_paused_and_drained: true,
                    data_path_suspended: true,
                    peer_up_to_date: true,
                },
                migration_id: Some(migration_id.clone()),
            },
        )
        .await
        .expect("record barrier");
}

/// Void the migration's recorded barriers at the witness with the
/// NODE holder's own credential (W9, the abort path's evidence
/// hygiene).
async fn void_barriers(kit: &WitnessKit, volume_id: &VolumeId, migration_id: &MigrationId) {
    let view = kit.client.inspect(volume_id).await.expect("view");
    host_client_for(&kit.server, NODE)
        .void_barrier(
            volume_id,
            VoidBarrierRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: op("void-barrier"),
                host_id: HostId::new(NODE).expect("valid host id"),
                epoch: view.current_epoch,
                migration_id: Some(migration_id.clone()),
            },
        )
        .await
        .expect("void barrier");
}

/// Seed, register and attach a volume on the primary host; returns
/// the fixture pieces for further assertions.
struct Attached {
    provider: Arc<DrbdProvider>,
    world: Arc<Mutex<FakeDrbd>>,
    state_path: std::path::PathBuf,
    resource: String,
    volume: VolumeId,
}

async fn attached_as(kit: &WitnessKit, volume_id: &str, minor: u32, port: u16) -> Attached {
    let f = fixture();
    seed_volume_with_identity(
        &f.base,
        &f.world,
        volume_id,
        GIB,
        ReplicationMode::A,
        minor,
        port,
    );
    let provider = authority_provider(kit, &f.state_path, &f.world);
    let vol = volume(volume_id);
    provider.register_volume(&vol, None).expect("register");
    provider
        .attach_volume(&vol, &attach_req(volume_id, 1, "handoff-vm"))
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

/// The standard single-volume fixture (minor/port `SEED_*`).
async fn attached(kit: &WitnessKit, volume_id: &str) -> Attached {
    attached_as(kit, volume_id, SEED_MINOR, SEED_PORT).await
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

/// The durable cut marker of a volume, straight from the state file.
fn cut_marker_of(state_path: &Path, volume_id: &VolumeId) -> Option<MigrationCut> {
    DrbdState::load(state_path)
        .expect("load state")
        .volume(volume_id)
        .expect("volume exists")
        .runtime
        .migration
        .clone()
}

// ------------------------------------------------------- quiesce (row 8)

/// Quiesce suspends the source, observes the suspension and stamps
/// the durable cut marker; the same migration replays idempotently
/// and a different migration is refused typed (one cut owner at a
/// time). The trait-object path is exercised end to end (the shape
/// the daemon wires for the coordinator).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiesce_suspends_observes_and_stamps_the_durable_marker() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-quiesce").await;
    let mig = migration("mig-quiesce");
    // Through the trait object: the B2 wiring shape.
    let surface: Arc<dyn HandoffSurface> = state.provider.clone();
    let proof = surface
        .quiesce_for_barrier(&state.volume, &mig)
        .await
        .expect("quiesce");
    assert!(proof.observed_suspended);
    assert!(proof.cut_marker_durable);
    assert!(proof.suspended_at >= START);
    // Observed facts, not command exit statuses.
    assert!(suspended(&state.world, SEED_MINOR));
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
    // The marker is durable in the state file, naming the migration.
    let cut = cut_marker_of(&state.state_path, &state.volume).expect("cut marker");
    assert_eq!(cut.migration_id, mig);
    assert_eq!(cut.suspended_at, proof.suspended_at);
    // Idempotent replay for the same migration re-proves.
    let replay = surface
        .quiesce_for_barrier(&state.volume, &mig)
        .await
        .expect("replay quiesce");
    assert_eq!(replay.suspended_at, proof.suspended_at);
    // A different migration is refused typed; the marker is unchanged.
    let error = surface
        .quiesce_for_barrier(&state.volume, &migration("mig-other"))
        .await
        .expect_err("one cut owner at a time");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert_eq!(
        cut_marker_of(&state.state_path, &state.volume)
            .expect("cut marker")
            .migration_id,
        mig
    );
}

/// Quiesce applies to the source writer: an unattached volume is a
/// typed refusal before anything is stamped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiesce_refuses_an_unattached_source_typed() {
    let kit = witness_kit().await;
    let f = fixture();
    seed_volume_with_identity(
        &f.base,
        &f.world,
        "vol-unattached",
        GIB,
        ReplicationMode::A,
        SEED_MINOR,
        SEED_PORT,
    );
    let provider = authority_provider(&kit, &f.state_path, &f.world);
    let vol = volume("vol-unattached");
    let error = provider
        .quiesce_for_barrier(&vol, &migration("mig-x"))
        .expect_err("no attachment");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert!(cut_marker_of(&f.state_path, &vol).is_none());
    assert!(!suspended(&f.world, SEED_MINOR));
}

// --------------------------------------------------- D6a while marked

/// The startup reconcile does NOT resume a migration-suspended
/// Primary: it reports the volume as migration-suspended and leaves
/// the suspension and the marker exactly in place (D6a).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_does_not_resume_a_migration_suspended_primary() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-resume").await;
    let mig = migration("mig-resume");
    state
        .provider
        .quiesce_for_barrier(&state.volume, &mig)
        .expect("quiesce");
    // A daemon restart over the same durable state and world: the
    // startup reconcile must honor the marker.
    let restarted = authority_provider(&kit, &state.state_path, &state.world);
    assert!(
        suspended(&state.world, SEED_MINOR),
        "the restart must not resume the migration-suspended writer"
    );
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
    assert_eq!(
        cut_marker_of(&state.state_path, &state.volume)
            .expect("cut marker")
            .migration_id,
        mig
    );
    let report = restarted
        .last_reconcile_report()
        .expect("report")
        .expect("a pass ran");
    assert_eq!(report.migration_suspended.len(), 1);
    assert_eq!(report.migration_suspended[0].volume_id, state.volume);
    assert_eq!(report.migration_suspended[0].migration_id, mig);
}

/// The marker's write-ahead discipline (row 7's provider half): a
/// crash between the marker save and the suspend command leaves a
/// marked Primary the reconcile SUSPENDS fail-closed — the window
/// never leaves a live data path behind, and never resumes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cut_marker_is_written_ahead_of_the_suspension() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-writeahead").await;
    let mig = migration("mig-writeahead");
    // Model the crash window exactly: the marker is durable, the
    // suspend command never ran.
    {
        let mut disk = DrbdState::load(&state.state_path).expect("load state");
        disk.volume_mut(&state.volume)
            .expect("volume")
            .runtime
            .migration = Some(MigrationCut {
            migration_id: mig.clone(),
            suspended_at: START,
        });
        disk.save(&state.state_path).expect("save state");
    }
    assert!(!suspended(&state.world, SEED_MINOR));
    // The restart's reconcile suspends the marked Primary fail-closed
    // and reports it — no resume, no heal, no detach.
    let restarted = authority_provider(&kit, &state.state_path, &state.world);
    assert!(
        suspended(&state.world, SEED_MINOR),
        "the write-ahead crash window must not leave a live data path"
    );
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
    let report = restarted
        .last_reconcile_report()
        .expect("report")
        .expect("a pass ran");
    assert_eq!(report.migration_suspended.len(), 1);
    assert_eq!(report.migration_suspended[0].migration_id, mig);
}

/// The renewal pass keeps renewing a cut-marked lease (D6a: the cut
/// needs a live lease; the W5 deadline remains the bound).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_renewal_pass_keeps_renewing_a_cut_marked_lease() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-renew-cut").await;
    state
        .provider
        .quiesce_for_barrier(&state.volume, &migration("mig-renew"))
        .expect("quiesce");
    kit.writer_clock
        .store(START + INTERVAL + 1, Ordering::SeqCst);
    let report = state.provider.renew_leases().expect("renewal pass");
    assert_eq!(report.renewed, vec![state.volume.clone()]);
    // Still suspended, still Primary, still marked.
    assert!(suspended(&state.world, SEED_MINOR));
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
    assert!(cut_marker_of(&state.state_path, &state.volume).is_some());
    let view = kit.client.inspect(&state.volume).await.expect("view");
    assert_eq!(view.lease_state, LeaseState::Live);
    assert_eq!(view.holder.as_ref().expect("holder").as_str(), NODE);
}

/// Attach and detach of a cut-marked volume are refused typed (D6a).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_and_detach_of_a_cut_marked_volume_are_refused_typed() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-frozen").await;
    state
        .provider
        .quiesce_for_barrier(&state.volume, &migration("mig-frozen"))
        .expect("quiesce");
    // A genuinely new attachment attempt (a distinct attachment id):
    // the crash-replay path must not answer for it.
    let mut reattach = attach_req("vol-frozen", 2, "handoff-vm");
    reattach.attachment_id = AttachmentId::new("att-vol-frozen-second").expect("id");
    reattach.operation_id = OperationId::new("op-attach-vol-frozen-second").expect("id");
    let attach_error = state
        .provider
        .attach_volume(&state.volume, &reattach)
        .await
        .expect_err("attach over a cut-marked volume");
    assert_eq!(attach_error.code, ApiErrorCode::InvalidState);
    assert!(attach_error.detail.contains("migration-suspended"));
    let detach_error = state
        .provider
        .detach_volume(
            &state.volume,
            &AttachmentId::new("att-vol-frozen").expect("id"),
            &detach_req("att-vol-frozen", 1),
        )
        .await
        .expect_err("detach of a cut-marked volume");
    assert_eq!(detach_error.code, ApiErrorCode::InvalidState);
    assert!(detach_error.detail.contains("migration-suspended"));
    // The refusal changed nothing.
    assert!(suspended(&state.world, SEED_MINOR));
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
    assert!(cut_marker_of(&state.state_path, &state.volume).is_some());
}

/// A cut that outlives its lease fails closed through the existing
/// W5 deadline fence (D6a: the lease deadline remains the bound): the
/// renewal pass self-fences the marked writer, and the cut marker
/// SURVIVES the fence as the residue the clear-cut-marker operation
/// resolves — the fence never silently concludes the migration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_that_outlives_its_lease_fences_but_keeps_the_marker() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-deadline-cut").await;
    let mig = migration("mig-deadline-cut");
    state
        .provider
        .quiesce_for_barrier(&state.volume, &mig)
        .expect("quiesce");
    // Past the W5 local deadline (the witness is irrelevant here: the
    // deadline is the bound the writer promised).
    kit.server.handle.abort();
    kit.writer_clock.store(START + TTL + 1, Ordering::SeqCst);
    let report = state.provider.renew_leases().expect("renewal pass");
    assert_eq!(report.fenced.len(), 1);
    assert!(report.fenced[0].demoted, "the device was not open");
    assert_eq!(role_of(&state.world, &state.resource), Role::Secondary);
    // The marker survived the fence: the volume is still the
    // migration's residue, reported as migration-suspended.
    let cut = cut_marker_of(&state.state_path, &state.volume).expect("cut marker");
    assert_eq!(cut.migration_id, mig);
    let report = state.provider.reconcile().expect("reconcile");
    assert_eq!(report.migration_suspended.len(), 1);
    assert_eq!(report.migration_suspended[0].migration_id, mig);
    // The operator's resolution path is now open: the Secondary
    // residue clears without a fencing proof.
    let response = state
        .provider
        .clear_cut_marker(&state.volume, None)
        .expect("clear the Secondary residue");
    assert_eq!(response.state, volvisor_types::VolumeLifecycle::Ready);
    assert!(cut_marker_of(&state.state_path, &state.volume).is_none());
}

// -------------------------------------------------- track_sync (row 9)

/// Peer lag (fake asynchronous apply) makes the barrier wait: the
/// typed, retryable refusal surfaces through the REAL status tokens
/// until the pin clears; a pre-freeze observation is refused outright
/// (D2: the boundary is fixed before it is proven), and a lost
/// connection is a refusal too, never a reported proof.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn track_sync_waits_for_peer_convergence_through_the_real_tokens() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-sync").await;
    let mig = migration("mig-sync");
    // Before the quiesce there is no marker: an observation now would
    // prove nothing about any boundary.
    let early = state
        .provider
        .track_sync(&state.volume)
        .expect_err("no boundary is fixed yet");
    assert_eq!(early.code, ApiErrorCode::InvalidState);
    state
        .provider
        .quiesce_for_barrier(&state.volume, &mig)
        .expect("quiesce");
    // The peer lags: a resync in progress over a not-UpToDate peer
    // disk (the real token shape) is a typed, retryable refusal.
    set_peer_lag(&state.world, SEED_MINOR, true);
    let lagging = state
        .provider
        .track_sync(&state.volume)
        .expect_err("the peer has not converged");
    assert_eq!(lagging.code, ApiErrorCode::ReplicaNotDurable);
    assert!(lagging.detail.contains("retry"));
    // Convergence flips on: the proof now carries the observed facts.
    set_peer_lag(&state.world, SEED_MINOR, false);
    let proof = state.provider.track_sync(&state.volume).expect("converged");
    assert!(proof.peer_up_to_date);
    assert!(!proof.resync_active);
    assert!(proof.connection_established);
    assert!(proof.observed_after_suspension);
    // A lost connection is a refusal too, never a proof.
    state.world.lock().expect("world").peer_online = false;
    let disconnected = state
        .provider
        .track_sync(&state.volume)
        .expect_err("no connection, no proof");
    assert_eq!(disconnected.code, ApiErrorCode::ReplicaNotDurable);
}

// --------------------------------------------- release_source (row 10)

/// Release refuses typed while the device is open (rule 17 — never
/// forced), keeps the cut exactly in place for the retry, and after
/// the VM destroy closes the device it demotes, re-verifies
/// Secondary, lifts the suspension and clears the records — with NO
/// witness call (the caller batches the W10 RevokeSet). A mismatched
/// migration id is refused before any command runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_source_refuses_an_open_device_and_completes_after_the_destroy() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-release").await;
    let mig = migration("mig-release");
    state
        .provider
        .quiesce_for_barrier(&state.volume, &mig)
        .expect("quiesce");
    state
        .provider
        .track_sync(&state.volume)
        .expect("track sync");
    // A marker mismatch is refused typed, before any command.
    let mismatch = state
        .provider
        .release_source(&state.volume, &migration("mig-other"))
        .expect_err("the marker's owner must name its own cut");
    assert_eq!(mismatch.code, ApiErrorCode::InvalidState);
    // The device is still open (the VM was not destroyed): the kernel
    // refuses the demotion and the provider surfaces it typed.
    state
        .world
        .lock()
        .expect("world")
        .open_devices
        .insert(SEED_MINOR);
    let busy = state
        .provider
        .release_source(&state.volume, &mig)
        .expect_err("the device is still open");
    assert_eq!(busy.code, ApiErrorCode::InvalidState);
    assert!(busy.detail.contains("never forced"));
    // The refusal changed nothing: still Primary, suspended, marked.
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
    assert!(suspended(&state.world, SEED_MINOR));
    assert!(cut_marker_of(&state.state_path, &state.volume).is_some());
    // The VM destroy closes the device: the release completes.
    state
        .world
        .lock()
        .expect("world")
        .open_devices
        .remove(&SEED_MINOR);
    state
        .provider
        .release_source(&state.volume, &mig)
        .expect("release");
    assert_eq!(role_of(&state.world, &state.resource), Role::Secondary);
    assert!(
        !suspended(&state.world, SEED_MINOR),
        "the suspension lifts with the release"
    );
    assert!(cut_marker_of(&state.state_path, &state.volume).is_none());
    let inspect = state
        .provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    assert_eq!(inspect.state, volvisor_types::VolumeLifecycle::Ready);
    assert_eq!(
        inspect.attachment_ids,
        [] as [volvisor_types::AttachmentId; 0]
    );
    // NO witness call: the lease is still live at the witness — the
    // caller batches the set-wide W10 RevokeSet, never a subset
    // release from here.
    let view = kit.client.inspect(&state.volume).await.expect("view");
    assert_eq!(view.lease_state, LeaseState::Live);
    assert_eq!(view.holder.as_ref().expect("holder").as_str(), NODE);
}

/// A release of a volume with no cut marker is a typed refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_source_without_a_marker_is_refused_typed() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-nomarker").await;
    let error = state
        .provider
        .release_source(&state.volume, &migration("mig-none"))
        .expect_err("no cut to release");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
}

// --------------------------------------------- abort_prepare (G5 gate)

/// The pre-cut abort tail: unsuspend, clear the marker, keep the
/// attachment — and ONLY after every recorded barrier of the epoch is
/// confirmed voided (G5's hard gate on any source resume). An
/// unreachable witness refuses fail-closed: the source stays
/// suspended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_prepare_is_gated_on_voided_barriers_and_fails_closed() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-abort").await;
    let mig = migration("mig-abort");
    state
        .provider
        .quiesce_for_barrier(&state.volume, &mig)
        .expect("quiesce");
    // An unvoided recorded barrier of the epoch gates the resume.
    record_barrier(&kit, &state.volume, &mig).await;
    let gated = state
        .provider
        .abort_prepare(&state.volume, &mig)
        .expect_err("an unvoided barrier gates the resume");
    assert_eq!(gated.code, ApiErrorCode::OperationInDoubt);
    assert!(suspended(&state.world, SEED_MINOR), "never a silent resume");
    assert!(cut_marker_of(&state.state_path, &state.volume).is_some());
    // An unreachable witness refuses fail-closed too (the gate cannot
    // be evaluated): still suspended, still marked.
    void_barriers(&kit, &state.volume, &mig).await;
    kit.server.handle.abort();
    let unreachable = state
        .provider
        .abort_prepare(&state.volume, &mig)
        .expect_err("the witness is unreachable");
    assert_eq!(unreachable.code, ApiErrorCode::UnknownFencingAuthority);
    assert!(suspended(&state.world, SEED_MINOR));
    assert!(cut_marker_of(&state.state_path, &state.volume).is_some());
}

/// With the barriers voided and the witness reachable, the abort
/// unsuspends and clears the marker while keeping the attachment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_prepare_unsuspends_and_clears_the_marker_pre_cut() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-abort-ok").await;
    let mig = migration("mig-abort-ok");
    state
        .provider
        .quiesce_for_barrier(&state.volume, &mig)
        .expect("quiesce");
    record_barrier(&kit, &state.volume, &mig).await;
    void_barriers(&kit, &state.volume, &mig).await;
    state
        .provider
        .abort_prepare(&state.volume, &mig)
        .expect("abort");
    assert!(!suspended(&state.world, SEED_MINOR));
    assert_eq!(role_of(&state.world, &state.resource), Role::Primary);
    assert!(cut_marker_of(&state.state_path, &state.volume).is_none());
    // The attachment survived: the source VM resumes through its own
    // controller path, still the recorded writer.
    let inspect = state
        .provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    assert_eq!(inspect.state, volvisor_types::VolumeLifecycle::Attached);
    assert_eq!(inspect.attachment_ids.len(), 1);
    let view = kit.client.inspect(&state.volume).await.expect("view");
    assert_eq!(view.lease_state, LeaseState::Live);
}

// ------------------------------------------ eligibility (row 12, D6a)

/// Eligibility is VM-wide: one unprepared participant refuses the
/// whole migration, never a subset — and a VM this provider holds no
/// attachment for is not asserted eligible (the coordinator composes
/// the VM-wide answer).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eligibility_is_vm_wide_one_unprepared_participant_refuses_the_migration() {
    let kit = witness_kit().await;
    let a = attached_as(&kit, "vol-elig-a", SEED_MINOR, SEED_PORT).await;
    // A second volume of the same VM (a distinct device identity),
    // seeded into the same durable state; a fresh provider instance
    // loads both (the running one holds its own in-memory snapshot).
    seed_volume_with_identity(
        a.state_path.parent().expect("state path has a parent"),
        &a.world,
        "vol-elig-b",
        GIB,
        ReplicationMode::A,
        SECOND_MINOR,
        SECOND_PORT,
    );
    let provider = authority_provider(&kit, &a.state_path, &a.world);
    let vol_b = volume("vol-elig-b");
    provider.register_volume(&vol_b, None).expect("register b");
    provider
        .attach_volume(&vol_b, &attach_req("vol-elig-b", 1, "handoff-vm"))
        .await
        .expect("attach b");
    // Both participants prepared: the VM is eligible.
    let report = provider
        .handoff_eligibility("handoff-vm")
        .expect("eligibility");
    assert!(report.eligible);
    assert_eq!(report.participants.len(), 2);
    assert!(report.participants.iter().all(|p| p.eligible));
    // One participant becomes unprepared (another handoff owns its
    // cut): the whole migration is refused, with typed reasons naming
    // the participant — the healthy participant stays individually
    // eligible, but the report is the composition.
    provider
        .quiesce_for_barrier(&vol_b, &migration("mig-other"))
        .expect("quiesce b");
    let report = provider
        .handoff_eligibility("handoff-vm")
        .expect("eligibility");
    assert!(!report.eligible);
    let participant_b = report
        .participants
        .iter()
        .find(|p| p.volume_id == vol_b)
        .expect("participant b");
    assert!(!participant_b.eligible);
    assert!(
        participant_b
            .reasons
            .iter()
            .any(|reason| reason.contains("migration-cut marker"))
    );
    let participant_a = report
        .participants
        .iter()
        .find(|p| p.volume_id == a.volume)
        .expect("participant a");
    assert!(participant_a.eligible);
    // A VM this provider holds no attachment for is not asserted
    // eligible (no evidence either way).
    let foreign = provider
        .handoff_eligibility("vm-elsewhere")
        .expect("eligibility");
    assert!(!foreign.eligible);
    assert!(
        foreign.participants.is_empty(),
        "no attachment of this VM lives here"
    );
}

// ---------------------------------------- clear-cut-marker (row 16a)

/// The clear-cut-marker admin operation refuses a Primary/writer
/// without a proof, and clears + reconciles when the role is
/// Secondary (the dead-source residue shape: the stale attachment
/// completes the interrupted detach back to `Ready`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_cut_marker_refuses_a_writer_and_clears_a_secondary() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-clear").await;
    let mig = migration("mig-clear");
    state
        .provider
        .quiesce_for_barrier(&state.volume, &mig)
        .expect("quiesce");
    // A writer is refused without a proof.
    let writer = state
        .provider
        .clear_cut_marker(&state.volume, None)
        .expect_err("a Primary is never cleared unproven");
    assert_eq!(writer.code, ApiErrorCode::InvalidState);
    assert!(cut_marker_of(&state.state_path, &state.volume).is_some());
    // The residue shape: the resource demoted out of band (the
    // destination adopted a dead source and the peer role moved);
    // clearing is now provably not-writer and reconciles normally.
    state
        .world
        .lock()
        .expect("world")
        .resources
        .get_mut(&state.resource)
        .expect("resource")
        .role = Role::Secondary;
    let response = state
        .provider
        .clear_cut_marker(&state.volume, None)
        .expect("clear the Secondary residue");
    assert_eq!(response.state, volvisor_types::VolumeLifecycle::Ready);
    assert_eq!(
        response.attachment_ids,
        [] as [volvisor_types::AttachmentId; 0]
    );
    assert!(cut_marker_of(&state.state_path, &state.volume).is_none());
    // With no marker there is nothing to clear: typed refusal.
    let empty = state
        .provider
        .clear_cut_marker(&state.volume, None)
        .expect_err("no marker");
    assert_eq!(empty.code, ApiErrorCode::InvalidState);
}

/// A fencing-proven Primary clears only with a proof the witness
/// corroborates against the volume's own retired epoch: the real
/// retirement proof succeeds (and the follow-up reconcile self-fences
/// the retired writer — never a resume without proof), a fabricated
/// proof is `UNSAFE_DATA_LOSS`, and an unreachable witness refuses
/// fail-closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_cut_marker_accepts_only_a_corroborated_fencing_proof() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-proof").await;
    let mig = migration("mig-proof");
    state
        .provider
        .quiesce_for_barrier(&state.volume, &mig)
        .expect("quiesce");
    // Retire our epoch at the witness: the lease lapses, the W7
    // window passes, the peer grants the next epoch. The grant's
    // fencing proof is the durable retirement record of OUR epoch.
    kit.witness_clock
        .store(START + TTL + 5 + 5 + 1, Ordering::SeqCst);
    let grant = grant_to_peer(&kit, &state.volume).await;
    // A fabricated proof (the right epoch, a wrong commit index) is
    // refused typed and changes nothing.
    let fabricated = volvisor_types::FencingProof {
        volume_id: state.volume.clone(),
        retired_epoch: grant.fencing_proof.retired_epoch,
        commit_index: grant.fencing_proof.commit_index + 1,
    };
    let wrong = state
        .provider
        .clear_cut_marker(&state.volume, Some(&fabricated))
        .expect_err("a fabricated proof never clears a writer");
    assert_eq!(wrong.code, ApiErrorCode::UnsafeDataLoss);
    assert!(cut_marker_of(&state.state_path, &state.volume).is_some());
    assert!(suspended(&state.world, SEED_MINOR));
    // An unreachable witness refuses fail-closed (the proof cannot be
    // verified): the marker stays.
    kit.server.handle.abort();
    let unreachable = state
        .provider
        .clear_cut_marker(&state.volume, Some(&grant.fencing_proof))
        .expect_err("the witness is unreachable");
    assert_eq!(unreachable.code, ApiErrorCode::UnknownFencingAuthority);
    assert!(cut_marker_of(&state.state_path, &state.volume).is_some());
    assert!(suspended(&state.world, SEED_MINOR));
}

/// The corroborated proof clears the marker, and the normal reconcile
/// that follows self-fences the retired writer (suspend already in
/// place, demote once the device is closed, `Ready`) — fail-closed,
/// never a resume.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_corroborated_fencing_proof_clears_and_reconciles_fail_closed() {
    let kit = witness_kit().await;
    let state = attached(&kit, "vol-proof-ok").await;
    let mig = migration("mig-proof-ok");
    state
        .provider
        .quiesce_for_barrier(&state.volume, &mig)
        .expect("quiesce");
    kit.witness_clock
        .store(START + TTL + 5 + 5 + 1, Ordering::SeqCst);
    let grant = grant_to_peer(&kit, &state.volume).await;
    let response = state
        .provider
        .clear_cut_marker(&state.volume, Some(&grant.fencing_proof))
        .expect("the corroborated proof clears the marker");
    assert!(cut_marker_of(&state.state_path, &state.volume).is_none());
    // The follow-up reconcile self-fenced the retired writer: demoted
    // (the device is closed), authority cleared, never resumed.
    assert_eq!(role_of(&state.world, &state.resource), Role::Secondary);
    let inspect = state
        .provider
        .inspect_volume(&state.volume)
        .await
        .expect("inspect");
    assert_eq!(inspect.state, volvisor_types::VolumeLifecycle::Ready);
    assert!(inspect.authority.is_none());
    assert_eq!(response.state, volvisor_types::VolumeLifecycle::Ready);
    // The witness shows the new holder — the fencing was real.
    let view = kit.client.inspect(&state.volume).await.expect("view");
    assert_eq!(view.holder.as_ref().expect("holder").as_str(), PEER_NODE);
}
