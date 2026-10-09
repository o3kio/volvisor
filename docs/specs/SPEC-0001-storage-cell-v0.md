> **SUPERSEDED v1 draft — historical reference (2026-10-09).** The current proposed architecture is [ADR-0003](../adr/0003-tiered-volume-virtualization.md), [ADR-0004](../adr/0004-nearline-replication-and-mobility.md), and [SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md). Do not interpret v1 storage-class or provider contracts as compatible aliases for v2.

# SPEC-0001 — Volvisor Storage Cell v0

Status: Proposed  
Date: 2026-10-05
Superseded-by: [SPEC-0002](SPEC-0002-volvisor-volume-virtualization.md) (v2 volume model); v1 operational invariants not restated there remain binding per SPEC-0002 section 13

Related:

- [ADR-0001 — Volvisor Storage Cell architecture](../adr/0001-volvisor-storage-cell-architecture.md)
- [ADR-0002 — Replicated-async local endpoint and migration barrier](../adr/0002-replicated-async-local-endpoint-and-migration.md)
- [Volvisor provider contract v1](../../contracts/volvisor-provider-v1.md)
- [Storage-class semantics v1](../../contracts/storage-class-semantics-v1.md)
- [Replicated-async contract v1](../../contracts/replicated-async-v1.md)
- [Replicated-async R&D](../research/replicated-async-rnd.md)

## 1. Purpose

Volvisor is the O3K/CellHV storage virtualization provider.

It discovers storage-capable hosts, classifies physical storage devices,
realizes one Storage Cell VM per enrolled host, exposes three explicit storage
classes, and provides the ownership/fencing/lifecycle contract shared by O3K
and CellHV.

The v0 storage classes are:

- `local-direct`
- `replicated-async`
- `cluster-durable`

The specification does not create a production support claim. Each class has a
separate evidence gate.

## 2. Architecture

Per host:

```
                       workload VMs
                  /         |          \
                 /          |           \
        local-direct  replicated-async  cluster-durable
             |              |               |
          VFIO        host-local volume      |
             |          presentation          |
             |              |                |
       direct-pool      Storage Cell VM-------+
        NVMe(s)           |          |
                          |          |
                   async-pool     Ceph-pool
                    NVMe(s)        NVMe(s)
                          |          |
                    peer cells      Ceph cluster
```

Components:

1. **Consumer control plane** — O3K or CellHV. Owns tenant/project canonical
   resources, placement and activation policy.
2. **Volvisor host agent** — privileged host component. Owns hardware discovery,
   PCI/VFIO ownership transitions, Storage Cell lifecycle and host-local
   presentation.
3. **Storage Cell VM** — one Cloud Hypervisor VM on each enrolled host. Owns
   replicated-async and Ceph data-plane components assigned to that host.
4. **Volvisor runtime metadata service** — peer/lease/fencing metadata needed
   for established storage operation. It may be implemented inside the Storage
   Cells; no separate tenant-facing Volvisor control plane is required.
5. **Fabric/storage backplane** — authenticated peer connectivity and storage
   transport. High-throughput data may use a direct trusted underlay.

## 3. Canonical identifiers

Provider paths and Linux device names are never canonical identity.

Required stable identifiers:

```
StorageDomainId
HostId
StorageCellId
DeviceId
VolumeId
AttachmentId
ReplicaId
Generation
WriterGeneration
```

A `DeviceId` must be derived from stable hardware identity where available,
for example NVMe NGUID/EUI-64/serial plus validated controller identity. A PCI
BDF and a Linux `/dev/nvmeXnY` name are observations, not identities.

If the provider cannot distinguish two devices safely, it must quarantine them
instead of guessing.

## 4. Host inventory

The host agent publishes an inventory equivalent to:

```text
HostStorageInventory {
    storage_domain_id
    host_id
    host_generation
    iommu_enabled

    devices[] {
        device_id
        pci_bdf
        iommu_group
        vendor
        model
        serial
        firmware
        namespace_identity
        capacity_bytes
        logical_block_size
        physical_block_size
        numa_node
        health
        observed_owner
    }

    storage_transports[] {
        tcp
        rdma?
        roce?
    }
}
```

Inventory is descriptive. Discovery alone never claims, wipes, formats or
rebinds a device.

## 5. Device role plan

The consumer control plane compiles an accepted host plan equivalent to:

```text
StorageCellPlan {
    storage_domain_id
    host_id
    plan_generation
    cell_image_digest

    devices[] {
        device_id
        role: local-direct | replicated-async | cluster-durable
        claim_generation
    }

    peer_set
    storage_network
    provider_configuration
}
```

The plan must name devices by stable `DeviceId`. The provider must
re-validate the current observed device before mutation.

A device role is exclusive. A device cannot simultaneously belong to two
storage classes.

## 6. Device lifecycle

Normative state model:

```
Discovered
    |
    v
Unclaimed
    |
    | explicit accepted claim
    v
Claimed
    |
    +--> DirectPool
    |
    +--> AsyncPool
    |
    +--> DurablePool
    |
    v
Quarantined
```

A role change that could destroy existing data requires explicit destructive
authorization and current ownership proof. Merely changing configuration is
not proof that old data is disposable.

Foreign or ambiguous state enters `Quarantined` and fails closed.

## 7. Storage Cell lifecycle

A Storage Cell is a Cloud Hypervisor VM with a pinned image digest.

Minimum requirements:

- deterministic cell identity bound to host identity;
- reserved CPU and memory;
- watchdog/restart policy;
- NUMA affinity to cell-owned NVMe and storage NICs where practical;
- no tenant login path;
- immutable/base image semantics preferred;
- state disks/metadata clearly separated from tenant data devices;
- health and version reported to the consumer control plane;
- controlled upgrade with compatibility checks.

A Storage Cell crash must not cause the host agent to reinitialize data
devices. Recovery first re-establishes ownership proof and then reattaches the
same claimed devices.

## 8. Storage class: local-direct

### 8.1 Allocation

A free DirectPool device is exclusively leased to one workload attachment.

Required checks before attachment:

- device is currently in DirectPool;
- no Storage Cell owns the PCI function;
- IOMMU group is safe for assignment;
- no conflicting sibling function would escape the intended isolation;
- claim generation matches;
- previous tenant data has been sanitized according to policy before reuse.

### 8.2 Data path

Reference path:

```
workload VM -> VFIO -> physical NVMe
```

The Storage Cell must not proxy foreground I/O for this class.

### 8.3 Semantics

- durability: host/device-local only;
- replication: none;
- RPO on device/host loss: entire local volume may be lost;
- live migration while attached: unsupported in v0;
- scheduling: workload is pinned to a host that satisfies the direct device
  allocation;
- intended uses: KV cache, scratch, ephemeral dataset/model cache, temporary
  training state and other rebuildable data.

## 9. Storage class: replicated-async

### 9.1 Allocation

Capacity comes from AsyncPool devices attached to Storage Cells.

A volume has:

- one current writer/primary placement;
- zero or more non-authoritative replicas;
- one stable `VolumeId`;
- ordered replication progress per replica.

At least one remote replica is required before the volume may advertise
`replicated-async` readiness, unless an explicit degraded policy says
otherwise.

### 9.2 Local presentation

The workload host consumes its serving replica through a host-local endpoint.

The endpoint mechanism is not fixed in v0, but it must satisfy:

- stable attachment identity derived from `VolumeId`;
- no tenant-reachable wildcard listener;
- deterministic recreation after host/Storage Cell restart;
- support for flush/FUA semantics required by the guest block device;
- a migration handoff can prepare an equivalent endpoint on the destination.

Candidate first implementations are NVMe-oF/TCP over an isolated host-local
link and vhost-user-blk/SPDK. Selection is R&D-gated.

### 9.3 Foreground write semantics

Steady state:

```
write
 -> validate writer generation
 -> local durable commit
 -> advance local committed sequence
 -> queue replication
 -> ACK guest
 -> replicate to peers
```

Remote durable acknowledgement is not required before the guest ACK.

The implementation must expose replication lag in bytes and/or sequence
distance and time.

### 9.4 Replica safety

Each replica records a durable progress boundary. A replica may only be
advertised as current through sequence N when every write through N is known
durable there.

A stale replica remains stale until resynchronization proves convergence.

### 9.5 Planned migration

Migration follows ADR-0002 and the replicated-async contract.

Mandatory sequence:

1. select destination host with compatible capacity;
2. ensure destination replica exists;
3. while VM memory pre-copy proceeds, continuously catch the replica up;
4. enter final storage barrier;
5. quiesce/pause source writes;
6. flush source and establish barrier sequence `B`;
7. prove destination `durable_seq >= B`;
8. fence/revoke source writer generation;
9. activate a strictly newer writer generation on destination;
10. activate destination local presentation;
11. resume/complete VM migration;
12. demote/resynchronize old source placement.

Failure to prove any fencing or barrier step aborts migration.

### 9.6 Unplanned failure

A peer replica can legitimately be behind the last source ACK.

Promotion therefore must distinguish:

- **safe** — target proven current to the authoritative committed boundary;
- **possible-loss** — latest acknowledged boundary cannot be proven present;
- **unsafe/ambiguous** — fencing or ordering is uncertain.

Unsafe/ambiguous promotion is forbidden.

Possible-loss promotion must require explicit policy/administrative
authorization and must publish the loss boundary when known.

## 10. Storage class: cluster-durable

The reference provider is Ceph.

Volvisor does not own the Ceph on-disk format or invent a new replicated
durability protocol for this class.

Initial expected shape:

- DurablePool devices are attached to the Storage Cell VM;
- Ceph OSDs use those devices;
- Ceph MON/MGR placement and lifecycle are managed by an explicit Volvisor
  Ceph-provider design;
- O3K/CellHV volumes consume Ceph-backed block storage through the selected
  provider integration.

The exact Ceph deployment model requires a follow-up ADR before implementation
claims production readiness.

The class must never silently degrade to replicated-async semantics.

## 11. Discovery and activation

Activation flow:

```
O3K/CellHV
  -> discover host capabilities
  -> receive HostStorageInventory
  -> operator/policy assigns device roles
  -> compile StorageCellPlan
  -> host agent validates plan
  -> journal ownership intent
  -> create/upgrade Storage Cell VM
  -> attach only cell-owned devices
  -> establish authenticated peer membership
  -> realize class providers
  -> publish readiness/evidence
```

There is no automatic destructive disk adoption.

## 12. Control-plane independence

After successful activation:

- existing I/O must continue through an O3K/CellHV control-plane outage when
  local runtime dependencies are healthy;
- existing replicated-async replication may continue between already enrolled
  peers;
- established Ceph I/O follows Ceph's own quorum behavior;
- local-direct remains a hardware attachment;
- new allocations, destructive role changes and ambiguous topology changes may
  fail closed until control-plane authority returns.

"Control-plane independent" does not mean "consensus-free" or
"authentication-free".

## 13. Storage backplane

Control and data traffic must be distinguishable.

Minimum transport support: TCP.

Optional fast paths:

- NVMe-oF/RDMA;
- RoCE/InfiniBand-backed RDMA;
- SPDK userspace transport;
- future GPU-aware transports.

Fabric may supply enrollment and reachability, but a storage provider may use a
dedicated underlay when that is required for bandwidth or latency. Encryption
requirements depend on whether the storage underlay is trusted; they must be
explicit and never silently disabled.

## 14. Scheduling contract

The consumer scheduler must understand at least:

```
free_local_direct_devices
free_local_direct_bytes
free_replicated_async_bytes
free_cluster_durable_bytes
storage_cell_health
storage_numa_locality
storage_transport_capabilities
replica_locality
```

Placement must satisfy compute/GPU and storage constraints together.

For example, a request for 4 GPUs and one local-direct NVMe cannot be placed on
a host that has the GPUs but no eligible direct-pool device.

## 15. Security

Required:

- authenticated host/cell identity;
- mTLS or equivalently strong peer authentication for control traffic;
- writer-generation fencing;
- tenant isolation of volume endpoints;
- secrets absent from serialized plans and ordinary logs;
- explicit sanitization policy before direct-device reassignment;
- no trust in device node names;
- no automatic adoption of foreign filesystem/signature state.

## 16. Observability

At minimum expose:

Host/device:

- device health and role;
- PCI/IOMMU/NUMA observations;
- ownership generation;
- Storage Cell health/version/restarts.

Replicated-async:

- primary host;
- replica hosts;
- local committed sequence;
- received/durable sequence per peer;
- replication lag bytes/time;
- dirty/resync range;
- writer generation;
- last successful barrier;
- migration barrier latency;
- degraded/possible-loss state.

Ceph:

- provider health summary;
- OSD/cluster health surfaced without redefining Ceph health semantics.

## 17. Evidence gates

### 17.1 local-direct

Must prove on real hardware:

- exact stable device selected;
- direct VFIO assignment to workload;
- Storage Cell has no data-path ownership;
- device cannot be attached to two VMs;
- foreign IOMMU/device state fails closed;
- reboot/restart does not change ownership incorrectly;
- sanitize/reassignment policy works;
- measured latency/throughput reported against host-native NVMe baseline.

### 17.2 replicated-async

Must prove:

- local-first ACK semantics;
- peer lag is measurable and honest;
- process crash replay;
- peer disconnect and delta resync;
- source host power loss with known stale-replica behavior;
- stale writer rejection after partition;
- no dual writer under induced partition;
- successful migration barrier with no acknowledged write loss;
- failed migration leaves source authoritative;
- destination failure during barrier aborts safely;
- restart with partial metadata fails closed;
- performance against DRBD and Ceph controls.

### 17.3 cluster-durable

Must use Ceph's own health/recovery evidence plus integration tests for:

- device ownership;
- OSD restart;
- host loss;
- volume attach/detach;
- migration;
- upgrade;
- control-plane outage.

## 18. Non-goals for v0

- file storage API;
- S3/object API;
- native KV-cache API;
- new erasure-coding implementation;
- custom replacement for Ceph;
- transparent migration of local-direct;
- multi-writer replicated-async;
- a fixed disk ratio;
- unbounded automatic cluster formation from unauthenticated discovery.

## 19. Acceptance rule

No implementation may claim a class is supported until its exact-SHA real-host
evidence gate passes and the normative contract version is recorded with the
result.
