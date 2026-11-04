# Volvisor volume API contract v2

Status: Proposed — normative for SPEC-0002; partially implemented (see per-section notes)
Version: 2
Date: 2026-10-09
Related: [ADR-0003](../docs/adr/0003-tiered-volume-virtualization.md), [SPEC-0002](../docs/specs/SPEC-0002-volvisor-volume-virtualization.md), [Nearline contract v2](nearline-replication-v2.md)

## Purpose and compatibility

An O3K/CellHV consumer manages **logical volumes** independently of physical disks, Ceph OSDs and VM attachments. The v1 `local-direct` contract was whole-device VFIO; v2 `native-local` is a new logical-volume class, not an in-place renaming or compatible synonym. Do not interpret v1 calls as v2 without explicit migration and negotiation.

Canonical class values:
- `native-local`
- `nearline-replicated`
- `ceph-rbd`

`pci-passthrough` is an optional independent **physical-device attachment profile**, never an alias for `native-local`.

## 1. CreateVolume

Illustrative interface, not a wire-protocol declaration:

```json
{
  "api_version": "volvisor.volume.v2",
  "operation_id": "opaque-unique-idempotency-key",
  "project_id": "tenant",
  "volume_id": "vol-opaque",
  "class": "nearline-replicated",
  "size_bytes": 107374182400,
  "logical_block_size": 4096,
  "provisioning": "thin",
  "placement": {"preferred_host_id": "host-a", "failure_domain": "host"},
  "local_protection": {"mode": "mirror", "min_healthy_legs": 2},
  "replication": {"engine": "drbd9", "mode": "async", "remote_replicas": 1, "allow_degraded_create": false},
  "migration_policy": {"live": "require_verified"},
  "encryption": {"mode": "provider-managed", "key_ref": "external-secret-reference"}
}
```

Accepted fields are schema-versioned. Unsupported policies/field combinations fail closed, rather than being silently ignored. A nearline volume with `local_protection=mirror` requests two *local media legs* on the primary, while `remote_replicas=1` requests another copy on a different host: these are separate resource demands. Do not infer that every sample host has sufficient devices to meet this request.

Valid nearline `replication.mode` values are `async` (DRBD A), `semi-sync` (DRBD B), and `sync` (DRBD C), subject to advertised provider capabilities; `async` remains default. `replication.engine` selects a qualified backend, initially `drbd9`; the engine choice must never alter volume protection silently. See [ADR-0007](../docs/adr/0007-drbd9-nearline-replication-provider.md). Quorum witness nodes do not count toward `remote_replicas`.

Field spellings in this contract are canonical (`replication.engine`, `replication.mode`, `replication.remote_replicas`, `local_protection.mode`). Prose in the ADRs may abbreviate them (for example `replication_mode=async`); the abbreviated and canonical forms denote the same field and must not diverge.

The provider must validate security context, quota, supported block alignment, capacity headroom including local thin-pool metadata and mirror legs, failure-domain constraints, encryption capability and current pool ownership. It returns requested vs effective policy, not merely requested policy.

## 2. InspectVolume / ListVolumes

Expose:
```text
volume_id, backend_class, project_id, generation, state,
provisioned_bytes, allocated_bytes, effective_protection,
failure_domain, health, attachment_ids, attachments[], current_writer,
backend_health, evidence_status
```

`ListVolumes` is `GET /v2/volumes` with optional query parameters:

```text
project_id=<project>          existing project filter
limit=<1..=1000>              optional page size
cursor=<opaque next_cursor>   continuation token from the previous page
```

Ordering is stable: `volume_id` ascending. The response keeps `volumes` and
adds `next_cursor` (present only while more entries follow; opaque, never
parsed by clients). A request **without** `limit` preserves the legacy
unlimited listing and carries no cursor, so an existing client never
silently loses volumes; new clients should send `limit` to obtain a bounded
response. A `cursor` without `limit` is `INVALID_REQUEST` (a continuation
token only resumes a bounded page). `limit` outside `1..=1000` and an
invalid cursor are `INVALID_REQUEST`. The cursor is a resume point, not a snapshot: entries
created or deleted between pages may affect later pages. The server-side
maximum is 1000; an operator may configure a smaller `max_list_limit`, never
a larger one.

Each entry in `attachments[]` is the full, hypervisor-neutral per-attachment
projection a consumer reconciles against:

```text
attachment_id, vm_id, host_id, access_mode, attachment_generation,
frontend, vmm_disk_id, state, state_revision, evidence, reporter,
recorded_at_ms
```

`attachment_generation` is the **detach/report-fencing token**. For a provider
that advertises `attachment_state_report`, it is **strictly increasing per
volume**: every new attachment instance records a generation greater than every
earlier instance on that volume, including after a detach and across provider
restarts, and it never restarts at `1` for a re-attach on a live volume (see
[attachment-state reporting](#attachment-state-reporting)). Providers without
the capability expose a provider-scoped token and consumers must treat it as
opaque. `state_revision` is the monotonic evidence revision and is the only
ordering input for `state` advancement; `recorded_at_ms` is a server-assigned
receipt timestamp and never an ordering authority. `evidence` and `reporter` describe the current state's
basis and are absent for a provider-recorded `prepared` attachment. The array
is additive: older payloads without it decode as empty, and consumers must
tolerate unknown additive fields. Its order is not significant: consumers
reconcile by `attachment_id`, never by position (first-party providers may
emit attachment-id order or a single element). `attachment_ids` and
`current_writer` remain for compatibility; `attachments[]` is authoritative
when present.

A backend may add typed details, but any unsupported status must report `unknown`, not false healthy. For `ceph-rbd`, reflect actual Ceph pool/OSD policy/health; for nearline, expose per-replica progress and data-loss risk; for native, show disk/pool/mirror health.

For nearline volumes under a writer-authority witness, the response carries an
`authority` section — the **writer-side observation**, never a fabricated one:

```text
authority: { epoch, lease_state, holder, lease_remaining_secs }
```

- `epoch` is the writer's recorded writer epoch (`0` = pre-authority, the
  pre-witness P3 behavior);
- `lease_state` is evaluated against the writer's local W5 deadline
  (`live` only while the deadline has not passed — an unreachable witness
  never turns a live local lease into a fabricated expiry, and a passed
  deadline never reports `live`);
- `lease_remaining_secs` is present only while live;
- volumes of other classes, and nearline volumes without witness
  management, omit the section entirely.

## 3. AttachVolume / DetachVolume / ReportAttachmentState

```text
Attach(volume_id, vm_id, host_id, attachment_id, expected_volume_generation,
       access_mode=single_writer, requested_frontend, vmm_disk_id, idempotency_key)
Detach(attachment_id, expected_attachment_generation, vm_stopped_or_io_drained_proof)
ReportAttachmentState(volume_id, attachment_id, expected_attachment_generation,
                      state=advertised|active, evidence=vmm_configured|guest_confirmed,
                      reporter?, detail?, operation_id)
  -> attachment_id, state, state_revision, evidence, reporter, recorded_at_ms,
     attachment_generation, applied
```

Conditions:
- exactly one active writable attachment by default across all classes;
- other readers only if backend and consumer have an explicit safe multi-reader contract;
- no guessed path or tenant-accessible backend credential;
- attach returns a **host-scoped, ephemeral backend handle**, not raw secrets;
- `vmm_disk_id`, when the consumer sets it, is the consumer's own VMM device
  identity for the frontend (e.g. Cloud Hypervisor's `--disk path=...,id=...`),
  recorded with the attachment as the durable mapping a grow's capacity
  notification addresses (section 4A); the provider never invents one — an
  attached volume without a recorded id cannot be notified, and the grow
  reports the recorded refusal, never a silent un-notified success;
- VMM attach evidence must distinguish `prepared`, `advertised` and `active`;
- detach cannot release authority until in-flight writes are drained and stale device handles rejected;
- stale generations return a typed conflict, not success;
- a crash must not fabricate a second attachment.

### Attachment-state reporting

A consumer — any VMM integration, not only Cloud Hypervisor (QEMU/KVM, Proxmox
VE and others share the same contract) — reports observed evidence through
`POST /v2/volumes/{volume_id}/attachment-state` with the `attachment_id` in the
JSON body (consistent with detach). The report is a **consumer attestation**:
volvisor records it verbatim and never promotes it to a provider-verified fact.

- Evidence rank is `prepared < advertised < active`. A report may only move
  **up**; an equal or lower report is a successful idempotent no-op
  (`applied=false`) carrying the current projection. There is no downgrade.
- `advertised` requires `vmm_configured` or `guest_confirmed`; `active`
  requires `guest_confirmed`. `prepared` is provider-recorded and is not a
  reportable target.
- `expected_attachment_generation` is a required instance precondition: a
  mismatch fails with `STALE_GENERATION`. A report **never advances**
  `attachment_generation` — that token remains the detach fence.
- `state_revision` starts at `1` for a freshly attached instance and is scoped
  to that instance; it advances by one only when a report applies. A no-op
  report leaves the projection (including `evidence`, `reporter` and
  `recorded_at_ms`) untouched. `recorded_at_ms` is server-assigned (Unix ms);
  ordering depends only on `state_revision`.
- An unknown or detached attachment is `NOT_FOUND` for a new `operation_id`.
  Replaying a recorded `operation_id` returns the recorded outcome verbatim.
- `detail` is optional, bounded to 1024 bytes, and is journal-only diagnostic
  context: it is not part of the current projection. `reporter` is an
  untrusted label, never authentication; when present it must be non-empty,
  is bounded to 1024 bytes, and must not contain control characters (a label
  cannot carry log injection).
- The report is journaled like every other mutation (same idempotency rules,
  section 7). Refusals split by where they occur: an **envelope** refusal
  (invalid api version, `prepared` target, unsupported evidence/state pairing,
  malformed or reserved `operation_id`, unknown fields, over-long `detail`,
  or an over-long, empty or control-bearing `reporter`) is detected before
  the journal write,
  so the `operation_id` remains reusable. A **stale-generation** or
  **not-found** refusal is detected by the provider after the journaled intent
  and is recorded as a terminal outcome; the `operation_id` is consumed and
  the recorded refusal replays verbatim, exactly like an apply.
- The phase is per-attachment-instance evidence, not liveness. Removing the
  frontend from the VMM must be followed by detach; it cannot un-report active.
- A report never fabricates a `prepared` transition and never downgrades. When
  a consumer needs to re-observe, it reads `attachments[]`; it does not replay
  an old report.
- Attachment ids must be unique for a volume's lifetime; consumers must mint a
  fresh id per attach. Independently, a provider that advertises
  `attachment_state_report` must fence delayed evidence with the strictly
  increasing per-volume `attachment_generation` described above, so a delayed
  report or detach from a retired instance can never be attributed to a new
  one: an old generation is `STALE_GENERATION`, and an id that is not the
  current instance is `NOT_FOUND`. A provider **MUST accept a re-attach of a
  previously consumed id as a new instance with a strictly greater
  generation** and **MUST NOT refuse the attach solely because the id was
  consumed**: `IDEMPOTENCY_CONFLICT` is reserved for a replayed id whose
  payload differs from the live instance (or another recorded conflict), not
  for an id whose previous instance was detached. Retaining a diagnostic
  tombstone is allowed, but it must not gate attach. An exact tombstone grows
  without bound in the volume's lifetime attach count, and a fixed-size
  probabilistic tombstone is unsound because its false positives refuse fresh
  ids (observed as `IDEMPOTENCY_CONFLICT` on a churned volume in issue #63).
- **Implementation subset (not a wider claim):** this revision is implemented
  by the `native-local` LVM provider and the conformance fake. `ceph-rbd` and
  `nearline-replicated` (DRBD) do not advertise `attachment_state_report` and
  answer the route's typed `NOT_FOUND`. The route and types are
  hypervisor-neutral; no consumer-specific field is defined here.

Ceph RBD exclusive-lock alone is not a substitute for O3K attachment authorization. For nearline, attachment admission must enforce authoritative writer epochs end-to-end.

## 4. Resize / Snapshot / Delete

Resize up/down and snapshot/clone are **capabilities**, not guaranteed features of the base create contract. Shrink is unsupported by default. Resize must preserve block alignment and reject when live frontend/backing changes are unsafe. Snapshot success must document crash-consistent vs application-consistent behavior and all attached-disk consistency groups.

Delete requires correct generation, fully detached/stopped writer, no dependent snapshots/clones, safe backend deletion proof and explicit data-erasure policy. Foreign/mismatched state is quarantined. A successful API response must correspond to persisted observed state, or unambiguously indicate accepted asynchronous work.

## 4A. GrowVolume and MoveVolumeBackingOnline

Online size growth and same-host live backing relocation are **separate APIs**, both different from cross-host VM migration. See [ADR-0006](../docs/adr/0006-online-resize-and-live-local-block-relocation.md).

```text
GrowVolume(volume_id, new_size_bytes, expected_generation, idempotency_key)
  -> backing_resized, guest_notification_status, effective_size_bytes

MoveVolumeBackingOnline(volume_id, target_pool_id, expected_generation,
                        operation_id, max_copy_bytes_per_sec)
  -> PREPARING | COPYING | MIRROR_READY | PIVOTED | COMPLETE |
     FAILED | IN_DOUBT
```

- Grow-only by default; backend may grow before the VMM/guest is notified. The provider must retry notification, not automatically shrink. Guest filesystem expansion is not implied. For an **attached** volume the notification is version-gated per [ADR-0006](../docs/adr/0006-online-resize-and-live-local-block-relocation.md): it is issued through the VMM's resize-disk API (REST over the per-VM API socket; the disk identity is the consumer-declared `vmm_disk_id` recorded at attach, section 3) only on a pinned, startup-verified version (upstream PR #7948 for externally grown host block devices); when the version is not proven, the **notification** is refused typed with the recorded reason — the grow itself succeeds and the response honestly reports `retry_required` — rather than growing silently un-notified. The `guest_notification_status` field records the outcome — `notified`, `retry_required` (pending or failed notification; the retry rule above) or `not_applicable` (no frontend).
- `MoveVolumeBackingOnline` does not change compute host or guest disk identity. Backends advertise `same_vg_extent_move` and `same_host_live_backing_move` separately, bound to actual LV layout/VMM/frontend/QSD qualification.
- **P6 implementation scope (ADR-0006 first slice):** only `same_vg_extent_move` is implemented and advertised; `same_host_live_backing_move` is advertised nowhere until the QSD mirror/pivot acceptance suite passes. Any move outside the advertised, qualified capability scope — cross-VG, cross-pool, cross-class, or a capability the backend does not qualify — is refused typed with `MOVE_UNSUPPORTED_SCOPE` (fail-closed, never silent, never a generic `FAILED`).
- Native LVM `pvmove` is restricted to supported physical extent migrations within the same VG, and should not be presented as arbitrary per-thin-LV cross-pool movement.
- General online same-host move via QEMU Storage Daemon vhost-user-blk is an **experimental** capability until mirror, pivot, reattach/restart, writer fencing, guest flush/FUA, idempotency and in-doubt recovery pass fault injection.
- Source deletion must only occur after the target pivot and persistent ownership reconciliation; an unknown result is not a safe reason to revert authority or delete either copy. Never report a generic `FAILED` after the pivot: the outcome is `IN_DOUBT` or rolls forward under reconciled authority, mirroring the cross-host handoff rules in section 5.
- **Implemented same-VG slice (P6-C) — the honest state subset.** The LVM `same_vg_extent_move` implementation answers `PREPARING | COPYING | COMPLETE | IN_DOUBT` only: a same-VG `pvmove` mirror is kernel-internal, so `MIRROR_READY` and `PIVOTED` are not honestly observable through `lvs` and are never reported; admission and scope refusals are typed errors raised **before** any state exists; an unknown outcome mid-move is `IN_DOUBT`. A `FAILED` move record is written by exactly one authority: the retry reconcile resolving a parked `IN_DOUBT` record whose world verifiably shows the pre-move placement (the relocation did not happen — an out-of-band `pvmove --abort` that completed the rollback, or a move that never started; the source intact and serving). It never surfaces as a drive response: a fresh operation drives a new move under a new `operation_id`, superseding the record at its `PREPARING` journal write. The concrete route is `POST /v2/volumes/{volume_id}/move-backing` — privileged, journaled, idempotent by `operation_id` (byte-identical replays return the recorded response; a differing body under the same id is a typed conflict). The response carries `state`, `generation`, `source_pv`, `target_pv` and `detail`; one fenced generation bump lands with the verified `COMPLETE` record.
- `max_copy_bytes_per_sec`, when set, is a typed `UNSUPPORTED_CLASS_OR_POLICY` refusal naming the parameter — never silently ignored: no same-VG `pvmove` implementation in this version can honor a rate limit.
- **No consumer abort in this slice.** This section defines no abort route and the implementation offers none — the only in-band outcomes are the drive's supervised result (`COMPLETE`), an honest `COPYING` (the supervision window expired with the move progressing; a fresh `operation_id` re-attaches and the retry reconcile completes the record), or a park (`IN_DOUBT`). An operator acting out of band (`pvmove --abort`) while the daemon is down leaves the record parked `IN_DOUBT` with the source extents intact and serving; a fresh operation refuses typed naming the park until the retry reconcile resolves it: to `COMPLETE` when the relocation is provable, or to a `FAILED` record when the world verifiably shows the extents back on the source (the relocation did not happen), after which a fresh operation is admitted. The mixed-placement abort (segments on both PVs — LVM's non-atomic rollback) stays parked: it is genuinely unresolvable by observation alone, and its resolution is the operator's. Source extents are freed by the verified completion alone (LVM's own `pvmove` landing, confirmed by observation before the `COMPLETE` record is written), never by an abort path.

## 5. MigrateVolume / MigrateVM

```text
CheckVmStorageMobility(vm_id, source, target) -> eligible, reasons[], participants[]
PrepareNearlineHandoff(migration_id, volume_ids[], target, expected_generations)
BarrierAndTransfer(migration_id, vm_paused_and_io_drained_proof)
ObserveHandoff(migration_id) -> PREPARED | PRECOPY | QUIESCED | BARRIER_DURABLE |
                                 SOURCE_REVOKED | IN_DOUBT | DESTINATION_AUTHORIZED |
                                 VM_RESUMED | COMPLETE | ABORTED
```

The observation states are the canonical migration states of the nearline contract section 6. Authority transfer is deliberately observable as two steps (`SOURCE_REVOKED`, then `DESTINATION_AUTHORIZED`); an implementation must never collapse them into a single atomic status. `IN_DOUBT` is reachable once the cut is entered (the durable point of no return, at or after the source-side barrier), when a pre-cut rollback cannot complete safely (a failed barrier void — fail-closed, the source is never resumed), and through any unresolvable post-authorization stall before `COMPLETE` (e.g. a dead destination VMM; a resolvable stall is reported as the canonical state plus a stall detail); it must never be reported as a generic `ABORTED`.

These calls are conceptual, not a license to implement handoff as two independent `detach/attach` operations. The consumer and provider must agree on a durable cutover identity and a consistent **all-writable-volume** boundary. Successful handoff requires exact-source-epoch durable target data and enforced source write revocation before target admission.

Native-local attached storage returns `MIGRATION_UNSUPPORTED_LOCAL_STORAGE`. Ceph RBD migration requires VMM/frontend interoperability and safe writer/lock handoff, not nearline replica convergence. Ordinary PCI passthrough migration remains unsupported by default unless precise migratable-device capability is proven.

Never emit a generic `ABORTED` when authority has moved. State becomes `IN_DOUBT` or rolls forward under fenced recovery.

Concrete v2 routes (nearline class only; a daemon without the migration
surface serves typed errors on all of them):

```text
POST /v2/vms/{vm_id}/check-mobility           200 {eligible, reasons[], participants[]}
POST /v2/migrations                           201 {migration_id, state, participants[]}   (PrepareNearlineHandoff)
POST /v2/migrations/{migration_id}/transfer   202 {state}                                  (BarrierAndTransfer)
GET  /v2/migrations/{migration_id}            200 {state, state_history[], in_doubt_detail?, cut_duration_secs?} (ObserveHandoff)
POST /v2/migrations/{migration_id}/abort      200 {state}
```

- The mutating routes are privileged and journaled. A byte-identical
  replay of `POST /v2/migrations` returns the recorded response; a
  differing body under the same `migration_id` is a typed conflict —
  never a silent second migration.
- `transfer` returns **202 Accepted**: the cut is accepted for
  processing and driven asynchronously; the outcome is observed, not
  returned. Volvisor pauses and verifies the VM itself: the caller's
  `vm_paused_and_io_drained_proof` is **recorded as corroboration
  only** — an absent or false proof never substitutes for volvisor's
  own verified pause, and never changes the drive's behavior. A
  completed migration's observation carries the measured wall-clock
  cut duration (`cut_duration_secs`: the seconds between the cut
  write-ahead's first durable step and `COMPLETE`).
- **Downtime statement**: this is a stop-and-copy cutover, not a live
  migration. The guest is paused for the barrier, cut, restore and
  resume; the window between `BarrierAndTransfer` and `VM_RESUMED` is
  guest-visible downtime. Memory pre-copy and any dual-primary I/O
  path remain out of scope (nearline contract section 6).
- **Snapshot-dir sharing**: the deployment provisions a snapshot
  directory readable and writable by both hosts' volvisor daemons (the
  source writes the VM snapshot; the destination reads it). The
  destination verifies cross-host readability at `PREPARED` and
  refuses typed otherwise — an unreadable snapshot dir fails the
  migration before any cut, never mid-cut.
- **Destination socket ownership**: the destination daemon verifies
  its VMM socket empty for the migrated VM id at `PREPARED` (a
  squatted id is refused typed, before the source's cut). After a
  successful preparation that socket is owned by the migration:
  whatever VM appears on it before the restore is treated as the
  migration's own half-restore and destroyed first (the re-drive
  rule) — a VM id must not be reused on the destination while one of
  its migrations is in flight.
- `abort` is refused typed once the cut is entered (the write-ahead's
  durable point of no return); before that it rolls the preparation
  back in the fenced order the nearline contract prescribes.

## 6. Local vs remote protection capability

Every volume exposes two independent axes:

```text
local_protection: none | mirror | provider_specific
remote_protection: none | asynchronous_peer | synchronous_peer | ceph_policy
```

`native-local` can be locally mirrored but still unavailable after host failure. `nearline-replicated` can be unmirrored locally yet have a stale remote copy. `ceph-rbd` uses Ceph's placement/replica or EC policy, not a Volvisor local mirror unless separately approved. Protection choice and current effective health must never be collapsed to a single boolean.

`synchronous_peer` is reported only for an observed synchronous replica (DRBD Protocol C read back from the resource's own definition, the replica observed `UpToDate`, no resync in progress). It states that writes are acknowledged after both durable media completed — RPO=0 under the protocol's own conditions (both nodes up, connection healthy) — and is never a claim about simultaneous-failure loss (AGENTS rule 16). `asynchronous_peer` remains possible-RPO and must never be presented as zero-RPO.

These axes are reporting dimensions of `effective_protection` (section 2), not additional request fields; the request-side fields are `local_protection` and `replication` (section 1).

## 7. Admission, state and failures

Common states: `Requested`, `Provisioning`, `Ready`, `Degraded`, `Attaching`, `Attached`, `Detaching`, `Deleting`, `Failed`, `Quarantined`.

Common errors:
`UNSUPPORTED_CLASS_OR_POLICY`, `INSUFFICIENT_FAILURE_DOMAINS`,
`NO_SAFE_CAPACITY`, `THIN_METADATA_EXHAUSTED`, `FOREIGN_DEVICE_STATE`,
`STALE_GENERATION`, `WRITER_ALREADY_ACTIVE`, `UNKNOWN_FENCING_AUTHORITY`,
`REPLICA_NOT_DURABLE`, `MIGRATION_UNSUPPORTED_LOCAL_STORAGE`,
`MOVE_UNSUPPORTED_SCOPE`, `VMM_HANDOFF_UNSUPPORTED`, `OPERATION_IN_DOUBT`,
`OPERATION_EXPIRED`, `UNSAFE_DATA_LOSS`, `CEPH_CLUSTER_UNHEALTHY`,
`RATE_LIMITED`, `OVERLOADED`, `REQUEST_TIMEOUT`, `PAYLOAD_TOO_LARGE`.

Mutations are idempotent by `operation_id` + exact immutable request hash. Reuse with different payload is conflict. Must be replay-safe after journaled intent, operation and response persistence; unknown commit outcome is an observable retriable state, not implicit retry with a new ID. The journal is bounded: a retry whose operation was evicted by retention answers `OPERATION_EXPIRED` (HTTP 410) instead of re-executing a possibly-completed mutation; the client must confirm the prior effect with a read and submit a new `operation_id`. Retention evicts the oldest terminal operations beyond the configured window, so an operation evicted past the retained fingerprint ring is indistinguishable from a new one; operators size that window above the client retry horizon. Operation ids and attachment ids carrying the reserved `derived:` prefix are refused typed before any journal write: the prefix is the engine's derived-identity namespace, unreachable from the consumer surface. The refusal is enforced at the routes whose surfaces key on these ids (operation ids on every mutating route; attachment ids on the attach route, whose provider keys attach idempotency by attachment id); other acceptors of an attachment id (the detach route's lookup key) fail closed at the provider.

## 8. Backend extension and security

Versioned provider capabilities:
`create`, `attach`, `resize`, `snapshot`, `clone`, `local_mirror`,
`encryption`, `replicate`, `live_migrate`, `offline_copy`,
`same_vg_extent_move`, `same_host_live_backing_move`,
`rbd_cluster_adapter`, `managed_ceph_osd`, `pci_passthrough`,
`attachment_state_report`.
`attachment_state_report` gates section 3's consumer evidence reporting. It is
advertised by the providers that qualify it (the native-local LVM provider and
the conformance fake); a provider without it answers the route's typed
`NOT_FOUND` (the default trait behavior), never a silent accept.
See the [provider authoring guide](../docs/provider-authoring.md) for the shared
semantics core, capability rules, and the conformance/parity suites a provider
must pass.
`managed_ceph_osd` includes the separately gated, disabled-by-default
experimental `rook-cell-experimental` deployment mode ([ADR-0008](../docs/adr/0008-rook-only-hyperconverged-cells.md)); it is not a fourth volume class.

Capabilities must be tied to implementation/VMM version and **evidence**, not inferred from a backend product name. No class is production-supported before its exact-SHA conformance and real-host failure evidence gates.

Physical device claim and Ceph OSD lifecycle use privileged admin authorization distinct from tenant volume operations. No tenant-supplied block path, OSD ID or backend name can bypass ownership checks.

## 9. Consumer invariants

1. A logical volume exists independently of any particular VM attachment.
2. A pool's physical disk identity is not a volume identity.
3. Only one active writer may exist without an explicit, separately proven multi-writer contract.
4. A local mirror is not a remote replica; an OSD is not an RBD image.
5. Migration eligibility is the intersection of all attached storage, VM devices, and target capabilities.
6. There is no invisible downgrade of protection, no implicit destructive adoption, and no success report with unresolved authority.

## 10. Nearline writer authority and adopt-and-promote

Nearline volumes may be managed by a **writer-authority witness** (see the
[nearline replication contract](nearline-replication-v2.md)): a third-party,
journal-backed epoch/lease registry. Witness management changes attachment
admission — every promotion acquires a lease **before** the device becomes
Primary, a refused or unreachable witness refuses the attach typed — and adds
one privileged operation:

```text
POST /v2/admin/nearline/{volume_id}/adopt
Authorization: Bearer <admin token>
{ "api_version": "volvisor.volume.v2", "operation_id": <id>,
  "allow_loss": <bool> }
```

The adopt-and-promote flow (unplanned failover to the surviving host):

1. **Adoption verification** — the derived resource name, a Secondary role, a
   definition naming the surviving host, the live DRBD data-generation
   lineage matching the witness registration, the registered endpoint
   backing identity, and — for volvisor-created backing — the ownership tag.
   Foreign state is never adopted (typed `FOREIGN_DEVICE_STATE` /
   `INVALID_STATE` refusals).
2. **Authority check** — the witness must show no live lease for the volume;
   a proof the witness supplies, never one the caller brings.
3. **Classification** from observed facts only:
   - `safe_current` — protocol C, local disk `UpToDate`, no live lease **and**
     a recorded operator barrier (the only evidence that closes the
     acknowledged-tail question; protocol alone never does);
   - `possible_loss` — the acknowledged tail is not provably present
     (`boundary` is always `"unknown"` in this stage: a recorded barrier
     gates `safe_current`, it never names a provable loss boundary for a
     volume that kept serving past it);
   - `unsafe` — integrity unprovable or a live lease still held.
4. **Promotion** — only `safe_current`, or a `possible_loss` with the
   request's explicit `allow_loss`, promotes; under a fresh witness epoch
   (the durable fencing proof) with `drbdadm primary --force` — the one
   justified `--force` path for exactly this case, gated on the authority
   check above, never relied upon as the fence itself.

The response is the classification and, **only on promotion**, the resulting
volume state:

```json
{ "classification": <safe_current | possible_loss{boundary, authorized} |
                     unsafe{reasons}>,
  "volume": <InspectVolume response | null> }
```

A classification-based refusal (`unsafe`, or unauthorized `possible_loss`)
is a **successful** response with `"volume": null`: the classification is
the result, and the surviving host's state is untouched. Nothing is claimed
beyond this surface: registration of existing volumes into the witness is an
explicit, separately authorized operation outside this API's tenant surface,
and the witness daemon, its deployment as a third failure domain, and the
lease/fence timing arguments are specified by the nearline replication
contract.

Operation semantics: admin-token required; journaled like every privileged
mutation (`operation_id` idempotency, `allow_loss` and the volume id folded
into the request hash — a changed authorization under the same operation id
is a typed `IDEMPOTENCY_CONFLICT`); `FENCE_PENDING` while the witness is
still inside its fence-wait window; `UNKNOWN_FENCING_AUTHORITY` when the
witness cannot be reached. Replays of a recorded outcome — refusal or
promotion — are byte-identical.

An adoption whose promotion *fails after the grant* (a refused or failed
`primary --force`, a failed verification) is a typed `INTERNAL` error that
leaves a **tracked `failed` volume** on the surviving host — never an
untracked Primary holding a live lease. The residue is fenced (suspended,
demoted when possible, the lease released only after a proven demotion — an
incomplete fence leaves the lease to expire at the witness under its fence
window), and the tracked entry is resolved through volume management (a
retry adopt is the typed `INVALID_STATE` "already exists" refusal until the
failed entry is deleted); there is no invisible re-adoption.

## 11. Durable state and upgrades

The provider-internal state behind this API (intent journal, LVM/Ceph/DRBD
state, migration records and peer preparations) is versioned and
fail-closed. A file declaring a newer schema than the running build is
refused with a typed `INTERNAL` error and is never partially decoded,
truncated or rewritten; an unknown field at a supported version is likewise
a refusal, never silent field loss. Older supported files are read in place
and upgraded only by their first successful write. There is no in-place
downgrade: rollback requires restoring a pre-upgrade state backup and
matching binary.

The exact stores, version carriers, N-1/N-2 fixture matrix and rollback
procedure are specified by
[schema compatibility](../docs/schema-compatibility.md) and enforced by the
workspace test job. This policy is implementation-facing; it does not add a
tenant API error code.

## 12. Admission control, timeouts, body limits and pagination

Admission refusals use the common error shape and are **not journaled**:
they occur before the handler reaches the journal, so they are safe to
retry.

The global concurrency cap, the read-only timeouts (a fallback plus
optional per-route overrides) and the body limit are enabled by default.
Per-principal rate limiting is **opt-in and disabled by
default**, so an unmodified deployment's success path is unchanged; an
operator enables it explicitly with a non-zero
`admission.rate_limit_per_second` and burst.

- `429 RATE_LIMITED` — the caller's per-principal rate limit was exceeded.
  Carries `Retry-After` (seconds). The key space is the configured principal
  set (never bearer tokens), bounded by configuration and a defensive
  ceiling. This refusal only occurs when rate limiting is enabled (non-zero
  configured rate).
- `503 OVERLOADED` — the global in-flight concurrency cap stayed exhausted
  for the admission wait window. Carries `Retry-After` (seconds).
- `504 REQUEST_TIMEOUT` — a **read-only** route exceeded its bounded
  timeout. `admission.read_timeout_secs` is the fallback for every read-only
  route; optional `[admission.route_timeouts]` entries override it per
  matched path (unknown or out-of-range keys are refused at startup). A
  read-only route without an override keeps the fallback, so the default
  behavior is uniform. Journaled mutations are deliberately not wrapped in
  this timeout, even when a mutating method shares a matched path with an
  overridden read route.
- `413 PAYLOAD_TOO_LARGE` — the request body exceeded `max_body_bytes`.
  Enforced while reading, so a streamed/chunked body without
  `Content-Length` is bounded too; rejected before parsing and before any
  journal write.

`/healthz`, `/v2/healthz`, `/readyz`, `/v2/readyz` and `/metrics` bypass the
concurrency cap and the rate limiter so probes and monitoring remain
available while load is shed. The four probe endpoints are public; `/metrics`
requires read access when scoped credentials are configured (and stays open
in the legacy/tokenless posture, like every read).

### Readiness and metric exposition

`/healthz` and `/v2/healthz` are **liveness only**: a `200` means the process
is serving, not that its durable dependencies are usable. `/readyz` and
`/v2/readyz` are readiness: they answer `200` only when the journal is open
and startup has completed (journal replay, provider construction/reconcile
and listener bind), and `503` otherwise. The JSON body names each check:

```json
{"status":"ready","startup_complete":true,
 "checks":{"journal":"open","provider":"reconciled","witness":"reachable"}}
```

`checks.witness` is `reachable`, `unreachable`, or `not_probed`. With no
witness URL configured the daemon reports `not_probed` and does **not** fail
readiness on that unknown; a configured witness whose bounded transport probe
fails makes readiness `503` while liveness stays `200`. This probe is
transport reachability of the witness endpoint, not a claim about authority,
quorum or data safety.

`/metrics` is Prometheus text exposition. Metric names, fixed label sets,
cardinality bounds, refreshes and omitted measurements are documented in
[observability](../docs/operations/observability.md); labels never carry
volume/attachment/VM ids, principals, tokens, URLs or error strings.

### Long-operation model

This revision has **no asynchronous operation handle and no long-poll
surface**. A journaled mutation is synchronous: validate, journal intent,
run the provider act, journal and return the outcome. The daemon applies a
wall-clock timeout only to read-only routes; it never drops a journaled
mutation's future after the intent, because that would abandon an act whose
outcome is not recorded. A client retry uses the **same `operation_id`** and
byte-identical body: the journal replays a recorded outcome or answers
`OPERATION_IN_DOUBT`; a retry whose operation fell outside the bounded
retention window answers `OPERATION_EXPIRED` and is never re-executed. A
client-side timeout or disconnect does not create
server-side resumability. A resumable operation handle remains explicitly
unimplemented; this section records the synchronous model instead of
pretending otherwise.
