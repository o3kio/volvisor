# Phase E — create/delete write-path lock scope

Status: normative for this workstream (the plan of record; implementation
commits cite it)
Date: 2026-11-05
Branch: `phase-a-scaling` (Phase D closed at `59f64cc` for the reverted
option C experiment; `91654bf` records the revert)
Builds on: [Phase D plan](2026-11-04-phase-d-control-plane-hardening.md),
[pre-production readiness report](../releases/pre-production-readiness.md),
[grow-tail decision memo](../reviews/2026-11-05-grow-tail-decision.md),
[tracker #62](https://github.com/o3kio/volvisor/issues/62),
[AGENTS rules 4, 8, 11 and 12](../../AGENTS.md).

## What this Phase E is — and is not

Phase D released the provider state lock across the grow body and moved the
grow capacity sample outside it. The same defect class remains on create and
delete: both hold the single provider state mutex across whole-file durable
saves **and** external LVM commands.

- Create holds the lock across `vgs`, the durable create-intent save,
  `lvcreate`, the size-verifying `lvs`, and the final save. A slow create
  therefore queues every mutation behind a `lvcreate`/`lvs` pair, even for
  other volumes.
- Delete holds the lock across `lvs`, the durable delete-intent save,
  `blkdiscard`/`lvremove`, and the final save. A `ZeroDiscard` delete holds
  it across a full-device discard plus `lvremove`.
- The move drive has its own durable record protocol and is **not** in this
  phase (see Out of scope).

This Phase E narrows the lock on create and delete while preserving the
durable-first intent, crash recovery, compensation and idempotency
contracts. It is a **hardening batch**, not a throughput or production claim.

## Non-claims

- No production-support, SLA, capacity, zero-RPO, durability or release
  claim.
- No claim that #62 is fixed until the paths are changed, tested and
  re-measured. A design document alone closes nothing.
- No claim that the lock release improves throughput, latency or the c128
  envelope before a fresh clean bundle says so.
- No real Ceph, DRBD, NVMe, multi-host or data-path evidence.
- No 24-hour soak, published flake rate, tag, push, PR, release or
  issue-state change.
- No change to the five readiness gate verdicts.

## Slices

### E1 — release the state lock across the delete body

**Gap.** `delete_volume_inner` keeps the provider state lock from the
request validation through the final save. The long steps are the
`blkdiscard`/`lvremove` pair; a `ZeroDiscard` delete can hold the mutex for
the full discard.

**Deliverables.**

- The durable delete intent remains the fence and the recovery anchor. It is
  saved under the state lock **before** any destructive command, exactly as
  today.
- After the intent save, the state guard is dropped and the command body
  runs without it. The final removal of the volume record, delete intent,
  stale create/grow intents and settled move record is a fresh
  `lock_state_stable()` critical section.
- A per-volume in-memory delete drive guard refuses a second concurrent
  delete of the same volume with a typed `InvalidState`, mirroring the grow
  drive guard. The durable intent (not this set) survives a restart.
- The final removal revalidates that the volume and the matching delete
  intent are still present. A mismatch fails closed instead of removing a
  different volume's record.
- The in-memory state keeps the volume and the delete intent until the final
  save, so `GET` results do not claim the delete completed early, and
  attach/grow/move keep refusing through the existing pending-delete fence.
- The `lvs` existence check stays inside the first critical section in E1
  (a short read relative to the destructive command). Moving it outside is
  an E3 residual, not a claim of this slice.

**Acceptance.**

- A deterministic test parks `lvremove` inside the fake runner. While it is
  parked, an attach on a *different* volume completes (the state lock is not
  held), an attach on the deleting volume is refused by the pending-delete
  fence, and a second delete of the same volume is refused by the delete
  drive guard.
- A test proves the final state file has no volume, no delete intent and no
  stale create/grow intent after a successful delete.
- The existing delete, crash-replay, intent and concurrency tests pass
  unchanged; `cargo fmt --all --check`, workspace clippy `-D warnings` and
  the full workspace test suite pass.
- A failed `lvremove` still leaves the durable intent and reports that a
  retry or restart completes the delete.

### E2 — release the state lock across the create allocation

**Gap.** `create_volume_inner` keeps the provider state lock across
`pick_pool_vg` (`vgs`), the create-intent save, `lvcreate`, the
size-verifying `lvs` and the final save.

**Deliverables.**

- The durable create intent remains durable-first and is saved under the
  state lock before `lvcreate`.
- Once the intent is durable, the state guard is dropped and `lvcreate` plus
  its verifying `lvs` run without it.
- Two creates must not spend the same free space while one allocation is in
  flight. A create takes a capacity reservation under the state lock before
  releasing it, using the existing per-VG reservation accounting that grow
  admission already subtracts. The reservation is removed and the allocation
  epoch bumped when the command returns, before the final commit.
- The final commit reacquires `lock_state_stable()`, inserts the volume
  record, removes the create intent and saves once, preserving the existing
  compensation classification for a failed final save.
- The existing unknown-outcome, known-failed-start, size-verification and
  adoption paths keep their current durability behaviour; each reacquires
  the state lock only for its state change/save, never across an LVM
  command.
- A per-volume create/delete drive guard refuses a concurrent structural
  mutation of the same volume. The durable intent remains the cross-restart
  fence.
- `pick_pool_vg`'s `vgs` sample stays inside the admission critical section
  in E2. Moving the sample outside (the grow epoch protocol) is an E3
  residual, not a claim of this slice.
- Accepted residuals: the failed-final-save compensation
  (`compensate_unpersisted_create`) still runs its checked `lvremove` in the
  reacquired critical section; moving that `lvremove` out is an E3 residual.
  A create whose pool shrank between its own sample and its reservation
  fails closed with `NO_SAFE_CAPACITY` instead of overcommitting.
- Commit-message correction: the `2c5b065` message says the reservation is
  released and then the epoch bumped. The code and this plan do the reverse
  (bump the epoch, then drop the reservation), which is the required order;
  see `publish_created_allocation`.

**Acceptance.**

- A deterministic test parks `lvcreate` inside the fake runner. While it is
  parked, an attach on a *different* volume completes; a second create with
  the same id is refused or resolves by the documented intent path, never by
  spending the same capacity concurrently; a grow admission sees the
  create's reservation and does not admit against the same bytes.
- The create crash windows still behave: an unknown `lvcreate` outcome keeps
  the durable intent; a known failed start clears it; a size-verification
  failure keeps it; an adoption is idempotent.
- The existing create/epoch negative controls pass unchanged.
- `cargo fmt --all --check`, workspace clippy `-D warnings` and the full
  workspace test suite pass.

### E3 — measurement (not a code claim)

Run the published harness unchanged on a clean tree at the exact commit that
contains E1+E2:

```
sudo scripts/bench/run-all.sh \
  --run-dir /tmp/opencode/volvisor-bench-phase-e-<date> \
  --skip-build --sweep-seconds 30 --soak-minutes 25
```

Compare against the `4290b84` capacity-domain bundle, which is the current
measured envelope after the option C revert. Publish the bundle only if the
tree is clean, the binary digest matches the source, and every measured
JSON/CSV is copied unmodified. State the result honestly: a regression, a
neutral result, or a gain. Do not claim a throughput gain from a micro
create/delete change; the mixed sweep has no create/delete, so a gain there
would be host/run drift, not this phase.

The 24-hour soak remains **NOT RUN**; the run is the shortened 25-minute
soak.

**E3 result (2026-11-05).** Measured at
`2c5b0659a68b5ce6b9286c5c1cd1df7a92f8ebb1` on a clean tree with the release
binary SHA256
`267283b2d0ce62b81cdc8595c8e5f2495f986e5e59a1148bf0c8a4eac5f0f682`,
against `4290b84`. The published bundle is
[2026-11-05-devhost-phase-e-write-lock-scope](../benchmarks/2026-11-05-devhost-phase-e-write-lock-scope/REPORT.md).
The result is a **measured mixed-throughput regression**: c8
3,846.899 → 3,756.659 ops/s (−2.35%), c32 2,609.257 → 2,476.245 (−5.10%),
c128 655.326 → 610.988 (−6.77%), c1 −0.14%; the 25-minute soak
7,134,530 → 6,626,734 ops (4,756.348 → 4,417.815 ops/s, −7.12%). The c128
overall p95 (117.870 → 100.558 ms) and read p95 (6.584 → 4.691 ms) fell, but
the aggregate c128 write p95 (5,078.906 → 4,297.266 ms) is a grow-population
quantile in both runs and the candidate's grow p50/p95 rose; it is not a
like-for-like latency gain. Micro create/delete p50 rose 5.5%/7.1% in this
single sample and is reported as neutral/unattributed.

The mixed sweep and soak contain no create/delete calls, so **none of these
deltas is attributed to E1/E2**. E3 therefore does not support a Phase E
throughput or latency gain. The recorded residuals remain: delete's `lvs`
check and create's `pick_pool_vg` `vgs` sample stay inside their admission
critical sections, and the failed-create compensation's `lvremove` stays in
the reacquired critical section. Create and delete also still run their
synchronous LVM bodies inline on async workers (only grow uses the blocking
handoff; move's request path runs its CLI calls inline), so sustained
structural concurrency at or above the worker count can pin the runtime —
pre-existing, not introduced by E1/E2. The gate tests exercise same-kind
duplicate refusal; the cross-kind create-vs-delete refusal is implemented by
the shared map but not directly gate-tested.
`#62` stays open; no gate verdict changes.

## Out of scope

- The online move drive (`pvmove`) and its lock scope.
- Asynchronous operation handles or grow/delete SLOs.
- #37/#38/#39 hardware, D3 storage-side fencing, D8 host loss.
- #47 signing key/tag/release, #48 CHV pin.
- #49's 14 consecutive nightly runs and published flake rate.
- #50's named owners and two-maintainer sign-off.
- Any push, tag, PR or issue-state mutation.

## Order of work

1. **E1 (delete)** — self-contained; the durable intent already fences the
   other structural operations.
2. **E2 (create)** — needs the create capacity reservation and the
   create/delete drive guard.
3. **E3 (measurement)** — only after E1 and E2 pass tests, lint and review.

`#62` stays open until explicitly instructed. All gate verdicts are
unchanged: gates 1, 2 and 5 **NOT MET**; gates 3 and 4 **PARTIAL**; overall
**NOT PRE-PRODUCTION READY**.
