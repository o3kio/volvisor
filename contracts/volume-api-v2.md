# Volvisor volume API contract v2

Status: Proposed — normative for SPEC-0002; not implemented
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
failure_domain, health, attachment_ids, current_writer,
backend_health, evidence_status
```

A backend may add typed details, but any unsupported status must report `unknown`, not false healthy. For `ceph-rbd`, reflect actual Ceph pool/OSD policy/health; for nearline, expose per-replica progress and data-loss risk; for native, show disk/pool/mirror health.

## 3. AttachVolume / DetachVolume

```text
Attach(volume_id, vm_id, host_id, attachment_id, expected_volume_generation,
       access_mode=single_writer, requested_frontend, idempotency_key)
Detach(attachment_id, expected_attachment_generation, vm_stopped_or_io_drained_proof)
```

Conditions:
- exactly one active writable attachment by default across all classes;
- other readers only if backend and consumer have an explicit safe multi-reader contract;
- no guessed path or tenant-accessible backend credential;
- attach returns a **host-scoped, ephemeral backend handle**, not raw secrets;
- VMM attach evidence must distinguish `prepared`, `advertised` and `active`;
- detach cannot release authority until in-flight writes are drained and stale device handles rejected;
- stale generations return a typed conflict, not success;
- a crash must not fabricate a second attachment.

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

- Grow-only by default; backend may grow before the VMM/guest is notified. The provider must retry notification, not automatically shrink. Guest filesystem expansion is not implied.
- `MoveVolumeBackingOnline` does not change compute host or guest disk identity. Backends advertise `same_vg_extent_move` and `same_host_live_backing_move` separately, bound to actual LV layout/VMM/frontend/QSD qualification.
- Native LVM `pvmove` is restricted to supported physical extent migrations within the same VG, and should not be presented as arbitrary per-thin-LV cross-pool movement.
- General online same-host move via QEMU Storage Daemon vhost-user-blk is an **experimental** capability until mirror, pivot, reattach/restart, writer fencing, guest flush/FUA, idempotency and in-doubt recovery pass fault injection.
- Source deletion must only occur after the target pivot and persistent ownership reconciliation; an unknown result is not a safe reason to revert authority or delete either copy. Never report a generic `FAILED` after the pivot: the outcome is `IN_DOUBT` or rolls forward under reconciled authority, mirroring the cross-host handoff rules in section 5.

## 5. MigrateVolume / MigrateVM

```text
CheckVmStorageMobility(vm_id, source, target) -> eligible, reasons[], participants[]
PrepareNearlineHandoff(migration_id, volume_ids[], target, expected_generations)
BarrierAndTransfer(migration_id, vm_paused_and_io_drained_proof)
ObserveHandoff(migration_id) -> PREPARED | PRECOPY | QUIESCED | BARRIER_DURABLE |
                                 SOURCE_REVOKED | IN_DOUBT | DESTINATION_AUTHORIZED |
                                 VM_RESUMED | COMPLETE | ABORTED
```

The observation states are the canonical migration states of the nearline contract section 6. Authority transfer is deliberately observable as two steps (`SOURCE_REVOKED`, then `DESTINATION_AUTHORIZED`) with `IN_DOUBT` between them; an implementation must never collapse them into a single atomic status.

These calls are conceptual, not a license to implement handoff as two independent `detach/attach` operations. The consumer and provider must agree on a durable cutover identity and a consistent **all-writable-volume** boundary. Successful handoff requires exact-source-epoch durable target data and enforced source write revocation before target admission.

Native-local attached storage returns `MIGRATION_UNSUPPORTED_LOCAL_STORAGE`. Ceph RBD migration requires VMM/frontend interoperability and safe writer/lock handoff, not nearline replica convergence. Ordinary PCI passthrough migration remains unsupported by default unless precise migratable-device capability is proven.

Never emit a generic `ABORTED` when authority has moved. State becomes `IN_DOUBT` or rolls forward under fenced recovery.

## 6. Local vs remote protection capability

Every volume exposes two independent axes:

```text
local_protection: none | mirror | provider_specific
remote_protection: none | asynchronous_peer | ceph_policy
```

`native-local` can be locally mirrored but still unavailable after host failure. `nearline-replicated` can be unmirrored locally yet have a stale remote copy. `ceph-rbd` uses Ceph's placement/replica or EC policy, not a Volvisor local mirror unless separately approved. Protection choice and current effective health must never be collapsed to a single boolean.

These axes are reporting dimensions of `effective_protection` (section 2), not additional request fields; the request-side fields are `local_protection` and `replication` (section 1).

## 7. Admission, state and failures

Common states: `Requested`, `Provisioning`, `Ready`, `Degraded`, `Attaching`, `Attached`, `Detaching`, `Deleting`, `Failed`, `Quarantined`.

Common errors:
`UNSUPPORTED_CLASS_OR_POLICY`, `INSUFFICIENT_FAILURE_DOMAINS`,
`NO_SAFE_CAPACITY`, `THIN_METADATA_EXHAUSTED`, `FOREIGN_DEVICE_STATE`,
`STALE_GENERATION`, `WRITER_ALREADY_ACTIVE`, `UNKNOWN_FENCING_AUTHORITY`,
`REPLICA_NOT_DURABLE`, `MIGRATION_UNSUPPORTED_LOCAL_STORAGE`,
`VMM_HANDOFF_UNSUPPORTED`, `OPERATION_IN_DOUBT`, `UNSAFE_DATA_LOSS`,
`CEPH_CLUSTER_UNHEALTHY`.

Mutations are idempotent by `operation_id` + exact immutable request hash. Reuse with different payload is conflict. Must be replay-safe after journaled intent, operation and response persistence; unknown commit outcome is an observable retriable state, not implicit retry with a new ID.

## 8. Backend extension and security

Versioned provider capabilities:
`create`, `attach`, `resize`, `snapshot`, `clone`, `local_mirror`,
`encryption`, `replicate`, `live_migrate`, `offline_copy`,
`same_vg_extent_move`, `same_host_live_backing_move`,
`rbd_cluster_adapter`, `managed_ceph_osd`, `pci_passthrough`.
`managed_ceph_osd` includes the separately gated, disabled-by-default
experimental `rook-cell` deployment mode ([ADR-0008](../docs/adr/0008-rook-only-hyperconverged-cells.md)); it is not a fourth volume class.

Capabilities must be tied to implementation/VMM version and **evidence**, not inferred from a backend product name. No class is production-supported before its exact-SHA conformance and real-host failure evidence gates.

Physical device claim and Ceph OSD lifecycle use privileged admin authorization distinct from tenant volume operations. No tenant-supplied block path, OSD ID or backend name can bypass ownership checks.

## 9. Consumer invariants

1. A logical volume exists independently of any particular VM attachment.
2. A pool's physical disk identity is not a volume identity.
3. Only one active writer may exist without an explicit, separately proven multi-writer contract.
4. A local mirror is not a remote replica; an OSD is not an RBD image.
5. Migration eligibility is the intersection of all attached storage, VM devices, and target capabilities.
6. There is no invisible downgrade of protection, no implicit destructive adoption, and no success report with unresolved authority.
