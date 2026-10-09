# SPEC-0002 — Volvisor three-mode volume virtualization

Status: Proposed / NOT production supported
Date: 2026-10-09
Supersedes: SPEC-0001 for proposed v2 volume model
Normative: [ADR-0003](../adr/0003-tiered-volume-virtualization.md), [ADR-0004](../adr/0004-nearline-replication-and-mobility.md), [Volume API v2](../../contracts/volume-api-v2.md), [Nearline contract v2](../../contracts/nearline-replication-v2.md)
Historical references: SPEC-0001 and v1 contracts (unimplemented draft).

## 1. Scope

Expose a common volume model with **three concrete backend modes**:
`native-local`, `nearline-replicated`, and `ceph-rbd`. Separate physical device enrollment, pool creation, volume provisioning, replica management, and hypervisor attachment. Provider software and consumer O3K/CellHV control plane are separately versioned.

A volume is not a PCI function, a raw NVMe disk, a Ceph OSD or a guest device name. Multiple logical volumes may share one eligible local capacity pool subject to quotas and isolation. An optional exclusive `pci-passthrough` attachment profile is **not a fourth logical-volume storage class**.

## 2. Data model

```text
StorageDomain { id, provider_kind, membership_generation }
PhysicalDevice { id, host_id, namespace_ids, capacity, health, owner_role, owner_generation }
Pool { id, backend_class, device_ids[], host_or_ceph_cluster, capacity, allocatable, protection, health }
Volume { id, project_id, class, pool_ref, size_bytes, provisioning, block_size,
         generation, lifecycle, effective_protection, failure_domain, health,
         data_epoch, snapshots[], backend_private_ref }
Replica { id, volume_id, host_id, disk_or_pool_ref, generation, role, durable_progress, health }
Attachment { id, volume_id, vm_id, host_id, generation, frontend, access_mode, state }
Migration { id, vm_id, source_host, target_host, participating_volume_ids[],
            vm_generation, epoch, state, cutover_boundary, last_error }
```

Stable identities are opaque, immutable and never based solely on Linux device names, PCI BDF, or Ceph friendly image names. The provider maintains authoritative cross-references and reconciles observed state. A backend private reference is not an invitation to direct tenant access.

## 3. Pool and disk ownership

Enrollment starts **read-only**. A disk is explicitly claimed for exactly one physical role: `native_pool`, `nearline_pool`, or `ceph_osd`. The role is distinct from the logical volume's class. A pool may combine devices only under explicit validated topology.

Before provisioning:
- verify stable identity (serial plus NVMe NGUID/EUI and controller/namespace properties as appropriate);
- verify current ownership generation, block geometry, signatures and absence of foreign claims;
- journal intent durably before every destructive transition;
- forbid unintended wipe/repartition/sanitize on initial discovery;
- use scoped destructive-authorization token for pool/OSD initialization;
- reconcile after power loss; never infer ownership from generated names.

`pci-passthrough` requires full compatible PCI function/IOMMU isolation and exclusivity, and cannot overlap a native pool/Storage Cell. Do not attempt generic namespace VFIO.

## 4. Native-local provider

Reference prototype: Linux LVM thick or dm-thin LV(s) on claimed disks, exported as stable host block paths to Cloud Hypervisor virtio-blk. A Storage Cell is **not permitted** on its foreground I/O path in v2; an exception requires a separately accepted design (see ADR-0003 section 1).

Provider shall implement:
- volume create, inspect, attach, detach, resize when backend-safe, and delete;
- exclusive single-writer attachment and robust stale-attachment rejection;
- pool free-space, thin-pool data **and metadata** utilization; explicit overcommit limits, ENOSPC behavior and read-only/fail-safe options;
- clear policy and evidence for TRIM/discard, zeroing/sanitization before cross-tenant reuse and end-to-end flush/FUA;
- optional local mirror (e.g. qualified md/dm implementation) with per-leg health and repair **not assumed** by basic native-local;
- crash-safe metadata recovery, volume/pool identity preservation and repair procedure.

A local volume cannot be live-migrated with only memory pre-copy. Requests for a VM with a native-local writable disk return `MIGRATION_UNSUPPORTED_LOCAL_STORAGE` unless it is detached or a separately specified offline storage-copy workflow is used.

## 4A. Online resize and same-host local storage relocation

[ADR-0006](../adr/0006-online-resize-and-live-local-block-relocation.md) distinguishes three different operations:

- **Online native LV growth:** grow underlying LVM/dm capacity, verify actual size, notify the running Cloud Hypervisor disk via the qualified `/vm.resize-disk` API, and let the guest resize partitions/filesystems. No disk-content copy or VM migration is needed. A failed guest notification after successful LV growth is a retryable partial completion, never an instruction to shrink backing data.
- **Same-VG physical-extent evacuation:** LVM `pvmove` may relocate supported LV extents online while the guest-facing dm mapping remains stable. It cannot be assumed to move an individual thin LV to a different thin pool; validate supported layout and scope.
- **Online whole-volume backing migration across pools/VGs on the same host:** optional QEMU Storage Daemon + Cloud Hypervisor vhost-user-blk mirror/pivot experiment. This introduces an external I/O engine and requires exclusive writer ownership, mirror/pivot failure semantics, flush/FUA correctness, guest identity stability, backend restart recovery and exact-VMM-version qualification before exposing a supported capability.

**None of these operations makes a native-local VM live-migratable to a different compute host.** Online backing relocation and cross-host nearline handoff have separate capability flags, failure modes and evidence requirements.

## 5. Nearline-replicated provider

A local data replica is served through a private host-local block endpoint under a Storage Cell. The frontend and engine are independent (e.g. kernel block / NVMe-oF/TCP via private link; later vhost-user/SPDK as validated). Replica peers occupy separate host failure domains. Optional local mirror is independently configured. See ADR-0004 and nearline contract for exact safety.

Required operations: create primary and replicas, seed, attach, disallow simultaneous writers, report durable lag, incremental resync, prepare migration, barrier, fenced handoff, reconcile in-doubt, promote with explicit classification, detach, and delete.

No claim of RPO=0 during asynchronous steady state. In particular, an extra local mirror can protect an SSD failure but cannot prevent loss of the unreplicated tail after complete host loss.

## 6. Ceph-RBD provider

Tenant block volumes are **RBD images**. OSDs are infrastructure daemons on physical devices. Mapping:
`VolumeId -> {cluster_fsid, pool_id/name, optional namespace, image_id, image_generation}` with ownership proof. Do not treat one OSD as one tenant volume.

Phase A: existing external Ceph cluster adapter. Validate cluster FSID, auth/key storage, pool policies, RBD image lifecycle, exclusive-lock/attachment discipline, RBD client mapping to the host frontend and monitoring. Respect backend min_size/replica or EC parameters and Ceph health.

Phase B (separate acceptance): Volvisor-managed OSD placement in Storage Cells, including MON/MGR quorum location, CRUSH failure domains, upgrades, device state, bootstrap/recovery and independent management of cluster shared infrastructure. Managed Ceph cannot be claimed production-ready on two OSD disks or one host.

Never double-replicate under an OSD by default. Existing Ceph clusters do not require OSDs to move into Storage Cell VMs. RBD snapshots, clones and resize are conditional on safe backend-validated workflows, not automatically shared API guarantees.

## 7. Consumer API and operation states

Operations are specified in [Volume API v2](../../contracts/volume-api-v2.md). Every mutation must carry idempotency key, expected generation and authorization scope. A provider must not return `Ready` while an attachment is merely declared.

Canonical operation phases:
- `create`: `Requested -> Provisioning -> Ready` or `Failed/Quarantined`
- `attach`: `Detached -> Preparing -> Attached`, with one active writer
- `detach`: drain VMM I/O, revoke attachment generation, verify endpoint teardown
- `delete`: reject non-detached or snapshot-dependent volumes; quarantine ambiguous leftover data
- `migrate`: `PREPARED -> PRECOPY -> QUIESCED -> BARRIER_DURABLE -> SOURCE_REVOKED -> DESTINATION_AUTHORIZED -> VM_RESUMED -> COMPLETE` (canonical states, nearline contract section 6); an interruption between `SOURCE_REVOKED` and `DESTINATION_AUTHORIZED` is `IN_DOUBT`, never a generic `ABORTED`
- `failover`: `Assessing -> Fencing -> AuthorizingLossIfAny -> Promoting -> Attached` (internal substates; externally published volume states remain the common states of Volume API v2 section 7)

Controller retries must never create duplicate volumes, leak physical device ownership, activate a stale endpoint, or change user-data ownership.

## 8. VM migration eligibility

Eligibility is computed over **all attached block volumes**, VMM devices and source/target compatibility:

- native-local writable or ordinary PCI passthrough: `unsupported` for v0 live migration.
- nearline: needs target local replica, catch-up capacity, final durable barrier and irrevocable exclusive writer handoff coordinated with Cloud Hypervisor.
- ceph-rbd: needs same accessible Ceph backing image, exclusive write authority, compatible mapped frontend and proven VMM migration contract.

If any required component lacks verified support, reject the VM migration before memory pre-copy. A successful VMM memory transfer alone is not a storage cutover proof. A group of nearline volumes must use a consistency-cut protocol across all disks; do not promote disk A independently while disk B remains source-writable.

Rollback semantics distinguish **before** and **after** authority commit. Never promise the source VM can resume after ownership has already moved.

## 9. Security

- mTLS/equivalent for peer control, authenticated per-volume access to block endpoints;
- storage traffic isolation from tenant networks;
- consistent tenant quotas, RBAC on volume and destructive device actions;
- no secret/key material in plan, journal, endpoint path, or normal logs;
- VolumeId and Ceph/RBD image ID are not authorization tokens;
- cryptographic erasure/key rotation requirements are a separate provider capability;
- `encryption` is a separately versioned capability, not a base-contract guarantee. `provider-managed` keys live in provider key management behind a `key_ref`, never in plans, journals, endpoint paths or logs. Key scope (per-volume/per-replica), rotation, and treating key destruction as sanitization each require their own evidence gate; until such a gate passes, cross-tenant reuse follows the explicit zeroing/discard policy in section 4, not cryptographic erasure;
- legacy backend disk signatures cannot be implicitly adopted.

## 10. Capacity, fault isolation, observability

Expose provisioned/allocated/physically-used bytes separately, with thin pool metadata exhaustion; local mirror healthy-leg counts, repair rates; peer replication durable progress, seconds/bytes lag and last caught-up boundary; Ceph health/pool policy and effective remaining usable capacity; frontend I/O errors, queue depth, p50/p99/p999 and achieved local/remote throughput.

Scheduler must account for compute+storage colocation, separate host/rack failure domains, surviving available physical capacity, storage/network bandwidth, reservation for initial seeding and rebuild, and mobility constraints. Do not hardcode eight drives or a 4/2/2 ratio.

## 11. Real-host acceptance matrix

| Class | Minimum gate |
|---|---|
| native-local | multi-volume pool isolation, crash replay at every mutation, stale attach and sanitization rejection, full thin pool, SSD loss with and without local mirror, reboot identity |
| nearline | all native checks relevant to owned local replicas, split-brain partitions, local mirror leg loss, peer loss/resync, unknown ACK tail, VMM handoff race, in-doubt recovery, confirmed no dual writer |
| ceph-rbd | RBD create/map/unmap, auth loss, OSD loss, host loss, Ceph degraded/full state, exclusive attach, migration, upgrade, control-plane outage, existing cluster interoperability |

Every test records precise product commit, VMM and kernel versions, Ceph/DRBD/tool versions, topology, write trace, failure injection and resulting state. Performance baselines against directly attached NVMe, DRBD 9 and Ceph RBD use matched hardware and workloads; never imply apples-to-apples results from different durability contracts.

## 12. Implementation sequence

P0: stable IDs, device claiming, single-volume API/attachments and provider conformance.
P1: native-local LVM prototype with thin-space safeguards and direct VFIO as distinct special profile.
P2: existing-Ceph RBD adapter with shared-backend attach/migration validation.
P3: nearline DRBD baseline and isolated frontends + local mirror experiment.
P4: witness/lease design, barrier state-machine, VM/storage cutover, crash/failure campaign.
P5: compare Mayastor/io_uring/SPDK alternatives; only build a new engine when justified.
P6: separate managed-Ceph OSD placement ADR/implementation and production gate.

The v2 documents fix semantics, **not implementation evidence**. Nothing here is a production support statement.

## 13. Inherited v1 requirements

Supersession is scoped: v2 replaces v1 class semantics, storage-class names, provider/replication contracts and migration authority. The following v1 operational requirements **remain binding** where this SPEC and the v2 contracts do not explicitly replace them:

- **Storage backplane and transport** (SPEC-0001 section 13): TCP is the minimum portable transport; RDMA/RoCE and dedicated trusted underlays are optional optimizations; control and data traffic remain distinguishable; underlay encryption policy is explicit and never silently disabled.
- **Control-plane outage behavior** (volvisor-provider-v1 section 12, ADR-0001 section 5): loss of O3K/CellHV connectivity alone must not detach healthy established volumes, revoke a valid writer, stop a healthy Storage Cell or reinitialize devices; new allocations and authority-changing operations may fail closed until authority returns.
- **Peer authentication and secret handling** (volvisor-provider-v1 section 13), **endpoint isolation** (section 15) and **observability truthfulness** (section 17).

If a v2 document needs to change one of these, it must say so explicitly rather than rely on silence.
