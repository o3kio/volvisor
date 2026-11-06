# ADR-0013 — Make the attachment-generation allocator durable across volume lifetime

Status: Accepted (decision accepted 2026-11-06 by zoorpha; implementation
pending under [#63](https://github.com/o3kio/volvisor/issues/63))
Decision-accepted: 2026-11-06 by zoorpha
Date: 2026-11-06
Revises: [ADR-0012](0012-monotonic-attachment-generation-fence.md) — the
generation **source**, not the report/detach fence. This revision is
accepted; ADR-0012's report/detach semantics are unchanged, and the
implementation remains pending under #63.
Related: [Volume API v2](../../contracts/volume-api-v2.md) §3,
[ADR-0012](0012-monotonic-attachment-generation-fence.md),
[SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md),
[Phase F evidence](../benchmarks/2026-11-06-devhost-phase-f-write-path-remainder/REPORT.md),
issue [#63](https://github.com/o3kio/volvisor/issues/63)

## Context

ADR-0012 made a strictly increasing `attachment_generation` the authoritative
fence for attachment-instance evidence. Its decision text says the first
attach on a fresh volume is `1`, and the generation advances per volume. The
implementation at `2edcecd` does exactly that: `create_volume` writes
`generation: 1`, and delete removes the volume record.

Phase F verification found the consequence: the fence is **per volume
lifetime**, not per durable volume identity. If a volume is deleted and a new
volume is created with the same `volume_id`, the recreated volume starts again
at generation `1`. A delayed report from an attachment on the old volume can
match generation `1` again if the consumer also reuses the `attachment_id`.
The contract's intent — a retired instance must never be attributed to a new
one — is then not guaranteed by the generation check alone.

The provider cannot close this with a per-id tombstone: attachment ids are
consumer-supplied, the set is unbounded, and ADR-0012 already rejected an
unbounded exact tombstone as a #62/#63 write-amplification regression. The
provider also cannot require consumers never to reuse an id: that is a
consumer-side convention, not a provider-enforced fence.

## Decision

**Use one durable, monotonically increasing attachment-generation allocator
for the whole LVM provider state, not a per-volume counter that resets on
create.**

- The LVM provider state carries a single `next_attachment_generation: u64`.
  Every successful attach allocates the current value for the new instance
  and advances the allocator under the same state lock and durable save that
  records the attachment. The value is never reset by create or delete.
- `attachment_generation` remains opaque. It is strictly increasing for every
  later instance on a volume, and also across delete + same-id recreate. A
  consumer must not assume it starts at `1`, is contiguous, or resets with a
  new volume incarnation. This revises ADR-0012's "first attach is `1`"
  sentence; the report/detach fence and `STALE_GENERATION` behavior are
  unchanged.
- Delete removes the volume and its attachments. It does not remove or reset
  the allocator. Create does not write a literal `1`.
- On decode of a schema-v3 state file, seed the allocator from
  `max(all persisted volume entry generations, all persisted attachment
  generations, any persisted allocator field) + 1`. A v3 file therefore
  upgrades without reusing a generation that is still recoverable from a
  live volume record; the schema bump to v4 follows
  [the compatibility policy](../schema-compatibility.md). An older binary
  must refuse a v4 file typed, not silently reinterpret it.
  **Upgrade-window residual:** a volume deleted before the upgrade has no
  record left, so its consumed generations are unrecoverable from the v3
  file. A delayed report from an attachment retired on such a pre-upgrade
  deleted incarnation can still match if the consumer also reuses the id and
  the value is reissued. This window cannot be closed from the v3 file; it is
  recorded, not hidden. After the upgrade the persisted allocator is the
  authority for every new incarnation.
- At `u64::MAX`, attach fails closed with a typed error. The allocator does
  not wrap. This is a recorded bound, not a tested realistic path.
- The generation fence remains the only authority. No retired-id tombstone is
  reintroduced. The legacy decode-only filter remains decode-only.

## Why this is the right layer

- **It is provider-enforced.** The fence no longer depends on a consumer
  choosing a fresh attachment id after a volume recreate.
- **It is bounded.** One `u64` in the state file, independent of attach
  churn, volume count, or delete/recreate count.
- **It preserves the existing contract shape.** Reports and detaches still
  carry `expected_attachment_generation`; only the source of the number
  changes. `STALE_GENERATION` remains the refusal.
- **It makes delete + recreate explicit.** The new volume incarnation is
  fenced by a generation strictly greater than every prior incarnation of
  that durable id.

## Consequences

- The LVM state schema advances to v4. The migration is decode-and-seed on
  first open plus upgrade-on-write; there is no in-place downgrade.
- Clients that assumed `1` on a fresh volume must stop assuming it. They
  already must treat the value as opaque under ADR-0012; this ADR makes that
  requirement operational.
- The default scenario catalog gains a delete + same-id recreate case that
  asserts a delayed report from the prior incarnation is refused.
- Rollback requires stopping the daemon and restoring a pre-upgrade state
  directory with the matching older binary, as
  [the compatibility policy](../schema-compatibility.md) requires.
- This is a control-plane fence decision. It is not production-support,
  data-path, SLA or multi-host evidence.

## Alternatives rejected

1. **Per-volume durable high-water map keyed by `volume_id`.** Correct, but
   the map grows with the number of distinct deleted volume ids unless it is
   evicted; eviction reintroduces the reuse window. A single allocator is
   bounded and simpler.
2. **Require consumers to use fresh attachment ids forever.** Not enforceable
   by the provider and not fail-closed if a consumer violates it.
3. **Keep the lifetime-scoped counter and document the gap.** That leaves the
   contract's stated intent dependent on consumer behavior, which is the
   defect this ADR closes.
4. **Unbounded exact retired-id tombstones.** Rejected by ADR-0012 and #63 for
   unbounded durable growth and write amplification.

## Implementation status

Not implemented in the Phase F evidence batch. The decision is accepted
(2026-11-06, zoorpha); implementation remains pending under `#63`. The Phase
F result records the gap; this ADR records the decision and the required
migration shape. Until it is implemented, the residual is listed in the
readiness report and `#63` remains open. Any implementation must add the
delete + recreate scenario to the default catalog before merge.

## Non-claims

This decision is a control-plane fencing decision, to be validated by unit,
conformance and single-host loop-LVM tests when implemented. It is not
production-support evidence, an SLA, a data-path claim, a guarantee of zero
RPO, or a multi-host/real-hardware qualification.
