# Open-issue remediation plan (2026-11-06)

Status: working plan. This document maps every open issue to the work that
can be completed locally now, the work that is only partially local, and the
work that is externally blocked. It does not change any readiness gate and it
does not turn an accepted ADR into evidence.

Related: [pre-production readiness](../releases/pre-production-readiness.md),
[production-readiness closure plan](2026-11-06-production-readiness-closure.md),
[default integration scenarios](../testing/default-integration-scenarios.md),
[ADR-0013](../adr/0013-durable-attachment-generation-allocator.md),
[ADR-0014](../adr/0014-journal-bounds-and-exhaustion-contract.md),
[schema compatibility](../schema-compatibility.md).

## Ground rules

- A code change is claimed only when it is implemented, exercised by an exact
  default scenario (or another named test), and has a rollback/refusal path.
- Hardware-, release-, soak- and external-repository-gated work stays open
  with its exact prerequisite recorded. It is never "closed by documentation".
- No measured JSON/CSV evidence is edited. Repeats and diagnostics are not
  evidence.
- The default gate remains 58+ exact scenarios, 60 s per test and 300 s for
  the warm catalog. Long-running proofs belong to scheduled/evidence lanes.

## Locally actionable now

### #63 — durable attachment-generation allocator (ADR-0013)

**Status: local implementation landed; issue stays open for evidence and the
upgrade-window residual.**

Implemented ADR-0013:

- bumped `LVM_STATE_SCHEMA_VERSION` to 4;
- added one durable `next_attachment_generation: u64` (0 means "seed me");
- seeded on decode from `max(entry generations, attachment generations) + 1`
  only when the field is absent/zero, and re-normalized on every save;
- allocated on every successful attach under the same state lock/save, never
  reset on create/delete;
- fails closed at `u64::MAX`;
- added a genuine v3 N-1 fixture (`lvm-state-v4-n1.json`) and a v3→v4 decode
  test (`SC-09`);
- added default scenario `GF-12` (delete + same-id recreate never reissues a
  retired generation);
- updated schema, configuration and ADR wording.

Residual: a volume deleted *before* the upgrade has no record in a v3 file,
so its consumed generations are unrecoverable. The ADR records that window.
Real-backend/multi-host evidence is still required.

### #64 — journal hard ceiling and cannot-fit refusal (ADR-0014)

**Status: local implementation landed; issue stays open for evidence.**

Implemented the remaining ADR-0014 decision:

- added `hard_ceiling_bytes` to `JournalRetention` and `[journal]`; defaults
  to `2 * max_log_bytes`, requires an explicit non-zero ceiling when the byte
  trigger is disabled, and refuses a ceiling below the trigger;
- added typed `JOURNAL_FULL` (HTTP 503) and wired it into the contract, the
  API error table and `from_wire`;
- attempts compaction before refusing an append that would cross the ceiling;
- refuses new durable appends at the ceiling and never evicts in-flight
  intents, terminal records or tombstones to get under it;
- added a cannot-fit admission flag: when the in-flight set cannot fit one
  frame, new intents are refused but outcome appends (draining) stay
  available;
- exposes bytes, trigger, ceiling, in-flight/terminal counts, tombstone window
  and refusal counters on `/metrics`;
- added default scenarios `JR-11` (ceiling refusal, reads/replay continue,
  recovery after unblocking) and `JR-12` (cannot-fit refuses new intents,
  outcomes drain, admission recovers).

`JR-04` remains: a failed compaction alone still never fails an
already-authorized durable append that fits under the ceiling.

### #45 — admission control, timeouts, pagination (remaining slice)

The admission caps, read timeouts, body limits and list pagination are
implemented. The remaining locally-actionable gap is the explicit **resumable
long-operation handle** and **distinct per-route mutation timeouts**, neither
of which exists. Both are durable-semantics/surface additions and require an
accepted contract amendment before implementation; they are recorded here,
not silently added.

### #49 — parser fuzzing and interleaving tests (remaining slice)

The fuzz corpus, compaction interleaving and nightly machinery are present.
The outstanding acceptance criterion is a **14-night flake-rate record**;
that is a scheduled-lane measurement, not a unit-test change. The local work
is to keep the corpus/interleaving suite green and to wire the flake report
format if it is missing.

### #50 — knowledge concentration (local documentation slice)

`ARCHITECTURE.md`, `CODEOWNERS`, a contributor guide and a review checklist
can be written locally. The two-maintainer release/restore sign-off depends
on #46/#47 and cannot be manufactured by documentation.

### #62 — control-plane write serialization (partial)

The current slice removed the gross lock-scope violations and coalesces
saves, but the durable state is still one whole-file rewrite/fsync and the
control plane still serializes on one provider-state lock. A truthful fix
needs a state-substrate redesign (per-record or segmented durable state with
a single-writer commit protocol) and an ADR/amendment. Not a small patch;
kept open.

## Partially local, evidence still missing

### #43 — performance and soak benchmarks

Benchmark harnesses and single-host loop-LVM numbers exist. A real-backend
24 h soak, published production-capacity numbers and an attributed
throughput/regression result are still missing. Prerequisite: a real
backend host plus an evidence lane, not the default catalog.

### #44 — observability

Latency/health/readiness/tracing and the new journal bounds metrics are
local. A real migration trace on real backends and a 24 h soak observation
are evidence-lane work.

### #46 — day-2 runbooks

Backup/restore/upgrade/witness-loss runbooks are written. Drills D3 (storage
fencing), D8 (host loss) and a live upgrade/rollback have not been executed;
they need real storage and at least two hosts.

## Externally blocked

| Issue | Exact prerequisite |
|---|---|
| #37 | Three physical hosts with real nearline devices, a witness, and an executed failure matrix. |
| #38 | Three physical hosts, three storage microVMs, real VFIO, CRUSH physical-host placement and a recorded GO/NO-GO. |
| #39 | Self-hosted Ceph/DRBD runners and a green **scheduled** integration run; the default catalog cannot host them. |
| #47 | Signing key, release tag, clean-host install and a support matrix. |
| #48 | #47 plus the CHV consumer-side change and integration guide. |
| #50 | Two maintainers independently completing a release and a restore drill (depends on #46/#47). |
| #49 | 14 consecutive nightly runs with the recorded flake rate. |

## Order of work

1. #63 (ADR-0013) — schema v4, allocator, `GF-12`.
2. #64 (ADR-0014) — ceiling, `JOURNAL_FULL`, cannot-fit, metrics,
   `JR-11`/`JR-12`.
3. Documentation truth pass: schema/configuration/testing/readiness.
4. Run the default gate, `release-scope.py check`, formatting and CI.
5. Report the remaining issues with the prerequisites above; do not close
   them without evidence.
6. Execute the [production-readiness closure plan](2026-11-06-production-readiness-closure.md)
   for the hardware, release, security, operability and consumer gates; it
   creates no evidence and changes no verdict until the runs land.

## Non-claims

This plan, and the implementation it tracks, is not production-support
evidence, an SLA, a data-path claim, a zero-RPO statement, a hardware or
multi-host qualification, or a release approval. Gate verdicts are unchanged:
1/2/5 remain **NOT MET**, 3/4 remain **PARTIAL**, overall **NOT
PRE-PRODUCTION READY**.
