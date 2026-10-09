//! Shared fixtures for the `volvisor-ceph` integration tests.
//!
//! [`FakeCeph`] is a minimal stateful Ceph simulation driven through the
//! closure mode of [`FakeRunner`]: images (with sizes, features and
//! `volvisor.*` metadata), the trash and the `rbd map` device mappings
//! live in a shared map; `rbd create`/`resize`/`trash move`/`map`/`unmap`
//! mutate them, and `rbd ls`/`info`/`showmapped`/`trash ls` plus the
//! `ceph fsid`/`health`/`df` queries report them — so the provider's
//! verification steps (sizes read back from `rbd info`, trash placement
//! from `rbd trash ls`, mappings from `rbd showmapped`) exercise real
//! round-trips instead of echoes.
//!
//! The simulation mirrors real CLI semantics that matter to the provider:
//! `rbd map` on an image lacking `exclusive-lock` fails, `rbd create` on
//! an existing name fails "already exists", `rbd unmap` on a missing
//! device fails, `rbd trash move` on a missing image fails, `rbd info`
//! on a missing image fails ENOENT-style, and `rbd showmapped` reflects
//! the live mapping table in the REAL output shape (an object with a
//! `devices` array). Sizes are byte-granular — there is **no**
//! extent rounding anywhere.
//!
//! The argv contract is pinned inside the fake: every invocation must
//! carry the configured FULL entity name via `--name` (fail the
//! dispatch otherwise), so a regression back to `--id` (which
//! double-prefixes) cannot pass silently.
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

use async_trait::async_trait;
use volvisor_ceph::provider::{CephProviderConfig, CephRbdProvider, image_name_for};
use volvisor_ceph::state::{CephState, StoredVolume, VolumeEntry, VolumeRuntime};
use volvisor_ceph::{CommandOutput, CommandRunner, FakeRunner};
use volvisor_provider::VolumeProvider;
use volvisor_types::domain::VolumeClass;
use volvisor_types::request::{
    AttachVolumeRequest, AttachVolumeResponse, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, GrowVolumeRequest, GrowVolumeResponse, InspectVolumeResponse,
};
use volvisor_types::{ApiError, AttachmentId, CapabilitySet, ProjectId, VolumeId, VolumeLifecycle};

/// The simulated cluster FSID.
pub const FSID: &str = "f340f0d0-feed-4000-8000-000000000001";
/// The monitor addresses every fixture configures.
pub const MON_HOSTS: &[&str] = &["mon-a:6789", "mon-b:6789"];
/// The pool the fixture provider operates on.
pub const POOL: &str = "volvisortest";
/// The Ceph user the fixture provider authenticates as (the FULL entity
/// name, passed via `--name` and pinned by the fake's dispatch check).
pub const USER: &str = "client.volvisor";
/// Simulated pool capacity (1 TiB) before any image allocation.
pub const POOL_MAX_AVAIL: u64 = 1 << 40;
/// The ownership metadata key (mirrors the provider constant).
pub const OWNER_META_KEY: &str = "volvisor.owner";
/// The generation metadata key (mirrors the provider constant).
pub const GENERATION_META_KEY: &str = "volvisor.generation";

/// One simulated RBD image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeImage {
    /// Image size in bytes (byte-granular).
    pub size: u64,
    /// Enabled image features (`exclusive-lock`, `layering`, ...).
    pub features: Vec<String>,
    /// Image metadata (`volvisor.owner`, `volvisor.generation`, ...).
    pub meta: BTreeMap<String, String>,
}

impl FakeImage {
    /// An image shaped exactly like a volvisor create: both features and
    /// the ownership record stamped.
    #[must_use]
    pub fn owned(size: u64, volume_id: &str) -> Self {
        Self {
            size,
            features: vec!["exclusive-lock".to_owned(), "layering".to_owned()],
            meta: BTreeMap::from([
                (OWNER_META_KEY.to_owned(), volume_id.to_owned()),
                (GENERATION_META_KEY.to_owned(), "1".to_owned()),
            ]),
        }
    }

    /// A foreign image: no volvisor metadata, no exclusive-lock.
    #[must_use]
    pub fn foreign(size: u64) -> Self {
        Self {
            size,
            features: Vec::new(),
            meta: BTreeMap::new(),
        }
    }
}

/// The simulated Ceph world shared between the provider and assertions.
pub struct FakeCeph {
    /// Cluster FSID reported by `ceph fsid`.
    pub fsid: String,
    /// Cluster status reported by `ceph health detail` (`HEALTH_OK`, ...).
    pub health: String,
    /// Pool free capacity **before any image allocations** (the baseline).
    /// The `ceph df` max_avail is derived: this baseline minus the sizes
    /// of all images currently present, mirroring real Ceph where
    /// `rbd create`/`resize` consume space and trash/delete returns it.
    pub pool_max_avail: u64,
    /// Pool baseline `bytes_used` before any image allocations.
    pub pool_bytes_used: u64,
    /// Pool replication `size` reported by `ceph osd pool get`.
    pub pool_size: u64,
    /// Pool replication `min_size` reported by `ceph osd pool get`.
    pub pool_min_size: u64,
    /// Image name → simulated image.
    pub images: BTreeMap<String, FakeImage>,
    /// Image names moved to the RBD trash (`rbd trash ls` reports them).
    pub trash: Vec<String>,
    /// Mapped device path (`/dev/rbdN`) → image name.
    pub mappings: BTreeMap<String, String>,
    /// Counter for the next `/dev/rbdN` device number.
    pub next_device: u64,
    /// When true, `rbd create` fails.
    pub fail_rbd_create: bool,
    /// When true, `rbd map` fails.
    pub fail_map: bool,
    /// When true, `rbd unmap` fails.
    pub fail_unmap: bool,
    /// When true, `rbd trash move` fails.
    pub fail_trash: bool,
    /// When true, `ceph health detail` fails.
    pub fail_health: bool,
    /// When true, `ceph df` fails.
    pub fail_df: bool,
    /// When true, `ceph fsid` fails.
    pub fail_fsid: bool,
    /// When true, `rbd showmapped` fails.
    pub fail_showmapped: bool,
    /// When true, `rbd image-meta set` fails.
    pub fail_meta_set: bool,
    /// When true, `rbd image-meta get` fails with a NON-ENOENT message
    /// (a transient read failure, e.g. a mon timeout — must never be
    /// mistaken for genuine key absence).
    pub fail_meta_get_transient: bool,
    /// When true, `ceph osd pool get` fails.
    pub fail_pool_get: bool,
    /// When true, `rbd create` claims success but never shows up in
    /// `rbd ls` (verification-failure injection).
    pub create_silent: bool,
    /// When true, `rbd resize` claims success but never updates the
    /// image size (verification-failure injection).
    pub resize_silent: bool,
}

impl Default for FakeCeph {
    fn default() -> Self {
        Self {
            fsid: FSID.to_owned(),
            health: "HEALTH_OK".to_owned(),
            pool_max_avail: POOL_MAX_AVAIL,
            pool_bytes_used: 0,
            pool_size: 3,
            pool_min_size: 2,
            images: BTreeMap::new(),
            trash: Vec::new(),
            mappings: BTreeMap::new(),
            next_device: 0,
            fail_rbd_create: false,
            fail_map: false,
            fail_unmap: false,
            fail_trash: false,
            fail_health: false,
            fail_df: false,
            fail_fsid: false,
            fail_showmapped: false,
            fail_meta_set: false,
            fail_meta_get_transient: false,
            fail_pool_get: false,
            create_silent: false,
            resize_silent: false,
        }
    }
}

impl FakeCeph {
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

/// The JSON body of `ceph df --format json` for the simulated world.
///
/// Mirrors the REAL per-pool stats shape (`stored`/`objects`/`kb_used`/
/// `bytes_used`/`percent_used`/`max_avail` — no replication fields;
/// those belong to `ceph osd pool get`). Reported `max_avail` is
/// derived (baseline minus live image sizes) so capacity checks observe
/// real consumption; `bytes_used` is the sum of the live image sizes
/// over the baseline.
fn ceph_df_body(world: &FakeCeph) -> String {
    let allocated: u64 = world.images.values().map(|image| image.size).sum();
    serde_json::json!({
        "pools": [
            {
                "name": POOL,
                "stats": {
                    "stored": 0,
                    "objects": 0,
                    "kb_used": world.pool_bytes_used.saturating_add(allocated) / 1024,
                    "bytes_used": world.pool_bytes_used.saturating_add(allocated),
                    "percent_used": 0.01,
                    "max_avail": world.pool_max_avail.saturating_sub(allocated),
                },
            },
            {"name": "other-pool", "stats": {"bytes_used": 1, "max_avail": 1}},
        ]
    })
    .to_string()
}

/// The JSON body of `rbd showmapped --format json` — the REAL shape: an
/// object with a `devices` array (each entry carrying the device id,
/// pool, image name, snapshot and device path).
fn showmapped_body(world: &FakeCeph) -> String {
    let devices: Vec<serde_json::Value> = world
        .mappings
        .iter()
        .map(|(device, image)| {
            serde_json::json!({
                "id": device.trim_start_matches("/dev/rbd"),
                "pool": POOL,
                "name": image,
                "snap": "-",
                "device": device,
            })
        })
        .collect();
    serde_json::json!({ "devices": devices }).to_string()
}

/// Skip the `-m <mons>` / `--name <user>` global flag pairs and return the
/// CLI subcommand.
fn subcommand<'a>(args: &[&'a str]) -> Option<&'a str> {
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "-m" | "--name" => index += 2,
            subcommand => return Some(subcommand),
        }
    }
    None
}

/// The value following `flag` in `args`.
fn arg_after<'a>(args: &[&'a str], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| *arg == flag)
        .and_then(|index| args.get(index + 1))
        .copied()
}

/// Split a `pool/image` spec.
fn split_spec(spec: &str) -> Option<(&str, &str)> {
    spec.split_once('/')
}

/// The scripted behavior for one command.
///
/// The argv contract is pinned first: both CLIs must receive the FULL
/// configured entity name via `--name`; a dispatch without it fails the
/// fake (an `INTERNAL` "not scripted" error), so a regression back to
/// `--id` (which would authenticate as the nonexistent
/// `client.client.volvisor` on a real cluster) cannot pass silently.
fn script(world: &mut FakeCeph, program: &str, args: &[&str]) -> Option<CommandOutput> {
    if arg_after(args, "--name") != Some(USER) {
        return None;
    }
    match program {
        "ceph" => match subcommand(args)? {
            "fsid" => {
                if world.fail_fsid {
                    Some(CommandOutput::failure("ceph fsid: simulated failure"))
                } else {
                    Some(CommandOutput::success(world.fsid.clone()))
                }
            }
            "health" => {
                if world.fail_health {
                    Some(CommandOutput::failure(
                        "ceph health: simulated failure (cluster unreachable)",
                    ))
                } else {
                    Some(CommandOutput::success(
                        serde_json::json!({ "status": world.health }).to_string(),
                    ))
                }
            }
            "df" => {
                if world.fail_df {
                    Some(CommandOutput::failure("ceph df: simulated failure"))
                } else {
                    Some(CommandOutput::success(ceph_df_body(world)))
                }
            }
            "osd" => script_ceph_osd(world, args),
            _ => None,
        },
        "rbd" => script_rbd(world, args),
        _ => None,
    }
}

/// `ceph osd pool get <pool> <key> --format json` (read-only policy
/// query). Real output is a small object such as `{"size":"3"}`.
fn script_ceph_osd(world: &mut FakeCeph, args: &[&str]) -> Option<CommandOutput> {
    // args: [-m, mons, --name, user,] osd pool get <pool> <key> --format json
    if args.get(5).copied() != Some("pool") || args.get(6).copied() != Some("get") {
        return None;
    }
    let pool = args.get(7)?;
    let key = args.get(8)?;
    if pool != &POOL {
        return Some(CommandOutput::failure(format!(
            "pool {pool} does not exist"
        )));
    }
    if world.fail_pool_get {
        return Some(CommandOutput::failure(
            "ceph osd pool get: simulated failure (mon timeout)",
        ));
    }
    let value = match *key {
        "size" => world.pool_size,
        "min_size" => world.pool_min_size,
        _ => return None,
    };
    // Real output shape: {"pool": "<pool>", "key": "<key>", "<key>": "<n>"}
    // (values are strings; extra fields ride along).
    let mut body = serde_json::Map::new();
    body.insert("pool".to_owned(), serde_json::json!(POOL));
    body.insert("key".to_owned(), serde_json::json!(*key));
    body.insert((*key).to_owned(), serde_json::json!(value.to_string()));
    Some(CommandOutput::success(
        serde_json::Value::Object(body).to_string(),
    ))
}

/// The scripted behavior for one `rbd` invocation.
fn script_rbd(world: &mut FakeCeph, args: &[&str]) -> Option<CommandOutput> {
    match subcommand(args)? {
        "create" => script_rbd_create(world, args),
        "info" => script_rbd_info(world, args),
        "image-meta" => script_rbd_image_meta(world, args),
        "resize" => script_rbd_resize(world, args),
        "map" => script_rbd_map(world, args),
        "unmap" => script_rbd_unmap(world, args),
        "showmapped" => {
            if world.fail_showmapped {
                Some(CommandOutput::failure("rbd showmapped: simulated failure"))
            } else {
                Some(CommandOutput::success(showmapped_body(world)))
            }
        }
        "trash" => script_rbd_trash(world, args),
        "ls" => {
            // rbd ls --pool <pool> --format json
            let names: Vec<String> = world.images.keys().cloned().collect();
            Some(CommandOutput::success(
                serde_json::Value::Array(
                    names.into_iter().map(serde_json::Value::String).collect(),
                )
                .to_string(),
            ))
        }
        "rm" => {
            // rbd rm <pool>/<image> (best-effort cleanup path)
            let spec = *args.last()?;
            let (_, image) = split_spec(spec)?;
            if world.images.remove(image).is_some() {
                Some(CommandOutput::success(String::new()))
            } else {
                Some(CommandOutput::failure(format!(
                    "rbd: error opening image {spec}: (2) No such file or directory"
                )))
            }
        }
        _ => None,
    }
}

/// `rbd create --image-feature <features> -s <size>B <pool>/<image>`.
fn script_rbd_create(world: &mut FakeCeph, args: &[&str]) -> Option<CommandOutput> {
    let size = arg_after(args, "-s")?;
    let size: u64 = size.trim_end_matches('B').parse().ok()?;
    let spec = *args.last()?;
    let (_, image) = split_spec(spec)?;
    if world.fail_rbd_create {
        return Some(CommandOutput::failure("rbd create: simulated failure"));
    }
    // Real rbd refuses to create an image whose name already exists.
    if world.images.contains_key(image) {
        return Some(CommandOutput::failure(format!(
            "rbd: {spec} already exists"
        )));
    }
    if !world.create_silent {
        let features = arg_after(args, "--image-feature")
            .map(|features| features.split(',').map(str::to_owned).collect())
            .unwrap_or_default();
        world.images.insert(
            image.to_owned(),
            FakeImage {
                size,
                features,
                meta: BTreeMap::new(),
            },
        );
    }
    Some(CommandOutput::success(String::new()))
}

/// `rbd info --format json <pool>/<image>`.
fn script_rbd_info(world: &mut FakeCeph, args: &[&str]) -> Option<CommandOutput> {
    let spec = *args.last()?;
    let (_, image) = split_spec(spec)?;
    match world.images.get(image) {
        None => Some(CommandOutput::failure(format!(
            "rbd: error opening image {spec}: (2) No such file or directory"
        ))),
        Some(image) => Some(CommandOutput::success(
            serde_json::json!({
                "size": image.size,
                "features": image.features,
            })
            .to_string(),
        )),
    }
}

/// `rbd image-meta set|get <pool>/<image> <key> [<value>]`.
fn script_rbd_image_meta(world: &mut FakeCeph, args: &[&str]) -> Option<CommandOutput> {
    match args.get(5).copied()? {
        "set" => {
            // rbd image-meta set <pool>/<image> <key> <value>
            let spec = *args.get(6)?;
            let key = *args.get(7)?;
            let value = *args.get(8)?;
            let (_, image) = split_spec(spec)?;
            if world.fail_meta_set {
                return Some(CommandOutput::failure(
                    "rbd image-meta set: simulated failure",
                ));
            }
            match world.images.get_mut(image) {
                None => Some(CommandOutput::failure(format!(
                    "rbd: error opening image {spec}: (2) No such file or directory"
                ))),
                Some(image) => {
                    image.meta.insert(key.to_owned(), value.to_owned());
                    Some(CommandOutput::success(String::new()))
                }
            }
        }
        "get" => {
            // rbd image-meta get <pool>/<image> <key>
            let spec = *args.get(6)?;
            let key = *args.get(7)?;
            let (_, image) = split_spec(spec)?;
            if world.fail_meta_get_transient {
                // A NON-ENOENT failure: a transient read problem that the
                // provider must surface as a typed error, never as key
                // absence.
                return Some(CommandOutput::failure(
                    "rbd image-meta get: simulated transient failure (mon timeout)",
                ));
            }
            match world
                .images
                .get(image)
                .and_then(|image| image.meta.get(key))
            {
                // Real rbd reports a missing metadata key as an
                // ENOENT-class failure ("(2) No such file or directory").
                None => Some(CommandOutput::failure(format!(
                    "failed to get metadata {key} of image {spec}: (2) No such file or directory"
                ))),
                Some(value) => Some(CommandOutput::success(value.clone())),
            }
        }
        _ => None,
    }
}

/// `rbd resize --allow-shrink=false -s <size>B <pool>/<image>`.
fn script_rbd_resize(world: &mut FakeCeph, args: &[&str]) -> Option<CommandOutput> {
    let size = arg_after(args, "-s")?;
    let size: u64 = size.trim_end_matches('B').parse().ok()?;
    let spec = *args.last()?;
    let (_, image) = split_spec(spec)?;
    match world.images.get_mut(image) {
        None => Some(CommandOutput::failure(format!(
            "rbd: error opening image {spec}: (2) No such file or directory"
        ))),
        Some(image) => {
            if size < image.size {
                return Some(CommandOutput::failure(
                    "rbd resize: shrinking is not allowed (--allow-shrink=false)",
                ));
            }
            if !world.resize_silent {
                image.size = size;
            }
            Some(CommandOutput::success(String::new()))
        }
    }
}

/// `rbd map --image <image> --pool <pool>`.
fn script_rbd_map(world: &mut FakeCeph, args: &[&str]) -> Option<CommandOutput> {
    let image = arg_after(args, "--image")?;
    let Some(entry) = world.images.get(image) else {
        return Some(CommandOutput::failure(format!(
            "rbd: error opening image {POOL}/{image}: (2) No such file or directory"
        )));
    };
    if !entry.features.iter().any(|f| f == "exclusive-lock") {
        return Some(CommandOutput::failure(
            "rbd: failed to lock: image lacks the exclusive-lock feature",
        ));
    }
    if world
        .mappings
        .values()
        .any(|mapped| mapped.as_str() == image)
    {
        return Some(CommandOutput::failure(format!(
            "rbd: {POOL}/{image} is already mapped by this client"
        )));
    }
    if world.fail_map {
        return Some(CommandOutput::failure("rbd map: simulated failure"));
    }
    let device = format!("/dev/rbd{}", world.next_device);
    world.next_device += 1;
    world.mappings.insert(device.clone(), image.to_owned());
    Some(CommandOutput::success(device))
}

/// `rbd unmap <device>`.
fn script_rbd_unmap(world: &mut FakeCeph, args: &[&str]) -> Option<CommandOutput> {
    let device = *args.last()?;
    if world.fail_unmap {
        return Some(CommandOutput::failure("rbd unmap: simulated failure"));
    }
    // Real rbd unmap of an unmapped device fails loudly.
    if world.mappings.remove(device).is_none() {
        return Some(CommandOutput::failure(format!("rbd: {device}: not mapped")));
    }
    Some(CommandOutput::success(String::new()))
}

/// `rbd trash move|ls [--pool <pool>]`.
fn script_rbd_trash(world: &mut FakeCeph, args: &[&str]) -> Option<CommandOutput> {
    match args.get(5).copied()? {
        "move" => {
            // rbd trash move <pool>/<image>
            let spec = *args.get(6)?;
            let (_, image) = split_spec(spec)?;
            if world.fail_trash {
                return Some(CommandOutput::failure("rbd trash move: simulated failure"));
            }
            if world.images.remove(image).is_some() {
                world.trash.push(image.to_owned());
                Some(CommandOutput::success(String::new()))
            } else {
                Some(CommandOutput::failure(format!(
                    "rbd: error opening image {spec}: (2) No such file or directory"
                )))
            }
        }
        "ls" => {
            // rbd trash ls --pool <pool> --format json
            let entries: Vec<serde_json::Value> = world
                .trash
                .iter()
                .map(|name| serde_json::json!({ "name": name }))
                .collect();
            Some(CommandOutput::success(
                serde_json::Value::Array(entries).to_string(),
            ))
        }
        _ => None,
    }
}

/// A tempdir that outlives the provider it backs (leaked for the test
/// process lifetime; test processes are short-lived).
pub fn leak_tempdir() -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    path
}

/// The fixture provider configuration over the simulated cluster.
#[must_use]
pub fn config() -> CephProviderConfig {
    CephProviderConfig {
        cluster_fsid: FSID.to_owned(),
        mon_hosts: MON_HOSTS.iter().map(|mon| (*mon).to_owned()).collect(),
        pool: POOL.to_owned(),
        user: USER.to_owned(),
    }
}

/// A provider construction kit returning the world handle too, for tests
/// that need to inject faults or inspect the simulation.
pub struct Fixture {
    /// The provider under test.
    pub provider: Arc<CephRbdProvider>,
    /// The simulated Ceph world.
    pub world: Arc<Mutex<FakeCeph>>,
    /// The scripted runner (for invocation assertions).
    pub runner: Arc<FakeRunner>,
    /// The state file path.
    pub state_path: PathBuf,
}

/// Build a fixture over a fresh state file and simulated cluster.
pub fn fixture() -> Fixture {
    let state_path = leak_tempdir().join("state.json");
    let world = Arc::new(Mutex::new(FakeCeph::default()));
    let runner = FakeCeph::runner(&world);
    let provider = CephRbdProvider::new(
        Arc::clone(&runner) as Arc<dyn CommandRunner>,
        config(),
        state_path.clone(),
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

/// A fresh provider over an existing state file, sharing the simulated
/// cluster (restarts / crash-replay tests).
pub fn provider_from(
    state_path: &std::path::Path,
    world: &Arc<Mutex<FakeCeph>>,
) -> Arc<CephRbdProvider> {
    let runner = FakeCeph::runner(world);
    CephRbdProvider::new(runner, config(), state_path.to_path_buf())
        .map(Arc::new)
        .expect("provider construction")
}

/// Seed a Ready volume directly into a state file **and** its owned image
/// into the simulated world (reconciliation / foreign-state tests).
pub fn seed_volume(
    state_path: &std::path::Path,
    world: &Arc<Mutex<FakeCeph>>,
    volume_id: &str,
    size_bytes: u64,
) {
    let volume_id = VolumeId::new(volume_id).expect("valid volume id");
    let image_name = image_name_for(&volume_id);
    let mut state = CephState::load(state_path).expect("load state");
    state.insert_volume(
        volume_id.clone(),
        StoredVolume {
            entry: VolumeEntry {
                image_name: image_name.clone(),
                size_bytes,
                requested_size_bytes: size_bytes,
                generation: 1,
                project_id: ProjectId::new("seed-project").expect("valid project id"),
                block_size: 4096,
                creation_payload: "seed".to_owned(),
                created_at: 0,
            },
            runtime: VolumeRuntime {
                state: VolumeLifecycle::Ready,
                attachment: None,
            },
        },
    );
    state.save(state_path).expect("seed volume state");
    world
        .lock()
        .expect("world")
        .images
        .insert(image_name, FakeImage::owned(size_bytes, volume_id.as_str()));
}

/// Seed a foreign image (no volvisor metadata) into the simulated world.
pub fn seed_foreign_image(world: &Arc<Mutex<FakeCeph>>, name: &str, size_bytes: u64) {
    world
        .lock()
        .expect("world")
        .images
        .insert(name.to_owned(), FakeImage::foreign(size_bytes));
}

/// Map an image directly in the simulated world (stale-mapping injection).
pub fn seed_mapping(world: &Arc<Mutex<FakeCeph>>, image_name: &str) -> String {
    let mut world = world.lock().expect("world");
    let device = format!("/dev/rbd{}", world.next_device);
    world.next_device += 1;
    world.mappings.insert(device.clone(), image_name.to_owned());
    device
}

// ---------------------------------------------------------------------------
// Conformance-kit class adapter
// ---------------------------------------------------------------------------

/// Test-only adapter that runs the shared conformance kit (whose fixtures
/// are hardwired to the `native-local` P0 profile) against the real
/// [`CephRbdProvider`].
///
/// The kit's fixture builders construct `native-local` create requests
/// and assert a `native-local` `backend_class`; the Ceph provider serves
/// only `ceph-rbd` (fail-closed class negotiation, as the trait demands).
/// This adapter rewrites **only the class field** in both directions and
/// delegates everything else — state, generations, single-writer
/// fencing, capacity, ownership proofs, trash — verbatim to the wrapped
/// provider, so the kit exercises the real Ceph semantics.
pub struct KitClassAdapter {
    /// The real provider under test.
    pub provider: Arc<CephRbdProvider>,
}

/// Rewrite the response class back to the kit's expectation.
fn reclass(mut response: InspectVolumeResponse) -> InspectVolumeResponse {
    if response.backend_class == VolumeClass::CephRbd {
        response.backend_class = VolumeClass::NativeLocal;
    }
    response
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
        let mut req = req.clone();
        if req.volume_class == VolumeClass::NativeLocal {
            req.volume_class = VolumeClass::CephRbd;
        }
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

/// A provider over an isolated simulated cluster for each conformance
/// check, wrapped in the kit's class adapter.
pub fn conformance_provider() -> Arc<KitClassAdapter> {
    let fixture = fixture();
    Arc::new(KitClassAdapter {
        provider: fixture.provider,
    })
}
