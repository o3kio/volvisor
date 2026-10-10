//! End-to-end migration tests over the real daemon pair (plan
//! `2026-10-09-p4b-vmm-storage-handoff.md` §9, stage B2 rows 13–23).
//!
//! The fixture is the full production shape, not a unit harness:
//!
//! - **two daemons** (`node-a` the source, `node-b` the destination),
//!   each a real `volvisor_api::router` served by `axum` over TCP,
//!   each over its own simulated DRBD world and its own durable
//!   journal — the source's `/v2/migrations` routes drive the
//!   destination through the **real HTTP peer path**
//!   (`HttpPeerClient` → `/v2/internal/peer/*`);
//! - a **real loopback witness** (`volvisor_witness::server`) with
//!   per-host W8 credentials and an injected frozen clock, so lease
//!   windows and barrier ordering are deterministic;
//! - two **`FakeVmm` instances** sharing one snapshot root (the shared
//!   filesystem of plan §1), their device hooks wired into the fake
//!   DRBD worlds so an in-use source device genuinely refuses a
//!   demotion (AGENTS rule 17 is *provable* here, not asserted);
//! - the real `wire_migration` composition (coordinator + peer route
//!   context) — but the **retry task is deliberately never spawned**:
//!   rows that need reconciliation call `resolve()` on the
//!   coordinator directly, so a parked drive is the test's own
//!   observation, never a background race.
//!
//! Rows 13–17, 22 and 23 are end-to-end (both daemons, real peer
//! HTTP); the fault cells inject at the `FakeVmm` knobs, the fake
//! DRBD world and the witness, then recover through the same surfaces
//! production would. Reference coverage that is *not* duplicated
//! here: the provider-level handoff-surface unit rows
//! (quiesce/release/abort-prepare/classifier/multi-volume) live in
//! `volvisor-drbd/tests/drbd_handoff_tests.rs` (commits `b1d52c2`,
//! `5f8f392`), the clear-cut-marker unit rows and the API-shape rows
//! in `volvisor-api/src/tests.rs`, and the admin route in commit
//! `7a17739`.
//!
//! Every assertion is a SAFETY fact (G1–G5 of plan §11): who holds
//! the writer epoch, whether a barrier is recorded or voided, whether
//! a device is open, what the durable record says — never a
//! happy-path "it returned 200".

// Integration-test code: invariant assertions may use expect/unwrap,
// and one row's setup legitimately exceeds the line budget.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::too_many_lines)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use volvisor_api::{AppState, router};
use volvisor_drbd::report::Role;
use volvisor_drbd::state::{DrbdState, MigrationCut, ReplicationMode};
use volvisor_drbd::{AuthorityContext, DrbdProvider, resource_name_for};
use volvisor_drbd_testkit::{
    NODE, PEER_NODE, SEED_MINOR, SEED_PORT, config_for, config_for_peer, fixture, leak_tempdir,
    seed_peer_volume, seed_volume_with_identity,
};
use volvisor_handoff::{Clock, HandoffState, MigrationSurface};
use volvisor_journal::Journal;
use volvisor_provider::{
    AdoptionSurface, DeviceHook, FakeVmm, HandoffSurface, VmState, VmmController, VolumeProvider,
};
use volvisor_types::request::{AccessModeRequest, AttachVolumeRequest};
use volvisor_types::{
    AttachmentId, BarrierAttestation, HostId, LeaseState, MigrationId, OperationId, VolumeId,
    WriterEpoch,
};
use volvisor_witness::client::{HttpWitnessConnection, WitnessConnection};
use volvisor_witness::proto::{
    BatchRelease, RecordBarrierRequest, RevokeSetRequest, WITNESS_PROTOCOL_VERSION,
};
use volvisor_witness::registry::{WitnessCore, WitnessCoreConfig};
use volvisor_witness::server::WitnessServerState;
use volvisord::handoff::{
    HttpPeerClient, ParticipantFacts, PeerClient, migration_records_dir, peer_preparations_dir,
    wire_migration,
};

// ---------------------------------------------------------- constants

/// The admin bearer token both daemons share (fail-closed auth on
/// every mutating route).
const ADMIN_TOKEN: &str = "migration-e2e-admin-token";
/// The daemon-to-daemon credential guarding `/v2/internal/peer/*`
/// (deliberately distinct from every witness credential, plan §6).
const PEER_TOKEN: &str = "migration-e2e-peer-token";
/// The witness's legacy admin (read-only inspect) credential.
const WITNESS_ADMIN_TOKEN: &str = "migration-e2e-witness-admin";
/// `node-a`'s W8 witness credential (its mutations).
const NODE_TOKEN: &str = "migration-e2e-node-a-witness";
/// `node-b`'s W8 witness credential (its mutations).
const PEER_NODE_TOKEN: &str = "migration-e2e-node-b-witness";
/// Deterministic witness knobs (the kit precedent): ttl 100s, grace
/// 5s, suspend budget 5s — the W7 fence window of a lease granted at
/// `START` ends at `START + TTL + 5 + 5`.
const START: u64 = 1_000;
const TTL: u64 = 100;
/// The bound on `WitnessHandle::stop`'s drain: hyper 1.x graceful
/// shutdown has no internal deadline, so a wedged in-flight
/// connection would otherwise hang the stop forever. Every witness
/// request is awaited before a stop, so the drain only closes idle
/// keep-alive connections — milliseconds — and five seconds stays
/// generous under parallel-suite load; expiring it is a kit failure
/// (the drain did not complete — unreachability is NOT proven),
/// never a pass.
const WITNESS_DRAIN_BOUND: Duration = Duration::from_secs(5);
/// One gibibyte (extent-aligned under the fixture's 4-MiB extents).
const GIB: u64 = 1 << 30;
/// The writer renewal cadence (well under ttl/2; nothing renews on
/// its own in these tests — the authorities are only consulted).
const INTERVAL: u64 = 20;
/// The second seeded volume's minor/port (multi-volume worlds).
const SECOND_MINOR: u32 = 12;
/// See [`SECOND_MINOR`].
const SECOND_PORT: u16 = 7901;
/// Bounded polling: real timeouts, small steps.
const POLL_BOUND: Duration = Duration::from_secs(10);
/// See [`POLL_BOUND`].
const POLL_STEP: Duration = Duration::from_millis(50);

// ------------------------------------------------------ HTTP client

/// Hand-rolled minimal HTTP/1.1 client (the e2e.rs precedent: no
/// client dependency by design). One fresh connection per request
/// (`connection: close`).
async fn http(
    method: &str,
    addr: SocketAddr,
    path: &str,
    body: Option<&str>,
    bearer: Option<&str>,
) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect daemon");
    let body = body.unwrap_or("");
    let auth_header = bearer
        .map(|token| format!("authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n{auth_header}connection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    let text = String::from_utf8_lossy(&response).into_owned();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status line");
    let body_start = text
        .find("\r\n\r\n")
        .map(|i| text[i + 4..].to_owned())
        .unwrap_or_default();
    (status, body_start)
}

/// One admin-authenticated call against a daemon.
async fn admin(method: &str, addr: SocketAddr, path: &str, body: Option<&str>) -> (u16, String) {
    http(method, addr, path, body, Some(ADMIN_TOKEN)).await
}

/// One peer-credential call against a daemon's internal routes.
async fn peer_call(method: &str, addr: SocketAddr, path: &str, body: &str) -> (u16, String) {
    http(method, addr, path, Some(body), Some(PEER_TOKEN)).await
}

/// Parse a JSON response body.
fn body_json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("valid JSON body")
}

/// The observed state name of a migration summary: unit variants
/// serialize as plain strings, `in_doubt`/`aborted` as
/// single-key objects.
fn state_name(summary: &serde_json::Value) -> String {
    match &summary["state"] {
        serde_json::Value::String(name) => name.clone(),
        serde_json::Value::Object(map) => map.keys().next().cloned().unwrap_or_default(),
        _ => String::new(),
    }
}

/// Assert a contract error body names `code`.
fn assert_error_code(body: &str, code: &str) {
    let value = body_json(body);
    assert_eq!(value["code"], code, "error body: {body}");
}

// ------------------------------------------------------------- witness

/// The loopback witness: one durable directory (restarts reload the
/// same journal), one frozen injected clock, per-host W8 credentials.
struct WitnessHandle {
    addr: SocketAddr,
    clock: Arc<AtomicU64>,
    dir: PathBuf,
    /// The graceful-shutdown trigger of the current serve: dropped by
    /// [`WitnessHandle::stop`] to start the drain. `None` once
    /// stopped (or before the first launch).
    shutdown: Option<tokio::sync::watch::Sender<()>>,
    serve: Option<JoinHandle<()>>,
}

impl WitnessHandle {
    /// Spawn a fresh witness on a free loopback port.
    async fn spawn(clock: Arc<AtomicU64>) -> Self {
        let dir = leak_tempdir();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind witness");
        let addr = listener.local_addr().expect("witness local addr");
        let mut witness = Self {
            addr,
            clock,
            dir,
            shutdown: None,
            serve: None,
        };
        witness.launch(listener);
        witness
    }

    /// Serve the (re-loaded) core on `listener`.
    fn launch(&mut self, listener: TcpListener) {
        let core = WitnessCore::open(
            &self.dir,
            WitnessCoreConfig {
                lease_ttl_secs: TTL,
                lease_grace_secs: 5,
                suspend_budget_secs: 5,
            },
        )
        .expect("witness core opens");
        let mut host_tokens = BTreeMap::new();
        host_tokens.insert(NODE.to_owned(), NODE_TOKEN.to_owned());
        host_tokens.insert(PEER_NODE.to_owned(), PEER_NODE_TOKEN.to_owned());
        let clock = Arc::clone(&self.clock);
        let state = Arc::new(WitnessServerState::with_clock(
            core,
            Some(WITNESS_ADMIN_TOKEN.to_owned()),
            host_tokens,
            Arc::new(move || clock.load(Ordering::SeqCst)),
        ));
        let app = volvisor_witness::server::router(state);
        // Graceful shutdown over a watch trigger: `stop` drops the
        // sender, the serve loop stops accepting, tells every
        // connection task to drain, and waits for all of them — the
        // drain barrier `stop` awaits (the deterministic
        // unreachability gate).
        let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(());
        let serve = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.changed().await;
                })
                .await
                .expect("witness serves");
        });
        self.shutdown = Some(shutdown);
        self.serve = Some(serve);
    }

    /// Stop serving (the durable directory survives) and PROVE the
    /// stop: drop the graceful-shutdown trigger, then await the serve
    /// future to completion — the listener is dropped AND every
    /// already-accepted connection task has exited (in-flight
    /// requests drained; no idle keep-alive connection is left
    /// serviceable), so after this returns no request can complete
    /// against the witness, whatever the client does with its pooled
    /// connections. The abort shape this replaces closed the pooled
    /// connections only asynchronously — a request dispatched right
    /// after the abort could still be answered inside the close
    /// window (the same race the drbd kits' shared `Server::stop`
    /// gate removed).
    ///
    /// The drain is bounded (hyper 1.x graceful shutdown has no
    /// internal deadline, so a wedged in-flight connection would
    /// otherwise hang this forever): expiring [`WITNESS_DRAIN_BOUND`]
    /// is a kit failure — the stop did NOT prove unreachability —
    /// never a pass.
    async fn stop(&mut self) {
        drop(self.shutdown.take());
        if let Some(serve) = self.serve.take() {
            let drained = tokio::time::timeout(WITNESS_DRAIN_BOUND, serve).await;
            assert!(
                drained.is_ok(),
                "the witness drain did not complete within {WITNESS_DRAIN_BOUND:?}: a \
                 connection task is wedged (an in-flight request never finished) — \
                 the stop did NOT prove unreachability; a kit failure, never a pass"
            );
        }
    }

    /// Stop, then re-serve the SAME directory on the SAME address
    /// (the journal replays; leases and barriers survive).
    async fn restart(&mut self) {
        self.stop().await;
        let listener = TcpListener::bind(self.addr).await.expect("rebind witness");
        self.launch(listener);
    }

    /// The legacy admin (inspect-only) client.
    fn admin_client(&self) -> HttpWitnessConnection {
        HttpWitnessConnection::new(
            format!("http://{}", self.addr),
            Some(WITNESS_ADMIN_TOKEN.to_owned()),
            Duration::from_secs(5),
        )
    }

    /// The client presenting `host`'s W8 credential.
    fn host_client(&self, host: &str) -> HttpWitnessConnection {
        let token = if host == NODE {
            NODE_TOKEN
        } else {
            PEER_NODE_TOKEN
        };
        HttpWitnessConnection::new(
            format!("http://{}", self.addr),
            Some(token.to_owned()),
            Duration::from_secs(5),
        )
    }
}

/// The witness's view of one volume (admin credential).
async fn witness_view(witness: &WitnessHandle, vol: &VolumeId) -> volvisor_types::AuthorityView {
    witness
        .admin_client()
        .inspect(vol)
        .await
        .expect("witness view")
}

/// Record one all-true migration barrier at the source's current
/// epoch with the source host's own credential (W8/W9).
async fn record_barrier(witness: &WitnessHandle, vol: &VolumeId, mig: &MigrationId) {
    let view = witness_view(witness, vol).await;
    witness
        .host_client(NODE)
        .record_barrier(
            vol,
            RecordBarrierRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: OperationId::new(format!("op-record-barrier-{}", mig.as_str()))
                    .expect("valid operation id"),
                host_id: HostId::new(NODE).expect("valid host id"),
                epoch: view.current_epoch,
                attestation: BarrierAttestation {
                    vm_paused_and_drained: true,
                    data_path_suspended: true,
                    peer_up_to_date: true,
                },
                migration_id: Some(mig.clone()),
            },
        )
        .await
        .expect("record barrier");
}

/// The source's W10 self-release of one volume's current epoch (the
/// source host's own credential; retires the source epoch).
async fn revoke_set(witness: &WitnessHandle, vol: &VolumeId, mig: &MigrationId) {
    let view = witness_view(witness, vol).await;
    witness
        .host_client(NODE)
        .revoke_set(RevokeSetRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: OperationId::new(format!("op-revoke-set-{}", mig.as_str()))
                .expect("valid operation id"),
            host_id: HostId::new(NODE).expect("valid host id"),
            migration_id: Some(mig.clone()),
            releases: vec![BatchRelease {
                volume_id: vol.clone(),
                epoch: view.current_epoch,
            }],
        })
        .await
        .expect("revoke set");
}

// ------------------------------------------------------------ the VMMs

/// The per-VM device map the VMM's hook maintains (VM id → the device
/// paths it holds).
type DeviceMap = Arc<Mutex<BTreeMap<String, Vec<String>>>>;

/// Build one wired fake VMM: the device hook keeps a per-VM map of
/// the devices it holds and recomputes the host world's
/// `open_devices` as the union of the minors behind `/dev/drbd{N}`
/// paths — a present VM (any state, `Created` included) holds its
/// devices for its whole lifetime; destroy is the only release.
fn wired_vmm(
    snapshot_root: &Path,
    world: &Arc<Mutex<volvisor_drbd_testkit::FakeDrbd>>,
) -> (Arc<FakeVmm>, DeviceMap) {
    let devices: DeviceMap = Arc::new(Mutex::new(BTreeMap::new()));
    let hook_world = Arc::clone(world);
    let hook_devices = Arc::clone(&devices);
    let hook: DeviceHook = Arc::new(move |vm_id: &str, held: &[String], open: bool| {
        let mut map = hook_devices.lock().expect("device map");
        if open {
            map.insert(vm_id.to_owned(), held.to_vec());
        } else {
            map.remove(vm_id);
        }
        let mut minors = BTreeSet::new();
        for path in map.values().flatten() {
            if let Some(minor) = path.strip_prefix("/dev/drbd") {
                if let Ok(minor) = minor.parse::<u32>() {
                    minors.insert(minor);
                }
            }
        }
        let mut world = hook_world.lock().expect("world");
        world.open_devices = minors;
    });
    (
        Arc::new(FakeVmm::new(snapshot_root).with_device_hook(hook)),
        devices,
    )
}

// ------------------------------------------------------------- the hosts

/// Everything one daemon is made of, immutable across restarts.
struct HostCore {
    /// This host's identity (`node-a`/`node-b`).
    name: String,
    /// This host's W8 witness credential.
    witness_token: String,
    /// The provider's durable state file.
    state_path: PathBuf,
    /// The simulated DRBD world of this host.
    world: Arc<Mutex<volvisor_drbd_testkit::FakeDrbd>>,
    /// The daemon's journal directory (flock-scoped; freed when the
    /// serve task drops).
    journal_dir: PathBuf,
    /// The witness-managed provider.
    provider: Arc<DrbdProvider>,
    /// This host's fake VMM.
    vmm: Arc<FakeVmm>,
    /// The OTHER daemon's address (the peer client's target).
    peer_addr: SocketAddr,
    /// The witness's address (the connection and probe target).
    witness_addr: SocketAddr,
    /// The frozen coordinator/authority clock.
    clock: Clock,
    /// The shared snapshot root (one directory per VM).
    snapshot_root: PathBuf,
}

/// One launched daemon: its address, its migration handle and its
/// serve task. Restarting rebuilds the journal, the store and the
/// handle over the same durable directories.
struct Host {
    core: Arc<HostCore>,
    addr: SocketAddr,
    handle: Arc<volvisord::handoff::MigrationHandle>,
    serve: Option<JoinHandle<()>>,
}

impl Host {
    /// Build and serve one daemon on `listener`.
    fn launch(core: Arc<HostCore>, listener: TcpListener) -> Host {
        let addr = listener.local_addr().expect("daemon local addr");
        let witness = Arc::new(HttpWitnessConnection::new(
            format!("http://{}", core.witness_addr),
            Some(core.witness_token.clone()),
            Duration::from_secs(5),
        ));
        let peer = HttpPeerClient::new(
            format!("http://{}", core.peer_addr),
            PEER_TOKEN,
            Duration::from_secs(30),
        );
        let provider = Arc::clone(&core.provider);
        let facts: ParticipantFacts = Arc::new(
            move |volume_id: &VolumeId, vm_id: &str, expected_generation: u64| {
                provider.migration_participant_facts(volume_id, vm_id, expected_generation)
            },
        );
        let (handle, peer_ctx) = wire_migration(
            HostId::new(core.name.as_str()).expect("valid host id"),
            witness,
            core.witness_addr,
            Arc::clone(&core.vmm) as Arc<dyn VmmController>,
            Arc::clone(&core.provider) as Arc<dyn HandoffSurface>,
            Arc::clone(&core.provider) as Arc<dyn VolumeProvider>,
            Arc::new(peer) as Arc<dyn PeerClient>,
            facts,
            core.snapshot_root.clone(),
            migration_records_dir(&core.journal_dir),
            peer_preparations_dir(&core.journal_dir),
            Arc::clone(&core.clock),
        )
        .expect("wire migration");
        let state = AppState::new(
            Arc::clone(&core.provider) as Arc<dyn VolumeProvider>,
            None,
            Journal::open(&core.journal_dir).expect("journal opens"),
            Some(ADMIN_TOKEN.to_owned()),
        )
        .with_adoption(Arc::clone(&core.provider) as Arc<dyn AdoptionSurface>)
        .with_handoff(Arc::clone(&core.provider) as Arc<dyn HandoffSurface>)
        .with_migration(Arc::clone(&handle) as Arc<dyn MigrationSurface>)
        .with_peer_routes(Some(PEER_TOKEN.to_owned()), peer_ctx);
        let app = router(Arc::new(state), 1 << 20);
        let serve = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("daemon serves");
        });
        Host {
            core,
            addr,
            handle,
            serve: Some(serve),
        }
    }

    /// Stop serving (the journal flock frees with the serve task).
    async fn stop(&mut self) {
        if let Some(serve) = self.serve.take() {
            serve.abort();
            let _ = serve.await;
        }
    }

    /// Stop, then re-serve the SAME durable directories on the SAME
    /// address: a new journal handle, a new migration store (records
    /// re-loaded from disk), a fresh migration handle.
    async fn restart(&mut self) {
        self.stop().await;
        let listener = TcpListener::bind(self.addr).await.expect("rebind daemon");
        let core = Arc::clone(&self.core);
        *self = Host::launch(core, listener);
    }
}

// -------------------------------------------------------------- the rig

/// One seeded volume: identity-seeded on the source, peer-seeded on
/// the destination (unless [`Seeds::a_only`]).
struct VolSpec {
    id: String,
    minor: u32,
    port: u16,
}

/// One VM of the source: the volumes attached to it (all single-writer,
/// generation 1) and the fake VMM holding their devices.
struct VmSpec {
    id: String,
    vols: Vec<String>,
}

/// What `rig_with` seeds before any daemon exists.
#[derive(Default)]
struct Seeds {
    /// Volumes seeded on A (identity + attachment below) and
    /// peer-seeded on B.
    vols: Vec<VolSpec>,
    /// Volumes seeded on A only (no destination replica).
    a_only: Vec<VolSpec>,
    /// Destination-side minor overrides per volume id (row 23's
    /// divergent minors); absent entries use the source minor.
    b_minors: BTreeMap<String, u32>,
    /// VMs created and started on A.
    vms: Vec<VmSpec>,
}

fn vol(id: &str, minor: u32, port: u16) -> VolSpec {
    VolSpec {
        id: id.to_owned(),
        minor,
        port,
    }
}

fn vm(id: &str, vols: &[&str]) -> VmSpec {
    VmSpec {
        id: id.to_owned(),
        vols: vols.iter().map(|id| (*id).to_owned()).collect(),
    }
}

/// The two-daemon fixture.
struct Rig {
    a: Host,
    b: Host,
    witness: WitnessHandle,
    /// The frozen clock (witness, authorities, coordinators).
    clock: Arc<AtomicU64>,
    snapshot_root: PathBuf,
}

/// The minor a volume is seeded at on the source.
fn seeded_minor(seeds: &Seeds, id: &str) -> u32 {
    seeds
        .vols
        .iter()
        .chain(seeds.a_only.iter())
        .find(|spec| spec.id == id)
        .map_or(SEED_MINOR, |spec| spec.minor)
}

/// Build the full fixture from `seeds`: witness, both worlds (volumes
/// seeded BEFORE the providers construct — the providers load state
/// at construction), both providers, both wired VMMs, the source's
/// registrations/attachments/VMs, then both daemons.
async fn rig_with(seeds: &Seeds) -> Rig {
    let clock = Arc::new(AtomicU64::new(START));
    let witness = WitnessHandle::spawn(Arc::clone(&clock)).await;

    let source = fixture();
    let target = fixture();
    let target_state = target.base.join("state-peer.json");
    for spec in &seeds.vols {
        seed_volume_with_identity(
            &source.base,
            &source.world,
            &spec.id,
            GIB,
            ReplicationMode::A,
            spec.minor,
            spec.port,
        );
        let b_minor = seeds
            .b_minors
            .get(&spec.id)
            .map_or(spec.minor, |minor| *minor);
        seed_peer_volume(
            &target.base,
            &target.world,
            &spec.id,
            GIB,
            ReplicationMode::A,
            b_minor,
            spec.port,
        );
    }
    for spec in &seeds.a_only {
        seed_volume_with_identity(
            &source.base,
            &source.world,
            &spec.id,
            GIB,
            ReplicationMode::A,
            spec.minor,
            spec.port,
        );
    }

    let provider_a = Arc::new(
        DrbdProvider::with_authority(
            volvisor_drbd_testkit::FakeDrbd::runner(&source.world),
            config_for(&source.base),
            source.state_path.clone(),
            authority_for(witness.addr, NODE, NODE_TOKEN, &clock),
        )
        .expect("source provider construction"),
    );
    let provider_b = Arc::new(
        DrbdProvider::with_authority(
            volvisor_drbd_testkit::FakeDrbd::runner(&target.world),
            config_for_peer(&target.base),
            target_state,
            authority_for(witness.addr, PEER_NODE, PEER_NODE_TOKEN, &clock),
        )
        .expect("destination provider construction"),
    );

    let snapshot_root = leak_tempdir();
    let (vmm_a, _devices_a) = wired_vmm(&snapshot_root, &source.world);
    let (vmm_b, _devices_b) = wired_vmm(&snapshot_root, &target.world);

    // The source's writer shape: register, attach (generation 1, the
    // witness grants epoch 1 to node-a on the promote), then the VMs.
    for spec in seeds.vols.iter().chain(seeds.a_only.iter()) {
        provider_a
            .register_volume(&volume(&spec.id), None)
            .expect("register");
    }
    for spec_vm in &seeds.vms {
        for vol_id in &spec_vm.vols {
            provider_a
                .attach_volume(&volume(vol_id), &attach_req(vol_id, 1, &spec_vm.id, NODE))
                .await
                .expect("attach");
        }
        let disks: Vec<String> = spec_vm
            .vols
            .iter()
            .map(|id| format!("/dev/drbd{}", seeded_minor(seeds, id)))
            .collect();
        let refs: Vec<&str> = disks.iter().map(String::as_str).collect();
        vmm_a.create(&spec_vm.id, &refs).expect("create VM");
        vmm_a.start(&spec_vm.id).expect("start VM");
    }

    // Both listeners bind first so the peer URLs are known, then the
    // daemons compose over them.
    let listener_a = TcpListener::bind("127.0.0.1:0").await.expect("bind source");
    let listener_b = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind destination");
    let addr_b = listener_b.local_addr().expect("destination local addr");
    let core_a = Arc::new(HostCore {
        name: NODE.to_owned(),
        witness_token: NODE_TOKEN.to_owned(),
        state_path: source.state_path.clone(),
        world: Arc::clone(&source.world),
        journal_dir: leak_tempdir(),
        provider: Arc::clone(&provider_a),
        vmm: Arc::clone(&vmm_a),
        peer_addr: addr_b,
        witness_addr: witness.addr,
        clock: clock_of(&clock),
        snapshot_root: snapshot_root.clone(),
    });
    let addr_a = listener_a.local_addr().expect("source local addr");
    let core_b = Arc::new(HostCore {
        name: PEER_NODE.to_owned(),
        witness_token: PEER_NODE_TOKEN.to_owned(),
        state_path: target.base.join("state-peer.json"),
        world: Arc::clone(&target.world),
        journal_dir: leak_tempdir(),
        provider: Arc::clone(&provider_b),
        vmm: Arc::clone(&vmm_b),
        peer_addr: addr_a,
        witness_addr: witness.addr,
        clock: clock_of(&clock),
        snapshot_root: snapshot_root.clone(),
    });
    let b = Host::launch(core_b, listener_b);
    let a = Host::launch(core_a, listener_a);
    Rig {
        a,
        b,
        witness,
        clock,
        snapshot_root,
    }
}

/// A witness-managed authority for `host` over the shared clock.
fn authority_for(
    witness_addr: SocketAddr,
    host: &str,
    token: &str,
    clock: &Arc<AtomicU64>,
) -> AuthorityContext {
    let connection: Arc<dyn volvisor_witness::BlockingWitnessConnection> =
        Arc::new(volvisor_witness::BlockingWitness::new(
            Arc::new(HttpWitnessConnection::new(
                format!("http://{witness_addr}"),
                Some(token.to_owned()),
                Duration::from_secs(5),
            )),
            tokio::runtime::Handle::current(),
            Duration::from_secs(5),
        ));
    let clock = Arc::clone(clock);
    AuthorityContext::new(
        connection,
        HostId::new(host).expect("valid host id"),
        INTERVAL,
        Arc::new(move || clock.load(Ordering::SeqCst)),
    )
    .expect("authority context")
}

/// The coordinator clock over the shared frozen clock.
fn clock_of(shared: &Arc<AtomicU64>) -> Clock {
    let clock = Arc::clone(shared);
    Arc::new(move || clock.load(Ordering::SeqCst))
}

// ------------------------------------------------------- small helpers

fn volume(raw: &str) -> VolumeId {
    VolumeId::new(raw).expect("valid volume id")
}

fn migration(raw: &str) -> MigrationId {
    MigrationId::new(raw).expect("valid migration id")
}

fn host_id(raw: &str) -> HostId {
    HostId::new(raw).expect("valid host id")
}

/// The DRBD resource name of a volume id (deterministic; the same
/// name in both worlds).
fn resource_of(volume_id: &str) -> String {
    resource_name_for(&volume(volume_id))
}

/// A single-writer attach request for `vm` on `host`.
fn attach_req(
    volume_id: &str,
    expected_generation: u64,
    vm: &str,
    host: &str,
) -> AttachVolumeRequest {
    AttachVolumeRequest {
        api_version: "volvisor.volume.v2".to_owned(),
        operation_id: OperationId::new(format!("op-attach-{volume_id}")).expect("valid id"),
        vm_id: vm.to_owned(),
        host_id: host_id(host),
        attachment_id: AttachmentId::new(format!("att-{volume_id}")).expect("valid id"),
        expected_volume_generation: expected_generation,
        access_mode: AccessModeRequest::SingleWriter,
        requested_frontend: None,
        vmm_disk_id: None,
    }
}

/// The observed role of one resource in a world.
fn role_of(world: &Arc<Mutex<volvisor_drbd_testkit::FakeDrbd>>, resource: &str) -> Role {
    world
        .lock()
        .expect("world")
        .resources
        .get(resource)
        .expect("resource exists")
        .role
}

/// Whether a minor is I/O-suspended in a world.
fn suspended(world: &Arc<Mutex<volvisor_drbd_testkit::FakeDrbd>>, minor: u32) -> bool {
    world
        .lock()
        .expect("world")
        .suspended_minors
        .contains(&minor)
}

/// The open device minors of a world.
fn open_minors(world: &Arc<Mutex<volvisor_drbd_testkit::FakeDrbd>>) -> BTreeSet<u32> {
    world.lock().expect("world").open_devices.clone()
}

/// The durable cut marker of a volume, straight from a state file.
fn cut_marker_of(state_path: &Path, volume_id: &VolumeId) -> Option<MigrationCut> {
    DrbdState::load(state_path)
        .expect("load state")
        .volume(volume_id)
        .expect("volume exists")
        .runtime
        .migration
        .clone()
}

/// The migration record file of one migration on one host.
fn record_file(journal_dir: &Path, mig: &MigrationId) -> PathBuf {
    migration_records_dir(journal_dir).join(format!("{}.json", mig.as_str()))
}

/// Wait until the spawned drive task of one host has finished (it
/// holds the drive lock for its whole life). After this returns, a
/// parked drive is definitive — the direct coordinator calls below
/// cannot race it.
async fn drive_settled(host: &Host) {
    let _guard = host.handle.drive_lock().lock().await;
}

/// `POST /v2/migrations` on one daemon (admin).
async fn post_prepare(addr: SocketAddr, body: &serde_json::Value) -> (u16, String) {
    admin("POST", addr, "/v2/migrations", Some(&body.to_string())).await
}

/// `POST /v2/migrations/{id}/transfer` on one daemon (admin).
async fn post_transfer(addr: SocketAddr, mig: &str) -> (u16, String) {
    let body = serde_json::json!({
        "vm_paused_and_io_drained_proof": {"consumer": "paused and drained"},
    });
    admin(
        "POST",
        addr,
        &format!("/v2/migrations/{mig}/transfer"),
        Some(&body.to_string()),
    )
    .await
}

/// `POST /v2/migrations/{id}/abort` on one daemon (admin; no body).
async fn post_abort(addr: SocketAddr, mig: &str) -> (u16, String) {
    admin("POST", addr, &format!("/v2/migrations/{mig}/abort"), None).await
}

/// `GET /v2/migrations/{id}` on one daemon.
async fn get_migration(addr: SocketAddr, mig: &str) -> (u16, String) {
    http("GET", addr, &format!("/v2/migrations/{mig}"), None, None).await
}

/// Poll `GET /v2/migrations/{id}` (bounded, real timeout) until the
/// observed state is `want_state` and — when `want_detail` is given —
/// the in-doubt detail contains it. Returns the final summary.
async fn poll_migration(
    addr: SocketAddr,
    mig: &str,
    want_state: &str,
    want_detail: Option<&str>,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + POLL_BOUND;
    loop {
        let (status, body) = get_migration(addr, mig).await;
        assert_eq!(status, 200, "observe migration: {body}");
        let summary = body_json(&body);
        let name = state_name(&summary);
        let detail_matches = want_detail.is_none_or(|part| {
            summary["in_doubt_detail"]
                .as_str()
                .is_some_and(|detail| detail.contains(part))
        });
        if name == want_state && detail_matches {
            return summary;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "migration {mig} did not reach {want_state} \
             (detail {want_detail:?}) within {POLL_BOUND:?}; last: {body}"
        );
        tokio::time::sleep(POLL_STEP).await;
    }
}

/// The `(state, cut)` pairs of a summary's append-only trace.
fn history_pairs(summary: &serde_json::Value) -> Vec<(String, Option<String>)> {
    summary["state_history"]
        .as_array()
        .expect("state history")
        .iter()
        .map(|entry| {
            (
                entry["state"].as_str().expect("state name").to_owned(),
                entry["cut"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

/// Whether the trace carries a transition detail containing `part`.
fn history_has_detail(summary: &serde_json::Value, part: &str) -> bool {
    summary["state_history"]
        .as_array()
        .expect("state history")
        .iter()
        .any(|entry| {
            entry["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains(part))
        })
}

/// Model the source host's death mid-cut (the kit precedent): its
/// kernel state vanishes — the suspension it took dies with it and
/// its role becomes the survivor's Secondary view.
fn model_dead_source(world: &Arc<Mutex<volvisor_drbd_testkit::FakeDrbd>>, resource: &str) {
    let mut world = world.lock().expect("world");
    let minor = {
        let resource_state = world.resources.get_mut(resource).expect("resource");
        resource_state.role = Role::Secondary;
        resource_state.minor
    };
    world.suspended_minors.remove(&minor);
}

/// Drop one record's cut write-ahead on disk (the "store dropped
/// mid-drive" crash-window model): `cut` becomes `null` and the
/// trailing history entries that carry a cut are popped, so the
/// re-loaded record is the pre-cut rollback shape while the witness
/// still holds the recorded barriers.
fn strip_cut_write_ahead(path: &Path) {
    let raw = std::fs::read_to_string(path).expect("record file");
    let mut value: serde_json::Value = serde_json::from_str(&raw).expect("record JSON");
    value["cut"] = serde_json::Value::Null;
    if let Some(history) = value["state_history"].as_array_mut() {
        while history.last().is_some_and(|entry| !entry["cut"].is_null()) {
            history.pop();
        }
    }
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&value).expect("serialize record"),
    )
    .expect("write record");
}

// ------------------------------------------------------------ row 13

/// Row 13 — the happy path over both daemons and the real peer HTTP
/// path: a two-volume VM migrates `PREPARED`→`COMPLETE`; the VM is
/// resumed on the target holding exactly the target devices, the
/// source devices are closed; the witness retired the source epoch
/// and mints the destination's live lease; one non-voided barrier per
/// volume is recorded with all attestations true; the source is
/// Secondary; the source's controller order is pause → snapshot →
/// destroy (the demote never precedes the destroy).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_13_happy_path_two_volume_vm() {
    let rig = rig_with(&Seeds {
        vols: vec![
            vol("vol-13a", SEED_MINOR, SEED_PORT),
            vol("vol-13b", SECOND_MINOR, SECOND_PORT),
        ],
        vms: vec![vm("vm-13", &["vol-13a", "vol-13b"])],
        ..Seeds::default()
    })
    .await;

    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-13",
            "vm_id": "vm-13",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-13a", "vol-13b"],
            "expected_generations": [2, 2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");
    let (status, body) = post_transfer(rig.a.addr, "mig-13").await;
    assert_eq!(status, 202, "transfer: {body}");
    let summary = poll_migration(rig.a.addr, "mig-13", "complete", None).await;

    // The destination VM runs, holding exactly the target devices;
    // the source VM is gone and its devices are closed.
    assert_eq!(
        rig.b.core.vmm.vm_state("vm-13").expect("vm state"),
        VmState::Running
    );
    assert_eq!(
        rig.b.core.vmm.vm_devices("vm-13").expect("vm devices"),
        vec!["/dev/drbd11".to_owned(), "/dev/drbd12".to_owned()]
    );
    assert_eq!(
        open_minors(&rig.b.core.world),
        BTreeSet::from([SEED_MINOR, SECOND_MINOR])
    );
    assert_eq!(
        rig.a.core.vmm.vm_state("vm-13").expect("vm state"),
        VmState::Absent
    );
    assert!(open_minors(&rig.a.core.world).is_empty());

    // Roles: source Secondary everywhere, destination Primary.
    for id in ["vol-13a", "vol-13b"] {
        assert_eq!(
            role_of(&rig.a.core.world, &resource_of(id)),
            Role::Secondary,
            "source role of {id}"
        );
        assert_eq!(
            role_of(&rig.b.core.world, &resource_of(id)),
            Role::Primary,
            "destination role of {id}"
        );
    }

    // The witness: the source epoch is retired, the destination holds
    // a live epoch-2 lease, and this migration's barrier is recorded
    // once, non-voided, with all three attestations true.
    for id in ["vol-13a", "vol-13b"] {
        let view = witness_view(&rig.witness, &volume(id)).await;
        assert_eq!(view.current_epoch, WriterEpoch(2), "epoch of {id}");
        assert_eq!(view.holder, Some(host_id(PEER_NODE)));
        assert_eq!(view.lease_state, LeaseState::Live);
        assert!(
            view.retirements
                .iter()
                .any(|retirement| retirement.epoch == WriterEpoch(1)),
            "the source epoch of {id} is retired"
        );
        let barriers: Vec<_> = view
            .barriers
            .iter()
            .filter(|barrier| {
                barrier
                    .migration_id
                    .as_ref()
                    .is_some_and(|mig| mig.as_str() == "mig-13")
            })
            .collect();
        assert_eq!(barriers.len(), 1, "one barrier of mig-13 on {id}");
        assert!(!barriers[0].voided);
        assert_eq!(
            barriers[0].attestation,
            BarrierAttestation {
                vm_paused_and_drained: true,
                data_path_suspended: true,
                peer_up_to_date: true,
            }
        );
    }

    // The source's controller order: pause before snapshot before
    // destroy (a coordinator that demotes early fails against the
    // busy device — row 14 proves that half).
    let calls = rig.a.core.vmm.calls().expect("vmm calls");
    let position = |method: &str| {
        let missing = format!("the source VMM never saw {method}");
        calls
            .iter()
            .position(|(seen, _)| *seen == method)
            .expect(&missing)
    };
    assert!(position("pause") < position("snapshot"));
    assert!(position("snapshot") < position("destroy"));

    // The canonical trace is complete and append-only.
    assert_eq!(history_pairs(&summary).len(), 12);
}

// ------------------------------------------------------------ row 14

/// Row 14 — the rule-17 ordering proof at the enforcement point: the
/// demote of a still-open source device refuses typed (never forced)
/// and leaves the cut in place; the coordinated drive performs the
/// destroy first and then completes; a *stray* volume of another VM
/// on the same host is untouched (still Primary, device still open).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_14_demote_only_after_destroy() {
    let rig = rig_with(&Seeds {
        vols: vec![vol("vol-14", SEED_MINOR, SEED_PORT)],
        a_only: vec![vol("vol-14s", SECOND_MINOR, SECOND_PORT)],
        vms: vec![vm("vm-14", &["vol-14"]), vm("stray-vm-14", &["vol-14s"])],
        ..Seeds::default()
    })
    .await;

    // The VM-wide eligibility of vm-14 names exactly its own volume
    // (rule 6: the stray VM's volume is another writer set).
    let (status, body) = admin(
        "POST",
        rig.a.addr,
        "/v2/vms/vm-14/check-mobility",
        Some(&serde_json::json!({"target_host": PEER_NODE}).to_string()),
    )
    .await;
    assert_eq!(status, 200, "check-mobility: {body}");
    let report = body_json(&body);
    assert_eq!(report["eligible"], true, "{body}");
    let participants: Vec<&str> = report["participants"]
        .as_array()
        .expect("participants")
        .iter()
        .map(|participant| participant["volume_id"].as_str().expect("volume id"))
        .collect();
    assert_eq!(participants, vec!["vol-14"]);

    // The demote of the still-open source device refuses typed; the
    // cut marker stays for the retry.
    let vol14 = volume("vol-14");
    let probe = migration("mig-14-probe");
    rig.a
        .core
        .provider
        .quiesce_for_barrier(&vol14, &probe)
        .expect("quiesce");
    let refusal = rig
        .a
        .core
        .provider
        .release_source(&vol14, &probe)
        .expect_err("the busy demote refuses");
    assert_eq!(refusal.code, volvisor_types::ApiErrorCode::InvalidState);
    assert!(
        refusal
            .detail
            .contains("drbdadm secondary refused to demote")
    );
    assert!(refusal.detail.contains("still open (busy)"));
    rig.a
        .core
        .provider
        .abort_prepare(&vol14, &probe)
        .expect("abort-prepare cleanup");

    // The coordinated migration drives the correct order and lands.
    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-14",
            "vm_id": "vm-14",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-14"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");
    let (status, body) = post_transfer(rig.a.addr, "mig-14").await;
    assert_eq!(status, 202, "transfer: {body}");
    poll_migration(rig.a.addr, "mig-14", "complete", None).await;

    // The stray volume is untouched: still Primary, its device still
    // open — only the migrated minor was released.
    assert_eq!(
        role_of(&rig.a.core.world, &resource_of("vol-14s")),
        Role::Primary
    );
    assert_eq!(
        open_minors(&rig.a.core.world),
        BTreeSet::from([SECOND_MINOR])
    );
    assert_eq!(
        rig.b.core.vmm.vm_state("vm-14").expect("vm state"),
        VmState::Running
    );
}

// ------------------------------------------------------------- row 15

/// Row 15a — a crash between the barrier and the snapshot (the
/// snapshot call itself fails): the observation is `IN_DOUBT` with
/// the cut-step detail, the abort past the cut is a total typed
/// refusal (journaled, replayed byte-identically), and once the fault
/// clears the reconcile resolves **forward** to `Complete` — never
/// through the abort path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_15a_snapshot_fault_resolves_forward() {
    let rig = rig_with(&Seeds {
        vols: vec![vol("vol-15a", SEED_MINOR, SEED_PORT)],
        vms: vec![vm("vm-15a", &["vol-15a"])],
        ..Seeds::default()
    })
    .await;
    rig.a
        .core
        .vmm
        .set_fail("vm-15a", |knobs| knobs.snapshot = true)
        .expect("set snapshot knob");

    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-15a",
            "vm_id": "vm-15a",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-15a"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");
    let (status, body) = post_transfer(rig.a.addr, "mig-15a").await;
    assert_eq!(status, 202, "transfer: {body}");
    poll_migration(
        rig.a.addr,
        "mig-15a",
        "in_doubt",
        Some("cut in progress: snapshotting"),
    )
    .await;

    // Abort at or past the cut: the total, typed refusal — journaled
    // and replayed byte-identically.
    let (status, body) = post_abort(rig.a.addr, "mig-15a").await;
    assert_eq!(status, 409, "abort refusal: {body}");
    assert_error_code(&body, "INVALID_STATE");
    assert!(
        body_json(&body)["message"]
            .as_str()
            .expect("message")
            .contains("abort refused: no abort path exists at or past the cut")
    );
    let (status_replay, body_replay) = post_abort(rig.a.addr, "mig-15a").await;
    assert_eq!(status_replay, 409);
    assert_eq!(body, body_replay, "the refusal replays byte-identically");

    // The fault clears; the reconcile resolves forward to Complete.
    rig.a
        .core
        .vmm
        .set_fail("vm-15a", |knobs| knobs.snapshot = false)
        .expect("clear snapshot knob");
    drive_settled(&rig.a).await;
    let record = rig
        .a
        .handle
        .coordinator()
        .resolve(&migration("mig-15a"))
        .await
        .expect("resolve");
    assert_eq!(record.state, HandoffState::Complete);
    assert_eq!(
        rig.b.core.vmm.vm_state("vm-15a").expect("vm state"),
        VmState::Running
    );
}

/// Row 15b — the crash between the source destroy and the demote
/// (round-1 review's first named window): the drive parks in the cut
/// with the VM still present; once the destroy is observed done
/// out-of-band, the reconcile **folds the external fact** and drives
/// forward — never to the abort path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_15b_crash_between_destroy_and_demote() {
    let rig = rig_with(&Seeds {
        vols: vec![vol("vol-15b", SEED_MINOR, SEED_PORT)],
        vms: vec![vm("vm-15b", &["vol-15b"])],
        ..Seeds::default()
    })
    .await;
    rig.a
        .core
        .vmm
        .set_fail("vm-15b", |knobs| knobs.destroy = true)
        .expect("set destroy knob");

    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-15b",
            "vm_id": "vm-15b",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-15b"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");
    let (status, body) = post_transfer(rig.a.addr, "mig-15b").await;
    assert_eq!(status, 202, "transfer: {body}");
    poll_migration(
        rig.a.addr,
        "mig-15b",
        "in_doubt",
        Some("cut in progress: destroying_vm"),
    )
    .await;
    drive_settled(&rig.a).await;

    // The parked shape: the VM is paused, the destroy did not land.
    assert_eq!(
        rig.a.core.vmm.vm_state("vm-15b").expect("vm state"),
        VmState::Paused
    );
    assert_eq!(open_minors(&rig.a.core.world), BTreeSet::from([SEED_MINOR]));

    // The fault clears and the destroy lands out-of-band; the
    // reconcile folds it and completes forward.
    rig.a
        .core
        .vmm
        .set_fail("vm-15b", |knobs| knobs.destroy = false)
        .expect("clear destroy knob");
    rig.a.core.vmm.destroy("vm-15b").expect("manual destroy");
    let record = rig
        .a
        .handle
        .coordinator()
        .resolve(&migration("mig-15b"))
        .await
        .expect("resolve");
    assert_eq!(record.state, HandoffState::Complete);
    assert!(
        history_has_detail(
            &parked_follow_up(&rig, "mig-15b").await,
            "vm absent: the destroy is observed done"
        ),
        "the fold detail is in the trace"
    );
    assert_eq!(
        rig.b.core.vmm.vm_state("vm-15b").expect("vm state"),
        VmState::Running
    );
}

/// Row 15c — the crash between the demote and the revoke (round-1
/// review's second named window): with the destroy *and* the demote
/// observed done out-of-band, the reconcile folds both facts and
/// drives forward — the demote is never repeated, never rolled back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_15c_crash_between_demote_and_revoke() {
    let rig = rig_with(&Seeds {
        vols: vec![vol("vol-15c", SEED_MINOR, SEED_PORT)],
        vms: vec![vm("vm-15c", &["vol-15c"])],
        ..Seeds::default()
    })
    .await;
    rig.a
        .core
        .vmm
        .set_fail("vm-15c", |knobs| knobs.destroy = true)
        .expect("set destroy knob");

    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-15c",
            "vm_id": "vm-15c",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-15c"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");
    let (status, body) = post_transfer(rig.a.addr, "mig-15c").await;
    assert_eq!(status, 202, "transfer: {body}");
    poll_migration(
        rig.a.addr,
        "mig-15c",
        "in_doubt",
        Some("cut in progress: destroying_vm"),
    )
    .await;
    drive_settled(&rig.a).await;

    // The fault clears; the destroy and the demote land out-of-band.
    rig.a
        .core
        .vmm
        .set_fail("vm-15c", |knobs| knobs.destroy = false)
        .expect("clear destroy knob");
    rig.a.core.vmm.destroy("vm-15c").expect("manual destroy");
    rig.a
        .core
        .provider
        .release_source(&volume("vol-15c"), &migration("mig-15c"))
        .expect("manual release");
    let record = rig
        .a
        .handle
        .coordinator()
        .resolve(&migration("mig-15c"))
        .await
        .expect("resolve");
    assert_eq!(record.state, HandoffState::Complete);
    let summary = parked_follow_up(&rig, "mig-15c").await;
    assert!(
        history_has_detail(&summary, "vm absent: the destroy is observed done"),
        "the destroy fold detail is in the trace"
    );
    assert!(
        history_has_detail(
            &summary,
            "all participants Secondary: the demotes are observed done"
        ),
        "the demote fold detail is in the trace"
    );
    assert_eq!(
        rig.b.core.vmm.vm_state("vm-15c").expect("vm state"),
        VmState::Running
    );
}

/// Row 15d — the crash between the revoke and the grant: with the
/// destroy, the demote *and* the witness revoke all observed done
/// out-of-band, the reconcile folds all three facts and drives
/// forward to `Complete`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_15d_crash_between_revoke_and_grant() {
    let rig = rig_with(&Seeds {
        vols: vec![vol("vol-15d", SEED_MINOR, SEED_PORT)],
        vms: vec![vm("vm-15d", &["vol-15d"])],
        ..Seeds::default()
    })
    .await;
    rig.a
        .core
        .vmm
        .set_fail("vm-15d", |knobs| knobs.destroy = true)
        .expect("set destroy knob");

    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-15d",
            "vm_id": "vm-15d",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-15d"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");
    let (status, body) = post_transfer(rig.a.addr, "mig-15d").await;
    assert_eq!(status, 202, "transfer: {body}");
    poll_migration(
        rig.a.addr,
        "mig-15d",
        "in_doubt",
        Some("cut in progress: destroying_vm"),
    )
    .await;
    drive_settled(&rig.a).await;

    // The fault clears; the destroy, the demote and the witness
    // revoke all land out-of-band.
    rig.a
        .core
        .vmm
        .set_fail("vm-15d", |knobs| knobs.destroy = false)
        .expect("clear destroy knob");
    rig.a.core.vmm.destroy("vm-15d").expect("manual destroy");
    rig.a
        .core
        .provider
        .release_source(&volume("vol-15d"), &migration("mig-15d"))
        .expect("manual release");
    revoke_set(&rig.witness, &volume("vol-15d"), &migration("mig-15d")).await;
    let record = rig
        .a
        .handle
        .coordinator()
        .resolve(&migration("mig-15d"))
        .await
        .expect("resolve");
    assert_eq!(record.state, HandoffState::Complete);
    let summary = parked_follow_up(&rig, "mig-15d").await;
    assert!(
        history_has_detail(&summary, "vm absent: the destroy is observed done"),
        "the destroy fold detail is in the trace"
    );
    assert!(
        history_has_detail(
            &summary,
            "all participants Secondary: the demotes are observed done"
        ),
        "the demote fold detail is in the trace"
    );
    assert!(
        history_has_detail(&summary, "leases revoked: the revoke is observed done"),
        "the revoke fold detail is in the trace"
    );
    assert_eq!(
        rig.b.core.vmm.vm_state("vm-15d").expect("vm state"),
        VmState::Running
    );
}

/// Re-read one migration's summary from the daemon (the fold details
/// are asserted on the served trace, not on a private handle).
async fn parked_follow_up(rig: &Rig, mig: &str) -> serde_json::Value {
    let (status, body) = get_migration(rig.a.addr, mig).await;
    assert_eq!(status, 200, "observe: {body}");
    body_json(&body)
}

// ------------------------------------------------------------ row 16

/// Row 16 — the abort before the cut with the witness unreachable:
/// the barrier could not even be recorded, so the rollback's G5 gate
/// (`abort_prepare`'s witness inspection) refuses typed
/// (`UNKNOWN_FENCING_AUTHORITY`, journaled, replayed byte-identically)
/// and **nothing is resumed** — the VM stays paused, the suspension
/// and the cut marker stay. Once the witness returns, the same abort
/// completes: barriers voided, source unsuspended and resumed, record
/// `Aborted`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_16_abort_with_witness_down_fails_closed() {
    let mut rig = rig_with(&Seeds {
        vols: vec![vol("vol-16", SEED_MINOR, SEED_PORT)],
        vms: vec![vm("vm-16", &["vol-16"])],
        ..Seeds::default()
    })
    .await;
    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-16",
            "vm_id": "vm-16",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-16"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");

    // The witness dies before the barrier can be recorded.
    rig.witness.stop().await;
    let (status, body) = post_transfer(rig.a.addr, "mig-16").await;
    assert_eq!(status, 202, "transfer: {body}");
    poll_migration(rig.a.addr, "mig-16", "quiesced", None).await;
    drive_settled(&rig.a).await;

    // The abort's G5 gate cannot be evaluated: the void fails closed —
    // the source is fenced, never resumed — and the abort reports the
    // terminal doubt honestly as its outcome.
    let (status, body) = post_abort(rig.a.addr, "mig-16").await;
    assert_eq!(status, 200, "abort: {body}");
    let observed = body_json(&body);
    assert_eq!(state_name(&observed), "in_doubt");
    assert_eq!(
        observed["in_doubt_detail"],
        serde_json::json!("abort void failed; source fenced"),
        "{body}"
    );
    // The journaled outcome replays byte-identically.
    let (status_replay, body_replay) = post_abort(rig.a.addr, "mig-16").await;
    assert_eq!(status_replay, 200);
    assert_eq!(body, body_replay, "the outcome replays byte-identically");

    // The durable shape is untouched by doubt-resolution: paused,
    // suspended, marked — the fence holds the source.
    let (status, body) = get_migration(rig.a.addr, "mig-16").await;
    assert_eq!(status, 200, "observe: {body}");
    assert_eq!(state_name(&body_json(&body)), "in_doubt");
    assert_eq!(
        rig.a.core.vmm.vm_state("vm-16").expect("vm state"),
        VmState::Paused
    );
    assert!(suspended(&rig.a.core.world, SEED_MINOR));
    assert!(cut_marker_of(&rig.a.core.state_path, &volume("vol-16")).is_some());

    // The witness returns; the recovery completes the rollback — the
    // source resumes only after the (empty) barrier set is confirmed
    // voided.
    rig.witness.restart().await;
    let record = rig
        .a
        .handle
        .coordinator()
        .resolve(&migration("mig-16"))
        .await
        .expect("resolve");
    assert!(matches!(record.state, HandoffState::Aborted { .. }));
    assert_eq!(
        rig.a.core.vmm.vm_state("vm-16").expect("vm state"),
        VmState::Running
    );
    assert!(!suspended(&rig.a.core.world, SEED_MINOR));
    assert!(cut_marker_of(&rig.a.core.state_path, &volume("vol-16")).is_none());
}

/// Row 16a — the clear-cut-marker admin operation refuses a live
/// writer with no corroborated proof (`INVALID_STATE`, journaled,
/// replayed byte-identically); the marker stays. Once the witness is
/// back, the coordinator's own abort path completes the rollback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_16a_clear_cut_marker_refuses_a_live_writer() {
    let mut rig = rig_with(&Seeds {
        vols: vec![vol("vol-16a", SEED_MINOR, SEED_PORT)],
        vms: vec![vm("vm-16a", &["vol-16a"])],
        ..Seeds::default()
    })
    .await;
    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-16a",
            "vm_id": "vm-16a",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-16a"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");

    // Park with the marker in place and the witness unreachable.
    rig.witness.stop().await;
    let (status, body) = post_transfer(rig.a.addr, "mig-16a").await;
    assert_eq!(status, 202, "transfer: {body}");
    poll_migration(rig.a.addr, "mig-16a", "quiesced", None).await;
    drive_settled(&rig.a).await;

    // A Primary writer with no proof is refused typed — fail-closed,
    // never on the caller's say-so.
    let (status, body) = admin(
        "POST",
        rig.a.addr,
        "/v2/admin/nearline/vol-16a/clear-cut-marker",
        Some(
            &serde_json::json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": "op-ccm-16a",
                "fencing_proof": null,
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status, 409, "clear-cut-marker refusal: {body}");
    assert_error_code(&body, "INVALID_STATE");
    assert!(
        body_json(&body)["message"]
            .as_str()
            .expect("message")
            .contains("requires the volume to be provably not-writer")
    );
    let (status_replay, body_replay) = admin(
        "POST",
        rig.a.addr,
        "/v2/admin/nearline/vol-16a/clear-cut-marker",
        Some(
            &serde_json::json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": "op-ccm-16a",
                "fencing_proof": null,
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status_replay, 409);
    assert_eq!(body, body_replay, "the refusal replays byte-identically");
    assert!(cut_marker_of(&rig.a.core.state_path, &volume("vol-16a")).is_some());

    // The witness returns; the coordinator's abort completes.
    rig.witness.restart().await;
    let record = rig
        .a
        .handle
        .coordinator()
        .abort(&migration("mig-16a"))
        .await
        .expect("abort");
    assert!(matches!(record.state, HandoffState::Aborted { .. }));
    assert_eq!(
        rig.a.core.vmm.vm_state("vm-16a").expect("vm state"),
        VmState::Running
    );
}

/// Row 16b — the terminal-`InDoubt` recovery: a rollback whose
/// barrier void cannot be confirmed (witness unreachable) fences the
/// source and records terminal `IN_DOUBT`; `resolve` while the
/// witness is down changes nothing (never a resume); once the witness
/// is back the void confirms and the abort completes — and the fence
/// residue stays durably recorded for the operator.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_16b_terminal_in_doubt_recovers_after_void() {
    let mut rig = rig_with(&Seeds {
        vols: vec![vol("vol-16b", SEED_MINOR, SEED_PORT)],
        vms: vec![vm("vm-16b", &["vol-16b"])],
        ..Seeds::default()
    })
    .await;
    rig.a
        .core
        .vmm
        .set_fail("vm-16b", |knobs| knobs.snapshot = true)
        .expect("set snapshot knob");
    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-16b",
            "vm_id": "vm-16b",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-16b"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");
    let (status, body) = post_transfer(rig.a.addr, "mig-16b").await;
    assert_eq!(status, 202, "transfer: {body}");
    poll_migration(
        rig.a.addr,
        "mig-16b",
        "in_doubt",
        Some("cut in progress: snapshotting"),
    )
    .await;
    drive_settled(&rig.a).await;
    assert_eq!(
        rig.a.core.vmm.vm_state("vm-16b").expect("vm state"),
        VmState::Paused
    );

    // The store drops the cut write-ahead mid-drive (the record
    // reloads as the pre-cut shape; the witness keeps the barriers),
    // then the witness dies.
    let mig = migration("mig-16b");
    rig.witness.stop().await;
    rig.a.stop().await;
    strip_cut_write_ahead(&record_file(&rig.a.core.journal_dir, &mig));
    rig.a.restart().await;

    // The abort fails closed into terminal IN_DOUBT: the void cannot
    // be confirmed, so the source is fenced — never resumed.
    let record = rig.a.handle.coordinator().abort(&mig).await.expect("abort");
    let HandoffState::InDoubt { detail, .. } = &record.state else {
        unreachable!("expected terminal in-doubt, got {:?}", record.state);
    };
    assert_eq!(detail, "abort void failed; source fenced");
    let history_detail = record
        .state_history
        .last()
        .expect("history entry")
        .detail
        .as_ref()
        .expect("history detail");
    assert!(history_detail.contains("void_barriers failed"));
    assert!(history_detail.contains("fenced 1 of 1 participants"));

    // Resolve while the witness is down: nothing changes.
    let record = rig
        .a
        .handle
        .coordinator()
        .resolve(&mig)
        .await
        .expect("resolve");
    assert!(matches!(record.state, HandoffState::InDoubt { .. }));
    assert_eq!(
        rig.a.core.vmm.vm_state("vm-16b").expect("vm state"),
        VmState::Paused
    );

    // The witness returns; the void confirms and the rollback
    // completes — the source resumes only after the confirmation.
    rig.witness.restart().await;
    let record = rig
        .a
        .handle
        .coordinator()
        .resolve(&mig)
        .await
        .expect("resolve");
    assert!(matches!(record.state, HandoffState::Aborted { .. }));
    assert_eq!(
        rig.a.core.vmm.vm_state("vm-16b").expect("vm state"),
        VmState::Running
    );

    // The fence residue stays durably recorded: the operator's
    // reconciliation owns it, nothing silently clears it.
    let state = DrbdState::load(&rig.a.core.state_path).expect("load state");
    let stored = state.volume(&volume("vol-16b")).expect("volume exists");
    assert!(
        stored.runtime.fence.is_some(),
        "the pending-fence residue stays recorded"
    );
}

/// Row 16c — the G5 regression for a crash INSIDE the barrier drive
/// (round-1 review, finding 1): the durable record parks at `QUIESCED`
/// with an **empty proof set** while the witness already holds a live
/// barrier of this migration. The rollback's void must enumerate the
/// participants and void by the witness's own barrier log — a vacuous
/// success over the empty proof list would resume the source over a
/// live barrier, the exact false-`SAFE_CURRENT` shape G5 excludes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_16c_void_covers_a_crash_inside_the_barrier_drive() {
    let mut rig = rig_with(&Seeds {
        vols: vec![vol("vol-16c", SEED_MINOR, SEED_PORT)],
        vms: vec![vm("vm-16c", &["vol-16c"])],
        ..Seeds::default()
    })
    .await;
    // Park the record at QUIESCED with the witness down: the barrier
    // drive cannot run, so nothing is persisted.
    rig.witness.stop().await;
    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-16c",
            "vm_id": "vm-16c",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-16c"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");
    let (status, body) = post_transfer(rig.a.addr, "mig-16c").await;
    assert_eq!(status, 202, "transfer: {body}");
    poll_migration(rig.a.addr, "mig-16c", "quiesced", None).await;
    drive_settled(&rig.a).await;

    // The witness returns and the crash window is simulated exactly:
    // the barrier lands on the WITNESS (as the interrupted drive's
    // first external act), while the STORE never persists the proof —
    // the record still reads QUIESCED with an empty proof set.
    rig.witness.restart().await;
    let mig = migration("mig-16c");
    record_barrier(&rig.witness, &volume("vol-16c"), &mig).await;
    let stored: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(record_file(&rig.a.core.journal_dir, &mig))
            .expect("read the stored record"),
    )
    .expect("parse the stored record");
    assert_eq!(state_name(&stored), "quiesced", "the record is pre-barrier");
    assert_eq!(
        stored["barrier_proofs"].as_array().map(Vec::len),
        Some(0),
        "the proof set is empty — the crash window"
    );
    let view = witness_view(&rig.witness, &volume("vol-16c")).await;
    assert!(
        view.barriers
            .iter()
            .any(|barrier| barrier.migration_id.as_ref() == Some(&mig) && !barrier.voided),
        "the witness holds the drive's live barrier"
    );

    // The abort's void enumerates the participants and voids the
    // proof-less live barrier from the witness log; only then does
    // the source resume.
    let record = rig.a.handle.coordinator().abort(&mig).await.expect("abort");
    assert!(matches!(record.state, HandoffState::Aborted { .. }));
    let view = witness_view(&rig.witness, &volume("vol-16c")).await;
    assert!(
        view.barriers
            .iter()
            .filter(|barrier| barrier.migration_id.as_ref() == Some(&mig))
            .all(|barrier| barrier.voided),
        "the proof-less live barrier is voided, not vacuously skipped"
    );
    assert_eq!(
        rig.a.core.vmm.vm_state("vm-16c").expect("vm state"),
        VmState::Running
    );
    assert!(!suspended(&rig.a.core.world, SEED_MINOR));
    assert!(cut_marker_of(&rig.a.core.state_path, &volume("vol-16c")).is_none());
}

// ------------------------------------------------------------ row 17

/// Row 17 — the dead source inside the fence window: the drive parks
/// mid-cut (the destroy fails), the source's kernel state vanishes,
/// and once the W7 window has passed the destination **adopts**
/// through the P4a admin path: the recorded, non-voided, all-true
/// migration barrier is machine-checked `SAFE_CURRENT` evidence —
/// D5's convergence — and the destination holds the next live epoch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_17_dead_source_adopts_safe_current() {
    let rig = rig_with(&Seeds {
        vols: vec![vol("vol-17", SEED_MINOR, SEED_PORT)],
        vms: vec![vm("vm-17", &["vol-17"])],
        ..Seeds::default()
    })
    .await;
    rig.a
        .core
        .vmm
        .set_fail("vm-17", |knobs| knobs.destroy = true)
        .expect("set destroy knob");
    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-17",
            "vm_id": "vm-17",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-17"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");
    let (status, body) = post_transfer(rig.a.addr, "mig-17").await;
    assert_eq!(status, 202, "transfer: {body}");
    poll_migration(
        rig.a.addr,
        "mig-17",
        "in_doubt",
        Some("cut in progress: destroying_vm"),
    )
    .await;
    drive_settled(&rig.a).await;

    // The source dies mid-cut and the witness clock passes the W7
    // fence window of its lease (granted at START, ttl TTL, grace 5,
    // suspend budget 5).
    model_dead_source(&rig.a.core.world, &resource_of("vol-17"));
    rig.clock.store(START + TTL + 5 + 5 + 1, Ordering::SeqCst);

    // The destination adopts: SAFE_CURRENT on migration-barrier
    // evidence, no loss authorization asked for or needed.
    let (status, body) = admin(
        "POST",
        rig.b.addr,
        "/v2/admin/nearline/vol-17/adopt",
        Some(
            &serde_json::json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": "op-adopt-17",
                "allow_loss": false,
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status, 200, "adopt: {body}");
    let adopted = body_json(&body);
    assert_eq!(adopted["classification"], "safe_current", "{body}");
    assert_eq!(adopted["evidence"], "migration-barrier", "{body}");
    assert!(
        adopted["volume"].is_object(),
        "a volume was adopted: {body}"
    );

    // The destination now holds the writer epoch.
    assert_eq!(
        role_of(&rig.b.core.world, &resource_of("vol-17")),
        Role::Primary
    );
    let view = witness_view(&rig.witness, &volume("vol-17")).await;
    assert_eq!(view.current_epoch, WriterEpoch(2));
    assert_eq!(view.holder, Some(host_id(PEER_NODE)));
    assert_eq!(view.lease_state, LeaseState::Live);
}

// ------------------------------------------------------------ row 18

/// Row 18a — the dead destination VMM: the restore act fails (the
/// peer route journals the failure), the drive parks at
/// `DESTINATION_AUTHORIZED` and the observation is the canonical
/// state **plus the typed stall detail** — `IN_DOUBT`-observable,
/// never a silent half-migration. The journaled failure replays
/// verbatim under its derived operation id, so the stall persists
/// even after the fault clears: the VM is never resumed
/// half-migrated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_18a_restore_fault_stalls_in_destination_authorized() {
    let rig = rig_with(&Seeds {
        vols: vec![vol("vol-18a", SEED_MINOR, SEED_PORT)],
        vms: vec![vm("vm-18a", &["vol-18a"])],
        ..Seeds::default()
    })
    .await;
    // The destination's restore fails before anything lands.
    rig.b
        .core
        .vmm
        .set_fail("vm-18a", |knobs| knobs.restore = true)
        .expect("set restore knob");
    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-18a",
            "vm_id": "vm-18a",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-18a"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");
    let (status, body) = post_transfer(rig.a.addr, "mig-18a").await;
    assert_eq!(status, 202, "transfer: {body}");
    poll_migration(
        rig.a.addr,
        "mig-18a",
        "destination_authorized",
        Some("stalled in DESTINATION_AUTHORIZED: migration not yet complete"),
    )
    .await;
    drive_settled(&rig.a).await;

    // The stalled shape: the VM exists nowhere — it was destroyed on
    // the source and never restored on the destination.
    assert_eq!(
        rig.a.core.vmm.vm_state("vm-18a").expect("vm state"),
        VmState::Absent
    );
    assert_eq!(
        rig.b.core.vmm.vm_state("vm-18a").expect("vm state"),
        VmState::Absent
    );
    let restore_calls = |rig: &Rig| {
        rig.b
            .core
            .vmm
            .calls()
            .expect("vmm calls")
            .into_iter()
            .filter(|(method, _)| *method == "restore")
            .count()
    };
    assert_eq!(restore_calls(&rig), 1, "exactly one restore attempt");

    // The fault clears — but the journaled failure replays verbatim
    // (the peer route's derived operation id): the stall persists.
    rig.b
        .core
        .vmm
        .set_fail("vm-18a", |knobs| knobs.restore = false)
        .expect("clear restore knob");
    let outcome = rig
        .a
        .handle
        .coordinator()
        .resolve(&migration("mig-18a"))
        .await;
    assert!(outcome.is_err(), "the journaled restore failure replays");
    assert_eq!(
        rig.b.core.vmm.vm_state("vm-18a").expect("vm state"),
        VmState::Absent
    );
    assert_eq!(restore_calls(&rig), 1, "the replay re-executes nothing");
    let summary = parked_follow_up(&rig, "mig-18a").await;
    assert_eq!(state_name(&summary), "destination_authorized");
    assert_eq!(
        summary["in_doubt_detail"].as_str().expect("stall detail"),
        "stalled in DESTINATION_AUTHORIZED: migration not yet complete"
    );
}

/// Row 18b — the half-restored destination: a crashed restore left a
/// defined (not-booted) VM on the destination; the restore act
/// **destroys the partial VM first** and, because the observation
/// happened before the destroy, does not run the restore itself — the
/// honest answer is `absent`. The journaled success replays
/// byte-identically and re-executes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_18b_half_restored_destination_destroyed_first() {
    let rig = rig_with(&Seeds {
        vols: vec![vol("vol-18b", SEED_MINOR, SEED_PORT)],
        vms: vec![vm("vm-18b", &["vol-18b"])],
        ..Seeds::default()
    })
    .await;
    let mig = migration("mig-18b");
    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-18b",
            "vm_id": "vm-18b",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-18b"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");

    // The source's snapshot artifacts, for real.
    let snapshot_dir = rig.snapshot_root.join("vm-18b");
    rig.a.core.vmm.pause("vm-18b").expect("pause");
    rig.a
        .core
        .vmm
        .snapshot("vm-18b", &snapshot_dir)
        .expect("snapshot");
    // The witness tail: the barrier, then the source's self-release.
    record_barrier(&rig.witness, &volume("vol-18b"), &mig).await;
    revoke_set(&rig.witness, &volume("vol-18b"), &mig).await;

    // The destination's grant (real peer HTTP, the peer credential):
    // the promoted replica's device path is the restore's input.
    let (status, body) = peer_call(
        "POST",
        rig.b.addr,
        "/v2/internal/peer/grant",
        &serde_json::json!({"migration_id": "mig-18b"}).to_string(),
    )
    .await;
    assert_eq!(status, 200, "grant: {body}");
    let device_path = body_json(&body)["grants"][0]["device_path"]
        .as_str()
        .expect("device path")
        .to_owned();

    // A crashed restore left a half-restored (defined, not-booted) VM
    // on the destination.
    rig.b
        .core
        .vmm
        .create("vm-18b", &[device_path.as_str()])
        .expect("create the half-restored VM");

    // The restore act re-drives: the partial VM is destroyed first
    // and the restore then runs into the emptied VMM, landing paused
    // (resume is false) — one call converges (plan §3's re-drive
    // rule).
    let request = serde_json::json!({
        "migration_id": "mig-18b",
        "snapshot_dir": snapshot_dir.to_string_lossy(),
        "disks": [{"declared_path": "/dev/drbd11", "device_path": device_path}],
        "resume": false,
    })
    .to_string();
    let (status, body) =
        peer_call("POST", rig.b.addr, "/v2/internal/peer/restore-vm", &request).await;
    assert_eq!(status, 200, "restore-vm: {body}");
    assert_eq!(body_json(&body)["vm_state"], "paused", "{body}");
    assert_eq!(
        rig.b.core.vmm.vm_state("vm-18b").expect("vm state"),
        VmState::Paused
    );
    let calls = rig.b.core.vmm.calls().expect("vmm calls");
    let destroyed_at = calls
        .iter()
        .position(|(method, _)| *method == "destroy")
        .expect("the partial VM was destroyed");
    let restored_at = calls
        .iter()
        .position(|(method, _)| *method == "restore")
        .expect("the restore ran into the emptied VMM");
    assert!(
        destroyed_at < restored_at,
        "the half-restored VM is destroyed before the restore: {calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _)| *method == "destroy")
            .count(),
        1,
        "the partial VM was destroyed exactly once"
    );

    // The journaled success replays byte-identically; nothing
    // re-executes.
    let (status_replay, body_replay) =
        peer_call("POST", rig.b.addr, "/v2/internal/peer/restore-vm", &request).await;
    assert_eq!(status_replay, 200);
    assert_eq!(body, body_replay, "the success replays byte-identically");
    let calls = rig.b.core.vmm.calls().expect("vmm calls");
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _)| *method == "destroy")
            .count(),
        1,
        "nothing re-executes on the replay"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _)| *method == "restore")
            .count(),
        1,
        "nothing re-executes on the replay"
    );
}

/// Row 18c — the COORDINATOR's own re-drive over a half-restored
/// destination (round-1 review, finding 2): the drive parks before
/// the grant (the destination daemon unreachable — a transport
/// failure, never a journaled peer-route failure), a crashed prior
/// restore leaves a defined-not-booted VM on the destination socket,
/// and the coordinator's resolve drives straight through it — the
/// restore act destroys the half-restore first and the migration
/// COMPLETES (row 18b proves the same rule at the route level).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_18c_coordinator_re_drives_a_half_restored_destination() {
    let mut rig = rig_with(&Seeds {
        vols: vec![vol("vol-18c", SEED_MINOR, SEED_PORT)],
        vms: vec![vm("vm-18c", &["vol-18c"])],
        ..Seeds::default()
    })
    .await;
    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-18c",
            "vm_id": "vm-18c",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-18c"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");

    // The destination daemon dies after the preparation: the drive's
    // grant cannot reach it — a transport failure parks the record at
    // the source-revoked `IN_DOUBT` observation (nothing is journaled
    // by the peer, so the re-drive is not blocked by a replayed
    // failure outcome).
    rig.b.stop().await;
    let (status, body) = post_transfer(rig.a.addr, "mig-18c").await;
    assert_eq!(status, 202, "transfer: {body}");
    poll_migration(
        rig.a.addr,
        "mig-18c",
        "in_doubt",
        Some("source revoked; destination grant not yet authorized"),
    )
    .await;
    drive_settled(&rig.a).await;

    // While the destination is down, a crashed prior restore's
    // half-restored (defined, not-booted) VM sits on its socket.
    rig.b
        .core
        .vmm
        .create("vm-18c", &[&format!("/dev/drbd{SEED_MINOR}")])
        .expect("create the half-restored VM");

    // The destination returns; the coordinator's re-drive grants,
    // then restores — over the half-restore, destroying it first —
    // and the migration completes.
    rig.b.restart().await;
    let record = rig
        .a
        .handle
        .coordinator()
        .resolve(&migration("mig-18c"))
        .await
        .expect("resolve");
    assert_eq!(record.state, HandoffState::Complete);
    assert_eq!(
        rig.b.core.vmm.vm_state("vm-18c").expect("vm state"),
        VmState::Running
    );
    assert_eq!(
        rig.a.core.vmm.vm_state("vm-18c").expect("vm state"),
        VmState::Absent,
        "the source VM is gone"
    );
    let calls = rig.b.core.vmm.calls().expect("vmm calls");
    let destroyed_at = calls
        .iter()
        .position(|(method, _)| *method == "destroy")
        .expect("the half-restored VM was destroyed");
    let restored_at = calls
        .iter()
        .position(|(method, _)| *method == "restore")
        .expect("the restore ran");
    assert!(
        destroyed_at < restored_at,
        "the half-restore is destroyed before the restore: {calls:?}"
    );
}

// ------------------------------------------------------------ row 19

/// Row 19 — the snapshot-dir boundary: when the shared directory is
/// unusable *from the destination host* (here: a file where the
/// migration's directory would be), `PREPARED` refuses typed through
/// the real peer path — after the replica verification, at the probe
/// — and nothing is persisted (the typed 404 on observe).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_19_snapshot_dir_unusable_refuses_prepare() {
    let rig = rig_with(&Seeds {
        vols: vec![vol("vol-19", SEED_MINOR, SEED_PORT)],
        vms: vec![vm("vm-19", &["vol-19"])],
        ..Seeds::default()
    })
    .await;
    // A file where the migration's snapshot directory would be: the
    // destination's write+read-back+remove probe fails.
    std::fs::write(rig.snapshot_root.join("vm-19"), b"not a directory")
        .expect("plant the unusable path");

    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-19",
            "vm_id": "vm-19",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-19"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 409, "prepare refusal: {body}");
    assert_error_code(&body, "INVALID_STATE");
    assert!(
        body_json(&body)["message"]
            .as_str()
            .expect("message")
            .contains("is not usable from this host")
    );

    // Nothing was persisted.
    let (status, body) = get_migration(rig.a.addr, "mig-19").await;
    assert_eq!(status, 404, "observe: {body}");
}

// ------------------------------------------------------------ row 20

/// Row 20 — eligibility with a fenced participant and a marked
/// participant: the fenced volume's attachment is gone (it is not
/// even part of the VM's writer set any more), the marked volume is
/// listed with the typed cut-marker reason, and the VM-wide answer is
/// `eligible: false` — one unprepared participant refuses the whole
/// migration, never a subset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_20_eligibility_fenced_and_marked_volumes() {
    let rig = rig_with(&Seeds {
        vols: vec![
            vol("vol-20a", SEED_MINOR, SEED_PORT),
            vol("vol-20b", SECOND_MINOR, SECOND_PORT),
        ],
        vms: vec![vm("vm-20", &["vol-20a", "vol-20b"])],
        ..Seeds::default()
    })
    .await;

    // One participant is fenced (fail-closed; its demote is deferred
    // by the open device, which is the honest pending fence).
    rig.a
        .core
        .provider
        .fail_closed_fence(&volume("vol-20a"), "test: fenced participant")
        .expect("fence");
    // The other carries another handoff's cut marker.
    let probe = migration("mig-20-probe");
    rig.a
        .core
        .provider
        .quiesce_for_barrier(&volume("vol-20b"), &probe)
        .expect("quiesce");

    let (status, body) = admin(
        "POST",
        rig.a.addr,
        "/v2/vms/vm-20/check-mobility",
        Some(&serde_json::json!({"target_host": PEER_NODE}).to_string()),
    )
    .await;
    assert_eq!(status, 200, "check-mobility: {body}");
    let report = body_json(&body);
    assert_eq!(report["eligible"], false, "{body}");
    let participants: Vec<&str> = report["participants"]
        .as_array()
        .expect("participants")
        .iter()
        .map(|participant| participant["volume_id"].as_str().expect("volume id"))
        .collect();
    // The fenced volume lost its attachment, so the report names only
    // the still-attached (marked) participant.
    assert_eq!(participants, vec!["vol-20b"]);
    let reasons: Vec<&str> = report["participants"][0]["reasons"]
        .as_array()
        .expect("reasons")
        .iter()
        .map(|reason| reason.as_str().expect("reason text"))
        .collect();
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("already carries a migration-cut marker")),
        "the typed cut-marker reason: {reasons:?}"
    );

    // Cleanup: the probe's marker is abortable (no barrier was
    // recorded for it).
    rig.a
        .core
        .provider
        .abort_prepare(&volume("vol-20b"), &probe)
        .expect("abort-prepare cleanup");
}

// ------------------------------------------------------------ row 21

/// Row 21 — the API discipline: prepare/transfer are journaled and
/// idempotent (identical replays byte-identical, a content conflict
/// under the same migration id is the typed conflict), the abort
/// replays byte-identically, and `ObserveHandoff` reports the exact
/// canonical trace — every cut step in order, the revoke/grant pair
/// never collapsed, the cut cleared only at `Complete`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_21_idempotency_and_canonical_observation() {
    let rig = rig_with(&Seeds {
        vols: vec![vol("vol-21", SEED_MINOR, SEED_PORT)],
        vms: vec![vm("vm-21", &["vol-21"])],
        ..Seeds::default()
    })
    .await;

    // --- the abort arm: prepare, abort, byte-identical abort replay.
    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-21a",
            "vm_id": "vm-21",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-21"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");
    let (status, body) = post_abort(rig.a.addr, "mig-21a").await;
    assert_eq!(status, 200, "abort: {body}");
    assert_eq!(
        state_name(&body_json(&body)),
        "aborted",
        "the aborted observation: {body}"
    );
    let (status_replay, body_replay) = post_abort(rig.a.addr, "mig-21a").await;
    assert_eq!(status_replay, 200);
    assert_eq!(body, body_replay, "the abort replays byte-identically");

    // --- the transfer arm: prepare replays byte-identically, a
    // different body under the same id is the typed conflict.
    let prepare = serde_json::json!({
        "migration_id": "mig-21b",
        "vm_id": "vm-21",
        "target_host": PEER_NODE,
        "volume_ids": ["vol-21"],
        "expected_generations": [2],
    });
    let (status, body) = post_prepare(rig.a.addr, &prepare).await;
    assert_eq!(status, 201, "prepare: {body}");
    let (status_replay, body_replay) = post_prepare(rig.a.addr, &prepare).await;
    assert_eq!(status_replay, 201);
    assert_eq!(body, body_replay, "the prepare replay is byte-identical");
    let conflicting = serde_json::json!({
        "migration_id": "mig-21b",
        "vm_id": "vm-21",
        "target_host": PEER_NODE,
        "volume_ids": ["vol-21"],
        "expected_generations": [3],
    });
    let (status, conflict) = post_prepare(rig.a.addr, &conflicting).await;
    assert_eq!(status, 409, "conflict: {conflict}");
    assert_error_code(&conflict, "IDEMPOTENCY_CONFLICT");

    // --- the drive: transfer replays byte-identically, and the final
    // trace is the exact canonical sequence.
    let (status, body) = post_transfer(rig.a.addr, "mig-21b").await;
    assert_eq!(status, 202, "transfer: {body}");
    let (status_replay, body_replay) = post_transfer(rig.a.addr, "mig-21b").await;
    assert_eq!(status_replay, 202);
    assert_eq!(body, body_replay, "the transfer replay is byte-identical");
    let summary = poll_migration(rig.a.addr, "mig-21b", "complete", None).await;
    let expected: Vec<(String, Option<String>)> = [
        ("prepared", None),
        ("precopy", None),
        ("quiesced", None),
        ("barrier_durable", None),
        ("barrier_durable", Some("snapshotting")),
        ("barrier_durable", Some("destroying_vm")),
        ("barrier_durable", Some("demoting")),
        ("barrier_durable", Some("revoking")),
        ("source_revoked", Some("revoking")),
        ("destination_authorized", Some("revoking")),
        ("vm_resumed", Some("revoking")),
        ("complete", None),
    ]
    .into_iter()
    .map(|(state, cut)| (state.to_owned(), cut.map(str::to_owned)))
    .collect();
    assert_eq!(history_pairs(&summary), expected);
}

// ------------------------------------------------------------ row 22

/// Row 22 — one-of-two target promotes failing blocks the VM restore:
/// the witness `GrantSet` is all-or-nothing (both leases minted for
/// the destination), but the second participant's promote is refused
/// and its rollback **releases the lease it cannot take** (never a
/// live lease without a writer), so the drive parks in the D3 window
/// (`IN_DOUBT`, "source revoked; destination grant not yet
/// authorized") with a half-promoted destination — one participant
/// Primary, one Secondary — and the VM restored nowhere. The
/// reconcile re-drives the grant, which replays the journaled
/// failure: with one participant holding no live lease the
/// all-granted fold cannot fire, the stall persists, and the VM is
/// never resumed half-migrated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_22_partial_target_promotion_blocks_restore() {
    let rig = rig_with(&Seeds {
        vols: vec![
            vol("vol-22a", SEED_MINOR, SEED_PORT),
            vol("vol-22b", SECOND_MINOR, SECOND_PORT),
        ],
        vms: vec![vm("vm-22", &["vol-22a", "vol-22b"])],
        ..Seeds::default()
    })
    .await;
    // The destination's promote of the second participant fails (the
    // fake world's injected `drbdadm primary` refusal).
    rig.b
        .core
        .world
        .lock()
        .expect("world")
        .fail_primary_resources
        .insert(resource_of("vol-22b"));

    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-22",
            "vm_id": "vm-22",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-22a", "vol-22b"],
            "expected_generations": [2, 2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");
    let (status, body) = post_transfer(rig.a.addr, "mig-22").await;
    assert_eq!(status, 202, "transfer: {body}");
    poll_migration(
        rig.a.addr,
        "mig-22",
        "in_doubt",
        Some("source revoked; destination grant not yet authorized"),
    )
    .await;
    drive_settled(&rig.a).await;

    // The half-promoted destination: the first participant promoted,
    // the second still Secondary; the VM restored nowhere.
    assert_eq!(
        role_of(&rig.b.core.world, &resource_of("vol-22a")),
        Role::Primary
    );
    assert_eq!(
        role_of(&rig.b.core.world, &resource_of("vol-22b")),
        Role::Secondary
    );
    assert_eq!(
        rig.a.core.vmm.vm_state("vm-22").expect("vm state"),
        VmState::Absent
    );
    assert_eq!(
        rig.b.core.vmm.vm_state("vm-22").expect("vm state"),
        VmState::Absent
    );
    // The witness `GrantSet` landed all-or-nothing (both leases went
    // to the destination), and the failed promote released the lease
    // it could not take: `vol-22a` holds a live writer lease as the
    // promoted Primary, while `vol-22b` — whose `drbdadm primary` was
    // refused — is back to a retired lease (never a live lease
    // without a writer behind it).
    let promoted_view = witness_view(&rig.witness, &volume("vol-22a")).await;
    assert_eq!(promoted_view.holder, Some(host_id(PEER_NODE)));
    assert_eq!(promoted_view.lease_state, LeaseState::Live);
    let refused_view = witness_view(&rig.witness, &volume("vol-22b")).await;
    assert_eq!(refused_view.holder, Some(host_id(PEER_NODE)));
    assert_eq!(
        refused_view.lease_state,
        LeaseState::Revoked,
        "the refused promote released its unusable lease"
    );

    // The reconcile re-drives the authorize step and the journaled
    // grant failure replays: `vol-22b` holds no live lease (its
    // promote keeps refusing), so the all-granted fold cannot fire
    // and the record stays parked at the source-revoked `IN_DOUBT`
    // observation — nothing is ever resumed half-migrated.
    let outcome = rig
        .a
        .handle
        .coordinator()
        .resolve(&migration("mig-22"))
        .await;
    assert!(outcome.is_err(), "the journaled grant failure replays");
    let summary = parked_follow_up(&rig, "mig-22").await;
    assert_eq!(state_name(&summary), "in_doubt", "summary: {summary}");
    assert_eq!(
        summary["in_doubt_detail"]
            .as_str()
            .expect("in-doubt detail"),
        "source revoked; destination grant not yet authorized"
    );
    assert!(
        !history_has_detail(
            &summary,
            "target grants observed: the grant is observed done"
        ),
        "the grant fold cannot fire while a participant holds no live lease"
    );
    // The lease shape survived the re-drive unchanged: the promoted
    // participant keeps its live lease, the refused one stays
    // retired.
    let promoted_view = witness_view(&rig.witness, &volume("vol-22a")).await;
    assert_eq!(promoted_view.holder, Some(host_id(PEER_NODE)));
    assert_eq!(promoted_view.lease_state, LeaseState::Live);
    let refused_view = witness_view(&rig.witness, &volume("vol-22b")).await;
    assert_eq!(refused_view.lease_state, LeaseState::Revoked);
    assert_eq!(
        rig.b.core.vmm.vm_state("vm-22").expect("vm state"),
        VmState::Absent,
        "the VM is never resumed half-migrated"
    );
}

// ------------------------------------------------------------ row 23

/// Row 23 — the restore's `config.json` disk-path rewrite: the source
/// VM held `/dev/drbd11`, the destination's promoted replica presents
/// `/dev/drbd12`; the peer restore-vm maps declared source paths to
/// the promoted device paths and the restored VM lands paused holding
/// exactly the destination's device. (The matching-minors no-op is
/// row 13's path, asserted there through the same real restore.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_23_restore_rewrites_divergent_disk_paths() {
    let rig = rig_with(&Seeds {
        vols: vec![vol("vol-23", SEED_MINOR, SEED_PORT)],
        b_minors: BTreeMap::from([("vol-23".to_owned(), SECOND_MINOR)]),
        vms: vec![vm("vm-23", &["vol-23"])],
        ..Seeds::default()
    })
    .await;
    let mig = migration("mig-23");
    let (status, body) = post_prepare(
        rig.a.addr,
        &serde_json::json!({
            "migration_id": "mig-23",
            "vm_id": "vm-23",
            "target_host": PEER_NODE,
            "volume_ids": ["vol-23"],
            "expected_generations": [2],
        }),
    )
    .await;
    assert_eq!(status, 201, "prepare: {body}");

    // The source's real snapshot artifacts (config.json declares the
    // source device `/dev/drbd11`), then the witness tail.
    let snapshot_dir = rig.snapshot_root.join("vm-23");
    rig.a.core.vmm.pause("vm-23").expect("pause");
    rig.a
        .core
        .vmm
        .snapshot("vm-23", &snapshot_dir)
        .expect("snapshot");
    record_barrier(&rig.witness, &volume("vol-23"), &mig).await;
    revoke_set(&rig.witness, &volume("vol-23"), &mig).await;

    // The destination's grant promotes ITS replica — minor 12 — and
    // answers with that device path.
    let (status, body) = peer_call(
        "POST",
        rig.b.addr,
        "/v2/internal/peer/grant",
        &serde_json::json!({"migration_id": "mig-23"}).to_string(),
    )
    .await;
    assert_eq!(status, 200, "grant: {body}");
    let device_path = body_json(&body)["grants"][0]["device_path"]
        .as_str()
        .expect("device path")
        .to_owned();
    assert_eq!(device_path, "/dev/drbd12", "the destination's minor");

    // The restore maps the declared source path onto the promoted
    // destination device; the restored VM lands paused holding it.
    let (status, body) = peer_call(
        "POST",
        rig.b.addr,
        "/v2/internal/peer/restore-vm",
        &serde_json::json!({
            "migration_id": "mig-23",
            "snapshot_dir": snapshot_dir.to_string_lossy(),
            "disks": [{"declared_path": "/dev/drbd11", "device_path": device_path}],
            "resume": false,
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, 200, "restore-vm: {body}");
    assert_eq!(body_json(&body)["vm_state"], "paused", "{body}");
    assert_eq!(
        rig.b.core.vmm.vm_state("vm-23").expect("vm state"),
        VmState::Paused
    );
    assert_eq!(
        rig.b.core.vmm.vm_devices("vm-23").expect("vm devices"),
        vec!["/dev/drbd12".to_owned()]
    );
    assert_eq!(
        open_minors(&rig.b.core.world),
        BTreeSet::from([SECOND_MINOR])
    );
}
