//! Shared fixtures for the `volvisor-lvm` integration tests.
//!
//! [`FakeLvm`] is a minimal stateful LVM simulation driven through the
//! closure mode of [`FakeRunner`]: LVs, PVs and VG free space live in a
//! shared map, `lvcreate`/`lvextend`/`lvremove` mutate it, and `lvs`/
//! `vgs`/`pvs` report it — so the provider's verification steps (sizes
//! read back from `lvs`) exercise real round-trips instead of echoes.
//!
//! The simulation mirrors real LVM semantics that matter to the provider:
//! `lvcreate`/`lvextend` round sizes **up** to the configured physical
//! extent size ([`FakeLvm::extent_size`], 4 MiB by default, also reported
//! as `vg_extent_size` by the simulated `vgs`), `lvcreate` fails on an LV
//! name that already exists, `lvremove` on a missing LV fails, and
//! `vgremove` on a missing VG fails.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention (see
//! the crate-level `cfg_attr(test)` in the library).

#![allow(dead_code)]
#![allow(clippy::expect_used, clippy::unwrap_used)]
// The simulated world is a fault-injection matrix; one bool per scripted
// failure is the clearest shape for test code.
#![allow(clippy::struct_excessive_bools)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use volvisor_lvm::provider::{LvmProvider, lv_name_for};
use volvisor_lvm::state::{DeviceEntry, LvmState};
use volvisor_lvm::{CommandOutput, CommandRunner, FakeRunner, RealRunner};
use volvisor_types::{ApiError, DeviceId, DeviceRole, VolumeId, VolumeLifecycle};

/// VG prefix used by the fixtures.
pub const VG_PREFIX: &str = "vvtest";
/// Destructive-authorization token used by the fixtures.
pub const AUTH_TOKEN: &str = "test-token";
/// The claimed device identity used by the conformance fixture.
pub const CLAIMED_DEVICE: &str = "dev-0123456789abcdef0123456789abcdef";
/// The VG name backing the conformance fixture pool.
pub const CLAIMED_VG: &str = "vvtest-abcdef01";
/// Simulated pool capacity (1 TiB).
pub const POOL_BYTES: u64 = 1 << 40;
/// Default simulated physical extent size (4 MiB), as in real LVM.
pub const EXTENT_BYTES: u64 = 4 << 20;

/// One simulated `pvmove` (keyed by LV path): the deterministic
/// relocation the fake world performs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeMove {
    /// The PV the extents are evacuated from.
    pub source: String,
    /// The PV the extents are evacuated to.
    pub target: String,
    /// Sync percentage (0–100). Advances by
    /// [`FakeLvm::move_advance_percent`] on every `lvs` query — the
    /// deterministic clock the drive's poll loop observes.
    pub percent: u64,
}

/// The simulated LVM world shared between the provider and assertions.
pub struct FakeLvm {
    /// LV full path (`vg/lv`) to size in bytes.
    pub lvs: BTreeMap<String, u64>,
    /// PV device paths known to `pvs`.
    pub pvs: Vec<String>,
    /// VG name to free bytes **before any LV allocations** (the baseline).
    /// The reported `vgs` free space is this baseline minus the sizes of
    /// all LVs currently present in the VG, mirroring real LVM where
    /// `lvcreate`/`lvextend` consume and `lvremove` returns space.
    pub vg_free: BTreeMap<String, u64>,
    /// VG name to total size in bytes.
    pub vg_size: BTreeMap<String, u64>,
    /// PV device path to its volume group (the `pvs` `vg_name` column).
    pub pv_vg: BTreeMap<String, String>,
    /// PV device path to total size in bytes.
    pub pv_size: BTreeMap<String, u64>,
    /// PV device path to free bytes — the per-PV capacity model:
    /// `lvcreate`/`lvextend` consume from the LV's placement PV, a
    /// completed move transfers the LV's extents source→target, and
    /// `lvremove` returns them. The same-VG move's target check reads
    /// exactly this.
    pub pv_free: BTreeMap<String, u64>,
    /// LV path to its backing PVs (the placement model; `lvcreate`
    /// places on the VG's first PV in report order). The `lvs`
    /// `devices` column derives from this — a `pvmove*` mirror
    /// segment reference while a move is active (the verified
    /// LVM 2.03.16 shape).
    pub lv_devices: BTreeMap<String, Vec<String>>,
    /// Active pvmove simulations, keyed by LV path.
    pub moves: BTreeMap<String, FakeMove>,
    /// Completed moves, kept for inspection and the
    /// deceptive-completion injection (the extents were observed
    /// relocated; whether they STAY relocated is what the completion
    /// verification checks).
    pub landed_moves: BTreeMap<String, FakeMove>,
    /// When > 0: once an LV that landed has been observed with its
    /// source freed this many times, the NEXT `lvs` query restores its
    /// placement to the source — the deterministic shape of "the world
    /// disagrees with itself between two consecutive observations"
    /// (an out-of-band reverse relocation racing the verification).
    /// The completion verification must refuse it and never free the
    /// source. 0 = never.
    pub restore_source_after_freed_sightings: u32,
    /// Per-LV counter of source-freed sightings (the knob's state).
    pub freed_sightings: BTreeMap<String, u32>,
    /// LVs whose placement restores to the source on the next `lvs`
    /// query (the scheduled half of the knob).
    pub pending_restore: Vec<String>,
    /// Sync percentage advanced per `lvs` query per active move (the
    /// deterministic clock; default 50 — two queries complete a move).
    pub move_advance_percent: u64,
    /// When true, active moves never advance (the supervision-window
    /// exhaustion shape).
    pub hold_moves: bool,
    /// When true, `pvmove` fails before starting anything (the
    /// start-failure crash window).
    pub fail_pvmove: bool,
    /// LV paths `lvremove` should report as missing (error injection).
    pub fail_lvremove_for: Vec<String>,
    /// When true, `blkdiscard` fails (error injection for ZeroDiscard).
    pub fail_blkdiscard: bool,
    /// When true, `lvcreate` claims success but never shows up in `lvs`
    /// (verification-failure injection).
    pub lvcreate_silent: bool,
    /// When true, `lvextend` claims success but never updates `lvs`
    /// (verification-failure injection).
    pub lvextend_silent: bool,
    /// Physical extent size `lvcreate`/`lvextend` round up to.
    pub extent_size: u64,
    /// When true, `vgcreate` fails (claim crash-window injection).
    pub fail_vgcreate: bool,
    /// When true, `pvremove` fails (release crash-window injection).
    pub fail_pvremove: bool,
    /// When true, `vgremove` fails even though the VG exists (release
    /// crash-window injection).
    pub fail_vgremove: bool,
    /// When true, `vgs` fails (honest-unknown injection for device-claim
    /// reconciliation).
    pub fail_vgs: bool,
    /// When true, `lvs` fails (honest-unknown injection for the move
    /// supervision: an unobservable world mid-move).
    pub fail_lvs: bool,
}

impl Default for FakeLvm {
    fn default() -> Self {
        Self {
            lvs: BTreeMap::new(),
            pvs: Vec::new(),
            vg_free: BTreeMap::new(),
            vg_size: BTreeMap::new(),
            pv_vg: BTreeMap::new(),
            pv_size: BTreeMap::new(),
            pv_free: BTreeMap::new(),
            lv_devices: BTreeMap::new(),
            moves: BTreeMap::new(),
            landed_moves: BTreeMap::new(),
            restore_source_after_freed_sightings: 0,
            freed_sightings: BTreeMap::new(),
            pending_restore: Vec::new(),
            move_advance_percent: 50,
            hold_moves: false,
            fail_pvmove: false,
            fail_lvremove_for: Vec::new(),
            fail_blkdiscard: false,
            lvcreate_silent: false,
            lvextend_silent: false,
            extent_size: EXTENT_BYTES,
            fail_vgcreate: false,
            fail_pvremove: false,
            fail_vgremove: false,
            fail_vgs: false,
            fail_lvs: false,
        }
    }
}

impl FakeLvm {
    /// A world with the conformance pool claimed and full of free space.
    #[must_use]
    pub fn with_claimed_pool() -> Self {
        let mut world = Self::default();
        world.vg_free.insert(CLAIMED_VG.to_owned(), POOL_BYTES);
        world.vg_size.insert(CLAIMED_VG.to_owned(), POOL_BYTES);
        world
    }

    /// Register a PV in `vg` with `size` bytes free (the move world's
    /// second PV, or a hand-seeded placement).
    pub fn add_pv_to_vg(&mut self, pv: &str, vg: &str, size: u64) {
        self.pvs.push(pv.to_owned());
        self.pv_vg.insert(pv.to_owned(), vg.to_owned());
        self.pv_size.insert(pv.to_owned(), size);
        self.pv_free.insert(pv.to_owned(), size);
    }

    /// Place an existing LV on `pv` and consume its extents from the
    /// PV's free space (hand-seeding a placement).
    pub fn place_lv_on(&mut self, path: &str, pv: &str) {
        let size = self.lvs.get(path).copied().unwrap_or_default();
        self.lv_devices.insert(path.to_owned(), vec![pv.to_owned()]);
        if let Some(free) = self.pv_free.get_mut(pv) {
            *free = (*free).saturating_sub(size);
        }
    }

    /// A scripted runner wired to this world (closure mode).
    #[must_use]
    pub fn runner(world: &Arc<Mutex<Self>>) -> Arc<FakeRunner> {
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
///
/// Also the deterministic move clock: every `lvs` query advances each
/// active move by [`FakeLvm::move_advance_percent`] (unless held), and
/// a move that reaches 100% lands — the LV's placement becomes the
/// target PV, the per-PV free accounts transfer, and the move ends.
/// While a move is active the row carries the **verified LVM 2.03.16
/// mid-move shape**: the `devices` column references the `pvmove0`
/// mirror segment and `copy_percent` is empty (the progress lives on
/// the hidden segment, not the LV row).
fn lvs_report(world: &mut FakeLvm) -> CommandOutput {
    advance_moves(world);
    // The deceptive-completion knob's scheduled half: placements that
    // restore on this query, before the rows are reported.
    let restoring: Vec<String> = std::mem::take(&mut world.pending_restore);
    for path in restoring {
        let Some(move_) = world.landed_moves.remove(&path) else {
            continue;
        };
        let size = world.lvs.get(&path).copied().unwrap_or_default();
        world
            .lv_devices
            .insert(path.clone(), vec![move_.source.clone()]);
        if let Some(free) = world.pv_free.get_mut(&move_.source) {
            *free = (*free).saturating_sub(size);
        }
        if let Some(free) = world.pv_free.get_mut(&move_.target) {
            *free = (*free).saturating_add(size);
        }
        world.freed_sightings.remove(&path);
    }
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for (path, size) in &world.lvs {
        let (vg, lv) = path.split_once('/').expect("path is vg/lv");
        let moving = world.moves.contains_key(path);
        let mut devices = if moving {
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
        // The knob's counting half: a landed LV observed with its
        // source freed counts toward the restore threshold.
        if let Some(move_) = world.landed_moves.get(path) {
            let freed = !devices.contains(&move_.source);
            if freed {
                let sightings = world.freed_sightings.entry(path.clone()).or_insert(0);
                *sightings += 1;
                if world.restore_source_after_freed_sightings > 0
                    && *sightings >= world.restore_source_after_freed_sightings
                {
                    world.pending_restore.push(path.clone());
                }
            }
        }
        if !moving {
            // Re-derive in case the row above was mutated by the
            // counting half (the placement is stable within a query).
            devices = world
                .lv_devices
                .get(path)
                .map(|pvs| {
                    pvs.iter()
                        .map(|pv| format!("{pv}(0)"))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
        }
        rows.push(serde_json::json!({
            "vg_name": vg,
            "lv_name": lv,
            "lv_size": size.to_string(),
            "lv_attr": if moving { "-wI-a-----" } else { "-wi-a-----" },
            "copy_percent": "",
            "devices": devices,
        }));
    }
    report("lv", &rows)
}

/// Advance the deterministic move clock (called once per `lvs`
/// query): every active move advances; a move that reaches 100%
/// completes — the LV's placement becomes the target, the free
/// accounts transfer source→target, the move record ends.
fn advance_moves(world: &mut FakeLvm) {
    if world.hold_moves {
        return;
    }
    let advance = world.move_advance_percent;
    for move_ in world.moves.values_mut() {
        move_.percent = move_.percent.saturating_add(advance);
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
        world.landed_moves.insert(path, move_);
    }
}

/// The `vgs` report rows for the simulated world.
///
/// Reported free space is derived: the VG's baseline free bytes minus the
/// sizes of all LVs currently allocated in it (saturating at zero). This
/// mirrors real LVM, where `lvcreate`/`lvextend` consume free space and
/// `lvremove` returns it, and prevents the simulated world from claiming
/// space that allocated LVs already occupy.
fn vgs_report(world: &FakeLvm) -> CommandOutput {
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

/// The `pvs` report rows for the simulated world: PV → VG membership
/// plus the per-PV size/free columns (the verified LVM 2.03.16
/// default column set). A PV registered without a VG reports an empty
/// `vg_name`, as real `pvs` does.
fn pvs_report(world: &FakeLvm) -> CommandOutput {
    let rows: Vec<serde_json::Value> = world
        .pvs
        .iter()
        .map(|path| {
            serde_json::json!({
                "pv_name": path,
                "vg_name": world.pv_vg.get(path).cloned().unwrap_or_default(),
                "pv_size": world
                    .pv_size
                    .get(path)
                    .copied()
                    .unwrap_or_default()
                    .to_string(),
                "pv_free": world
                    .pv_free
                    .get(path)
                    .copied()
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect();
    report("pv", &rows)
}

/// The scripted behavior for one command.
#[allow(clippy::too_many_lines)] // one arm per LVM verb — the kit's table
fn script(world: &mut FakeLvm, program: &str, args: &[&str]) -> Option<CommandOutput> {
    match program {
        "lvs" => {
            if world.fail_lvs {
                return Some(CommandOutput::failure("lvs: simulated failure"));
            }
            Some(lvs_report(world))
        }
        "vgs" => {
            if world.fail_vgs {
                return Some(CommandOutput::failure("vgs: simulated failure"));
            }
            Some(vgs_report(world))
        }
        "pvs" => Some(pvs_report(world)),
        "lsblk" => Some(CommandOutput::success(lsblk_for(world))),
        "lvcreate" => {
            // lvcreate --yes -L <size>B -n <lv> <vg>
            let size = arg_after(args, "-L")?;
            let size: u64 = size.trim_end_matches('B').parse().ok()?;
            let lv = arg_after(args, "-n")?;
            let vg = *args.last()?;
            let path = format!("{vg}/{lv}");
            // Real LVM refuses to create an LV whose name already exists.
            if world.lvs.contains_key(&path) {
                return Some(CommandOutput::failure(format!(
                    "lvcreate: {path} already exists"
                )));
            }
            if !world.lvcreate_silent {
                // Real LVM rounds the requested size up to whole extents.
                let effective = round_up_to_extent(size, world.extent_size);
                world.lvs.insert(path.clone(), effective);
                // Placement: the VG's first PV in report order (the
                // single-PV worlds unchanged; the move world's source).
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
            }
            Some(CommandOutput::success(String::new()))
        }
        "lvextend" => {
            // lvextend --yes -L <size>B <vg>/<lv>
            let size = arg_after(args, "-L")?;
            let size: u64 = size.trim_end_matches('B').parse().ok()?;
            let path = *args.last()?;
            if !world.lvextend_silent {
                let effective = round_up_to_extent(size, world.extent_size);
                let previous = world.lvs.get(path).copied().unwrap_or_default();
                world.lvs.insert(path.to_owned(), effective);
                // Consume the growth from the LV's placement PV.
                let delta = effective.saturating_sub(previous);
                let placement = world
                    .lv_devices
                    .get(path)
                    .and_then(|pvs| pvs.first().cloned());
                if let Some(pv) = placement {
                    if let Some(free) = world.pv_free.get_mut(&pv) {
                        *free = (*free).saturating_sub(delta);
                    }
                }
            }
            Some(CommandOutput::success(String::new()))
        }
        "lvremove" => {
            // lvremove --yes <vg>/<lv>
            let path = *args.last()?;
            if world.fail_lvremove_for.iter().any(|p| p == path) {
                return Some(CommandOutput::failure("lvremove: device is busy"));
            }
            // Real lvremove of a missing LV fails loudly.
            if let Some(size) = world.lvs.remove(path) {
                // Return the extents to the placement PVs (the
                // per-PV capacity model stays honest).
                let placement = world.lv_devices.remove(path).unwrap_or_default();
                for pv in placement {
                    if let Some(free) = world.pv_free.get_mut(&pv) {
                        *free = (*free).saturating_add(size);
                    }
                }
            } else {
                return Some(CommandOutput::failure(format!(
                    "lvremove: {path} not found"
                )));
            }
            Some(CommandOutput::success(String::new()))
        }
        "pvmove" => {
            // pvmove [--abort] | --background --noudevsync -n <vg>/<lv> <source> <target>
            if args.contains(&"--abort") {
                // Verified LVM 2.03.16 behavior: the abort abandons
                // every active move back to its source (the LV's
                // placement never changed mid-move) and exits 0,
                // including when nothing is moving.
                world.moves.clear();
                return Some(CommandOutput::success(String::new()));
            }
            if world.fail_pvmove {
                return Some(CommandOutput::failure("pvmove: simulated failure"));
            }
            let path = arg_after(args, "-n")?.to_owned();
            let source = (*args.get(args.len().saturating_sub(2))?).to_owned();
            let target = (*args.last()?).to_owned();
            // Verified LVM 2.03.16 behavior: a re-run while the source
            // PV carries an active move attaches to it, IGNORES the
            // remaining arguments, and exits 0 — the silent no-op the
            // provider's one-move-per-source-PV refusal exists for.
            if world.moves.values().any(|move_| move_.source == source) {
                return Some(CommandOutput::success(format!(
                    "  Detected pvmove in progress for {source}.\n  WARNING: Ignoring \
                     remaining command line arguments.\n"
                )));
            }
            // Verified: a scoped pvmove whose LV holds no extents on
            // the source fails ("No data to move", exit 5).
            let on_source = world
                .lv_devices
                .get(&path)
                .is_some_and(|pvs| pvs.iter().any(|pv| pv == &source));
            if !on_source {
                return Some(CommandOutput::failure(format!(
                    "  No data to move for {}.\n",
                    path.split_once('/').map_or(path.as_str(), |(vg, _)| vg)
                )));
            }
            // Real LVM refuses a move whose extents do not fit in the
            // target's free space (insufficient free extents).
            let size = world.lvs.get(&path).copied().unwrap_or_default();
            let target_free = world.pv_free.get(&target).copied().unwrap_or_default();
            if target_free < size {
                return Some(CommandOutput::failure(format!(
                    "  Insufficient free space: {size} bytes needed, {target_free} \
                     available on {target}\n"
                )));
            }
            world.moves.insert(
                path,
                FakeMove {
                    source,
                    target,
                    percent: 0,
                },
            );
            Some(CommandOutput::success(String::new()))
        }
        "blkdiscard" => {
            if world.fail_blkdiscard {
                Some(CommandOutput::failure(
                    "blkdiscard: operation not permitted",
                ))
            } else {
                Some(CommandOutput::success(String::new()))
            }
        }
        "pvcreate" => {
            world.pvs.push((*args.last()?).to_owned());
            Some(CommandOutput::success(String::new()))
        }
        "vgcreate" => {
            if world.fail_vgcreate {
                return Some(CommandOutput::failure("vgcreate: simulated failure"));
            }
            // vgcreate --yes <vg> <path...>
            let vg = (*args.get(1)?).to_owned();
            world.vg_free.insert(vg.clone(), POOL_BYTES);
            world.vg_size.insert(vg.clone(), POOL_BYTES);
            // Every PV handed to vgcreate joins it with the pool's
            // capacity free (the per-PV model mirrors the VG model).
            for pv in args.iter().skip(2) {
                world.add_pv_to_vg(pv, &vg, POOL_BYTES);
            }
            Some(CommandOutput::success(String::new()))
        }
        "vgremove" => {
            // vgremove --yes <vg>
            let vg = *args.last()?;
            if world.fail_vgremove {
                return Some(CommandOutput::failure("vgremove: simulated failure"));
            }
            // Real vgremove of a missing VG fails loudly ("Volume group
            // ... not found"); the unconditional success here hid the
            // release stuck-state crash window.
            if world.vg_free.remove(vg).is_none() && world.vg_size.remove(vg).is_none() {
                return Some(CommandOutput::failure(format!(
                    "vgremove: volume group {vg} not found"
                )));
            }
            Some(CommandOutput::success(String::new()))
        }
        "pvremove" => {
            if world.fail_pvremove {
                return Some(CommandOutput::failure("pvremove: simulated failure"));
            }
            // pvremove --yes <path>
            let path = *args.last()?;
            world.pvs.retain(|pv| pv != path);
            world.pv_vg.remove(path);
            world.pv_size.remove(path);
            world.pv_free.remove(path);
            Some(CommandOutput::success(String::new()))
        }
        _ => None,
    }
}

/// Round `size` up to a whole multiple of `extent` (real LVM behavior).
fn round_up_to_extent(size: u64, extent: u64) -> u64 {
    if extent == 0 {
        return size;
    }
    size.div_ceil(extent) * extent
}

/// The `lsblk` JSON describing the fake host's disks.
fn lsblk_for(world: &FakeLvm) -> String {
    // A single whole disk with a WWN; its size reflects simulated usage so
    // capacity-driven tests have something realistic to observe.
    let _ = world;
    serde_json::json!({
        "blockdevices": [
            {"name": "loop0", "type": "loop", "size": 104_857_600,
             "serial": null, "wwn": null, "model": null},
            {"name": "sda", "type": "disk", "size": POOL_BYTES,
             "serial": "FIXTURE-SERIAL-1", "wwn": "0x5000c500fixt0001",
             "model": "Fixture Disk"},
        ]
    })
    .to_string()
}

/// The value following `flag` in `args`.
fn arg_after<'a>(args: &[&'a str], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| *arg == flag)
        .and_then(|index| args.get(index + 1))
        .copied()
}

/// A tempdir that outlives the provider it backs (leaked for the test
/// process lifetime; test processes are short-lived).
pub fn leak_tempdir() -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    path
}

/// Seed a state file with one claimed native-pool device.
pub fn seed_claimed_state(state_path: &std::path::Path) {
    let mut state = LvmState::default();
    state.insert_device(
        DeviceId::new(CLAIMED_DEVICE).expect("valid device id"),
        DeviceEntry {
            stable_identity: "0x5000c500fixt0001".to_owned(),
            path: "/dev/disk/by-id/wwn-0x5000c500fixt0001".to_owned(),
            vg_name: CLAIMED_VG.to_owned(),
            role: DeviceRole::NativePool,
            owner_generation: 1,
        },
    );
    state.save(state_path).expect("seed state");
}

/// A claim request carrying `token` (the destructive-authorization
/// credential under test).
pub fn claim_request(token: &str) -> volvisor_types::ClaimDeviceRequest {
    volvisor_types::ClaimDeviceRequest {
        api_version: volvisor_types::API_VERSION.to_owned(),
        operation_id: volvisor_types::OperationId::new("op-claim-fixture")
            .expect("valid fixture operation id"),
        authorization_token: token.to_owned(),
    }
}

/// A release request carrying `token`.
pub fn release_request(token: &str) -> volvisor_types::ReleaseDeviceRequest {
    volvisor_types::ReleaseDeviceRequest {
        api_version: volvisor_types::API_VERSION.to_owned(),
        operation_id: volvisor_types::OperationId::new("op-release-fixture")
            .expect("valid fixture operation id"),
        authorization_token: token.to_owned(),
    }
}

/// A provider over the simulated LVM with a pre-claimed pool.
///
/// Each call yields an isolated provider (own state file, own simulated
/// world), suitable for the conformance kit's per-test construction.
pub fn conformance_provider() -> Arc<LvmProvider> {
    let state_path = leak_tempdir().join("state.json");
    seed_claimed_state(&state_path);
    let world = Arc::new(Mutex::new(FakeLvm::with_claimed_pool()));
    let runner = FakeLvm::runner(&world);
    let sysfs_root = leak_tempdir();
    LvmProvider::new(
        runner,
        state_path,
        sysfs_root,
        VG_PREFIX.to_owned(),
        AUTH_TOKEN.to_owned(),
    )
    .map(Arc::new)
    .expect("provider construction")
}

/// A provider construction kit returning the world handle too, for tests
/// that need to inject faults or inspect the simulation.
pub struct Fixture {
    /// The provider under test.
    pub provider: Arc<LvmProvider>,
    /// The simulated LVM world.
    pub world: Arc<Mutex<FakeLvm>>,
    /// The scripted runner (for invocation assertions).
    pub runner: Arc<FakeRunner>,
    /// The state file path.
    pub state_path: PathBuf,
}

/// A fresh provider over an existing state file, sharing the simulated
/// LVM world (restarts / crash-replay tests).
pub fn provider_from(
    state_path: &std::path::Path,
    world: &Arc<Mutex<FakeLvm>>,
) -> Arc<LvmProvider> {
    provider_from_with_timing(state_path, world, default_move_timing())
}

/// [`provider_from`] with explicit move timing (the fault rows' restarts
/// keep the fast knobs of the incarnation they replace).
pub fn provider_from_with_timing(
    state_path: &std::path::Path,
    world: &Arc<Mutex<FakeLvm>>,
    timing: volvisor_lvm::provider::MoveTiming,
) -> Arc<LvmProvider> {
    let runner = FakeLvm::runner(world);
    LvmProvider::new(
        runner,
        state_path.to_path_buf(),
        leak_tempdir(),
        VG_PREFIX.to_owned(),
        AUTH_TOKEN.to_owned(),
    )
    .map(|provider| Arc::new(provider.with_move_timing(timing)))
    .expect("provider construction")
}

/// Seed a volume entry directly into a state file (reconciliation tests).
pub fn seed_volume(state_path: &std::path::Path, volume_id: &str, vg_name: &str, size_bytes: u64) {
    let volume_id = VolumeId::new(volume_id).expect("valid volume id");
    let mut state = LvmState::load(state_path).expect("load state");
    state.insert_volume(
        volume_id.clone(),
        volvisor_lvm::state::StoredVolume {
            entry: volvisor_lvm::state::VolumeEntry {
                vg_name: vg_name.to_owned(),
                lv_name: lv_name_for(&volume_id),
                size_bytes,
                requested_size_bytes: size_bytes,
                generation: 1,
                data_epoch: 0,
                project_id: volvisor_types::ProjectId::new("seed-project")
                    .expect("valid project id"),
                block_size: 4096,
                creation_payload: "seed".to_owned(),
            },
            runtime: volvisor_lvm::state::VolumeRuntime {
                state: VolumeLifecycle::Ready,
                attachment: None,
            },
        },
    );
    state.save(state_path).expect("seed volume state");
}

/// Seed a move record directly into a state file (crash-model tests:
/// the durable record a dead incarnation left behind).
pub fn seed_move(
    state_path: &std::path::Path,
    volume_id: &str,
    source_pv: &str,
    target_pv: &str,
    move_state: volvisor_types::MoveVolumeBackingState,
) {
    let volume_id = VolumeId::new(volume_id).expect("valid volume id");
    let mut state = LvmState::load(state_path).expect("load state");
    state.insert_move(
        volume_id,
        volvisor_lvm::state::MoveRecord {
            operation_id: volvisor_types::OperationId::new("op-seeded-move")
                .expect("valid fixture operation id"),
            source_pv: source_pv.to_owned(),
            target_pv: target_pv.to_owned(),
            state: move_state,
            detail: None,
        },
    );
    state.save(state_path).expect("seed move state");
}

/// Build a fixture over the simulated LVM with a pre-claimed pool.
pub fn fixture() -> Fixture {
    let state_path = leak_tempdir().join("state.json");
    seed_claimed_state(&state_path);
    fixture_at(state_path, true)
}

/// Build a fixture over an empty (unclaimed) provider state.
pub fn unclaimed_fixture() -> Fixture {
    let state_path = leak_tempdir().join("state.json");
    fixture_at(state_path, false)
}

fn fixture_at(state_path: PathBuf, claimed: bool) -> Fixture {
    if claimed {
        seed_claimed_state(&state_path);
    }
    let world = Arc::new(Mutex::new(FakeLvm::with_claimed_pool()));
    let runner = FakeLvm::runner(&world);
    let sysfs_root = leak_tempdir();
    let provider = LvmProvider::new(
        Arc::clone(&runner) as Arc<dyn CommandRunner>,
        state_path.clone(),
        sysfs_root,
        VG_PREFIX.to_owned(),
        AUTH_TOKEN.to_owned(),
    )
    .map(Arc::new)
    .expect("provider construction");
    Fixture {
        provider,
        world,
        runner,
        state_path,
    }
}

/// A real-command provider over an empty state (integration tests).
pub fn real_provider(state_path: PathBuf) -> Result<Arc<LvmProvider>, ApiError> {
    let runner: Arc<dyn CommandRunner> = Arc::new(RealRunner::default());
    LvmProvider::new(
        runner,
        state_path,
        PathBuf::from("/"),
        "volvisorit".to_owned(),
        "integration-token".to_owned(),
    )
    .map(Arc::new)
}

// ---------------------------------------------------------------------------
// The same-VG move fixtures (P6-C)
// ---------------------------------------------------------------------------

/// The move world's source PV (the claimed pool's first PV in report
/// order — `lvcreate` places there).
pub const MOVE_SOURCE_PV: &str = "/dev/pv-a";
/// The move world's evacuation target PV.
pub const MOVE_TARGET_PV: &str = "/dev/pv-b";

/// The move tests' default fast timing: a 10 ms poll and a 250 ms
/// supervision window (the deterministic world completes a move in two
/// `lvs` queries; a held move exhausts the window in ~25 polls).
pub fn default_move_timing() -> volvisor_lvm::provider::MoveTiming {
    volvisor_lvm::provider::MoveTiming {
        poll_interval: std::time::Duration::from_millis(10),
        supervision_window: std::time::Duration::from_millis(250),
    }
}

/// A fixture for the same-VG move tests: the claimed pool's VG spread
/// over two PVs — `lvcreate` places volumes on
/// [`MOVE_SOURCE_PV`], and [`MOVE_TARGET_PV`] is the evacuation
/// target — with the fast move timing.
pub fn move_fixture() -> Fixture {
    move_fixture_with_timing(default_move_timing())
}

/// [`move_fixture`] with explicit move timing.
pub fn move_fixture_with_timing(timing: volvisor_lvm::provider::MoveTiming) -> Fixture {
    let state_path = leak_tempdir().join("state.json");
    seed_claimed_state(&state_path);
    let world = Arc::new(Mutex::new(FakeLvm::with_claimed_pool()));
    {
        let mut world = world.lock().expect("world lock");
        world.add_pv_to_vg(MOVE_SOURCE_PV, CLAIMED_VG, POOL_BYTES);
        world.add_pv_to_vg(MOVE_TARGET_PV, CLAIMED_VG, POOL_BYTES);
        // Two PVs of `POOL_BYTES` each: the VG's own baseline follows.
        world.vg_free.insert(CLAIMED_VG.to_owned(), 2 * POOL_BYTES);
        world.vg_size.insert(CLAIMED_VG.to_owned(), 2 * POOL_BYTES);
    }
    let runner = FakeLvm::runner(&world);
    let provider = LvmProvider::new(
        Arc::clone(&runner) as Arc<dyn CommandRunner>,
        state_path.clone(),
        leak_tempdir(),
        VG_PREFIX.to_owned(),
        AUTH_TOKEN.to_owned(),
    )
    .map(|provider| Arc::new(provider.with_move_timing(timing)))
    .expect("provider construction");
    Fixture {
        provider,
        world,
        runner,
        state_path,
    }
}
