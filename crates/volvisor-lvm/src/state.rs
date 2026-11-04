//! Durable JSON state for the LVM provider.
//!
//! The state file is the provider-internal cross-reference store: which
//! volume identity maps to which `vg/lv`, the single recorded attachment
//! per volume, and which devices have been claimed into which volume
//! groups. It is never exposed to tenants (the API surface derives all
//! responses from it).
//!
//! Persistence is atomic: [`LvmState::save`] writes a unique sibling
//! temp file, fsyncs it, renames it over the target and fsyncs the parent
//! directory, so a crash can never leave a torn or half-renamed state
//! file. A missing file loads as an empty state (first start).
//!
//! # Bounded, hashed temp files and the startup sweep
//!
//! Every physical write uses its own bounded temp name,
//! `.volvisor-<16-lowercase-hex-fnv1a64>.<pid>.<sequence>.tmp`, where the
//! hash is a stable FNV-1a 64-bit hash of the canonical state-path
//! identity (the canonicalized parent plus the file name; raw `OsStr`
//! bytes on unix, lossy UTF-8 elsewhere) and the sequence is a
//! process-wide atomic counter. The name does not grow with the state
//! file name, so a long state name cannot exceed `NAME_MAX`. Two
//! writers — even in the same process — therefore never share, truncate
//! or delete each other's temp file, and a failed save removes only the
//! temp path it allocated for that attempt.
//!
//! A process that died mid-save leaves its temp file behind. The
//! provider constructor sweeps them before it loads or writes state
//! (`sweep_stale_temps`). The sweep only inspects the state file's own
//! parent directory and only considers the exact legacy `<path>.tmp`
//! name plus this state path's hashed namespace. It never removes a temp
//! registered as in flight in this process; it removes a same-pid
//! unregistered temp immediately (pid reuse), and removes a foreign-pid
//! temp only after a one-hour grace. A regular legacy `<path>.tmp` has no
//! pid component and is treated like a foreign writer: it is removed only
//! when its mtime is older than that grace, because a fresh one may
//! belong to an old binary during a rolling restart or to an operator's
//! copy; non-regular entries are skipped. A foreign-pid or legacy temp
//! whose mtime is in the future is never swept (`duration_since` fails),
//! which is the conservative fail-safe; a same-pid unregistered temp is
//! still removed immediately regardless of mtime. The real guarantee is
//! therefore: active temps
//! are never removed, stale same-pid temps are removed, and a foreign
//! process's recent temp (or a fresh legacy temp) is left alone.
//!
//! Two intermediate temp shapes were never released: `f0e5237`'s
//! `<path>.<pid>.<sequence>.tmp` and `a8c23d6`'s lexical-path-hash
//! namespace. For an already-canonical spelling (the common case) the
//! final sweep recognizes the `a8c23d6` namespace, so already-stale
//! same-pid or aged leftovers are removed under the pid/grace policy; a
//! fresh foreign-pid leftover still waits out the one-hour grace. Only a
//! leftover created through a non-canonical spelling (symlink, `./`, `..`
//! or a bare relative path) is orphaned by the canonical-identity hash
//! change. The `f0e5237` shape is genuinely never recognized. Those
//! unrecognized leftovers need manual removal; do not broaden the sweep to
//! add the `f0e5237` shape or the orphaned non-canonical `a8c23d6`
//! spelling.
//!
//! This sweep is not cross-process writer exclusion. Cross-process
//! exclusion is the journal flock; two daemons must not alias one state
//! path (including a custom `lvm_state_path`). The sweep only cleans
//! crash leftovers and cannot make two live writers safe.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Deserializer, Serialize};
use volvisor_types::crash::{STORE_LVM_STATE, StoreCrashHooks, StoreSavePoint};
use volvisor_types::request::ErasurePolicy;
use volvisor_types::schema::{SchemaVersionProbe, ensure_supported_schema_version};
use volvisor_types::{
    AccessMode, ApiError, ApiErrorCode, AttachmentEvidence, AttachmentId, AttachmentState,
    DeviceId, DeviceRole, HostId, MoveVolumeBackingState, OperationId, ProjectId, VolumeId,
    VolumeLifecycle,
};

/// Current durable LVM-state schema version.
///
/// Version 1 is the baseline (including the pre-versioning format; a
/// missing `schema_version` decodes as
/// [`volvisor_types::BASELINE_SCHEMA_VERSION`]). Version 2 adds the
/// durable `pending_creates`/`pending_grows`/`pending_deletes` intent
/// maps introduced for issue #70. Version 3 replaces the unbounded
/// `retired_attachment_ids` vector with the fixed-size
/// [`RetiredAttachmentFilter`] introduced for issue #63. Because each
/// is an additive serialized-shape change, the version is bumped per
/// the [durable-state schema compatibility policy](../../../docs/schema-compatibility.md):
/// older files still load (the intent maps default empty; the legacy
/// retired-id vector is imported into the filter) and are upgraded on
/// the first successful save, while an older binary refuses a newer
/// file with the typed unsupported-version error instead of failing on
/// an unknown field.
pub const LVM_STATE_SCHEMA_VERSION: u32 = 3;

/// Number of bits in one volume's retired-attachment tombstone filter.
///
/// One kibibyte per volume. The filter is only materialized once a
/// volume has retired its first attachment id, so the common
/// never-attached volume costs no bytes. See
/// [`RetiredAttachmentFilter`] for the saturation/fail-closed
/// trade-off.
pub const RETIRED_ATTACHMENT_FILTER_BITS: usize = 8 * 1024;

/// Number of double-hash probes per inserted attachment id.
const RETIRED_ATTACHMENT_FILTER_HASHES: u64 = 7;

/// Byte length of [`RETIRED_ATTACHMENT_FILTER_BITS`].
const RETIRED_ATTACHMENT_FILTER_BYTES: usize = RETIRED_ATTACHMENT_FILTER_BITS / 8;

/// Fixed-size Bloom filter for a volume's consumed attachment ids.
///
/// **Legacy schema-v3 shape, no longer authoritative.** This filter was
/// introduced for issue #63 to bound the schema-v1/v2
/// `retired_attachment_ids` vector, but it is unsound as a refusal
/// mechanism: once a churned volume fills it, every fresh id is a false
/// positive and is refused as `IDEMPOTENCY_CONFLICT`, making the volume
/// unusable. It has **no false negatives** (`true` is "consumed or a false
/// positive"), which is why it must never be used to reject a request. The
/// current provider keeps the type only to decode a v3 file without losing
/// data and never inserts into or consults it; delayed reports are fenced
/// by the strictly increasing attachment generation.
///
/// Bits are set from SHA-256 double hashing over the id's bytes, so the
/// on-disk representation is stable across process restarts and
/// architectures. `inserted` is the legacy saturation metric. The number of
/// bits and probes are schema properties; changing either requires another
/// schema bump.
///
/// The filter is serialized as an object holding the bit array as a
/// lowercase hex string plus the insertion counter (not a JSON number
/// array) so the whole-state file stays compact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetiredAttachmentFilter {
    bits: Vec<u8>,
    inserted: u64,
}

impl Default for RetiredAttachmentFilter {
    fn default() -> Self {
        Self {
            bits: vec![0; RETIRED_ATTACHMENT_FILTER_BYTES],
            inserted: 0,
        }
    }
}

impl RetiredAttachmentFilter {
    /// Whether no id has ever been inserted (the compact on-disk shape).
    ///
    /// Requires both the counter and every bit to be clear. A state file
    /// with set bits but a zero counter is not canonical, but treating it as
    /// empty would omit the filter on the next save and silently lose
    /// tombstones (a false negative for every lost id), so it is preserved.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inserted == 0 && self.bits.iter().all(|byte| *byte == 0)
    }

    /// Number of attachment ids ever inserted (including duplicates).
    #[must_use]
    pub fn inserted(&self) -> u64 {
        self.inserted
    }

    /// Fraction of filter bits currently set, in `0.0..=1.0`.
    ///
    /// Serialized alongside the bit array so an operator can inspect creep
    /// toward saturation in the durable state file; a saturated filter keeps
    /// refusing correctly (no false negatives), but every new id is also
    /// refused (false positives). This revision exposes no metric or log
    /// line for it.
    // `RETIRED_ATTACHMENT_FILTER_BITS` is 8192, exactly representable in
    // `f64`; the fraction is a diagnostic, never a control-flow input.
    #[allow(clippy::cast_precision_loss)]
    #[must_use]
    pub fn saturation(&self) -> f64 {
        let set: u32 = self.bits.iter().map(|byte| byte.count_ones()).sum();
        f64::from(set) / RETIRED_ATTACHMENT_FILTER_BITS as f64
    }

    /// Record that `id` was consumed by this volume.
    ///
    /// Idempotent in the bit array: re-inserting the same id sets the same
    /// bits and leaves [`RetiredAttachmentFilter::saturation`] unchanged;
    /// the `inserted` counter still advances.
    pub fn insert(&mut self, id: &AttachmentId) {
        for index in filter_indices(id.as_str()) {
            self.bits[index / 8] |= 1 << (index % 8);
        }
        self.inserted = self.inserted.saturating_add(1);
    }

    /// Whether `id` may have been consumed by this volume.
    ///
    /// `false` is definitive (never consumed). `true` means "consumed or a
    /// false positive", so it is **diagnostic only**: treating it as an
    /// authoritative refusal would reject fresh ids once the filter fills.
    /// The attachment generation is the fence.
    #[must_use]
    pub fn maybe_contains(&self, id: &AttachmentId) -> bool {
        filter_indices(id.as_str()).all(|index| self.bits[index / 8] & (1 << (index % 8)) != 0)
    }
}

/// The double-hashed bit indices for one attachment id.
fn filter_indices(id: &str) -> impl Iterator<Item = usize> {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(id.as_bytes());
    let mut first = [0u8; 8];
    let mut second = [0u8; 8];
    first.copy_from_slice(&digest[..8]);
    second.copy_from_slice(&digest[8..16]);
    let h1 = u64::from_le_bytes(first);
    // An even stride would visit at most half the filter; force odd.
    let h2 = u64::from_le_bytes(second) | 1;
    (0..RETIRED_ATTACHMENT_FILTER_HASHES).map(move |probe| {
        let index = h1.wrapping_add(probe.wrapping_mul(h2));
        // `index % 8192` always fits `usize` on every supported target; the
        // conversion cannot fail, and the fallback only keeps the helper
        // panic-free without weakening the filter (a wrong-but-valid bit
        // would add a fail-closed false positive).
        usize::try_from(index % RETIRED_ATTACHMENT_FILTER_BITS as u64).unwrap_or_default()
    })
}

impl Serialize for RetiredAttachmentFilter {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("RetiredAttachmentFilter", 2)?;
        state.serialize_field("bits", &encode_hex(&self.bits))?;
        state.serialize_field("inserted", &self.inserted)?;
        state.end()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetiredAttachmentFilterWire {
    bits: String,
    inserted: u64,
}

impl<'de> Deserialize<'de> for RetiredAttachmentFilter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let wire = RetiredAttachmentFilterWire::deserialize(deserializer)?;
        // Reject a wrong-length `bits` string before allocating the
        // decoded buffer: a corrupt file can otherwise make the decoder
        // allocate proportional to an arbitrarily large hex field.
        let expected_hex_len = RETIRED_ATTACHMENT_FILTER_BYTES * 2;
        if wire.bits.len() != expected_hex_len {
            return Err(D::Error::custom(format!(
                "invalid retired-attachment filter: {} hex bytes, expected {}",
                wire.bits.len(),
                expected_hex_len
            )));
        }
        let bits = decode_hex(&wire.bits).map_err(|detail| {
            D::Error::custom(format!("invalid retired-attachment filter: {detail}"))
        })?;
        if bits.len() != RETIRED_ATTACHMENT_FILTER_BYTES {
            return Err(D::Error::custom(format!(
                "invalid retired-attachment filter: {} bytes, expected {}",
                bits.len(),
                RETIRED_ATTACHMENT_FILTER_BYTES
            )));
        }
        Ok(Self {
            bits,
            inserted: wire.inserted,
        })
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    if text.len() % 2 != 0 {
        return Err("odd-length hex string".to_owned());
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    let raw = text.as_bytes();
    for pair in raw.chunks_exact(2) {
        let high = (pair[0] as char)
            .to_digit(16)
            .ok_or_else(|| "non-hex character".to_owned())?;
        let low = (pair[1] as char)
            .to_digit(16)
            .ok_or_else(|| "non-hex character".to_owned())?;
        // Two hex digits always fit a byte; the conversion cannot fail and
        // the fallback only keeps the decoder panic-free. A corrupted byte
        // is still rejected by the exact-length check above.
        bytes.push(u8::try_from(high * 16 + low).unwrap_or_default());
    }
    Ok(bytes)
}

/// The volume→LV cross-reference plus the durable volume attributes.
///
/// `lv_name` is derived from the `volume_id` (sanitized plus a hash
/// suffix over the full id, so uniqueness rests on that 32-bit suffix
/// rather than the sanitized segment itself);
/// `vg_name` identifies the claimed pool the volume lives in (a
/// provider-internal reference, never exposed to tenants).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VolumeEntry {
    /// Volume group holding the logical volume.
    pub vg_name: String,
    /// Logical volume name (the sanitized volume identity).
    pub lv_name: String,
    /// Effective (provisioned) size in bytes: what `lvs` reported after
    /// LVM rounded the request up to whole physical extents.
    pub size_bytes: u64,
    /// The size the caller originally requested, in bytes. Thick LVM
    /// rounds sizes up to the physical extent, so this can be smaller
    /// than `size_bytes`; idempotent-create replay compares *this*
    /// value (never the rounded one) against a replayed request.
    ///
    /// The initial P1 format had no requested-vs-effective distinction.
    /// It decodes with this field equal to `size_bytes` (the only size
    /// that file recorded), never with a guessed or zero value.
    pub requested_size_bytes: u64,
    /// Volume generation (optimistic concurrency fencing).
    pub generation: u64,
    /// Writer epoch / data epoch for authority reasoning.
    pub data_epoch: u64,
    /// Owning tenant project (needed to answer inspect/list).
    pub project_id: ProjectId,
    /// Logical block size in bytes.
    pub block_size: u32,
    /// Canonical creation payload with `operation_id` normalized out;
    /// used to detect idempotent replays vs. payload conflicts.
    pub creation_payload: String,
}

/// Wire shape for [`VolumeEntry`]; differs only in the one legacy field.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VolumeEntryWire {
    vg_name: String,
    lv_name: String,
    size_bytes: u64,
    #[serde(default)]
    requested_size_bytes: Option<u64>,
    generation: u64,
    data_epoch: u64,
    project_id: ProjectId,
    block_size: u32,
    creation_payload: String,
}

impl<'de> Deserialize<'de> for VolumeEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = VolumeEntryWire::deserialize(deserializer)?;
        Ok(Self {
            vg_name: wire.vg_name,
            lv_name: wire.lv_name,
            size_bytes: wire.size_bytes,
            // P1 files recorded only the effective size. That is the
            // honest requested size for them; an absent field never
            // becomes a fabricated zero.
            requested_size_bytes: wire.requested_size_bytes.unwrap_or(wire.size_bytes),
            generation: wire.generation,
            data_epoch: wire.data_epoch,
            project_id: wire.project_id,
            block_size: wire.block_size,
            creation_payload: wire.creation_payload,
        })
    }
}

/// The mutable runtime portion of a stored volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VolumeRuntime {
    /// Current lifecycle state.
    pub state: VolumeLifecycle,
    /// The single recorded attachment, if any (single-writer P0 profile).
    pub attachment: Option<AttachmentRecord>,
    /// **Legacy schema-v3 tombstones**, retained only so a v3 state file
    /// decodes without loss. The current provider never inserts into or
    /// consults this filter: a delayed report is fenced by the strictly
    /// increasing attachment generation, and refusing a Bloom-filter hit
    /// would reject fresh ids once the filter fills (issue #63). Omitted
    /// while empty so a never-attached volume stays compact.
    #[serde(
        rename = "retired_attachment_filter",
        default,
        skip_serializing_if = "RetiredAttachmentFilter::is_empty"
    )]
    pub retired_attachments: RetiredAttachmentFilter,
}

/// Wire shape for [`VolumeRuntime`].
///
/// `retired_attachment_ids` is the unbounded schema-v1/v2 vector: it is
/// accepted on load (and folded into [`RetiredAttachmentFilter`]) but
/// never written again. `retired_attachment_filter` is the schema-v3
/// shape.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VolumeRuntimeWire {
    state: VolumeLifecycle,
    attachment: Option<AttachmentRecord>,
    #[serde(default)]
    retired_attachment_ids: Vec<AttachmentId>,
    #[serde(default)]
    retired_attachment_filter: Option<RetiredAttachmentFilter>,
}

impl<'de> Deserialize<'de> for VolumeRuntime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = VolumeRuntimeWire::deserialize(deserializer)?;
        let mut retired_attachments = wire.retired_attachment_filter.unwrap_or_default();
        // Import a legacy v1/v2 vector. Inserting into an existing
        // filter as well is a safe superset (never a false negative).
        for id in wire.retired_attachment_ids {
            retired_attachments.insert(&id);
        }
        Ok(Self {
            state: wire.state,
            attachment: wire.attachment,
            retired_attachments,
        })
    }
}

/// A volume as stored in provider state: durable entry + runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredVolume {
    /// Durable cross-reference and attributes.
    pub entry: VolumeEntry,
    /// Lifecycle state and attachment record.
    pub runtime: VolumeRuntime,
}

/// A recorded attachment (crash-replay never fabricates a second one).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentRecord {
    /// Attachment identity (caller-chosen, opaque).
    pub id: AttachmentId,
    /// Consuming VM identity.
    pub vm_id: String,
    /// Host the attachment is scoped to.
    pub host_id: HostId,
    /// Attachment generation.
    pub generation: u64,
    /// Granted access mode.
    pub access_mode: AccessMode,
    /// The VMM-side disk id the consumer configured for this
    /// frontend, when it configured one (P6-B): the durable mapping
    /// a grow's capacity notification addresses the VMM's
    /// resize-disk call with. Absent on an attached volume is a
    /// recorded, fail-closed refusal reason at notification time —
    /// never a guess. Optional and defaulted so state files written
    /// before P6-B load unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vmm_disk_id: Option<String>,
    /// Current evidence state (contract section 3). Optional/defaulted so
    /// state files written before consumer reporting load as `prepared`.
    #[serde(default)]
    pub state: AttachmentState,
    /// Monotonic evidence revision; advances only on an applied report.
    /// Defaulted to the initial revision for pre-report state files.
    #[serde(default = "volvisor_types::domain::initial_state_revision")]
    pub state_revision: u64,
    /// Evidence basis of the current state; absent for `prepared`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<AttachmentEvidence>,
    /// Consumer that reported the current state (untrusted label).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reporter: Option<String>,
    /// Server-assigned receipt time of the current state, Unix ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at_ms: Option<u64>,
}

/// A claimed physical device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceEntry {
    /// Stable hardware identity (WWN/serial) recorded at claim time.
    pub stable_identity: String,
    /// Device path used for `pvcreate`/`pvremove` at claim time.
    pub path: String,
    /// Volume group created on this device.
    pub vg_name: String,
    /// Role the device is claimed for (exactly one, SPEC-0002 section 3).
    pub role: DeviceRole,
    /// Monotonic ownership generation.
    pub owner_generation: u64,
}

/// One journaled online move of a volume's backing extents
/// (contract section 4A, `same_vg_extent_move` scope).
///
/// The record is the **reconciliation anchor**: the daemon can die
/// at any boundary of the move, and the kernel-side `pvmove` mirror
/// plus this record are what survive. The record's state is the
/// honest subset of [`MoveVolumeBackingState`] — `PREPARING`,
/// `COPYING`, `COMPLETE`, `IN_DOUBT`, and `FAILED` written by the
/// retry reconcile **alone** (the verified-rollback resolution of a
/// parked `IN_DOUBT`: the extents verifiably back on the source, the
/// relocation did not happen) — never `MIRROR_READY`/`PIVOTED`
/// (mirror-path states; a same-VG extent move never pivots, the
/// LV's dm identity is stable) and never `FAILED` from a drive (an
/// unknown outcome is `IN_DOUBT`; deterministic rejections are
/// typed errors, not states).
///
/// Keyed by volume: at most one move record exists per volume. A
/// `COMPLETE` record for the same source/target answers re-issues
/// idempotently; an `IN_DOUBT` record parks further moves typed
/// until reconciliation proves completion, resolves the verified
/// rollback, or an operator resolves it; a `FAILED` record admits a
/// fresh drive (the new record supersedes it at the drive's
/// `PREPARING` journal write).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveRecord {
    /// The operation that started (or most recently re-attached to)
    /// this move — diagnostics only; the API-layer idempotency
    /// journal is the replay authority.
    pub operation_id: OperationId,
    /// The PV the extents are being evacuated from.
    pub source_pv: String,
    /// The PV the extents are being evacuated to.
    pub target_pv: String,
    /// The move's current state (the honest subset — see the type
    /// documentation).
    pub state: MoveVolumeBackingState,
    /// Honest detail for non-complete states (the `IN_DOUBT` reason,
    /// or the supervision note for `COPYING`).
    pub detail: Option<String>,
}

/// A durable intent for a create whose `lvcreate` may already have
/// run (the kill-9 window between the CLI success and the volume
/// record's commit).
///
/// Written and fsynced **before** `lvcreate`. If the daemon dies after
/// the CLI created the LV but before the volume record is durable,
/// startup recovery adopts the LV only when it is present and at
/// least the recorded requested size; a missing LV drops the intent,
/// and a smaller LV fails startup rather than adopting a wrong
/// geometry. The payload and requested size are the idempotent-replay
/// comparison for a retried create.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateIntent {
    /// Volume group the LV was created in.
    pub vg_name: String,
    /// Logical volume name (must equal the id-derived name).
    pub lv_name: String,
    /// The size the caller requested, in bytes (pre-extent-rounding).
    pub requested_size_bytes: u64,
    /// Owning tenant project.
    pub project_id: ProjectId,
    /// Logical block size in bytes.
    pub block_size: u32,
    /// Canonical creation payload with `operation_id` normalized out;
    /// a retried create must match this and the requested size.
    pub creation_payload: String,
}

/// A durable intent for an online grow between the capacity check and
/// the final volume-record commit.
///
/// Written and fsynced **before** `lvextend`. Recovery rolls the grow
/// forward: an LV at or above the requested size completes the record
/// (generation bump) and an LV still at the from-size drops the
/// intent (the grow never landed). An LV in between fails startup
/// rather than guessing. The provider never shrinks an LV as
/// compensation — data may already have been written into the new
/// extents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrowIntent {
    /// The operation that authorized the grow.
    pub operation_id: OperationId,
    /// The volume size before the grow, in bytes.
    pub from_size_bytes: u64,
    /// The size the grow requested, in bytes.
    pub requested_size_bytes: u64,
    /// The volume generation the grow was authorized against.
    pub expected_generation: u64,
}

/// A durable intent for a delete between the non-destructive
/// validation and the volume record's removal.
///
/// Written and fsynced **before** `blkdiscard`/`lvremove`. Recovery
/// completes the delete when the stored generation still matches the
/// authorization; a stale-generation intent is dropped without any
/// deletion (never delete on a stale authorization). Foreign LVs are
/// never touched — recovery only acts on the intent's own recorded
/// volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteIntent {
    /// The operation that authorized the delete.
    pub operation_id: OperationId,
    /// The volume generation the delete was authorized against.
    pub expected_generation: u64,
    /// The erasure policy the delete must honor.
    pub erasure_policy: ErasurePolicy,
}

/// The whole durable provider state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LvmState {
    /// Durable schema version; absent in pre-versioning files (baseline 1).
    #[serde(default = "volvisor_types::baseline_schema_version")]
    schema_version: u32,
    volumes: BTreeMap<VolumeId, StoredVolume>,
    devices: BTreeMap<DeviceId, DeviceEntry>,
    /// Online-move records, keyed by volume (at most one per
    /// volume). Defaulted so state files written before P6-C load
    /// unchanged.
    #[serde(default)]
    moves: BTreeMap<VolumeId, MoveRecord>,
    /// Durable create intents, keyed by volume. Added in schema v2;
    /// defaulted so v1 files load unchanged and skipped when empty so
    /// a state with no pending create serializes without the map.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pending_creates: BTreeMap<VolumeId, CreateIntent>,
    /// Durable grow intents, keyed by volume. Added in schema v2;
    /// defaulted so v1 files load unchanged and skipped when empty so
    /// a state with no pending grow serializes without the map.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pending_grows: BTreeMap<VolumeId, GrowIntent>,
    /// Durable delete intents, keyed by volume. Added in schema v2;
    /// defaulted so v1 files load unchanged and skipped when empty so
    /// a state with no pending delete serializes without the map.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pending_deletes: BTreeMap<VolumeId, DeleteIntent>,
}

impl Default for LvmState {
    fn default() -> Self {
        Self {
            schema_version: LVM_STATE_SCHEMA_VERSION,
            volumes: BTreeMap::new(),
            devices: BTreeMap::new(),
            moves: BTreeMap::new(),
            pending_creates: BTreeMap::new(),
            pending_grows: BTreeMap::new(),
            pending_deletes: BTreeMap::new(),
        }
    }
}

impl LvmState {
    /// Load the state from `path`.
    ///
    /// A missing file is an empty state (first start). A corrupt file, an
    /// unknown field at a supported version, or a version newer than
    /// [`LVM_STATE_SCHEMA_VERSION`] is a typed `INTERNAL` error — state is
    /// never guessed, partially decoded or reset silently.
    pub fn load(path: &Path) -> Result<Self, ApiError> {
        let path_display = path.display();
        match fs::read_to_string(path) {
            Ok(text) => {
                // Probe the version with unknown fields tolerated, so a file
                // from a newer build gets the precise unsupported-version
                // refusal rather than a generic parse error.
                let probe: SchemaVersionProbe = serde_json::from_str(&text).map_err(|e| {
                    ApiError::new(
                        ApiErrorCode::Internal,
                        format!("failed to parse provider state {path_display}: {e}"),
                    )
                })?;
                ensure_supported_schema_version(
                    "lvm",
                    path,
                    probe.schema_version,
                    LVM_STATE_SCHEMA_VERSION,
                )?;
                serde_json::from_str(&text).map_err(|e| {
                    ApiError::new(
                        ApiErrorCode::Internal,
                        format!("failed to parse provider state {path_display}: {e}"),
                    )
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ApiError::new(
                ApiErrorCode::Internal,
                format!("failed to read provider state {path_display}: {e}"),
            )),
        }
    }

    /// Persist the state atomically.
    ///
    /// Takes `&mut self` to mark the intent to persist a mutation: write
    /// a unique bounded `.volvisor-<hash>.<pid>.<sequence>.tmp` (mode
    /// `0600` on unix — the state names devices and volume placements, so
    /// it is owner-only), fsync, rename over `path`, fsync the directory.
    /// If any step fails, the temp file this call allocated is removed and
    /// an `INTERNAL` error is returned; the previous state file remains
    /// intact and another writer's temp file is never touched.
    pub fn save(&mut self, path: &Path) -> Result<(), ApiError> {
        self.save_with_hooks(path, None)
    }

    /// [`Self::save`] with the store-save crash seam armed: the
    /// boundaries (after the tmp content write, after its fsync,
    /// after the rename, and after the rename before the directory
    /// fsync) consult `crash` exactly like the grow store's saves, so
    /// a rig can kill the daemon at any commit split of a
    /// move-record mutation, or inject a non-kill save error at the
    /// directory-fsync boundary. The seam is inert unless the rig
    /// armed this store's points.
    pub fn save_with_hooks(
        &mut self,
        path: &Path,
        crash: Option<&StoreCrashHooks>,
    ) -> Result<(), ApiError> {
        let data = self.encode_for_save()?;
        Self::save_encoded_with_hooks(&data, path, crash)
    }

    /// Encode the state for a durable save, advancing the schema version
    /// to the version this build writes.
    ///
    /// Split out from [`Self::save_with_hooks`] so the provider's coalesced
    /// writer can capture the exact bytes under its state lock and perform
    /// the physical write later (Phase D D3 slice 2, #62). The encoding
    /// shape is unchanged: compact JSON of the whole state.
    pub(crate) fn encode_for_save(&mut self) -> Result<Vec<u8>, ApiError> {
        // A successful open may have decoded an older file; every save
        // writes the current version so an upgrade becomes durable on the
        // first mutation (or the startup reconcile's first save).
        self.schema_version = LVM_STATE_SCHEMA_VERSION;
        // Issue #62: the state file is internal, whole-file rewritten and
        // fsynced on every mutation. Compact JSON roughly halves the bytes
        // written and fsynced versus pretty-printing, without changing the
        // decoded shape or schema.
        serde_json::to_vec(self).map_err(|e| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("failed to serialize provider state: {e}"),
            )
        })
    }

    /// Write already-encoded state bytes through the existing atomic save
    /// sequence.
    ///
    /// This is the physical half of [`Self::save_with_hooks`]: one unique
    /// bounded sibling temp, the four crash consults in their established
    /// order, fsync, rename and directory fsync. It takes bytes rather than
    /// `&mut self` so the provider's coalesced writer can commit a capture
    /// it reserved earlier while another caller owns the state reference
    /// (Phase D D3 slice 2, #62). The temp namespace, permissions, cleanup
    /// and error strings are unchanged from slice 1.
    pub(crate) fn save_encoded_with_hooks(
        data: &[u8],
        path: &Path,
        crash: Option<&StoreCrashHooks>,
    ) -> Result<(), ApiError> {
        let tmp_path = unique_sibling_tmp_path(path);
        // Register before `create_new` and keep the guard for the whole
        // attempt (including the error cleanup). A panic/unwind drops it
        // too, so the startup sweep can never mistake a temp this process
        // is still writing for a crash leftover.
        let _registration = TempRegistration::register(&tmp_path);
        // Track whether this call actually created the temp file: cleanup
        // must never remove a file that already existed at the allocated
        // name (for example a stale file the startup sweep did not see).
        let mut created = false;
        let result = Self::save_encoded_to(data, &tmp_path, path, crash, &mut created);
        if result.is_err() && created {
            drop(fs::remove_file(&tmp_path));
        }
        result
    }

    fn save_encoded_to(
        data: &[u8],
        tmp_path: &Path,
        path: &Path,
        crash: Option<&StoreCrashHooks>,
        created: &mut bool,
    ) -> Result<(), ApiError> {
        let internal = |detail: String| ApiError::new(ApiErrorCode::Internal, detail);
        let tmp_display = tmp_path.display();
        let path_display = path.display();
        let consult = |point: StoreSavePoint| -> Result<(), ApiError> {
            if let Some(crash) = crash {
                // The kill seam first (it never returns), then the
                // rig-only injected error, which makes the save return
                // a typed error instead of dying.
                crash.consult(STORE_LVM_STATE, point);
                if crash.take_save_failure(STORE_LVM_STATE, point) {
                    return Err(internal(format!(
                        "injected state save failure for {STORE_LVM_STATE} at {point:?}"
                    )));
                }
            }
            Ok(())
        };
        let mut file = create_owner_only(tmp_path)
            .map_err(|e| internal(format!("failed to create {tmp_display}: {e}")))?;
        // The unique temp name did not exist (create_new); this call now
        // owns it until the rename consumes it.
        *created = true;
        file.write_all(data)
            .map_err(|e| internal(format!("failed to write {tmp_display}: {e}")))?;
        consult(StoreSavePoint::AfterTmpWrite)?;
        file.sync_all()
            .map_err(|e| internal(format!("failed to fsync {tmp_display}: {e}")))?;
        drop(file);
        consult(StoreSavePoint::AfterFsyncBeforeRename)?;
        fs::rename(tmp_path, path).map_err(|e| {
            internal(format!(
                "failed to rename {tmp_display} to {path_display}: {e}"
            ))
        })?;
        // The rename consumed the temp; it is now the target file and
        // must never be cleaned up as a temp on a later error.
        *created = false;
        consult(StoreSavePoint::AfterRename)?;
        // The directory-fsync boundary: the rename has landed, so a
        // rig-injected failure here leaves the NEW file as the durable
        // state while still exercising the caller's error path.
        consult(StoreSavePoint::AfterRenameBeforeDirFsync)?;
        // fsync the directory so the rename itself is durable.
        let dir = fs::File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
        .map_err(|e| internal(format!("failed to open parent of {path_display}: {e}")))?;
        dir.sync_all()
            .map_err(|e| internal(format!("failed to fsync parent of {path_display}: {e}")))?;
        Ok(())
    }

    /// All stored volumes, ordered by volume identity.
    #[must_use]
    pub fn volumes(&self) -> &BTreeMap<VolumeId, StoredVolume> {
        &self.volumes
    }

    /// Look up one stored volume.
    #[must_use]
    pub fn volume(&self, id: &VolumeId) -> Option<&StoredVolume> {
        self.volumes.get(id)
    }

    /// Look up one stored volume for mutation.
    pub fn volume_mut(&mut self, id: &VolumeId) -> Option<&mut StoredVolume> {
        self.volumes.get_mut(id)
    }

    /// Insert or replace a stored volume.
    pub fn insert_volume(&mut self, id: VolumeId, volume: StoredVolume) -> Option<StoredVolume> {
        self.volumes.insert(id, volume)
    }

    /// Remove a stored volume.
    pub fn remove_volume(&mut self, id: &VolumeId) -> Option<StoredVolume> {
        self.volumes.remove(id)
    }

    /// Iterate all stored volumes mutably.
    pub fn volumes_mut(&mut self) -> impl Iterator<Item = &mut StoredVolume> {
        self.volumes.values_mut()
    }

    /// All claimed devices, ordered by device identity.
    #[must_use]
    pub fn devices(&self) -> &BTreeMap<DeviceId, DeviceEntry> {
        &self.devices
    }

    /// Look up one claimed device.
    #[must_use]
    pub fn device(&self, id: &DeviceId) -> Option<&DeviceEntry> {
        self.devices.get(id)
    }

    /// Insert or replace a claimed device.
    pub fn insert_device(&mut self, id: DeviceId, entry: DeviceEntry) -> Option<DeviceEntry> {
        self.devices.insert(id, entry)
    }

    /// Remove a claimed device.
    pub fn remove_device(&mut self, id: &DeviceId) -> Option<DeviceEntry> {
        self.devices.remove(id)
    }

    /// All online-move records, ordered by volume identity.
    #[must_use]
    pub fn moves(&self) -> &BTreeMap<VolumeId, MoveRecord> {
        &self.moves
    }

    /// Look up one volume's move record.
    #[must_use]
    pub fn move_record(&self, id: &VolumeId) -> Option<&MoveRecord> {
        self.moves.get(id)
    }

    /// Look up one volume's move record for mutation.
    pub fn move_record_mut(&mut self, id: &VolumeId) -> Option<&mut MoveRecord> {
        self.moves.get_mut(id)
    }

    /// Insert or replace one volume's move record.
    pub fn insert_move(&mut self, id: VolumeId, record: MoveRecord) -> Option<MoveRecord> {
        self.moves.insert(id, record)
    }

    /// Remove one volume's move record (a failed start with a
    /// verified-untouched world leaves nothing to reconcile).
    pub fn remove_move(&mut self, id: &VolumeId) -> Option<MoveRecord> {
        self.moves.remove(id)
    }

    /// All pending create intents, ordered by volume identity.
    #[must_use]
    pub fn create_intents(&self) -> &BTreeMap<VolumeId, CreateIntent> {
        &self.pending_creates
    }

    /// Look up one volume's pending create intent.
    #[must_use]
    pub fn create_intent(&self, id: &VolumeId) -> Option<&CreateIntent> {
        self.pending_creates.get(id)
    }

    /// Insert or replace one volume's pending create intent.
    pub fn insert_create_intent(
        &mut self,
        id: VolumeId,
        intent: CreateIntent,
    ) -> Option<CreateIntent> {
        self.pending_creates.insert(id, intent)
    }

    /// Remove one volume's pending create intent.
    pub fn remove_create_intent(&mut self, id: &VolumeId) -> Option<CreateIntent> {
        self.pending_creates.remove(id)
    }

    /// All pending grow intents, ordered by volume identity.
    #[must_use]
    pub fn grow_intents(&self) -> &BTreeMap<VolumeId, GrowIntent> {
        &self.pending_grows
    }

    /// Look up one volume's pending grow intent.
    #[must_use]
    pub fn grow_intent(&self, id: &VolumeId) -> Option<&GrowIntent> {
        self.pending_grows.get(id)
    }

    /// Insert or replace one volume's pending grow intent.
    pub fn insert_grow_intent(&mut self, id: VolumeId, intent: GrowIntent) -> Option<GrowIntent> {
        self.pending_grows.insert(id, intent)
    }

    /// Remove one volume's pending grow intent.
    pub fn remove_grow_intent(&mut self, id: &VolumeId) -> Option<GrowIntent> {
        self.pending_grows.remove(id)
    }

    /// All pending delete intents, ordered by volume identity.
    #[must_use]
    pub fn delete_intents(&self) -> &BTreeMap<VolumeId, DeleteIntent> {
        &self.pending_deletes
    }

    /// Look up one volume's pending delete intent.
    #[must_use]
    pub fn delete_intent(&self, id: &VolumeId) -> Option<&DeleteIntent> {
        self.pending_deletes.get(id)
    }

    /// Insert or replace one volume's pending delete intent.
    pub fn insert_delete_intent(
        &mut self,
        id: VolumeId,
        intent: DeleteIntent,
    ) -> Option<DeleteIntent> {
        self.pending_deletes.insert(id, intent)
    }

    /// Remove one volume's pending delete intent.
    pub fn remove_delete_intent(&mut self, id: &VolumeId) -> Option<DeleteIntent> {
        self.pending_deletes.remove(id)
    }
}

/// The process-wide sequence for [`unique_sibling_tmp_path`].
///
/// The process id makes the name unique across live processes; the
/// atomic sequence makes it unique across every write in this process,
/// including concurrent writers. A stale file from a previous process
/// with a recycled pid is removed by the startup sweep before this
/// process writes.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Bounded prefix of this implementation's temp names.
const TEMP_PREFIX: &str = ".volvisor-";

/// Suffix shared by the bounded namespace and the legacy fixed name.
const TEMP_SUFFIX: &str = ".tmp";

/// Number of lowercase hex digits in the canonical path-identity hash.
const TEMP_HASH_HEX_LEN: usize = 16;

/// A foreign process's temp must be at least this old before the sweep
/// assumes it is a crash leftover rather than an active writer.
const FOREIGN_TEMP_GRACE: Duration = Duration::from_secs(60 * 60);

/// Stable FNV-1a 64-bit hash. The algorithm is part of the on-disk temp
/// namespace, so it must not change casually.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3_u64);
    }
    hash
}

/// Hash of the canonical state-path identity bytes: raw `OsStr` bytes on
/// unix, lossy UTF-8 bytes elsewhere.
///
/// The identity is [`canonical_state_identity`] (the canonicalized parent
/// plus the file name), so a restart that spells the same file through a
/// symlink, `..` or `./` still recognizes its predecessor's temp files.
#[cfg(unix)]
fn state_hash(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    fnv1a64(canonical_state_identity(path).as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn state_hash(path: &Path) -> u64 {
    fnv1a64(canonical_state_identity(path).to_string_lossy().as_bytes())
}

/// One parsed temp name in this implementation's bounded namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TempName {
    hash: u64,
    pid: u64,
    sequence: u64,
}

fn decimal_component(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Parse the bounded namespace strictly:
/// `.volvisor-<16 lowercase hex>.<decimal pid>.<decimal sequence>.tmp`.
///
/// The hash must be exactly 16 lowercase hex digits, there must be
/// exactly three dot-separated components, and both numeric components
/// must be non-empty decimal. Anything else is not ours.
fn parse_temp_name(name: &str) -> Option<TempName> {
    let core = name.strip_prefix(TEMP_PREFIX)?.strip_suffix(TEMP_SUFFIX)?;
    let mut components = core.split('.');
    let (Some(hash), Some(pid), Some(sequence), None) = (
        components.next(),
        components.next(),
        components.next(),
        components.next(),
    ) else {
        return None;
    };
    if hash.len() != TEMP_HASH_HEX_LEN
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    Some(TempName {
        hash: u64::from_str_radix(hash, 16).ok()?,
        pid: decimal_component(pid)?,
        sequence: decimal_component(sequence)?,
    })
}

/// A fresh, collision-free temp path for one physical save of `path`.
///
/// The bounded name is
/// `.volvisor-<16-lowercase-hex-fnv1a64>.<pid>.<sequence>.tmp`, built
/// from a stable hash of this state path's canonical identity plus the
/// process id and a process-wide atomic sequence. The name never grows
/// with the state file name, so a long state name cannot exceed
/// `NAME_MAX`, and no other writer can receive the same name.
pub(crate) fn unique_sibling_tmp_path(path: &Path) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        "{TEMP_PREFIX}{:016x}.{}.{sequence}{TEMP_SUFFIX}",
        state_hash(path),
        std::process::id()
    );
    state_parent(path).join(name)
}

/// The parent directory a state path's temp files live in.
fn state_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Canonical identity used by the process-wide writer-gate registry:
/// canonicalize the parent when possible, then append the file name;
/// fall back to the given path.
pub(crate) fn canonical_state_identity(path: &Path) -> PathBuf {
    let Some(file_name) = path.file_name() else {
        return path.to_path_buf();
    };
    state_parent(path)
        .canonicalize()
        .map_or_else(|_| path.to_path_buf(), |parent| parent.join(file_name))
}

fn in_flight_temps() -> &'static Mutex<BTreeSet<PathBuf>> {
    static IN_FLIGHT: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(|| Mutex::new(BTreeSet::new()))
}

fn lock_in_flight() -> MutexGuard<'static, BTreeSet<PathBuf>> {
    in_flight_temps()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// RAII registration for one in-flight temp file.
///
/// Registered before `create_new` and removed on drop, including on
/// panic/unwind, so the startup sweep can never remove a temp this
/// process is still writing.
struct TempRegistration {
    identity: PathBuf,
}

impl TempRegistration {
    fn register(path: &Path) -> Self {
        let identity = canonical_state_identity(path);
        lock_in_flight().insert(identity.clone());
        Self { identity }
    }
}

impl Drop for TempRegistration {
    fn drop(&mut self) {
        lock_in_flight().remove(&self.identity);
    }
}

fn temp_is_registered(path: &Path) -> bool {
    lock_in_flight().contains(&canonical_state_identity(path))
}

/// Whether `candidate` is the exact legacy `<state>.tmp` name of
/// `path`.
///
/// Compares constructed `OsStr` values, so a non-UTF-8 state file name
/// still has a sweepable legacy temp.
fn is_legacy_temp_name(path: &Path, candidate: &std::ffi::OsStr) -> bool {
    let Some(state_file_name) = path.file_name() else {
        return false;
    };
    let mut legacy = state_file_name.to_os_string();
    legacy.push(TEMP_SUFFIX);
    candidate == legacy
}

/// Whether `candidate` is provably a temp file of the state at `path`.
///
/// Accepts only the exact legacy `<state>.tmp` name (this build never
/// writes it) and this state's hash in the bounded new namespace.
/// Anything else — another state's hash, a malformed hash, a
/// non-numeric middle, extra components, or a different suffix — is
/// not ours and is never removed.
fn is_stale_temp_name(path: &Path, state_hash: u64, candidate: &std::ffi::OsStr) -> bool {
    if is_legacy_temp_name(path, candidate) {
        return true;
    }
    candidate
        .to_str()
        .and_then(parse_temp_name)
        .is_some_and(|temp| temp.hash == state_hash)
}

/// Every temp path in `path`'s namespace in its own parent directory,
/// sorted for deterministic callers/tests.
///
/// The scan is deliberately shallow (one directory, no recursion) and
/// name-gated by [`is_stale_temp_name`], so it never deletes an
/// arbitrary file. This is the candidate list; [`sweep_stale_temps`]
/// applies the live-writer policy before removing anything.
pub(crate) fn stale_temp_paths(path: &Path) -> Vec<PathBuf> {
    let own_hash = state_hash(path);
    let Ok(entries) = fs::read_dir(state_parent(path)) else {
        return Vec::new();
    };
    let mut stale: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| is_stale_temp_name(path, own_hash, &entry.file_name()))
        .map(|entry| entry.path())
        .collect();
    stale.sort();
    stale
}

/// Whether one candidate temp may be removed without touching a live
/// writer.
///
/// A temp registered as in flight is never removed. A same-pid
/// unregistered temp is pid reuse and is removed immediately. A
/// foreign-pid temp and a regular legacy `<state>.tmp` (which has no pid
/// component, so it is treated exactly like a foreign writer) are removed
/// only after [`FOREIGN_TEMP_GRACE`]. A non-regular entry is skipped.
fn temp_may_be_swept(candidate: &Path, current_pid: u64, now: SystemTime) -> bool {
    if temp_is_registered(candidate) {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(candidate) else {
        return false;
    };
    // A directory or other non-regular entry that merely looks like a
    // temp is never ours to delete.
    if !metadata.file_type().is_file() {
        return false;
    }
    // `stale_temp_paths` already proved the name is ours. A `None`
    // parse here is the legacy fixed name, which may be non-UTF-8.
    let parsed = candidate
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .and_then(parse_temp_name);
    match parsed {
        Some(temp) if temp.pid == current_pid => {
            // Same pid but not registered: this incarnation cannot be
            // writing it, so it is pid reuse or a lost registration.
            true
        }
        // A foreign-pid temp or the legacy fixed name: a live writer in
        // another process (or an older binary during a rolling restart /
        // an operator's copy) may have just created it. Only a file
        // older than the grace is presumed stale.
        Some(_) | None => metadata.modified().is_ok_and(|modified| {
            now.duration_since(modified)
                .is_ok_and(|age| age > FOREIGN_TEMP_GRACE)
        }),
    }
}

/// Remove stale sibling temp files left by a crashed save of `path`.
///
/// Best-effort by design: a state directory that cannot be read or a
/// temp entry that cannot be removed is skipped, never an arbitrary
/// recursive delete and never a startup failure. Returns the number of
/// files removed.
///
/// The provider constructor calls this before loading or writing state.
/// Active temps registered by this process are never removed; a
/// same-pid unregistered temp is removed regardless of age (pid reuse);
/// a foreign-pid temp and a regular legacy `<state>.tmp` (no pid
/// component, treated like a foreign writer) are removed only after
/// [`FOREIGN_TEMP_GRACE`]; a non-regular entry is skipped. Cross-process
/// exclusion is the journal flock, not this sweep: two daemons must not
/// alias one state path.
pub(crate) fn sweep_stale_temps(path: &Path) -> usize {
    let current_pid = u64::from(std::process::id());
    let now = SystemTime::now();
    stale_temp_paths(path)
        .into_iter()
        .filter(|candidate| temp_may_be_swept(candidate, current_pid, now))
        .filter(|candidate| fs::remove_file(candidate).is_ok())
        .count()
}

/// Create `path` for writing with owner-only permissions.
///
/// The call uses `create_new`, so it never truncates an existing file:
/// the caller's unique temp name is its own, and a pre-existing file at
/// that name is not silently adopted or removed. On unix the file is
/// created with mode `0600`; other platforms use the platform default
/// creation mode.
fn create_owner_only(path: &Path) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use volvisor_types::crash::CRASH_PANIC_PREFIX;

    const N1_FIXTURE: &str = include_str!("../../../tests/fixtures/schema/lvm-state-v1-n1.json");
    const N2_FIXTURE: &str = include_str!("../../../tests/fixtures/schema/lvm-state-v1-n2.json");
    const TOMBSTONE_FIXTURE: &str =
        include_str!("../../../tests/fixtures/schema/lvm-state-v1-tombstones.json");
    /// The last released v1 shape (written by the pre-#70 binary at
    /// `ee739bc`) used as the N-1 fixture for schema v2.
    const V2_N1_FIXTURE: &str = include_str!("../../../tests/fixtures/schema/lvm-state-v2-n1.json");

    fn write_fixture(dir: &tempfile::TempDir, name: &str, text: &str) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, text).expect("write fixture");
        path
    }

    fn sample_state() -> LvmState {
        let volume_id = VolumeId::new("vol-1").expect("valid id");
        let project_id = ProjectId::new("tenant-a").expect("valid id");
        let device_id = DeviceId::new("dev-0123456789abcdef").expect("valid id");
        let mut state = LvmState::default();
        state.insert_volume(
            volume_id,
            StoredVolume {
                entry: VolumeEntry {
                    vg_name: "vg-1".to_owned(),
                    lv_name: "vol-1".to_owned(),
                    size_bytes: 1024,
                    requested_size_bytes: 512,
                    generation: 3,
                    data_epoch: 1,
                    project_id,
                    block_size: 4096,
                    creation_payload: "{\"class\":\"native-local\"}".to_owned(),
                },
                runtime: VolumeRuntime {
                    state: VolumeLifecycle::Ready,
                    retired_attachments: RetiredAttachmentFilter::default(),
                    attachment: Some(AttachmentRecord {
                        id: AttachmentId::new("att-1").expect("valid id"),
                        vm_id: "vm-1".to_owned(),
                        host_id: HostId::new("host-1").expect("valid id"),
                        generation: 1,
                        access_mode: AccessMode::SingleWriter,
                        vmm_disk_id: None,
                        state: volvisor_types::AttachmentState::Prepared,
                        state_revision: volvisor_types::domain::initial_state_revision(),
                        evidence: None,
                        reporter: None,
                        recorded_at_ms: None,
                    }),
                },
            },
        );
        state.insert_device(
            device_id,
            DeviceEntry {
                stable_identity: "wwn-0xabc".to_owned(),
                path: "/dev/disk/by-id/wwn-0xabc".to_owned(),
                vg_name: "vg-1".to_owned(),
                role: DeviceRole::NativePool,
                owner_generation: 1,
            },
        );
        state
    }

    #[test]
    fn load_missing_file_is_empty_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nonexistent.json");
        let state = LvmState::load(&path).expect("empty state");
        assert_eq!(state, LvmState::default());
    }

    #[test]
    fn empty_state_serializes_without_intent_maps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let mut state = LvmState::default();
        state.save(&path).expect("save");
        let text = fs::read_to_string(&path).expect("read state");
        assert!(
            text.contains("\"schema_version\":3"),
            "the current schema version is stamped: {text}"
        );
        assert!(
            !text.contains("pending_creates"),
            "an empty state must not gain a create-intent map: {text}"
        );
        assert!(
            !text.contains("retired_attachment_filter"),
            "an empty state must not gain a retired-id filter: {text}"
        );
        assert!(
            !text.contains("pending_grows"),
            "an empty state must not gain a grow-intent map: {text}"
        );
        assert!(
            !text.contains("pending_deletes"),
            "an empty state must not gain a delete-intent map: {text}"
        );
        assert_eq!(LvmState::load(&path).expect("reload"), state);
    }

    #[test]
    fn intent_maps_round_trip_losslessly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let volume_id = VolumeId::new("vol-intent").expect("valid id");
        let mut state = sample_state();
        state.insert_create_intent(
            volume_id.clone(),
            CreateIntent {
                vg_name: "vg-1".to_owned(),
                lv_name: "vol-intent".to_owned(),
                requested_size_bytes: 2048,
                project_id: ProjectId::new("tenant-a").expect("valid id"),
                block_size: 4096,
                creation_payload: "{\"class\":\"native-local\"}".to_owned(),
            },
        );
        state.insert_grow_intent(
            volume_id.clone(),
            GrowIntent {
                operation_id: OperationId::new("op-grow").expect("valid id"),
                from_size_bytes: 1024,
                requested_size_bytes: 4096,
                expected_generation: 3,
            },
        );
        state.insert_delete_intent(
            volume_id.clone(),
            DeleteIntent {
                operation_id: OperationId::new("op-delete").expect("valid id"),
                expected_generation: 3,
                erasure_policy: ErasurePolicy::ZeroDiscard,
            },
        );
        state.save(&path).expect("save");
        let loaded = LvmState::load(&path).expect("load");
        assert_eq!(loaded, state);
        assert_eq!(
            loaded
                .create_intent(&volume_id)
                .expect("create intent")
                .requested_size_bytes,
            2048
        );
        assert_eq!(
            loaded
                .grow_intent(&volume_id)
                .expect("grow intent")
                .expected_generation,
            3
        );
        assert_eq!(
            loaded
                .delete_intent(&volume_id)
                .expect("delete intent")
                .erasure_policy,
            ErasurePolicy::ZeroDiscard
        );

        // Removing every intent restores the compact shape.
        state.remove_create_intent(&volume_id);
        state.remove_grow_intent(&volume_id);
        state.remove_delete_intent(&volume_id);
        state.save(&path).expect("save cleared");
        let text = fs::read_to_string(&path).expect("read state");
        assert!(!text.contains("pending_creates"), "{text}");
        assert!(!text.contains("pending_grows"), "{text}");
        assert!(!text.contains("pending_deletes"), "{text}");
    }

    #[test]
    fn save_load_round_trip_is_lossless() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let mut state = sample_state();
        state.save(&path).expect("save");
        let loaded = LvmState::load(&path).expect("load");
        assert_eq!(loaded, state);
    }

    #[test]
    fn save_is_atomic_and_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let mut state = sample_state();
        state.save(&path).expect("save");
        assert!(path.exists());
        assert!(
            stale_temp_paths(&path).is_empty(),
            "no temp-file residue after a successful save"
        );

        // Overwriting an existing state also leaves no residue.
        state.save(&path).expect("save again");
        assert_eq!(stale_temp_paths(&path), Vec::<PathBuf>::new());
        let reloaded = LvmState::load(&path).expect("reload");
        assert_eq!(reloaded, state);
    }

    #[test]
    fn unique_tmp_names_do_not_collide_across_writers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");

        // Two independent writers in the same process must never receive
        // the same temp name, even when they generate concurrently.
        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                (0..64)
                    .map(|_| unique_sibling_tmp_path(&path))
                    .collect::<Vec<_>>()
            }));
        }
        let names: BTreeSet<PathBuf> = handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("writer thread"))
            .collect();
        assert_eq!(names.len(), 128, "every generated temp name is unique");
        for name in names {
            let file_name = name
                .file_name()
                .and_then(|name| name.to_str())
                .expect("name");
            let parsed =
                parse_temp_name(file_name).expect("generated name is in the bounded namespace");
            assert_eq!(
                parsed.hash,
                state_hash(&path),
                "the generated name carries this state path's hash"
            );
        }
    }

    #[test]
    fn temp_hash_follows_the_canonical_parent_spelling() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let dot = dir.path().join(".").join("state.json");
        let child = dir.path().join("child");
        fs::create_dir(&child).expect("child directory");
        let dotdot = child.join("..").join("state.json");

        assert_eq!(state_hash(&path), state_hash(&dot));
        assert_eq!(state_hash(&path), state_hash(&dotdot));
        let temp = unique_sibling_tmp_path(&dotdot);
        let parsed = temp
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(parse_temp_name)
            .expect("generated temp is in the bounded namespace");
        assert_eq!(
            parsed.hash,
            state_hash(&path),
            "a differently spelled parent carries the same canonical hash"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_parent_shares_the_temp_namespace_and_a_leftover_is_swept() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let real_parent = dir.path().join("real");
        fs::create_dir(&real_parent).expect("real parent");
        let link_parent = dir.path().join("link");
        symlink(&real_parent, &link_parent).expect("symlinked parent");

        let real = real_parent.join("state.json");
        let via_link = link_parent.join("state.json");
        assert_eq!(
            state_hash(&real),
            state_hash(&via_link),
            "a symlinked parent spelling hashes to the real parent's identity"
        );

        let leftover = temp_path_for(&via_link, u64::from(std::process::id()), 1);
        fs::write(&leftover, b"stale through the symlink").expect("stale via symlink");
        assert_eq!(
            sweep_stale_temps(&real),
            1,
            "the real spelling sweeps a leftover named through the symlink"
        );
        assert!(
            !real_parent
                .join(leftover.file_name().expect("temp name"))
                .exists(),
            "the leftover is gone from the real directory"
        );
    }

    #[test]
    fn temp_name_stays_bounded_for_a_long_state_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(format!("{}.json", "s".repeat(240)));
        let temp = unique_sibling_tmp_path(&path);
        let name = temp
            .file_name()
            .and_then(|name| name.to_str())
            .expect("name");
        assert!(
            name.len() < 128,
            "the temp name must not grow with the state file name: {name}"
        );
        assert!(name.len() < 255, "NAME_MAX safety: {name}");
    }

    fn temp_path_for(path: &Path, pid: u64, sequence: u64) -> PathBuf {
        state_parent(path).join(format!(
            "{TEMP_PREFIX}{:016x}.{pid}.{sequence}{TEMP_SUFFIX}",
            state_hash(path)
        ))
    }

    fn age_file(path: &Path, age: Duration) {
        use std::fs::FileTimes;

        let modified = SystemTime::now()
            .checked_sub(age)
            .expect("test age fits the platform clock");
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open file to age")
            .set_times(FileTimes::new().set_modified(modified))
            .expect("age the file");
    }

    #[test]
    fn startup_sweep_removes_only_this_state_paths_stale_temps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");

        let first = unique_sibling_tmp_path(&path);
        let second = unique_sibling_tmp_path(&path);
        fs::write(&first, b"stale").expect("first stale temp");
        fs::write(&second, b"stale").expect("second stale temp");
        let legacy = dir.path().join("state.json.tmp");
        fs::write(&legacy, b"legacy stale").expect("legacy temp");
        age_file(&legacy, FOREIGN_TEMP_GRACE + Duration::from_secs(1));

        // Files that are *not* provably this state's temp namespace.
        let foreign = dir.path().join("other.json.123.456.tmp");
        fs::write(&foreign, b"foreign").expect("foreign file");
        let foreign_hash = state_parent(&path).join(format!(
            "{TEMP_PREFIX}{:016x}.123.456{TEMP_SUFFIX}",
            state_hash(&path) ^ 1
        ));
        fs::write(&foreign_hash, b"other state's hashed temp").expect("foreign hash temp");
        let malformed = dir.path().join("state.json.notnumeric.tmp");
        fs::write(&malformed, b"not ours").expect("malformed file");
        let wrong_suffix = dir.path().join("state.json.123.456.tmp.bak");
        fs::write(&wrong_suffix, b"wrong suffix").expect("wrong-suffix file");

        assert_eq!(sweep_stale_temps(&path), 3);
        assert!(!first.exists(), "the first unique temp is swept");
        assert!(!second.exists(), "the second unique temp is swept");
        assert!(!legacy.exists(), "the legacy fixed temp is swept");
        assert!(foreign.exists(), "another state file's temp is untouched");
        assert!(
            foreign_hash.exists(),
            "a different state hash is not this state's temp"
        );
        assert!(malformed.exists(), "a non-numeric middle is untouched");
        assert!(wrong_suffix.exists(), "a non-.tmp suffix is untouched");
    }

    #[test]
    fn registered_active_temp_survives_a_sweep() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let active = unique_sibling_tmp_path(&path);
        fs::write(&active, b"in-flight").expect("active temp");

        let registration = TempRegistration::register(&active);
        assert_eq!(
            sweep_stale_temps(&path),
            0,
            "the sweep must not remove a registered in-flight temp"
        );
        assert!(active.exists(), "the active temp survives");

        // Once the writer has unwound, the same-pid leftover is swept.
        drop(registration);
        assert_eq!(sweep_stale_temps(&path), 1);
        assert!(!active.exists(), "an unregistered same-pid temp is swept");
    }

    #[test]
    fn same_pid_unregistered_temp_is_swept_even_when_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let temp = unique_sibling_tmp_path(&path);
        fs::write(&temp, b"fresh leftover").expect("fresh temp");

        assert_eq!(sweep_stale_temps(&path), 1);
        assert!(
            !temp.exists(),
            "a same-pid unregistered temp is pid reuse, not a live writer"
        );
    }

    #[test]
    fn foreign_pid_temp_is_kept_until_the_grace_elapses() {
        use std::fs::FileTimes;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let foreign_pid = u64::from(std::process::id()) + 1;
        let fresh = temp_path_for(&path, foreign_pid, 1);
        let old = temp_path_for(&path, foreign_pid, 2);
        fs::write(&fresh, b"possibly live").expect("fresh foreign temp");
        fs::write(&old, b"stale foreign temp").expect("old foreign temp");
        let old_time = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        fs::OpenOptions::new()
            .write(true)
            .open(&old)
            .expect("open old foreign temp")
            .set_times(FileTimes::new().set_modified(old_time))
            .expect("age the foreign temp");

        assert_eq!(sweep_stale_temps(&path), 1);
        assert!(
            fresh.exists(),
            "a fresh foreign-pid temp may belong to a live writer"
        );
        assert!(
            !old.exists(),
            "a foreign-pid temp older than the grace is swept"
        );
    }

    #[test]
    fn future_mtime_temp_is_never_swept() {
        use std::fs::FileTimes;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let future = SystemTime::now() + Duration::from_secs(2 * 60 * 60);
        let foreign_pid = u64::from(std::process::id()) + 1;
        let hashed = temp_path_for(&path, foreign_pid, 1);
        let legacy = dir.path().join("state.json.tmp");
        fs::write(&hashed, b"future foreign temp").expect("future hashed temp");
        fs::write(&legacy, b"future legacy temp").expect("future legacy temp");
        for candidate in [&hashed, &legacy] {
            fs::OpenOptions::new()
                .write(true)
                .open(candidate)
                .expect("open future temp")
                .set_times(FileTimes::new().set_modified(future))
                .expect("set future mtime");
        }

        assert_eq!(
            sweep_stale_temps(&path),
            0,
            "a future mtime is treated as not provably stale and is never swept"
        );
        assert!(hashed.exists(), "a future hashed temp is never swept");
        assert!(legacy.exists(), "a future legacy temp is never swept");
    }

    #[test]
    fn same_pid_future_temp_is_still_swept() {
        use std::fs::FileTimes;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let temp = temp_path_for(&path, u64::from(std::process::id()), 1);
        fs::write(&temp, b"future same-pid temp").expect("future same-pid temp");
        let future = SystemTime::now() + Duration::from_secs(2 * 60 * 60);
        fs::OpenOptions::new()
            .write(true)
            .open(&temp)
            .expect("open future same-pid temp")
            .set_times(FileTimes::new().set_modified(future))
            .expect("set future mtime");

        assert_eq!(
            sweep_stale_temps(&path),
            1,
            "same-pid is pid reuse and is swept regardless of age"
        );
        assert!(!temp.exists(), "the future same-pid temp is swept");
    }

    #[test]
    fn fresh_legacy_temp_survives_a_sweep() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let legacy = dir.path().join("state.json.tmp");
        fs::write(&legacy, b"possibly an old binary or an operator copy")
            .expect("fresh legacy temp");

        assert_eq!(
            sweep_stale_temps(&path),
            0,
            "a fresh legacy temp has no pid component and may belong to a live old binary"
        );
        assert!(
            legacy.exists(),
            "the fresh legacy temp must not be removed before the grace"
        );
    }

    #[test]
    fn directory_shaped_legacy_temp_survives_a_sweep() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let legacy = dir.path().join("state.json.tmp");
        fs::create_dir(&legacy).expect("directory-shaped legacy temp");

        assert_eq!(
            sweep_stale_temps(&path),
            0,
            "a non-regular entry is never removed"
        );
        assert!(
            legacy.is_dir(),
            "the directory survives the best-effort sweep"
        );
    }

    #[test]
    fn create_owner_only_refuses_an_existing_file_without_truncating() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("existing.tmp");
        fs::write(&path, b"keep me").expect("pre-existing file");

        let error = create_owner_only(&path).expect_err("create_new must refuse an existing file");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(&path).expect("existing content"),
            b"keep me",
            "refusal must not truncate the existing file"
        );
    }

    #[test]
    fn create_owner_only_reports_a_missing_parent_without_creating_anything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing_parent = dir.path().join("missing");
        let path = missing_parent.join("state.tmp");

        let error =
            create_owner_only(&path).expect_err("a missing parent must fail the temp create");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "a missing parent is a real pre-rename I/O failure: {error}"
        );
        assert!(
            !path.exists(),
            "the failed create must not leave a temp file"
        );
        assert!(
            !missing_parent.exists(),
            "the failed create must not create the parent"
        );
    }

    #[test]
    fn create_owner_only_refuses_a_directory_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("directory-shaped.tmp");
        fs::create_dir(&path).expect("directory target");

        let error = create_owner_only(&path).expect_err("a directory target must fail the create");
        #[cfg(unix)]
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::IsADirectory | std::io::ErrorKind::AlreadyExists
            ),
            "the Unix create error is EISDIR or EEXIST: {error}"
        );
        #[cfg(not(unix))]
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::AlreadyExists
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::Other
            ),
            "the platform reports the directory target as an existing/permission error: {error}"
        );
        assert!(path.is_dir(), "the directory target must not be modified");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_state_path_temp_is_swept() {
        use std::os::unix::ffi::OsStringExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let state_name = std::ffi::OsString::from_vec(b"state-\xff.json".to_vec());
        let path = dir.path().join(state_name);
        let temp = unique_sibling_tmp_path(&path);
        fs::write(&temp, b"stale").expect("non-UTF-8 state temp");

        let mut legacy_name = path.file_name().expect("file name").to_os_string();
        legacy_name.push(".tmp");
        let legacy = dir.path().join(legacy_name);
        fs::write(&legacy, b"legacy").expect("non-UTF-8 legacy temp");
        age_file(&legacy, FOREIGN_TEMP_GRACE + Duration::from_secs(1));

        let candidates = stale_temp_paths(&path);
        assert!(
            candidates.contains(&temp),
            "hashed temp missing from candidates: {candidates:?}"
        );
        assert!(
            candidates.contains(&legacy),
            "legacy temp missing from candidates: {candidates:?}"
        );
        assert_eq!(sweep_stale_temps(&path), 2);
        assert!(!temp.exists(), "the hashed temp is swept");
        assert!(!legacy.exists(), "the legacy temp is swept");
    }

    #[test]
    fn a_failed_save_does_not_remove_another_writers_temp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let other_writer = unique_sibling_tmp_path(&path);
        fs::write(&other_writer, b"another writer's in-flight temp").expect("foreign temp");

        let hooks = StoreCrashHooks::new();
        hooks.arm_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterTmpWrite, 0);
        let mut state = sample_state();
        let err = state
            .save_with_hooks(&path, Some(&hooks))
            .expect_err("the armed save must fail");
        assert_eq!(err.code, ApiErrorCode::Internal);

        assert!(
            other_writer.exists(),
            "a failed save removes only its own uniquely-named temp"
        );
        assert_eq!(
            fs::read(&other_writer).expect("foreign temp content"),
            b"another writer's in-flight temp"
        );
        assert_eq!(
            stale_temp_paths(&path),
            vec![other_writer],
            "only the failed call's own temp was cleaned up"
        );
    }

    #[test]
    fn save_with_hooks_maps_a_missing_temp_parent_to_internal_without_cleanup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing_parent = dir.path().join("missing");
        let path = missing_parent.join("state.json");
        let mut state = sample_state();
        let entries_before: BTreeSet<PathBuf> = fs::read_dir(dir.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();

        let error = state
            .save_with_hooks(&path, Some(&StoreCrashHooks::new()))
            .expect_err("a missing parent must fail the temp create before the write");

        assert_eq!(
            error.code,
            ApiErrorCode::Internal,
            "a pre-rename create failure is a typed INTERNAL: {error}"
        );
        assert!(
            error.detail.contains("failed to create"),
            "the error names the failed temp create: {error}"
        );
        assert!(
            !missing_parent.exists(),
            "the failed create must not create the parent or leave a temp there"
        );
        assert!(
            !path.exists(),
            "the failed create must not leave a state file"
        );
        let entries_after: BTreeSet<PathBuf> = fs::read_dir(dir.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        assert_eq!(
            entries_after, entries_before,
            "the failed temp create must leave no residue in the state parent's parent directory"
        );
    }

    #[test]
    fn injected_save_failure_after_rename_keeps_new_state_and_before_rename_keeps_old() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let volume_id = VolumeId::new("vol-1").expect("valid id");
        let hooks = StoreCrashHooks::new();
        let mut state = sample_state();
        state.save(&path).expect("initial save");

        // Fail at the directory-fsync boundary, after the rename has
        // landed: the save reports the injected error, but a reload
        // must read the NEW state (the rename is the commit).
        state
            .volume_mut(&volume_id)
            .expect("volume")
            .entry
            .generation = 4;
        hooks.arm_save_failure(
            STORE_LVM_STATE,
            StoreSavePoint::AfterRenameBeforeDirFsync,
            0,
        );
        let err = state
            .save_with_hooks(&path, Some(&hooks))
            .expect_err("the armed save must fail");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("injected state save failure"), "{err}");
        assert!(err.detail.contains("AfterRenameBeforeDirFsync"), "{err}");
        assert!(
            stale_temp_paths(&path).is_empty(),
            "no temp-file residue after the injected failure"
        );
        let loaded = LvmState::load(&path).expect("load the post-rename state");
        assert_eq!(
            loaded.volume(&volume_id).expect("volume").entry.generation,
            4,
            "the rename landed before the injected failure"
        );

        // A pre-rename failure leaves the OLD durable state readable.
        state
            .volume_mut(&volume_id)
            .expect("volume")
            .entry
            .generation = 5;
        hooks.arm_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterFsyncBeforeRename, 0);
        let err = state
            .save_with_hooks(&path, Some(&hooks))
            .expect_err("the armed pre-rename save must fail");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("injected state save failure"), "{err}");
        assert!(
            stale_temp_paths(&path).is_empty(),
            "the tmp cleanup removes the unrenamed file"
        );
        let loaded = LvmState::load(&path).expect("load the old state");
        assert_eq!(
            loaded.volume(&volume_id).expect("volume").entry.generation,
            4,
            "a pre-rename failure leaves the old file untouched"
        );
    }

    #[test]
    fn every_store_save_crash_seam_fires_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        // Phase D D3 slice 2: the bytes-only writer must keep consulting
        // all four save points in the established order.
        for point in [
            StoreSavePoint::AfterTmpWrite,
            StoreSavePoint::AfterFsyncBeforeRename,
            StoreSavePoint::AfterRename,
            StoreSavePoint::AfterRenameBeforeDirFsync,
        ] {
            let hooks = StoreCrashHooks::new();
            hooks.arm(STORE_LVM_STATE, point);
            let mut state = sample_state();
            let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = state.save_with_hooks(&path, Some(&hooks));
            }))
            .expect_err("an armed save point must fire");
            let payload = payload
                .downcast_ref::<String>()
                .expect("the crash payload is the marked string");
            assert!(
                payload.starts_with(CRASH_PANIC_PREFIX),
                "the seam at {point:?} must carry the filter prefix: {payload}"
            );
            // A killed save leaves its unique temp behind; clean it up so
            // the next seam starts from the same directory shape.
            for temp in stale_temp_paths(&path) {
                let _ = fs::remove_file(temp);
            }
        }
    }

    #[test]
    fn load_rejects_corrupt_state_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"{ not json").expect("write corrupt state");
        let err = LvmState::load(&path).expect_err("corrupt state must fail");
        assert_eq!(err.code, ApiErrorCode::Internal);
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_created_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let mut state = sample_state();
        state.save(&path).expect("save");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "state file must be owner-only");
        // An overwritten state keeps the restrictive mode as well.
        state.save(&path).expect("save again");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // A failed save never leaves a temp-file residue behind.
        assert_eq!(stale_temp_paths(&path), Vec::<PathBuf>::new());
    }

    // -----------------------------------------------------------------
    // Golden historical fixtures (real pre-tombstone / initial formats
    // from git history; see docs/schema-compatibility.md).
    // -----------------------------------------------------------------

    #[test]
    fn n_minus_one_fixture_loads_attachment_evidence_and_move_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(&dir, "lvm-state-v1-n1.json", N1_FIXTURE);
        let state = LvmState::load(&path).expect("N-1 state loads");
        let volume_id = VolumeId::new("vol-upgrade-n1").expect("valid id");
        let volume = state.volume(&volume_id).expect("volume preserved");
        assert_eq!(volume.runtime.state, VolumeLifecycle::Attached);
        assert_eq!(volume.entry.generation, 7);
        assert_eq!(volume.entry.data_epoch, 3);
        let attachment = volume.runtime.attachment.as_ref().expect("attachment");
        assert_eq!(attachment.vm_id, "vm-upgrade");
        assert_eq!(attachment.generation, 5);
        assert_eq!(attachment.vmm_disk_id.as_deref(), Some("disk-n1"));
        assert_eq!(attachment.state, AttachmentState::Active);
        assert_eq!(attachment.state_revision, 3);
        assert_eq!(
            attachment.evidence,
            Some(AttachmentEvidence::GuestConfirmed)
        );
        assert_eq!(attachment.reporter.as_deref(), Some("consumer-upgrade"));
        assert_eq!(attachment.recorded_at_ms, Some(1_700_000_000_123));
        assert!(
            volume.runtime.retired_attachments.is_empty(),
            "a pre-tombstone file has no retired ids"
        );

        let device_id = DeviceId::new("dev-upgrade01").expect("valid id");
        assert_eq!(
            state.device(&device_id).expect("device").owner_generation,
            3
        );
        let movement = state.move_record(&volume_id).expect("move preserved");
        assert_eq!(movement.state, MoveVolumeBackingState::InDoubt);
        assert_eq!(movement.target_pv, "/dev/disk/by-id/wwn-0x5000c500eeff0011");
    }

    #[test]
    fn n_minus_two_fixture_defaults_pre_report_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(&dir, "lvm-state-v1-n2.json", N2_FIXTURE);
        let state = LvmState::load(&path).expect("N-2 state loads");
        let volume_id = VolumeId::new("vol-upgrade-n2").expect("valid id");
        let volume = state.volume(&volume_id).expect("volume preserved");
        let attachment = volume.runtime.attachment.as_ref().expect("attachment");
        // Pre-report fields decode through their documented defaults.
        assert_eq!(attachment.state, AttachmentState::Prepared);
        assert_eq!(
            attachment.state_revision,
            volvisor_types::domain::initial_state_revision()
        );
        assert_eq!(attachment.vmm_disk_id, None);
        assert_eq!(attachment.evidence, None);
        assert_eq!(attachment.reporter, None);
        assert_eq!(attachment.recorded_at_ms, None);
        assert!(state.moves().is_empty(), "pre-P6-C fixture has no moves");
        assert!(
            state.create_intents().is_empty(),
            "a pre-intent fixture has no create intents"
        );
        assert!(
            state.grow_intents().is_empty(),
            "a pre-intent fixture has no grow intents"
        );
        assert!(
            state.delete_intents().is_empty(),
            "a pre-intent fixture has no delete intents"
        );
        assert!(
            volume.runtime.retired_attachments.is_empty(),
            "a pre-tombstone file has no retired ids"
        );
        // The initial format had only the effective size; the requested
        // size backfills from it rather than defaulting to zero.
        assert_eq!(volume.entry.requested_size_bytes, volume.entry.size_bytes);
    }

    #[test]
    fn tombstone_fixture_preserves_retired_attachment_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(&dir, "lvm-state-v1-tombstones.json", TOMBSTONE_FIXTURE);
        let state = LvmState::load(&path).expect("tombstone state loads");
        let volume_id = VolumeId::new("vol-upgrade-tombstone").expect("valid id");
        let volume = state.volume(&volume_id).expect("volume preserved");
        let filter = &volume.runtime.retired_attachments;
        assert!(
            filter.maybe_contains(&AttachmentId::new("att-retired-oldest").expect("id")),
            "the legacy vector was imported into the filter"
        );
        assert!(
            filter.maybe_contains(&AttachmentId::new("att-retired-newer").expect("id")),
            "the legacy vector was imported into the filter"
        );
        assert!(
            !filter.maybe_contains(&AttachmentId::new("att-never-used").expect("id")),
            "an unused id does not match"
        );
        assert_eq!(filter.inserted(), 2);
    }

    #[test]
    fn retired_attachment_filter_has_no_false_negatives_and_is_bounded() {
        let mut filter = RetiredAttachmentFilter::default();
        assert!(filter.is_empty());
        let ids: Vec<AttachmentId> = (0..2_000)
            .map(|index| AttachmentId::new(format!("att-{index}")).expect("id"))
            .collect();
        for id in &ids {
            filter.insert(id);
        }
        for id in &ids {
            assert!(filter.maybe_contains(id), "no false negatives: {id}");
        }
        assert_eq!(filter.inserted(), 2_000);
        assert!(filter.saturation() > 0.0 && filter.saturation() <= 1.0);
        // Re-inserting is idempotent in the bit array (only the
        // saturation counter advances).
        let before = filter.saturation();
        filter.insert(&ids[0]);
        assert_eq!(filter.saturation(), before);

        // The serialized form is a fixed-size hex string plus the
        // saturation counter, independent of how many ids were added.
        let json = serde_json::to_string(&filter).expect("serialize filter");
        assert!(json.len() < 3_000, "bounded on disk: {} bytes", json.len());
        let back: RetiredAttachmentFilter =
            serde_json::from_str(&json).expect("deserialize filter");
        assert_eq!(back, filter);
        for id in &ids {
            assert!(back.maybe_contains(id));
        }
    }

    #[test]
    fn malformed_retired_attachment_filter_is_rejected() {
        let short = r#"{"bits":"00","inserted":1}"#;
        assert!(serde_json::from_str::<RetiredAttachmentFilter>(short).is_err());
        let non_hex = format!(
            r#"{{"bits":"{}","inserted":1}}"#,
            "zz".repeat(RETIRED_ATTACHMENT_FILTER_BYTES)
        );
        assert!(serde_json::from_str::<RetiredAttachmentFilter>(&non_hex).is_err());
        let odd = format!(
            r#"{{"bits":"{}","inserted":1}}"#,
            "0".repeat(RETIRED_ATTACHMENT_FILTER_BYTES * 2 + 1)
        );
        assert!(serde_json::from_str::<RetiredAttachmentFilter>(&odd).is_err());
    }

    #[test]
    fn a_legacy_vector_is_merged_into_an_existing_filter_without_false_negatives() {
        // A hand-written or transitional file may carry both shapes. The
        // merge must be a superset: neither the filter's ids nor the
        // legacy vector's ids (duplicates included) may be lost.
        let mut filter = RetiredAttachmentFilter::default();
        let filtered = AttachmentId::new("att-already-filtered").expect("id");
        filter.insert(&filtered);
        let json = serde_json::json!({
            "state": "Ready",
            "attachment": null,
            "retired_attachment_ids": ["att-legacy", "att-legacy"],
            "retired_attachment_filter": filter,
        });
        let runtime: VolumeRuntime = serde_json::from_value(json).expect("both shapes decode");
        assert!(runtime.retired_attachments.maybe_contains(&filtered));
        assert!(
            runtime
                .retired_attachments
                .maybe_contains(&AttachmentId::new("att-legacy").expect("id"))
        );
        assert_eq!(runtime.retired_attachments.inserted(), 3);
    }

    #[test]
    fn a_noncanonical_filter_with_set_bits_is_never_treated_as_empty() {
        // Defense in depth: if a state file carries set bits but a zero
        // counter, omitting the filter on the next save would drop real
        // tombstones and create false negatives. It must be preserved.
        let bits = format!("01{}", "00".repeat(RETIRED_ATTACHMENT_FILTER_BYTES - 1));
        let json = format!(r#"{{"bits":"{bits}","inserted":0}}"#);
        let filter: RetiredAttachmentFilter =
            serde_json::from_str(&json).expect("noncanonical filter decodes");
        assert!(!filter.is_empty());
        let encoded = serde_json::to_string(&filter).expect("serialize");
        assert!(encoded.contains("\"inserted\":0"));
        assert!(encoded.len() > 2_000, "the bit array is preserved");
    }

    const V3_N1_FIXTURE: &str = include_str!("../../../tests/fixtures/schema/lvm-state-v3-n1.json");

    #[test]
    fn v3_n_minus_one_fixture_imports_legacy_ids_and_upgrades_to_v3() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = write_fixture(&dir, "lvm-state-v3-n1.json", V3_N1_FIXTURE);
        let mut state = LvmState::load(&source).expect("v2 state loads in v3");
        let volume_id = VolumeId::new("vol-upgrade-tombstone").expect("valid id");
        let volume = state.volume(&volume_id).expect("v2 volume preserved");
        assert_eq!(volume.entry.generation, 9);
        assert!(
            volume
                .runtime
                .retired_attachments
                .maybe_contains(&AttachmentId::new("att-retired-oldest").expect("id")),
            "the v2 retired-id vector was imported"
        );
        assert!(
            volume
                .runtime
                .retired_attachments
                .maybe_contains(&AttachmentId::new("att-retired-newer").expect("id"))
        );

        let target = dir.path().join("upgraded-v3.json");
        state.save(&target).expect("upgrade-on-write");
        let text = fs::read_to_string(&target).expect("read upgraded");
        assert!(
            text.contains("\"schema_version\":3"),
            "the first save stamps schema v3: {text}"
        );
        assert!(
            !text.contains("retired_attachment_ids"),
            "the legacy unbounded vector is never written again: {text}"
        );
        assert!(
            text.contains("retired_attachment_filter"),
            "the bounded filter is written: {text}"
        );
        let reloaded = LvmState::load(&target).expect("reload");
        assert_eq!(
            reloaded, state,
            "no retired id is lost by the v2→v3 upgrade"
        );
        assert!(
            reloaded
                .volume(&volume_id)
                .expect("volume")
                .runtime
                .retired_attachments
                .maybe_contains(&AttachmentId::new("att-retired-newer").expect("id"))
        );
    }

    #[test]
    fn v2_n_minus_one_fixture_loads_and_upgrades_to_v3() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = write_fixture(&dir, "lvm-state-v2-n1.json", V2_N1_FIXTURE);
        let mut state = LvmState::load(&source).expect("v1 state loads in v2");
        let volume_id = VolumeId::new("vol-upgrade-n1").expect("valid id");
        let volume = state.volume(&volume_id).expect("v1 volume preserved");
        assert_eq!(volume.entry.generation, 7);
        assert_eq!(
            volume
                .runtime
                .attachment
                .as_ref()
                .expect("v1 attachment")
                .state_revision,
            3
        );
        assert_eq!(
            state.move_record(&volume_id).expect("v1 move record").state,
            MoveVolumeBackingState::InDoubt
        );
        assert!(
            state.create_intents().is_empty()
                && state.grow_intents().is_empty()
                && state.delete_intents().is_empty(),
            "a v1 file has no intent maps"
        );

        let target = dir.path().join("upgraded-v3.json");
        state.save(&target).expect("upgrade-on-write");
        let text = fs::read_to_string(&target).expect("read upgraded");
        assert!(
            text.contains("\"schema_version\":3"),
            "the first save stamps schema v3: {text}"
        );
        assert!(
            !text.contains("pending_creates")
                && !text.contains("pending_grows")
                && !text.contains("pending_deletes"),
            "empty intent maps stay omitted: {text}"
        );
        assert_eq!(
            LvmState::load(&target).expect("reload"),
            state,
            "no durable field is lost by the v1→v3 upgrade"
        );
    }

    #[test]
    fn old_fixture_upgrades_on_save_and_round_trips_losslessly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = write_fixture(&dir, "n1.json", N1_FIXTURE);
        let mut state = LvmState::load(&source).expect("load");
        let target = dir.path().join("upgraded.json");
        state.save(&target).expect("save");
        let text = fs::read_to_string(&target).expect("read upgraded");
        assert!(
            text.contains("\"schema_version\":3"),
            "upgrade-on-write stamps the current schema version: {text}"
        );
        assert_eq!(
            LvmState::load(&target).expect("reload"),
            state,
            "no durable field is lost by load/save"
        );
    }

    #[test]
    fn future_schema_version_is_refused_typed_without_rewriting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let future = N1_FIXTURE.replacen(
            '{',
            "{\n  \"schema_version\": 4,\n  \"future_field\": true,",
            1,
        );
        let path = write_fixture(&dir, "future.json", &future);
        let err = LvmState::load(&path).expect_err("future version must be refused");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.is_unsupported_schema_version(), "{err}");
        assert!(err.detail.contains("schema_version 4"), "{err}");
        assert_eq!(
            fs::read_to_string(&path).expect("read refused file"),
            future,
            "a refused future-version file is never truncated or rewritten"
        );
    }

    #[test]
    fn zero_schema_version_is_refused_typed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let zero = N1_FIXTURE.replacen('{', "{\n  \"schema_version\": 0,", 1);
        let path = write_fixture(&dir, "zero.json", &zero);
        let err = LvmState::load(&path).expect_err("zero version must be refused");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(!err.is_unsupported_schema_version());
        assert!(err.detail.contains("invalid schema_version 0"), "{err}");
    }

    #[test]
    fn unknown_fields_at_a_supported_version_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Top-level unknown field.
        let top = N1_FIXTURE.replacen('{', "{\n  \"future_field\": true,", 1);
        let path = write_fixture(&dir, "top.json", &top);
        let err = LvmState::load(&path).expect_err("top-level unknown field");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(!err.is_unsupported_schema_version(), "{err}");

        // Nested unknown field inside a stored volume.
        let nested = N1_FIXTURE.replacen(
            "\"runtime\": {",
            "\"runtime\": {\n        \"future_field\": true,",
            1,
        );
        let path = write_fixture(&dir, "nested.json", &nested);
        let err = LvmState::load(&path).expect_err("nested unknown field");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("unknown field"), "{err}");
    }
}
