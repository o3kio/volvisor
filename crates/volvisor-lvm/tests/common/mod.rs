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
use volvisor_lvm::runner::{CommandOutput, CommandRunner, FakeRunner, RealRunner};
use volvisor_lvm::state::{DeviceEntry, LvmState};
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
}

impl Default for FakeLvm {
    fn default() -> Self {
        Self {
            lvs: BTreeMap::new(),
            pvs: Vec::new(),
            vg_free: BTreeMap::new(),
            vg_size: BTreeMap::new(),
            fail_lvremove_for: Vec::new(),
            fail_blkdiscard: false,
            lvcreate_silent: false,
            lvextend_silent: false,
            extent_size: EXTENT_BYTES,
            fail_vgcreate: false,
            fail_pvremove: false,
            fail_vgremove: false,
            fail_vgs: false,
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
fn lvs_report(world: &FakeLvm) -> CommandOutput {
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

/// The `pvs` report rows for the simulated world.
fn pvs_report(world: &FakeLvm) -> CommandOutput {
    let rows: Vec<serde_json::Value> = world
        .pvs
        .iter()
        .map(|path| serde_json::json!({ "pv_name": path }))
        .collect();
    report("pv", &rows)
}

/// The scripted behavior for one command.
fn script(world: &mut FakeLvm, program: &str, args: &[&str]) -> Option<CommandOutput> {
    match program {
        "lvs" => Some(lvs_report(world)),
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
                world.lvs.insert(path, effective);
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
                world.lvs.insert(path.to_owned(), effective);
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
            if world.lvs.remove(path).is_none() {
                return Some(CommandOutput::failure(format!(
                    "lvremove: {path} not found"
                )));
            }
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
            // vgcreate --yes <vg> <path>
            let vg = (*args.get(1)?).to_owned();
            world.vg_free.insert(vg.clone(), POOL_BYTES);
            world.vg_size.insert(vg, POOL_BYTES);
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
    let runner = FakeLvm::runner(world);
    LvmProvider::new(
        runner,
        state_path.to_path_buf(),
        leak_tempdir(),
        VG_PREFIX.to_owned(),
        AUTH_TOKEN.to_owned(),
    )
    .map(Arc::new)
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
