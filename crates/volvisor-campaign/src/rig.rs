//! The campaign rig (P5 plan §3.3/§4): two full daemons, one
//! loopback witness, two linked fake DRBD worlds, two wired fake
//! VMMs and the shared frozen clock — composed from the exported
//! constructors exactly as `crates/volvisord/tests/migration_e2e.rs`
//! composes them (the composition pattern, not the file).
//!
//! What this module adds over the e2e fixture is the **task-group
//! kill model** (§3.3): every daemon runs under this rig's
//! supervisor as one group — the serve task, a rig-side lease
//! renewal loop (production's `spawn_renewal_task` is private, so
//! the loop is re-implemented here over the public
//! [`DrbdProvider::renew_leases`] surface), the migration retry
//! task (`spawn_migration_retry_task`, the recovery engine) and the
//! transfer drive tasks (the `MigrationHandle` registry's
//! `abort_drive_tasks`). The journal-append crash hook
//! (`volvisor_api::crash`) fires into exactly this group, so a kill
//! aborts the whole daemon — never a graceful drain — and a restart
//! re-opens everything from the durable artifacts, which is also
//! the proof the journal `flock` came free (the restart's
//! `Journal::open` succeeds only when every `AppState` `Arc` of the
//! killed daemon has unwound).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{MutexGuard, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use volvisor_api::{AppState, CrashHooks, KillSwitch, router};
use volvisor_drbd::AuthorityContext;
use volvisor_drbd::provider::{DrbdProvider, DrbdProviderConfig, resource_name_for};
use volvisor_drbd::report::Role;
use volvisor_drbd_testkit::{
    FakeDrbd, NODE, PEER_NODE, SEED_MINOR, SEED_PORT, config_for, config_for_peer, fixture,
    leak_tempdir, link_replication_peers, seed_peer_volume, seed_volume_with_identity,
};
use volvisor_handoff::Clock;
use volvisor_journal::Journal;
use volvisor_provider::{
    AdoptionSurface, DeviceHook, FakeVmm, HandoffSurface, VmmController, VolumeProvider,
};
use volvisor_types::request::AccessModeRequest;
use volvisor_types::request::AttachVolumeRequest;
use volvisor_types::{AttachmentId, HostId, OperationId, VolumeId};
use volvisor_witness::client::{HttpWitnessConnection, WitnessConnection};
use volvisor_witness::registry::{WitnessCore, WitnessCoreConfig};
use volvisor_witness::server::WitnessServerState;
use volvisord::handoff::{
    HttpPeerClient, MigrationHandle, ParticipantFacts, PeerClient, migration_records_dir,
    peer_preparations_dir, spawn_migration_retry_task, wire_migration,
};

// ---------------------------------------------------------- constants

/// The admin bearer token both daemons share (fail-closed auth on
/// every mutating route).
pub const ADMIN_TOKEN: &str = "campaign-admin-token";
/// The daemon-to-daemon credential guarding `/v2/internal/peer/*`
/// (deliberately distinct from every witness credential, plan §6).
pub const PEER_TOKEN: &str = "campaign-peer-token";
/// The witness's legacy admin (read-only inspect) credential.
pub const WITNESS_ADMIN_TOKEN: &str = "campaign-witness-admin";
/// `node-a`'s W8 witness credential (its mutations).
pub const NODE_TOKEN: &str = "campaign-node-a-witness";
/// `node-b`'s W8 witness credential (its mutations).
pub const PEER_NODE_TOKEN: &str = "campaign-node-b-witness";
/// Deterministic witness knobs (the kit precedent): ttl 100 s,
/// grace 5 s, suspend budget 5 s.
pub const START: u64 = 1_000;
/// See [`START`].
pub const TTL: u64 = 100;
/// One gibibyte (extent-aligned under the fixture's 4-MiB extents;
/// 262 144 logical blocks of device surface — every scenario's
/// write count stays far below, so block indices are unique).
pub const GIB: u64 = 1 << 30;
/// The writer renewal cadence (well under ttl/2).
pub const INTERVAL: u64 = 20;
/// The rig-side renewal loop's tick (production's shape:
/// `renew_leases` itself throttles actual renewals to `INTERVAL`).
const RENEWAL_TICK: Duration = Duration::from_secs(1);
/// Bounded polling: real timeouts, small steps (house style). The
/// bound is the failure-detector ceiling, not a sleep — the happy
/// path returns at the first observation. 15s gives a 3× margin over
/// the retry task's ~5s tick on a loaded CI box (a kill-and-recover
/// row must cross one tick to see the re-drive).
pub const POLL_BOUND: Duration = Duration::from_secs(15);
/// See [`POLL_BOUND`].
pub const POLL_STEP: Duration = Duration::from_millis(50);

// ------------------------------------------------------- the panic hook

/// Install this test binary's crash-injection filter (once): the
/// journal-append hook terminates the firing request with a
/// `volvisor_api::CRASH_PANIC_PREFIX` payload — an injected kill,
/// not a failure — so those panics print nothing while every real
/// panic still reaches the previous hook.
static PANIC_HOOK: std::sync::Once = std::sync::Once::new();

fn install_panic_filter() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let injected = info
                .payload()
                .downcast_ref::<String>()
                .is_some_and(|payload| payload.starts_with(volvisor_api::CRASH_PANIC_PREFIX));
            if !injected {
                previous(info);
            }
        }));
    });
}

// ------------------------------------------------------- HTTP client

/// One request's outcome: the daemon answered, or the connection
/// died without a reply — the observed shape of a kill firing
/// mid-handler (§3.3: the only in-flight request at kill time is
/// the firing one, and it never gets a response).
#[derive(Debug)]
pub enum Reply {
    /// The daemon answered `(status, body)`.
    Response(u16, String),
    /// The connection died without a status line — no reply landed.
    Died,
}

impl Reply {
    /// Unwrap a served reply, panicking with `context` on a dead
    /// connection (a kill the scenario did not arm is a rig bug).
    pub fn served(self, context: &str) -> (u16, String) {
        match self {
            Reply::Response(status, body) => (status, body),
            Reply::Died => panic!("{context}: the connection died without a reply"),
        }
    }
}

/// Hand-rolled minimal HTTP/1.1 client (the e2e precedent: no
/// client dependency by design). One fresh connection per request
/// (`connection: close`), so no server-side task outlives a
/// scenario's driving (§3.3's one-request-at-a-time discipline).
pub async fn http(
    method: &str,
    addr: SocketAddr,
    path: &str,
    body: Option<&str>,
    bearer: Option<&str>,
) -> Reply {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return Reply::Died;
    };
    let body = body.unwrap_or("");
    let auth_header = bearer
        .map(|token| format!("authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n{auth_header}connection: close\r\n\r\n{body}",
        body.len()
    );
    if stream.write_all(request.as_bytes()).await.is_err() {
        return Reply::Died;
    }
    let mut response = Vec::new();
    if stream.read_to_end(&mut response).await.is_err() {
        return Reply::Died;
    }
    let text = String::from_utf8_lossy(&response).into_owned();
    // No parseable status line means no reply landed: a killed
    // request closes the connection with nothing (or a partial
    // fragment) served.
    let Some(status) = text
        .split_whitespace()
        .nth(1)
        .and_then(|word| word.parse::<u16>().ok())
    else {
        return Reply::Died;
    };
    let body = text
        .find("\r\n\r\n")
        .map(|index| text[index + 4..].to_owned())
        .unwrap_or_default();
    Reply::Response(status, body)
}

/// One admin-authenticated call against a daemon.
pub async fn admin(method: &str, addr: SocketAddr, path: &str, body: Option<&str>) -> Reply {
    http(method, addr, path, body, Some(ADMIN_TOKEN)).await
}

/// Parse a JSON response body.
pub fn body_json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("valid JSON body")
}

/// The observed state name of a migration summary: unit variants
/// serialize as plain strings, `in_doubt`/`aborted` as single-key
/// objects.
pub fn state_name(summary: &serde_json::Value) -> String {
    match &summary["state"] {
        serde_json::Value::String(name) => name.clone(),
        serde_json::Value::Object(map) => map.keys().next().cloned().unwrap_or_default(),
        _ => String::new(),
    }
}

// ------------------------------------------------------------- witness

/// The loopback witness: one durable directory, one frozen injected
/// clock, per-host W8 credentials (the e2e composition) — plus the
/// campaign's kill model (§3.3 applied to the witness itself): the
/// serve task is abortable as one group, a restart re-opens the
/// core from the journal (the W3 replay/roll-forward is the
/// recovery), and the rig-owned store-save seam (§3.1's witness
/// mid-save variant) survives every restart so an arm cannot leak
/// or vanish across one.
pub struct WitnessHandle {
    /// The witness's serving address.
    pub addr: SocketAddr,
    /// The shared frozen clock.
    pub clock: Arc<AtomicU64>,
    /// The durable witness directory (journal survives; the evidence
    /// emitter snapshots it).
    pub dir: PathBuf,
    /// The witness's mid-save crash seam (P5 plan §3.1): kills
    /// inside the witness's own durable mutations. The rig arms
    /// `(mutation kind, point)` pairs; the instance is shared by
    /// every core this handle ever serves.
    pub crash: Arc<volvisor_types::crash::StoreCrashHooks>,
    /// The live serve task's slot (taken at kill or stop) — an
    /// `Arc` so the kill switch's closure can hold a `Weak` to it.
    serve: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Set by the witness kill switch; the scenarios poll it.
    killed: Arc<AtomicBool>,
}

impl WitnessHandle {
    /// Spawn a fresh witness on a free loopback port.
    pub async fn spawn(clock: Arc<AtomicU64>) -> Self {
        let dir = leak_tempdir();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind witness");
        let addr = listener.local_addr().expect("witness local addr");
        let mut witness = Self {
            addr,
            clock,
            dir,
            crash: Arc::new(volvisor_types::crash::StoreCrashHooks::new()),
            serve: Arc::new(Mutex::new(None)),
            killed: Arc::new(AtomicBool::new(false)),
        };
        witness.install_kill_switch();
        witness.launch(listener).await;
        witness
    }

    /// Register the witness kill switch (once; the switch is
    /// permanent): mark the kill and abort the serve task. The
    /// firing handler task dies with its own panic (the seam's
    /// consult), so the switch only needs to stop the listener. The
    /// closure holds a `Weak` to the serve slot (a strong closure
    /// would cycle and leak the handle; an upgrade failure means
    /// the rig already dropped the witness).
    fn install_kill_switch(&self) {
        let killed = Arc::clone(&self.killed);
        let weak_serve = Arc::downgrade(&self.serve);
        self.crash.set_kill_switch(Arc::new(move || {
            killed.store(true, Ordering::SeqCst);
            if let Some(serve_slot) = weak_serve.upgrade() {
                if let Some(serve) = take_slot(&serve_slot) {
                    serve.abort();
                }
            }
        }));
    }

    /// Serve the (re-loaded) core on `listener`.
    async fn launch(&mut self, listener: TcpListener) {
        let mut core = open_witness_core(&self.dir).await;
        core.attach_store_crash_hooks(Arc::clone(&self.crash));
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
        // The spawn hops through the blocking pool (the same
        // discipline as `volvisor-witness`'s blocking adapter, for
        // the same reason): a task spawned directly from a runtime
        // *worker* lands in that worker's LIFO slot, which no other
        // worker can steal, so a rig constructed from a spawned
        // task (the matrix cells) would block its worker in the
        // seeding's synchronous witness calls before the serve task
        // is ever polled — starving the very witness it is seeding.
        // A blocking-pool thread holds no worker core, so the spawn
        // lands on the stealable inject queue and a parked worker
        // picks the serve task up immediately.
        let handle = tokio::runtime::Handle::current();
        let spawn = handle.clone();
        let serve = handle
            .spawn_blocking(move || {
                spawn.spawn(async move {
                    axum::serve(listener, app).await.expect("witness serves");
                })
            })
            .await
            .expect("witness serve spawn hop");
        *lock_slot(&self.serve) = Some(serve);
    }

    /// Whether the witness kill switch has fired.
    pub fn is_killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }

    /// Stop the witness (the outage window: kill-free from the
    /// seam's point of view — the listener goes away, the durable
    /// journal survives) and await the serve task's death so the
    /// journal `flock` frees for the restart.
    pub async fn stop(&mut self) {
        if let Some(serve) = take_slot(&self.serve) {
            serve.abort();
            let _ = serve.await;
        }
        self.await_lingering_handlers().await;
    }

    /// Restart the witness on the same address from the durable
    /// journal (the W3 replay/roll-forward is the recovery): rebind,
    /// re-open the core, re-attach the shared crash seam, re-serve.
    /// The kill flag resets with the relaunch (the comprehensive
    /// review's S3, same rationale as [`Daemon::restart`]): the old
    /// serve task is awaited dead first, so no late kill can land,
    /// and a scenario polling [`WitnessHandle::is_killed`] after a
    /// restart waits for the NEW kill, never reads the old one's
    /// sticky residue (row 14 kills the witness in every F3/F4
    /// cycle).
    pub async fn restart(&mut self) {
        self.stop().await;
        self.killed.store(false, Ordering::SeqCst);
        let listener = TcpListener::bind(self.addr).await.expect("rebind witness");
        self.launch(listener).await;
    }

    /// Bounded wait for the aborted serve future's detached
    /// connection tasks to drop the state `Arc` (the journal
    /// `flock` frees asynchronously from the restarter's view —
    /// the same race [`open_journal`] absorbs for the daemons). The
    /// probe opens the raw journal only: it must NOT run the
    /// witness core's W3b roll-forward — that semantic act belongs
    /// to the restart's own `WitnessCore::open`.
    async fn await_lingering_handlers(&self) {
        let deadline = tokio::time::Instant::now() + POLL_BOUND;
        loop {
            if Journal::open(&self.dir).is_ok() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "witness journal open (flock) did not free within {} s",
                POLL_BOUND.as_secs()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The legacy admin (inspect-only) client.
    pub fn admin_client(&self) -> HttpWitnessConnection {
        HttpWitnessConnection::new(
            format!("http://{}", self.addr),
            Some(WITNESS_ADMIN_TOKEN.to_owned()),
            Duration::from_secs(5),
        )
    }

    /// The client presenting `host`'s W8 credential.
    pub fn host_client(&self, host: &str) -> HttpWitnessConnection {
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

/// Open a witness core with the bounded flock retry a restart needs
/// (the killed witness's journal frees asynchronously — see
/// [`open_journal`] for the daemon-side twin).
async fn open_witness_core(dir: &Path) -> WitnessCore {
    let deadline = tokio::time::Instant::now() + POLL_BOUND;
    loop {
        match WitnessCore::open(
            dir,
            WitnessCoreConfig {
                lease_ttl_secs: TTL,
                lease_grace_secs: 5,
                suspend_budget_secs: 5,
            },
        ) {
            Ok(core) => return core,
            Err(error) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "witness core open (flock) did not free within {} s: {error}",
                    POLL_BOUND.as_secs()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

/// The witness's view of one volume (admin credential — an
/// operator's read-only observation surface).
pub async fn witness_view(
    witness: &WitnessHandle,
    vol: &VolumeId,
) -> volvisor_types::AuthorityView {
    witness
        .admin_client()
        .inspect(vol)
        .await
        .expect("witness view")
}

// ------------------------------------------------------------- the VMMs

/// The per-VM device map the VMM's hook maintains (VM id → the
/// device paths it holds).
type DeviceMap = Arc<Mutex<BTreeMap<String, Vec<String>>>>;

/// Build one wired fake VMM (the e2e composition): the device hook
/// keeps a per-VM map of the devices it holds and recomputes the
/// host world's `open_devices` — a present VM holds its devices for
/// its whole lifetime; destroy is the only release.
fn wired_vmm(snapshot_root: &Path, world: &Arc<Mutex<FakeDrbd>>) -> (Arc<FakeVmm>, DeviceMap) {
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

// ------------------------------------------------------------- the daemon

/// One daemon's task group (§3.3): everything the kill aborts as one
/// unit. The serve task, the rig-side renewal loop and the retry
/// task are `JoinHandle`s the supervisor owns; the drive tasks live
/// in the handle's registry (`abort_drive_tasks`).
struct TaskGroup {
    serve: JoinHandle<()>,
    renewal: JoinHandle<()>,
    retry: JoinHandle<()>,
    handle: Arc<MigrationHandle>,
}

/// Abort the group's tasks (a kill's first half — the second half,
/// the firing request's own death, happens inside the crash hook).
fn abort_group(group: &TaskGroup) {
    group.serve.abort();
    group.renewal.abort();
    group.retry.abort();
    // The drive tasks: the registry abort needs an await, and the
    // kill switch fires from inside a request task (under the
    // journal lock), so the abort runs as its own spawned task —
    // the same task-group semantics, one poll later. The supervisor
    // awaits the drain in [`Daemon::await_death`].
    let handle = Arc::clone(&group.handle);
    tokio::spawn(async move {
        handle.abort_drive_tasks().await;
    });
}

/// Everything one daemon is made of, immutable across restarts (the
/// e2e `HostCore` shape) plus the campaign's kill plumbing: the
/// crash hook instance, the current task group's slot and the
/// killed flag the scenarios poll.
pub struct DaemonCore {
    /// This host's identity (`node-a`/`node-b`).
    pub name: String,
    /// This host's W8 witness credential.
    pub witness_token: String,
    /// The provider's durable state file.
    pub state_path: PathBuf,
    /// The simulated DRBD world of this host.
    pub world: Arc<Mutex<FakeDrbd>>,
    /// The daemon's journal directory (flock-scoped; freed when the
    /// serve task drops).
    pub journal_dir: PathBuf,
    /// This host's verified provider configuration (the input each
    /// provider-factory pass re-loads state under). The
    /// provider itself is **per-launch** (see the `make_provider`
    /// method).
    pub provider_config: DrbdProviderConfig,
    /// This host's fake VMM.
    pub vmm: Arc<FakeVmm>,
    /// The OTHER daemon's address (the peer client's target).
    pub peer_addr: SocketAddr,
    /// The witness's address (the connection and probe target).
    pub witness_addr: SocketAddr,
    /// The coordinator's STAMP clock ([`Rig::stamp_clock`]'s view):
    /// pure timestamps — the migration record's history entries,
    /// `updated_at`, the measured cut duration — sharing the
    /// writer's per-ack tick domain so the §2.3 rule-1 boundary
    /// cross-check is comparable (and live: the clock advances once
    /// per acknowledged write, with no freeze). Never read for
    /// lease arithmetic.
    pub clock: Clock,
    /// The AUTHORITY clock ([`Rig::clock`]'s view — the lease
    /// domain): the witness grant/renewal deadlines, the W5
    /// response-anchored local deadlines and the provider's
    /// W5/W7 enforcement all read THIS clock, so the writer's
    /// stamp ticks can never expire a lease as a rig artifact
    /// (the stage-C lesson the old `CLOCK_ADVANCE_CAP` encoded;
    /// the comprehensive review's S1 keeps the property
    /// structurally — the split — instead of by a cap).
    pub authority_clock: Clock,
    /// The shared snapshot root (one directory per VM).
    pub snapshot_root: PathBuf,
    /// This daemon's journal-append crash hook (§3.1) — the armed
    /// table the scenario aims and the kill switch fires into.
    pub crash: Arc<CrashHooks>,
    /// The live task group's slot (filled at launch, taken at kill
    /// or stop).
    group: Mutex<Option<TaskGroup>>,
    /// The killed group's slot (the kill switch moves it here for
    /// the supervisor to await).
    dead_group: Mutex<Option<TaskGroup>>,
    /// Set by the kill switch; the scenarios poll it to learn a
    /// fired kill landed.
    killed: Arc<AtomicBool>,
}

impl DaemonCore {
    /// Build a FRESH provider from the durable artifacts (the
    /// honest restart shape): the closure-mode runner re-answers the
    /// startup verification over the same world, the state file is
    /// re-loaded from disk and reconciled. Stage B's store-save
    /// kills (§3.1) fire *inside* the provider's state lock, so a
    /// restart must not reuse the poisoned instance — and a real
    /// process restart never does: the new process constructs a new
    /// provider over the same media and the same state file, which
    /// is exactly this method.
    fn make_provider(&self) -> Arc<DrbdProvider> {
        Arc::new(
            DrbdProvider::with_authority(
                FakeDrbd::runner(&self.world),
                self.provider_config.clone(),
                self.state_path.clone(),
                authority_for(
                    self.witness_addr,
                    &self.name,
                    &self.witness_token,
                    &self.authority_clock,
                ),
            )
            .expect("provider construction over the durable artifacts"),
        )
    }

    /// Take whichever group slot holds the daemon's tasks.
    fn take_any_group(self: &Arc<Self>) -> Option<TaskGroup> {
        take_slot(&self.dead_group).or_else(|| take_slot(&self.group))
    }

    /// The kill switch (§3.3): mark the kill, abort the whole group,
    /// park it for the supervisor to await. Firing order inside a
    /// crash hook guarantees the tasks stop before the rig can drive
    /// the daemon again. The closure holds `Weak` handles, not
    /// `Arc`s: a strong closure would cycle (`DaemonCore` → its
    /// crash state → the closure → `DaemonCore`) and leak the pair
    /// together when the rig drops the daemon; the `killed` flag is
    /// a plain `Arc<AtomicBool>` (it must survive to the
    /// supervisor's post-mortem reads even if the core went away).
    /// An upgrade failure means the rig already dropped the daemon —
    /// there is nothing left to kill and nothing driving it.
    fn kill_switch(self: &Arc<Self>) -> KillSwitch {
        let weak = Arc::downgrade(self);
        let killed = Arc::clone(&self.killed);
        Arc::new(move || {
            killed.store(true, Ordering::SeqCst);
            let Some(core) = weak.upgrade() else {
                return;
            };
            let Some(group) = take_slot(&core.group) else {
                return;
            };
            abort_group(&group);
            *lock_slot(&core.dead_group) = Some(group);
        })
    }

    /// Register the kill switch this core's journal-append crash
    /// hook fires into (called once at construction; the switch is
    /// permanent). Each launch separately registers the SAME switch
    /// on the per-launch store-save seams (the provider's and the
    /// migration store's — both fresh instances a restart rebuilds).
    fn install_kill_switch(self: &Arc<Self>) {
        self.crash.set_kill_switch(self.kill_switch());
    }
}

/// Lock a group slot, recovering from the poison a firing crash
/// hook can leave behind (a poisoned slot holds a group, not a
/// mystery).
fn lock_slot<T>(slot: &Mutex<Option<T>>) -> MutexGuard<'_, Option<T>> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Take a group slot's content (empty → `None`).
fn take_slot<T>(slot: &Mutex<Option<T>>) -> Option<T> {
    lock_slot(slot).take()
}

/// Everything immutable one daemon needs, built once and wired with
/// its kill switch (the switch is permanent; each launch refreshes
/// the group slot it aborts). Construct the struct and call
/// [`DaemonCore::install_kill_switch`] — the constructor-as-struct
/// keeps the twelve-fixture field list readable.
fn daemon_core(core: DaemonCore) -> Arc<DaemonCore> {
    let core = Arc::new(core);
    core.install_kill_switch();
    core
}

/// One launched daemon: its address, its migration handle and its
/// per-launch provider. The serve, renewal and retry tasks live in
/// the core's group slot — the rig deliberately holds NO `AppState`
/// clone, so a kill's unwinding releases the journal `flock` (the
/// restart's `Journal::open` is the proof).
pub struct Daemon {
    /// The daemon's immutable core (shared across restarts).
    pub core: Arc<DaemonCore>,
    /// The daemon's serving address.
    pub addr: SocketAddr,
    /// The daemon's migration handle (the retry task's and the
    /// drive registry's owner).
    pub handle: Arc<MigrationHandle>,
    /// This launch's provider (fresh from the durable artifacts at
    /// every launch — see the core's `make_provider` method; the
    /// store-save seams a scenario arms hang off it).
    pub provider: Arc<DrbdProvider>,
}

impl Daemon {
    /// Build and serve one daemon on `listener` (the e2e
    /// `Host::launch` composition, plus the retry task, the renewal
    /// loop and the crash-hook wiring the campaign needs).
    pub async fn launch(core: Arc<DaemonCore>, listener: TcpListener) -> Daemon {
        let addr = listener.local_addr().expect("daemon local addr");
        // The per-launch provider (§3.3's restart shape): state
        // re-loaded from disk, reconciled against the world, with
        // the daemon's kill switch registered on its store-save
        // seam — a store kill fires into exactly this group.
        let provider = core.make_provider();
        provider
            .store_crash_hooks()
            .set_kill_switch(core.kill_switch());
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
        let facts_provider = Arc::clone(&provider);
        let facts: ParticipantFacts = Arc::new(
            move |volume_id: &VolumeId, vm_id: &str, expected_generation: u64| {
                facts_provider.migration_participant_facts(volume_id, vm_id, expected_generation)
            },
        );
        let (handle, peer_ctx) = wire_migration(
            HostId::new(core.name.as_str()).expect("valid host id"),
            witness,
            core.witness_addr,
            Arc::clone(&core.vmm) as Arc<dyn VmmController>,
            Arc::clone(&provider) as Arc<dyn HandoffSurface>,
            Arc::clone(&provider) as Arc<dyn VolumeProvider>,
            Arc::new(peer) as Arc<dyn PeerClient>,
            facts,
            core.snapshot_root.clone(),
            migration_records_dir(&core.journal_dir),
            peer_preparations_dir(&core.journal_dir),
            Arc::clone(&core.clock),
        )
        .expect("wire migration");
        // The migration store's store-save seam is fresh per
        // `wire_migration` call: register the same kill switch so a
        // record-save kill fires into the same group.
        handle
            .store_crash_hooks()
            .set_kill_switch(core.kill_switch());

        // The recovery engine (§3.3's group member): the production
        // retry task, spawned abortable and part of the kill group.
        // Its startup pass re-drives every non-terminal record — the
        // restart's recovery the scenarios poll through the public
        // observation route.
        let retry = spawn_migration_retry_task(Arc::clone(&handle));

        // The rig-side renewal loop (§3.3: production's
        // `spawn_renewal_task` is private, so the loop is
        // re-implemented over the public `renew_leases` surface —
        // the same tick shape, every outcome a structured event,
        // never a crash).
        let renewal = spawn_renewal_loop(Arc::clone(&provider));

        let state = AppState::new(
            Arc::clone(&provider) as Arc<dyn VolumeProvider>,
            None,
            open_journal(&core.journal_dir).await,
            Some(ADMIN_TOKEN.to_owned()),
        )
        .with_adoption(Arc::clone(&provider) as Arc<dyn AdoptionSurface>)
        .with_handoff(Arc::clone(&provider) as Arc<dyn HandoffSurface>)
        .with_migration(Arc::clone(&handle) as Arc<dyn volvisor_handoff::MigrationSurface>)
        .with_peer_routes(Some(PEER_TOKEN.to_owned()), peer_ctx)
        .with_crash_hooks(Arc::clone(&core.crash));
        let app = router(Arc::new(state), 1 << 20);
        let serve = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("daemon serves");
        });

        // Park the group where the kill switch (and only it) finds
        // it — launching over a leftover group would orphan tasks.
        {
            let mut group = lock_slot(&core.group);
            assert!(
                group.is_none(),
                "launching over a live group (the previous daemon was never awaited)"
            );
            *group = Some(TaskGroup {
                serve,
                renewal,
                retry,
                handle: Arc::clone(&handle),
            });
        }
        Daemon {
            core,
            addr,
            handle,
            provider,
        }
    }

    /// Whether this daemon's kill switch has fired (set before the
    /// group abort, so a `true` means the tasks are stopping).
    pub fn is_killed(&self) -> bool {
        self.core.killed.load(Ordering::SeqCst)
    }

    /// Await the death of whichever group this daemon runs under
    /// (killed or gracefully stopped): every aborted task is
    /// dropped exactly here, the drive tasks drain, and with them
    /// every `AppState` `Arc` unwinds — the journal `flock` frees.
    async fn await_death(&self) {
        if let Some(group) = self.core.take_any_group() {
            let _ = group.serve.await;
            let _ = group.renewal.await;
            let _ = group.retry.await;
            group.handle.abort_drive_tasks().await;
        }
    }

    /// Kill-free teardown (scenario end / a graceful stop): abort
    /// the group, await the death. The durable artifacts survive.
    pub async fn stop(&mut self) {
        if let Some(group) = take_slot(&self.core.group) {
            abort_group(&group);
            *lock_slot(&self.core.dead_group) = Some(group);
        }
        self.await_death().await;
    }

    /// Re-serve the SAME durable directories on the SAME address
    /// after a kill OR a graceful stop (§3.3's restart): a new
    /// journal handle, a new migration store (records re-loaded from
    /// disk), a fresh migration handle, a fresh retry task whose
    /// startup pass is the recovery. `stop` first: a killed daemon's
    /// group is already aborted and parked (the kill switch's own
    /// act), while a HEALTHY daemon's group is still live — awaiting
    /// it without the abort would wait forever, so the restart of a
    /// healthy daemon (the operator's re-drive action) aborts and
    /// parks its group exactly like a kill would have. The
    /// `Journal::open` inside the launch is the flock-freedom proof
    /// — it succeeds only when the old daemon's last `AppState`
    /// `Arc` unwound.
    pub async fn restart(&mut self) {
        self.stop().await;
        // The kill flag is per-LAUNCH state, not per-daemon state
        // (the comprehensive review's S3): the old launch's group is
        // fully dead — awaited above, so no late abort can land —
        // and the flag resets before the new launch serves. A
        // sticky flag made `is_killed()` vacuously true from the
        // second kill onward (row 14 kills the same daemon every
        // 5th cycle), which could mask a second kill that never
        // armed or fired. `Reply::Died` remains the primary kill
        // evidence; the flag is the polling convenience.
        self.core.killed.store(false, Ordering::SeqCst);
        let listener = TcpListener::bind(self.addr).await.expect("rebind daemon");
        let core = Arc::clone(&self.core);
        *self = Daemon::launch(core, listener).await;
    }
}

/// The rig-side renewal loop (§3.3: production's
/// `spawn_renewal_task` is private, so the loop is re-implemented
/// over the public [`DrbdProvider::renew_leases`] surface — the
/// same tick shape, every outcome a structured event, never a
/// crash; a dead witness or a killed daemon surfaces as an error
/// event the loop outlives).
fn spawn_renewal_loop(provider: Arc<DrbdProvider>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(RENEWAL_TICK).await;
            match provider.renew_leases() {
                Ok(report) => {
                    for renewed in &report.renewed {
                        tracing::info!(
                            kind = "renew_lease",
                            volume_id = %renewed,
                            "campaign renewal pass"
                        );
                    }
                }
                Err(error) => {
                    tracing::error!(
                        kind = "renew_leases",
                        error = %error,
                        "campaign renewal pass failed"
                    );
                }
            }
        }
    })
}

/// Open a daemon's journal with a bounded retry: a killed daemon's
/// `flock` frees asynchronously from the restarter's view (the
/// aborted serve future's detached connection tasks close within
/// their own poll — axum 0.8 ties their graceful close to the serve
/// future's drop), exactly as a real process's fd release races its
/// restarter. The bound is generous, the step is small.
async fn open_journal(dir: &Path) -> Journal {
    let deadline = tokio::time::Instant::now() + POLL_BOUND;
    loop {
        match Journal::open(dir) {
            Ok(journal) => return journal,
            Err(error) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "journal open (flock) did not free within {} s: {error} — a killed \
                     daemon's AppState Arcs leaked",
                    POLL_BOUND.as_secs()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

// -------------------------------------------------------------- the rig

/// The two-daemon campaign fixture: both daemons under the
/// supervisor's task-group kill model, the witness, the linked
/// worlds (content replication is real across them — §2.1) and the
/// scenario's single VM and volume.
pub struct Rig {
    /// The source daemon (`node-a`).
    pub a: Daemon,
    /// The destination daemon (`node-b`).
    pub b: Daemon,
    /// The loopback witness.
    pub witness: WitnessHandle,
    /// The frozen LEASE clock (witness, authorities, the provider's
    /// W5/W7 enforcement — the lease domain): the writer NEVER
    /// advances it (a guest I/O tick is not a second of lease
    /// time); the scenarios advance it deliberately (lease lapses,
    /// fence windows), and nothing else moves it.
    pub clock: Arc<AtomicU64>,
    /// The monotonic STAMP clock (the comprehensive review's S1):
    /// the writer advances it ONE tick per acknowledged write,
    /// unbounded, and the coordinator's history stamps read it —
    /// the two sides of the §2.3 rule-1 boundary cross-check share
    /// it, so the check stays live for the whole scenario. The OLD
    /// design let the writer advance the lease clock under a
    /// `CLOCK_ADVANCE_CAP`, which froze both sides at the same
    /// value past the cap and made `boundary_skew_ticks: 0` a
    /// tautology (an early barrier past the freeze was invisible).
    /// Nothing lease-relevant reads this clock: a stamp tick only
    /// ORDERS events, it never expires a lease (the stage-C lesson
    /// the old cap encoded, preserved structurally by the split).
    pub stamp_clock: Arc<AtomicU64>,
    /// The shared snapshot root.
    pub snapshot_root: PathBuf,
    /// The scenario's VM id (created and started on the source).
    pub vm: String,
    /// The scenario's volume id (identity-seeded on the source,
    /// peer-seeded on the destination, attached to the VM) — the
    /// FIRST of [`Rig::volumes`] (the single-volume scenarios' name).
    pub volume: String,
    /// Every participating volume id, in attach order (the
    /// multi-volume cut's participant set, §9 row 12).
    pub volumes: Vec<String>,
    /// The source host's fixture directory (the survivor daemon of
    /// [`Rig::launch_source_survivor`] boots over it).
    pub source_base: PathBuf,
    /// The destination host's fixture directory.
    pub target_base: PathBuf,
    /// The source's fake VMM (the writer's guest-I/O model reads
    /// its VM state).
    pub vmm_a: Arc<FakeVmm>,
    /// The source's world (the writer's device lives here).
    pub world_a: Arc<Mutex<FakeDrbd>>,
    /// The destination's world (the oracle's destination reads).
    pub world_b: Arc<Mutex<FakeDrbd>>,
}

/// A single-writer attach request for `vm` on `host` (the e2e
/// shape: generation 1 at attach time).
fn attach_req(volume_id: &str, vm: &str, host: &str) -> AttachVolumeRequest {
    AttachVolumeRequest {
        api_version: "volvisor.volume.v2".to_owned(),
        operation_id: OperationId::new(format!("op-attach-{volume_id}")).expect("valid id"),
        vm_id: vm.to_owned(),
        host_id: HostId::new(host).expect("valid host id"),
        attachment_id: AttachmentId::new(format!("att-{volume_id}")).expect("valid id"),
        expected_volume_generation: 1,
        access_mode: AccessModeRequest::SingleWriter,
        requested_frontend: None,
    }
}

/// A witness-managed authority for `host` over a coordinator clock
/// (the e2e composition; the clock is the rig's shared frozen
/// closure, so the per-launch provider factory can rebuild the
/// authority without holding the `Arc<AtomicU64>` itself).
fn authority_for(
    witness_addr: SocketAddr,
    host: &str,
    token: &str,
    clock: &Clock,
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
    AuthorityContext::new(
        connection,
        HostId::new(host).expect("valid host id"),
        INTERVAL,
        Arc::clone(clock),
    )
    .expect("authority context")
}

/// A clock closure over one of the rig's shared counters (the
/// coordinator's stamp view or the authority's lease view — see
/// [`Rig::stamp_clock`] / [`Rig::clock`] for which is which and
/// why they are different counters).
fn clock_of(shared: &Arc<AtomicU64>) -> Clock {
    let clock = Arc::clone(shared);
    Arc::new(move || clock.load(Ordering::SeqCst))
}

/// The observed role of one resource in a world (a world-level
/// observation the G5 checks read).
pub fn role_of(world: &Arc<Mutex<FakeDrbd>>, resource: &str) -> Role {
    world
        .lock()
        .expect("world")
        .resources
        .get(resource)
        .expect("resource exists")
        .role
}

/// The source's writer shape (setup, not scenario driving — the e2e
/// composition boundary): register every volume, attach each
/// single-writer (generation 1; the witness grants epoch 1 to node-a
/// on the promote), then create and start the VM holding every
/// volume's device.
async fn seed_source_workload(
    provider: &Arc<DrbdProvider>,
    vmm: &Arc<FakeVmm>,
    volume_ids: &[String],
    vm: &str,
) {
    for volume_id in volume_ids {
        let volume = VolumeId::new(volume_id).expect("valid volume id");
        provider.register_volume(&volume, None).expect("register");
        provider
            .attach_volume(&volume, &attach_req(volume_id, vm, NODE))
            .await
            .expect("attach");
    }
    // The `i`-th volume's device identity (the constructor's
    // documented allocation).
    let volume_count: u32 = volume_ids.len().try_into().expect("volume count fits u32");
    let owned: Vec<String> = (0..volume_count)
        .map(|index| format!("/dev/drbd{}", SEED_MINOR + index))
        .collect();
    let devices: Vec<&str> = owned.iter().map(String::as_str).collect();
    vmm.create(vm, &devices).expect("create VM");
    vmm.start(vm).expect("start VM");
}

/// Build the full campaign fixture for one VM and one volume (the
/// single-volume shape — see [`campaign_rig_volumes`]).
pub async fn campaign_rig(vm: &str, volume_id: &str) -> Rig {
    campaign_rig_volumes(vm, &[volume_id]).await
}

/// One daemon's immutable core over the rig's shared clocks (the
/// e2e `DaemonCore` composition; see [`DaemonCore::clock`] and
/// [`DaemonCore::authority_clock`] for the two-clock split — the
/// comprehensive review's S1). The two daemons differ only in name,
/// token, state, world, VMM, peer direction and provider config.
#[allow(clippy::too_many_arguments)]
fn rig_daemon_core(
    name: &str,
    token: &str,
    state_path: PathBuf,
    world: &Arc<Mutex<FakeDrbd>>,
    journal_dir: PathBuf,
    provider_config: DrbdProviderConfig,
    vmm: &Arc<FakeVmm>,
    peer_addr: SocketAddr,
    witness_addr: SocketAddr,
    stamp_clock: &Arc<AtomicU64>,
    lease_clock: &Arc<AtomicU64>,
    snapshot_root: &Path,
) -> Arc<DaemonCore> {
    daemon_core(DaemonCore {
        name: name.to_owned(),
        witness_token: token.to_owned(),
        state_path,
        world: Arc::clone(world),
        journal_dir,
        provider_config,
        vmm: Arc::clone(vmm),
        peer_addr,
        witness_addr,
        clock: clock_of(stamp_clock),
        authority_clock: clock_of(lease_clock),
        snapshot_root: snapshot_root.to_path_buf(),
        crash: Arc::new(CrashHooks::new()),
        group: Mutex::new(None),
        dead_group: Mutex::new(None),
        killed: Arc::new(AtomicBool::new(false)),
    })
}

/// Build the full campaign fixture for one VM and N volumes (§9
/// row 12's multi-volume cut): witness, both worlds (every volume
/// identity-seeded on the source and peer-seeded on the destination
/// BEFORE the providers construct — providers load state at
/// construction — with one device identity per volume, and the two
/// worlds LINKED so queue drains and resyncs really move content,
/// §2.1), both providers, both wired VMMs, the source's
/// register/attach/VM over every volume, then both daemons under
/// the supervisor. The `i`-th volume's device identity is
/// `SEED_MINOR + i` / `SEED_PORT + i` (the fixture's allocation
/// ranges hold ten volumes; more than that is a rig bug).
pub async fn campaign_rig_volumes(vm: &str, volume_ids: &[&str]) -> Rig {
    install_panic_filter();
    assert!(
        !volume_ids.is_empty(),
        "a campaign rig needs at least one volume"
    );
    let volume_ids: Vec<String> = volume_ids.iter().map(ToString::to_string).collect();
    let clock = Arc::new(AtomicU64::new(START));
    // The stamp clock (S1): the writer's per-ack ticks and the
    // coordinator's stamps share it; the lease clock above stays
    // untouched by guest I/O.
    let stamp_clock = Arc::new(AtomicU64::new(START));
    let witness = WitnessHandle::spawn(Arc::clone(&clock)).await;

    let source = fixture();
    let target = fixture();
    let target_state = target.base.join("state-peer.json");
    for (index, volume_id) in volume_ids.iter().enumerate() {
        let index: u32 = index.try_into().expect("volume index fits u32");
        let port_delta: u16 = index.try_into().expect("volume index fits u16");
        let minor = SEED_MINOR + index;
        let port = SEED_PORT + port_delta;
        seed_volume_with_identity(
            &source.base,
            &source.world,
            volume_id,
            GIB,
            volvisor_drbd::state::ReplicationMode::A,
            minor,
            port,
        );
        seed_peer_volume(
            &target.base,
            &target.world,
            volume_id,
            GIB,
            volvisor_drbd::state::ReplicationMode::A,
            minor,
            port,
        );
    }
    link_replication_peers(&source.world, &target.world);

    let snapshot_root = leak_tempdir();
    let (vmm_a, _devices_a) = wired_vmm(&snapshot_root, &source.world);
    let (vmm_b, _devices_b) = wired_vmm(&snapshot_root, &target.world);

    // Both listeners bind first so the peer URLs are known, then the
    // daemons compose over them.
    let listener_a = TcpListener::bind("127.0.0.1:0").await.expect("bind source");
    let listener_b = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind destination");
    let addr_b = listener_b.local_addr().expect("destination local addr");
    let addr_a = listener_a.local_addr().expect("source local addr");
    let core_a = rig_daemon_core(
        NODE,
        NODE_TOKEN,
        source.state_path.clone(),
        &source.world,
        leak_tempdir(),
        config_for(&source.base),
        &vmm_a,
        addr_b,
        witness.addr,
        &stamp_clock,
        &clock,
        &snapshot_root,
    );
    let core_b = rig_daemon_core(
        PEER_NODE,
        PEER_NODE_TOKEN,
        target_state,
        &target.world,
        leak_tempdir(),
        config_for_peer(&target.base),
        &vmm_b,
        addr_a,
        witness.addr,
        &stamp_clock,
        &clock,
        &snapshot_root,
    );

    // The source's writer shape (setup, not scenario driving — the
    // e2e composition boundary): register, attach (generation 1; the
    // witness grants epoch 1 to node-a on the promote), then the VM.
    // The setup runs on the core's FIRST provider product; each
    // daemon launch then constructs its own fresh provider over the
    // same durable state file (the honest restart shape — the
    // setup's saves are what the launch re-loads).
    seed_source_workload(&core_a.make_provider(), &vmm_a, &volume_ids, vm).await;

    let b = Daemon::launch(core_b, listener_b).await;
    let a = Daemon::launch(core_a, listener_a).await;
    Rig {
        a,
        b,
        witness,
        clock,
        stamp_clock,
        snapshot_root,
        vm: vm.to_owned(),
        volume: volume_ids[0].clone(),
        volumes: volume_ids,
        source_base: source.base.clone(),
        target_base: target.base.clone(),
        vmm_a,
        world_a: Arc::clone(&source.world),
        world_b: Arc::clone(&target.world),
    }
}

impl Rig {
    /// The DRBD resource name of the rig's volume (deterministic;
    /// the same name in both worlds).
    pub fn resource(&self) -> String {
        resource_name_for(&VolumeId::new(self.volume.as_str()).expect("valid volume id"))
    }

    /// The rig's volume id, typed.
    pub fn volume_id(&self) -> VolumeId {
        VolumeId::new(self.volume.as_str()).expect("valid volume id")
    }

    /// The DRBD resource names of every participating volume, in
    /// [`Rig::volumes`] order (deterministic; the same names in
    /// both worlds).
    pub fn resources(&self) -> Vec<String> {
        self.volumes
            .iter()
            .map(|volume| resource_name_for(&VolumeId::new(volume.as_str()).expect("valid id")))
            .collect()
    }

    /// Launch the P4a "surviving host" daemon over the SOURCE's host
    /// directory (§5.1's promotion attempt of the source): a fresh
    /// control state (a state file of its own — no tracked volumes)
    /// over the source's world, config and identity, sharing the
    /// witness and the peer link. This is the unplanned-failover
    /// composition the adopt flow exists for (`drbd_authority_tests`
    /// boots the same shape in-process): the operator tries to bring
    /// the volume back on a host whose control state was lost, and
    /// the adopt route's classification answers from observed facts
    /// only. The survivor holds its OWN journal (fresh) and never
    /// touches the source daemon's — the two daemons coexist, the
    /// parked source keeps its retry task.
    pub async fn launch_source_survivor(&self, tag: &str) -> Daemon {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind survivor");
        let core = daemon_core(DaemonCore {
            name: NODE.to_owned(),
            witness_token: NODE_TOKEN.to_owned(),
            state_path: self.source_base.join(format!("state-survivor-{tag}.json")),
            world: Arc::clone(&self.world_a),
            journal_dir: leak_tempdir(),
            provider_config: config_for(&self.source_base),
            vmm: Arc::clone(&self.vmm_a),
            peer_addr: self.b.addr,
            witness_addr: self.witness.addr,
            clock: clock_of(&self.stamp_clock),
            authority_clock: clock_of(&self.clock),
            snapshot_root: self.snapshot_root.clone(),
            crash: Arc::new(CrashHooks::new()),
            group: Mutex::new(None),
            dead_group: Mutex::new(None),
            killed: Arc::new(AtomicBool::new(false)),
        });
        Daemon::launch(core, listener).await
    }
}

// ---------------------------------------------------- migration driving

/// `POST /v2/migrations` (admin) — the prepare act.
pub async fn post_prepare(addr: SocketAddr, body: &serde_json::Value) -> Reply {
    admin("POST", addr, "/v2/migrations", Some(&body.to_string())).await
}

/// `POST /v2/migrations/{id}/transfer` (admin) — the barrier-and-
/// transfer act. The body's proof is opaque consumer attestation
/// (the coordinator journals it; the drive does its own proofs).
pub async fn post_transfer(addr: SocketAddr, mig: &str) -> Reply {
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

/// `POST /v2/migrations/{id}/abort` (admin; no body).
pub async fn post_abort(addr: SocketAddr, mig: &str) -> Reply {
    admin("POST", addr, &format!("/v2/migrations/{mig}/abort"), None).await
}

/// `GET /v2/volumes/{id}` (admin) — the provider inspect surface an
/// operator has (the D6a checks read it).
pub async fn get_volume(addr: SocketAddr, volume_id: &str) -> Reply {
    admin("GET", addr, &format!("/v2/volumes/{volume_id}"), None).await
}

/// `GET /v2/migrations/{id}` — the public observation route.
pub async fn get_migration(addr: SocketAddr, mig: &str) -> Reply {
    http("GET", addr, &format!("/v2/migrations/{mig}"), None, None).await
}

/// Poll `GET /v2/migrations/{id}` (bounded, real timeout) until the
/// observed state is `want_state`; returns the final summary.
pub async fn poll_migration(addr: SocketAddr, mig: &str, want_state: &str) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + POLL_BOUND;
    loop {
        let (status, body) = get_migration(addr, mig).await.served("observe migration");
        assert_eq!(status, 200, "observe migration: {body}");
        let summary = body_json(&body);
        if state_name(&summary) == want_state {
            return summary;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "migration {mig} never reached {want_state} (last: {})",
            serde_json::to_string_pretty(&summary).unwrap_or_default()
        );
        tokio::time::sleep(POLL_STEP).await;
    }
}
