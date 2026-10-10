//! End-to-end same-VG move tests over the real daemon shape (plan
//! `2026-10-10-post-p5-implementation-plan.md`, stage P6-C).
//!
//! The fixture is the full production composition, not a unit
//! harness:
//!
//! - one **daemon**: a real `volvisor_api::router` served by `axum`
//!   over TCP, over a real `LvmProvider` driven through a scripted
//!   `FakeRunner` LVM world (the whole lifecycle — claim, create,
//!   attach, move — crosses the real HTTP surface and the real
//!   journal);
//! - the **retry reconcile task** is spawned only by the row that
//!   needs it (the `grow_e2e` precedent), with a short tick — every
//!   other row owns its own passes, so a parked record is the
//!   test's own observation, never a background race.
//!
//! What this file pins — the facts only the daemon composition can
//! prove:
//!
//! - the move crossing the real route with the typed response
//!   envelope, one verified generation bump, and the extents really
//!   relocated (read back through the simulated world, not the
//!   response);
//! - the **Terminal replay** discipline: a completed outcome and an
//!   in-flight `COPYING` outcome both replay byte-identically for
//!   the same `operation_id`, with no provider re-execution (no
//!   second `pvmove`), while a fresh `operation_id` re-attaches to
//!   the same kernel-side move;
//! - the typed conflict (a different target while a move is
//!   active) reconstructing as a 409 through the HTTP surface;
//! - the **durable-boundary daemon death**: the request dies
//!   in-band at the armed `lvm_state` save, the record it left
//!   behind says `PREPARING`, nothing in the world moved, and a
//!   fresh incarnation over the same durable state re-drives the
//!   journaled intent to `COMPLETE` with exactly one `pvmove` and
//!   one generation bump across both incarnations;
//! - the **retry task** (the runtime's `spawn_move_retry_task`
//!   shape, short tick) completing a parked `COPYING` record
//!   between consumer requests.
//!
//! What this file deliberately does NOT duplicate: the provider's
//! move state-machine rows (the full refusal table, the fault
//! tails, every crash window) live in `volvisor-lvm`'s
//! `move_tests.rs`; the unqualified-provider refusal and the
//! envelope/journal shapes live in `volvisor-api`'s route tests.
//!
//! Every assertion is a SAFETY fact: what the durable record says,
//! where the extents sit, how many `pvmove` invocations ran — never
//! a happy-path "it returned 200".

// Integration-test code: invariant assertions may use expect/unwrap,
// and one row's setup legitimately exceeds the line budget.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use volvisor_api::{AppState, router};
use volvisor_journal::Journal;
use volvisor_lvm::LvmProvider;
use volvisor_lvm::state::LvmState;
use volvisor_provider::{AdminSurface, CommandOutput, CommandRunner, FakeRunner, VolumeProvider};
use volvisor_types::crash::{KillSwitch, STORE_LVM_STATE, StoreSavePoint};
use volvisor_types::id::VolumeId;

// ---------------------------------------------------------- constants

/// The admin bearer token (fail-closed auth on every mutating route).
const ADMIN_TOKEN: &str = "move-e2e-admin-token";
/// The device-claim destructive-authorization token.
const CLAIM_TOKEN: &str = "move-e2e-claim-token";
/// One gibibyte (extent-aligned under the fixture's 4-MiB extents).
const GIB: u64 = 1 << 30;
/// Simulated pool capacity (1 TiB).
const POOL_BYTES: u64 = 1 << 40;
/// Simulated physical extent size (4 MiB), as in real LVM.
const EXTENT_BYTES: u64 = 4 << 20;
/// The companion target PV every claimed VG carries in this world
/// (the evacuation destination; the source is the claimed disk's
/// by-id path).
const MOVE_TARGET_PV: &str = "/dev/pv-b";

// ------------------------------------------------- scripted LVM world

/// The compact simulated LVM world this e2e needs (the
/// `volvisor-lvm` test kit's `FakeLvm`, trimmed to the
/// claim/create/attach/move surface): LVs with per-PV placement, a
/// per-PV capacity model, and the `pvmove` simulation whose
/// deterministic clock advances on every `lvs` query — the shape
/// verified against real LVM 2.03.16 (mid-move the LV row's
/// `devices` column references the `pvmove0` mirror segment and
/// `copy_percent` stays empty; a completed move leaves no trace but
/// the placement).
struct ScriptedLvm {
    /// LV full path (`vg/lv`) to size in bytes.
    lvs: BTreeMap<String, u64>,
    /// LV path to its backing PVs (the placement model).
    lv_devices: BTreeMap<String, Vec<String>>,
    /// PV device paths known to `pvs`.
    pvs: Vec<String>,
    /// PV path to its volume group.
    pv_vg: BTreeMap<String, String>,
    /// PV path to free bytes (the per-PV capacity model).
    pv_free: BTreeMap<String, u64>,
    /// VG name to free bytes (before any LV allocations).
    vg_free: BTreeMap<String, u64>,
    /// VG name to total size in bytes.
    vg_size: BTreeMap<String, u64>,
    /// Active pvmove simulations, keyed by LV path.
    moves: BTreeMap<String, ScriptedMove>,
    /// Sync percentage advanced per `lvs` query (the deterministic
    /// clock; two queries complete a move).
    move_advance_percent: u64,
    /// When true, active moves never advance (the supervision-window
    /// exhaustion shape).
    hold_moves: bool,
    /// Physical extent size.
    extent_size: u64,
}

/// One simulated `pvmove`.
struct ScriptedMove {
    source: String,
    target: String,
    percent: u64,
}

impl ScriptedLvm {
    fn new() -> Self {
        Self {
            lvs: BTreeMap::new(),
            lv_devices: BTreeMap::new(),
            pvs: Vec::new(),
            pv_vg: BTreeMap::new(),
            pv_free: BTreeMap::new(),
            vg_free: BTreeMap::new(),
            vg_size: BTreeMap::new(),
            moves: BTreeMap::new(),
            move_advance_percent: 50,
            hold_moves: false,
            extent_size: EXTENT_BYTES,
        }
    }

    /// Register a PV in `vg` with the pool's capacity free.
    fn add_pv_to_vg(&mut self, pv: &str, vg: &str) {
        self.pvs.push(pv.to_owned());
        self.pv_vg.insert(pv.to_owned(), vg.to_owned());
        self.pv_free.insert(pv.to_owned(), POOL_BYTES);
    }

    /// The scripted runner wired to this world (closure mode).
    fn runner(world: &Arc<Mutex<Self>>) -> Arc<FakeRunner> {
        let world = Arc::clone(world);
        Arc::new(FakeRunner::with_closure(move |program, args| {
            let mut world = world.lock().ok()?;
            script(&mut world, program, args)
        }))
    }

    /// The LV's current placement (the assertion surface).
    fn placement_of(&self, path: &str) -> Vec<String> {
        self.lv_devices.get(path).cloned().unwrap_or_default()
    }
}

/// A JSON report wrapper: `{"report":[{"<key>":[rows]}]}`.
fn report(key: &str, rows: &[serde_json::Value]) -> CommandOutput {
    CommandOutput::success(serde_json::json!({ "report": [{ key: rows }] }).to_string())
}

/// The `lvs` report rows — and the deterministic move clock: every
/// query advances each active move, and a move that reaches 100%
/// lands (the placement becomes the target, the free accounts
/// transfer, the move ends).
fn lvs_report(world: &mut ScriptedLvm) -> CommandOutput {
    if !world.hold_moves {
        let advance = world.move_advance_percent;
        for move_ in world.moves.values_mut() {
            move_.percent += advance;
        }
        let landed: Vec<String> = world
            .moves
            .iter()
            .filter(|(_, move_)| move_.percent >= 100)
            .map(|(path, _)| path.clone())
            .collect();
        for path in landed {
            let Some(move_) = world.moves.remove(&path) else {
                continue;
            };
            let size = world.lvs.get(&path).copied().unwrap_or_default();
            world
                .lv_devices
                .insert(path.clone(), vec![move_.target.clone()]);
            if let Some(free) = world.pv_free.get_mut(&move_.target) {
                *free = (*free).saturating_sub(size);
            }
            if let Some(free) = world.pv_free.get_mut(&move_.source) {
                *free = (*free).saturating_add(size);
            }
        }
    }
    let rows: Vec<serde_json::Value> = world
        .lvs
        .iter()
        .map(|(path, size)| {
            let (vg, lv) = path.split_once('/').expect("path is vg/lv");
            let moving = world.moves.contains_key(path);
            let devices = if moving {
                "pvmove0(0)".to_owned()
            } else {
                world
                    .lv_devices
                    .get(path)
                    .map(|pvs| {
                        pvs.iter()
                            .map(|pv| format!("{pv}(0)"))
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default()
            };
            serde_json::json!({
                "vg_name": vg,
                "lv_name": lv,
                "lv_size": size.to_string(),
                "lv_attr": if moving { "-wI-a-----" } else { "-wi-a-----" },
                "copy_percent": "",
                "devices": devices,
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
                .map(|path| {
                    serde_json::json!({
                        "pv_name": path,
                        "vg_name": world.pv_vg.get(path).cloned().unwrap_or_default(),
                        "pv_free": world
                            .pv_free
                            .get(path)
                            .copied()
                            .unwrap_or_default()
                            .to_string(),
                    })
                })
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
            world.lvs.insert(path.clone(), effective);
            // Placement: the VG's first PV in report order (the
            // claimed disk's by-id path sorts before the companion
            // target PV).
            let placement = world
                .pv_vg
                .iter()
                .find(|(_, pv_vg)| pv_vg == &vg)
                .map(|(pv, _)| pv.clone());
            if let Some(pv) = placement {
                world.lv_devices.insert(path.clone(), vec![pv.clone()]);
                if let Some(free) = world.pv_free.get_mut(&pv) {
                    *free = (*free).saturating_sub(effective);
                }
            }
            Some(CommandOutput::success(String::new()))
        }
        "pvmove" => {
            // pvmove --background --noudevsync -n <vg>/<lv> <source> <target>
            let path = arg_after(args, "-n")?.to_owned();
            let source = (*args.get(args.len().saturating_sub(2))?).to_owned();
            let target = (*args.last()?).to_owned();
            // Verified LVM 2.03.16 behavior: a re-run while the
            // source PV carries an active move attaches to it,
            // IGNORES the remaining arguments, and exits 0.
            if world.moves.values().any(|move_| move_.source == source) {
                return Some(CommandOutput::success(
                    "  Detected pvmove in progress; WARNING: Ignoring remaining \
                     command line arguments.\n"
                        .to_owned(),
                ));
            }
            let on_source = world
                .lv_devices
                .get(&path)
                .is_some_and(|pvs| pvs.iter().any(|pv| pv == &source));
            if !on_source {
                return Some(CommandOutput::failure("  No data to move.\n"));
            }
            world.moves.insert(
                path,
                ScriptedMove {
                    source,
                    target,
                    percent: 0,
                },
            );
            Some(CommandOutput::success(String::new()))
        }
        "pvcreate" => Some(CommandOutput::success(String::new())),
        "vgcreate" => {
            // vgcreate --yes <vg> <path>
            let vg = (*args.get(1)?).to_owned();
            world.vg_free.insert(vg.clone(), 2 * POOL_BYTES);
            world.vg_size.insert(vg.clone(), 2 * POOL_BYTES);
            // The claimed disk joins its VG, and the fixture's
            // companion target PV joins it too (the two-PV world the
            // move rows evacuate across).
            let claimed = (*args.last()?).to_owned();
            world.add_pv_to_vg(&claimed, &vg);
            world.add_pv_to_vg(MOVE_TARGET_PV, &vg);
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
async fn http_lossy(method: &str, addr: SocketAddr, path: &str, body: &str) -> Option<u16> {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect daemon");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nauthorization: Bearer {ADMIN_TOKEN}\r\nconnection: close\r\n\r\n{body}",
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
/// rows): one directory tree, one scripted LVM.
struct World {
    /// Kept alive for the rig's lifetime.
    _dir: tempfile::TempDir,
    /// The simulated LVM.
    lvm: Arc<Mutex<ScriptedLvm>>,
    /// The sysfs root (the by-id symlink the discovery resolves).
    sysfs_root: PathBuf,
    /// The journal directory (journal + LVM state).
    journal_dir: PathBuf,
}

impl World {
    fn new() -> Arc<Self> {
        let dir = tempfile::tempdir().expect("tempdir");
        // The stable hardware identity: the discovery resolves the
        // fixture disk's WWN through the by-id index, so the claimed
        // PV (and the lvcreate placement) is the by-id path, never
        // the volatile /dev/sda name (SPEC-0002 section 3).
        let by_id = dir.path().join("sysroot/dev/disk/by-id");
        std::fs::create_dir_all(&by_id).expect("by-id dir");
        std::os::unix::fs::symlink("../../sda", by_id.join("wwn-0x5000c500fixt0001"))
            .expect("by-id symlink");
        Arc::new(Self {
            journal_dir: dir.path().join("journal"),
            sysfs_root: dir.path().join("sysroot"),
            lvm: Arc::new(Mutex::new(ScriptedLvm::new())),
            _dir: dir,
        })
    }

    /// The provider over the durable state file and the scripted
    /// LVM, with the e2e's fast move timing (a 10 ms poll, a 250 ms
    /// supervision window: a held move exhausts the window, an
    /// unheld one completes within two polls).
    fn provider(&self) -> Arc<LvmProvider> {
        let runner: Arc<dyn CommandRunner> = ScriptedLvm::runner(&self.lvm);
        LvmProvider::new(
            runner,
            self.state_path(),
            self.sysfs_root.clone(),
            "vve2e".to_owned(),
            CLAIM_TOKEN.to_owned(),
        )
        .map(|provider| {
            Arc::new(
                provider.with_move_timing(volvisor_lvm::provider::MoveTiming {
                    poll_interval: Duration::from_millis(10),
                    supervision_window: Duration::from_millis(250),
                }),
            )
        })
        .expect("provider construction")
    }

    /// The durable LVM state file path (the runtime's convention).
    fn state_path(&self) -> PathBuf {
        self.journal_dir.join("lvm-state.json")
    }

    /// One volume's (move record state, generation) from the durable
    /// state file — the truth a reload sees, read directly.
    fn recorded_move(&self, volume: &str) -> (Option<volvisor_lvm::state::MoveRecord>, u64) {
        let volume_id = VolumeId::new(volume).expect("valid volume id");
        let state = LvmState::load(&self.state_path()).expect("load state");
        (
            state.move_record(&volume_id).cloned(),
            state
                .volume(&volume_id)
                .map_or(0, |stored| stored.entry.generation),
        )
    }

    /// The volume's LV placement in the simulated world.
    fn placement(&self, path: &str) -> Vec<String> {
        self.lvm.lock().expect("world lock").placement_of(path)
    }
}

/// One daemon incarnation: the full composition, served over TCP.
struct Daemon {
    addr: SocketAddr,
    /// The provider (the crash row arms its store seam between
    /// requests; the retry row drives its reconcile pass).
    provider: Arc<LvmProvider>,
    serve: Option<JoinHandle<()>>,
}

impl Daemon {
    /// Launch over `world`.
    async fn launch(world: Arc<World>) -> Daemon {
        let provider = world.provider();
        let admin: Arc<dyn AdminSurface> = provider.clone();
        let state = AppState::new(
            Arc::clone(&provider) as Arc<dyn VolumeProvider>,
            Some(admin),
            Journal::open(&world.journal_dir).expect("journal opens"),
            Some(ADMIN_TOKEN.to_owned()),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind daemon");
        let addr = listener.local_addr().expect("daemon local addr");
        let app = router(Arc::new(state), 1 << 20);
        let serve = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("daemon serves");
        });
        Daemon {
            addr,
            provider,
            serve: Some(serve),
        }
    }

    /// Launch over `world` with the move retry task running at
    /// `tick` (the runtime's `spawn_move_retry_task` shape — the
    /// blocking-pool pass, the await, the sleep — with the test's
    /// tick instead of the 5 s production one).
    async fn launch_with_move_retry(world: Arc<World>, tick: Duration) -> (Daemon, JoinHandle<()>) {
        let daemon = Self::launch(world).await;
        let provider = Arc::clone(&daemon.provider);
        let retry = tokio::spawn(async move {
            loop {
                let provider = Arc::clone(&provider);
                let _ = tokio::task::spawn_blocking(move || provider.move_reconcile_pass()).await;
                tokio::time::sleep(tick).await;
            }
        });
        (daemon, retry)
    }

    /// Arm the provider's durable-state save seam at `point` (the
    /// crash row: the next `lvm_state` save at that point dies
    /// in-band, after firing `kill`).
    fn arm_store_crash(&self, point: StoreSavePoint, kill: KillSwitch) {
        let hooks = self.provider.store_crash_hooks();
        hooks.set_kill_switch(kill);
        hooks.arm(STORE_LVM_STATE, point);
    }

    /// Stop serving (the journal flock frees with the serve task).
    async fn stop(&mut self) {
        if let Some(serve) = self.serve.take() {
            serve.abort();
            let _ = serve.await;
        }
    }
}

// ------------------------------------------------------ row helpers

/// Claim the fixture disk, create one volume, and attach it — the
/// full HTTP lifecycle every row starts from. Leaves the volume at
/// generation 2 (attached) with its extents on the claimed disk's
/// by-id path.
async fn claim_create_attach(daemon: &Daemon) {
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
         \"project_id\":\"move-e2e\",\"volume_id\":\"vol-move\",\
         \"class\":\"native-local\",\"size_bytes\":{GIB}}}"
    );
    let (status, body) = admin("POST", daemon.addr, "/v2/volumes", Some(&create)).await;
    assert_eq!(status, 200, "create: {body}");

    let attach = "{\"api_version\":\"volvisor.volume.v2\",\"operation_id\":\"op-attach\",\
                  \"vm_id\":\"vm-1\",\"host_id\":\"host-1\",\"attachment_id\":\"att-1\",\
                  \"expected_volume_generation\":1}";
    let (status, body) = admin(
        "POST",
        daemon.addr,
        "/v2/volumes/vol-move/attach",
        Some(attach),
    )
    .await;
    assert_eq!(status, 200, "attach: {body}");
}

/// One move request body.
fn move_body(operation_id: &str, target: &str, expected_generation: u64) -> String {
    format!(
        "{{\"api_version\":\"volvisor.volume.v2\",\"operation_id\":\"{operation_id}\",\
         \"target_pool_id\":\"{target}\",\"expected_generation\":{expected_generation}}}"
    )
}

/// One move request against the daemon.
async fn move_call(
    daemon: &Daemon,
    operation_id: &str,
    target: &str,
    expected_generation: u64,
) -> (u16, String) {
    admin(
        "POST",
        daemon.addr,
        "/v2/volumes/vol-move/move-backing",
        Some(&move_body(operation_id, target, expected_generation)),
    )
    .await
}

/// The volume's LV path in the world (the placement assertion
/// surface).
fn world_lv_path(world: &World) -> String {
    let state = LvmState::load(&world.state_path()).expect("load state");
    let volume_id = VolumeId::new("vol-move").expect("valid volume id");
    let stored = state.volume(&volume_id).expect("the volume");
    format!("{}/{}", stored.entry.vg_name, stored.entry.lv_name)
}

// -------------------------------------------------------------- rows

/// The full lifecycle through the real route: the attached volume's
/// extents evacuate to the companion PV, the response reports the
/// verified completion with exactly one generation bump, the
/// durable record and the world agree, and one `pvmove` ran. The
/// same `operation_id` then replays the outcome byte-identically
/// with no second `pvmove`.
#[tokio::test]
async fn a_move_completes_through_the_real_api_and_replays_terminally() {
    let world = World::new();
    let mut daemon = Daemon::launch(Arc::clone(&world)).await;
    claim_create_attach(&daemon).await;
    let lv_path = world_lv_path(&world);
    let source = world.placement(&lv_path)[0].clone();
    assert_ne!(source, MOVE_TARGET_PV, "created on the source PV");

    let (status, body) = move_call(&daemon, "op-move-1", MOVE_TARGET_PV, 2).await;
    assert_eq!(status, 200, "move: {body}");
    let value = body_json(&body);
    assert_eq!(value["state"], "COMPLETE", "the move: {body}");
    assert_eq!(
        value["generation"], 3,
        "one bump over the attached 2: {body}"
    );
    assert_eq!(value["source_pv"], source);
    assert_eq!(value["target_pv"], MOVE_TARGET_PV);
    assert_eq!(value["detail"], serde_json::Value::Null);

    // The durable record and the world agree with the response.
    let (record, generation) = world.recorded_move("vol-move");
    assert_eq!(
        record.expect("the record").state,
        volvisor_types::MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 3);
    assert_eq!(
        world.placement(&lv_path),
        vec![MOVE_TARGET_PV.to_owned()],
        "the extents really relocated"
    );

    // The same operation replays the journaled outcome
    // byte-identically: no provider re-execution, no second pvmove.
    let (replay_status, replay_body) = move_call(&daemon, "op-move-1", MOVE_TARGET_PV, 2).await;
    assert_eq!(replay_status, 200);
    assert_eq!(replay_body, body, "the Terminal replay is byte-identical");
    assert_eq!(
        world.placement(&lv_path),
        vec![MOVE_TARGET_PV.to_owned()],
        "still exactly one relocation"
    );
    let (_, generation) = world.recorded_move("vol-move");
    assert_eq!(generation, 3, "no second bump for the replay");

    daemon.stop().await;
}

/// An in-flight outcome is truthful and journaled: the held move
/// exhausts the window and answers `COPYING`; the same
/// `operation_id` replays that observation byte-identically (it is
/// the recorded outcome of that call, not a re-drive); a fresh
/// `operation_id` re-attaches to the same kernel-side move and
/// completes it — one generation bump across the whole saga.
#[tokio::test]
async fn a_copying_outcome_replays_and_a_fresh_operation_re_attaches() {
    let world = World::new();
    let mut daemon = Daemon::launch(Arc::clone(&world)).await;
    claim_create_attach(&daemon).await;
    let lv_path = world_lv_path(&world);
    world.lvm.lock().expect("world lock").hold_moves = true;

    let (status, body) = move_call(&daemon, "op-move-1", MOVE_TARGET_PV, 2).await;
    assert_eq!(status, 200, "move: {body}");
    let value = body_json(&body);
    assert_eq!(
        value["state"], "COPYING",
        "the honest in-flight answer: {body}"
    );
    assert_eq!(value["generation"], 2, "no bump while copying: {body}");
    assert!(
        value["detail"]
            .as_str()
            .expect("detail")
            .contains("supervision window expired"),
        "the detail names the window: {body}"
    );
    assert!(
        world
            .lvm
            .lock()
            .expect("world lock")
            .moves
            .contains_key(&lv_path),
        "the pvmove is still running outside the daemon"
    );

    // The same operation_id replays the journaled COPYING outcome
    // byte-identically — the Terminal replay of a 200.
    let (replay_status, replay_body) = move_call(&daemon, "op-move-1", MOVE_TARGET_PV, 2).await;
    assert_eq!(replay_status, 200);
    assert_eq!(replay_body, body, "the COPYING outcome replays verbatim");

    // A fresh operation re-attaches to the same move and completes.
    world.lvm.lock().expect("world lock").hold_moves = false;
    let (status, body) = move_call(&daemon, "op-move-2", MOVE_TARGET_PV, 2).await;
    assert_eq!(status, 200, "re-attach: {body}");
    let value = body_json(&body);
    assert_eq!(
        value["state"], "COMPLETE",
        "the re-attached move completes: {body}"
    );
    assert_eq!(
        value["generation"], 3,
        "one bump across both operations: {body}"
    );
    assert_eq!(world.placement(&lv_path), vec![MOVE_TARGET_PV.to_owned()]);
    let (_, generation) = world.recorded_move("vol-move");
    assert_eq!(generation, 3);

    daemon.stop().await;
}

/// A different target while a move is active is the typed conflict
/// through the HTTP surface — a 409 naming the active move, with
/// nothing journaled for the refused operation and the original
/// move still completable.
#[tokio::test]
async fn an_active_move_conflict_is_a_typed_409_through_http() {
    let world = World::new();
    let mut daemon = Daemon::launch(Arc::clone(&world)).await;
    claim_create_attach(&daemon).await;
    world.lvm.lock().expect("world lock").hold_moves = true;

    let (status, body) = move_call(&daemon, "op-move-1", MOVE_TARGET_PV, 2).await;
    assert_eq!(status, 200, "the first move is in flight: {body}");
    assert_eq!(body_json(&body)["state"], "COPYING");

    // A different target: the typed conflict, reconstructed as 409
    // through the error surface (the ALL_CODES discipline).
    let (status, body) = move_call(&daemon, "op-move-2", "/dev/pv-c", 2).await;
    assert_eq!(status, 409, "the conflict: {body}");
    let value = body_json(&body);
    assert_eq!(value["code"], "INVALID_STATE", "the typed code: {body}");
    assert!(
        value["message"]
            .as_str()
            .expect("message")
            .contains("already active"),
        "the refusal names the active move: {body}"
    );

    // The original move still completes when released.
    world.lvm.lock().expect("world lock").hold_moves = false;
    let (status, body) = move_call(&daemon, "op-move-3", MOVE_TARGET_PV, 2).await;
    assert_eq!(status, 200, "re-attach after the conflict: {body}");
    assert_eq!(body_json(&body)["state"], "COMPLETE");

    daemon.stop().await;
}

/// The durable-boundary daemon death: the request dies in-band at
/// the armed `lvm_state` save (after the rename — the record IS
/// `PREPARING` durably), nothing in the world moved (the pvmove
/// never started), and a fresh incarnation over the same durable
/// state re-drives the journaled intent to `COMPLETE` — exactly one
/// `pvmove` and one generation bump across both incarnations.
#[tokio::test]
async fn a_daemon_death_at_the_preparing_save_re_drives_on_a_fresh_incarnation() {
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
    let mut daemon = Daemon::launch(Arc::clone(&world)).await;
    claim_create_attach(&daemon).await;
    let lv_path = world_lv_path(&world);
    let source = world.placement(&lv_path)[0].clone();

    let killed = Arc::new(AtomicBool::new(false));
    let kill = {
        let killed = Arc::clone(&killed);
        Arc::new(move || killed.store(true, Ordering::SeqCst)) as KillSwitch
    };
    daemon.arm_store_crash(StoreSavePoint::AfterRename, kill);

    // The move request dies at the PREPARING save.
    let status = http_lossy(
        "POST",
        daemon.addr,
        "/v2/volumes/vol-move/move-backing",
        &move_body("op-move-crash", MOVE_TARGET_PV, 2),
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

    // What a reload sees, observed directly on the durable file and
    // the world: the record says PREPARING, the extents never left
    // the source, no pvmove is running.
    let (record, generation) = world.recorded_move("vol-move");
    let record = record.expect("the dead incarnation's record");
    assert_eq!(
        record.state,
        volvisor_types::MoveVolumeBackingState::Preparing
    );
    assert_eq!(generation, 2, "nothing was freed by the death");
    assert_eq!(
        world.placement(&lv_path),
        vec![source.clone()],
        "the extents never left the source"
    );
    assert!(
        world.lvm.lock().expect("world lock").moves.is_empty(),
        "the pvmove never started"
    );

    // The fresh incarnation (unarmed) resolves the journaled intent:
    // a new consumer operation re-drives it to COMPLETE.
    daemon.stop().await;
    let mut fresh = Daemon::launch(Arc::clone(&world)).await;
    let (status, body) = move_call(&fresh, "op-move-2", MOVE_TARGET_PV, 2).await;
    assert_eq!(status, 200, "the re-driven move: {body}");
    let value = body_json(&body);
    assert_eq!(value["state"], "COMPLETE", "the re-drive completes: {body}");
    assert_eq!(
        value["generation"], 3,
        "one bump across both incarnations: {body}"
    );
    let (record, generation) = world.recorded_move("vol-move");
    assert_eq!(
        record.expect("the record").state,
        volvisor_types::MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 3);
    assert_eq!(world.placement(&lv_path), vec![MOVE_TARGET_PV.to_owned()]);

    fresh.stop().await;
}

/// The retry reconcile task (the runtime's
/// `spawn_move_retry_task` shape at a short tick) owns the
/// journaled-move lifecycle between consumer requests: a parked
/// `COPYING` record — the window expired, the consumer walked away
/// — completes on the task's pass once the world lets the move
/// land, observed on the durable record before any consumer asks.
#[tokio::test]
async fn the_retry_task_completes_a_parked_copying_record() {
    let world = World::new();
    let (mut daemon, retry) =
        Daemon::launch_with_move_retry(Arc::clone(&world), Duration::from_millis(50)).await;
    claim_create_attach(&daemon).await;
    let lv_path = world_lv_path(&world);
    world.lvm.lock().expect("world lock").hold_moves = true;

    let (status, body) = move_call(&daemon, "op-move-1", MOVE_TARGET_PV, 2).await;
    assert_eq!(status, 200, "move: {body}");
    assert_eq!(body_json(&body)["state"], "COPYING");
    let (record, generation) = world.recorded_move("vol-move");
    assert_eq!(
        record.expect("the record").state,
        volvisor_types::MoveVolumeBackingState::Copying
    );
    assert_eq!(generation, 2);

    // Release the world: the task's next pass observes the landing
    // and completes the record — the durable truth, before any
    // consumer asks.
    world.lvm.lock().expect("world lock").hold_moves = false;
    let mut completed = false;
    for _ in 0..200 {
        let (record, generation) = world.recorded_move("vol-move");
        if record
            .is_some_and(|record| record.state == volvisor_types::MoveVolumeBackingState::Complete)
            && generation == 3
        {
            completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        completed,
        "the retry task completed the parked record (state: {:?})",
        world.recorded_move("vol-move").0.map(|record| record.state)
    );
    assert_eq!(
        world.placement(&lv_path),
        vec![MOVE_TARGET_PV.to_owned()],
        "the extents relocated under the task's authority"
    );

    // A later consumer operation observes the completed truth
    // idempotently — no new move, no second bump.
    let (status, body) = move_call(&daemon, "op-move-2", MOVE_TARGET_PV, 3).await;
    assert_eq!(status, 200, "idempotent observation: {body}");
    let value = body_json(&body);
    assert_eq!(value["state"], "COMPLETE");
    assert_eq!(value["generation"], 3);

    retry.abort();
    daemon.stop().await;
}
