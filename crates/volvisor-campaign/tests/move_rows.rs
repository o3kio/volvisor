//! P6-C scenario row 16 (the post-P5 implementation plan's
//! sibling-harness clause; P5 plan §9's evidence discipline): the
//! same-VG move's **durable boundaries** under kill — every
//! journaled boundary of `MoveVolumeBackingOnline` (the journal
//! append, the PREPARING save, the pvmove start, the
//! completion/verification save) plus the out-of-band abort class,
//! each recovered by restart → reconcile → intent-resolution and
//! checked against the contract's state vocabulary.
//!
//! This is the sibling harness the completion gate names: the DRBD
//! nearline rig has no move path, so the row drives a **real
//! `LvmProvider` over a scripted LVM world** through the full
//! daemon composition (axum over TCP, the real journal, the real
//! durable state file) with the campaign's `Evidence` records —
//! same discipline by construction, no shared rig.
//!
//! The boundaries and their recovery shapes (the crash model's two
//! survivors are the durable record and the `lvs`-observable
//! world; the kernel dm mirror and the backgrounded `pvmove` live
//! outside the daemon):
//!
//! - **journal intent** — the request dies after the intent
//!   append: nothing durable anywhere; a fresh operation drives the
//!   move and completes it;
//! - **the PREPARING save** — the record is durable, the world
//!   untouched: a fresh incarnation re-drives the journaled intent
//!   (exactly one `pvmove` and one generation bump across
//!   incarnations);
//! - **the pvmove start** — the record still says `PREPARING`
//!   while the mirror runs outside: the restart rolls it to
//!   `COPYING` and the intent resolves to `COMPLETE`;
//! - **the verification save** — the world relocated (the source
//!   extents freed by the pvmove itself) while the journal lags at
//!   `COPYING`: the restart completes it under reconciled
//!   authority, one bump;
//! - **after the COMPLETE save** — the record is durable: the
//!   restart does nothing and a fresh operation observes the
//!   completed truth idempotently;
//! - **the out-of-band abort while down** — the record `COPYING`,
//!   the world aborted back to the source: the restart parks
//!   `IN_DOUBT` and a fresh operation refuses typed, the source
//!   intact, no bump.
//!
//! Every assertion reads the durable state file, the scripted
//! world, or the HTTP surface — never provider internals. Every
//! scenario emits its §6 evidence record (with the LVM rig's own
//! components label — no `witness` claim for a rig without one).

// Test target (the e2e precedent): invariant assertions may
// expect/unwrap; the rig's helpers are already bounded.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use volvisor_api::crash::CrashPoint;
use volvisor_api::op_kinds::OP_MOVE_VOLUME_BACKING;
use volvisor_api::{AppState, router};
use volvisor_campaign::evidence::Evidence;
use volvisor_journal::Journal;
use volvisor_lvm::LvmProvider;
use volvisor_lvm::state::{LvmState, MoveRecord};
use volvisor_provider::{AdminSurface, CommandOutput, CommandRunner, FakeRunner, VolumeProvider};
use volvisor_types::MoveVolumeBackingState;
use volvisor_types::crash::{KillSwitch, STORE_LVM_STATE, StoreSavePoint};
use volvisor_types::id::VolumeId;

// ---------------------------------------------------------- constants

/// The admin bearer token (fail-closed auth on every mutating route).
const ADMIN_TOKEN: &str = "row16-admin-token";
/// The device-claim destructive-authorization token.
const CLAIM_TOKEN: &str = "row16-claim-token";
/// One gibibyte (extent-aligned under the fixture's 4-MiB extents).
const GIB: u64 = 1 << 30;
/// Simulated pool capacity (1 TiB).
const POOL_BYTES: u64 = 1 << 40;
/// Simulated physical extent size (4 MiB), as in real LVM.
const EXTENT_BYTES: u64 = 4 << 20;
/// The companion target PV every claimed VG carries (the
/// evacuation destination; the source is the claimed disk's by-id
/// path).
const MOVE_TARGET_PV: &str = "/dev/pv-b";
/// The moved volume (one per rig; the rows share its name).
const VOLUME: &str = "vol-row-16";

// --------------------------------------------------- the panic filter

/// This binary's crash-injection filter (the rig's precedent): a
/// firing hook terminates its request with a
/// `volvisor_api::CRASH_PANIC_PREFIX` payload — an injected kill,
/// not a failure — so those panics print nothing while every real
/// panic still reaches the previous hook.
static PANIC_FILTER: Once = Once::new();

fn install_panic_filter() {
    PANIC_FILTER.call_once(|| {
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

// ------------------------------------------------- scripted LVM world

/// The compact simulated LVM world this row needs (the
/// `volvisord` move e2e's world, the `FakeLvm` test kit's
/// trimmed sibling): LVs with per-PV placement, a per-PV capacity
/// model, and the `pvmove` simulation whose deterministic clock
/// advances on every `lvs` query — the shapes verified against
/// real LVM 2.03.16 (mid-move the LV row's `devices` column
/// references the `pvmove0` mirror segment, `copy_percent` stays
/// empty, and a re-run attaches to a running move with exit 0
/// while a start with nothing on the source refuses
/// `No data to move`, exit 5).
struct ScriptedLvm {
    /// LV full path (`vg/lv`) to size in bytes.
    lvs: BTreeMap<String, u64>,
    /// LV path to its backing PVs (the placement model).
    lv_devices: BTreeMap<String, Vec<String>>,
    /// PV device paths known to `pvs`.
    pvs: Vec<String>,
    /// PV path to its volume group.
    pv_vg: BTreeMap<String, String>,
    /// PV path to free bytes.
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

    /// The scripted runner wired to this world (closure mode —
    /// invocations are recorded for the argv/count assertions).
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

/// The `lvs` report rows — and the deterministic move clock.
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
            let path = arg_after(args, "-n")?.to_owned();
            let source = (*args.get(args.len().saturating_sub(2))?).to_owned();
            let target = (*args.last()?).to_owned();
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
            let vg = (*args.get(1)?).to_owned();
            world.vg_free.insert(vg.clone(), 2 * POOL_BYTES);
            world.vg_size.insert(vg.clone(), 2 * POOL_BYTES);
            let claimed = (*args.last()?).to_owned();
            world.add_pv_to_vg(&claimed, &vg);
            world.add_pv_to_vg(MOVE_TARGET_PV, &vg);
            Some(CommandOutput::success(String::new()))
        }
        _ => None,
    }
}

// ------------------------------------------------------ HTTP client

/// Hand-rolled minimal HTTP/1.1 client (the e2e precedent): one
/// fresh connection per request (`connection: close`).
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

/// The lossy variant for the kill rows: the handler dies
/// mid-request, so the connection breaks — `None` is the honest
/// observation, not a test failure.
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

/// The durable world several daemon incarnations share: one
/// directory tree, one scripted LVM, one sysfs root carrying the
/// stable by-id identity the claim resolves.
struct MoveRig {
    /// Kept alive for the rig's lifetime.
    _dir: tempfile::TempDir,
    /// The simulated LVM.
    lvm: Arc<Mutex<ScriptedLvm>>,
    /// The sysfs root (the by-id symlink the discovery resolves).
    sysfs_root: PathBuf,
    /// The journal directory (journal + LVM state).
    journal_dir: PathBuf,
    /// The scripted runner (the pvmove-count assertions).
    runner: Arc<FakeRunner>,
}

impl MoveRig {
    fn new() -> Arc<Self> {
        install_panic_filter();
        let dir = tempfile::tempdir().expect("tempdir");
        // The stable hardware identity: the discovery resolves the
        // fixture disk's WWN through the by-id index, so the claimed
        // PV (and the lvcreate placement) is the by-id path, never
        // the volatile /dev/sda name (SPEC-0002 section 3).
        let by_id = dir.path().join("sysroot/dev/disk/by-id");
        std::fs::create_dir_all(&by_id).expect("by-id dir");
        std::os::unix::fs::symlink("../../sda", by_id.join("wwn-0x5000c500fixt0001"))
            .expect("by-id symlink");
        let lvm = Arc::new(Mutex::new(ScriptedLvm::new()));
        let runner = ScriptedLvm::runner(&lvm);
        Arc::new(Self {
            journal_dir: dir.path().join("journal"),
            sysfs_root: dir.path().join("sysroot"),
            lvm,
            runner,
            _dir: dir,
        })
    }

    /// The durable LVM state file path (the runtime's convention).
    fn state_path(&self) -> PathBuf {
        self.journal_dir.join("lvm-state.json")
    }

    /// The provider over the durable state file and the scripted
    /// LVM, with the row's fast move timing (a 10 ms poll, a 250 ms
    /// supervision window: a held move exhausts the window, an
    /// unheld one completes within two polls).
    fn provider(&self) -> Arc<LvmProvider> {
        let runner: Arc<dyn CommandRunner> = Arc::clone(&self.runner) as Arc<dyn CommandRunner>;
        LvmProvider::new(
            runner,
            self.state_path(),
            self.sysfs_root.clone(),
            "vvrow16".to_owned(),
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

    /// The volume's (move record, generation) from the durable
    /// state file — the truth a reload sees, read directly.
    fn recorded_move(&self) -> (Option<MoveRecord>, u64) {
        let volume_id = VolumeId::new(VOLUME).expect("valid volume id");
        let state = LvmState::load(&self.state_path()).expect("load state");
        (
            state.move_record(&volume_id).cloned(),
            state
                .volume(&volume_id)
                .map_or(0, |stored| stored.entry.generation),
        )
    }

    /// The volume's LV path in the world.
    fn lv_path(&self) -> String {
        let state = LvmState::load(&self.state_path()).expect("load state");
        let volume_id = VolumeId::new(VOLUME).expect("valid volume id");
        let stored = state.volume(&volume_id).expect("the volume");
        format!("{}/{}", stored.entry.vg_name, stored.entry.lv_name)
    }

    /// The volume's LV placement in the simulated world.
    fn placement(&self) -> Vec<String> {
        let path = self.lv_path();
        self.lvm
            .lock()
            .expect("world lock")
            .lv_devices
            .get(&path)
            .cloned()
            .unwrap_or_default()
    }

    /// How many `pvmove` invocations ran (the no-double-execution
    /// observable).
    fn pvmove_count(&self) -> usize {
        self.runner
            .invocations()
            .iter()
            .filter(|invocation| invocation.program == "pvmove")
            .count()
    }
}

/// One daemon incarnation: the full composition, served over TCP.
struct Daemon {
    addr: SocketAddr,
    /// The provider (the rows arm its store seam between requests
    /// and drive its reconcile pass).
    provider: Arc<LvmProvider>,
    /// The journal-append crash hooks (armed between requests).
    journal_crash: Arc<volvisor_api::CrashHooks>,
    serve: Option<JoinHandle<()>>,
}

impl Daemon {
    /// Launch over `rig` (a fresh incarnation: fresh provider
    /// construction — the startup reconcile classifies any journaled
    /// move — fresh journal replay, fresh AppState).
    async fn launch(rig: &Arc<MoveRig>) -> Daemon {
        let provider = rig.provider();
        let admin: Arc<dyn AdminSurface> = provider.clone();
        let journal_crash = Arc::new(volvisor_api::CrashHooks::new());
        let state = AppState::new(
            Arc::clone(&provider) as Arc<dyn VolumeProvider>,
            Some(admin),
            Journal::open(&rig.journal_dir).expect("journal opens"),
            Some(ADMIN_TOKEN.to_owned()),
        )
        .with_crash_hooks(Arc::clone(&journal_crash));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind daemon");
        let addr = listener.local_addr().expect("daemon local addr");
        let app = router(Arc::new(state), 1 << 20);
        let serve = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("daemon serves");
        });
        Daemon {
            addr,
            provider,
            journal_crash,
            serve: Some(serve),
        }
    }

    /// Arm the journal-append seam for one operation kind (the
    /// request dies in-band at the point, after firing `kill`).
    fn arm_journal(&self, point: CrashPoint, kill: KillSwitch) {
        self.journal_crash.set_kill_switch(kill);
        self.journal_crash.arm(OP_MOVE_VOLUME_BACKING, point);
    }

    /// Arm the provider's durable-state save seam at `point` (the
    /// request or pass dies in-band at the point, after firing
    /// `kill`).
    fn arm_store(&self, point: StoreSavePoint, kill: KillSwitch) {
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

/// A kill switch that only marks (the panic IS the process death;
/// the rig stops the incarnation and starts a fresh one over the
/// same durable surfaces).
fn marking_kill() -> (Arc<AtomicBool>, KillSwitch) {
    let killed = Arc::new(AtomicBool::new(false));
    let switch = {
        let killed = Arc::clone(&killed);
        Arc::new(move || killed.store(true, Ordering::SeqCst)) as KillSwitch
    };
    (killed, switch)
}

/// Drive the retry reconcile pass in the runtime task's shape
/// (`spawn_blocking` + await) until it dies at the armed boundary:
/// the `JoinError` is the task-death observation. Bounded by the
/// world's deterministic clock (two advancing observations land a
/// move; the completing pass is the one that saves — and dies).
async fn drive_passes_until_kill(provider: &Arc<LvmProvider>) -> usize {
    let mut passes = 0;
    loop {
        passes += 1;
        let provider = Arc::clone(provider);
        let attempt = tokio::task::spawn_blocking(move || provider.move_reconcile_pass()).await;
        if attempt.is_err() {
            return passes;
        }
        assert!(passes < 10, "the armed kill never fired");
    }
}

// ------------------------------------------------------ row helpers

/// Claim the fixture disk, create one volume, and attach it — the
/// full HTTP lifecycle every row starts from. Leaves the volume at
/// generation 2 (attached) with its extents on the claimed disk's
/// by-id path.
async fn seed(daemon: &Daemon) {
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
         \"project_id\":\"row-16\",\"volume_id\":\"{VOLUME}\",\
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
        &format!("/v2/volumes/{VOLUME}/attach"),
        Some(attach),
    )
    .await;
    assert_eq!(status, 200, "attach: {body}");
}

/// One move request body.
fn move_body(operation_id: &str, expected_generation: u64) -> String {
    format!(
        "{{\"api_version\":\"volvisor.volume.v2\",\"operation_id\":\"{operation_id}\",\
         \"target_pool_id\":\"{MOVE_TARGET_PV}\",\"expected_generation\":{expected_generation}}}"
    )
}

/// One move request against the daemon (the rig always targets the
/// companion PV).
async fn move_call(daemon: &Daemon, operation_id: &str, expected_generation: u64) -> (u16, String) {
    admin(
        "POST",
        daemon.addr,
        &format!("/v2/volumes/{VOLUME}/move-backing"),
        Some(&move_body(operation_id, expected_generation)),
    )
    .await
}

/// The move request that dies at an armed boundary (the lossy
/// client: the connection breaking IS the daemon-death shape). The
/// requests that die carry the pre-move generation (2): every kill
/// in this row fires before a bump could land.
async fn move_lossy(daemon: &Daemon, operation_id: &str) -> Option<u16> {
    http_lossy(
        "POST",
        daemon.addr,
        &format!("/v2/volumes/{VOLUME}/move-backing"),
        &move_body(operation_id, 2),
    )
    .await
}

/// The LVM rig's components claim (§6: label what actually ran —
/// no `witness` entry for a rig without one).
fn lvm_components() -> serde_json::Value {
    json!({
        "volvisord": env!("CARGO_PKG_VERSION"),
        "lvm-tooling": "fake-scripted (LVM 2.03.16 shapes)",
        "witness": serde_json::Value::Null,
    })
}

// -------------------------------------------------------------- rows

/// The journal-append boundary: the request dies after the intent
/// append, before anything ran — no move record, nothing in the
/// world, the generation untouched. A fresh incarnation replays the
/// dangling intent without wedging, and a fresh operation drives
/// the move to `COMPLETE` with one bump.
#[tokio::test]
async fn move_killed_after_journal_intent() {
    let rig = MoveRig::new();
    // The evidence record opens with the rig (its duration is the
    // scenario's wall time, §6).
    let mut evidence = Evidence::new("row-16/move-killed-after-journal-intent");
    evidence
        .fault("kill", "journal:AfterIntent(move_volume_backing)")
        .components(lvm_components());
    let mut daemon = Daemon::launch(&rig).await;
    seed(&daemon).await;
    let source = rig.placement()[0].clone();

    let (killed, kill) = marking_kill();
    daemon.arm_journal(CrashPoint::AfterIntent, kill);
    let status = move_lossy(&daemon, "op-move-1").await;
    assert_ne!(
        status,
        Some(200),
        "the request dies with the injected kill (observed: {status:?})"
    );
    assert!(killed.load(Ordering::SeqCst), "the kill fired in-band");

    // The durable truth at the death: a journal intent with no
    // outcome, no move record, the world untouched.
    let (record, generation) = rig.recorded_move();
    assert!(record.is_none(), "no move record without a provider run");
    assert_eq!(generation, 2);
    assert_eq!(rig.placement(), vec![source.clone()], "nothing moved");

    // The fresh incarnation resolves the intent with a fresh
    // operation id (the consumer recovery path).
    daemon.stop().await;
    let mut fresh = Daemon::launch(&rig).await;
    let (status, body) = move_call(&fresh, "op-move-2", 2).await;
    assert_eq!(status, 200, "the recovered move: {body}");
    let value = body_json(&body);
    assert_eq!(value["state"], "COMPLETE", "the move: {body}");
    assert_eq!(value["generation"], 3, "one bump: {body}");
    assert_eq!(rig.placement(), vec![MOVE_TARGET_PV.to_owned()]);
    assert_eq!(rig.pvmove_count(), 1, "exactly one pvmove ran");
    fresh.stop().await;

    evidence
        .invariant(
            "no record without an outcome",
            "the journal holds the intent; the LVM state holds no move record",
        )
        .invariant(
            "the world untouched",
            "the extents never left the source PV",
        )
        .invariant(
            "replay does not wedge",
            "the fresh incarnation opened the journal with the dangling intent and served",
        )
        .invariant(
            "intent resolution",
            "a fresh operation id drove the move to COMPLETE with one generation bump and one pvmove",
        )
        .outcome("recovered: COMPLETE (the fresh operation re-drove the journaled intent)");
    evidence.finish_single_daemon(&rig.journal_dir);
}

/// The PREPARING-save boundary (after the rename — the record IS
/// durable): the request dies with the journaled intent in place
/// and nothing in the world. A fresh incarnation re-drives it —
/// exactly one `pvmove` and one generation bump across both
/// incarnations.
#[tokio::test]
async fn move_killed_at_preparing_save() {
    let rig = MoveRig::new();
    // The evidence record opens with the rig (its duration is the
    // scenario's wall time, §6).
    let mut evidence = Evidence::new("row-16/move-killed-at-preparing-save");
    evidence
        .fault("kill", "lvm_state:AfterRename(PREPARING)")
        .components(lvm_components());
    let mut daemon = Daemon::launch(&rig).await;
    seed(&daemon).await;
    let source = rig.placement()[0].clone();

    let (killed, kill) = marking_kill();
    daemon.arm_store(StoreSavePoint::AfterRename, kill);
    let status = move_lossy(&daemon, "op-move-1").await;
    assert_ne!(status, Some(200), "the request dies at the PREPARING save");
    assert!(killed.load(Ordering::SeqCst), "the kill fired in-band");

    // The durable truth at the death: PREPARING, nothing started.
    let (record, generation) = rig.recorded_move();
    let record = record.expect("the dead incarnation's record");
    assert_eq!(record.state, MoveVolumeBackingState::Preparing);
    assert_eq!(generation, 2, "nothing was freed by the death");
    assert!(rig.lvm.lock().expect("world").moves.is_empty());
    assert_eq!(rig.placement(), vec![source.clone()]);

    // The fresh incarnation re-drives the journaled intent.
    daemon.stop().await;
    let mut fresh = Daemon::launch(&rig).await;
    let (status, body) = move_call(&fresh, "op-move-2", 2).await;
    assert_eq!(status, 200, "the re-driven move: {body}");
    let value = body_json(&body);
    assert_eq!(value["state"], "COMPLETE", "the re-drive completes: {body}");
    assert_eq!(
        value["generation"], 3,
        "one bump across incarnations: {body}"
    );
    assert_eq!(rig.placement(), vec![MOVE_TARGET_PV.to_owned()]);
    assert_eq!(
        rig.pvmove_count(),
        1,
        "the dead incarnation never started a pvmove"
    );
    fresh.stop().await;

    evidence
        .invariant(
            "the record is durable at the death",
            "PREPARING with the source and target journaled",
        )
        .invariant(
            "nothing started",
            "no pvmove in the world; the extents on the source",
        )
        .invariant(
            "re-drive, not re-invention",
            "the fresh operation re-used the journaled record and completed with one pvmove",
        )
        .invariant("one bump", "generation 3 exactly, across both incarnations")
        .outcome("recovered: COMPLETE (the re-driven journaled intent)");
    evidence.finish_single_daemon(&rig.journal_dir);
}

/// The pvmove-start boundary: the record still says `PREPARING`
/// while the mirror runs outside the daemon (the death lands
/// between the start and the `COPYING` save — the re-attach path
/// journals nothing until the start succeeds). A fresh incarnation
/// rolls the record to `COPYING` from the world and the intent
/// resolves to `COMPLETE`.
#[tokio::test]
async fn move_killed_after_pvmove_start() {
    let rig = MoveRig::new();
    // The evidence record opens with the rig (its duration is the
    // scenario's wall time, §6).
    let mut evidence = Evidence::new("row-16/move-killed-after-pvmove-start");
    evidence
        .fault(
            "kill",
            "lvm_state:AfterFsyncBeforeRename(COPYING; pvmove already started)",
        )
        .components(lvm_components());
    let mut first = Daemon::launch(&rig).await;
    seed(&first).await;

    // Death one: at the PREPARING save (the record durable, the
    // world clean) — the setup for the boundary under test.
    let (killed_one, kill_one) = marking_kill();
    first.arm_store(StoreSavePoint::AfterRename, kill_one);
    let status = move_lossy(&first, "op-move-1").await;
    assert_ne!(status, Some(200), "the first death lands as designed");
    assert!(killed_one.load(Ordering::SeqCst));
    first.stop().await;

    // Death two: between the pvmove start and the COPYING save. The
    // re-attach journals nothing until its start succeeds (an
    // unchanged record has no durable boundary to write), so the
    // armed after-fsync point fires on the COPYING save — with the
    // mirror already live.
    let mut second = Daemon::launch(&rig).await;
    let (killed_two, kill_two) = marking_kill();
    second.arm_store(StoreSavePoint::AfterFsyncBeforeRename, kill_two);
    let status = move_lossy(&second, "op-move-2").await;
    assert_ne!(
        status,
        Some(200),
        "the request dies between the start and the COPYING save"
    );
    assert!(killed_two.load(Ordering::SeqCst), "the kill fired in-band");

    // The durable truth at the death: PREPARING while the mirror
    // runs outside.
    let (record, generation) = rig.recorded_move();
    let record = record.expect("the record");
    assert_eq!(
        record.state,
        MoveVolumeBackingState::Preparing,
        "the journal lags the world"
    );
    assert_eq!(generation, 2);
    assert!(
        !rig.lvm.lock().expect("world").moves.is_empty(),
        "the pvmove survived the daemon (it lives outside)"
    );
    let source = rig.placement()[0].clone();
    second.stop().await;

    // The fresh incarnation classifies the record from the world:
    // the live mirror rolls it forward — to COPYING, or all the way
    // to COMPLETE when the constructor's own observations let the
    // deterministic clock land the move (both are the reconcile's
    // honest answers; the journal never keeps lagging at PREPARING
    // against a live mirror).
    let mut third = Daemon::launch(&rig).await;
    let (record, generation) = rig.recorded_move();
    assert_ne!(
        record.expect("the record").state,
        MoveVolumeBackingState::Preparing,
        "the startup reconcile caught the journal up with the live mirror"
    );
    let (status, body) = move_call(&third, "op-move-3", generation).await;
    assert_eq!(status, 200, "the resolved move: {body}");
    let value = body_json(&body);
    assert_eq!(value["state"], "COMPLETE", "the intent resolves: {body}");
    assert_eq!(
        value["generation"], 3,
        "one bump across three incarnations: {body}"
    );
    assert_eq!(rig.placement(), vec![MOVE_TARGET_PV.to_owned()]);
    assert_eq!(rig.pvmove_count(), 1, "exactly one pvmove ran");
    let _ = source;
    third.stop().await;

    evidence
        .invariant(
            "the journal lags the world",
            "the record says PREPARING while the mirror runs outside the daemon",
        )
        .invariant(
            "the startup reconcile rolls forward",
            "a fresh incarnation classified the live mirror and rolled the record to COPYING",
        )
        .invariant(
            "intent resolution",
            "the journaled intent resolved to COMPLETE with one bump and one pvmove",
        )
        .outcome("recovered: COMPLETE (the rolled record resolved under a fresh operation)");
    evidence.finish_single_daemon(&rig.journal_dir);
}

/// The completion/verification boundary: the world relocated (the
/// pvmove freed the source extents itself) while the journal lags
/// at `COPYING` — the retry pass dies inside its completing save.
/// A fresh incarnation completes the record under reconciled
/// authority with exactly one bump.
#[tokio::test]
async fn move_pass_killed_at_verification() {
    let rig = MoveRig::new();
    // The evidence record opens with the rig (its duration is the
    // scenario's wall time, §6).
    let mut evidence = Evidence::new("row-16/move-pass-killed-at-verification");
    evidence
        .fault(
            "kill",
            "lvm_state:AfterFsyncBeforeRename(COMPLETE; the pass's completing save)",
        )
        .components(lvm_components());
    let mut daemon = Daemon::launch(&rig).await;
    seed(&daemon).await;

    // Park the move in the window: the record is COPYING, the
    // mirror held at 0 percent.
    rig.lvm.lock().expect("world").hold_moves = true;
    let (status, body) = move_call(&daemon, "op-move-1", 2).await;
    assert_eq!(status, 200, "the parked move: {body}");
    assert_eq!(body_json(&body)["state"], "COPYING");

    // Arm the completing save's torn window and let the world move:
    // the pass (the retry task's shape) observes the landing,
    // verifies, bumps — and dies inside the save, before the rename.
    let (killed, kill) = marking_kill();
    daemon.arm_store(StoreSavePoint::AfterFsyncBeforeRename, kill);
    rig.lvm.lock().expect("world").hold_moves = false;
    let passes = drive_passes_until_kill(&daemon.provider).await;
    assert!(killed.load(Ordering::SeqCst), "the kill fired in-band");
    assert!(
        passes >= 2,
        "the completing pass is not the first (observed {passes})"
    );

    // The durable truth at the death: COPYING while the world
    // relocated (the source extents freed by the pvmove itself).
    let (record, generation) = rig.recorded_move();
    let record = record.expect("the record");
    assert_eq!(record.state, MoveVolumeBackingState::Copying);
    assert_eq!(generation, 2, "the dead pass's bump never landed");
    assert!(
        rig.lvm.lock().expect("world").moves.is_empty(),
        "the pvmove finished outside the daemon"
    );
    assert_eq!(
        rig.placement(),
        vec![MOVE_TARGET_PV.to_owned()],
        "the extents relocated while the journal lags"
    );
    daemon.stop().await;

    // The fresh incarnation completes the record under reconciled
    // authority — the roll-forward, one bump.
    let mut fresh = Daemon::launch(&rig).await;
    let (record, generation) = rig.recorded_move();
    assert_eq!(
        record.expect("the record").state,
        MoveVolumeBackingState::Complete,
        "the startup reconcile completed the provable relocation"
    );
    assert_eq!(generation, 3, "exactly one bump across the death");
    assert_eq!(rig.pvmove_count(), 1);
    fresh.stop().await;

    evidence
        .invariant(
            "the journal lags the relocated world",
            "the record says COPYING while the extents sit on the target (the source freed by the pvmove itself)",
        )
        .invariant(
            "the dead bump never landed",
            "generation 2 at the death — the pass's in-memory bump died with it",
        )
        .invariant(
            "roll-forward under reconciled authority",
            "the startup reconcile completed the provable relocation with exactly one bump",
        )
        .outcome("recovered: COMPLETE (the roll-forward at startup, one bump)");
    evidence.finish_single_daemon(&rig.journal_dir);
}

/// After the COMPLETE save (the record durable, the bump landed):
/// the pass dies immediately after its save. A fresh incarnation
/// does nothing — a completed record is a historical fact — and a
/// fresh operation observes the completed truth idempotently: no
/// second bump, no second `pvmove`.
#[tokio::test]
async fn move_pass_killed_after_complete_save() {
    let rig = MoveRig::new();
    // The evidence record opens with the rig (its duration is the
    // scenario's wall time, §6).
    let mut evidence = Evidence::new("row-16/move-pass-killed-after-complete-save");
    evidence
        .fault("kill", "lvm_state:AfterRename(COMPLETE)")
        .components(lvm_components());
    let mut daemon = Daemon::launch(&rig).await;
    seed(&daemon).await;

    rig.lvm.lock().expect("world").hold_moves = true;
    let (status, body) = move_call(&daemon, "op-move-1", 2).await;
    assert_eq!(status, 200, "the parked move: {body}");
    assert_eq!(body_json(&body)["state"], "COPYING");

    // Arm AFTER the rename: the completing save lands, then dies.
    let (killed, kill) = marking_kill();
    daemon.arm_store(StoreSavePoint::AfterRename, kill);
    rig.lvm.lock().expect("world").hold_moves = false;
    let _passes = drive_passes_until_kill(&daemon.provider).await;
    assert!(killed.load(Ordering::SeqCst), "the kill fired in-band");

    // The durable truth at the death: COMPLETE with the bump.
    let (record, generation) = rig.recorded_move();
    assert_eq!(
        record.expect("the record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 3, "the completing save carried the bump");
    daemon.stop().await;

    // The fresh incarnation does nothing; a fresh operation
    // observes the completed truth idempotently.
    let mut fresh = Daemon::launch(&rig).await;
    let (record, generation) = rig.recorded_move();
    assert_eq!(
        record.expect("the record").state,
        MoveVolumeBackingState::Complete
    );
    assert_eq!(generation, 3, "the startup reconcile touched nothing");
    let (status, body) = move_call(&fresh, "op-move-2", 3).await;
    assert_eq!(status, 200, "the idempotent observation: {body}");
    let value = body_json(&body);
    assert_eq!(value["state"], "COMPLETE");
    assert_eq!(value["generation"], 3, "no second bump");
    assert_eq!(rig.pvmove_count(), 1, "no second pvmove");
    fresh.stop().await;

    evidence
        .invariant(
            "the completed record is durable",
            "COMPLETE with the generation bump landed before the death",
        )
        .invariant(
            "a completed record is a historical fact",
            "the startup reconcile neither re-completed nor re-bumped it",
        )
        .invariant(
            "idempotent re-observation",
            "a fresh operation answered COMPLETE with no second bump and no second pvmove",
        )
        .outcome("recovered: COMPLETE (nothing to resolve — the durable fact stood)");
    evidence.finish_single_daemon(&rig.journal_dir);
}

/// The out-of-band abort class (no kill): the record is `COPYING`
/// when the daemon goes down, and an operator's `pvmove --abort`
/// lands while it is down. The fresh incarnation parks `IN_DOUBT`
/// (never a silent revert, never a generic failure), a fresh
/// operation refuses typed naming the park, the source extents are
/// intact, and the generation never bumped.
#[tokio::test]
async fn move_aborted_out_of_band_while_down() {
    let rig = MoveRig::new();
    // The evidence record opens with the rig (its duration is the
    // scenario's wall time, §6).
    let mut evidence = Evidence::new("row-16/move-aborted-out-of-band-while-down");
    evidence.components(lvm_components());
    let mut daemon = Daemon::launch(&rig).await;
    seed(&daemon).await;
    let source = rig.placement()[0].clone();

    rig.lvm.lock().expect("world").hold_moves = true;
    let (status, body) = move_call(&daemon, "op-move-1", 2).await;
    assert_eq!(status, 200, "the parked move: {body}");
    assert_eq!(body_json(&body)["state"], "COPYING");
    daemon.stop().await;

    // The out-of-band abort while the daemon is down: the mirror is
    // torn down and the placement restores to the source (the
    // verified `pvmove --abort` semantics).
    rig.lvm.lock().expect("world").moves.clear();

    // The fresh incarnation parks IN_DOUBT at startup.
    let mut fresh = Daemon::launch(&rig).await;
    let (record, generation) = rig.recorded_move();
    let record = record.expect("the record");
    assert_eq!(record.state, MoveVolumeBackingState::InDoubt);
    assert!(
        record
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("without relocating")),
        "the park names the un-relocated outcome: {:?}",
        record.detail
    );
    assert_eq!(generation, 2, "no bump for an unverified outcome");

    // A fresh operation refuses typed; the source keeps serving.
    let (status, body) = move_call(&fresh, "op-move-2", 2).await;
    assert_eq!(status, 409, "the parked record refuses: {body}");
    let value = body_json(&body);
    assert_eq!(value["code"], "INVALID_STATE", "the typed code: {body}");
    assert!(
        value["message"]
            .as_str()
            .expect("message")
            .contains("IN_DOUBT"),
        "the refusal names the park: {body}"
    );
    assert_eq!(
        rig.placement(),
        vec![source.clone()],
        "the source extents are intact and serving"
    );
    assert_eq!(rig.pvmove_count(), 1, "no new pvmove for a refusal");
    fresh.stop().await;

    evidence
        .fault(
            "out-of-band pvmove --abort",
            "while the daemon is down (record COPYING)",
        )
        .invariant(
            "the unknown outcome parks",
            "IN_DOUBT at the startup reconcile, never a silent revert or a generic failure",
        )
        .invariant(
            "fail-closed for new work",
            "a fresh operation refused typed (INVALID_STATE) naming the park",
        )
        .invariant(
            "the source intact",
            "the extents never left the source PV; the generation never bumped",
        )
        .outcome("parked: IN_DOUBT with the source intact (the operator resolves it)");
    evidence.finish_single_daemon(&rig.journal_dir);
}
