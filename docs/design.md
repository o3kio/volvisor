> **SUPERSEDED v1 draft — historical reference (2026-10-09).** The current proposed architecture is [ADR-0003](adr/0003-tiered-volume-virtualization.md), [ADR-0004](adr/0004-nearline-replication-and-mobility.md), and [SPEC-0002](specs/SPEC-0002-volvisor-volume-virtualization.md). Do not interpret v1 storage-class or provider contracts as compatible aliases for v2.

# Volvisor Storage Cell — engineering design

Status: Proposed
Applies to: Storage Cell v0

Normative order:

1. docs/adr/0001-volvisor-storage-cell-architecture.md
2. docs/adr/0002-replicated-async-local-endpoint-and-migration.md
3. docs/specs/SPEC-0001-storage-cell-v0.md
4. contracts/volvisor-provider-v1.md
5. contracts/storage-class-semantics-v1.md
6. contracts/replicated-async-v1.md

The R&D comparison in docs/research/replicated-async-rnd.md is non-normative.

## 1. Product shape

Volvisor converts local storage hardware in O3K/CellHV hosts into three explicit
failure contracts.

~~~
                         O3K / CellHV
                              |
                      placement / plan
                              |
       +----------------------+----------------------+
       |                                             |
   host agent                                    Fabric /
       |                                      storage underlay
       |
  +----+--------------------------------------------------+
  |                        HOST                           |
  |                                                      |
  | workload VMs                                         |
  |    |              |                    |             |
  | local-direct   replicated-async    cluster-durable   |
  |    |              |                    |             |
  |   VFIO      host-local presentation    Ceph/RBD      |
  |    |              |                    |             |
  | direct NVMe       +------ Storage Cell VM -------+   |
  |                          |               |           |
  |                      async NVMe       Ceph NVMe      |
  +--------------------------+---------------+-----------+
                             |               |
                         peer cells       Ceph peers
~~~

The Storage Cell is not in the local-direct data path.

## 2. Why a Storage Cell VM

The storage stack is large, privileged and operationally independent from the
hypervisor.

Putting it in a Cloud Hypervisor VM provides:

- a failure boundary around Ceph/SPDK/replication processes;
- a versioned image independent of the host base OS;
- explicit PCI ownership;
- resource reservation and NUMA placement;
- a consistent appliance shape across O3K and CellHV.

This also creates a failure amplifier: one cell failure can temporarily remove
several cell-owned devices. The cell is therefore infrastructure, not an
ordinary tenant VM. Reserved resources, watchdog, deterministic restart and
real-host evidence are mandatory.

## 3. Host agent boundary

The host agent is intentionally small. It owns operations that require host
privilege:

- device discovery and stable identity;
- IOMMU group verification;
- VFIO binding and ownership transfer;
- Storage Cell VM lifecycle;
- host-local volume presentation;
- journal-before-mutate state;
- fencing integration.

It does not become a general storage server.

## 4. Direct pool

Direct-pool hardware remains controlled by the host agent until leased.

The device transitions from free direct capacity to an exclusive workload
attachment, is rebound through VFIO, and is assigned directly to the workload
VM.

The Storage Cell may receive health/inventory observations but must not own or
proxy the device while it is attached.

## 5. Async pool

Async-pool devices are attached to the Storage Cell.

Logical model:

~~~
volume V on host A

VM
 |
stable local presentation V
 |
Storage Cell A
 |
local replica
 |
+---- background ordered replication ----> Storage Cell B replica
~~~

The first prototype optimizes for correctness visibility rather than minimum
microseconds. A standard local NVMe-oF/TCP frontend is acceptable until
measurement proves it is the bottleneck.

## 6. Durable pool

Durable-pool devices are attached to the Storage Cell and used by Ceph.

Volvisor does not redefine Ceph durability. The Ceph provider adapter maps
Volvisor device ownership and volume lifecycle to Ceph while preserving Ceph
health semantics.

## 7. Control plane and runtime

O3K or CellHV remains the consumer control plane. It discovers capabilities,
assigns roles and compiles plans.

Volvisor does not introduce a second mandatory tenant-facing control plane.

After activation, established I/O must continue through a consumer control-plane
outage when the local runtime and its peers remain healthy. Unsafe topology
changes may fail closed until authority returns.

## 8. Data paths

~~~
local-direct:
workload -> VFIO -> physical NVMe

replicated-async:
workload -> host-local presentation -> local Storage Cell -> local NVMe
                                               |
                                               +-> async peer replication

cluster-durable:
workload -> Ceph provider -> Ceph cluster
~~~

## 9. Storage network

Fabric may provide identity, enrollment and policy for storage peers.

The data plane is not required to traverse the tenant overlay. A trusted
storage underlay or RDMA fabric may be selected when it is materially better
for storage traffic.

TCP is the baseline transport. RDMA is an optimization, not a correctness
dependency.

## 10. Migration

local-direct:

- pinned/non-migratable while attached in v0.

replicated-async:

- prepare equivalent destination local endpoint;
- continuously converge target replica during memory pre-copy;
- quiesce and flush;
- prove destination durable through the barrier sequence;
- fence source;
- transfer writer generation;
- resume on destination.

cluster-durable:

- uses shared durable backend semantics and the selected Cloud Hypervisor/Ceph
  integration.

## 11. AI-cloud mapping

Typical policy:

~~~
KV cache / scratch / disposable hot data
    -> local-direct

model cache / warm KV / intermediate checkpoint
    -> replicated-async

VM root/data disks / databases / critical checkpoint
    -> cluster-durable
~~~

These are policy defaults, not hard-coded content types.

## 12. Design principle

Volvisor follows the same engineering instinct as Fabric:

**Compose proven low-level primitives, place strict O3K contracts around
ownership and lifecycle, and write new distributed machinery only where an
existing primitive does not match the required semantics.**

For v0 that means VFIO for direct storage, Ceph for durable cluster storage,
and focused R&D only for the local-first replicated tier.
