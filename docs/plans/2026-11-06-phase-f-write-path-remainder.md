# Phase F — the #62 write-path remainder, with #63/#64 verification

Status: normative for this workstream (the plan of record; implementation
commits cite it). F1/F2 are implemented and F3 measured at
`2edcecd9b3d4c555831e6d4db1337e3dbcff6a15`; the evidence bundle is
`docs/benchmarks/2026-11-06-devhost-phase-f-write-path-remainder/`.
Date: 2026-11-06
Branch: `phase-a-scaling`; plan baseline HEAD `8d4579a`, implementation and
measured HEAD `2edcecd`
Builds on: [Phase E plan](2026-11-05-phase-e-write-path-lock-scope.md),
[Phase D plan](2026-11-04-phase-d-control-plane-hardening.md),
[pre-production readiness report](../releases/pre-production-readiness.md),
[ADR-0012](../adr/0012-monotonic-attachment-generation-fence.md),
[tracker #62](https://github.com/o3kio/volvisor/issues/62),
[#63](https://github.com/o3kio/volvisor/issues/63),
[#64](https://github.com/o3kio/volvisor/issues/64),
[AGENTS rules 4, 8, 11 and 12](../../AGENTS.md).

## Correction to the Phase E carry-over

The Phase E plan listed three items as open after E4: the move drive's
`pvmove` lock scope, the remaining short in-lock steps, and issues #63/#64.

An audit at `8d4579a` finds that **#63 and #64 are already implemented**:

- **#63** — the unbounded `retired_attachment_ids` vector is gone. Schema v3
  writes a fixed-size decode-only filter, and the authoritative
  detach/report fence is the strictly increasing per-volume
  `attachment_generation` (`e08a2be`, `ec6f9df`; ADR-0012). A churned volume
  no longer accumulates per-id state. ADR-0012 is **Accepted** (2026-11-06,
  zoorpha); the issue stays open because ADR-0013's durable allocator and
  its delete + same-id recreate scenario are not implemented.
- **#64** — the journal has bounded retention, atomic snapshot compaction,
  an exact tombstone ring, backoff for failed compactions and config wiring
  (`f38a08d`, `2eea13a`, `d0241ea`). The **RSS high-water** and the
  unrotated daemon-log volume recorded in Phase C remain open residuals;
  they are not the unbounded-journal defect the issue describes.

Phase F therefore does **not** redesign #63 or #64 again. It finishes the
real `#62` remainder and produces fresh, exact-SHA evidence that the #63/#64
bounds behave as implemented.

## What this Phase F is — and is not

Phase F narrows the remaining provider-state critical sections listed below
and proves the move drive's existing lock scope with deterministic gate
tests. It is a **hardening and verification batch**, not a throughput or
production claim.

- No production-support, SLA, zero-RPO, durability, capacity or release
  claim.
- No claim that #62, #63 or #64 is fixed until the paths are tested and the
  evidence is recorded; the issues stay open until explicitly instructed.
- No throughput, latency or zero-regression claim without a fresh clean
  benchmark at the exact final SHA.
- No change to the five readiness gate verdicts.
- No push, tag, PR, issue-state change or ADR acceptance.

## Audit result — the move drive's lock scope

At `8d4579a` the same-VG move request and supervision path **already
releases the provider state lock across every external CLI call**:

| Step | Lock held? | CLI under the lock? |
|---|---|---|
| `admit_move` | published `Arc` snapshot | no |
| `observe_lv` (`lvs`) | no | no |
| `qualify_move_target` (`pvs`) | no | no |
| `derive_move_source` | snapshot | no |
| `guard_fresh_move_source` (`lvs`) | no | no |
| `journal_move_intent` | yes | **no** — revalidate + durable save only |
| `start_pvmove` | no | no |
| `persist_copying_record` | yes | **no** — classify + durable save only |
| `supervise_move` poll (`lvs`) | no | no |
| `finish_complete` / `park_in_doubt` / `resolve_absent_move` | yes | no — durable save only |

So the Phase E phrase "the online move drive and its lock scope" describes a
path that is already narrow at the request/supervision boundary. Phase F
adds the deterministic proof that was missing, rather than claiming a new
narrowing.

Three genuine residuals remain:

1. `persist_copying_record` re-reads the durable file **while holding the
   lock** on the save-failure path (`LvmState::load` after a failed
   `persist`). The file is bounded since #63 and the re-read is failure-only.
   Moving it outside the guard would open a window where a concurrent
   supervisor could complete the move between the re-read and the in-memory
   restore, so it is recorded as an **accepted residual** rather than
   restructured in F1.
2. Startup `apply_delete_intents` runs `discard_and_remove_lv` under the
   startup state lock. Startup is single-threaded, so this is not a hot-path
   stall; it is recorded, not silently omitted.
3. Move's synchronous CLI calls still run inline on async workers (Phase E
   E4 residual). That is a **worker handoff** item, not a lock-scope item;
   it is out of Phase F unless separately scheduled.

## Slices

### F1 — prove the move drive's lock scope

**Gap.** No deterministic test parks a move's `lvs` observation or its
`pvmove` start and shows that another volume's write still completes. The
lock release is visible in the source, but a regression that reintroduced
the guard around the CLI would not fail a test.

**Deliverables.**

- A gate test parks the move's first `lvs` observation. While it is parked,
  an attach on a different volume completes, and the move's own record is
  still the durable `PREPARING` anchor (no half-written state).
- A second gate test parks `pvmove` after the `PREPARING` save. While it is
  parked, an attach on a different volume completes and the durable record
  still says `PREPARING`; releasing the gate completes the move with exactly
  one generation bump.
- Both tests fail under a targeted negative control that holds the state
  lock across the parked call.
- `persist_copying_record`'s failure-path durable re-read stays under the
  lock and is recorded as an accepted residual: its classification must be
  atomic with the in-memory restore, and the re-read is bounded and
  failure-only. The typed errors are unchanged.
- The move path's CLI-inline worker handoff is recorded as an accepted
  residual, not claimed fixed.

**Acceptance.**

- The two gate tests pass and their negative controls are observed to fail
  before the assertion that matters.
- The existing move crash/reconcile, adoption and completion tests pass
  unchanged.
- `cargo fmt --all --check`, workspace clippy `-D warnings` and the full
  workspace test suite pass.

### F2 — the remaining short in-lock steps

Three CLI calls still run under the provider state lock. Each is short
relative to `lvcreate`/`lvremove`, but each still queues every mutation
behind an external command.

#### F2a — delete's existence check (`lvs`)

**Gap.** `delete_volume_inner` calls `lv_size` (`lvs`) while holding the
state guard.

**Deliverables.**

- Snapshot the volume names and validate the request under the lock, then
  drop the guard and run the `lvs` existence check outside it.
- Reacquire the lock and revalidate the volume generation, lifecycle and
  matching pending intent before the durable delete intent is saved. A
  changed volume/incarnation fails closed.
- The durable intent remains the fence and is still saved before any
  destructive command.
- A failed/reordered revalidation never authorizes `blkdiscard`/`lvremove`.

**Acceptance.**

- A gate test parks the delete's `lvs` and shows another volume's attach
  completes; the delete still completes after release.
- A test proves a volume deleted/recreated in the observation window is
  refused rather than removed under a stale authorization.
- The existing delete crash, intent and erasure-policy tests pass unchanged.

#### F2b — create's pool sample (`vgs`)

**Gap.** `prepare_create_intent` calls `pick_pool_vg` → `vg_capacity_map`
(`vgs`) while holding the state guard.

**Deliverables.**

- Sample `vgs` outside the lock using the existing allocation-epoch
  protocol: read the epoch, sample, reacquire the lock, and retry on an
  epoch change within the existing bounded budget.
- Reserve the extent-rounded demand under the lock, then persist the create
  intent in the same critical section. The reservation is released on every
  command outcome.
- A retry that exhausts the budget fails closed with the existing typed
  capacity refusal; it never allocates against a stale sample.

**Acceptance.**

- A gate test parks the create's `vgs`; another volume's attach completes
  while it is parked.
- A test bumps the allocation epoch while the sample is parked and proves
  the create retries or refuses, never overcommits.
- The existing create reservation, adoption and compensation tests pass
  unchanged.

#### F2c — failed-create compensation (`lvremove`)

**Gap.** `compensate_unpersisted_create` runs a checked `lvremove` in the
reacquired critical section after a failed final save.

**Deliverables.**

- Keep the durable-file classification under the lock; drop the guard for
  the compensating `lvremove`; reacquire only to remove the volume/intent
  from memory and persist the cleared state.
- The uncommitted in-memory edit is rolled back to the last published
  durable view before the guard is released, so a failed save can never
  surface as a phantom volume.
- When the durable file is unreadable, reacquisition falls back to the raw
  state guard (skipping only the alignment load) so the clear save is still
  attempted and its failure classified; a poisoned mutex is reported
  without masking the original save error.
- Preserve the carve-outs: a landed final save keeps the volume and LV; a
  failed cleanup reports the manual remediation; an unreadable durable file
  is treated as not committed (the existing fail-closed direction).
- The original save error is never masked.

**Acceptance.**

- A gate test parks the compensating `lvremove` and shows another volume's
  attach completes.
- Both carve-outs (landed final save, failed cleanup) still produce their
  exact typed errors.
- The existing `#58`/`#70` compensation tests pass unchanged.

### F3 — verify the #63/#64 bounds at the exact Phase F SHA

This slice adds **evidence, not a new retention design**.

**Deliverables.**

- Run the published harness on a clean tree at the exact Phase F commit
  (shortened 25-minute soak, unchanged from the Phase E methodology).
- Record journal on-disk high-water/end, state-file high-water/end, RSS,
  fds, restart replay time and errors, and compare with the Phase C
  post-fix bundle.
- State explicitly that a 25-minute soak is **not** a 24-hour leak proof,
  and that the RSS high-water is not claimed fixed.
- Update the readiness ledger and the #64 residual wording only from the
  measured bundle. Do not close #63/#64; record that their code is present
  and ADR-0012 acceptance is recorded (2026-11-06, zoorpha).

**Acceptance.**

- The bundle is published only if the tree is clean, the binary digest
  matches the source, and every measured JSON/CSV is copied unmodified.
- The report distinguishes "code present" from "issue closed" and does not
  claim a throughput gain.

## Order of work

1. **F1 (move proof)** — self-contained; the request path is already narrow,
   so the first change is a regression test plus the failure-path re-read.
2. **F2a (delete `lvs`)** — one call, one revalidation.
3. **F2b (create `vgs`)** — needs the allocation-epoch retry budget.
4. **F2c (compensation `lvremove`)** — error-path restructuring; do last.
5. **F3 (verification)** — only after F1/F2 pass tests, lint and review.

## Out of scope

- Any redesign of journal retention/compaction (#64 code is already present).
- Any new retired-id mechanism (#63 code is already present; ADR-0012
  acceptance is an authority decision).
- The 24-hour soak, real-LVM/physical-disk soak, or data-path evidence.
- #37/#38/#39 hardware, D3 storage-side fencing, D8 host loss.
- #47 signing key/tag/release, #48 CHV pin.
- #49's 14 consecutive nightly runs and published flake rate.
- #50's named owners and two-maintainer sign-off.
- Any push, tag, PR, ADR acceptance or issue-state mutation.

`#62`, `#63` and `#64` stay open until explicitly instructed. All gate
verdicts are unchanged: gates 1, 2 and 5 **NOT MET**; gates 3 and 4
**PARTIAL**; overall **NOT PRE-PRODUCTION READY**.

## Completion note

F1, F2a, F2b, F2c and F3 are complete at
`2edcecd9b3d4c555831e6d4db1337e3dbcff6a15` on `phase-a-scaling`. The
measured evidence bundle is
[2026-11-06-devhost-phase-f-write-path-remainder](../benchmarks/2026-11-06-devhost-phase-f-write-path-remainder/REPORT.md):
a clean-tree 1/8/32/128 sweep and a **shortened 25-minute soak**, not the
24-hour soak. It is a single-host development-lab control-plane measurement
and supports no production, data-path, SLA, durability, capacity or
throughput claim. `#62`, `#63` and `#64` remain **open** (their code is
present and tested; the issues are not closed), ADR-0012 and the Phase F
ADRs are **Accepted** (2026-11-06, zoorpha), and no gate verdict changes:
gates 1/2/5 **NOT MET**, gates 3/4 **PARTIAL**, overall **NOT
PRE-PRODUCTION READY**. The plan's earlier
"No push, tag, PR…" line was the pre-authorization constraint; the branch
was later pushed as WIP under explicit authorization, and that push is not
evidence for this batch.
