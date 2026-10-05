# ADR-0001 — Volvisor Storage Cell architecture

Status: Proposed  
Date: 2026-10-05  
Decision-accepted: pending  
Supersedes: none  
Superseded-by: none

Related documents:

- [SPEC-0001 — Storage Cell v0](../specs/SPEC-0001-storage-cell-v0.md)
- [Volvisor provider contract v1](../../contracts/volvisor-provider-v1.md)
- [Storage-class semantics v1](../../contracts/storage-class-semantics-v1.md)
- [ADR-0002 — Replicated-async local endpoint and migration barrier](0002-replicated-async-local-endpoint-and-migration.md)
- [Replicated-async R&D](../research/replicated-async-rnd.md)
- O3K Fabric: https://github.com/o3kio/fabric
- Cloud Hypervisor: https://github.com/cloud-hypervisor/cloud-hypervisor

This ADR is an architecture proposal. It creates no production, durability,
migration, or performance claim until the SPEC and contracts are implemented
and their evidence gates pass.

## Context

O3K already has a deliberate separation between compute and network
virtualization:

- Cloud Hypervisor provides the VM execution boundary;
- Fabric provides the shared network realization used by O3K and CellHV;
- storage still needs an equivalent O3K-native realization layer.

The goal is not to create another general-purpose distributed filesystem and
not to replace Ceph. The goal is to turn storage devices already present in an
O3K/CellHV host into explicit storage service classes with different failure,
latency, locality, and migration semantics.

The target deployment is hyperconverged x86 infrastructure. A host may contain
multiple NVMe devices and use different physical devices for different classes.
A representative prototype layout is:

```
8 NVMe devices on one host

NVMe 0..3  -> local-direct
NVMe 4..5  -> replicated-async capacity pool
NVMe 6..7  -> cluster-durable / Ceph OSD capacity
```

This 4/2/2 split is an example only. It is not an API contract and must never
be assumed by scheduling or provider code.

Prototype terminology maps to the normative class names as follows:

| Prototype term | Normative Volvisor class |
|---|---|
| direct NVMe | `local-direct` |
| fast-async NVMe | `replicated-async` |
| cluster-slow / Ceph | `cluster-durable` |

The semantic names are intentional: performance changes with hardware, while
failure and acknowledgement guarantees must remain stable.

The architecture must also fit AI-cloud workloads. AI systems frequently need
both ends of the storage spectrum: device-local disposable capacity for KV
cache, scratch and hot datasets; and durable shared storage for customer data,
VM disks and checkpoints. A single "fast/slow disk" abstraction hides the
failure semantics that matter.

## Decision

### 1. Volvisor is a storage virtualization layer, not a new storage engine

Volvisor is the storage counterpart to the O3K Fabric provider concept.

```
Compute virtualization  -> Cloud Hypervisor
Network virtualization  -> Fabric
Storage virtualization  -> Volvisor
```

Volvisor owns discovery, device classification, ownership, fencing, lifecycle,
presentation, scheduling vocabulary and evidence. It composes existing storage
primitives where those primitives already solve the hard problem correctly.

In particular:

- direct device assignment uses VFIO/PCI passthrough;
- the fast replicated class is an R&D surface and may use NVMe-oF, SPDK,
  io_uring/O_DIRECT or another proven block datapath;
- the cluster-durable class uses Ceph; Volvisor does not reimplement Ceph
  durability, recovery, scrubbing or erasure coding.

### 2. One Storage Cell VM per enrolled host

Each enrolled storage-capable host runs one privileged **Storage Cell** VM
under Cloud Hypervisor.

The Storage Cell owns the storage software that should be isolated from the
hypervisor host, including the replicated-async engine and the Ceph provider
components assigned to that host.

The host itself keeps a deliberately small privileged **Volvisor host agent**.
The host agent owns only operations that cannot safely be delegated to a guest:

- hardware/NVMe discovery;
- stable device identity verification;
- IOMMU group validation;
- VFIO bind/unbind and PCI ownership transfer;
- Storage Cell VM creation, restart and image lifecycle;
- creation of host-local presentation endpoints;
- journal-before-mutate ownership records and fencing.

The host agent is not a storage data engine.

### 3. Not every disk belongs to the Storage Cell VM

Physical devices are classified before use. A direct-pool device and a
Storage-Cell-owned device are different ownership states.

A device assigned to `local-direct` is passed directly to a workload VM and
is not simultaneously attached to the Storage Cell. A device assigned to
`replicated-async` or `cluster-durable` is assigned to the Storage Cell and
must not be directly attached to a tenant workload.

No PCI device may have two owners. Ambiguous ownership fails closed.

### 4. Three storage classes

#### `local-direct`

A whole PCI-addressable NVMe controller/function is exclusively leased to one
workload and passed to the workload VM through VFIO. Ordinary namespace
subdivision on a shared controller is not treated as an isolation boundary;
an individual namespace is eligible only when hardware exposes it through an
independently assignable PCI function with safe IOMMU isolation.

Data path:

```
workload VM -> guest NVMe driver -> PCIe/VFIO -> physical NVMe
```

Required semantics:

- lowest-latency class;
- no Volvisor data path;
- host/device failure domain;
- no replication by Volvisor;
- no transparent live migration while the device is attached in v0;
- suitable for KV cache, training scratch, disposable hot datasets and other
  reconstructable data.

The scheduler must treat a workload with `local-direct` storage as
storage-pinned unless a future device-migration contract explicitly says
otherwise.

#### `replicated-async`

A block volume is served from storage local to the workload host and is
replicated to one or more peer Storage Cells outside the foreground
acknowledgement path.

Steady-state acknowledgement means **local durable commit**, not remote durable
commit. Therefore the class has a non-zero RPO under unplanned host loss.

The important invariant is locality:

```
workload on host A -> host-A local volume endpoint -> host-A NVMe
workload on host B -> host-B local volume endpoint -> host-B NVMe
```

The volume identity is stable while the physical serving node changes.

Planned live migration uses a separate convergence/barrier protocol defined by
ADR-0002: before ownership transfers, the target replica must be proven caught
up through the source's committed sequence, outstanding writes must be flushed,
and the single-writer lease must be fenced and transferred. Thus steady-state
replication may be asynchronous while a successful planned migration is
lossless at the storage handoff.

#### `cluster-durable`

This class is backed by Ceph.

Volvisor provides lifecycle and presentation integration but does not implement
a replacement distributed storage engine.

Required semantics include:

- multi-node durable storage according to the configured Ceph policy;
- no acknowledgement semantics weaker than the selected Ceph backend contract;
- volumes remain usable independently of any one compute host;
- normal shared-storage migration semantics are expected, subject to the
  Cloud Hypervisor and O3K migration contracts.

The initial implementation may run OSD roles inside Storage Cells. Placement of
MON/MGR and exact Ceph bootstrap/lifecycle are provider details that require a
separate accepted design before a production claim.

### 5. No separate mandatory Volvisor control plane

O3K or CellHV is the consumer control plane. It discovers storage-capable hosts,
compiles device/Storage Cell plans and activates Volvisor.

Volvisor must not require a second always-available central service merely to
keep established volume I/O running.

For replicated-async, the Volvisor runtime must maintain enough durable,
fenced runtime metadata to preserve current writer authority and replica
progress while the consumer control plane is absent. This may be an embedded
replicated metadata/lease service inside the Storage Cells; it is part of the
storage runtime, not a new tenant-facing control plane.

Required outage behavior:

- loss of the O3K/CellHV control plane must not interrupt already established
  `replicated-async` or `cluster-durable` I/O solely because the control
  plane is absent;
- existing local-direct attachments remain attached;
- unsafe topology-changing operations may fail closed while authoritative
  control-plane state is unavailable;
- no cell may invent a new tenant allocation merely because the consumer
  control plane is unreachable.

### 6. Storage network is a first-class backplane

Replication, Ceph, health and migration traffic use a storage backplane.

Fabric may provide identity, enrollment, addressing and policy, but Volvisor
must not require high-throughput storage data to traverse an unnecessary
WireGuard/VXLAN overlay when a dedicated trusted storage underlay or RDMA
network exists.

The transport contract must permit:

- TCP as the minimum portable transport;
- direct trusted underlay paths;
- RDMA/RoCE where supported;
- later GPU/data-plane transports without changing volume identity semantics.

### 7. Storage-class ratios are policy, never architecture

No fixed "4 direct / 2 replicated / 2 Ceph" assumption exists.

Examples of valid hosts include:

```
AI compute       6 direct / 2 replicated / 0 Ceph
general compute  2 direct / 2 replicated / 4 Ceph
storage-heavy    0 direct / 2 replicated / 10 Ceph
```

The scheduler consumes discovered capacity by class and stable device identity.

### 8. Safety and ownership rules are stricter than networking

A mistaken network mutation can disconnect a host; a mistaken storage mutation
can destroy customer data. Volvisor therefore adopts the following mandatory
discipline:

- journal before mutation;
- stable hardware identity, never `/dev/nvmeXnY` alone;
- foreign-state rejection;
- generation-fenced ownership;
- explicit claim before destructive initialization;
- no automatic adoption of an unknown formatted device;
- no wipe, sanitize or repartition operation without current ownership proof;
- replay-safe apply/remove operations;
- secrets and encryption keys excluded from plans, journals and logs.

## Consequences

Positive:

- storage becomes an O3K-native provider abstraction rather than a collection of
  per-product scripts;
- AI workloads can explicitly choose locality/durability semantics;
- local-direct preserves the physical NVMe path;
- replicated-async can optimize for local latency without pretending to have
  synchronous durability;
- Ceph remains the proven durable cluster backend;
- the Storage Cell VM isolates large and fast-moving storage stacks from the
  hypervisor host;
- O3K and CellHV can consume the same Volvisor contracts.

Negative / accepted risks:

- a Storage Cell failure can temporarily remove multiple cell-owned devices from
  service even when the physical SSDs are healthy;
- the Storage Cell therefore requires reserved CPU/memory, watchdog,
  deterministic restart and NUMA-aware placement;
- replicated-async is new R&D and must not be advertised as production durable
  before crash, fencing, recovery and migration evidence exists;
- local-direct workloads are pinned by storage;
- Ceph operation remains operationally non-trivial even though its data engine
  is not rewritten.

## Non-goals

- replacing Ceph;
- implementing a new general-purpose distributed filesystem;
- making `local-direct` live-migratable in v0;
- claiming RPO=0 for steady-state `replicated-async`;
- exposing file, S3, KV-cache and block protocols simultaneously in v0;
- automatic destructive claiming of discovered disks;
- requiring homogeneous hosts or a fixed disk ratio.

## Required follow-up

Implementation may begin only against SPEC-0001 and the v1 contracts.
Replicated-async implementation selection remains gated by the R&D comparison
in `docs/research/replicated-async-rnd.md`.
