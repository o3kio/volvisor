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

### D3 — #62 write-path hardening (slice 1 delivered; slice 2 next)

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
  sequence durable. The win is one file `sync_all` plus one directory
  `sync_all` for N covered waiters. Publication moves from `StateGuard`
  drop to a post-durability, sequence-tagged snapshot; the failure-keep
  branches that rely on drop-publication are migrated explicitly here.
- **Publication is post-durability and monotonic (slice 2).** The read
  view is sequence-tagged; a lower sequence never replaces a higher one. A
  failed uncovered sequence is never published.
- **Capture monotonicity.** In-memory state is never rolled back between
  captures, so every later capture is a superset of every earlier one.
- **Waiter resolution is guaranteed.** No waiter parks without a
  barrier-guaranteed exit. A caller may be answered with an error only after
  the pipeline proves that no pending capture contains its mutation.
- **Failure barrier.** Stop accepting captures; drain or fail in-flight
  leaders; reconcile each unresolved sequence against the durable file
  (proven-landed → success-with-note; proven-absent → error; unreadable or
  unprovable → poison); then align in-memory state exactly to the durable
  file. Revert-to-snapshot is legal only at this quiesce point. Poison only
  the unprovable case; a provably-not-durable failure keeps serving.
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
  durable file proves it; no resurrection after an error (the journal's
  terminal-replay rule); leader-kill wake-all; quiesced align on an
  uncovered failure; the post-durability publication window is tested;
  monotonic publication; both fsyncs counted; every crash seam still
  fires.
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
5. **D3 slice 2 (release the lock) — next.** The only batch that may claim a
   throughput change, and only with the new clean-tree measurement. #62 stays
   open until both land.
6. D4 only when the accepting authority is recorded.
