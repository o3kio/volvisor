//! Shared fixtures for the `volvisor-drbd` integration tests.
//!
//! [`FakeDrbd`] is a minimal stateful DRBD+LVM simulation driven through
//! the closure mode of [`FakeRunner`]: logical volumes (with sizes and
//! `volvisor.*` tags, extent-rounded like real thick LVM), on-LV DRBD
//! metadata, and running resources (role, local/peer disk states,
//! device size) live in shared maps; `lvcreate`/`lvextend`/`lvremove`
//! and the scoped `drbdadm` lifecycle verbs (`create-md`, `up`, `down`,
//! `primary`, `secondary`, `resize`) mutate them, and `lvs`/`vgs`/
//! `drbdsetup status`/`blockdev --getsize64` report them — so the
//! provider's verification steps (roles re-read from status, LV
//! geometry from `lvs`, device size from `blockdev`) exercise real
//! round-trips instead of echoes.
//!
//! The simulation mirrors real CLI semantics that matter to the
//! provider:
//!
//! - every `drbdadm` action **requires** the `-c <file>` scoping and
//!   parses the REAL resource file at that path (the fake refuses to
//!   act on a resource the file does not define), so the rule-7
//!   guarantee is exercised end to end;
//! - `lvcreate` on an existing name fails "already exists", and sizes
//!   round UP to whole extents (thick LVM);
//! - `drbdadm create-md` refuses non-interactively when metadata
//!   already exists on the backing LV (the crashed-predecessor
//!   signature), while `up` adopts it;
//! - `drbdadm primary` without `--force` refuses over a non-`UpToDate`
//!   local disk ("Refusing to be Primary..."), and `secondary` refuses
//!   while the device is open (EBUSY-class stderr);
//! - `drbdsetup status` answers in the verified text grammar and fails
//!   "No such resource" for unknown/down resources;
//! - `blockdev --getsize64` reports the STORED device size (set at `up`
//!   and `resize` to the minimum of the local and peer backing sizes),
//!   never a live computation.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention (see
//! the crate-level `cfg_attr(test)` in the library).

#![allow(dead_code)]
#![allow(clippy::expect_used, clippy::unwrap_used)]
// The simulated world is a fault-injection matrix; one bool per scripted
// failure is the clearest shape for test code.
#![allow(clippy::struct_excessive_bools)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use volvisor_drbd::provider::{DrbdProvider, DrbdProviderConfig, resource_name_for};
use volvisor_drbd::report::{DiskState, Role};
use volvisor_drbd::resgen::{ResourceDefinition, parse_resource_file};
use volvisor_drbd::state::{DrbdState, ReplicationMode, StoredVolume, VolumeEntry, VolumeRuntime};
use volvisor_drbd::{CommandOutput, CommandRunner, FakeRunner};
use volvisor_provider::VolumeProvider;
use volvisor_types::domain::{Health, VolumeClass};
use volvisor_types::request::{
    AttachVolumeRequest, AttachVolumeResponse, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, GrowVolumeRequest, GrowVolumeResponse, InspectVolumeResponse,
    ReplicationModeRequest, ReplicationPolicyRequest,
};
use volvisor_types::{ApiError, AttachmentId, CapabilitySet, ProjectId, VolumeId, VolumeLifecycle};

/// The nearline VG every fixture configures.
pub const VG: &str = "vgdrbd";
/// The local `uname -n` the fixture world answers with.
pub const NODE: &str = "node-a";
/// The operator-provisioned peer's node name.
pub const PEER_NODE: &str = "node-b";
/// The local replication address.
pub const LOCAL_ADDR: &str = "10.0.0.1";
/// The peer's fixed address and listening port.
pub const PEER_ADDR: &str = "10.0.0.2:7800";
/// The shared secret written into the fixture secret file.
pub const SECRET: &str = "fixture-peer-secret";
/// Free space of the simulated VG before any allocation (100 GiB).
pub const VG_FREE: u64 = 100 << 30;
/// Extent size of the simulated VG (LVM's default 4 MiB).
pub const EXTENT: u64 = 4 << 20;
/// The operator-declared DRBD minor range.
pub const MINOR_MIN: u32 = 10;
/// See [`MINOR_MIN`].
pub const MINOR_MAX: u32 = 20;
/// The operator-declared local replication port range.
pub const PORT_MIN: u16 = 7900;
/// See [`PORT_MIN`].
pub const PORT_MAX: u16 = 7910;
/// Minor/port used by [`seed_volume`] fixtures.
pub const SEED_MINOR: u32 = 11;
/// See [`SEED_MINOR`].
pub const SEED_PORT: u16 = 7900;

/// One simulated logical volume (keyed `vg/lv`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeLv {
    /// Size in bytes (extent-rounded at creation/extension, like real
    /// thick LVM).
    pub size: u64,
    /// LVM tags (`volvisor.owner=<id>`, `volvisor.generation=1`, ...).
    pub tags: Vec<String>,
}

/// One simulated running DRBD resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeResource {
    /// The minor (from the resource file; `/dev/drbd<minor>`).
    pub minor: u32,
    /// The local role.
    pub role: Role,
    /// The local disk state.
    pub local_disk: DiskState,
    /// The peer's disk state for THIS resource (per-resource: seeding
    /// one volume must not make the peer look data-holding for
    /// another).
    pub peer_disk: DiskState,
    /// The peer's role for this resource.
    pub peer_role: Role,
    /// Whether a resync to the peer is in progress (emits
    /// `replication:`/`done:` on the peer-disk line).
    pub resyncing: bool,
    /// The STORED device size (`blockdev --getsize64` answer), set at
    /// `up` and `resize` — never computed live.
    pub device_size: u64,
    /// The peer node name parsed from the resource file at `up` (the
    /// status connection line is named by it).
    pub peer_node: String,
}

/// The simulated DRBD + LVM world shared between the provider and
/// assertions.
pub struct FakeDrbd {
    /// The `uname -n` answer.
    pub node_name: String,
    /// The nearline volume group name.
    pub vg_name: String,
    /// Free space of the VG BEFORE any allocation (reported `vg_free`
    /// is derived: this baseline minus current LV sizes).
    pub vg_free: u64,
    /// Physical extent size of the VG.
    pub vg_extent_size: u64,
    /// `vg/lv` → simulated LV.
    pub lvs: BTreeMap<String, FakeLv>,
    /// Resources whose backing LV carries DRBD metadata (`create-md`
    /// ran; survives `down`, dies with `lvremove` — internal metadata
    /// lives on the LV).
    pub metadata: BTreeSet<String>,
    /// Resource name → running resource.
    pub resources: BTreeMap<String, FakeResource>,
    /// Whether the replication link to the peer is up (affects every
    /// resource's status: connected vs `WFConnection`).
    pub peer_online: bool,
    /// The disk state a peer reports for a resource at its `up` moment
    /// (default `Inconsistent` = provably fresh; pin `UpToDate` for
    /// foreign-peer-data tests). Per-resource once established.
    pub new_peer_disk: DiskState,
    /// The role a peer reports for a resource at its `up` moment
    /// (default Secondary; pin Primary for foreign-peer tests).
    pub new_peer_role: Role,
    /// The peer's backing LV size override: `None` (default) follows
    /// the local LV (the P3 operator model grows both ends), `Some(n)`
    /// pins a smaller/larger peer backing (honest-boundary tests).
    /// Applies to every resource — tests using it hold isolated
    /// fixtures.
    pub peer_lv_size: Option<u64>,
    /// Set when a `primary --force` ran while the peer was connected
    /// (the seeding overwrite the foreign-data tests pin).
    pub peer_overwritten: bool,
    /// Whether a post-seed resync completes (default true); `false`
    /// leaves the peer `Inconsistent` with a `replication:` line.
    pub resync_completes: bool,
    /// Minors whose device is held open (demotion and `down` refuse
    /// with EBUSY-class stderr).
    pub open_devices: BTreeSet<u32>,
    // -- Fault-injection matrix (one bool per scripted failure) --
    /// `lvcreate` fails.
    pub fail_lvcreate: bool,
    /// `lvextend` fails.
    pub fail_lvextend: bool,
    /// `drbdadm primary` (and `primary --force`) fails.
    pub fail_primary: bool,
    /// `drbdadm secondary` fails (with [`Self::fail_secondary_message`]
    /// when set, else a generic failure).
    pub fail_secondary: bool,
    /// Overrides the `drbdadm secondary` failure stderr.
    pub fail_secondary_message: Option<String>,
    /// `drbdadm down` fails.
    pub fail_down: bool,
    /// `drbdadm create-md` fails.
    pub fail_create_md: bool,
    /// `drbdadm up` fails.
    pub fail_up: bool,
    /// `drbdadm resize` fails.
    pub fail_resize: bool,
    /// `drbdsetup status` fails with a NON-"No such resource" stderr (a
    /// transient query failure that must surface as INTERNAL).
    pub fail_status: bool,
    /// `blockdev --getsize64` fails.
    pub fail_blockdev: bool,
    /// `vgs` fails.
    pub fail_vgs: bool,
    /// `lvs` fails.
    pub fail_lvs: bool,
    /// `uname -n` fails.
    pub fail_uname: bool,
    /// `drbdadm --version` fails.
    pub fail_drbdadm_version: bool,
}

impl Default for FakeDrbd {
    fn default() -> Self {
        Self {
            node_name: NODE.to_owned(),
            vg_name: VG.to_owned(),
            vg_free: VG_FREE,
            vg_extent_size: EXTENT,
            lvs: BTreeMap::new(),
            metadata: BTreeSet::new(),
            resources: BTreeMap::new(),
            peer_online: true,
            new_peer_disk: DiskState::Inconsistent,
            new_peer_role: Role::Secondary,
            peer_lv_size: None,
            peer_overwritten: false,
            resync_completes: true,
            open_devices: BTreeSet::new(),
            fail_lvcreate: false,
            fail_lvextend: false,
            fail_primary: false,
            fail_secondary: false,
            fail_secondary_message: None,
            fail_down: false,
            fail_create_md: false,
            fail_up: false,
            fail_resize: false,
            fail_status: false,
            fail_blockdev: false,
            fail_vgs: false,
            fail_lvs: false,
            fail_uname: false,
            fail_drbdadm_version: false,
        }
    }
}

impl FakeDrbd {
    /// A scripted runner wired to this world (closure mode).
    #[must_use]
    pub fn runner(world: &Arc<Mutex<Self>>) -> Arc<FakeRunner> {
        let world = Arc::clone(world);
        Arc::new(FakeRunner::with_closure(move |program, args| {
            let mut world = world.lock().ok()?;
            script(&mut world, program, args)
        }))
    }

    /// The effective peer backing size for a resource whose local LV is
    /// `local_size` (the follow-local default or the pinned override).
    #[must_use]
    fn peer_backing(&self, local_size: u64) -> u64 {
        self.peer_lv_size.unwrap_or(local_size)
    }

    /// The derived free space of the VG (baseline minus allocations).
    #[must_use]
    fn free_bytes(&self) -> u64 {
        let allocated: u64 = self.lvs.values().map(|lv| lv.size).sum();
        self.vg_free.saturating_sub(allocated)
    }
}

/// Round `size` up to whole `extent`s (thick LVM allocation).
fn extent_round_up(size: u64, extent: u64) -> u64 {
    if extent == 0 {
        return size;
    }
    size.div_ceil(extent) * extent
}

/// The `disk:`/`peer-disk:` spelling of a disk state.
fn disk_str(disk: &DiskState) -> String {
    match disk {
        DiskState::UpToDate => "UpToDate".to_owned(),
        DiskState::Inconsistent => "Inconsistent".to_owned(),
        DiskState::Outdated => "Outdated".to_owned(),
        DiskState::Consistent => "Consistent".to_owned(),
        DiskState::Failed => "Failed".to_owned(),
        DiskState::Diskless => "Diskless".to_owned(),
        DiskState::DUnknown => "DUnknown".to_owned(),
        DiskState::Other(other) => other.clone(),
    }
}

/// The `role:` spelling of a role.
fn role_str(role: Role) -> &'static str {
    match role {
        Role::Primary => "Primary",
        Role::Secondary => "Secondary",
    }
}

/// The value following `flag` in `args`.
fn arg_after<'a>(args: &[&'a str], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| *arg == flag)
        .and_then(|index| args.get(index + 1))
        .copied()
}

/// The `drbdsetup status` text for one running resource — the verified
/// grammar: resource line (indent 0, `role:`), `disk:` line (indent 2),
/// a peer-node-named connection line (indent 2), `peer-disk:` line
/// (indent 4, optionally with `replication:`/`done:`), trailing blank
/// line.
fn status_text(world: &FakeDrbd, name: &str, resource: &FakeResource) -> String {
    if world.peer_online {
        let mut peer_disk_line = format!("    peer-disk:{}", disk_str(&resource.peer_disk));
        if resource.resyncing {
            peer_disk_line.push_str(" replication:SyncTarget done:37.50%");
        }
        format!(
            "{name} role:{role}\n  disk:{disk}\n  {peer} role:{peer_role}\n{peer_disk_line}\n\n",
            role = role_str(resource.role),
            disk = disk_str(&resource.local_disk),
            peer = resource.peer_node,
            peer_role = role_str(resource.peer_role),
        )
    } else {
        format!(
            "{name} role:{role}\n  disk:{disk}\n  {peer} connection:WFConnection\n\n",
            role = role_str(resource.role),
            disk = disk_str(&resource.local_disk),
            peer = resource.peer_node,
        )
    }
}

/// The scripted behavior for one command.
fn script(world: &mut FakeDrbd, program: &str, args: &[&str]) -> Option<CommandOutput> {
    match program {
        "drbdadm" => script_drbdadm(world, args),
        "drbdsetup" => script_drbdsetup(world, args),
        "blockdev" => script_blockdev(world, args),
        "lvs" => Some(script_lvs(world)),
        "vgs" => Some(script_vgs(world)),
        "lvcreate" => script_lvcreate(world, args),
        "lvextend" => script_lvextend(world, args),
        "lvremove" => script_lvremove(world, args),
        "uname" => Some(script_uname(world)),
        _ => None,
    }
}

/// `drbdadm --version` or the scoped `drbdadm -c <file> <verbs>
/// [options] <resource>`.
///
/// The argv contract is pinned first: every action invocation must be
/// scoped through `-c <file>` and the file (parsed with the REAL
/// parser) must define the named resource, so a regression to the
/// global `/etc/drbd.conf` or a foreign resource name cannot pass
/// silently.
fn script_drbdadm(world: &mut FakeDrbd, args: &[&str]) -> Option<CommandOutput> {
    if args == ["--version"] {
        if world.fail_drbdadm_version {
            return Some(CommandOutput::failure(
                "drbdadm: error while loading shared libraries",
            ));
        }
        // The real `drbdadm --version` output shape (version tags).
        return Some(CommandOutput::success(
            "DRBDADM_BUILDTAG=fixture\nDRBDADM_VERSION=9.29.0\nDRBDADM_API_VERSION=2\n\
             DRBD_KERNEL_VERSION_CODE=0x090f00\nDRBDADM_VERSION_CODE=0x090d00\n",
        ));
    }
    if args.first().copied() != Some("-c") {
        // Not scripted: the closure answers None and the provider sees
        // INTERNAL "no scripted output", failing the test loudly.
        return None;
    }
    let path = Path::new(args.get(1)?);
    let resource = args.last().copied()?;
    let verbs = &args[2..args.len().saturating_sub(1)];
    // Real drbdadm fails when the config file cannot be opened.
    let Ok(content) = std::fs::read_to_string(path) else {
        return Some(CommandOutput::failure(format!(
            "drbdadm: Cannot open config file '{}': No such file or directory",
            path.display()
        )));
    };
    let Ok(parsed) = parse_resource_file(&content) else {
        return Some(CommandOutput::failure(format!(
            "drbdadm: parse error in config file '{}'",
            path.display()
        )));
    };
    if parsed.name != resource {
        return Some(CommandOutput::failure(format!(
            "{resource}: no resource defined in '{}'",
            path.display()
        )));
    }
    match verbs.first().copied() {
        Some("create-md") => {
            if world.fail_create_md {
                return Some(CommandOutput::failure(
                    "drbdadm create-md: simulated failure",
                ));
            }
            // Real create-md refuses non-interactively when metadata
            // already exists (it would ask for confirmation).
            if world.metadata.contains(resource) {
                return Some(CommandOutput::failure(format!(
                    "drbdadm create-md {resource}: metadata already exists on the backing \
                     device; confirmation requires a terminal, aborting"
                )));
            }
            world.metadata.insert(resource.to_owned());
            Some(CommandOutput::success(format!(
                "initial metadata created for {resource}\n"
            )))
        }
        Some("up") => script_drbdadm_up(world, &parsed, resource),
        Some("down") => {
            if world.fail_down {
                return Some(CommandOutput::failure("drbdadm down: simulated failure"));
            }
            if let Some(resource_state) = world.resources.get(resource) {
                if world.open_devices.contains(&resource_state.minor) {
                    return Some(CommandOutput::failure(format!(
                        "drbdadm down {resource}: State change failed: (-16) Device or resource \
                         busy: /dev/drbd{} is open by another process",
                        resource_state.minor
                    )));
                }
            }
            // Internal metadata on the LV survives down.
            world.resources.remove(resource);
            Some(CommandOutput::success(String::new()))
        }
        Some("primary") => script_drbdadm_primary(world, resource, verbs.contains(&"--force")),
        Some("secondary") => script_drbdadm_secondary(world, resource),
        Some("resize") => {
            if world.fail_resize {
                return Some(CommandOutput::failure("drbdadm resize: simulated failure"));
            }
            // The device follows the smaller of the two backings.
            let disk = parsed.disks.first().map(String::as_str)?;
            let lv_key = disk.strip_prefix("/dev/")?;
            let local = world.lvs.get(lv_key).map(|lv| lv.size)?;
            let peer = world.peer_backing(local);
            let resource_state = world.resources.get_mut(resource)?;
            resource_state.device_size = local.min(peer);
            Some(CommandOutput::success(String::new()))
        }
        _ => None,
    }
}

/// `drbdadm -c <file> up <resource>`: adopt or create the running
/// resource described by the (real, parsed) file.
fn script_drbdadm_up(
    world: &mut FakeDrbd,
    parsed: &volvisor_drbd::resgen::ParsedResource,
    resource: &str,
) -> Option<CommandOutput> {
    if world.fail_up {
        return Some(CommandOutput::failure("drbdadm up: simulated failure"));
    }
    if world.resources.contains_key(resource) {
        // Real `up` over an up resource is an idempotent success.
        return Some(CommandOutput::success(String::new()));
    }
    // The backing LV must exist (the res file names /dev/<vg>/<lv>).
    let disk = parsed.disks.first().map(String::as_str)?;
    let lv_key = disk.strip_prefix("/dev/")?.to_owned();
    let Some(lv) = world.lvs.get(&lv_key) else {
        return Some(CommandOutput::failure(format!(
            "drbdadm up {resource}: open({disk}) failed: No such file or directory"
        )));
    };
    if !world.metadata.contains(resource) {
        // Real `up` without created metadata fails loudly.
        return Some(CommandOutput::failure(format!(
            "drbdadm up {resource}: no metadata found; run create-md first"
        )));
    }
    let minor = parsed.minor?;
    let peer_node = parsed
        .nodes
        .iter()
        .find(|node| node.name != world.node_name)
        .map_or_else(|| PEER_NODE.to_owned(), |node| node.name.clone());
    let device_size = lv.size.min(world.peer_backing(lv.size));
    world.resources.insert(
        resource.to_owned(),
        FakeResource {
            minor,
            role: Role::Secondary,
            // Fresh metadata is never seeded.
            local_disk: DiskState::Inconsistent,
            peer_disk: world.new_peer_disk.clone(),
            peer_role: world.new_peer_role,
            resyncing: false,
            device_size,
            peer_node,
        },
    );
    Some(CommandOutput::success(String::new()))
}

/// `drbdadm -c <file> primary [--force] <resource>`.
fn script_drbdadm_primary(
    world: &mut FakeDrbd,
    resource: &str,
    force: bool,
) -> Option<CommandOutput> {
    if world.fail_primary {
        return Some(CommandOutput::failure("drbdadm primary: simulated failure"));
    }
    let state = world.resources.get_mut(resource)?;
    if force {
        // The seeding promotion: the forced source becomes UpToDate and
        // the peer is overwritten (resync follows).
        state.local_disk = DiskState::UpToDate;
        if world.peer_online {
            world.peer_overwritten = true;
            if world.resync_completes {
                state.peer_disk = DiskState::UpToDate;
                state.resyncing = false;
            } else {
                state.peer_disk = DiskState::Inconsistent;
                state.resyncing = true;
            }
        }
    } else {
        // Real DRBD refuses a plain promotion over a non-UpToDate disk
        // (data-loss guard) and over a Primary peer (dual-primary).
        if state.local_disk != DiskState::UpToDate {
            return Some(CommandOutput::failure(format!(
                "drbdadm primary {resource}: Refusing to be Primary without at least one \
                 UpToDate disk"
            )));
        }
        if world.peer_online && state.peer_role == Role::Primary {
            return Some(CommandOutput::failure(format!(
                "drbdadm primary {resource}: Refusing to be Primary while peer is Primary"
            )));
        }
    }
    state.role = Role::Primary;
    Some(CommandOutput::success(String::new()))
}

/// `drbdadm -c <file> secondary <resource>`.
fn script_drbdadm_secondary(world: &mut FakeDrbd, resource: &str) -> Option<CommandOutput> {
    if world.fail_secondary {
        return Some(CommandOutput::failure(
            world
                .fail_secondary_message
                .clone()
                .unwrap_or_else(|| "drbdadm secondary: simulated failure".to_owned()),
        ));
    }
    let state = world.resources.get_mut(resource)?;
    // Real kernel refusal while the device is open (EBUSY).
    if world.open_devices.contains(&state.minor) {
        return Some(CommandOutput::failure(format!(
            "drbdadm secondary {resource}: State change failed: (-16) Device or resource busy: \
             /dev/drbd{} is open by another process",
            state.minor
        )));
    }
    state.role = Role::Secondary;
    Some(CommandOutput::success(String::new()))
}

/// `drbdsetup status <resource>`: the verified text grammar for a
/// running resource, or the real "No such resource" failure for a
/// down/unknown one.
fn script_drbdsetup(world: &mut FakeDrbd, args: &[&str]) -> Option<CommandOutput> {
    if args.first().copied() != Some("status") {
        return None;
    }
    let resource: &str = args.get(1)?;
    if world.fail_status {
        // A NON-"No such resource" failure: a transient problem that
        // must surface as INTERNAL, never as a silent down.
        return Some(CommandOutput::failure(
            "drbdsetup status: simulated transient failure (timeout)",
        ));
    }
    match world.resources.get(resource) {
        Some(state) => {
            let text = status_text(world, resource, state);
            Some(CommandOutput::success(text))
        }
        None => Some(CommandOutput::failure(format!(
            "{resource}: No such resource"
        ))),
    }
}

/// `blockdev --getsize64 /dev/drbdN`: the STORED device size.
fn script_blockdev(world: &mut FakeDrbd, args: &[&str]) -> Option<CommandOutput> {
    if args.first().copied() != Some("--getsize64") {
        return None;
    }
    let device = args.get(1)?;
    if world.fail_blockdev {
        return Some(CommandOutput::failure(
            "blockdev: simulated failure (cannot open device)",
        ));
    }
    let minor: u32 = device.strip_prefix("/dev/drbd")?.parse().ok()?;
    let size = world
        .resources
        .values()
        .find(|state| state.minor == minor)
        .map(|state| state.device_size)?;
    Some(CommandOutput::success(format!("{size}\n")))
}

/// `lvs --reportformat json --units b --nosuffix -o
/// vg_name,lv_name,lv_size,lv_tags`: the real report shape (one
/// `report` object with an `lv` array; tags as ONE comma-separated
/// string — the plain-json format LVM prints).
fn script_lvs(world: &FakeDrbd) -> CommandOutput {
    if world.fail_lvs {
        return CommandOutput::failure("lvs: simulated failure");
    }
    let rows: Vec<serde_json::Value> = world
        .lvs
        .iter()
        .filter_map(|(key, lv)| {
            let (vg, name) = key.split_once('/')?;
            Some(serde_json::json!({
                "vg_name": vg,
                "lv_name": name,
                "lv_size": lv.size.to_string(),
                "lv_tags": lv.tags.join(","),
            }))
        })
        .collect();
    CommandOutput::success(serde_json::json!({ "report": [{ "lv": rows }] }).to_string())
}

/// `vgs --reportformat json --units b --nosuffix -o
/// vg_name,vg_free,vg_size,vg_extent_size`.
fn script_vgs(world: &FakeDrbd) -> CommandOutput {
    if world.fail_vgs {
        return CommandOutput::failure("vgs: simulated failure");
    }
    let rows = [serde_json::json!({
        "vg_name": world.vg_name,
        "vg_free": world.free_bytes().to_string(),
        "vg_size": world.vg_free.to_string(),
        "vg_extent_size": world.vg_extent_size.to_string(),
    })];
    CommandOutput::success(serde_json::json!({ "report": [{ "vg": rows }] }).to_string())
}

/// `lvcreate --yes -L <n>B --addtag <owner> --addtag <generation> -n
/// <lv> <vg>`: rounds up to whole extents, stamps the tags, refuses
/// existing names.
fn script_lvcreate(world: &mut FakeDrbd, args: &[&str]) -> Option<CommandOutput> {
    let size = arg_after(args, "-L")?;
    let size: u64 = size.strip_suffix('B')?.parse().ok()?;
    let lv_name = arg_after(args, "-n")?;
    let vg = args.last().copied()?;
    if world.fail_lvcreate {
        return Some(CommandOutput::failure(
            "lvcreate: simulated failure (insufficient free extents)",
        ));
    }
    let key = format!("{vg}/{lv_name}");
    if world.lvs.contains_key(&key) {
        return Some(CommandOutput::failure(format!(
            "Logical volume \"{lv_name}\" already exists in volume group \"{vg}\""
        )));
    }
    let tags: Vec<String> = args
        .windows(2)
        .filter(|window| window[0] == "--addtag")
        .map(|window| window[1].to_owned())
        .collect();
    world.lvs.insert(
        key,
        FakeLv {
            size: extent_round_up(size, world.vg_extent_size),
            tags,
        },
    );
    Some(CommandOutput::success(format!(
        "Logical volume \"{lv_name}\" created"
    )))
}

/// `lvextend --yes -L <n>B <vg>/<lv>`: grow-only, extent-rounded.
fn script_lvextend(world: &mut FakeDrbd, args: &[&str]) -> Option<CommandOutput> {
    let size = arg_after(args, "-L")?;
    let size: u64 = size.strip_suffix('B')?.parse().ok()?;
    let spec = args.last().copied()?;
    let Some(lv) = world.lvs.get_mut(spec) else {
        return Some(CommandOutput::failure(format!(
            "lvextend: Failed to find logical volume \"{spec}\""
        )));
    };
    if world.fail_lvextend {
        return Some(CommandOutput::failure(
            "lvextend: simulated failure (insufficient free space)",
        ));
    }
    let target = extent_round_up(size, world.vg_extent_size);
    if target < lv.size {
        return Some(CommandOutput::failure(format!(
            "lvextend: New size ({target} extents) not larger than existing size"
        )));
    }
    lv.size = target;
    Some(CommandOutput::success(format!(
        "Size of logical volume {spec} changed"
    )))
}

/// `lvremove --yes <vg>/<lv>`: removes the LV and its internal DRBD
/// metadata.
fn script_lvremove(world: &mut FakeDrbd, args: &[&str]) -> Option<CommandOutput> {
    let spec = args.last().copied()?;
    if world.lvs.remove(spec).is_none() {
        return Some(CommandOutput::failure(format!(
            "lvremove: Failed to find logical volume \"{spec}\""
        )));
    }
    if let Some((_, lv_name)) = spec.split_once('/') {
        world.metadata.remove(lv_name);
    }
    Some(CommandOutput::success(String::new()))
}

/// `uname -n`.
fn script_uname(world: &FakeDrbd) -> CommandOutput {
    if world.fail_uname {
        return CommandOutput::failure("uname: simulated failure");
    }
    CommandOutput::success(format!("{}\n", world.node_name))
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A tempdir that outlives the provider it backs (leaked for the test
/// process lifetime; test processes are short-lived).
pub fn leak_tempdir() -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    path
}

/// The fixture configuration derived from one host directory: the
/// provider state, the generated `drbd.d/` config dir, the `proc/`
/// module proof and the secret file all live under `base`.
#[must_use]
pub fn config_for(base: &Path) -> DrbdProviderConfig {
    DrbdProviderConfig {
        vg_name: VG.to_owned(),
        config_dir: base.join("drbd.d"),
        node_name: NODE.to_owned(),
        local_address: LOCAL_ADDR.to_owned(),
        peer_name: PEER_NODE.to_owned(),
        peer_address: PEER_ADDR.to_owned(),
        shared_secret_file: base.join("secret"),
        port_min: PORT_MIN,
        port_max: PORT_MAX,
        minor_min: MINOR_MIN,
        minor_max: MINOR_MAX,
        proc_root: base.join("proc"),
    }
}

/// Build the fixture host directory (state file path, config dir,
/// procfs module file, secret file).
fn host_dir() -> (PathBuf, PathBuf) {
    let base = leak_tempdir();
    std::fs::create_dir(base.join("drbd.d")).expect("config dir");
    std::fs::create_dir(base.join("proc")).expect("proc dir");
    std::fs::write(base.join("proc").join("drbd"), "version: 9.2.x\n").expect("proc module file");
    std::fs::write(base.join("secret"), SECRET).expect("secret file");
    let state_path = base.join("state.json");
    (base, state_path)
}

/// A provider construction kit returning the world handle too, for
/// tests that need to inject faults or inspect the simulation.
pub struct Fixture {
    /// The provider under test.
    pub provider: Arc<DrbdProvider>,
    /// The simulated DRBD + LVM world.
    pub world: Arc<Mutex<FakeDrbd>>,
    /// The scripted runner (for invocation assertions).
    pub runner: Arc<FakeRunner>,
    /// The state file path.
    pub state_path: PathBuf,
    /// The fixture host directory (config dir, proc root, secret).
    pub base: PathBuf,
}

/// Build a fixture over a fresh state file and simulated world.
pub fn fixture() -> Fixture {
    let (base, state_path) = host_dir();
    let world = Arc::new(Mutex::new(FakeDrbd::default()));
    let runner = FakeDrbd::runner(&world);
    let provider = DrbdProvider::new(
        Arc::clone(&runner) as Arc<dyn CommandRunner>,
        config_for(&base),
        state_path.clone(),
    )
    .map(Arc::new)
    .expect("provider construction");
    Fixture {
        provider,
        world,
        runner,
        state_path,
        base,
    }
}

/// Build a fixture whose provider is constructed AFTER `setup` ran
/// against the host directory and world (pre-seeded state-file tests:
/// the provider must load the seeded state at startup).
pub fn fixture_after(setup: impl FnOnce(&Path, &Arc<Mutex<FakeDrbd>>)) -> Fixture {
    let (base, state_path) = host_dir();
    let world = Arc::new(Mutex::new(FakeDrbd::default()));
    setup(&base, &world);
    let runner = FakeDrbd::runner(&world);
    let provider = DrbdProvider::new(
        Arc::clone(&runner) as Arc<dyn CommandRunner>,
        config_for(&base),
        state_path.clone(),
    )
    .map(Arc::new)
    .expect("provider construction");
    Fixture {
        provider,
        world,
        runner,
        state_path,
        base,
    }
}

/// A fresh provider over an existing state file and host directory,
/// sharing the simulated world (restart / crash-replay tests).
pub fn provider_from(state_path: &Path, world: &Arc<Mutex<FakeDrbd>>) -> Arc<DrbdProvider> {
    let runner = FakeDrbd::runner(world);
    let base = state_path
        .parent()
        .expect("state path has a parent directory");
    DrbdProvider::new(runner, config_for(base), state_path.to_path_buf())
        .map(Arc::new)
        .expect("provider construction")
}

/// The peer's `(ip, port)` the fixture definitions carry.
fn peer_endpoint() -> (String, u16) {
    let (ip, port) = PEER_ADDR
        .rsplit_once(':')
        .expect("fixture peer address is <ip>:<port>");
    (ip.to_owned(), port.parse().expect("fixture peer port"))
}

/// The resource definition matching a [`seed_volume`] state entry.
pub fn seed_definition(resource: &str, minor: u32, port: u16) -> ResourceDefinition {
    let (peer_ip, peer_port) = peer_endpoint();
    ResourceDefinition {
        resource_name: resource.to_owned(),
        minor,
        protocol: ReplicationMode::A,
        local_node: NODE.to_owned(),
        local_address: LOCAL_ADDR.to_owned(),
        local_port: port,
        peer_node: PEER_NODE.to_owned(),
        peer_address: peer_ip,
        peer_port,
        disk_path: format!("/dev/{VG}/{resource}"),
        shared_secret: SECRET.to_owned(),
    }
}

/// Write a resource file for a seeded volume into the fixture config
/// dir (`base/drbd.d`).
pub fn write_seed_res_file(base: &Path, resource: &str, minor: u32, port: u16) {
    seed_definition(resource, minor, port)
        .write(&base.join("drbd.d"))
        .expect("write fixture res file");
}

/// Seed a Ready, seeded volume directly into a state file **and** its
/// owned LV, metadata, resource file and running resource (both ends
/// `UpToDate`, Secondary) into the simulated world (reconciliation /
/// foreign-state / lifecycle tests).
pub fn seed_volume(base: &Path, world: &Arc<Mutex<FakeDrbd>>, volume_id: &str, size_bytes: u64) {
    let volume_id = VolumeId::new(volume_id).expect("valid volume id");
    let resource = resource_name_for(&volume_id);
    let state_path = base.join("state.json");
    let mut state = DrbdState::load(&state_path).expect("load state");
    state.insert_volume(
        volume_id.clone(),
        StoredVolume {
            entry: VolumeEntry {
                resource_name: resource.clone(),
                vg_name: VG.to_owned(),
                lv_name: resource.clone(),
                minor: SEED_MINOR,
                port: SEED_PORT,
                size_bytes,
                requested_size_bytes: size_bytes,
                generation: 1,
                project_id: ProjectId::new("seed-project").expect("valid project id"),
                block_size: 4096,
                replication_mode: ReplicationMode::A,
                creation_payload: "seed".to_owned(),
                created_at: 0,
            },
            runtime: VolumeRuntime {
                state: VolumeLifecycle::Ready,
                attachment: None,
                seeded: true,
            },
        },
    );
    // Keep the monotonic counters ahead of the seeded resource so a
    // later allocation can never collide with it.
    state.observe_minor(SEED_MINOR);
    state.observe_port(SEED_PORT);
    state.save(&state_path).expect("seed volume state");
    write_seed_res_file(base, &resource, SEED_MINOR, SEED_PORT);
    let mut world = world.lock().expect("world");
    let key = format!("{VG}/{resource}");
    let extent = world.vg_extent_size;
    world.lvs.insert(
        key.clone(),
        FakeLv {
            size: extent_round_up(size_bytes, extent),
            tags: vec![
                format!("volvisor.owner={}", volume_id.as_str()),
                "volvisor.generation=1".to_owned(),
            ],
        },
    );
    world.metadata.insert(resource.clone());
    let local = world.lvs.get(&key).map_or(size_bytes, |lv| lv.size);
    let device = local.min(world.peer_backing(local));
    world.resources.insert(
        resource.clone(),
        FakeResource {
            minor: SEED_MINOR,
            role: Role::Secondary,
            local_disk: DiskState::UpToDate,
            peer_disk: DiskState::UpToDate,
            peer_role: Role::Secondary,
            resyncing: false,
            device_size: device,
            peer_node: PEER_NODE.to_owned(),
        },
    );
}

/// Seed an LV into the simulated world without a state entry: owned
/// (`Some(volume_id)`) or foreign (`None`) — untracked-LV and
/// reclaim tests.
pub fn seed_lv(
    world: &Arc<Mutex<FakeDrbd>>,
    vg: &str,
    name: &str,
    size_bytes: u64,
    owner: Option<&str>,
) {
    let mut tags = Vec::new();
    if let Some(owner) = owner {
        tags.push(format!("volvisor.owner={owner}"));
        tags.push("volvisor.generation=1".to_owned());
    }
    world.lock().expect("world").lvs.insert(
        format!("{vg}/{name}"),
        FakeLv {
            size: size_bytes,
            tags,
        },
    );
}

// ---------------------------------------------------------------------------
// Conformance-kit class adapter
// ---------------------------------------------------------------------------

/// Test-only adapter that runs the shared conformance kit (whose
/// fixtures are hardwired to the `native-local` P0 profile) against
/// the real [`DrbdProvider`].
///
/// The kit's fixture builders construct bare `native-local` create
/// requests and assert a `native-local` `backend_class` with
/// `Unknown` health axes; the DRBD provider serves only
/// `nearline-replicated` (fail-closed class negotiation, as the trait
/// demands), requires the replication policy the kit never sends, and
/// reports *observed* health on inspect. This adapter rewrites exactly
/// those three presentation concerns — the class field in both
/// directions, the injected `drbd9` replication policy on create, and
/// the health axes back to the kit's `Unknown`-until-proven
/// expectation — and delegates everything else (state, generations,
/// single-writer fencing, capacity, ownership proofs, seeding, grow
/// boundaries, delete) verbatim to the wrapped provider, so the kit
/// exercises the real DRBD semantics.
pub struct KitClassAdapter {
    /// The real provider under test.
    pub provider: Arc<DrbdProvider>,
}

/// Rewrite a response back to the kit's expectations: the
/// `native-local` class, `Unknown` health axes and no remote protection
/// axis (the kit's P0 assertions cannot express observed health or the
/// nearline remote replica; both are real provider semantics asserted
/// in the behavior tests instead).
fn reclass(mut response: InspectVolumeResponse) -> InspectVolumeResponse {
    if response.backend_class == VolumeClass::NearlineReplicated {
        response.backend_class = VolumeClass::NativeLocal;
    }
    response.health = Health::Unknown;
    response.backend_health = Health::Unknown;
    response.effective_protection.remote = volvisor_types::domain::RemoteProtectionAxis::None;
    response
}

/// Inject the `drbd9` nearline policy the kit never sends.
fn nearline_request(mut req: CreateVolumeRequest) -> CreateVolumeRequest {
    if req.volume_class == VolumeClass::NativeLocal {
        req.volume_class = VolumeClass::NearlineReplicated;
    }
    if req.replication.is_none() {
        req.replication = Some(ReplicationPolicyRequest {
            engine: Some("drbd9".to_owned()),
            mode: ReplicationModeRequest::Async,
            remote_replicas: 1,
            allow_degraded_create: false,
        });
    }
    req
}

#[async_trait]
impl VolumeProvider for KitClassAdapter {
    fn name(&self) -> &str {
        self.provider.name()
    }

    fn capabilities(&self) -> CapabilitySet {
        self.provider.capabilities()
    }

    fn supported_classes(&self) -> &[VolumeClass] {
        self.provider.supported_classes()
    }

    async fn create_volume(
        &self,
        req: &CreateVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError> {
        let req = nearline_request(req.clone());
        self.provider.create_volume(&req).await.map(reclass)
    }

    async fn inspect_volume(&self, id: &VolumeId) -> Result<InspectVolumeResponse, ApiError> {
        self.provider.inspect_volume(id).await.map(reclass)
    }

    async fn list_volumes(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<InspectVolumeResponse>, ApiError> {
        self.provider
            .list_volumes(project)
            .await
            .map(|volumes| volumes.into_iter().map(reclass).collect())
    }

    async fn attach_volume(
        &self,
        volume_id: &VolumeId,
        req: &AttachVolumeRequest,
    ) -> Result<AttachVolumeResponse, ApiError> {
        self.provider.attach_volume(volume_id, req).await
    }

    async fn detach_volume(
        &self,
        volume_id: &VolumeId,
        attachment_id: &AttachmentId,
        req: &DetachVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError> {
        self.provider
            .detach_volume(volume_id, attachment_id, req)
            .await
            .map(reclass)
    }

    async fn grow_volume(
        &self,
        volume_id: &VolumeId,
        req: &GrowVolumeRequest,
    ) -> Result<GrowVolumeResponse, ApiError> {
        self.provider.grow_volume(volume_id, req).await
    }

    async fn delete_volume(
        &self,
        volume_id: &VolumeId,
        req: &DeleteVolumeRequest,
    ) -> Result<(), ApiError> {
        self.provider.delete_volume(volume_id, req).await
    }
}

/// A provider over an isolated simulated world for each conformance
/// check, wrapped in the kit's class adapter.
pub fn conformance_provider() -> Arc<KitClassAdapter> {
    let fixture = fixture();
    Arc::new(KitClassAdapter {
        provider: fixture.provider,
    })
}
