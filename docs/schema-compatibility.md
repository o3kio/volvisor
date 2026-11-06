# Durable-state schema compatibility and upgrade policy

Status: implemented for the stores listed below; enforced by the normal
workspace test job.
Related: [Volume API v2](../contracts/volume-api-v2.md),
[nearline replication contract v2](../contracts/nearline-replication-v2.md),
[provider contract v1](../contracts/volvisor-provider-v1.md),
[configuration](configuration.md).

Volvisor's durable state is authoritative recovery input: a daemon that
opens it must either interpret all of it or refuse to start. Guessing a
missing field, silently dropping an unknown one, or partially decoding a
file is never allowed (AGENTS rule 8's fail-closed reading).

## Covered stores and exact paths

| Store | File(s) | Version carrier | Current version |
|---|---|---|---|
| Intent journal | `<journal_dir>/journal.log` (binary frames) | `record_version` inside each frame payload | `RECORD_VERSION = 2` |
| LVM provider state | `lvm_state_path`, default `<journal_dir>/lvm-state.json` | `schema_version` | `LVM_STATE_SCHEMA_VERSION = 4` |
| Ceph provider state | `ceph_state_path`, default `<journal_dir>/ceph-state.json` | `schema_version` | `CEPH_STATE_SCHEMA_VERSION = 1` |
| DRBD provider state | `drbd_state_path`, default `<journal_dir>/drbd-state.json` | `schema_version` | `DRBD_STATE_SCHEMA_VERSION = 1` |
| Migration records | `<journal_dir>/migrations/<migration_id>.json` | `schema_version` | `MIGRATION_RECORD_SCHEMA_VERSION = 1` |
| Peer target preparations | `<journal_dir>/peer-preparations/<migration_id>.json` | `schema_version` | `TARGET_PREPARATION_SCHEMA_VERSION = 1` |
| Attachment records and tombstones | embedded in LVM/Ceph/DRBD state | containing store's `schema_version` | same as the containing store |

The LVM store's attachment tombstones are part of `VolumeRuntime`; they
are never a separate file. Schema v3 wrote the fixed-size
`retired_attachment_filter` Bloom filter (issue #63); the schema-v1/v2
`retired_attachment_ids` vector is still accepted on load and is imported
into the filter. **Since ADR-0012 the filter is legacy decode-only**: the
provider never inserts into it from the attach/detach path and never consults
it, because its false positives refused fresh ids on a churned volume, and the
strictly increasing per-volume attachment generation is the fence. A legacy
v1/v2 vector is still imported at decode, so an old file loads without loss.
The field is optional and is omitted while empty, so the serialized shape
remains a valid v3 file and no schema bump is required. A v3 file written
before ADR-0012 still carries its filter and loads without loss. The journal
lock file (`journal.log`'s sibling `journal.lock`) carries no durable schema.

Out of scope for this policy (follow-up work; they need the same treatment
before any format change): the grow-notification store
(`grow-notifications.json`), the lending/device-ownership store
(`ownership.json`), and the witness registry (which is derived from the
journal, not a second on-disk format).

## Versioning rules

1. **Baseline 1 is the pre-versioning shape.** Files written before the
   explicit version field existed decode as version 1. Adding the
   `schema_version` metadata field itself did not bump any store: the
   historical shape and the current shape decode identically, so a bump
   would claim a compatibility break that does not exist.
2. **Any future serialized-shape change requires a version bump**, including
   an additive field. Increment the affected store's version constant (or
   `RECORD_VERSION` for the journal). Do not rely on a serde default to make
   an additive change invisible: a bump is what lets an older binary refuse
   the file instead of silently losing the field.
3. **A bump ships with a new N-1 fixture.** Before changing the shape, keep
   a dump of the last released format under `tests/fixtures/schema/` and add
   a decode test for it. The previous N-1 fixture becomes the N-2 fixture
   only when a real N-2 format exists; never fabricate an intermediate
   shape.
4. **Upgrade-on-write.** An older supported file is read in place and is
   rewritten at the current version by the first successful mutation/save
   (for the journal, the next appended frame carries the current
   `record_version`; existing frames are not rewritten). Loading alone never
   mutates the file.
5. **No silent field loss.** Unknown fields at a supported version are
   rejected by the record's `deny_unknown_fields` shape. A decode failure is
   a typed `INTERNAL` startup error; it never drops a field, file or record.

### Historical additive changes grandfathered at version 1

These changes were made before this policy existed. They are covered by the
golden fixtures below and remain version 1; the current decoder accepts all
of them.

| Store | Additive change | Source commit |
|---|---|---|
| LVM | `requested_size_bytes` (initial format recorded only the effective size) | `c532b4c` |
| LVM | `vmm_disk_id` | `fe16f05` |
| LVM | attachment `state`, `state_revision`, `evidence`, `reporter`, `recorded_at_ms`; `retired_attachment_ids` | `a8c0254`, `c00bc3e` |
| LVM | `moves` map | `f235238` |
| Ceph, DRBD | `vmm_disk_id` | `fe16f05` |
| DRBD | authority block, pending fence, migration-cut marker | `398fd44`, `f686531` |
| Handoff | `consumer_proof`, `cut_started_at`, `cut_completed_at` | `88d8174` |
| Handoff | `barrier_lineage_refusal` | `531d340` |
| Peer preparation | `grant_attempt` (absent means 0) | `d6e91a3` |
| Journal | `http_status` (absent means `None`) | `88d8174` |

The initial LVM format had no requested-vs-effective distinction. Its
decoder backfills `requested_size_bytes` from `size_bytes` — the only size
that file recorded — rather than defaulting to zero or guessing a different
value.

The first additive change made under this policy is issue
[#70](https://github.com/o3kio/volvisor/issues/70): the durable LVM intent
maps `pending_creates`, `pending_grows` and `pending_deletes` are schema v2,
not a silent v1 additive change. `lvm-state-v2-n1.json` is the last released
v1 shape; v1 files load with the maps defaulted empty and are upgraded to v2
by the first successful save. An older binary sees `schema_version: 2` and
refuses the file with the typed unsupported-version error (never a generic
parse failure on an unknown field).

Issue [#63](https://github.com/o3kio/volvisor/issues/63) is schema v3: the
unbounded `retired_attachment_ids` vector is replaced by the fixed-size
`retired_attachment_filter`. `lvm-state-v3-n1.json` is the last released v2
shape (including non-empty retired ids). A v2 file loads, imports its vector
into the filter, and the first successful save writes v3 without the legacy
vector. v3 files are written as compact JSON (whitespace is not semantic;
the decoded shape is unchanged). An older binary refuses a v3 file with the
typed unsupported-version error. ADR-0012 later stopped using the filter as a
refusal mechanism: it saturates under churn and then refuses fresh ids, and a
strictly increasing attachment generation is a sound fence that needs no
per-id state. The field stays decodable (and is re-serialized if a legacy file
had one) but is never populated by the current attach/detach path; a legacy
v1/v2 vector is still imported at decode. The v3 shape is unchanged.

ADR-0013 is schema v4: the LVM state gains one field,
`next_attachment_generation` (the durable provider-wide allocator).
`lvm-state-v4-n1.json` is the last released v3 shape. A v3 file loads with the
allocator unseeded (`0`); on decode the allocator is seeded to
`max(persisted volume entry generations, persisted attachment generations) + 1`
only when the field is absent or `0`, and the first successful save writes v4
with the field always serialized. A non-zero
persisted allocator is authoritative and is never reset by create, delete or
recreate, so an attachment generation retired on a volume is never reissued.
A fresh `LvmState::default()` starts at `1`. Every save re-normalizes the
allocator through the same seeding rule, so a state assembled without it (a
migration, a hand-restored file, a test fixture) is raised to the invariant
before it is persisted rather than silently persisted unseeded. An allocator at
`u64::MAX` fails closed: allocation returns a typed `INTERNAL` refusal instead
of wrapping. An older binary refuses a v4 file with the typed
unsupported-version error. The residual upgrade window — a volume deleted
before the v4 file was ever written has no durable record of its consumed
generations — is recorded in ADR-0013 and the readiness report; it is not
represented as fixed.

Issue [#64](https://github.com/o3kio/volvisor/issues/64) is journal
`record_version` 2. Version 2 adds the `snapshot` record emitted by bounded
retention/compaction; version-1 records still replay unchanged and the first
post-upgrade append stamps version 2 (the log is never rewritten merely to
upgrade it). A **compaction snapshot is the first and only base record** of a
compacted log: it carries the retained terminal operations, every in-flight
intent, and a bounded ring of exact 64-bit fingerprints of recently evicted
operations. Operations evicted within the ring answer
`OPERATION_EXPIRED` (HTTP 410) instead of re-executing; operations older than
the ring are indistinguishable from new ones, so size the ring above the
longest client retry horizon. The live lookup is fail-closed on **any**
fingerprint match. Replay re-admission of an operation that only a
provisional replay-window eviction recorded clears its fingerprint; a 64-bit
collision can therefore also drop a different provisional entry, which then
becomes indistinguishable from a new operation and may re-execute. Durable
snapshot tombstones are never cleared by that path. The snapshot must fit one
8 MiB journal frame, so the configured retention bounds are **upper bounds**:
compaction first evicts the oldest unpinned terminal operations (recording
their fingerprints as tombstones) and then shortens the tombstone ring as
needed. The effective
retention can therefore be lower than configured, but compaction always makes
progress. In-flight intents and pinned operations are never evicted; if they
alone exceed one frame, compaction fails closed and new intents are refused
(`cannot_fit`) while outcome appends drain the set; the failure **count** is
exposed on `/metrics`, the detail is in the daemon log and
`Journal::last_compaction_error`. A failed compaction is retried after bounded
new growth — immediately after one record for the first failure, then with an
exponential backoff capped at 64 records or 64 MiB of new log — so a
persistently failing target is not re-serialized on every append. The witness registry
folds the complete record history and therefore opts out of compaction: it
refuses a compacted journal with a typed error rather than deriving authority
state from a truncated prefix. ADR-0014 adds the fail-closed exhaustion
contract on top of these bounds. `max_log_bytes` is a compaction **trigger**,
not a hard bound; `hard_ceiling_bytes` (default twice the byte trigger, and
required explicitly non-zero when the byte trigger is disabled) is the stop
condition. An append that would cross the ceiling first gets one forced
compaction attempt; if the log is still over, nothing is written and the append
is refused with the typed `JOURNAL_FULL` (HTTP 503). Reads and replay continue.
The refusal never evicts in-flight intents, terminal operations or tombstones
to make room. When the irreducible in-flight/pinned set alone cannot fit one
snapshot frame, compaction fails closed, new intents are refused
(`cannot_fit`), and outcome appends remain available so the outstanding
outcomes can drain; a later successful compaction clears the flag and reopens
admission. `/metrics` exposes `journal_hard_ceiling_bytes`,
`journal_in_flight_operations`, `journal_terminal_operations`,
`journal_tombstone_capacity`, `journal_compaction_attempts_total`,
`journal_cannot_fit` and `journal_refusals_total{reason}` alongside the
existing journal gauges. The witness journal (`open_with_records`) disables
compaction and therefore does not enforce the hard ceiling; its registry still
refuses a compacted log. `journal-v2-n1.log` is a version-1 log from
before the `http_status` field was added (byte-identical to
`journal-v1-n1.log`); it exercises the oldest v1 replay era, while the
version-independent `Outcome` decoder covers the later v1 shape that carried
`http_status`.

## Unknown, future and older versions

Every covered store probes the version **before** the strict decode. The
probe tolerates unknown fields on purpose: a file written by a newer build
must be recognized as newer before its unknown fields can produce a generic
parse error.

- **Version newer than supported** (or a journal `record_version` newer than
  `RECORD_VERSION`): typed `INTERNAL` error whose detail starts with
  `unsupported durable-state schema version`. Startup refuses to open the
  store. The file is left byte-for-byte intact; no partial decode, rewrite,
  truncation or reset occurs.
- **Version 0**: typed `INTERNAL` error (`invalid schema_version 0`). Zero is
  not a valid version; it is not treated as legacy.
- **Version ≤ supported**: strict decode. Older files are accepted,
  interpreted by the documented defaults/backfills above, and upgraded on
  the first successful write.
- **Missing file**: an empty state (first start), not an error.
- **Unparseable/corrupt file**: typed `INTERNAL` error; the store is not
  opened empty and the file is not replaced.

The journal has one deliberate distinction. A structurally torn tail
(incomplete frame, bad magic, bad CRC, broken sequence chain) contains no
complete durable record and is truncated on replay, as before. A **complete
CRC-valid frame** whose payload is undecodable, carries an unknown field at
a supported version, or declares an unsupported version is durable state:
replay refuses open and leaves the log untouched. Truncating it would be
silent data loss.

## Downgrade and rollback policy

**There is no in-place downgrade.** A store at version N can only be opened
by a binary that supports N or newer. An older binary sees a future version
and refuses startup with the typed unsupported-version error. Volvisor does
not down-convert records, strip fields, edit `schema_version` by hand, or
truncate the journal to make an older binary accept it.

Rolling back a deployment therefore requires restoring state, not just
installing the old binary:

1. Stop the daemon so no writer holds the journal lock.
2. Restore the pre-upgrade backup of the whole state directory (journal log
   plus all provider states, migration records and peer preparations) taken
   while stopped.
3. Install and start the binary matching that backup's schema versions.

Restoring a backup discards every operation committed after it. Backend
resources created or mutated after that point must be reconciled by the
operator before the restored daemon is trusted; the daemon will not invent
the missing intent records. Keep the pre-upgrade backup until the new
version has been exercised, and never run an older binary against the
newer directory in place.

## Golden fixture matrix

All fixtures are real historical shapes, taken from git history or (for the
tombstone and journal cases) the current shape immediately before the
`schema_version` field was added. They are not synthetic variants of the
current struct.

| Fixture | Store | Provenance | What it locks down |
|---|---|---|---|
| `tests/fixtures/schema/lvm-state-v1-n1.json` | LVM | `a8c0254` (consumer reporting, before retired ids) | attachment state/revision/generation, `vmm_disk_id`, evidence, move record |
| `tests/fixtures/schema/lvm-state-v1-n2.json` | LVM N-2 | `fafb62e` (initial P1 format) | absent `requested_size_bytes` backfills from `size_bytes`; pre-P6 fields default |
| `tests/fixtures/schema/lvm-state-v1-tombstones.json` | LVM | `c00bc3e` (tombstones, before version field) | `retired_attachment_ids` preserved |
| `tests/fixtures/schema/lvm-state-v2-n1.json` | LVM v1 (N-1 for v2) | last released v1 shape, written by the pre-#70 binary at `ee739bc` from the `a8c0254` fixture | v1 attachment state/revision/evidence, move record, and empty intent maps; upgrade-on-write stamps the current version (covered by the `volvisor-lvm` unit test and the daemon-store test `a_v1_stamped_n_minus_one_lvm_state_upgrades_on_write`) |
| `tests/fixtures/schema/lvm-state-v3-n1.json` | LVM v2 (N-1 for v3) | last released v2 shape: the `c00bc3e` tombstone fixture stamped `schema_version: 2` | legacy `retired_attachment_ids` import into the v3 filter; upgrade-on-write stamps v3 and never writes the vector again (covered by the `volvisor-lvm` unit test and the daemon-store test `a_v2_stamped_n_minus_one_lvm_state_imports_tombstones_and_upgrades_on_write`) |
| `tests/fixtures/schema/lvm-state-v4-n1.json` | LVM v3 (N-1 for v4) | last released v3 shape with an attached volume (entry generation 7, attachment generation 5) | the allocator seeds from the persisted entry/attachment generations when absent/zero, the first save writes v4 with the allocator, and no retired generation is reissued (`state::tests::v4_n_minus_one_fixture_seeds_the_attachment_allocator_and_upgrades_on_write`) |
| `tests/fixtures/schema/ceph-state-v1-n1.json` | Ceph N-1 | `ab9cedd` (before `vmm_disk_id`) | attachment mapping, absent disk id is `None` |
| `tests/fixtures/schema/drbd-state-v1-n1.json` | DRBD N-1 | `f686531` (before `vmm_disk_id`) | authority block, migration-cut marker, counters |
| `tests/fixtures/schema/drbd-state-v1-n2.json` | DRBD N-2 | `97e6841` (initial P3 format) | defaulted authority/fence/migration/disk id |
| `tests/fixtures/schema/migration-record-v1-n1.json` | Handoff N-1 | `01b440d` (consumer proof/cut timestamps, before barrier refusal) | proof, timestamps, history |
| `tests/fixtures/schema/migration-record-v1-n2.json` | Handoff N-2 | `124cc55` (initial P4b B1 record) | initial shape decodes |
| `tests/fixtures/schema/peer-preparation-v1-n1.json` | Peer preparation N-1 | `d6e91a3^` (before `grant_attempt`) | absent attempt counter defaults to 0 |
| `tests/fixtures/schema/journal-v1-n1.log` | Journal N-1 | `3cf2e18` (before `http_status`) | binary replay, absent status is `None`, idempotency |
| `tests/fixtures/schema/journal-v2-n1.log` | Journal v1 (N-1 for v2) | a version-1 log from before `http_status` was added, byte-identical to `journal-v1-n1.log` (the revision that wrote `record_version: 1`) | version-1 records replay under `RECORD_VERSION = 2`; upgrade-on-write stamps the next append v2; idempotent replay of the v1 outcome |

**Where N-2 does not exist, it is stated rather than fabricated:**

- **Ceph** has no N-2. The state store was introduced at `ab9cedd` with
  `requested_size_bytes`; the only later shape change was the additive
  `vmm_disk_id`, so both historical eras are the N-1 fixture above.
- **Journal** has no N-2. The journal was introduced at `3cf2e18` with
  `record_version` 1 and only gained the additive `http_status` before
  version 2 added the snapshot record; there is no released shape older than
  version 1. Both version-1 fixtures above are the N-1 shape.
- **Peer preparations** have no N-2. The store was introduced at `b1d52c2`
  and the only later change was additive `grant_attempt`; the initial and
  N-1 shapes are otherwise identical.

## Tests and CI

The compatibility matrix runs in the ordinary workspace job
(`cargo test --workspace --locked` in `.github/workflows/ci.yaml`); it needs
no privileged devices, cluster or network.

- Store-unit tests: `crates/volvisor-lvm/src/state.rs`,
  `crates/volvisor-ceph/src/state.rs`,
  `crates/volvisor-drbd/src/state.rs`,
  `crates/volvisor-journal/src/journal.rs`.
- Handoff store fixtures: `crates/volvisor-handoff/tests/schema_fixtures.rs`.
- Populated-directory upgrade (every covered store at once, including
  attachment state/revision, tombstones, journal replay/idempotency and
  future-version refusal): `crates/volvisord/tests/schema_upgrade.rs`.

A dedicated CI job would duplicate the same tests without adding signal, so
the normal workspace job is the compatibility gate.

## Changing a covered format

1. Add the new field/record shape and bump the store's version constant.
2. Add the last released shape as a new fixture (retain the existing N-2).
3. Add a decode test and an upgrade-on-write test; assert a future version
   is refused typed and the file is untouched.
4. Update the matrix above and the store's doc comment.
5. Run `cargo fmt --all --check`, clippy with `-D warnings`, and
   `cargo test --workspace --locked`.
