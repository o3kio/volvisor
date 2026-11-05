# Phase D — control-plane throughput and log-volume hardening

Status: normative for this workstream (the plan of record; implementation
commits cite it)
Date: 2026-11-04
Branch: `phase-a-scaling` (Phase C committed at `813de99`)
Builds on: [Phase C plan](2026-11-04-phase-c-state-substrate-verification.md),
[post-fix benchmark bundle](../benchmarks/2026-11-04-devhost-postfix/REPORT.md),
[pre-production readiness report](../releases/pre-production-readiness.md),
[tracker #36](https://github.com/o3kio/volvisor/issues/36),
[AGENTS rules 4, 8, 11 and 12](../../AGENTS.md).

## What this Phase D is — and is not

Phase C fixed the attachment-id regression and bounded the journal on disk,
then published an honest single-host comparison. Its own residuals are the
input to Phase D:

- the control-plane write path still holds one provider-state mutex across a
  whole-file rewrite and `fsync` ([#62](https://github.com/o3kio/volvisor/issues/62));
- the default INFO log still emits several lines per operation and the
  benchmark captured 2.57 GB to an unrotated file in ~30 minutes
  ([#64](https://github.com/o3kio/volvisor/issues/64) residual);
- the legacy `admin_token`-only posture left reads open, the last named
  transport/auth limitation in gate 3 (closed by D2 in this batch).

This Phase D is a **hardening batch**, not the multi-host, signed-release or
consumer work that #36 still gates. It does not create hardware evidence and
does not close #62 by assertion.

## Non-claims

- No production-support, SLA, capacity, zero-RPO, durability or release
  claim.
- No real Ceph, DRBD, NVMe, multi-host or data-path evidence.
- No claim that #62 is fixed until the write path is changed, tested and
  re-measured. A design document alone closes nothing.
- No 24-hour soak and no published flake rate.
- No tag, GitHub release, push, pull request or issue-state change.
- No change to the five readiness gate verdicts.

## Slices

### D1 — log volume and retention (#64 residual)

**Gap.** Every consumer mutation logs an `accepting <op>` line at INFO before
dispatch and the request audit emits a second INFO line afterwards. Neither
line is a durable record; the journal is. On the 2026-11-04 run the daemon
wrote 2,572,981,477 bytes over ~30 minutes, almost all of it this pair.

**Deliverables.**

- The pre-dispatch `accepting <op>` lines in
  `crates/volvisor-api/src/handlers.rs` move to DEBUG. The request audit
  line, provider failure logs and every journal event stay at INFO or above.
- The successful provider-completion line in `crates/volvisor-api/src/ops.rs`
  moves to DEBUG; a failed provider call stays at INFO.
- `docs/operations/observability.md` records the default volume, the
  `RUST_LOG` override and the fact that the packaged systemd unit writes to
  journald (which owns rotation). A file-capture example uses `logrotate
  copytruncate`; the daemon does not claim an internal rotating file sink.
- The readiness ledger records the measured reduction without claiming a
  bound on all workloads.

**Acceptance.**

- `cargo fmt --all --check`, clippy and the `volvisor-api` tests pass.
- The audit event and the failure paths still log at INFO.
- A benchmark rerun is **not** part of this slice; the next measurement uses
  the existing bundle and counts lines per operation.

### D2 — close the legacy admin-token read gap (gate 3 partial)

**Gap.** `AuthPolicy::protect_reads` returns true only when scoped
`[credentials]` exist. With only the legacy top-level `admin_token`, every
read route (`/v2/volumes`, `/v2/volumes/{id}`, `/v2/capabilities`,
`/metrics`, migration inspection) is anonymous. The admin token already
exists to protect mutations; the same credential can protect reads.

**Deliverables.**

- `protect_reads` returns true when **any** credential is configured,
  including the legacy admin token. A tokenless daemon keeps the documented
  loopback-dev open-read posture and still fails closed for mutations.
- A new test proves: legacy-token-only rejects anonymous reads with
  `UNAUTHORIZED`, accepts the token, and leaves `/healthz`,
  `/v2/healthz`, `/readyz` and `/v2/readyz` public.
- `docs/security/deployment.md`, `docs/operations/observability.md` and the
  `v0.1.0` release notes describe the new posture and the migration step:
  read clients must send the token.
- This is a security posture change in an unreleased prototype, not a
  compatibility guarantee. It removes a documented limitation rather than
  adding a new feature.

**Acceptance.**

- The existing scoped-role matrix and the tokenless tests still pass.
- No token ever appears in a log or an error body.

### D3 — #62 write-path hardening (slice 1 and slice 2 delivered; capacity-domain grow admission measured at `4290b84`; c128 grow tail open)

**Gap.** Attach, detach, report, create, grow, delete and move all take
`LvmProvider::state` and hold it across `LvmState::save_with_hooks`
(serialize, write tmp, `sync_all`, rename, directory `sync_all`). Reads were
already decoupled by the committed snapshot, but writers still serialize on
the fsync. The post-fix run shows c32 −4.1 % and c128 −2.6 % versus the
pre-fix baseline; the ceiling is unchanged. The slice-2 run recorded below is
the post-change measurement.

**Why this is not a mechanical edit.** Releasing the lock before the save
breaks two current invariants: a failed save must not be observable as
committed, and a later save must not erase an earlier mutation that is
already durable. The create/grow/delete/move paths also use multi-save
compensation and durable intents, so a partial conversion would mix two
commit models.

**Adversarial review (2026-11-04): NO-GO as first written.**
An independent review read the full `persist()` funnel, `StateGuard`,
`revert()`/`reconcile_failed_save()`, the compensation flows and the crash
seams. It confirmed the leader-coalescing shape but rejected the initial
"convert attach/detach/report while the rest stays synchronous" cut. The
blocking findings:

1. **Durable-file regression (critical).** A legacy direct save that lands
   between a pipeline capture and the leader's write is erased when the
   leader writes the older capture. The conversion cut is wrong.
2. **`.tmp` collision (critical).** `sibling_tmp_path` is a fixed
   `<path>.tmp`; a mixed model lets two writers truncate or delete each
   other's temp file.
3. **Publication and rollback (critical).** `StateGuard`'s drop publishes
   uncommitted state when the lock is released early; `revert()` restores
   the last durable snapshot and would erase other waiters' pending
   in-memory mutations. The existing helpers assume one save per guard.
4. **Failure protocol (high).** A failed leader can strand waiters, and a
   later covering write can resurrect a mutation whose caller was already
   told it failed. The API journal records provider failures terminally, so
   a false failure is not recoverable by retry.
5. **Leader panic (high).** A panic or armed crash seam in the leader parks
   every waiter unless wake-all-on-poison is explicit.
6. **Coverage invariant (high).** "Sequence ≥ mine is durable" is sound only
   if captures are monotonic supersets and in-memory state is never rolled
   back between captures.
7. **Runtime blocking (medium).** The simple mutations run synchronously on
   tokio workers; parking ~c128 workers on a condvar would starve `/healthz`
   and unrelated routes.
8. **Crash-seam determinism (medium).** Coalescing changes the physical save
   count that `arm_save_failure(..., skip)` observes, and
   `AfterRenameBeforeDirFsync` must keep one consistent classification.

**Corrected design of record.** The following are hard invariants, not
options:

- **One durable writer for every save.** `persist()` — including admin
  claim/release and the compensation paths — routes through the single
  writer from the first slice. No class keeps a second direct-file path.
  Per-operation staging may only change *whether the caller releases the
  state lock before waiting*.
- **Slice 1 is a semantic no-op.** All callers still hold the state lock
  across the wait; captures therefore never overlap. Each physical write
  uses a bounded, path-hashed temp name
  (`.volvisor-<16-lowercase-hex-fnv1a64>.<pid>.<sequence>.tmp`, so it
  cannot exceed `NAME_MAX`) plus a startup sweep. The hash is over the
  canonical state-path identity (the canonicalized parent plus the file
  name). The writer gate keys on that identity; the in-flight registry
  keys on the canonical identity of each temp path (the same
  canonicalization scheme, a different key), so a restart that spells the
  parent differently (symlink, `..`, `./`) recognizes its predecessor's
  temps. The sweep never removes a temp
  registered in flight by this process, removes same-pid leftovers
  immediately, removes a foreign-pid temp only after a one-hour grace, and
  treats a regular legacy `<state>.tmp` like a foreign writer (it has no
  pid component, so it is removed only when its mtime is older than that
  grace; a fresh one may belong to an old binary during a rolling restart
  or to an operator's copy). Non-regular entries are skipped;
  cross-process exclusion remains the journal flock, so two daemons must
  not alias a custom `lvm_state_path`. Publication remains `StateGuard`
  drop in slice 1, and the failure-keep branches that rely on it are
  unchanged. Two intermediate temp shapes were never released:
  `f0e5237`'s `<path>.<pid>.<sequence>.tmp` and `a8c23d6`'s
  lexical-path-hash namespace. For an already-canonical spelling (the
  common case) the final sweep recognizes the `a8c23d6` namespace, so
  already-stale same-pid or aged leftovers are removed under the pid/grace
  policy; a fresh foreign-pid leftover still waits out the one-hour grace.
  Only a leftover created through a non-canonical spelling (symlink, `./`,
  `..` or a bare relative path) is orphaned by the canonical-identity hash
  change. The `f0e5237` shape is genuinely never recognized. Those
  unrecognized leftovers need manual removal. All existing suites pass; two
  failure-injection rigs were re-mechanized onto the injected `AfterTmpWrite`
  consult because unique temp names removed the fixed `.tmp` path they used
  to block (see the slice-1 acceptance note). Physical save counts are
  unchanged. **No
  throughput claim from slice 1.**
- **Slice 2 is the release-the-lock change.** Attach/detach/report capture
  `(sequence, encoded bytes, snapshot)` under the lock, release it, and
  wait. A leader writes the latest capture once and marks every covered
  sequence durable, so the intended work is one file `sync_all` plus one
  directory `sync_all` for N covered waiters. For async captures,
  publication is a post-durability, sequence-tagged snapshot; lock-holding
  synchronous callers keep drop publication unless their caller reconciles
  a failed save. No throughput claim follows from the design alone.
- **Publication is post-durability and monotonic (slice 2).** The read
  view is sequence-tagged; a lower sequence never replaces a higher one. A
  failed uncovered sequence is never published.
- **Capture monotonicity.** In-memory state is never rolled back between
  captures, so every later capture is a superset of every earlier one.
- **Waiter resolution is guaranteed.** No waiter parks without a
  watermark-guaranteed exit. A caller may be answered with an error only after
  the pipeline proves that no pending capture contains its mutation.
- **Failure barrier (corrected by the slice-2 review below).** There is no
  transient barrier and no quiesce. An unresolved sequence resolves only
  through the permanent watermarks: `durable_seq` (success),
  `failed_through` (the recorded failure), or terminal poison. A
  readable-but-different or unreadable durable file fails every outstanding
  sequence, sets `needs_alignment`, and publishes nothing; it does **not**
  poison. Poison is reserved for a leader panic, leader abandonment or a
  poisoned mutex. In-memory state is aligned only at `lock_state` or by a
  caller's `reconcile_failed_save`, never inside `persist`.
- **Leader panic wakes everyone.** A panic or armed kill sets a writer
  poison flag, wakes all waiters with a typed failure, and mirrors today's
  mutex-poison behavior. The rig must still observe kills at the same task
  boundary.
- **Crash seams stay inside the physical write.** All four
  `StoreSavePoint` consults remain. `AfterRenameBeforeDirFsync` keeps one
  classification standard for both the waiter answer and reconciliation.
  Concurrent rigs that assert exact save counts are audited and updated.
- **Async-safe waiting.** Consume the wait on `spawn_blocking` or use an
  async-aware notification; do not pin tokio workers on a std condvar.

#### Slice-2 design review (2026-11-04): NO-GO as first proposed

A second independent review read the slice-2 proposal against the delivered
slice-1 writer, the compensation paths and the crash rigs. It rejected the
proposal as written. The proposal still mixed two commit models: it released
the lock for some callers while leaving rollback, publication and failure
classification on the single-save/`StateGuard` assumptions, and it treated
"the leader wrote the latest capture" as sufficient without stating the
sequence and watermark rules that make coverage sound. The blocking findings
and the corrections that are now **hard requirements**:

1. **Reservation order (critical).** A sequence reserved outside the provider
   state lock can invert with a concurrent mutation, so a later sequence
   could omit an earlier caller's mutation. Capture **and** reservation happen
   under the provider state lock; reservation order must equal mutation order.
2. **One sequence/watermark space (critical).** The leader's latest-write
   coverage is sound only if every caller's capture joins one shared,
   monotonic sequence and watermark space. Per-caller or per-call-type
   counters are not acceptable.
3. **Permanent watermarks, no transient barrier (critical).** A waiter may
   resolve only when `durable_seq` covers it, `failed_through` covers it, or
   the writer is terminally poisoned. A transient "barrier"/"in-flight" flag
   can lose a wake or strand a waiter after the flag clears.
4. **Alignment is admission, not persistence (critical).** Alignment may
   happen only in `lock_state` or a caller's `reconcile_failed_save`, never
   inside `persist`. A save must not change the state it is about to encode,
   and a caller that already holds the lock must not silently re-read under
   it.
5. **No state lock in the leader failure path (critical).** A synchronous
   waiter may hold the provider state lock while the leader classifies a
   failure; if the leader takes that lock, it deadlocks. The leader never
   takes the provider state lock, on success or failure.
6. **Poison is terminal, divergence is not (critical).** Poisoning on every
   unprovable save turns a recoverable, alignable divergence into a dead
   provider. A readable-but-different or unreadable durable file fails every
   outstanding sequence and sets `needs_alignment` **without** poisoning.
   Poison only on leader panic/abandonment or a poisoned mutex (and on a
   poisoned physical-write gate, which cannot be serialized safely).
7. **Gate and lineage scope (high).** The physical-write gate is per
   canonical state path in this process; watermarks and pending captures are
   per provider lineage. Two live providers aliasing one state path are
   unsupported, even though their gate is shared. Different paths must not
   share watermarks.
8. **Report no-op (high).** An attachment report saves only when
   `response.applied`; a no-op report must not reserve a sequence, write, or
   wake waiters.
9. **One wake helper (high).** Wake both waiter classes from one helper
   (`Condvar::notify_all` plus `tokio::sync::Notify::notify_waiters`). Async
   waiters subscribe before re-checking the watermarks, so a wake between
   the check and the await is not lost.
10. **No pinned async worker (high).** The leader's blocking write loop runs
    on `spawn_blocking`; a waiter awaits `Notify`, not a pinned tokio worker
    or a std condvar on an executor thread.
11. **Armed-kill payload stays in band (high).** A leader panic (including an
    armed crash seam) is re-raised by `commit_async` with `resume_unwind`, so
    the campaign rigs still observe the original payload. The writer is
    poisoned and every waiter woken with a typed failure before the re-raise.
12. **Leadership is RAII (high).** A `LeaderToken` disarms only on a resolved
    leader; dropping an armed token (panic, cancellation, abandonment)
    poisons the writer and wakes every waiter.

**Delivery status of this change set.** The code implements the corrected
design: `state.rs` splits encoding from the physical save so a capture can be
written later without the state reference; `provider.rs` replaces the
single-shot `DurableWriter` with a per-lineage coalescing writer and per-path
gate; attach/detach/applied-report capture under the lock, drop it, and await
durability; synchronous mutations still lead inline under the lock. Admission
is gated on the permanent watermarks: an async admission resolves an
outstanding capture without holding the state lock, and a synchronous
admission does the same while holding it, before it derives any replay,
not-found or no-op answer. The deterministic tests cover coalescing into one
physical write, landed-failure success only when the durable file proves it,
unproven failure without poison, unreadable durable fail-closed, monotonic
publication, panic wake-all and armed-kill re-raise, abandonment, gate
poisoning, capture order, admission alignment, admission waiting across a
pending-replay window, and the report no-op. The post-implementation review
below then found and fixed a critical leadership-handoff race and a high
phantom-resurrection path; the re-review of `1184a27` found and this change
fixes a high same-id replay window and a consistency gap in synchronous
admission. Only the newest pending capture is retained; the monotonic
snapshot supersedes earlier captures, so one write covers every earlier
sequence while waiters resolve by watermarks. **The required clean-tree
benchmark has now run at `c4b0b13`** (2026-11-04T23:10:13Z–23:40:32Z, worktree
clean, comparator `aab773b`); it is published as
[2026-11-04-devhost-slice2](../benchmarks/2026-11-04-devhost-slice2/REPORT.md).
The mixed ceiling is unchanged and the high-in-flight write tail is worse, so
**this work makes no throughput-gain claim**; see the measurement section
below. #62 stays open until explicitly instructed, and all gate verdicts are
unchanged.

#### Slice-2 implementation review (2026-11-04): NO-GO on `8220414`

An adversarial review of the implemented slice-2 commit found two defects the
green suite did not exercise, plus lower-severity items. The findings and
their fixes in this change set:

1. **Leadership handoff race (critical).** `run_leader` checked "no pending
   capture" and cleared `leader_active` in two separate writer-lock critical
   sections. A reservation between them enqueued itself as a waiter after the
   leader had decided to exit, so the waiter could park forever; the
   synchronous flavor waits while holding the provider state lock, which
   deadlocks the mutation path. Fix: `leader_step` takes the no-pending
   decision and the leadership release in one critical section, so a new
   reservation either joins a leader that will write it or becomes the next
   leader. A deterministic regression test holds a reservation at that exact
   boundary.
2. **Phantom resurrection after terminal poison (high).** `reserve` refused a
   poisoned writer without requiring admission alignment. An async
   attach/detach could then leave its failed in-memory mutation visible to a
   same-id retry, which the idempotency replay answered as success while the
   durable file had no attachment. Fix: a poisoned refusal sets
   `needs_alignment`, and `capture_and_reserve` restores the last published
   (proven-durable) view on any encode or reservation error. A test poisons
   the writer, drives two same-id attaches, and checks the retry fails, the
   read view is aligned and the durable file never contains the attachment.
3. **Unproven leader success (low).** `finish_leader` returned `Ok(())` when
   no watermark resolved its own sequence. It now fails closed with a typed
   `INTERNAL` and requires admission alignment.
4. **Memory amplification (low).** The writer retains only the newest pending
   capture (with its sequence). In-memory state is monotonic, so the newest
   snapshot is a superset and one write covers every earlier sequence; the
   permanent watermarks resolve the earlier waiters. This bounds retained
   capture memory to one encoded buffer plus one snapshot per provider
   lineage.
5. **Accepted residual: synchronous waiters block a runtime thread.** The
   remaining synchronous `persist` callers (create/grow/delete/move/admin)
   still hold the provider state lock and wait on a std `Condvar`. If such a
   call runs on a tokio worker and loses leadership, that worker parks for the
   leader's physical write. This is not a regression from slice 1 (those
   callers previously performed the whole fsync on the worker);
   attach/detach/report are async-safe on `spawn_blocking`/`Notify`.
   Converting the multi-save compensation paths is a separate change.
6. **Accepted residual: runtime-shutdown cancellation is terminal poison.** A
   `spawn_blocking` `JoinError::is_cancelled` is treated as terminal poison.
   It can only occur when the runtime is shutting down (the process is
   ending), and it is the fail-closed choice; an abandoned leader token also
   poisons by RAII.

#### Slice-2 re-review (2026-11-04): NO-GO on `1184a27`; findings fixed here

A third independent review read the fix commit against the journal replay
rules, the admission paths and the alignment code. It confirmed the four
`1184a27` fixes were real and non-tautological, but found one high
correctness window and one consistency gap the tests did not cover, plus
three documentation/claim corrections. This change fixes all of them:

1. **Same-id replay window (high).** Attach mutated in-memory state, reserved
   under the state lock, then dropped the lock before durability. A second
   caller with the same attachment id acquired the lock in that window, found
   the attachment in the uncommitted in-memory view, and returned a success
   replay. The journal replays a recorded success byte-for-byte, so if the
   covering write then failed unproven the journal held a terminal success
   for an attachment that was never durable. Detach and report had the
   mirror image: a not-found or no-op answer could be derived from an
   uncommitted removal or report.
   Fix: every async admission first reads the writer's `AdmissionGate` under
   the state lock. `Wait(seq)` drops the lock and awaits that capture's
   permanent watermark without pinning a worker; `Align` re-enters
   admission so the next `lock_state` reloads the durable file. A covering
   failure is not this caller's failure: the loop re-aligns and re-executes,
   so a detach/attach is retried from the durable view instead of answering
   from the failed mutation. `Ready` cannot be invalidated afterwards,
   because a new capture requires the state lock the admission still holds
   and a failure requires an outstanding capture.
2. **Sync admission could observe an uncommitted async mutation (high).**
   Synchronous create/grow/delete/move/admin admission took `lock_state`
   while an async attach/detach/report capture was pending. It could read an
   uncommitted generation/attachment bump and make a terminal decision
   (a stale-generation refusal, an idempotent replay, a delete fence) that
   the later covering failure invalidated. Fix: synchronous admission now
   uses `lock_state_stable`, which holds the state lock and waits on the
   permanent watermarks before returning a view a decision may rely on. The
   wait cannot deadlock: the leader never takes the state lock, and holding
   it prevents a new capture from being reserved. This is the same
   accepted-residual class as synchronous `persist` waiters.
3. **Alignment vs. outstanding capture (high).** `lock_state` could reload
   the durable file while a capture was still outstanding. That file
   predates the capture's snapshot, so the reload could point in-memory state
   below a snapshot that was about to be published (reachable when an encode
   failure set `needs_alignment` while an older capture was pending).
   Fix: alignment is skipped while `latest.is_some()`
   (`needs_idle_alignment`), and `clear_needs_alignment` refuses to clear
   while a capture is outstanding. The admission gate resolves the capture
   first; only then does alignment reload the file that now includes (or
   excludes) the resolved capture. The `latest`-only monotonicity claim
   therefore holds even when `encode_for_save` fails: the failed mutation is
   never allowed to hide an older in-flight snapshot.
4. **Documentation truth (medium/low).** The readiness ledger claimed "no
   production behavior changed after `a068947`: everything since is
   test/doc-only evidence correction"; that is false once slice 2 landed. It
   now scopes the statement to the interval between `a068947` and the
   slice-2 implementation (`8220414`). The plan names `1184a27` as the first
   review-fix commit instead of referring to an anonymous final SHA, and the
   benchmark requirement now names the closing commit that will be recorded
   with the measurement instead of leaving the SHA unidentified. The
   `capture_and_reserve` text says "last published view" (what the code
   restores), not "last durable view". The new admission-wait test is named
   in the slice acceptance list.
5. **Test for the replay window (evidence).** A deterministic test persists a
   durable baseline, holds the per-path physical-write gate, arms a one-shot
   pre-rename save failure, and starts the first attach. Once that attach is
   blocked inside its physical write, it starts a same-id attach, proves the
   replay has not finished and does not hold the provider state lock, and
   proves the durable file still has no attachment. It then releases the
   gate: the first write fails unproven, and the replay must re-align,
   re-execute and make the attachment durable (exactly one failed and one
   successful physical write). Without the gate loop the replay finishes
   while the first write is still blocked, so the test is not tautological.


#### Slice-2 clean-tree measurement (2026-11-04): recorded at `c4b0b13`

The required run is published as
[2026-11-04-devhost-slice2](../benchmarks/2026-11-04-devhost-slice2/REPORT.md).
Source `c4b0b1303594c494bb26d3643d952434eda8ec34`, worktree clean
(`worktree_dirty: no`), binary `volvisord c4b0b13` SHA256
`1f86136093792c161e85d9c8924bc965b635e54d157d975a9780a2d2ddc81bf6`, run
window 2026-11-04T23:10:13Z–23:40:32Z, same harness shape (`--skip-build
--sweep-seconds 30 --soak-minutes 25`). The comparator is the clean-tree
`aab773b` [post-fix bundle](../benchmarks/2026-11-04-devhost-postfix/REPORT.md);
both manifests verified. **No code or harness change was made after the
measured SHA**; the documentation change that records this run is a
follow-up commit.

Measured results:

- **Mixed sweep:** 1845.526 / 3410.746 / 858.371 / 237.088 ops/s at
  1/8/32/128 in-flight, versus 1952.155 / 3493.445 / 862.937 / 235.502 at
  `aab773b` (−5.5 / −2.4 / −0.5 / +0.7 %). The ceiling is unchanged, so no
  throughput-gain claim is made.
- **Read/write split:** read p95 at c32 fell 179.332 → 1.188 ms and at c128
  1288.505 → 197.815 ms; write p95 rose 388.84 → 590.847 ms at c32 and
  1552.261 → 6330.548 ms at c128. Overall p50 fell (462.278 → 1.005 ms) while
  overall p95 rose (1349.487 → 3553.448 ms). Reads were already snapshot
  reads at both SHAs; the split is consistent with async waiters no longer
  pinning runtime workers while synchronous `grow` still holds the state lock
  across an LVM resize. The harness does not attribute delay; the cause is an
  observation, not a proven diagnosis.
- **Soak:** 6,929,746 ops at 4619.824 ops/s versus 5,424,744 at 3616.49
  (+27.7 %), with the same recorded actual read ratio (0.7999), 0 HTTP
  errors and 0 attachment conflicts; the 77 network errors are client
  reconnects at the planned restart. RSS delta was +2,943,816 KiB (peak
  3,236,184 KiB) versus +1,780,896 KiB (peak 2,323,028 KiB). The soak is a
  single non-fixed-dataset run: the rate is recorded, not claimed as a
  capacity improvement.
- **Bounded artifacts:** state ended at 3,401 B (max 3,684 B); journal ended
  at 43,412,309 B (max 75,497,689 B). The mid-soak restart listed 4/4
  volumes in 0.547 s with unchanged sizes.
- **Diagnostic probes:** cap-32/128-in-flight shed 77 × `503` under its
  configured 1 s queue timeout; the 50 req/s probe returned 896 × `429`.
  These are diagnostic shapes, not the default-cap sweep.

New residuals from this measurement:

- **Write-tail attribution (open).** c32/c128 write p95 is worse than the
  baseline while read p95 is much better and throughput is flat. The next
  step is to attribute the stall (synchronous grow, coalescing/fairness, or
  the harness mix) before any regression/fix claim.
- **RSS high-water (open).** The peak rose to 3,236,184 KiB. A leak is
  neither proven nor ruled out; no cap or fix is claimed.
- **Post-poison alignment reload (accepted, low, pre-existing).** After
  terminal poison, `lock_state` may still load the durable file before
  `reserve` refuses. This is fail-closed and leftover work at worst; it is
  left unchanged here so the measured code SHA stays frozen. No correctness
  impact is claimed.
- **24-hour soak (NOT RUN).** The run is 25 minutes with one restart.

#### Grow-lock fix and re-measurement (2026-11-05): recorded at `8029df8`

The slice-2 measurement left the write-tail cause unattributed. A
grows-disabled diagnostic then isolated it: with grow work removed from the
mixed workers, c128 write p95 fell **6.33 s → 0.57 s** and throughput
**237 → 2,522 ops/s**; c32 write p95 fell **590 → 85 ms**. The serializer was
`grow_volume_inner` holding the provider state mutex across the long
`lvextend` command (≈200 ms median): every other volume's admission queued
behind one resize. The diagnostic is not published bundle evidence.

The fix is committed locally at `8029df8e8f6e1856499927df6f434422894005ad`:

- Grow is split into **prepare** (capacity check, in-memory per-VG
  reservation, durable grow intent) under the state lock, **`run_lvextend`**
  and size verification with the lock released, and **commit** (reacquire
  the lock, re-validate the exact intent and volume generation, persist the
  observed size).
- The durable grow intent remains the crash-recovery anchor. Fresh attach,
  detach, delete, move start and move completion are refused typed while it
  is live; **reports stay admitted** (they never move the generation or
  size). A second concurrent grow of one volume is refused by a non-blocking
  drive set.
- The in-memory per-VG reservation preserves the capacity-check-plus-
  allocation atomicity the coarse lock used to provide for creates and
  other grows.
- A non-zero `lvextend` exit clears the intent only when `lvs` proves the LV
  is still at its from-size.
- The move reconcile pass defers a verified completion for the one growing
  volume (`deferred_while_growing`) and continues classifying the rest,
  instead of aborting the pass.

Static checks at the fix SHA: `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets --locked -- -D warnings`, full
`cargo test --workspace --locked` and
`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked` all
pass. Three adversarial review rounds plus two final re-reviews returned GO
with no BLOCKER/HIGH/MEDIUM findings; only the LOW/NIT residuals recorded
below remain.

The re-measurement is published as
[2026-11-05-devhost-slice2-fix](../benchmarks/2026-11-05-devhost-slice2-fix/REPORT.md).
Source `8029df8e8f6e1856499927df6f434422894005ad`, worktree clean
(`worktree_dirty: no`), binary `volvisord 8029df8` SHA256
`18f260c2ef878985f23fdcc15a659d36db87ff7209477756e98638ff0e28b941`, run
window 2026-11-05T09:29:39Z–10:00:20Z, same harness shape (`--skip-build
--sweep-seconds 30 --soak-minutes 25`). The primary comparator is the
clean-tree regression run `c4b0b13`
([slice-2 bundle](../benchmarks/2026-11-04-devhost-slice2/REPORT.md)); both
manifests verified. **No code or harness change was made after the measured
SHA**; the documentation change recording this run is a follow-up commit.

Measured results:

- **Mixed sweep:** 1729.154 / 3118.787 / 1830.005 / 440.376 ops/s at
  1/8/32/128 in-flight. Against `c4b0b13` that is −6.3 / −8.6 / **+113.2 /
  +85.7 %**. The c1/c8 drop is recorded and not attributed.
- **Write tail:** c32 write p95 **590.847 → 206.203 ms** (−65.1 %, now below
  the `aab773b` baseline of 388.84 ms); c128 write p95
  **6330.548 → 3413.886 ms** (−46.1 %, still about 2.2× the `aab773b`
  baseline of 1552.261 ms). The harness does not attribute the residual
  c128 tail, so no claim is made that it is fully eliminated.
- **Reads:** read p95 1.481 ms at c32 and 87.276 ms at c128, versus
  179.332 / 1288.505 ms at `aab773b`.
- **Soak:** 6,640,950 ops at 4425.586 ops/s, with 0 HTTP errors and 0
  attachment conflicts; the 46 network errors are client reconnects at the
  planned restart. RSS delta was +2,126,132 KiB (peak 2,538,456 KiB) versus
  +2,943,816 KiB (peak 3,236,184 KiB) at `c4b0b13`. State ended at 3,147 B
  (max 3,950 B); journal ended at 75,496,834 B (max 75,497,693 B, consistent
  with the documented compaction hysteresis). The mid-soak restart listed
  4/4 volumes in 0.264 s.
- **Diagnostic probes:** cap-32/128-in-flight shed 16 × `503`; the 50 req/s
  probe returned 960 × `429`. Diagnostic shapes, not capacity results.

**What this measurement supports.** The grow-lock serialization is gone:
c32/c128 throughput recovers sharply and the c32 write tail is below the
pre-regression baseline. **What it does not support:** a blanket
throughput-gain claim (c1/c8 fell; the c128 write tail remains above the old
baseline), a capacity/SLA claim, or a claim that the write-tail work is
finished.

New residuals from the fix (LOW/NIT unless noted; none is a production
claim):

- **Residual c128 write tail (open).** Write p95 is still 3413.886 ms.
  Candidate contributors (the grow intent's prepare/commit fsyncs, the
  synchronous writer funnel, the harness mix) are not attributed by the
  harness.
- **Create/delete still hold the state lock across `lvcreate`/`lvremove`
  (pre-existing, out of this change).** The mixed harness does not create or
  delete inside the sweep, so this run does not measure them.
- **Move admission capacity still ignores grow reservations (pre-existing,
  M2).** Not fixed here.
- **A live grow intent with an absent or mid-size LV needs a same-request
  retry, a restart, or an operator.** A non-zero/unknown `lvextend` outcome
  keeps the intent; the reconcile pass defers a settled move for that volume
  but does not starve other records.
- **Post-poison alignment reload and the unbounded INFO log remain**
  (pre-existing; see the slice-2 residuals and the benchmark report).
- **24-hour soak (NOT RUN).** The run is 25 minutes with one restart.

#62 stays open until explicitly instructed. All gate verdicts are unchanged.

**Rejected alternatives.** Per-volume files need a multi-file commit for
delete/move/create intent atomicity; an append-only WAL would rewrite every
startup and reconciliation path; write-behind (ack before `fsync`) violates
the attach fencing contract. None is a shortcut for this slice.

**Acceptance for the code slices.**

- Slice 1: all existing workspace, journal-fault, conformance and campaign
  suites pass; a direct-save-vs-pending-capture interleaving is impossible
  by construction; unique-temp generation, the legacy and foreign-temp
  grace (including a fresh legacy `<state>.tmp` surviving a sweep) and
  cleanup are tested; the state layer proves a real pre-rename
  `create_owner_only` I/O failure (a missing parent and a directory
  target) and the forced `AfterTmpWrite` consult covers the provider
  classification; no save-count drift in single-writer rigs. Accepted
  residuals: no provider-end-to-end rig now drives a genuine EACCES/ENOSPC
  on temp creation; the state-layer tests now cover the create-failure
  mapping through `save_with_hooks` (a missing parent) and
  `create_owner_only`, and the forced `AfterTmpWrite` rig covers
  classification. Per-save `canonicalize()` calls remain inside the locked
  region; slice 2 inherits that note. A pathological state file named like
  its own temp is theoretically self-swept (a 2^-64 fixed point) but is
  not practically reachable. The symlink test assumes a symlink-capable
  `TMPDIR` and panics (not skips) where the filesystem refuses symlinks
  (for example some FAT/NFS mounts); `d5.sh` Part B was not live-drilled at
  this SHA in this environment, so its `.volvisor-` message assertion is
  code-derived until the next drill. The slice-1 review loop is complete:
  every adversarial-review finding is either closed or explicitly recorded
  above as a residual.
- Slice 2: deterministic tests for covered-failure success only when the
  durable file proves it; no resurrection after an error, including a
  poisoned refusal (the journal's terminal-replay rule); leader-kill wake-all
  and armed-kill payload re-raise, including through async `commit_async`;
  admission/`reconcile_failed_save` alignment on an uncovered failure (never
  inside `persist`); admission never derives a replay, not-found or no-op
  answer from an outstanding capture: the async paths await the permanent
  watermark without holding the state lock and the synchronous paths use
  `lock_state_stable`; the pending-replay window test holds the physical
  write, proves a same-id replay is parked and the durable file has no
  attachment, then fails the covering write and proves the replay
  re-executes and makes the attachment durable; the post-durability
  publication window (a reservation or capture publishes nothing; only a
  proven write does); monotonic publication;
  atomic leadership handoff (a reservation at the quiesce boundary becomes
  the next leader, never a stranded waiter); report no-op; every crash seam
  still fires. The writer retains only the newest pending capture: in-memory
  state is monotonic, so one write of the newest snapshot covers every
  earlier sequence, and waiters resolve by permanent watermarks. The
  coalescing test counts one physical-write attempt for N waiting callers;
  the state-layer seam test covers both fsync boundaries (no separate
  fsync counter is added, so this is proof of execution, not a count).
- The clean-tree benchmark at `c4b0b13` is published with the same comparator
  ([bundle](../benchmarks/2026-11-04-devhost-slice2/REPORT.md)); the ceiling
  is unchanged and the high-in-flight write tail is worse, so no
  throughput-gain is claimed. Delivered.
- #62 stays open until explicitly instructed.

#### Capacity-domain grow admission, adversarial review, and re-measurement (2026-11-05): recorded at `4290b84`

The `8029df8` re-measurement left the c128 write p95 at 3,413.886 ms and did
not attribute the remainder. The grows-disabled diagnostic had already
isolated the tail to grow work. The remaining artificial serialization was
the capacity sample: `vgs` ran behind a capacity-domain mutex, so concurrent
grows convoyed, and a create could spend capacity between a grow's sample
and its reservation.

`a3ea5abf` samples capacity outside the provider state lock; an allocation
epoch invalidates a sample that raced with any allocation; the per-VG
reservation and durable grow intent publish under the state lock; a capped
optimistic-retry budget turns sustained churn into a typed refusal instead
of a loop; a per-volume drive guard refuses a duplicate grow; and
`run_blocking_body` hands the worker's core off for the synchronous LVM
body. `6519068` removed the provider-level grow semaphore after a
same-commit A/B diagnostic measured the bounded arm at lower c128
throughput and longer grow latency while the LVM-bound completion rate
stayed roughly constant.

**Adversarial review (2026-11-05).** Two independent read-only reviews read
the delta `8029df8..6519068` and the system design. Neither found a Critical
or High runtime-correctness defect on the success path. They did find two
Medium defects, both fixed at `4290b8425baf35f73f5df80b76fc9a3a75f39583`
before any measurement of the final tree:

- **Create-attempt epoch window (Medium).** A create whose `lvcreate`
  outcome was unknown, exited non-zero, failed size verification, or was
  adopted on retry could spend capacity without bumping the allocation
  epoch. A grow that sampled before the attempt could reserve against the
  pre-attempt figure. The fix bumps the epoch on every post-`lvcreate` exit
  path and whenever a durable create intent is found on retry; it is
  conservative on an attempt that never reached LVM (one extra sample).
  Three integration tests cover the unknown outcome, the known failed
  start, and adoption; all three fail on the unfixed parent as negative
  controls.
- **Publish error path (Medium).** The grow publish discarded
  `lock_state_stable()`'s `Result`, so on a poisoned state mutex or an
  unreadable alignment load it bumped the epoch and released the
  reservation without the state guard and with no observable error. The
  fix matches the result, keeps the bump and drop on both paths, and emits
  a structured warning when the lock cannot be held. The unlocked drop is
  safe today because a create holds the state mutex across its sample and
  reservation read, and a poisoned mutex also refuses new creates; the
  warning keeps the degraded path visible.

The reviews also corrected the record rather than the code:

- The `a3ea5abf` commit message said its regression tests covered "the
  retry cap" with negative controls. The cap **is** exercised by a unit
  test (`sustained_epoch_churn_refuses_after_the_attempt_cap`), but it was
  not part of the negative-control set; the message overstates that. The
  provider-level `OVERLOADED` is raised after the journal intent and is
  therefore the **terminal** outcome for that `operation_id`; a retry needs
  a new id. The constant, test and error detail now say so.
- The plan-of-record and readiness ledger ended at `8029df8` while the tree
  carried later production changes; this addendum and the readiness
  addendum close that gap.
- The bulkhead removal's "bounded by the admission cap" is a **count**
  bound only. The cap is shared with reads and attaches, mutations have no
  execution timeout, and rate limiting is off by default. A sustained grow
  flood can shed reads/attaches with `503` after the queue timeout, and
  `max_concurrent_requests` can be raised to 65536 without an LVM
  cross-check. This is documented in
  [admission-control.md](../admission-control.md); the
  semaphore is not re-added, because the A/B showed it did not raise the
  LVM-bound completion rate.
- The c128 aggregate write p95 is a quantile of a bimodal mixture sitting
  near the 5 % grow-share boundary. It flips between the non-grow and grow
  populations and must not be compared head-to-head across SHAs. The
  report publishes the per-operation split, realized grow share, grow
  completion rate, and two same-SHA repeat sweeps.

Static checks at `4290b84`: `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets --locked -- -D warnings`, full
`cargo test --workspace --locked`, and
`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked` pass.
The three new create/epoch negative controls fail at the unfixed parent.

**Clean-tree re-measurement (2026-11-05, `4290b84`).** The run used the
published harness unchanged (identical hashes at `8029df8`), source clean,
binary `80ecd137…`, window 12:18:30–12:49:23Z. Against `8029df8`, throughput
rose 1,729.154 → 1,825.419 ops/s at c1 (+5.6 %), 3,118.787 → 3,846.899 at
c8 (+23.4 %), 1,830.005 → 2,609.257 at c32 (+42.6 %), 440.376 → 655.326 at
c128 (+48.8 %, completion-inclusive; the first 30 s window completed 851.3
vs 439.3 ops/s), and the 25-minute soak 4,425.586 → 4,756.348 (+7.5 %). The
non-grow tail collapsed: c32 write p95 206.203 → 82.927 ms (−59.8 %), c128
attach/report/detach p95 3,200–3,642 → 194–211 ms, c128 read p95 87.276 →
6.584 ms (−92.5 %), c128 overall p95 1,836.718 → 117.870 ms (−93.6 %). All
274,352 default-cap sweep requests returned 2xx; the soak ran 7,134,530 ops
at 4,756.348 ops/s with 0 HTTP/provider errors, 0 conflicts and 81 client
reconnects at the planned restart. The c128 **aggregate write p95** rose
3,413.886 → 5,078.906 ms (+48.8 %) but is **not like-for-like**: the
candidate's 5,078.906 ms is a grow value (slowest non-grow 819.067 ms), the
comparator's 3,413.886 ms is a non-grow value (slowest grow 600.611 ms).
Grow completions rose 255 → 438 and the LVM-bound completion rate 8.43 →
11.12/s, while the grow queue deepened (p50 338 → 8,339 ms, p95 499 →
19,633 ms). **The c128 grow tail is not fixed**; it is bound by the host's
LVM-per-VG serialization.
Two clean same-SHA repeats (`--skip-soak`, non-evidence) measured c128
599.405 and 614.158 ops/s with aggregate write p95 7,478.820 and
7,586.304 ms (both grow values; non-grow p95 179–228 ms), versus the
published 655.326 ops/s and 5,078.906 ms. They agree on the non-grow path
and the seconds-long grow queue, and differ in both completion-inclusive
throughput and the aggregate p95 because the realized grow share sits near
the 5 % boundary. The published sample was not selected from them. The
bundle is published at
[2026-11-05-devhost-capacity-domain-fix](../benchmarks/2026-11-05-devhost-capacity-domain-fix/REPORT.md).
The earlier `6519068` measurement is superseded and is not evidence for
`4290b84`.

New residuals and corrections from this work (LOW/NIT unless noted; none is
a production claim):

- **c128 grow queue/tail (open).** Grow completions are LVM-per-VG-bound;
  128 simultaneous resizes queue for seconds. The aggregate c128 write p95
  is composition- and run-sensitive.
- **Shared-cap grow starvation posture (open).** A grow flood can occupy
  the shared admission cap and shed reads/attaches; rate limiting is
  disabled by default, and the cap has no LVM cross-check. Documented, not
  solved.
- **Create still holds the state lock across its `vgs` sample and
  `lvcreate`/`lvs` (pre-existing, understated before).** This is the same
  defect class the epoch protocol removed for grow; the mixed sweep has no
  create/delete, so it is not measured here.
- **In-doubt journal intents accumulate across restarts (pre-existing).**
  A client disconnect or crash after the intent and before the outcome
  leaves `OPERATION_IN_DOUBT` permanently; recovery reconciles provider
  state, not journal intents. The runbook requires a new operation id.
- **Retry `resume_pending_grow` and `settle_failed_lvextend` run `lvs`
  under the state lock; `intent_is_durable` re-reads the durable file under
  the lock (pre-existing).** Admitted, not measured.
- **`block_in_place` worker-set cost is unmeasured; the LocalSet fallback
  pins the caller's thread and prints the runtime panic hook's refusal
  message.** Correctness fallback for embedders; `volvisord` uses the
  multi-thread runtime.
- **Duplicate-grow refusals are journaled terminal failures**, so
  same-volume spam is a journal write-amplification vector (bounded by
  compaction).
- **Cumulative-run RSS/journal effects are not attributed.** The c128
  segment starts with much higher RSS/journal than a fresh-daemon
  diagnostic; the cause is not claimed.
- **The default INFO daemon log is unbounded** (2.24 GiB raw here).
- **24-hour soak (NOT RUN).** The run is 25 minutes with one restart.

#62 stays open until explicitly instructed. All gate verdicts are unchanged.

### D4 — ADR-0012 acceptance

The generation fence is implemented and measured, but ADR-0012 is still
`Proposed`. Accepting it is an authority decision, not an implementation
task; record the accepting identity and date when that authority acts. This
batch does not self-accept it.

### D5 — ledger update

Add a Phase D addendum to
[pre-production-readiness.md](../releases/pre-production-readiness.md) that
records D1/D2 as delivered, D3 slices 1 and 2 as delivered with the slice-2
measurement recorded and #62 open, and keeps gates 1, 2 and 5 NOT MET with
gates 3 and 4 partial.

## Out of scope

- #37 three-physical-host nearline evidence, #38 Rook-cell POC, #39 green
  Ceph/DRBD rig run, D3 storage-side fencing, D8 host loss.
- #47 signed release and clean-host install; #48 CHV pin update.
- #49's 14 consecutive nightly runs and published flake rate.
- #50's named per-area owners and two-maintainer release/restore sign-off.
- Any GitHub issue, tag, push or pull-request mutation.

## Order of work

1. **D1 (log volume) — delivered.** Self-contained, no protocol change.
2. **D2 (read posture) — delivered.** Self-contained, tests and docs updated.
3. **D5 (ledger) — delivered.**
4. **D3 slice 1 (single-writer funnel) — delivered** (`f0e5237`,
   `a8c23d6`, `a068947`, `913c347`, `aae0e09`, and the follow-up test/doc
   corrections). Semantic no-op: callers still hold the
   provider state lock across every save; no coalescing; no
   wire/schema/durability change; no throughput claim; all existing suites
   green and physical save counts unchanged.
5. **D3 slice 2 (release the lock) — delivered and measured at
   `c4b0b13`.** The clean-tree run is published: the mixed-sweep ceiling is
   unchanged, read p95 falls sharply while write p95 rises at high in-flight,
   and no throughput-gain is claimed. #62 stays open until explicitly
   instructed.
6. D4 only when the accepting authority is recorded.
