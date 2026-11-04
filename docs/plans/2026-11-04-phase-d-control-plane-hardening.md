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

### D3 — #62 write-path design (implementation staged)

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

**Design of record (to be reviewed before code).**

- A single durable-writer critical section (leader coalescing) separate from
  the state mutex. A mutation captures `(sequence, encoded_state)` while it
  holds the state lock, records the sequence, publishes the read snapshot
  only after durability, and releases the lock.
- A committer that acquires the writer section writes the **latest**
  captured encoding, `fsync`s, renames and directory-`fsync`s once, then
  marks every covered sequence durable. Waiters whose sequence is covered
  return success without another fsync (group commit).
- A failed write leaves the covered sequences unresolved; no sequence is
  reported durable without a completed file+directory `fsync`. A sequence
  covered by a later successful encoding is durable even if its own write
  lost the race, and is reported as such.
- Publication is monotonic: a lower sequence never replaces a higher
  published snapshot. A failed uncovered sequence reconciles against the
  durable file; if reconciliation cannot prove the outcome the provider
  fails closed (poisoned/`INTERNAL`) rather than guessing.
- The conversion is atomic per operation class. Start with
  attach/detach/report (no LVM CLI, one save), then decide separately
  whether create/grow/delete/move can share the model without weakening
  their intent/compensation contracts.

**Acceptance for the eventual code slice (not this batch).**

- New deterministic tests: coalesced commits produce one fsync for N
  waiters; a covered failure returns success only when the durable file
  contains the mutation; an uncovered failure reverts or poisons and never
  publishes; snapshot publication never regresses; the store-crash seams
  still fire at every boundary.
- Full workspace tests, journal fault tests and the conformance suite pass.
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
records D1/D2 as delivered, D3 as design-only, and keeps gates 1, 2 and 5
NOT MET with gates 3 and 4 partial.

## Out of scope

- #37 three-physical-host nearline evidence, #38 Rook-cell POC, #39 green
  Ceph/DRBD rig run, D3 storage-side fencing, D8 host loss.
- #47 signed release and clean-host install; #48 CHV pin update.
- #49's 14 consecutive nightly runs and published flake rate.
- #50's named per-area owners and two-maintainer release/restore sign-off.
- Any GitHub issue, tag, push or pull-request mutation.

## Order of work

1. D1 (log volume) — self-contained, no protocol change.
2. D2 (read posture) — self-contained, requires test and doc updates.
3. D5 (ledger).
4. D3 design review; implementation is a separate batch.
5. D4 only when the accepting authority is recorded.
