# ADR-0012 — Fence attachment instances by a strictly increasing generation

Status: Accepted (decision accepted 2026-11-06 by zoorpha; implemented in the
Phase C state-substrate slice)
Decision-accepted: 2026-11-06 by zoorpha
Date: 2026-11-04
Revises: [Volume API v2](../../contracts/volume-api-v2.md) §3 attachment-state
reporting (the retired-id-tombstone mechanism, not the evidence rules)
Related: [ADR-0003](0003-tiered-volume-virtualization.md),
[ADR-0013](0013-durable-attachment-generation-allocator.md) (Accepted:
revises the generation source and the "first attach is `1`" sentence;
implementation pending, the report/detach fence here is unchanged),
[SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md),
[readiness report](../releases/pre-production-readiness.md),
[Phase C plan](../plans/2026-11-04-phase-c-state-substrate-verification.md),
issues [#62](https://github.com/o3kio/volvisor/issues/62),
[#63](https://github.com/o3kio/volvisor/issues/63), [#64](https://github.com/o3kio/volvisor/issues/64)

## Context

Volume API v2 §3 requires a provider that advertises
`attachment_state_report` to refuse an attachment id that the volume consumed
earlier in its lifetime (`IDEMPOTENCY_CONFLICT`), so a delayed report from a
retired instance can never be attributed to a new one. That requirement was
written when `attachment_generation` was a constant `1` for every attachment:
the id tombstone was the only thing that distinguished a retired instance from
a new one.

An exact tombstone is unbounded in the volume's lifetime attach count, which is
issue #63. The Phase A fix replaced it with a fixed-size Bloom filter. A
post-fix single-host benchmark exposed the consequence: after a volume churns
through a few thousand attaches, the filter saturates, every fresh id is a
false positive, and every new attach is refused with
`IDEMPOTENCY_CONFLICT`. That is fail-closed but it makes a legitimately churned
volume unusable, and it produced 1.41 million spurious conflicts in a 25-minute
run. The raw records for that invalid run are preserved in
[the pre-fix invalid-run artifact](../benchmarks/2026-11-04-devhost-invalid-prefix/README.md);
they are diagnostic only and are **not** capacity evidence.

A probabilistic filter cannot satisfy the contract's intent: "refuse a consumed
id" must not also refuse an id that was never consumed, and its false-positive
rate grows without bound with churn.

## Decision

**The authoritative fence for attachment-instance evidence is a strictly
increasing per-volume `attachment_generation`.**

- Each new attachment instance records the volume generation it consumed (a
  value already persisted on every volume), then the volume generation
  advances. Generations are therefore strictly greater for every later
  instance on that volume, including after detach and across restarts. The
  first attach on a fresh volume is still `1`.
- A report or detach validates `expected_attachment_generation`; a retired
  instance's generation is `STALE_GENERATION` against any later instance. A
  report or detach for an id that is not the current instance is `NOT_FOUND`.
- A provider **must accept a re-attach of a previously consumed id as a new
  instance with a strictly greater generation**, and must not refuse it solely
  because the id was consumed. `IDEMPOTENCY_CONFLICT` remains reserved for a
  live instance with the same id but a different payload. A per-id tombstone
  is optional diagnostics, never the fence.
- The LVM provider never inserts into or consults the schema-v3
  `retired_attachment_filter` from the attach/detach path. The legacy v1/v2
  `retired_attachment_ids` vector is still imported into the filter at decode,
  so an old file loads without loss. The type and field remain decodable (no
  schema bump: the field is optional and is omitted while empty). The
  conformance fake stops retaining the retired-id set.

The token is a `u64`. Reaching `u64::MAX` attaches on one volume is not
reachable in any realistic lifetime; the implementation does not wrap-check,
and this ADR records that bound rather than treating an impossible overflow as
a tested path.

## Why this is not a weakening

`attachment_generation` was previously constant, so the tombstone was the only
protection against a reused id. With a strictly increasing generation:

- a delayed report from a retired instance is always stale, even if the
  consumer reuses the id;
- a delayed detach is always stale, so it cannot release a newer writer;
- an attach is still fenced by `expected_volume_generation`, which advances on
  detach, so a stale attach cannot resurrect an instance.

Single-writer admission, journal idempotency, generation checks and
fail-closed error reporting are unchanged. The new rule is strictly stronger
than a constant token plus a bounded/ probabilistic tombstone at the boundary
where the tombstone forgets.

## Consequences

- A volume can sustain attach/detach churn without unbounded durable state or
  false-positive refusals (issue #63's operational failure).
- Consumers must treat `attachment_generation` as opaque and strictly
  increasing per volume; they must not assume it is `1` or contiguous.
- A provider that only implements the constant-generation shape does not
  conform to this ADR; it must use a strictly increasing per-volume source.
  An exact tombstone is not a substitute.
- Historical evidence bundles that demonstrated the retired-id `409` remain
  records of the pre-Phase-C behavior; they are not evidence for the current
  fence. The day-2 D1 drill is updated to demonstrate the generation fence.

## Alternatives rejected

1. **Unbounded exact tombstone.** Contract-compliant but grows with every
   attach and is rewritten/fsynced in the whole-state file, reintroducing the
   write amplification that #62 measured.
2. **Fixed-size Bloom filter (Phase A).** Bounded, but false positives refuse
   fresh ids and saturate under churn; the benchmark showed a churned volume
   becoming effectively unusable.
3. **Bounded exact recent window only.** Avoids false positives, but silently
   forgets old ids; it still needs the generation fence to be safe, and adds
   state for no additional safety.

## Non-claims

This decision is a control-plane fencing change validated by unit, conformance
and single-host loop-LVM tests. It is not production-support evidence, an SLA,
a data-path claim or a multi-host/real-hardware qualification.
