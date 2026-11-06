# ADR-0017 — LVM provider state is a snapshot plus an append-only record log

Status: Proposed — not accepted, not implemented
Decision owner: zoorpha
Date: 2026-11-06
Related: [ADR-0012](0012-monotonic-attachment-generation-fence.md), [ADR-0013](0013-durable-attachment-generation-allocator.md),
[ADR-0014](0014-journal-bounds-and-exhaustion-contract.md), [ADR-0015](0015-write-path-critical-section-invariant.md),
[ADR-0016](0016-semantic-change-and-default-scenario-gate.md), [schema compatibility](../schema-compatibility.md),
[default scenarios](../testing/default-integration-scenarios.md), [closure plan](../plans/2026-11-06-production-readiness-closure.md),
[2026-11-03 baseline](../benchmarks/2026-11-03-devhost/REPORT.md), issue [#62](https://github.com/o3kio/volvisor/issues/62)

## Context
Every LVM mutation is admitted under one state mutex (`crates/volvisor-lvm/src/provider.rs:513`) and made durable by rewriting the
whole `lvm-state.json` through a unique temp, `fsync`, `rename` and directory `fsync`
(`crates/volvisor-lvm/src/state.rs:873-894`, `:928-987`). Synchronous create/grow/delete/move/admin paths hold the mutex across
that save (`persist` at `provider.rs:1652-1662`; callers at `:3425`, `:3691`, `:4247`, `:4484`, `:5171`, `:6516`, plus
`admin.rs:179`, `:266`, startup at `provider.rs:2856`). A weighted mutation pays two whole-state saves (intent and commit), each
capture clones the whole state under the lock (`provider.rs:1351`), one gate serializes each canonical path (`provider.rs:735`,
`:762-777`), provider state carries its own pending create/grow/delete intents (`state.rs:635-668`), and the API journal holds
operation idempotency and recorded outcomes. This is the durable-state half of issue #62.

### The stale premise in issue #62
The issue's numbers are the **2026-11-03 baseline at `e3aa365`**, not HEAD: c1 `1665.787 ops/s`, c128 `241.716 ops/s` (`0.145x`
c1), c128 p95 `1251.564 ms` ([baseline report](../benchmarks/2026-11-03-devhost/REPORT.md), lines 131-151). Since then: reads take
the published `Arc<LvmState>` snapshot instead of `lock_state` (`provider.rs:1833-1839 state_snapshot`, used at `:1853`, `:5692`,
`:5924`, `:5958`, `:7399`, `:7410`); attach/detach/report release the lock before the durability wait (`provider.rs:3754-3853`,
`:3870-3921`, `:3945-3988`; `capture_and_reserve` `:3850`/`:3917`/`:3980`, `commit_async` `:3853`/`:3920`/`:3987`);
create/grow/delete release it across external LVM commands (ADR-0015, WP-01..WP-11); concurrent attach/detach/report saves
coalesce (`provider.rs:13-43`, `WriterCore` `:689-1271`).

Still true at the `bf6a4b5` code state (later docs-only commits are code-equivalent; re-pin before measuring): one admission mutex
(`provider.rs:513`); synchronous create/grow/delete/move/admin paths hold it
across the durable save; whole-state encode and temp-write/fsync/rename/dir-fsync per save (`state.rs:873-894`, `:928-987`);
one physical-write gate per canonical path (`provider.rs:735`, `:762-777`); two whole-state saves per weighted mutation; a
whole-state clone per capture (`provider.rs:1351`). **HEAD is unmeasured after `3fa984a`.**

The measured c128 write tail is dominated by the grow `vgs` epoch-retry convoy (`lvextend`/`lvs`, about 1.8–2.4 s), not the state
save (about 1.7–1.9 ms). A c128 fix must not be attributed to this ADR, and the baseline must not be quoted as a HEAD before/after.

## Decision
**Design C: provider state is a versioned snapshot plus an append-only, CRC-framed, sequence-chained record log. The record
`fsync` is the commit point, and the state mutex never spans that durability wait.** This **amends ADR-0015's lock invariant**; it
does not silently reinterpret it (see [Amendment to ADR-0015](#amendment-to-adr-0015)). Until accepted, ADR-0015 remains binding
as written.

**Store layout.** `lvm-state.json` is the snapshot (schema v5); `lvm-state.log` is the record log; `lvm-state.lock` is a `flock`
for one live writer per state path and carries no durable schema. All three sit beside `lvm_state_path` (`provider.rs:1580-1592`).
The log is not `journal.log`: it has its own path, lock and record schema.

**Frame format.** Reuse the journal layout — magic, `u64` sequence, `u32` payload length, `u32` CRC-32, payload
(`crates/volvisor-journal/src/frame.rs:1-36`). The payload carries a provider-state record version and one typed record. Sequence
numbers continue across compaction: the first post-compaction append is `marker + 1`. A CRC-valid frame with an undecodable
payload, unknown field or unsupported record version is durable state and refuses startup. A structurally torn tail (incomplete
header, bad magic, bad CRC) contains no complete record and is truncated.

**Commit protocol.** (1) **Under the state lock:** validate, mutate in memory, encode **one** record, reserve the next sequence;
capture order equals sequence order. (2) **Release the lock** — no `fsync`, rename, external command or durability wait under it.
(3) The leader concatenates pending records in sequence order into **one write and one `sync_all`**; this generalizes
`WriterCore`, which must not drop older captures as the whole-state coalescer does (`provider.rs:557-581`), because records are
deltas. (4) After the `sync_all`, publish the read snapshot and mark covered sequences durable. (5) Ack only when the caller's own
sequence is durable — `fsync`-before-ack is preserved; a synchronous caller drops the guard and parks outside the lock, an async
caller awaits the watermark. (6) An unprovable append fails **every** outstanding sequence, publishes nothing and requires
admission alignment against snapshot plus log; this generalizes `fail_unproven` (`provider.rs:1146-1158`) and keeps the
no-phantom-success rule.

**Records — no atomic transition may split.**

| Record | Carries | Preserved transition |
|---|---|---|
| `VolumePut` | Full `StoredVolume` plus create-intent removal | Create commit and intent removal in one record |
| `VolumeRemove` | Volume removal, delete intent, stale create/grow intents, move record | Delete completion atomic, as at `provider.rs:5122-5136` |
| `CreateIntentPut`/`Clear` | Full create intent | Intent before `lvcreate`; compensation clear |
| `GrowIntentPut`/`Clear` | Full grow intent | Intent before `lvextend`; completion clear |
| `DeleteIntentPut`/`Clear` | Full delete intent | Intent before `lvremove`; compensation clear |
| `AttachmentUpdate` | Full attachment plus `next_attachment_generation` | Allocator value and attachment in one record |
| `MoveRecordPut`/`Clear` | Full move record | Move anchor and terminal classification |
| `DevicePut`/`Remove` | Full device claim | Admin claim/release |
| `SnapshotMarker` | Snapshot sequence | Base marker for replay/truncation |

Records are apply-idempotent whole-key replacements or explicit removals. No record relies on a second record to complete one
logical transition. The API intent journal remains the operation-idempotency store; this log records provider state, not request
outcomes.

**Readers.** The contract is unchanged: `state_snapshot` (`provider.rs:1833-1839`) returns the last published `Arc<LvmState>`;
`GET`/list/inspect do not take `lock_state`. Publication moves after durable group commit. The O(state) clone per publication is
retained for this proposal and is an open question, not a settled cost.

**Snapshot and compaction.** Compaction writes a complete snapshot through the existing atomic save (`state.rs:928-987`) carrying
a `SnapshotMarker` sequence `N`, then replaces/truncates the log with the journal's rename and directory-fsync protocol
(`crates/volvisor-journal/src/journal.rs:1034-1149`). Replay skips frames at or below `N`, so a crash in the gap replays
duplicates the marker skips. Compaction is a **full snapshot with zero eviction**: live volumes, intents, attachments, devices and
the allocator are never expired to make a snapshot fit. There are no tombstones, terminal ring or "expired" answer.

**Startup.** (1) Take the `flock`; a second writer is refused typed. (2) Sweep stale temps (`provider.rs:1587-1591`). (3) Load the
v5 snapshot or migrate v4. (4) Replay frames above the marker; truncate a torn tail. (5) A CRC-valid undecodable payload or
unsupported record version refuses startup; a missing snapshot with a non-empty log refuses rather than guessing a base.
(6) **Proposed:** a broken sequence chain refuses startup instead of truncating — a deliberate divergence from the journal and
owner decision 5. (7) Run the existing constructor reconcile (`provider.rs:1580-1619`).

**Crash seams.**

| Seam | Result |
|---|---|
| Mid-append before `fsync` | Torn tail, no ack; replay truncates |
| After `fsync` before ack | Durable; replay applies it; a same-operation retry gets API `OPERATION_IN_DOUBT` |
| Crash during snapshot write | Old snapshot; log replays from old marker |
| Between snapshot rename and log truncate | New snapshot plus old log; records ≤ marker skipped |
| Unprovable append failure | All outstanding sequences fail; nothing publishes; admission re-aligns |

**Why provider state is not the intent journal.** The stores share frame, sequence and compaction machinery **only**. The journal
expires operations and has a hard-ceiling refusal contract (ADR-0014); provider state must never evict live state, and its
compaction is a full snapshot. The witness `open_with_records` precedent — refusing a compacted journal because it folds the
complete history — shows why a consumer with different retention semantics must not inherit journal eviction
(`docs/schema-compatibility.md:168-171`).

### Amendment to ADR-0015
ADR-0015 permits the state lock to span a "durable save" (`0015-write-path-critical-section-invariant.md:48-52`). **ADR-0017
amends that bullet:** the lock may span validation, in-memory mutation, one-record encoding and sequence reservation, but not the
record `fsync`, the group-commit wait, a snapshot write or any external command. "The durable intent, not the lock, is the
cross-command fence" still applies. If this ADR is not accepted, ADR-0015 is unchanged and no part of this design may be
implemented as if it were.

## Alternatives rejected
1. **A — narrow the existing lock only.** Release the mutex before the whole-state save without a log. Rejected: it removes the
   stall but keeps O(state) encode, two whole-state saves, whole-file write amplification and per-save `fsync`; tail latency
   still scales with live state, and there is nothing to group-commit.
2. **B — one file per resource.** Rejected: one logical transition (create plus intent removal; delete plus three intent clears
   plus move) becomes a multi-file atomicity problem with no single commit point; directory fsyncs and startup scans multiply.
3. **D — embedded KV (redb, sled, SQLite).** Rejected for now: a new storage engine, its crash semantics, licence/provenance
   review and a new migration surface, for a problem the existing frame/replay machinery already solves. Revisit only if
   measurement demands it.

## Consequences
**Positive.** The lock holds no durability wait; concurrent mutations amortize one `fsync` per group; an append writes O(record),
not O(state); replay is sequence-idempotent; snapshot plus log bounds recovery work; the read path keeps its lock-free contract.

**Negative.** Two durable artifacts must be backed up and restored together; replay/compaction is new correctness-critical code;
the record schema needs its own versioning and fixtures; ack latency depends on the leader; the crash-seam matrix grows; `flock`
turns the unsupported two-provider alias into a typed refusal; startup writes during v4 migration.

**Operational.** Back up snapshot and log as one directory while stopped; never run two daemons on one state path; monitor log
bytes, records, compaction failures, refusals and replay time; roll back by whole-directory restore, never in-place downgrade;
the filesystem must honor file and directory `fsync`.

## Compatibility and migration
`LVM_STATE_SCHEMA_VERSION` becomes `5` (`state.rs:104`). The v4 shape written by `3fa984a` remains the N-1 format and is accepted
by the v5 binary; v4 is in-repo, not a tagged release, so this N-1 fixture is pinned to that revision, not to a tag.

| On-disk state | v5 binary | pre-v5 (v4) binary |
|---|---|---|
| v4 snapshot, no log | One-time migration save, then opens | Correct pre-migration state |
| v5 snapshot, marker 0, empty log | Opens | Typed unsupported-version refusal; untouched |
| v5 snapshot plus log | Replays above marker | Typed unsupported-version refusal; untouched |
| v4 snapshot plus non-empty log | Impossible under the ordered migration; refuses | Would ignore the log — hence the order is mandatory |

**Migration v4 → v5.** Add `tests/fixtures/schema/lvm-state-v5-n1.json`: the v4 shape written by `3fa984a`, recorded as the N-1
shape even though v4 has no distribution tag. On v4 open, still single-writer and before any append: write the v5 snapshot with marker `0`
through the atomic save, then create the empty log (file `fsync` plus directory `fsync`), then allow appends. This one-time
startup save is a **documented exception** to [schema-compatibility](../schema-compatibility.md) rule 4 ("loading alone never
mutates the file"). It is justified because the alternative leaves a v4 snapshot beside a non-empty v5 log, and a rollback or old
binary would open the stale snapshot and silently miss committed records. The migration save closes that window before the first
append; normal upgrade-on-write then applies. On acceptance, `docs/schema-compatibility.md` rule 4 must be amended to record this
one-time startup exception; the ADR does not amend it by itself.

**Rollback.** No in-place downgrade. Stop the daemon, restore the pre-upgrade backup of the whole state directory (snapshot, log,
journal and other provider states), then install the matching binary, per [schema-compatibility](../schema-compatibility.md).
Restoring discards everything committed after the backup; backend resources must be reconciled by the operator.

**Rollback and replay evidence.** Before implementation is merged, SL-02..SL-04, SL-08 and SL-11 must pass, and a stopped-directory
restore drill must start the pre-v5 binary against the restored backup and record the result. A plan alone is not evidence.

## Fail-closed refusals

| Condition | Refusal |
|---|---|
| `flock` held by another process | Typed startup `INTERNAL`; state untouched |
| Snapshot schema newer than 5 | Typed unsupported-version; untouched |
| CRC-valid frame, undecodable payload | Startup refuses; log untouched |
| Unsupported record version | Startup refuses; log untouched |
| Broken sequence chain | **Proposed:** startup refuses; log untouched (owner decision 5) |
| Torn tail | Truncated; those records were never acknowledged |
| Missing snapshot with non-empty log | Startup refuses; no guessed base |
| Log at ceiling and compaction cannot succeed | **Proposed:** new mutations refused typed, reads/replay continue, no eviction (owner decision 3) |
| Unprovable append outcome | All outstanding sequences fail; nothing publishes; alignment required |
| Corrupt snapshot | Typed `INTERNAL`; never opened empty or replaced |

## Proposed default scenarios
**Proposed names only.** None is in [`default-integration-scenarios.md`](../testing/default-integration-scenarios.md) or its TSV,
and none may be cited as a gate until this ADR is accepted, implemented and the rows are added. All are `volvisor-lvm`; the
harness is `--lib` unless the row says `lvm_tests` (that is, `--test lvm_tests`). Each must fit **60 s**, and the whole warm
catalog must stay under **300 s**.

| ID | Proves | Proposed test function |
|---|---|---|
| SL-01 | Startup takes `flock` and replays a `VolumePut` before publication | `state_log_startup_takes_flock_and_replays_a_volume_put_before_publication` |
| SL-02 | Torn tail truncated, not acknowledged | `state_log_torn_tail_is_truncated_without_acknowledging_the_record` |
| SL-03 | CRC-valid undecodable frame refuses startup | `state_log_crc_valid_undecodable_frame_refuses_startup` |
| SL-04 | Broken sequence chain refuses startup | `state_log_broken_sequence_chain_refuses_startup` |
| SL-05 | Group commit writes pending records in sequence order | `state_log_group_commit_writes_pending_records_in_sequence_order` |
| SL-06 | Ack follows the single `fsync` | `state_log_ack_waits_for_the_single_fsync` |
| SL-07 | Unproven append fails outstanding sequences and requires alignment | `state_log_unproven_append_fails_every_outstanding_sequence_and_requires_alignment` |
| SL-08 | Snapshot-then-truncate skips duplicate replay | `state_log_compaction_snapshot_then_truncate_skips_duplicate_replay` |
| SL-09 | Compaction never evicts live state | `state_log_compaction_never_evicts_a_live_volume_intent_or_allocator` |
| SL-10 | Attachment and allocator are one record | `attachment_update_carries_the_allocator_in_one_record` (`lvm_tests`) |
| SL-11 | Delete completion records are atomic | `volume_remove_carries_the_completion_intents_and_move_record_in_one_record` (`lvm_tests`) |
| SC-10 | v5 N-1 fixture migrates to marker 0 and empty log | `state::tests::v5_n_minus_one_fixture_migrates_to_a_v5_snapshot_with_marker_zero_and_empty_log` |
| SC-11 | v4-only state loads; v5 is refused untouched | `state::tests::v4_snapshot_without_a_log_loads_and_a_v5_snapshot_is_refused_without_rewrite` |
| WP-12 | Create does not hold the lock across the log `fsync` | `create_releases_the_state_lock_across_the_state_log_fsync` (`lvm_tests`) |
| WP-13 | Delete/grow do not hold the lock across the log `fsync` | `delete_and_grow_release_the_state_lock_across_the_state_log_fsync` (`lvm_tests`) |
| WP-14 | An attach waiter does not hold the lock across group commit | `attach_waiters_do_not_hold_the_state_lock_across_group_commit_durability` (`lvm_tests`) |

The bare names above are proposed **function** names. Rows marked `lvm_tests` are top-level integration tests and already complete. `--lib` rows must
carry their final module path when added (for example `state_log::tests::<name>`), and every row must be verified with `--exact` before the catalog
changes; a bare function name is not runnable by the harness.

## What must be measured
A benchmark is not a durability proof, and no 2026-11-03 number is a current-code control. Measure on a recorded clean host at the
implementation SHA: first the `bf6a4b5` code state as the baseline (docs-only commits after it are code-equivalent; HEAD is
unmeasured after `3fa984a`), then the same workload after; mixed throughput and
p50/p95/p99 at c1/c8/c32/c128; `fsync` calls per committed mutation and group-commit coalescing; log bytes/records per operation;
snapshot bytes/time versus live-volume count (the baseline state reached 2.4 MiB); startup replay time for 10³/10⁵ records and
torn tails; read-snapshot publication cost; RSS and disk high-water over a separately recorded soak; and the c128 grow tail
(`vgs` epoch-retry convoy) as its own item.

## What this does not close
- The ADR-0013 upgrade residual (a volume deleted before the v4 file was written has no durable generation record).
- Two live providers aliasing one state path: now `flock`-refused, not supported.
- The API-layer journal's own per-operation `fsync`s.
- The measured c128 grow tail (`lvextend`/`lvs` epoch-retry convoy).
- Real-backend, multi-host or 24-hour soak evidence.
- Any throughput, SLA or production claim, and any before/after at HEAD — HEAD is unmeasured, so none exists yet.

## Open questions / owner decisions
1. **Does this close #62, or stay open pending the measured bundle?** Proposed: stay open until before/after and replay evidence exist.
2. **Phasing.** Convert create/grow/delete/move/admin to async commit in the same change, or land the log first with synchronous
   callers that drop the guard before waiting? Proposed: log first, then async.
3. **Log hard-ceiling semantics.** Mirror ADR-0014's typed refusal (proposed default here) or require compaction to succeed and
   refuse mutations until it does. Either way, never evict live state.
4. **`admin.rs:83-163` holds the state lock across `pvcreate`/`vgcreate`/scan/`vgs`.** An apparent ADR-0015 gap or exception: in
   scope here, a separate ADR-0015 amendment, or a documented exception?
5. **Broken-sequence-chain policy.** Refuse startup (proposed) or truncate like the journal.
6. **Scope.** LVM only, or shared with Ceph/DRBD provider states?
7. **Shared machinery.** A shared frame/compaction crate, or copy the pattern into the LVM crate?
8. **Read-snapshot publication cost.** Keep the O(state) clone, or move to copy-on-write/lazy publication. Needs a prototype and measurement.

## Implementation status
None. This change adds only this Proposed ADR: no v5 constant, log format, migration, fixture, scenario row or measurement exists,
and the default catalog is unchanged. It does not authorize implementation before acceptance.

## Non-claims
This ADR does not claim #62 is fixed, that the current code is faster than the 2026-11-03 baseline, that the c128 tail is addressed, that group
commit has been measured, or that the provider is production-supported. It claims no throughput, latency, capacity, recovery-time
or SLA result. It does not accept a broken-sequence policy, a hard-ceiling policy, an implementation phase or Ceph/DRBD scope;
those remain owner decisions. A future accepted ADR, a crash-injection drill and a recorded before/after are still required.
