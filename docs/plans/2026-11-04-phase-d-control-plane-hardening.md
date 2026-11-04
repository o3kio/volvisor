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

### D3 — #62 write-path hardening (slice 1 delivered; slice 2 code landed, measurement pending)

**Gap.** Attach, detach, report, create, grow, delete and move all take
`LvmProvider::state` and hold it across `LvmState::save_with_hooks`
(serialize, write tmp, `sync_all`, rename, directory `sync_all`). Reads were
already decoupled by the committed snapshot, but writers still serialize on
the fsync. The post-fix run shows c32 −4.1 % and c128 −2.6 % versus the
pre-fix baseline; the ceiling is unchanged.

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
durability; synchronous mutations still lead inline under the lock. The
deterministic tests cover coalescing into one physical write, landed-failure
success only when the durable file proves it, unproven failure without
poison, unreadable durable fail-closed, monotonic publication, panic wake-all
and armed-kill re-raise, abandonment, gate poisoning, capture order, admission
alignment, and the report no-op. The post-implementation review below then
found and fixed a critical leadership-handoff race and a high
phantom-resurrection path; the tests now also cover the atomic handoff, the
poisoned-refusal no-resurrection case, the async armed-kill re-raise and the
post-durability publication window. Only the newest pending capture is
retained; the monotonic snapshot supersedes earlier captures, so one write
covers every earlier sequence while waiters resolve by watermarks. **There is
no throughput claim:** the required clean-tree benchmark at the final
slice-2 SHA has not been run, so the measured-baseline claim in the section
above is not updated by this change set. #62 stays open until that run
exists, and all gate verdicts are unchanged.

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
   `needs_alignment`, and `capture_and_reserve` restores the last durable view
   on any encode or reservation error. A test poisons the writer, drives two
   same-id attaches, and checks the retry fails, the read view is aligned and
   the durable file never contains the attachment.
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


**Rejected alternatives.** Per-volume files need a multi-file commit for
delete/move/create intent atomicity; an append-only WAL would rewrite every
startup and reconciliation path; write-behind (ack before `fsync`) violates
the attach fencing contract. None is a shortcut for this slice.

**Acceptance for the eventual code slices (not this batch).**

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
  inside `persist`); the post-durability publication window (a reservation or
  capture publishes nothing; only a proven write does); monotonic publication;
  atomic leadership handoff (a reservation at the quiesce boundary becomes
  the next leader, never a stranded waiter); report no-op; every crash seam
  still fires. The writer retains only the newest pending capture: in-memory
  state is monotonic, so one write of the newest snapshot covers every
  earlier sequence, and waiters resolve by permanent watermarks. The
  coalescing test counts one physical-write attempt for N waiting callers;
  the state-layer seam test covers both fsync boundaries (no separate
  fsync counter is added, so this is proof of execution, not a count).
- A new clean-tree benchmark at the implementation SHA is published with
  the same comparator; no throughput claim before that run.
- #62 stays open until the code and the measurement both exist.

### D4 — ADR-0012 acceptance

The generation fence is implemented and measured, but ADR-0012 is still
`Proposed`. Accepting it is an authority decision, not an implementation
task; record the accepting identity and date when that authority acts. This
batch does not self-accept it.

### D5 — ledger update

Add a Phase D addendum to
[pre-production-readiness.md](../releases/pre-production-readiness.md) that
records D1/D2 as delivered, D3 slice 1 as delivered with #62 open, and
keeps gates 1, 2 and 5 NOT MET with gates 3 and 4 partial.

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
5. **D3 slice 2 (release the lock) — code landed; measurement pending.** The
   only batch that may claim a throughput change, and only with the new
   clean-tree measurement. #62 stays open until both land.
6. D4 only when the accepting authority is recorded.
