//! # volvisor-drbd-testkit
//!
//! The shared test kit for the volvisor DRBD fake world — **test-only,
//! never production**. Extracted from the `volvisor-drbd` integration
//! tests' `common` module so the daemon-level end-to-end tests (the
//! P4b stage-B2 `migration_e2e` harness) can build both migration
//! hosts over the same simulated DRBD + LVM world the provider tests
//! use.
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
//! - `drbdadm create-md` models the real `drbdmeta` semantics: over an
//!   all-zero backing LV it (re-)initializes the metadata — even over
//!   a crashed predecessor's existing metadata — while a backing LV
//!   carrying non-zero data is refused ("Operation refused");
//! - `drbdadm primary` without `--force` refuses over a non-`UpToDate`
//!   local disk ("Refusing to be Primary..."), and `secondary` refuses
//!   while the device is open with the real held-open stderr
//!   (`(-12) Device is held open by someone` plus the kernel's opener
//!   info);
//! - `drbdsetup status` answers in the verified text grammar of the
//!   claimed `DRBDADM_VERSION=9.29.0` — including the unconditional
//!   indent-2 `open:` line, reflecting the world's open-devices state —
//!   and fails "No such resource" for unknown/down resources;
//! - a minor pinned in the world's peer-apply-lag set models
//!   asynchronous peer apply that has not converged through a
//!   suspension's boundary: its status reports a resync in progress
//!   (`replication:SyncSource ... peer-disk:Inconsistent`) until the
//!   pin clears, so a convergence proof must observe the real tokens
//!   rather than assume catch-up (P4b plan §8 item 3);
//! - `drbdsetup suspend-io`/`resume-io` freeze/unfreeze a device's
//!   data path by MINOR (a bare decimal or `/dev/drbd<N>` — a bare
//!   resource name is NOT resolvable, mirroring drbdsetup's
//!   `dt_minor_of_dev`), and a suspended resource carries the verified
//!   `suspended:user` qualifier on its `drbdsetup status` resource
//!   line;
//! - `drbdsetup show-gi <resource> <peer-node-id> <volume>` answers in
//!   the verified data-generation-identity shape (the ASCII-art
//!   pretty print over the current/bitmap/history UUID line) from the
//!   per-resource lineage set assigned at create-md time;
//! - `blockdev --getsize64` reports the STORED device size (set at `up`
//!   and `resize` to the minimum of the local and peer backing sizes),
//!   never a live computation.
//!
//! # The fake data path (P5 plan §2)
//!
//! The world also models **content**, not just state: every running
//! resource carries a block map ([`Block`], 4 KiB logical blocks,
//! sparse — an absent block reads as zeroes under the resource's
//! lineage), opened through the kit's [`open_device`] handle, which
//! enforces the single-writer and suspension rules the real stack
//! enforces at the device (writes land only on the Primary; a
//! suspended minor refuses writes; a held handle blocks the demote
//! exactly as the `FakeVmm` device hooks do). A write's return is the
//! **source-side ack** (Protocol C-shaped up to the source's own map,
//! rule 16): the bytes land in the source's map **and** in the
//! resource's async peer-apply queue ([`QueuedApply`]), and reach the
//! peer's map only when the queue drains — through the campaign's
//! [`apply_peer_writes`] (pre-quiesce lag shaping only), the fake's
//! steady-state protocol-A transport ([`spawn_peer_transport`], the
//! lagged link a live writer needs for convergence to be observable
//! at all) or the fake's
//! content-copying resync (the system path: the post-barrier drain at
//! `suspend-io` and the completing seeding resync). The
//! data-bearing status tokens the convergence gate reads
//! (`peer-disk:UpToDate`, no `replication:` line) are **derived from
//! the queue's state** (P5 plan §2.3, the anti-circularity hinge): a
//! resource reads `UpToDate` only when its apply queue is fully
//! drained — an empty queue is trivially drained, so write-free
//! fixtures observe the same statuses as before. `read_raw`/
//! `write_raw` are the post-mortem/injection surfaces that bypass
//! role, suspension and the openers set — TEST-ONLY, like the
//! `FakeFailKnobs` precedent: assertion and driving never use them.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention (the
//! same rule the `volvisor-drbd` integration tests follow).

#![allow(dead_code)]
#![allow(clippy::expect_used, clippy::unwrap_used)]
// The simulated world is a fault-injection matrix; one bool per scripted
// failure is the clearest shape for test code.
#![allow(clippy::struct_excessive_bools)]
// Doc-formatting churn lints (the workspace `missing_errors_doc`/
// `doc_markdown` precedent): every fixture builder documents its
// `expect` discipline in prose; a generated `# Panics` section and
// `#[must_use]` on every accessor add churn, not safety, to test-only
// code.
#![allow(clippy::missing_panics_doc, clippy::must_use_candidate)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use async_trait::async_trait;
use volvisor_drbd::provider::{DrbdProvider, DrbdProviderConfig, resource_name_for};
use volvisor_drbd::report::{DiskState, Role};
use volvisor_drbd::resgen::{ResourceDefinition, parse_resource_file};
use volvisor_drbd::state::{DrbdState, ReplicationMode, StoredVolume, VolumeEntry, VolumeRuntime};
use volvisor_drbd::{AuthorityContext, CommandOutput, CommandRunner, FakeRunner};
use volvisor_provider::VolumeProvider;
use volvisor_types::domain::{Health, VolumeClass};
use volvisor_types::request::{
    AttachVolumeRequest, AttachVolumeResponse, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, GrowVolumeRequest, GrowVolumeResponse, InspectVolumeResponse,
    ReplicationModeRequest, ReplicationPolicyRequest,
};
use volvisor_types::{
    ApiError, ApiErrorCode, AttachmentId, CapabilitySet, ProjectId, VolumeId, VolumeLifecycle,
};

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
/// The logical block size of the fake data path (P5 plan §2.1): one
/// [`Block`] payload, the unit the write-trace oracle writes, verifies
/// and injects.
pub const BLOCK_SIZE: usize = 4096;

/// One 4 KiB data block of the fake data path (P5 plan §2.1): the
/// bytes, the data-generation identity set that wrote them and the
/// per-block state of the peer-apply window.
///
/// The flag is the **writing side's** bookkeeping: a block written
/// through a [`DeviceHandle`] starts `false` and flips only when the
/// peer-apply queue drains its content to the peer (or the fake's
/// content-copying resync runs). Blocks arriving in a peer world's
/// map through replication land `false` — their window is the
/// *sender's* map's concern; presence in the receiving map is itself
/// the ground truth the oracle verifies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// The bytes (crc-checked by the oracle).
    pub payload: [u8; BLOCK_SIZE],
    /// The data-generation UUID set that wrote this block (the
    /// resource's lineage at write time).
    pub lineage: GiSet,
    /// Whether the peer has applied this exact content yet (the
    /// async peer-apply window's per-block state).
    pub applied_at_peer: bool,
}

/// One entry of a resource's async peer-apply queue (P5 plan §2.3): a
/// source-side write that has been **acked** (it landed in the
/// source's block map) but not yet applied at the peer. Entries carry
/// their full payload and are applied in queue order by a drain —
/// [`apply_peer_writes`] (the campaign's pre-quiesce lag control) or
/// the fake's content-copying resync (the system path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedApply {
    /// The write's acknowledgment sequence (monotonic per resource;
    /// the value [`DeviceHandle::write`] returned).
    pub seq: u64,
    /// The logical block index.
    pub block: u64,
    /// The payload as acknowledged at the source.
    pub payload: [u8; BLOCK_SIZE],
    /// The lineage the write carried.
    pub lineage: GiSet,
}

/// One simulated logical volume (keyed `vg/lv`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeLv {
    /// Size in bytes (extent-rounded at creation/extension, like real
    /// thick LVM).
    pub size: u64,
    /// LVM tags (`volvisor.owner=<id>`, `volvisor.generation=1`, ...).
    pub tags: Vec<String>,
    /// Whether the LV's data area carries non-zero data. Real
    /// `drbdmeta` re-initializes metadata over an all-zero data area
    /// (even one with existing metadata) and refuses create-md only
    /// over non-zero data, so this is the bit its decision rests on.
    /// Fresh `lvcreate` starts all-zero; seeding (`primary --force`)
    /// and fixture pins mark data.
    pub has_data: bool,
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
    /// The resource's block map (P5 plan §2.1): logical block index →
    /// content. SPARSE — a block absent from the map reads as a
    /// zeroed payload under the resource's lineage (an unwritten
    /// region of a real thick device), so a seeded volume costs no
    /// per-block memory until something writes it.
    pub blocks: BTreeMap<u64, Block>,
    /// The async peer-apply queue (P5 plan §2.3): acked writes not
    /// yet applied at the peer, in write order. Runtime kernel-side
    /// state: it dies with `down` (the resource itself).
    pub apply_queue: VecDeque<QueuedApply>,
    /// The monotonic write-acknowledgment counter behind
    /// [`QueuedApply::seq`] (the sequence [`DeviceHandle::write`]
    /// returns).
    pub write_seq: u64,
}

/// One volume's DRBD data-generation identity set — the content
/// `drbdsetup show-gi` reports, i.e. the lineage a witness
/// registration attests. Real DRBD keeps a current UUID, a bitmap
/// base UUID and history UUIDs per volume (`UI_CURRENT`, `UI_BITMAP`,
/// `UI_HISTORY_START..=UI_HISTORY_END` — user/v84/linux/drbd.h:338-346,
/// the only in-tree definition of the enum the v9 build compiles
/// against); freshly created metadata carries only the current UUID
/// (`v08_md_initialize`: current = `UUID_JUST_CREATED`, bitmap = 0,
/// history all 0 — user/shared/drbdmeta.c:2679-2686, v09 equivalent at
/// 2753).
///
/// The fake's values are DETERMINISTIC per resource name. The real
/// kernel generates a random current UUID when it first uses
/// just-created metadata (the kernel special-cases
/// `UUID_JUST_CREATED`, per the comment at drbdmeta.c:1524-1526);
/// that randomization is kernel-side and not verifiable from the
/// drbd-utils tree — `ASSUMPTION(unverified)` — which is exactly why
/// the fake derives stable values instead: adoption verification
/// compares the UUID set recorded at registration time against a
/// later reading, so a re-seeded fake world must reproduce
/// byte-identical output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GiSet {
    /// The current data generation UUID (`UI_CURRENT`).
    pub current_uuid: u64,
    /// The bitmap's base data generation UUID (`UI_BITMAP`); 0 while
    /// no resync is pending (the create-md initialization).
    pub bitmap_uuid: u64,
    /// The history UUIDs (`UI_HISTORY_START..=UI_HISTORY_END`, two
    /// slots); 0 until the current UUID is rotated.
    pub history_uuids: [u64; 2],
}

/// The `UUID_JUST_CREATED` value fresh metadata carries
/// (user/v84/linux/drbd.h:356).
const UUID_JUST_CREATED: u64 = 4;

/// FNV-1a 64 — the fake's deterministic derivation (no cryptographic
/// claim; it only needs to be stable per resource name).
fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl GiSet {
    /// The deterministic identity set of a resource: a name-derived
    /// nonzero current UUID (standing in for the kernel's random
    /// post-handshake UUID — see the type documentation) over the
    /// create-md-initialized bitmap and history (all zero,
    /// drbdmeta.c:2683-2685).
    #[must_use]
    pub fn for_resource(resource: &str) -> Self {
        Self::from_current(fnv1a64(resource.as_bytes()))
    }

    /// A freshly minted identity set for the `generation`-th creation
    /// of `resource` (`generation >= 1`). Real `create-md` generates a
    /// NEW random current UUID every time it initializes metadata
    /// (v08_md_initialize, drbdmeta.c:2679-2686): a recreated
    /// same-named volume must NOT inherit the old lineage — this is
    /// exactly the recreated-volume hole the adopt flow's lineage
    /// comparison exists to close, so the fake models it (the salted
    /// derivation stands in for the kernel's randomness).
    #[must_use]
    pub fn for_resource_generation(resource: &str, generation: u64) -> Self {
        let mut data = resource.as_bytes().to_vec();
        data.extend_from_slice(&generation.to_le_bytes());
        Self::from_current(fnv1a64(&data))
    }

    /// Build a set from the derived current UUID (zero maps to
    /// `UUID_JUST_CREATED`), over create-md-initialized bitmap and
    /// history.
    fn from_current(current: u64) -> Self {
        Self {
            current_uuid: match current {
                0 => UUID_JUST_CREATED,
                value => value,
            },
            bitmap_uuid: 0,
            history_uuids: [0, 0],
        }
    }

    /// The `dt_print_v9_uuids` UUID line (user/v9/drbdtool_common.c:64-87):
    /// `current:bitmap:history:history:` followed by SEVEN local
    /// metadata-flag digits and FIVE peer-flag digits, all
    /// colon-separated. `X64(016)` is `%016lX` — UPPERCASE zero-padded
    /// hex (user/shared/drbd_endian.h:156,163). The flag digits here
    /// are the create-md-initialized metadata flags (MDF_AL_CLEAN
    /// only, drbdmeta.c:2686; digit order per drbdtool_common.c:73-86);
    /// the live kernel-computed flag values are not verifiable from
    /// the userspace sources — `ASSUMPTION(unverified)`: a clean,
    /// freshly created volume reports exactly these digits.
    #[must_use]
    pub fn uuid_line(&self) -> String {
        format!(
            "{:016X}:{:016X}:{:016X}:{:016X}:0:0:0:0:1:0:0:0:0:0:0:0",
            self.current_uuid, self.bitmap_uuid, self.history_uuids[0], self.history_uuids[1]
        )
    }

    /// The full `drbdsetup show-gi` stdout — VERBATIM the
    /// `dt_pretty_print_v9_uuids` shape (user/v9/drbdtool_common.c:89-114):
    /// the ASCII-art UUID header, the `dt_print_v9_uuids` line
    /// (drbdtool_common.c:64-87) and the flag legend.
    #[must_use]
    pub fn show_gi_text(&self) -> String {
        let mut out = String::new();
        out.push('\n');
        out.push_str("       +--<  Current data generation UUID  >-\n");
        out.push_str("       |               +--<  Bitmap's base data generation UUID  >-\n");
        out.push_str("       |               |                 +--<  younger history UUID  >-\n");
        out.push_str("       |               |                 |         +-<  older history  >-\n");
        out.push_str("       V               V                 V         V\n");
        out.push_str(&self.uuid_line());
        out.push('\n');
        out.push_str("                                                                    ^ ^ ^ ^ ^ ^ ^ ^ ^ ^ ^ ^\n");
        out.push_str("                                      -<  Data consistency flag  >--+ | | | | | | | | | | |\n");
        out.push_str("                             -<  Data was/is currently up-to-date  >--+ | | | | | | | | | |\n");
        out.push_str("                                  -<  Node was/is currently primary  >--+ | | | | | | | | |\n");
        out.push_str(" -<  This node was a crashed primary, and has not seen its peer since  >--+ | | | | | | | |\n");
        out.push_str("             -<  The activity-log was applied, the disk can be attached  >--+ | | | | | |\n");
        out.push_str("        -<  The activity-log was disabled, peer is completely out of sync  >--+ | | | | |\n");
        out.push_str("                              -<  This node was primary when it lost quorum  >--+ | | | |\n");
        out.push_str("                                          -<  Node was/is currently connected  >--+ | | |\n");
        out.push_str("                              -<  The peer's disk was out-dated or inconsistent  >--+ | | |\n");
        out.push_str("                                 -<   A fence policy other the dont-care was used  >--+ | |\n");
        out.push_str("                  -<  Node was in the progress of marking all blocks as out of sync  >--+ |\n");
        out.push_str("                     -<  At least once we saw this node with a backing device attached >--+\n");
        out.push('\n');
        out
    }
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
    /// Resources whose `create-md` SUCCEEDED (initialized or
    /// re-initialized metadata). Real drbdmeta re-initializes over an
    /// all-zero LV even when metadata already exists, so this set —
    /// not the absence of a refusal — is the proof the tests assert
    /// the real (non-inverted) create-md semantics.
    pub create_md_ran: BTreeSet<String>,
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
    /// Refcount of kit-opened [`DeviceHandle`]s per minor (P5 plan
    /// §2.1): the handle's participation in the openers/busy model,
    /// composing with [`Self::open_devices`] (the `FakeVmm` device
    /// hooks' set) rather than duplicating it — a minor is busy for
    /// the demote/`down`/status-`open:` checks while EITHER holds it.
    /// A handle increments on open and decrements on drop.
    pub device_openers: BTreeMap<u32, u64>,
    /// Minors whose data path is frozen by the operator `drbdsetup
    /// suspend-io` (the self-fencing data-path freeze; cleared by
    /// `resume-io`). Runtime kernel state, not metadata: it dies with
    /// `down`. A suspended resource carries the verified
    /// `suspended:user` qualifier on its `drbdsetup status` resource
    /// line.
    pub suspended_minors: BTreeSet<u32>,
    /// Minors whose peer has not converged through a suspension's
    /// boundary yet (P4b plan §8 item 3): models asynchronous peer
    /// apply lag — the status answers through the REAL tokens (a
    /// `replication:SyncSource` resync line over a not-`UpToDate`
    /// peer disk), never a side channel, so a convergence proof
    /// (`track_sync`) must observe the tokens. Flip convergence
    /// on/off by inserting/removing the minor.
    pub peer_lagging: BTreeSet<u32>,
    /// The peer's DRBD node id (the generated resource files pin the
    /// local node to `node-id 0` and the peer to `node-id 1`; the
    /// peer-device-context `show-gi` addresses the peer device by it).
    pub peer_node_id: u32,
    /// Resource name → data-generation identity set (what `show-gi`
    /// reports): on-LV metadata content, assigned at create-md,
    /// surviving `down`, dying with `lvremove`.
    pub lineage: BTreeMap<String, GiSet>,
    /// Monotonic counter of lineage sets minted in this world (see
    /// [`GiSet::for_resource_generation`]): every `create-md` (and
    /// first materialization of a seeded resource) mints a fresh
    /// generation, so a recreated same-named volume never inherits the
    /// old identity set.
    pub lineage_salt: u64,
    /// The simulated peer HOST's world, when the rig composes two
    /// worlds into one replication pair (P5 plan §2.1/§2.3 — the
    /// cross-host data path: queue drains and content-copying
    /// resyncs hand blocks to the linked world's same-named
    /// resource). A [`Weak`] reference by design: the link is
    /// bidirectional and must not form an owning cycle. Wired by
    /// [`link_replication_peers`]; `None` (the default) is the
    /// single-world shape, where the peer's map is modeled by the
    /// source-side [`Block::applied_at_peer`] flags alone.
    peer_world: Option<Weak<Mutex<FakeDrbd>>>,
    /// Cross-world replication effects a scripted command queued for
    /// the runner closure to apply after releasing this world's lock
    /// (taking the linked world's lock while holding this one could
    /// deadlock the pair). See [`DeferredResync`].
    deferred_resyncs: Vec<DeferredResync>,
    // -- Fault-injection matrix (one bool per scripted failure) --
    /// `lvcreate` fails.
    pub fail_lvcreate: bool,
    /// `lvextend` fails.
    pub fail_lvextend: bool,
    /// `drbdadm primary` (and `primary --force`) fails.
    pub fail_primary: bool,
    /// `drbdadm primary` (and `primary --force`) fails for exactly
    /// these resource names — the per-volume scalpel next to the
    /// world-wide [`Self::fail_primary`]: a multi-volume handoff faults
    /// one participant's promotion while the others succeed, so a test
    /// can pin the all-or-nothing stall shape (the coordinator must
    /// never half-promote and call it done).
    pub fail_primary_resources: BTreeSet<String>,
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
            create_md_ran: BTreeSet::new(),
            resources: BTreeMap::new(),
            peer_online: true,
            new_peer_disk: DiskState::Inconsistent,
            new_peer_role: Role::Secondary,
            peer_lv_size: None,
            peer_overwritten: false,
            resync_completes: true,
            open_devices: BTreeSet::new(),
            device_openers: BTreeMap::new(),
            suspended_minors: BTreeSet::new(),
            peer_lagging: BTreeSet::new(),
            peer_node_id: 1,
            lineage: BTreeMap::new(),
            lineage_salt: 0,
            peer_world: None,
            deferred_resyncs: Vec::new(),
            fail_lvcreate: false,
            fail_lvextend: false,
            fail_primary: false,
            fail_primary_resources: BTreeSet::new(),
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
    /// A scripted runner wired to this world (closure mode). The
    /// closure releases the world lock before applying any
    /// cross-world replication effects a command queued (the world's
    /// deferred-resync list) — a scripted command never holds this
    /// world's lock while taking the linked peer world's.
    #[must_use]
    pub fn runner(world: &Arc<Mutex<Self>>) -> Arc<FakeRunner> {
        let world = Arc::clone(world);
        Arc::new(FakeRunner::with_closure(move |program, args| {
            let mut guard = world.lock().ok()?;
            let output = script(&mut guard, program, args);
            let deferred = std::mem::take(&mut guard.deferred_resyncs);
            drop(guard);
            for (resource, blocks) in deferred {
                replicate_blocks_to_peer(&world, &resource, &blocks);
            }
            output
        }))
    }

    /// Whether a minor's device is busy (blocks demotion/`down`): the
    /// `FakeVmm` device hooks' [`Self::open_devices`] set OR a live
    /// kit [`DeviceHandle`] ([`Self::device_openers`]) — one busy
    /// model, both opener kinds composing into it (P5 plan §2.1's
    /// openers rule).
    fn device_busy(&self, minor: u32) -> bool {
        self.open_devices.contains(&minor) || self.device_openers.contains_key(&minor)
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
/// grammar of the claimed DRBDADM_VERSION=9.29.0: resource line
/// (indent 0, `role:`, plus the `suspended:` qualifier while any
/// suspension reason is set), ONE device line (indent 2) carrying
/// `disk:` and the UNCONDITIONAL `open:` (kernel >= 9.2.9) —
/// drbdsetup.c's device_status prints both through the
/// column-oriented wrap_printf, so they share a line — a
/// peer-node-named connection line (indent 2), and the peer-device
/// line (indent 4; `replication:` FIRST with `done:` and no `%`
/// suffix while resyncing, per drbdsetup.c peer_device_status),
/// trailing blank line.
fn status_text(world: &FakeDrbd, name: &str, resource: &FakeResource) -> String {
    // drbdsetup.c: `open:` is printed unconditionally on kernel
    // >= 9.2.9, naming whether the device is currently held open.
    // Either opener kind holds it: a VM's device (the openers set)
    // or a kit DeviceHandle (the openers refcount).
    let open = if world.device_busy(resource.minor) {
        "yes"
    } else {
        "no"
    };
    // drbdsetup.c resource_status (3070-3076): the `suspended:`
    // qualifier follows `role:` on the resource line whenever ANY
    // suspension reason is set; susp_str (2523-2550) composes the
    // reasons bit-wise and spells the operator `drbdsetup
    // suspend-io` reason `user` (res_susp, strs[1] at 2526). The
    // kernel-side mapping DRBD_ADM_SUSPEND_IO → the resource-level
    // user-suspension flag is kernel code, not in the drbd-utils
    // tree: ASSUMPTION(unverified) — the vocabulary and the print
    // condition are the verified parts.
    //
    // Wrap budget (user/shared/wrap_printf.c:15-33 — non-tty output
    // wraps past 80 columns): the qualifier appends "
    // suspended:user" (15 columns) after " role:<Role>"
    // (13..15 columns), so names up to 50 columns stay on one line —
    // every standard `vol-<id>-<hash8>` name does; the 63-column
    // maximum name would wrap, in the same fail-closed direction as
    // the P3 `suspended:no-data` analysis
    // (RESOURCE_NAME_SANITIZED_MAX_CHARS, src/provider.rs).
    let suspended = if world.suspended_minors.contains(&resource.minor) {
        " suspended:user"
    } else {
        ""
    };
    if world.peer_online {
        // Peer-apply lag (P4b plan §8 item 3, generalized by the P5
        // plan §2.3 gate-coupling rule): a lagging minor reports a
        // resync in progress over a not-`UpToDate` peer disk — the
        // exact token shape a convergence proof must observe and
        // refuse. Lag is DERIVED, never independently set: the manual
        // `peer_lagging` pin OR a non-drained async apply queue both
        // hold the peer back (the data-bearing tokens the convergence
        // gate reads cannot lie while the queue is open — an empty
        // queue is trivially drained, so a write-free fixture reads
        // the same statuses as before).
        let lagging =
            world.peer_lagging.contains(&resource.minor) || !resource.apply_queue.is_empty();
        let resyncing = resource.resyncing || lagging;
        let peer_disk = if lagging {
            DiskState::Inconsistent
        } else {
            resource.peer_disk.clone()
        };
        // A seeding local (UpToDate) resyncs the fresh peer
        // (Inconsistent) FROM here, so the local replication state is
        // SyncSource — the direction as seen from this node. The
        // resyncing state always pairs with an Inconsistent peer.
        let peer_disk_line = if resyncing {
            format!(
                "replication:SyncSource peer-disk:{} done:37.50",
                disk_str(&peer_disk)
            )
        } else {
            format!("peer-disk:{}", disk_str(&peer_disk))
        };
        format!(
            "{name} role:{role}{suspended}\n  disk:{disk} open:{open}\n  {peer} role:{peer_role}\n    \
             {peer_disk_line}\n\n",
            role = role_str(resource.role),
            disk = disk_str(&resource.local_disk),
            peer = resource.peer_node,
            peer_role = role_str(resource.peer_role),
        )
    } else {
        format!(
            "{name} role:{role}{suspended}\n  disk:{disk} open:{open}\n  {peer} connection:WFConnection\n\n",
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
            // Real drbdmeta semantics (drbdmeta.c): with a non-tty
            // stdin the move prompt auto-declines, then md_initialize
            // RE-INITIALIZES the metadata whenever the data-area start
            // is all-zero — even over a crashed predecessor's existing
            // valid metadata. Only non-zero data at the data-area start
            // is refused ("Operation refused").
            let disk = parsed.disks.first().map(String::as_str)?;
            let lv_key = disk.strip_prefix("/dev/")?;
            if world.lvs.get(lv_key).is_some_and(|lv| lv.has_data) {
                return Some(CommandOutput::failure(format!(
                    "drbdadm create-md {resource}: Operation refused: {disk} seems to contain \
                     non-zero data; this operation would destroy it (exit code 40)"
                )));
            }
            world.metadata.insert(resource.to_owned());
            world.create_md_ran.insert(resource.to_owned());
            // create-md (re-)initializes the on-LV metadata, which
            // includes the data-generation identity set: fresh
            // metadata carries only the current UUID
            // (v08_md_initialize — drbdmeta.c:2679-2686) and a NEW
            // random current UUID is generated per initialization —
            // the fake models that with a fresh generation salt (see
            // [`GiSet::for_resource_generation`]).
            world.lineage_salt += 1;
            world.lineage.insert(
                resource.to_owned(),
                GiSet::for_resource_generation(resource, world.lineage_salt),
            );
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
                if world.device_busy(resource_state.minor) {
                    return Some(CommandOutput::failure(format!(
                        "drbdadm down {resource}: State change failed: (-16) Device or resource \
                         busy: /dev/drbd{} is open by another process",
                        resource_state.minor
                    )));
                }
            }
            // Internal metadata on the LV survives down; the user I/O
            // suspension is runtime kernel state and dies with the
            // device.
            if let Some(state) = world.resources.remove(resource) {
                world.suspended_minors.remove(&state.minor);
            }
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
    // Materialize the on-LV identity set for resources whose metadata
    // was seeded straight into the world: a fresh generation salt, as
    // create-md would mint (the stored set is the single source of
    // truth for the resource from here on).
    world.lineage_salt += 1;
    world
        .lineage
        .entry(resource.to_owned())
        .or_insert_with(|| GiSet::for_resource_generation(resource, world.lineage_salt));
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
            // A fresh resource's data path starts empty (the sparse
            // zero fill; P5 plan §2.1) with an empty apply queue.
            blocks: BTreeMap::new(),
            apply_queue: VecDeque::new(),
            write_seq: 0,
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
    if world.fail_primary || world.fail_primary_resources.contains(resource) {
        return Some(CommandOutput::failure("drbdadm primary: simulated failure"));
    }
    let state = world.resources.get_mut(resource)?;
    let mut resync_copies_content = false;
    if force {
        // The seeding promotion: the forced source becomes UpToDate and
        // the peer is overwritten (resync follows). Seeding makes the
        // backing LV hold the volume's data — from here on create-md
        // over it would be refused (non-zero data area).
        state.local_disk = DiskState::UpToDate;
        if let Some(lv) = world
            .lvs
            .get_mut(&format!("{}/{}", world.vg_name, resource))
        {
            lv.has_data = true;
        }
        if world.peer_online {
            world.peer_overwritten = true;
            if world.resync_completes {
                // The completing resync is content-copying (P5 plan
                // §2.4): payload AND lineage reach the peer, and the
                // apply queue drains with it. The copy runs only when
                // the peer actually needed the resync (a
                // not-`UpToDate` peer disk, the seeding shape) — an
                // already-`UpToDate` peer (the promoted destination's
                // view of its source) keeps its content, exactly as a
                // real promotion over an in-sync pair resyncs nothing.
                resync_copies_content = state.peer_disk != DiskState::UpToDate;
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
    if resync_copies_content {
        resync_to_peer(world, resource);
    }
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
    // The real kernel refusal while the device is open
    // (SS_DEVICE_IN_USE): drbdsetup prints the state-change failure and
    // the kernel's opener info verbatim. Either opener kind triggers
    // it — a VM's device or a kit DeviceHandle (the one busy model).
    let minor = world.resources.get(resource)?.minor;
    if world.device_busy(minor) {
        return Some(CommandOutput::failure(format!(
            "drbd{minor}: State change failed: (-12) Device is held open by someone\n\
             additional info from kernel:\n\
             \x20/dev/drbd{minor} open_cnt:1, writable:1; list of openers follows\n\
             drbd{minor} opened by qemu (pid 1234) at 2026-10-09 12:34:56\n"
        )));
    }
    world.resources.get_mut(resource)?.role = Role::Secondary;
    Some(CommandOutput::success(String::new()))
}

/// `drbdsetup <verb> ...`: `status <resource>` (the verified text
/// grammar for a running resource, or the real "No such resource"
/// failure for a down/unknown one), the minor-context
/// `suspend-io`/`resume-io <minor>` (the self-fencing data-path
/// freeze) and the peer-device-context `show-gi <resource>
/// <peer-node-id> <volume>` (the data-generation identities). Every
/// argv form mirrors the real drbdsetup argument parsing; see the
/// per-command citations.
fn script_drbdsetup(world: &mut FakeDrbd, args: &[&str]) -> Option<CommandOutput> {
    match args.first().copied()? {
        "status" => script_drbdsetup_status(world, args),
        "suspend-io" => Some(script_drbdsetup_suspend_io(world, args, true)),
        "resume-io" => Some(script_drbdsetup_suspend_io(world, args, false)),
        "show-gi" => Some(script_drbdsetup_show_gi(world, args)),
        _ => None,
    }
}

/// `drbdsetup status <resource>`: the verified text grammar for a
/// running resource, or the real "No such resource" failure for a
/// down/unknown one.
fn script_drbdsetup_status(world: &mut FakeDrbd, args: &[&str]) -> Option<CommandOutput> {
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

/// `dt_minor_of_dev` (user/shared/shared_tool.c:631-674): only a bare
/// decimal or `/dev/drbd<decimal>` resolves to a minor. The third
/// real branch — an existing block-device node whose major is the
/// DRBD major — cannot exist in the simulated world and is not
/// modeled. Everything else, including a bare RESOURCE NAME (resource
/// names may contain digits, and interpreting those would be
/// dangerous — the comment at shared_tool.c:638-650), is
/// unresolvable.
fn minor_of_spec(spec: &str) -> Option<u32> {
    let digits = spec.strip_prefix("/dev/drbd").unwrap_or(spec);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `drbdsetup suspend-io|resume-io <minor>` — the CTX_MINOR commands
/// (user/v9/drbdsetup.c:363-366, `DRBD_ADM_SUSPEND_IO`/
/// `DRBD_ADM_RESUME_IO`, no payload). The single context argument is
/// resolved through `dt_minor_of_dev` (drbdsetup.c:4695-4703 →
/// shared_tool.c:631-674): a bare decimal or `/dev/drbd<minor>`;
/// anything else — a bare resource name, `all` (refused outright for
/// a CTX_MINOR-only command, drbdsetup.c:4691-4693) — fails with
/// "Cannot determine minor device number of device '<arg>'"
/// (drbdsetup.c:4697-4701, exit 20).
///
/// A minor with no running resource answers through the netlink
/// error surface of `_generic_config_cmd` → `check_error`:
/// "<obj>: Failure: (<code>) <message>" (drbdsetup.c:988-991, exit
/// 10) with ERR_MINOR_INVALID = 127 "Device minor not allocated"
/// (drbdsetup.c:512, code from user/v84/linux/drbd.h:137).
/// ASSUMPTION(unverified): which code the KERNEL picks for an
/// unknown minor (127 vs ERR_RES_NOT_KNOWN = 158, "Unknown resource",
/// drbdsetup.c:547) is kernel-side and not in the drbd-utils tree;
/// 127 is modeled because a CTX_MINOR request fails the minor lookup
/// first.
///
/// Idempotency: `check_error` ignores SS_NOTHING_TO_DO-class replies
/// (drbdsetup.c:996-997), so a double suspend and a resume of a
/// non-suspended device both succeed. ASSUMPTION(unverified): that
/// the kernel actually classifies the no-op case there — kernel-side.
fn script_drbdsetup_suspend_io(
    world: &mut FakeDrbd,
    args: &[&str],
    suspend: bool,
) -> CommandOutput {
    // drbdsetup's context loop demands exactly one argument per
    // context key (drbdsetup.c:4675-4743): a missing argument is
    // "Missing argument <n> to command" (4683-4686, exit 20), excess
    // arguments are "Excess arguments: ..." (1150-1156 →
    // warn_print_excess_args, 1022-1028).
    if args.len() < 2 {
        return CommandOutput::failure("Missing argument 2 to command\n");
    }
    if args.len() > 2 {
        return CommandOutput::failure(format!("Excess arguments: {}", args[2..].join(" ")));
    }
    let spec = args[1];
    if spec == "all" {
        return CommandOutput::failure("command does not accept argument 'all'");
    }
    let Some(minor) = minor_of_spec(spec) else {
        return CommandOutput::failure(format!(
            "Cannot determine minor device number of device '{spec}'"
        ));
    };
    if !world
        .resources
        .iter()
        .any(|(_, state)| state.minor == minor)
    {
        return CommandOutput::failure(format!(
            "{spec}: Failure: (127) Device minor not allocated"
        ));
    }
    if suspend {
        world.suspended_minors.insert(minor);
        // The post-barrier drain (P5 plan §2.3/§2.4): suspending the
        // data path is the barrier's boundary, and the fake's
        // content-copying resync — the SYSTEM path, never the campaign
        // — is what closes the peer-apply window behind it: the peer
        // applies the outstanding queue, exactly as a real peer keeps
        // applying in-flight replication after the source freezes.
        // Gated on the link state: over a partition nothing drains,
        // and the convergence gate must (and does) keep refusing on
        // the not-established connection. The campaign's
        // `apply_peer_writes` remains the ONLY way to shape how much
        // applied BEFORE this boundary (the pre-quiesce lag).
        if world.peer_online {
            if let Some((resource, _)) = world
                .resources
                .iter()
                .find(|(_, state)| state.minor == minor)
            {
                let resource = resource.clone();
                resync_to_peer(world, &resource);
            }
        }
    } else {
        world.suspended_minors.remove(&minor);
    }
    CommandOutput::success(String::new())
}

/// `drbdsetup show-gi <resource> <peer_node_id> <volume>` — the
/// CTX_PEER_DEVICE form (user/v9/drbdsetup.c:388, lockless): the
/// context is resource + peer node id + volume, each a mandatory
/// positional argument (user/v9/drbdsetup.h:53-54; peer node id and
/// volume parse as numbers, drbdsetup.c:4736-4739 → m_strtoll,
/// shared_tool.c:532-547).
///
/// `show_or_get_gi_cmd` (drbdsetup.c:4146-4203) walks the KERNEL's
/// peer devices, so a resource that is not up answers
/// "<resource>: No such peer device" (4164-4165, exit 10) — same for
/// a peer-node-id/volume that matches no peer device
/// (peer_device_ctx_match, 4138-4144). An up resource whose local
/// disk is detached answers "Device has no disk" (4179-4185, exit 1;
/// the preceding "Device is unconfigured" branch at 4173-4177 needs
/// an L_OFF peer connection the world does not model). A match prints
/// the identity set through `dt_pretty_print_v9_uuids` (4196-4198 →
/// drbdtool_common.c:89-114). Exit codes are modeled only through
/// the success flag (the runner does not expose them).
fn script_drbdsetup_show_gi(world: &mut FakeDrbd, args: &[&str]) -> CommandOutput {
    if args.len() < 4 {
        return CommandOutput::failure(format!("Missing argument {} to command\n", args.len() + 1));
    }
    if args.len() > 4 {
        return CommandOutput::failure(format!("Excess arguments: {}", args[4..].join(" ")));
    }
    let resource = args[1];
    // m_strtoll (shared_tool.c:532-547): a non-numeric context
    // argument is "<arg> is not a valid number" (exit 20).
    let Ok(peer_node_id) = args[2].parse::<u32>() else {
        return CommandOutput::failure(format!("{} is not a valid number", args[2]));
    };
    let Ok(volume) = args[3].parse::<u32>() else {
        return CommandOutput::failure(format!("{} is not a valid number", args[3]));
    };
    let Some(state) = world.resources.get(resource) else {
        return CommandOutput::failure(format!("{resource}: No such peer device"));
    };
    // peer_device_ctx_match (drbdsetup.c:4138-4144) compares resource
    // name, peer node id AND volume. The generated resource files pin
    // the peer to node-id 1 and every volvisor resource is
    // single-volume (volume 0).
    if peer_node_id != world.peer_node_id || volume != 0 {
        return CommandOutput::failure(format!("{resource}: No such peer device"));
    }
    if state.local_disk == DiskState::Diskless {
        return CommandOutput::failure("Device has no disk\n");
    }
    // The identity set is on-LV metadata: assigned at create-md (see
    // script_drbdadm), materialized deterministically for resources
    // seeded straight into the world, stable across down/up.
    let gi = world
        .lineage
        .entry(resource.to_owned())
        .or_insert_with(|| {
            world.lineage_salt += 1;
            GiSet::for_resource_generation(resource, world.lineage_salt)
        })
        .clone();
    CommandOutput::success(gi.show_gi_text())
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
            // A freshly created LV is all-zero (drbdmeta's
            // re-initialize-vs-refuse decision rests on this).
            has_data: false,
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
        // The identity set lives in the LV's internal metadata: it
        // dies with the LV.
        world.lineage.remove(lv_name);
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
// The fake data path (P5 plan §2)
// ---------------------------------------------------------------------------

/// One deferred cross-world resync effect: the resource and the full
/// block map a scripted command copied toward the linked peer world.
/// Queued by [`resync_to_peer`] while the runner closure holds this
/// world's lock; the closure applies it (through
/// [`replicate_blocks_to_peer`]) only after releasing the lock, so no
/// scripted command ever holds both worlds' locks at once — the
/// replication link is bidirectional, and nested locks across the
/// pair could deadlock.
type DeferredResync = (String, Vec<(u64, Block)>);

/// Lock the world, mapping poisoning to a typed `INTERNAL` error (the
/// fake VMM's lock discipline — never a panic).
fn world_lock(world: &Arc<Mutex<FakeDrbd>>) -> Result<MutexGuard<'_, FakeDrbd>, ApiError> {
    world.lock().map_err(|_| {
        ApiError::new(
            ApiErrorCode::Internal,
            "fake DRBD world lock poisoned by a previous failure",
        )
    })
}

/// The fake's content-copying resync (P5 plan §2.4): every local block
/// — payload AND lineage — is copied to the peer and the apply queue
/// is drained with it. This is the SYSTEM path that closes the
/// peer-apply window (the post-barrier drain at `suspend-io`, the
/// completing seeding resync); the campaign never calls it — its only
/// queue control is [`apply_peer_writes`], the pre-quiesce lag shaper.
///
/// The copy carries EVERY source block, including [`write_raw`]'s
/// out-of-band writes (which never enter the queue): real DRBD's
/// bitmap marks a rogue writer's blocks dirty and the resync copies
/// them faithfully — divergence propagates, and the witness's
/// classifier is what must catch it, not the transport (the §5.1
/// stale-write injection depends on exactly this). Deliberate, not
/// an oversight.
///
/// The copy to a LINKED peer world cannot run here (the caller may
/// hold this world's lock — see [`DeferredResync`]), so it is queued
/// for the runner closure; the single-world effects (flags flipped,
/// queue drained) are immediate, keeping the resource consistent with
/// its own status.
fn resync_to_peer(world: &mut FakeDrbd, resource: &str) {
    let Some(state) = world.resources.get_mut(resource) else {
        return;
    };
    for block in state.blocks.values_mut() {
        block.applied_at_peer = true;
    }
    state.apply_queue.clear();
    let blocks: Vec<(u64, Block)> = state
        .blocks
        .iter()
        .map(|(index, block)| (*index, block.clone()))
        .collect();
    if blocks.is_empty() {
        return;
    }
    world.deferred_resyncs.push((resource.to_owned(), blocks));
}

/// Hand a resync's copied blocks to the linked peer world's
/// same-named resource. The caller must hold NO lock on `world` (see
/// [`DeferredResync`]); a missing link (the single-world shape) or a
/// peer world without the resource is a silent no-op — the copy is a
/// replication effect, and there is nothing to replicate to.
fn replicate_blocks_to_peer(world: &Arc<Mutex<FakeDrbd>>, resource: &str, blocks: &[(u64, Block)]) {
    let Some(peer) = world_lock(world)
        .ok()
        .and_then(|world| world.peer_world.as_ref().and_then(Weak::upgrade))
    else {
        return;
    };
    let Ok(mut peer_world) = peer.lock() else {
        return;
    };
    let Some(peer_resource) = peer_world.resources.get_mut(resource) else {
        return;
    };
    for (index, block) in blocks {
        peer_resource.blocks.insert(
            *index,
            Block {
                payload: block.payload,
                lineage: block.lineage.clone(),
                // The flag is the WRITING side's bookkeeping (see
                // [`Block`]); a block replicated in lands false — its
                // window is the sender's map's concern, and presence
                // in this map is itself the ground truth.
                applied_at_peer: false,
            },
        );
    }
}

/// Hand drained queue entries to the peer (P5 plan §2.3): first the
/// linked peer world's map — with NO lock on this world held, the
/// same no-nested-locks discipline as [`replicate_blocks_to_peer`] —
/// then the source-side flags, only for entries whose content is
/// still current (a newer write to the same block superseded the
/// entry; the newer queue entry carries that content).
///
/// Returns whether the entries were **delivered**. The gate-coupling
/// hinge (§2.3) makes this load-bearing: a drain that silently
/// dropped entries — the linked peer alive but its same-named
/// resource absent — would flip the source-side flags and empty the
/// queue while the blocks never landed, and the gate would read
/// `UpToDate` over un-applied blocks, exactly the lie §2.3 forbids.
/// A `false` return tells the caller to put the entries back: the
/// queue stays open and the gate stays closed. A world with **no
/// peer link at all** (the single-world shape) delivers by
/// definition — there is no peer to lie to, and the queue is
/// source-side bookkeeping only.
fn apply_at_peer(world: &Arc<Mutex<FakeDrbd>>, resource: &str, entries: &[QueuedApply]) -> bool {
    if entries.is_empty() {
        return true;
    }
    // The linked peer, if one exists. The single-world shape (no
    // link at all) delivers by definition — there is no peer to lie
    // to, and the queue is source-side bookkeeping only — so it
    // falls through to the bookkeeping below rather than returning
    // early (the flags still flip: the drain is real bookkeeping).
    // A POISONED source-world lock also has no link to read, but it
    // is NOT the single-world shape — it is an unreadable world, and
    // an unreadable world fails closed: the drain reports failure so
    // the entries requeue rather than the gate reading `UpToDate`
    // over blocks whose delivery state is unknown (round-2 N2).
    let peer = match world_lock(world) {
        Ok(world) => world.peer_world.as_ref().and_then(Weak::upgrade),
        Err(_) => return false,
    };
    if let Some(peer) = peer {
        let Ok(mut peer_world) = peer.lock() else {
            return false;
        };
        let Some(peer_resource) = peer_world.resources.get_mut(resource) else {
            return false;
        };
        for entry in entries {
            peer_resource.blocks.insert(
                entry.block,
                Block {
                    payload: entry.payload,
                    lineage: entry.lineage.clone(),
                    applied_at_peer: false,
                },
            );
        }
    }
    if let Ok(mut world) = world.lock() {
        if let Some(state) = world.resources.get_mut(resource) {
            for entry in entries {
                if let Some(block) = state.blocks.get_mut(&entry.block) {
                    if block.payload == entry.payload && block.lineage == entry.lineage {
                        block.applied_at_peer = true;
                    }
                }
            }
        }
    }
    true
}

/// Compose two worlds into one replication pair (P5 plan §2.1/§2.3):
/// each world's linked peer becomes the other, so queue drains and
/// content-copying resyncs on either side hand blocks to the other's
/// same-named resource. The links are [`Weak`] — bidirectional strong
/// references would form an owning cycle — so the rig must keep both
/// worlds alive for the scenario's lifetime (every harness does: the
/// worlds live in the rig structs).
pub fn link_replication_peers(a: &Arc<Mutex<FakeDrbd>>, b: &Arc<Mutex<FakeDrbd>>) {
    a.lock().expect("world").peer_world = Some(Arc::downgrade(b));
    b.lock().expect("world").peer_world = Some(Arc::downgrade(a));
}

/// An open handle on one running resource's device (P5 plan §2.1) —
/// the fake's ENFORCED data path, the surface both the write-trace
/// oracle and a guest's I/O model run on:
///
/// - opening requires the Primary role (a Secondary's device fails
///   typed, E_ROFS-shaped — the single-writer invariant the real
///   stack enforces at the device);
/// - writes refuse while the minor is suspended (the quiesce is real
///   at the data path, not just a flag; the real kernel BLOCKS a
///   suspended write — the kit refuses typed instead, so a frozen
///   writer fails loudly and deterministically rather than hanging
///   the scenario);
/// - the handle participates in the openers/busy model exactly as the
///   `FakeVmm` device hooks do: while it lives, the demote (and
///   `down`) refuse with the held-open error and the status `open:`
///   line reads `yes` (rule 17 — a held device blocks the demote);
/// - a write's return is the SOURCE-SIDE ack (Protocol C-shaped up to
///   the source's own map, rule 16): the bytes land in the source's
///   block map AND in the async peer-apply queue, and the returned
///   sequence number is the write's identity in that queue.
///
/// Reads work on any role (a Secondary's device is readable) and
/// through a suspension — abort-path verification reads the source
/// device after the barrier, and post-mortem reads below enforcement
/// use [`read_raw`] instead.
pub struct DeviceHandle {
    /// The world the device lives in.
    world: Arc<Mutex<FakeDrbd>>,
    /// The device's minor (`/dev/drbd<minor>`).
    minor: u32,
}

impl Drop for DeviceHandle {
    fn drop(&mut self) {
        // Release the opener: decrement the refcount and forget the
        // minor at zero. A poisoned lock leaves the count alone — the
        // world is unusable anyway.
        if let Ok(mut world) = self.world.lock() {
            if let Some(count) = world.device_openers.get_mut(&self.minor) {
                *count -= 1;
                if *count == 0 {
                    world.device_openers.remove(&self.minor);
                }
            }
        }
    }
}

impl DeviceHandle {
    /// The device's minor.
    #[must_use]
    pub fn minor(&self) -> u32 {
        self.minor
    }

    /// Write one logical block (exactly [`BLOCK_SIZE`] bytes) and
    /// return the write's acknowledgment sequence — the source-side
    /// ack, the identity [`apply_peer_writes`] drains by. The write
    /// lands in the source's block map and in the async peer-apply
    /// queue; it reaches the peer's map only when the queue drains
    /// (P5 plan §2.3's window: the acknowledged set may exceed the
    /// peer-applied set, and the status tokens say so).
    ///
    /// # Errors
    /// [`ApiErrorCode::NotFound`] when the resource is gone; `INVALID_STATE`
    /// when the resource is no longer Primary (E_ROFS-shaped — the
    /// role flipped under the handle) or the minor is suspended;
    /// `INVALID_REQUEST` for a wrong-size payload or a block beyond
    /// the device.
    pub fn write(&self, block: u64, payload: &[u8]) -> Result<u64, ApiError> {
        let mut world = world_lock(&self.world)?;
        // The name is cloned out so the mutable map access below is
        // not borrow-tied to the lookups above (the guard's Deref
        // borrows the whole world, not per-field).
        let name = world
            .resources
            .iter()
            .find(|(_, state)| state.minor == self.minor)
            .map(|(name, _)| name.clone())
            .ok_or_else(|| ApiError::not_found(format!("drbd{}: No such resource", self.minor)))?;
        let state = &world.resources[&name];
        if state.role != Role::Primary {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "write(/dev/drbd{}): (-30) Read-only file system — the resource is no \
                     longer Primary; the fake enforces the single-writer invariant at the \
                     device",
                    self.minor
                ),
            ));
        }
        if world.suspended_minors.contains(&self.minor) {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "write(/dev/drbd{}): the data path is suspended (suspended:user) — the \
                     quiesce is real at the data path, not just a flag",
                    self.minor
                ),
            ));
        }
        if payload.len() != BLOCK_SIZE {
            return Err(ApiError::new(
                ApiErrorCode::InvalidRequest,
                format!(
                    "write(/dev/drbd{}): payload is {} bytes; exactly {BLOCK_SIZE} (one \
                     logical block) is required",
                    self.minor,
                    payload.len()
                ),
            ));
        }
        let block_count = state.device_size / BLOCK_SIZE as u64;
        if block >= block_count {
            return Err(ApiError::new(
                ApiErrorCode::InvalidRequest,
                format!(
                    "write(/dev/drbd{}): block {block} is beyond the device ({block_count} \
                     logical blocks)",
                    self.minor
                ),
            ));
        }
        let lineage = world
            .lineage
            .get(&name)
            .cloned()
            .unwrap_or_else(|| GiSet::for_resource(&name));
        let mut bytes = [0_u8; BLOCK_SIZE];
        bytes.copy_from_slice(payload);
        let state = world
            .resources
            .get_mut(&name)
            .expect("the resource was found above");
        state.write_seq += 1;
        let seq = state.write_seq;
        state.blocks.insert(
            block,
            Block {
                payload: bytes,
                lineage: lineage.clone(),
                applied_at_peer: false,
            },
        );
        state.apply_queue.push_back(QueuedApply {
            seq,
            block,
            payload: bytes,
            lineage,
        });
        Ok(seq)
    }

    /// Read one logical block from the resource's local map. An
    /// absent block is the sparse zero fill: a zeroed payload under
    /// the resource's current lineage, trivially in sync
    /// (`applied_at_peer: true` — an unwritten region has no
    /// peer-apply window).
    ///
    /// # Errors
    /// [`ApiErrorCode::NotFound`] when the resource is gone;
    /// `INVALID_REQUEST` for a block beyond the device.
    pub fn read(&self, block: u64) -> Result<Block, ApiError> {
        let world = world_lock(&self.world)?;
        let name = world
            .resources
            .iter()
            .find(|(_, state)| state.minor == self.minor)
            .map(|(name, _)| name.clone())
            .ok_or_else(|| ApiError::not_found(format!("drbd{}: No such resource", self.minor)))?;
        let state = &world.resources[&name];
        let block_count = state.device_size / BLOCK_SIZE as u64;
        if block >= block_count {
            return Err(ApiError::new(
                ApiErrorCode::InvalidRequest,
                format!(
                    "read(/dev/drbd{}): block {block} is beyond the device ({block_count} \
                     logical blocks)",
                    self.minor
                ),
            ));
        }
        if let Some(block_content) = state.blocks.get(&block) {
            return Ok(block_content.clone());
        }
        let lineage = world
            .lineage
            .get(&name)
            .cloned()
            .unwrap_or_else(|| GiSet::for_resource(&name));
        Ok(Block {
            payload: [0_u8; BLOCK_SIZE],
            lineage,
            applied_at_peer: true,
        })
    }
}

/// Open one running resource's device (P5 plan §2.1): the enforced
/// path — role-checked at open, suspension-checked at write,
/// openers-tracked for the demote/busy model (see [`DeviceHandle`]).
///
/// # Errors
/// [`ApiErrorCode::NotFound`] when no running resource holds the
/// minor; `INVALID_STATE` (E_ROFS-shaped) when the resource is not
/// Primary — a Secondary's device is read-only, and the fake enforces
/// the single-writer invariant the real stack enforces.
pub fn open_device(world: &Arc<Mutex<FakeDrbd>>, minor: u32) -> Result<DeviceHandle, ApiError> {
    let mut guard = world_lock(world)?;
    let role = guard
        .resources
        .values()
        .find(|state| state.minor == minor)
        .map(|state| state.role);
    let Some(role) = role else {
        return Err(ApiError::not_found(format!(
            "drbd{minor}: No such resource"
        )));
    };
    if role != Role::Primary {
        return Err(ApiError::new(
            ApiErrorCode::InvalidState,
            format!(
                "open(/dev/drbd{minor}): (-30) Read-only file system — the resource is \
                 Secondary; writes land only on the Primary (the single-writer invariant, \
                 enforced at the device)"
            ),
        ));
    }
    *guard.device_openers.entry(minor).or_insert(0) += 1;
    Ok(DeviceHandle {
        world: Arc::clone(world),
        minor,
    })
}

/// Apply the queued peer writes of `minor` up to (including)
/// acknowledgment sequence `up_to` (P5 plan §2.3) — the CAMPAIGN's
/// pre-quiesce lag control, and deliberately its only queue control:
/// shaping how much of the acknowledged tail the peer has applied
/// before the boundary. The post-barrier drain is the fake's
/// content-copying resync (the system path, the resync the
/// `suspend-io` barrier and a completing seeding promotion run),
/// never this function.
///
/// Entries apply in queue order; a linked peer world's same-named
/// resource receives their content, and the source-side blocks are
/// marked applied. Draining the queue fully is what lets the
/// resource read `peer-disk:UpToDate` again — the status tokens are
/// derived from this state (the gate-coupling rule).
///
/// # Errors
/// [`ApiErrorCode::NotFound`] when no running resource holds the
/// minor.
///
/// The campaign's pre-quiesce lag shaper (P5 plan §2.3) and the
/// transport's drain primitive: apply every queued write whose
/// acknowledgment sequence is at most `up_to` (a `u64::MAX` bound
/// drains everything). A refused delivery requeues — the delivery
/// rules and the single-world shapes are documented on the private
/// `apply_at_peer` helper.
///
/// # Concurrency shape (round-2 N1, the single-drainer invariant)
///
/// The ordering guarantees — front-first requeue, same-block entries
/// applied oldest-first — hold under **one drainer per world at a
/// time** (the spawned transport thread, or a single-threaded test
/// body). Two concurrent drainers could interleave a failed drain's
/// requeue with a successful drain of a newer same-block entry. That
/// is the invariant's limit, stated here deliberately: stage C adds
/// a second drainer only together with a serialized drain (one world
/// lock critical section around pop-and-apply, or a drain mutex).
pub fn apply_peer_writes(
    world: &Arc<Mutex<FakeDrbd>>,
    minor: u32,
    up_to: u64,
) -> Result<(), ApiError> {
    let (resource, entries) = {
        let mut guard = world_lock(world)?;
        let name = guard
            .resources
            .iter()
            .find(|(_, state)| state.minor == minor)
            .map(|(name, _)| name.clone())
            .ok_or_else(|| ApiError::not_found(format!("drbd{minor}: No such resource")))?;
        let state = guard
            .resources
            .get_mut(&name)
            .expect("the resource was found above");
        let mut entries = Vec::new();
        while state
            .apply_queue
            .front()
            .is_some_and(|entry| entry.seq <= up_to)
        {
            if let Some(entry) = state.apply_queue.pop_front() {
                entries.push(entry);
            }
        }
        (name, entries)
    };
    if !apply_at_peer(world, &resource, &entries) {
        // The delivery failed (the linked peer's resource is absent —
        // §2.3's hinge): put the entries back, front-first in the
        // original order, so the queue stays open and the gate stays
        // closed. The drain is retried when the peer can receive.
        let mut guard = world_lock(world)?;
        let state = guard
            .resources
            .get_mut(&resource)
            .expect("the resource was found above");
        for entry in entries.into_iter().rev() {
            state.apply_queue.push_front(entry);
        }
    }
    Ok(())
}

/// The fake's steady-state protocol-A transport (P5 plan §2.1): a
/// background thread that drains `minor`'s peer-apply queue with a
/// real-time `lag`, modeling the asynchronous peer apply the real
/// stack performs continuously — "asynchronous peer apply is real
/// time" ([`volvisor_drbd`]'s convergence gate retries the typed
/// `REPLICA_NOT_DURABLE` refusal while the peer lags). Without it a
/// live writer would hold the queue non-empty forever and even a
/// happy-path migration could never observe convergence; with it the
/// peer-apply window is genuinely open for at most `lag` after every
/// write (a real, nonzero tail mid-flight) and closes on its own.
///
/// This is a DATA-PATH component of the fake (the replication link
/// itself — it lives below the daemons and survives their kills,
/// exactly as a real link would), not a campaign injection: the
/// post-barrier drain at `suspend-io` remains the fake's only
/// SYSTEM-path window closer (`resync_to_peer`), and stopping the
/// transport is how a rig models the link's steady state ending
/// (a frozen tail for lag shaping). It races nothing: queue drains
/// are idempotent and the world lock is never held across a drain.
///
/// The thread is detached on drop (it exits within one `lag`); call
/// [`PeerTransport::join`] to observe the exit deterministically.
pub fn spawn_peer_transport(
    world: &Arc<Mutex<FakeDrbd>>,
    minor: u32,
    lag: Duration,
) -> PeerTransport {
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let world = Arc::clone(world);
        let stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name(format!("peer-transport-drbd{minor}"))
            .spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(lag);
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    // A partition (the world's `peer_online` flag, P5
                    // plan §5.5) stops the drain — the transport does
                    // not deliver over a down link; the queue stays
                    // open and the gate stays closed until the
                    // partition heals. Stopping the transport entirely
                    // is the OTHER way to end the steady state (a
                    // removed link); the flag is the in-place
                    // partition. The check and the drain are
                    // deliberately NOT atomic: a flip between them
                    // lets one in-flight drain deliver over a link
                    // that just dropped — real DRBD semantics (in-
                    // flight writes complete when the link drops),
                    // recorded in round-2 N3.
                    let online = world_lock(&world).is_ok_and(|world| world.peer_online);
                    if !online {
                        continue;
                    }
                    // A `u64::MAX` bound drains everything queued —
                    // the transport's lag IS the bound.
                    let _ = apply_peer_writes(&world, minor, u64::MAX);
                }
            })
            .expect("spawn peer transport")
    };
    PeerTransport {
        stop,
        thread: Some(thread),
    }
}

/// One running [`spawn_peer_transport`] link (see its docs): stop it
/// to freeze the peer-apply window (the queue stops draining), join
/// it to observe the exit.
pub struct PeerTransport {
    /// The stop flag the thread polls between lags.
    stop: Arc<AtomicBool>,
    /// The transport thread; `None` once joined.
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PeerTransport {
    /// Ask the link to stop (it exits within one lag); idempotent.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Stop and wait for the thread's exit — after this the queue is
    /// definitively frozen (no in-flight drain can land afterwards).
    pub fn join(mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PeerTransport {
    fn drop(&mut self) {
        // Detach: the thread exits within one lag on its own.
        self.stop();
    }
}

/// Post-mortem read of one logical block (P5 plan §2.1's escape
/// hatch): the resource's local map, with NONE of the enforced path's
/// rules — no role check, no suspension check, and the openers set is
/// never touched. **TEST-ONLY injection surface (the `FakeFailKnobs`
/// doc-gate precedent): assertion and driving never use it** — it
/// exists so an oracle can read the bytes of a fenced, demoted or
/// suspended resource (bytes are the ground truth) and nothing else.
///
/// # Errors
/// [`ApiErrorCode::NotFound`] when no running resource holds the
/// minor (a `down`ed resource's block map died with the device — the
/// fake models no on-LV block persistence, the same boundary as
/// `down` removing the resource).
pub fn read_raw(world: &Arc<Mutex<FakeDrbd>>, minor: u32, block: u64) -> Result<Block, ApiError> {
    let world = world_lock(world)?;
    let name = world
        .resources
        .iter()
        .find(|(_, state)| state.minor == minor)
        .map(|(name, _)| name.clone())
        .ok_or_else(|| ApiError::not_found(format!("drbd{minor}: No such resource")))?;
    let state = &world.resources[&name];
    if let Some(block_content) = state.blocks.get(&block) {
        return Ok(block_content.clone());
    }
    let lineage = world
        .lineage
        .get(&name)
        .cloned()
        .unwrap_or_else(|| GiSet::for_resource(&name));
    Ok(Block {
        payload: [0_u8; BLOCK_SIZE],
        lineage,
        applied_at_peer: true,
    })
}

/// Out-of-band write of one logical block (P5 plan §2.4's injection
/// surface): the resource's local map, with NONE of the enforced
/// path's rules — no role check, no suspension check, no bounds
/// check, the openers set never touched, and (the point) NO apply
/// queue entry: a rogue writer below volvisor's enforcement is below
/// the replication path too, so the peer never receives these bytes.
/// **TEST-ONLY injection surface (the `FakeFailKnobs` doc-gate
/// precedent): driving and assertion never use it** — it models an
/// actor volvisor cannot see (the stale-write-after-fence injection)
/// and nothing else.
///
/// # Errors
/// [`ApiErrorCode::NotFound`] when no running resource holds the
/// minor; `INVALID_REQUEST` for a payload that is not exactly
/// [`BLOCK_SIZE`] bytes.
pub fn write_raw(
    world: &Arc<Mutex<FakeDrbd>>,
    minor: u32,
    block: u64,
    payload: &[u8],
) -> Result<(), ApiError> {
    if payload.len() != BLOCK_SIZE {
        return Err(ApiError::new(
            ApiErrorCode::InvalidRequest,
            format!(
                "write_raw(drbd{minor}): payload is {} bytes; exactly {BLOCK_SIZE} (one \
                 logical block) is required",
                payload.len()
            ),
        ));
    }
    let mut world = world_lock(world)?;
    let name = world
        .resources
        .iter()
        .find(|(_, state)| state.minor == minor)
        .map(|(name, _)| name.clone())
        .ok_or_else(|| ApiError::not_found(format!("drbd{minor}: No such resource")))?;
    let mut bytes = [0_u8; BLOCK_SIZE];
    bytes.copy_from_slice(payload);
    let lineage = world
        .lineage
        .get(&name)
        .cloned()
        .unwrap_or_else(|| GiSet::for_resource(&name));
    world
        .resources
        .get_mut(&name)
        .expect("the resource was found above")
        .blocks
        .insert(
            block,
            Block {
                payload: bytes,
                lineage,
                applied_at_peer: false,
            },
        );
    Ok(())
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

/// Like [`provider_from`], but witness-managed (P4a): the provider
/// takes the writer-authority context and its startup reconciliation
/// fail-closes on primaries it cannot prove.
pub fn provider_from_with_authority(
    state_path: &Path,
    world: &Arc<Mutex<FakeDrbd>>,
    authority: AuthorityContext,
) -> Arc<DrbdProvider> {
    let runner = FakeDrbd::runner(world);
    let base = state_path
        .parent()
        .expect("state path has a parent directory");
    DrbdProvider::with_authority(
        runner,
        config_for(base),
        state_path.to_path_buf(),
        authority,
    )
    .map(Arc::new)
    .expect("provider construction")
}

/// The fixture configuration seen from the OTHER host (the P3 peer):
/// node, peer and addresses swapped (the seeded definitions pin the
/// original node's replication port to `SEED_PORT`). Adoption tests
/// run on this view after flipping the world's node name (see
/// [`flip_world_to_peer`]).
#[must_use]
pub fn config_for_peer(base: &Path) -> DrbdProviderConfig {
    let (peer_ip, _peer_port) = peer_endpoint();
    DrbdProviderConfig {
        node_name: PEER_NODE.to_owned(),
        peer_name: NODE.to_owned(),
        local_address: peer_ip,
        peer_address: format!("{LOCAL_ADDR}:{SEED_PORT}"),
        ..config_for(base)
    }
}

/// Flip the simulated world to the peer host's view: `uname -n`
/// answers the peer name and the res-file peer device of a resource
/// is the original node (node-id 0 — the generated definitions pin
/// the local node to 0 and the peer to 1).
pub fn flip_world_to_peer(world: &Arc<Mutex<FakeDrbd>>) {
    let mut world = world.lock().expect("world");
    world.node_name.clear();
    world.node_name.push_str(PEER_NODE);
    world.peer_node_id = 0;
}

/// Pin or clear asynchronous peer-apply lag on a minor (see
/// [`FakeDrbd::peer_lagging`]): while pinned, the resource's status
/// reports a resync in progress over a not-`UpToDate` peer disk — the
/// real token shape `track_sync` must observe and refuse until the
/// lag clears.
pub fn set_peer_lag(world: &Arc<Mutex<FakeDrbd>>, minor: u32, lagging: bool) {
    let mut world = world.lock().expect("world");
    if lagging {
        world.peer_lagging.insert(minor);
    } else {
        world.peer_lagging.remove(&minor);
    }
}

/// Seed a running resource, its res file and its backing LV into the
/// host directory and world WITHOUT any state entry — the shape the
/// surviving (peer) host sees for a volume whose primary died (the
/// P4a plan §5 adopt-and-promote fixture). `tagged` selects the
/// volvisor-created branch (a `volvisor.owner`-tagged LV) versus the
/// operator-provisioned peer side (no tag).
pub fn seed_foreign_volume(
    base: &Path,
    world: &Arc<Mutex<FakeDrbd>>,
    volume_id: &str,
    size_bytes: u64,
    protocol: ReplicationMode,
    tagged: bool,
) -> String {
    let volume_id = VolumeId::new(volume_id).expect("valid volume id");
    let resource = resource_name_for(&volume_id);
    let (peer_ip, peer_port) = peer_endpoint();
    ResourceDefinition {
        resource_name: resource.clone(),
        minor: SEED_MINOR,
        protocol,
        local_node: NODE.to_owned(),
        local_address: LOCAL_ADDR.to_owned(),
        local_port: SEED_PORT,
        peer_node: PEER_NODE.to_owned(),
        peer_address: peer_ip,
        peer_port,
        disk_path: format!("/dev/{VG}/{resource}"),
        shared_secret: SECRET.to_owned(),
    }
    .write(&base.join("drbd.d"))
    .expect("write fixture res file");
    let mut world = world.lock().expect("world");
    let key = format!("{VG}/{resource}");
    let extent = world.vg_extent_size;
    let mut tags = vec!["volvisor.generation=1".to_owned()];
    if tagged {
        tags.insert(0, format!("volvisor.owner={}", volume_id.as_str()));
    }
    world.lvs.insert(
        key.clone(),
        FakeLv {
            size: extent_round_up(size_bytes, extent),
            tags,
            has_data: true,
        },
    );
    world.metadata.insert(resource.clone());
    world.lineage_salt += 1;
    let generation = world.lineage_salt;
    world.lineage.insert(
        resource.clone(),
        GiSet::for_resource_generation(&resource, generation),
    );
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
            peer_node: NODE.to_owned(),
            // Seeding lands an empty block map under the seeded
            // lineage (the sparse zero fill; P5 plan §2.1).
            blocks: BTreeMap::new(),
            apply_queue: VecDeque::new(),
            write_seq: 0,
        },
    );
    resource
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
    seed_definition_with_protocol(resource, minor, port, ReplicationMode::A)
}

/// Like [`seed_definition`], with the replication protocol chosen (the
/// adopt-flow classification branches on it — protocol C with a
/// recorded barrier is the `SAFE_CURRENT` evidence row).
pub fn seed_definition_with_protocol(
    resource: &str,
    minor: u32,
    port: u16,
    protocol: ReplicationMode,
) -> ResourceDefinition {
    let (peer_ip, peer_port) = peer_endpoint();
    ResourceDefinition {
        resource_name: resource.to_owned(),
        minor,
        protocol,
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
    seed_volume_with_protocol(base, world, volume_id, size_bytes, ReplicationMode::A);
}

/// Like [`seed_volume`], with the resource definition's replication
/// protocol chosen (the adopt-flow classification branches on it).
pub fn seed_volume_with_protocol(
    base: &Path,
    world: &Arc<Mutex<FakeDrbd>>,
    volume_id: &str,
    size_bytes: u64,
    protocol: ReplicationMode,
) {
    seed_volume_with_identity(
        base, world, volume_id, size_bytes, protocol, SEED_MINOR, SEED_PORT,
    );
}

/// Seed a volume with an explicit minor/port: multi-volume worlds
/// need one device identity per seeded resource (the standard
/// [`seed_volume_with_protocol`] pins the single-volume fixture
/// identity).
pub fn seed_volume_with_identity(
    base: &Path,
    world: &Arc<Mutex<FakeDrbd>>,
    volume_id: &str,
    size_bytes: u64,
    protocol: ReplicationMode,
    minor: u32,
    port: u16,
) {
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
                minor,
                port,
                size_bytes,
                requested_size_bytes: size_bytes,
                generation: 1,
                project_id: ProjectId::new("seed-project").expect("valid project id"),
                block_size: 4096,
                replication_mode: protocol,
                creation_payload: "seed".to_owned(),
                created_at: 0,
            },
            runtime: VolumeRuntime {
                state: VolumeLifecycle::Ready,
                attachment: None,
                seeded: true,
                authority: None,
                fence: None,
                migration: None,
            },
        },
    );
    // Keep the monotonic counters ahead of the seeded resource so a
    // later allocation can never collide with it.
    state.observe_minor(minor);
    state.observe_port(port);
    state.save(&state_path).expect("seed volume state");
    seed_definition_with_protocol(&resource, minor, port, protocol)
        .write(&base.join("drbd.d"))
        .expect("write fixture res file");
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
            // A seeded volume holds data on both ends (create-md over
            // this LV would be refused — the real drbdmeta rule).
            has_data: true,
        },
    );
    world.metadata.insert(resource.clone());
    // The seeded resource's FIXED identity set (deterministic per
    // resource name — a re-seeded world reproduces it byte for byte,
    // which is what adoption-time lineage comparison needs; see
    // [`GiSet`]).
    world
        .lineage
        .insert(resource.clone(), GiSet::for_resource(&resource));
    let local = world.lvs.get(&key).map_or(size_bytes, |lv| lv.size);
    let device = local.min(world.peer_backing(local));
    world.resources.insert(
        resource.clone(),
        FakeResource {
            minor,
            role: Role::Secondary,
            local_disk: DiskState::UpToDate,
            peer_disk: DiskState::UpToDate,
            peer_role: Role::Secondary,
            resyncing: false,
            device_size: device,
            peer_node: PEER_NODE.to_owned(),
            // Seeding lands an empty block map under the seeded
            // lineage (the sparse zero fill; P5 plan §2.1).
            blocks: BTreeMap::new(),
            apply_queue: VecDeque::new(),
            write_seq: 0,
        },
    );
}

/// The resource definition as the PEER host sees the same volume: the
/// node names, addresses and port sides swapped, the same backing path
/// (the P3 operator model deploys the identical definition on both
/// ends). The local node of THIS definition (`node-b`) carries
/// `node-id 0` and the original node `node-id 1` (resgen pins local to
/// 0, peer to 1 — so from the peer host's world the original node is
/// addressed as peer device 1, the default [`FakeDrbd::peer_node_id`]).
///
/// Symmetric replication ports keep the peer view self-contained: the
/// endpoint identity the witness sees is host/resource/disk (see the
/// provider's `endpoint_backing_identity`), never the port.
pub fn peer_definition(
    resource: &str,
    minor: u32,
    port: u16,
    protocol: ReplicationMode,
) -> ResourceDefinition {
    let (peer_ip, _peer_port) = peer_endpoint();
    ResourceDefinition {
        resource_name: resource.to_owned(),
        minor,
        protocol,
        local_node: PEER_NODE.to_owned(),
        local_address: peer_ip,
        local_port: port,
        peer_node: NODE.to_owned(),
        peer_address: LOCAL_ADDR.to_owned(),
        peer_port: port,
        disk_path: format!("/dev/{VG}/{resource}"),
        shared_secret: SECRET.to_owned(),
    }
}

/// Seed a volume's PEER-side replica into a second simulated world:
/// the backing LV (with the ownership tag and data on it), the on-LV
/// metadata, the running resource (`Secondary`, both ends `UpToDate`,
/// the original node as its peer), the peer-view res file and the
/// FIXED lineage set — and **no state-file entry**.
///
/// This is the destination host of a cross-host handoff: its replica
/// is established (adoption's lineage comparison must match the
/// source's [`seed_volume_with_identity`] — hence the same
/// deterministic [`GiSet::for_resource`]), but volvisor on that host
/// has never tracked the volume, so the destination-side promote
/// (`primary --force` through the provider's seeding promotion, which
/// is why the world's `peer_role` never blocks it) starts from a
/// clean slate exactly as a real peer would.
///
/// The world's `uname -n` answer is set to [`PEER_NODE`] here (the
/// peer host's identity); construct its provider with
/// [`config_for_peer`] over the same `base`.
pub fn seed_peer_volume(
    base: &Path,
    world: &Arc<Mutex<FakeDrbd>>,
    volume_id: &str,
    size_bytes: u64,
    protocol: ReplicationMode,
    minor: u32,
    port: u16,
) {
    let volume_id = VolumeId::new(volume_id).expect("valid volume id");
    let resource = resource_name_for(&volume_id);
    peer_definition(&resource, minor, port, protocol)
        .write(&base.join("drbd.d"))
        .expect("write fixture peer res file");
    let mut world = world.lock().expect("world");
    PEER_NODE.clone_into(&mut world.node_name);
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
            // The replica holds data on both ends (the source's
            // seeding resync reached it).
            has_data: true,
        },
    );
    world.metadata.insert(resource.clone());
    // The SAME fixed identity set the source's world carries for this
    // resource: the replica shares the source's data-generation
    // lineage, which is what adoption-time comparison proves.
    world
        .lineage
        .insert(resource.clone(), GiSet::for_resource(&resource));
    let local = world.lvs.get(&key).map_or(size_bytes, |lv| lv.size);
    let device = local.min(world.peer_backing(local));
    world.resources.insert(
        resource.clone(),
        FakeResource {
            minor,
            role: Role::Secondary,
            local_disk: DiskState::UpToDate,
            peer_disk: DiskState::UpToDate,
            peer_role: Role::Secondary,
            resyncing: false,
            device_size: device,
            peer_node: NODE.to_owned(),
            // The replica's data path starts empty (the sparse zero
            // fill; the source's writes reach it through the apply
            // queue's drains — P5 plan §2.3).
            blocks: BTreeMap::new(),
            apply_queue: VecDeque::new(),
            write_seq: 0,
        },
    );
    // Deliberately NO state-file entry: the destination host has never
    // tracked this volume (the peer-side promote must be able to run
    // from the res file and a live witness grant alone — a pre-existing
    // foreign entry is exactly what its re-drive gate refuses).
}

/// Seed an LV into the simulated world without a state entry: owned
/// (`Some(volume_id)`) or foreign (`None`) — untracked-LV and
/// reclaim tests. `has_data` pins whether the LV's data area carries
/// non-zero data (drbdmeta's create-md decision rests on it).
pub fn seed_lv(
    world: &Arc<Mutex<FakeDrbd>>,
    vg: &str,
    name: &str,
    size_bytes: u64,
    owner: Option<&str>,
    has_data: bool,
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
            has_data,
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
