# ADR-0014 — Journal retention bounds are a contract, with fail-closed exhaustion

Status: Accepted (decision accepted 2026-11-06 by zoorpha; the local
hard-ceiling implementation landed under
[#64](https://github.com/o3kio/volvisor/issues/64), which remains open for the
evidence work)
Decision-accepted: 2026-11-06 by zoorpha
Date: 2026-11-06
Related: [ADR-0012](0012-monotonic-attachment-generation-fence.md),
[SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md),
[journal retention config](../../examples/volvisor.toml),
[Phase F evidence](../benchmarks/2026-11-06-devhost-phase-f-write-path-remainder/REPORT.md),
issue [#64](https://github.com/o3kio/volvisor/issues/64)

## Context

The journal now has bounded terminal retention, an exact tombstone ring,
atomic compaction, replay eviction, compaction backoff and configuration
wiring. Phase F re-confirmed the code and tests and recorded six residual
questions that the bounds do not answer by themselves:

1. in-flight intents are never evicted, because they are recovery state;
2. "expiry" is a count window, not a TTL;
3. a persistent compaction failure backs off but leaves the log growing;
4. the terminal registry can reach twice `max_retained_operations` between
   compactions, and the one-frame planner can reduce effective retention;
5. a 64-bit fingerprint collision can answer `OPERATION_EXPIRED`;
6. the observed log high-water can sit above the configured byte trigger.

These are not all defects. Some are necessary consequences of correctness.
The architecture must say which are the contract, which are soft targets, and
what happens when the soft target cannot be met.

## Decision

**The configured journal values are steady-state targets; terminal retention
is bounded; in-flight intents are authoritative; and exhaustion is
fail-closed, never a silent drop.**

1. **Terminal operations are bounded.** Terminal records and their
   fingerprints are retained only within `max_retained_operations` and
   `max_tombstones`. Eviction is deliberate and recorded in the tombstone
   ring.
2. **In-flight intents are never evicted.** An intent is the only durable
   evidence that a mutation may have reached storage. Evicting it would
   silently discard a possible side effect. In-flight cardinality must be
   bounded by mutation admission and by the one-frame budget. Today the
   one-frame budget bounds the snapshot, not admission. **Implemented
   (`JR-12`):** if the in-flight set cannot fit, compaction fails closed with
   a typed error, new mutating work is refused (`cannot_fit`) until the set
   drains or an operator resolves it, and outcome appends remain available so
   the outstanding outcomes can drain. No best-effort eviction. `JR-04` still
   pins that a failed compaction alone never fails an already-authorized
   append that fits under the ceiling.
3. **Expiry is a count window, not a TTL.** A retry whose terminal record is
   older than the newest `max_tombstones` evictions is indistinguishable from
   an unknown operation and may execute as a new operation. Consumers must
   not rely on indefinite replay. This is the durability contract, not an
   accident of the implementation.
4. **Soft bounds are intentional.** The `2x max_retained_operations`
   hysteresis and the one-frame planner may keep the live registry above the
   configured steady state or reduce effective retention to fit one frame.
   `max_log_records` and `max_log_bytes` are compaction triggers, not hard
   ceiling guarantees.
5. **A persistent compaction failure must not exhaust the disk.** A hard
   ceiling is added above the byte trigger. When `max_log_bytes > 0`, the
   default ceiling is twice that value and must be configured at or above the
   trigger. When the byte trigger is disabled (`max_log_bytes = 0`), the
   ceiling must be set explicitly; a zero ceiling is invalid. When the log
   reaches the hard ceiling and compaction still cannot make progress, new
   durable appends are refused with a typed `JOURNAL_FULL` (mapped to the
   API's overload/503 surface), while reads and replay continue. The refusal
   is counted in a metric and logged at error level. The daemon does not
   delete terminal records, in-flight intents or tombstones to get under the
   ceiling.
6. **Fingerprint collisions fail closed.** The tombstone ring stores exact
   64-bit fingerprints. A different operation with the same fingerprint is
   treated as expired and refused. The collision probability is accepted;
   an unbounded exact-id store is rejected for the same reason as an
   unbounded tombstone.
7. **Observed overshoot is not a violation.** The compacted snapshot can
   itself be at or above the byte trigger, and the post-compaction growth
   floor deliberately prevents rewriting it on every append. A high-water
   above `max_log_bytes` is therefore expected; only the hard ceiling is a
   stop condition.

## Required observability

The daemon must expose, at minimum:

- current journal bytes and the configured trigger/hard ceiling;
- compaction attempts, successes and failures (the existing
  `journal_compaction_failures_total` at least);
- in-flight intent count;
- hard-ceiling refusals, with a distinct reason code;
- the newest tombstone window bound and the current terminal count.

An alert on approaching the hard ceiling is an operational mitigation, not a
substitute for the fail-closed refusal.

## Consequences

- A new configuration key and validation rules are required. A hard ceiling
  below the trigger is invalid.
- A persistently failing compaction becomes a visible, bounded outage for
  mutations rather than an unbounded disk leak. Reads and recovery remain
  available.
- Tests cover: refusal at the ceiling, recovery after compaction succeeds,
  no eviction of in-flight intents, the typed error surface, and the
  cannot-fit refusal of new mutating work. The cannot-fit test preserves
  `JR-04`: a failed compaction alone still never fails an already-authorized
  durable append that fits under the ceiling.
- The existing replay, tombstone, compaction and config tests remain in the
  default scenario catalog.
- This is a control-plane storage-bound decision. It is not production
  capacity, SLA, data-path or durability evidence.

## Alternatives rejected

1. **Evict in-flight intents under pressure.** Rejected: it can discard a
   possible storage side effect and break recovery.
2. **Unbounded exact tombstones.** Rejected by #64: durable state grows with
   every operation and is rewritten on every save.
3. **A TTL instead of a count window.** A TTL adds clock dependence and still
   needs a storage bound; it does not answer what happens to an in-flight
   intent.
4. **Alert-only on persistent compaction failure.** Rejected as the sole
   control: an alert does not bound a disk that is already filling.
5. **Delete the log and restart empty on failure.** Rejected: it destroys the
   replay and idempotency record.

## Implementation status

The terminal bounds, tombstone ring, replay eviction, compaction backoff and
config wiring are implemented and covered by the default scenarios. The local
hard-ceiling implementation also landed under `#64`: `hard_ceiling_bytes` is
configurable (default twice the byte trigger; explicit non-zero required when
the trigger is disabled; at least the trigger), a durable append that would
cross it gets one forced compaction attempt and is then refused with the typed
`JOURNAL_FULL` (HTTP 503) without evicting in-flight intents, terminal records
or tombstones. A cannot-fit in-flight/pinned set closes **new-intent**
admission (`cannot_fit`) while outcome appends remain available to drain it,
and a later successful compaction reopens admission. `/metrics` exposes the
in-flight/hard-ceiling gauges, `journal_compaction_attempts_total`,
`journal_cannot_fit` and `journal_refusals_total{reason}`. The default catalog
pins `JR-11` (ceiling refusal and recovery) and `JR-12` (cannot-fit admission
and drain); `JR-04` still covers "a failed compaction alone never fails an
already-authorized append that fits under the ceiling".

`#64` remains open: no real-backend, crash-consistency-on-real-media or
24-hour soak evidence has been collected, and the Phase F report records those
residuals. See the readiness report and the
[remediation plan](../plans/2026-11-06-open-issue-remediation.md).

## Non-claims

This decision does not claim that the current 25-minute soak bounds RSS, that
the journal is unbounded in normal operation today, or that a hard ceiling
has been measured. It is a design decision and an implementation obligation,
not production evidence.
