//! End-to-end grow-notification tests over the real daemon shape
//! (plan `2026-10-10-post-p5-implementation-plan.md`, stage P6-B).
//!
//! The fixture is the full production composition, not a unit
//! harness:
//!
//! - one **daemon**: a real `volvisor_api::router` served by `axum`
//!   over TCP, over a real `LvmProvider` driven through a scripted
//!   `FakeRunner` LVM world (the whole lifecycle — claim, create,
//!   attach, grow — crosses the real HTTP surface and the real
//!   journal);
//! - the **real grow-notification engine** (`volvisor-provider`'s
//!   `grow` module) composed exactly the way `volvisord`'s runtime
//!   wires it for the lvm provider: the provider's own facts
//!   enumeration, a `GrowNotificationStore` under the journal
//!   directory, a proven (or deliberately refused) version gate, and
//!   a `FakeVmm` for the VMM seam;
//! - the **retry task is deliberately never spawned** (the
//!   `migration_e2e` precedent): rows that need reconciliation call
//!   `retry_pass()` on the engine directly, so a parked retry is the
//!   test's own observation, never a background race.
//!
//! What this file deliberately does NOT duplicate: the engine's
//! state-machine rows (including the store's crash seam at every
//! boundary) live in `volvisor-provider/tests/grow_tests.rs`; the
//! config-gate field validation and `--check-config` hermeticity
//! live in `volvisord`'s config tests; the compose seam's journal
//! replay byte-compatibility lives in `volvisor-api/src/tests.rs`.
//!
//! Every assertion is a SAFETY fact: what the VMM was told, what the
//! backing's size actually is, what the durable notification record
//! says — never a happy-path "it returned 200".

// Integration-test code: invariant assertions may use expect/unwrap,
// and one row's setup legitimately exceeds the line budget.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use volvisor_api::{AppState, router};
use volvisor_journal::Journal;
use volvisor_lvm::LvmProvider;
use volvisor_provider::vmm::{FakeVmm, VmmController};
use volvisor_provider::{
    CommandOutput, CommandRunner, FakeRunner, GrowAttachmentFacts, GrowNotificationEngine,
    GrowNotificationStore, GrowNotifier, VmmVersionGate,
};
use volvisor_types::crash::{KillSwitch, STORE_GROW_NOTIFICATIONS, StoreSavePoint};
use volvisor_types::id::VolumeId;

// ---------------------------------------------------------- constants

/// The admin bearer token (fail-closed auth on every mutating route).
const ADMIN_TOKEN: &str = "grow-e2e-admin-token";
/// The device-claim destructive-authorization token.
const CLAIM_TOKEN: &str = "grow-e2e-claim-token";
/// One gibibyte (extent-aligned under the fixture's 4-MiB extents).
const GIB: u64 = 1 << 30;
/// Simulated pool capacity (1 TiB).
const POOL_BYTES: u64 = 1 << 40;
/// Simulated physical extent size (4 MiB), as in real LVM.
const EXTENT_BYTES: u64 = 4 << 20;

// ------------------------------------------------- scripted LVM world

/// The compact simulated LVM world this e2e needs (the
/// `volvisor-lvm` test kit's `FakeLvm`, trimmed to the
/// claim/create/attach/grow surface): LVs and VG free space in a
/// shared map, `lvcreate`/`lvextend` mutating it, `lvs`/`vgs`
/// reporting it — so the provider's verification steps (sizes read
/// back from `lvs`) exercise real round-trips instead of echoes.
/// `lvcreate`/`lvextend` round sizes up to whole extents, as real
/// LVM does.
struct ScriptedLvm {
    /// LV full path (`vg/lv`) to size in bytes.
    lvs: BTreeMap<String, u64>,
    /// PV device paths known to `pvs`.
    pvs: Vec<String>,
    /// VG name to free bytes (before any LV allocations).
    vg_free: BTreeMap<String, u64>,
    /// VG name to total size in bytes.
    vg_size: BTreeMap<String, u64>,
    /// Physical extent size.
    extent_size: u64,
}

impl ScriptedLvm {
    fn new() -> Self {
        Self {
            lvs: BTreeMap::new(),
            pvs: Vec::new(),
            vg_free: BTreeMap::new(),
            vg_size: BTreeMap::new(),
            extent_size: EXTENT_BYTES,
        }
    }

    /// The scripted runner wired to this world (closure mode).
    fn runner(world: &Arc<Mutex<Self>>) -> Arc<FakeRunner> {
        let world = Arc::clone(world);
        Arc::new(FakeRunner::with_closure(move |program, args| {
            let mut world = world.lock().ok()?;
            script(&mut world, program, args)
        }))
    }
}

/// A JSON report wrapper: `{"report":[{"<key>":[rows]}]}`.
fn report(key: &str, rows: &[serde_json::Value]) -> CommandOutput {
    CommandOutput::success(serde_json::json!({ "report": [{ key: rows }] }).to_string())
}

/// The `lvs` report rows for the simulated world.
fn lvs_report(world: &ScriptedLvm) -> CommandOutput {
    let rows: Vec<serde_json::Value> = world
        .lvs
        .iter()
        .map(|(path, size)| {
            let (vg, lv) = path.split_once('/').expect("path is vg/lv");
            serde_json::json!({
                "vg_name": vg,
                "lv_name": lv,
                "lv_size": size.to_string(),
            })
        })
        .collect();
    report("lv", &rows)
}

/// The `vgs` report rows (free space derived, mirroring real LVM).
fn vgs_report(world: &ScriptedLvm) -> CommandOutput {
    let rows: Vec<serde_json::Value> = world
        .vg_free
        .keys()
        .map(|vg| {
            let allocated: u64 = world
                .lvs
                .iter()
                .filter_map(|(path, size)| {
                    path.split_once('/')
                        .and_then(|(vg_name, _)| (vg_name == vg).then_some(*size))
                })
                .sum();
            let free = world
                .vg_free
                .get(vg)
                .copied()
                .unwrap_or_default()
                .saturating_sub(allocated);
            serde_json::json!({
                "vg_name": vg,
                "vg_free": free.to_string(),
                "vg_size": world.vg_size.get(vg).copied().unwrap_or_default().to_string(),
                "vg_extent_size": world.extent_size.to_string(),
            })
        })
        .collect();
    report("vg", &rows)
}

/// The `lsblk` JSON describing the fake host's one whole disk.
fn lsblk_report() -> CommandOutput {
    CommandOutput::success(
        serde_json::json!({
            "blockdevices": [
                {"name": "loop0", "type": "loop", "size": 104_857_600,
                 "serial": null, "wwn": null, "model": null},
                {"name": "sda", "type": "disk", "size": POOL_BYTES,
                 "serial": "FIXTURE-SERIAL-1", "wwn": "0x5000c500fixt0001",
                 "model": "Fixture Disk"},
            ]
        })
        .to_string(),
    )
}

/// The value following `flag` in `args`.
fn arg_after<'a>(args: &[&'a str], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| *arg == flag)
        .and_then(|index| args.get(index + 1))
        .copied()
}

/// Round `size` up to a whole multiple of `extent` (real LVM behavior).
fn round_up_to_extent(size: u64, extent: u64) -> u64 {
    if extent == 0 {
        return size;
    }
    size.div_ceil(extent) * extent
}

/// The scripted behavior for one command.
fn script(world: &mut ScriptedLvm, program: &str, args: &[&str]) -> Option<CommandOutput> {
    match program {
        "lvs" => Some(lvs_report(world)),
        "vgs" => Some(vgs_report(world)),
        "pvs" => {
            let rows: Vec<serde_json::Value> = world
                .pvs
                .iter()
                .map(|path| serde_json::json!({ "pv_name": path }))
                .collect();
            Some(report("pv", &rows))
        }
        "lsblk" => Some(lsblk_report()),
        "lvcreate" => {
            // lvcreate --yes -L <size>B -n <lv> <vg>
            let size = arg_after(args, "-L")?;
            let size: u64 = size.trim_end_matches('B').parse().ok()?;
            let lv = arg_after(args, "-n")?;
            let vg = *args.last()?;
            let path = format!("{vg}/{lv}");
            if world.lvs.contains_key(&path) {
                return Some(CommandOutput::failure(format!(
                    "lvcreate: {path} already exists"
                )));
            }
            let effective = round_up_to_extent(size, world.extent_size);
            world.lvs.insert(path, effective);
            Some(CommandOutput::success(String::new()))
        }
        "lvextend" => {
            // lvextend --yes -L <size>B <vg>/<lv>
            let size = arg_after(args, "-L")?;
            let size: u64 = size.trim_end_matches('B').parse().ok()?;
            let path = *args.last()?;
            let effective = round_up_to_extent(size, world.extent_size);
            world.lvs.insert(path.to_owned(), effective);
            Some(CommandOutput::success(String::new()))
        }
        "pvcreate" => {
            world.pvs.push((*args.last()?).to_owned());
            Some(CommandOutput::success(String::new()))
        }
        "vgcreate" => {
            // vgcreate --yes <vg> <path>
            let vg = (*args.get(1)?).to_owned();
            world.vg_free.insert(vg.clone(), POOL_BYTES);
            world.vg_size.insert(vg, POOL_BYTES);
            Some(CommandOutput::success(String::new()))
        }
        _ => None,
    }
}

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

/// One admin-authenticated call against the daemon.
async fn admin(method: &str, addr: SocketAddr, path: &str, body: Option<&str>) -> (u16, String) {
    http(method, addr, path, body, Some(ADMIN_TOKEN)).await
}

/// The lossy variant for the crash row: the handler dies mid-request,
/// so the connection breaks — `None` is the honest observation, not a
/// test failure.
async fn http_lossy(
    method: &str,
    addr: SocketAddr,
    path: &str,
    body: &str,
    bearer: &str,
) -> Option<u16> {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect daemon");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nauthorization: Bearer {bearer}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut response = Vec::new();
    // A broken connection (reset) or a truncated read are both "the
    // response never arrived".
    match stream.read_to_end(&mut response).await {
        Ok(_) => {
            let text = String::from_utf8_lossy(&response).into_owned();
            text.split_whitespace().nth(1).and_then(|s| s.parse().ok())
        }
        Err(_) => None,
    }
}

/// Parse a JSON response body.
fn body_json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("valid JSON body")
}

// ------------------------------------------------------------- rig

/// The durable world several daemon incarnations share (the restart
/// rows): one directory tree, one scripted LVM, one VMM.
struct World {
    /// Kept alive for the rig's lifetime.
    _dir: tempfile::TempDir,
    /// The simulated LVM.
    lvm: Arc<Mutex<ScriptedLvm>>,
    /// The fake VMM (shared across restarts: its resize log is the
    /// observation surface).
    vmm: Arc<FakeVmm>,
    /// The journal directory (journal, LVM state, notification store).
    journal_dir: PathBuf,
}

impl World {
    fn new() -> Arc<Self> {
        let dir = tempfile::tempdir().expect("tempdir");
        let vmm = Arc::new(FakeVmm::new(dir.path().join("snapshots")));
        vmm.create("vm-1", &["/dev/vg/vol-grow"])
            .expect("create the fake VM");
        Arc::new(Self {
            journal_dir: dir.path().join("journal"),
            lvm: Arc::new(Mutex::new(ScriptedLvm::new())),
            vmm,
            _dir: dir,
        })
    }

    /// The provider over the durable state file and the scripted LVM.
    fn provider(&self) -> Arc<LvmProvider> {
        let runner: Arc<dyn CommandRunner> = ScriptedLvm::runner(&self.lvm);
        LvmProvider::new(
            runner,
            self.journal_dir.join("lvm-state.json"),
            PathBuf::from("/nonexistent-sysfs"),
            "vve2e".to_owned(),
            CLAIM_TOKEN.to_owned(),
        )
        .map(Arc::new)
        .expect("provider construction")
    }

    /// The notification store path (the runtime's convention).
    fn store_path(&self) -> PathBuf {
        self.journal_dir.join("grow-notifications.json")
    }
}

/// A proven version gate, through the real probe path (a scripted
/// `cloud-hypervisor --version`).
fn proven_gate() -> VmmVersionGate {
    // The probe invokes the configured binary path, so the script
    // matches the name, not the full path.
    let runner = FakeRunner::with_closure(|program, _args| {
        program
            .rsplit('/')
            .next()
            .is_some_and(|name| name == "cloud-hypervisor")
            .then(|| CommandOutput::success("cloud-hypervisor v37.0\n"))
    });
    VmmVersionGate::probe(
        Some(PathBuf::from("/usr/bin/cloud-hypervisor").as_path()),
        Some("37.0.0"),
        &runner,
    )
}

/// One daemon incarnation: the full composition, served over TCP.
struct Daemon {
    addr: SocketAddr,
    engine: Arc<GrowNotificationEngine>,
    world: Arc<World>,
    serve: Option<JoinHandle<()>>,
}

impl Daemon {
    /// Launch over `world` with a proven gate.
    async fn launch(world: Arc<World>) -> Daemon {
        Self::launch_inner(world, proven_gate(), None).await
    }

    /// Launch over `world` with a deliberately refused gate (the
    /// fail-closed posture row).
    async fn launch_refused(world: Arc<World>, reason: &str) -> Daemon {
        Self::launch_inner(world, VmmVersionGate::refused(reason), None).await
    }

    /// Launch over `world` with the notification store's crash seam
    /// armed at `point` (the crash row: the intent journal's save
    /// dies there, in-band, after firing `kill`).
    async fn launch_crashing(world: Arc<World>, point: StoreSavePoint, kill: KillSwitch) -> Daemon {
        Self::launch_inner(world, proven_gate(), Some((point, kill))).await
    }

    async fn launch_inner(
        world: Arc<World>,
        gate: VmmVersionGate,
        crash: Option<(StoreSavePoint, KillSwitch)>,
    ) -> Daemon {
        let provider = world.provider();
        let store =
            GrowNotificationStore::open(world.store_path()).expect("open the notification store");
        if let Some((point, kill)) = crash {
            let hooks = Arc::clone(store.store_crash_hooks());
            hooks.set_kill_switch(kill);
            hooks.arm(STORE_GROW_NOTIFICATIONS, point);
        }
        let facts: GrowAttachmentFacts = {
            let provider = Arc::clone(&provider);
            Arc::new(move || provider.grow_attachment_facts())
        };
        let clock: volvisor_provider::grow::Clock = {
            let ticks = Arc::new(AtomicU64::new(1_700_000_000));
            Arc::new(move || ticks.fetch_add(1, Ordering::SeqCst))
        };
        let engine = Arc::new(GrowNotificationEngine::new(
            Some(Arc::clone(&world.vmm) as Arc<dyn VmmController>),
            gate,
            facts,
            store,
            clock,
        ));
        let admin: Arc<dyn volvisor_provider::AdminSurface> = provider.clone();
        let state = AppState::new(
            Arc::clone(&provider) as Arc<dyn volvisor_provider::VolumeProvider>,
            Some(admin),
            Journal::open(&world.journal_dir).expect("journal opens"),
            Some(ADMIN_TOKEN.to_owned()),
        )
        .with_grow_notifier(Arc::clone(&engine) as Arc<dyn GrowNotifier>);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind daemon");
        let addr = listener.local_addr().expect("daemon local addr");
        let app = router(Arc::new(state), 1 << 20);
        let serve = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("daemon serves");
        });
        Daemon {
            addr,
            engine,
            world,
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

    /// The one LV's size in the simulated world (exactly one volume
    /// exists in these rows).
    fn backing_size(&self) -> u64 {
        let world = self.world.lvm.lock().expect("world lock");
        let (_, size) = world
            .lvs
            .iter()
            .next()
            .expect("exactly one LV exists after create");
        *size
    }
}

// ------------------------------------------------------ row helpers

/// Claim the fixture disk, create one volume, and attach it — the
/// full HTTP lifecycle every attached row starts from. Returns the
/// device claim's 200 (already asserted) and leaves the volume at
/// generation 2 (attached).
async fn claim_create_attach(daemon: &Daemon, vmm_disk_id: Option<&str>) {
    let devices = admin("GET", daemon.addr, "/v2/admin/devices", None).await;
    assert_eq!(devices.0, 200, "device list: {}", devices.1);
    let device_id = body_json(&devices.1)["devices"][0]["id"]
        .as_str()
        .expect("a discovered device id")
        .to_owned();
    let claim = format!(
        "{{\"api_version\":\"volvisor.volume.v2\",\"operation_id\":\"op-claim\",\
         \"authorization_token\":\"{CLAIM_TOKEN}\"}}"
    );
    let (status, body) = admin(
        "POST",
        daemon.addr,
        &format!("/v2/admin/devices/{device_id}/claim"),
        Some(&claim),
    )
    .await;
    assert_eq!(status, 200, "claim: {body}");

    let create = format!(
        "{{\"api_version\":\"volvisor.volume.v2\",\"operation_id\":\"op-create\",\
         \"project_id\":\"grow-e2e\",\"volume_id\":\"vol-grow\",\"class\":\"native-local\",\
         \"size_bytes\":{GIB}}}"
    );
    let (status, body) = admin("POST", daemon.addr, "/v2/volumes", Some(&create)).await;
    assert_eq!(status, 200, "create: {body}");

    let disk_id_field = vmm_disk_id
        .map(|id| format!(",\"vmm_disk_id\":\"{id}\""))
        .unwrap_or_default();
    let attach = format!(
        "{{\"api_version\":\"volvisor.volume.v2\",\"operation_id\":\"op-attach\",\
         \"vm_id\":\"vm-1\",\"host_id\":\"host-1\",\"attachment_id\":\"att-1\",\
         \"expected_volume_generation\":1{disk_id_field}}}"
    );
    let (status, body) = admin(
        "POST",
        daemon.addr,
        "/v2/volumes/vol-grow/attach",
        Some(&attach),
    )
    .await;
    assert_eq!(status, 200, "attach: {body}");
}

/// One grow request with a fresh operation id (the consumer's
/// recovery path after a crash: a new id, never the in-doubt one).
/// `expected_generation` is the volume's current generation (create
/// starts at 1; every attach, detach and completed grow bumps it).
async fn grow(
    daemon: &Daemon,
    operation_id: &str,
    new_size_bytes: u64,
    expected_generation: u64,
) -> (u16, String) {
    let body = format!(
        "{{\"api_version\":\"volvisor.volume.v2\",\"operation_id\":\"{operation_id}\",\
         \"new_size_bytes\":{new_size_bytes},\"expected_generation\":{expected_generation}}}"
    );
    admin(
        "POST",
        daemon.addr,
        "/v2/volumes/vol-grow/grow",
        Some(&body),
    )
    .await
}

fn volume() -> VolumeId {
    VolumeId::new("vol-grow").expect("valid volume id")
}

// -------------------------------------------------------------- rows

#[tokio::test]
async fn an_attached_grow_notifies_the_guest_through_the_real_api() {
    let world = World::new();
    let mut daemon = Daemon::launch(Arc::clone(&world)).await;
    claim_create_attach(&daemon, Some("disk-vol-grow")).await;

    let (status, body) = grow(&daemon, "op-grow-1", 2 * GIB, 2).await;
    assert_eq!(status, 200, "grow: {body}");
    let value = body_json(&body);
    assert_eq!(
        value["guest_notification_status"], "notified",
        "the composed status crossed the real API surface: {body}"
    );
    assert_eq!(value["effective_size_bytes"], 2 * GIB);
    assert_eq!(value["backing_resized"], true);

    // The VMM was told exactly the effective size, through the
    // recorded disk id.
    assert_eq!(
        daemon.world.vmm.resize_calls().expect("resizes"),
        vec![volvisor_provider::FakeResizeCall {
            vm_id: "vm-1".to_owned(),
            disk_id: "disk-vol-grow".to_owned(),
            new_size_bytes: 2 * GIB,
        }]
    );
    // The backing really grew (read back through the simulated lvs,
    // not the response).
    assert_eq!(daemon.backing_size(), 2 * GIB);
    // The durable notification state says notified.
    let record = daemon
        .engine
        .record(&volume())
        .expect("record")
        .expect("the record exists");
    assert_eq!(record.target_size_bytes, 2 * GIB);
    assert!(!record.is_pending());

    // The same operation id replays byte-identically (the journaled
    // outcome carries the composed status; the closure — and the
    // notifier inside it — never re-runs).
    let (replay_status, replay_body) = grow(&daemon, "op-grow-1", 2 * GIB, 2).await;
    assert_eq!(replay_status, 200, "replay: {replay_body}");
    assert_eq!(replay_body, body, "the replay is byte-identical");
    assert_eq!(
        daemon.world.vmm.resize_calls().expect("resizes").len(),
        1,
        "the replay does not re-drive the notification"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn a_failed_notification_keeps_the_grown_backing_and_the_pass_converges() {
    let world = World::new();
    let mut daemon = Daemon::launch(Arc::clone(&world)).await;
    claim_create_attach(&daemon, Some("disk-vol-grow")).await;
    daemon
        .world
        .vmm
        .set_fail("vm-1", |knobs| knobs.resize_disk = true)
        .expect("arm the fault");

    let (status, body) = grow(&daemon, "op-grow-1", 2 * GIB, 2).await;
    assert_eq!(status, 200, "the grow itself succeeds: {body}");
    let value = body_json(&body);
    assert_eq!(
        value["guest_notification_status"], "retry_required",
        "the failed notification is honest: {body}"
    );
    assert_eq!(value["effective_size_bytes"], 2 * GIB);

    // The never-shrink rule, observed on the backing: the resize
    // failed and the backing STAYS at the grown size — the
    // notification is retried, never the grow undone.
    assert_eq!(daemon.backing_size(), 2 * GIB);
    // Nothing was told to the VMM.
    assert_eq!(
        daemon.world.vmm.resize_calls().expect("resizes"),
        Vec::new()
    );
    // The reason is recorded durably.
    let record = daemon
        .engine
        .record(&volume())
        .expect("record")
        .expect("the record exists");
    let reason = record.pending_reason().expect("pending with a reason");
    assert!(reason.contains("injected"), "{reason}");
    assert_eq!(record.target_size_bytes, 2 * GIB);

    // Recovery through the surface production would: the retry pass
    // converges retry_required -> notified.
    daemon
        .world
        .vmm
        .set_fail("vm-1", |knobs| knobs.resize_disk = false)
        .expect("clear the fault");
    let report = daemon.engine.retry_pass().expect("the pass");
    assert_eq!(report.notified, vec![(volume(), 2 * GIB)]);
    assert_eq!(report.retry_required, Vec::new());
    assert_eq!(
        daemon.world.vmm.resize_calls().expect("resizes"),
        vec![volvisor_provider::FakeResizeCall {
            vm_id: "vm-1".to_owned(),
            disk_id: "disk-vol-grow".to_owned(),
            new_size_bytes: 2 * GIB,
        }]
    );

    daemon.stop().await;
}

#[tokio::test]
async fn a_detached_grow_reports_not_applicable() {
    let world = World::new();
    let mut daemon = Daemon::launch(Arc::clone(&world)).await;
    // Claim and create only: no attachment exists.
    claim_create_attach(&daemon, None).await;
    let detach = "{\"api_version\":\"volvisor.volume.v2\",\"operation_id\":\"op-detach\",\
                  \"attachment_id\":\"att-1\",\"expected_attachment_generation\":1,\
                  \"vm_stopped_or_io_drained_proof\":\"vm_stopped\"}";
    let (status, body) = admin(
        "POST",
        daemon.addr,
        "/v2/volumes/vol-grow/detach",
        Some(detach),
    )
    .await;
    assert_eq!(status, 200, "detach: {body}");

    let (status, body) = grow(&daemon, "op-grow-1", 2 * GIB, 3).await;
    assert_eq!(status, 200, "grow: {body}");
    assert_eq!(
        body_json(&body)["guest_notification_status"],
        "not_applicable",
        "no frontend exists to notify: {body}"
    );
    assert_eq!(daemon.backing_size(), 2 * GIB, "the backing still grew");
    assert_eq!(
        daemon.world.vmm.resize_calls().expect("resizes"),
        Vec::new(),
        "the VMM is never told anything"
    );
    assert_eq!(
        daemon.engine.record(&volume()).expect("record"),
        None,
        "no notification obligation is recorded for a detached volume"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn an_unaddressable_attachment_records_the_refusal() {
    let world = World::new();
    let mut daemon = Daemon::launch(Arc::clone(&world)).await;
    // Attached, but the consumer configured no VMM disk id for the
    // frontend.
    claim_create_attach(&daemon, None).await;

    let (status, body) = grow(&daemon, "op-grow-1", 2 * GIB, 2).await;
    assert_eq!(status, 200, "grow: {body}");
    assert_eq!(
        body_json(&body)["guest_notification_status"],
        "retry_required",
        "a frontend exists, so this is never not_applicable: {body}"
    );
    assert_eq!(daemon.backing_size(), 2 * GIB);
    assert_eq!(
        daemon.world.vmm.resize_calls().expect("resizes"),
        Vec::new(),
        "the VMM is never told anything"
    );
    let reason = daemon
        .engine
        .record(&volume())
        .expect("record")
        .expect("the record exists")
        .pending_reason()
        .expect("pending with a reason");
    assert!(reason.contains("vmm_disk_id"), "{reason}");

    daemon.stop().await;
}

#[tokio::test]
async fn a_refused_version_gate_never_touches_the_vmm() {
    let world = World::new();
    let mut daemon = Daemon::launch_refused(
        Arc::clone(&world),
        "the observed cloud-hypervisor version 36.0 is below the configured minimum 37.0.0",
    )
    .await;
    claim_create_attach(&daemon, Some("disk-vol-grow")).await;

    let (status, body) = grow(&daemon, "op-grow-1", 2 * GIB, 2).await;
    assert_eq!(status, 200, "the grow itself succeeds: {body}");
    let value = body_json(&body);
    assert_eq!(value["guest_notification_status"], "retry_required");
    assert_eq!(value["effective_size_bytes"], 2 * GIB);
    // The gate refusal never fails the grow, and the VMM is never
    // told anything below the proven minimum.
    assert_eq!(daemon.backing_size(), 2 * GIB);
    assert_eq!(
        daemon.world.vmm.resize_calls().expect("resizes"),
        Vec::new()
    );
    let reason = daemon
        .engine
        .record(&volume())
        .expect("record")
        .expect("the record exists")
        .pending_reason()
        .expect("pending with a reason");
    assert!(reason.contains("below the configured minimum"), "{reason}");

    daemon.stop().await;
}

#[tokio::test]
async fn a_crash_between_the_backing_grow_and_the_notification_converges_on_restart() {
    // Keep the crash marker out of the test output (the campaign's
    // filtered-hook shape; the panic IS the injected process death).
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let ours = info
            .payload()
            .downcast_ref::<String>()
            .is_some_and(|message| message.starts_with(volvisor_types::crash::CRASH_PANIC_PREFIX));
        if !ours {
            default_hook(info);
        }
    }));

    let world = World::new();
    let killed = Arc::new(AtomicBool::new(false));
    let kill = {
        let killed = Arc::clone(&killed);
        Arc::new(move || killed.store(true, Ordering::SeqCst)) as KillSwitch
    };
    let mut daemon =
        Daemon::launch_crashing(Arc::clone(&world), StoreSavePoint::AfterRename, kill).await;
    claim_create_attach(&daemon, Some("disk-vol-grow")).await;

    // The grow request dies mid-notification (the intent journal's
    // save is killed after the rename — the new state is exactly
    // what a reload sees).
    let body = "{\"api_version\":\"volvisor.volume.v2\",\"operation_id\":\"op-grow-crash\",\
                \"new_size_bytes\":2147483648,\"expected_generation\":2}";
    let status = http_lossy(
        "POST",
        daemon.addr,
        "/v2/volumes/vol-grow/grow",
        body,
        ADMIN_TOKEN,
    )
    .await;
    assert_ne!(
        status,
        Some(200),
        "the request dies with the injected crash (observed status: {status:?})"
    );
    assert!(
        killed.load(Ordering::SeqCst),
        "the kill switch fired before the in-band panic"
    );

    // What a reload sees, observed directly on the durable files:
    // the backing grew, the intent is recorded, the VMM was never
    // told.
    assert_eq!(daemon.backing_size(), 2 * GIB);
    assert_eq!(
        daemon.world.vmm.resize_calls().expect("resizes"),
        Vec::new(),
        "the notification never reached the VMM"
    );
    let store = GrowNotificationStore::open(world.store_path()).expect("reopen the store");
    let record = store.get(&volume()).expect("the intent is durable");
    assert!(record.is_pending());
    assert_eq!(record.target_size_bytes, 2 * GIB);

    // The consumer cannot resolve the in-doubt operation by reusing
    // its id — the strict fail-closed rule.
    let (status, body) = grow(&daemon, "op-grow-crash", 2 * GIB, 2).await;
    assert_eq!(
        body_json(&body)["code"],
        "OPERATION_IN_DOUBT",
        "the in-doubt operation is never re-executed: {body} (status {status})"
    );

    // The restart: a fresh daemon over the same durable paths. The
    // startup reconcile (the retry task's first pass — driven here
    // directly, never spawned) re-drives the notification.
    daemon.stop().await;
    let mut daemon = Daemon::launch(Arc::clone(&world)).await;
    let report = daemon.engine.retry_pass().expect("the restart pass");
    assert_eq!(
        report.notified,
        vec![(volume(), 2 * GIB)],
        "the startup reconcile re-drives the notification"
    );
    assert_eq!(
        daemon.world.vmm.resize_calls().expect("resizes"),
        vec![volvisor_provider::FakeResizeCall {
            vm_id: "vm-1".to_owned(),
            disk_id: "disk-vol-grow".to_owned(),
            new_size_bytes: 2 * GIB,
        }]
    );
    assert!(
        !daemon
            .engine
            .record(&volume())
            .expect("record")
            .expect("the record exists")
            .is_pending()
    );

    // The consumer's recovery path: the crashed grow already brought
    // the backing to 2 GiB (grow-only refuses a same-size re-issue),
    // so the next legitimate grow — a new operation id, a larger
    // size — composes end-to-end on the recovered daemon.
    let (status, body) = grow(&daemon, "op-grow-recovered", 3 * GIB, 3).await;
    assert_eq!(status, 200, "the recovered grow: {body}");
    assert_eq!(
        body_json(&body)["guest_notification_status"],
        "notified",
        "the consumer sees the converged state: {body}"
    );
    assert_eq!(
        daemon.world.vmm.resize_calls().expect("resizes"),
        vec![
            volvisor_provider::FakeResizeCall {
                vm_id: "vm-1".to_owned(),
                disk_id: "disk-vol-grow".to_owned(),
                new_size_bytes: 2 * GIB,
            },
            volvisor_provider::FakeResizeCall {
                vm_id: "vm-1".to_owned(),
                disk_id: "disk-vol-grow".to_owned(),
                new_size_bytes: 3 * GIB,
            }
        ],
        "the reconcile drove the notification, then the recovered grow drove the new size"
    );

    daemon.stop().await;
}
