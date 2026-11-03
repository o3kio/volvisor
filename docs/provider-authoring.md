# Provider authoring guide

Status: Working guide (Volume API v2 P0/P1 providers)  
Audience: engineers adding or changing a `VolumeProvider` backend

This guide describes the seam between the Volume API v2 surface and a storage
engine, the semantics every provider must share, and the semantics each
backend owns. It complements the normative
[Volume API v2 contract](../contracts/volume-api-v2.md) and the binding
semantic contract on the [`VolumeProvider`](../crates/volvisor-provider/src/provider.rs)
trait.

The short version: implement backend I/O and durable state in your crate,
implement the narrow shared-semantics traits over your existing records, and
prove behavior with the conformance and parity suites. Do not fork the shared
decisions and do not weaken fencing to make a test pass.

## 1. The provider seam

A provider implements [`VolumeProvider`](../crates/volvisor-provider/src/provider.rs):

| Method | Required behavior |
| --- | --- |
| `name`, `capabilities`, `supported_classes` | Honest identity, capability and class advertisement. Absent capabilities must fail closed downstream. |
| `create_volume` | Create a fresh volume at generation 1, or replay an identical prior creation of the same `volume_id`. The same id with a different payload is `IDEMPOTENCY_CONFLICT`. |
| `inspect_volume` / `list_volumes` | Read-only projections; `NOT_FOUND` for an absent volume, project-scoped filtering for lists. |
| `attach_volume` | Fence the expected volume generation, resolve attachment replay by identity, enforce single-writer admission, and return a host-scoped `Frontend` handle. Evidence starts at `Prepared`. |
| `detach_volume` | Fence the expected attachment generation, require the type-mandatory drain proof, release the writer, and advance the volume generation exactly once. |
| `report_attachment_state` | Optional and capability-gated: apply consumer evidence monotonically and never advance the attachment generation. The default trait implementation is the fail-closed `NOT_FOUND`. |
| `grow_volume` | Grow-only, generation-fenced; reject shrink and unsupported policy typed. |
| `delete_volume` | Require a fully detached volume, no dependents and the correct expected generation; never adopt or destroy foreign state. |

Backend I/O (CLI invocations, ownership proofs, device handles, durable record
layout) stays in the provider crate. The API server sees only this trait.

## 2. Shared semantics core

[`volvisor_provider::semantics`](../crates/volvisor-provider/src/semantics.rs)
contains the decisions that must be indistinguishable across backends:

- expected-generation fencing at volume and attachment scope
  (`ensure_expected_generation`, `ensure_expected_attachment_generation`);
- attach replay by identity (`AttachmentIdentity`,
  `attachment_payload_matches`, `resolve_attachment_replay`);
- single-writer admission (`ensure_no_active_attachment`);
- retired attachment-id rejection (`ensure_attachment_id_not_retired`);
- monotonic consumer-report application (`apply_attachment_report`).

The core is pure: it performs no I/O and does not serialize records. Providers
expose only the facts the decisions need through two narrow traits implemented
over their **existing** record types:

- `AttachmentRecordView` (required): attachment id, VM id, host id, access
  mode, VMM disk id, attachment generation.
- `AttachmentStateRecord` (only for providers advertising
  `attachment_state_report`): current state, revision, evidence, reporter,
  receipt time, and a mutation hook that advances the revision exactly once.

### Attach replay

Resolve replay **before** the generation fence. An attach request that names an
already-recorded instance replays the recorded response even though the volume
generation has since moved; the same attachment id with a different volume, VM,
host, access mode or VMM disk id is `IDEMPOTENCY_CONFLICT`, never a second
attachment.

```rust
let identity = AttachmentIdentity::from_request(volume_id, req, mode);
if let Some(record) = find_recorded_attachment(state, &req.attachment_id) {
    resolve_attachment_replay(recorded_volume_id, record, &identity)?;
    return Ok(attach_response(record));
}
ensure_expected_generation(req.expected_volume_generation, current)?;
ensure_no_active_attachment(has_attachment, volume_id)?;
```

### Detach and reports

Detach fences `expected_attachment_generation`; a consumer report validates the
same generation but never advances it. After a detach, the attachment id is
retired for the volume's lifetime wherever reports are advertised, so a delayed
report from the old instance can never label a new one.

## 3. Backend-specific behavior (do not generalize)

Keep these in your provider; they are not shared semantics:

- device-handle acquisition and release (`rbd map`/`unmap`, DRBD
  promote/demote, LVM has no per-attachment handle);
- ownership proofs and foreign-state classification;
- DRBD writer-authority leases, fencing, and migration cuts;
- Ceph mapping/zombie reconciliation and LVM online-move records;
- tombstone persistence layout (LVM stores a list, the fake a set);
- whether `attachment_state_report` is advertised at all (Ceph and DRBD do
  not advertise it today).

If a new rule looks universal, add it to `semantics.rs` with unit tests and run
the parity suite against every provider. If it is engine-specific, keep it in
the engine crate and cover it with that crate's behavior tests.

## 4. Capabilities and error mapping

Advertise only what the exact implementation version and evidence support.
Absent capabilities must produce a typed refusal, never a silent fallback.

| Situation | Typed error |
| --- | --- |
| Expected volume/attachment generation does not match | `STALE_GENERATION` |
| Same id with a different payload | `IDEMPOTENCY_CONFLICT` |
| Second writable attachment | `WRITER_ALREADY_ACTIVE` |
| Attachment/volume absent for a new operation | `NOT_FOUND` |
| Lifecycle/state precondition not met | `INVALID_STATE` |
| Unsupported class, policy or field | `UNSUPPORTED_CLASS_OR_POLICY` |
| Unresolved or foreign backend state | `FOREIGN_DEVICE_STATE` (quarantine, never adopt) |

Never convert a fencing or replay conflict into success, and never fabricate
`Healthy` health or `Active` attachment evidence that was not observed.

## 5. Durable state and schema policy

Do not change public HTTP payloads or on-disk state schemas to fit a shared
helper. Keep schema v1 records and strict decode (`deny_unknown_fields`) intact.
If a record must gain a field, use the existing schema evolution policy
([schema compatibility](schema-compatibility.md)) and preserve replay of older
records. `AttachmentRecordView` and `AttachmentStateRecord` are read-only
adapters over the current layout; they do not change what is persisted.

## 6. Tests every provider must pass

### Conformance kit

The shared kit in
[`volvisor_provider::conformance`](../crates/volvisor-provider/src/conformance.rs)
covers create/inspect, idempotent create, payload conflicts, single-writer
attach, generation fencing, detach, grow, delete, project filtering, capability
gating, the move-contract rules, and the parity suite below. Run it through the
macro:

```rust
volvisor_provider::provider_conformance_tests! { make_provider }
```

The macro also generates a `parity` test, so a provider using the kit cannot
silently skip the cross-provider checks.

### Parity suite

The parity driver compares **decision classes**, not backend details, across the
shared lifecycle:

```rust
let provider: &dyn VolumeProvider = provider.as_ref();
assert_eq!(volvisor_provider::conformance::assert_parity(provider).await, Ok(()));
```

`assert_parity` runs two sequences:

- **universal** (every provider): create replay/conflict, attach stale/replay,
  same-id different-payload conflict, second-writer refusal, detach and delete
  fencing, `NOT_FOUND` after delete. The observed sequence must equal both the
  shared fixture and the in-memory reference provider.
- **report/tombstone** (capability-conditional): providers advertising
  `attachment_state_report` must apply reports monotonically, answer
  equal/lower reports as no-ops, refuse stale generations, and reject reuse of
  a retired id. Providers without the capability must answer the report route
  with `NOT_FOUND`.

Add the parity test next to the conformance test in your crate (the generated
macro test above does this automatically; call `assert_parity` directly for a
custom harness). It runs in ordinary `cargo test --workspace` with the simulated
worlds, so a divergence fails the default CI run.

### Real backends

The Ceph and DRBD parity/conformance runs use their simulated test worlds. Real
cluster runs remain env-gated (`VOLVISOR_TEST_CEPH`, `VOLVISOR_TEST_DRBD`) and
are **not** executed by the default suite; a passing simulated run is not
evidence of real-cluster behavior. Report real runs separately with the exact
cluster, versions and failure evidence.

## 7. Review checklist

- [ ] New decisions either live in `semantics.rs` with unit tests, or are
      documented as backend-specific.
- [ ] Attach replay resolves before the generation fence.
- [ ] Single-writer, stale-generation and replay-conflict behavior is unchanged
      or stricter.
- [ ] Reports never advance the attachment generation, never downgrade, and
      retired ids are rejected.
- [ ] No HTTP payload or on-disk schema change; strict decode preserved.
- [ ] Conformance and parity tests pass for the simulated world.
- [ ] Real-backend runs are either reported with evidence or explicitly marked
      not run.
