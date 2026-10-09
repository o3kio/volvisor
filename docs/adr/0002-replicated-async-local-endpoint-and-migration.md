> **SUPERSEDED v1 draft — historical reference (2026-10-09).** Superseded for replication, migration handoff and failure handling by [ADR-0004](0004-nearline-replication-and-mobility.md); see also [ADR-0003](0003-tiered-volume-virtualization.md) and [SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md). Do not interpret v1 storage-class or provider contracts as compatible aliases for v2.

# ADR-0002 — Replicated-async local endpoint and migration barrier

Status: Proposed  
Date: 2026-10-05  
Decision-accepted: pending  
Supersedes: none  
Superseded-by: ADR-0004 (replication, migration handoff and failure handling where inconsistent)

Related:

- [ADR-0001 — Volvisor Storage Cell architecture](0001-volvisor-storage-cell-architecture.md)
- [SPEC-0001 — Storage Cell v0](../specs/SPEC-0001-storage-cell-v0.md)
- [Replicated-async contract v1](../../contracts/replicated-async-v1.md)
- [Replicated-async R&D](../research/replicated-async-rnd.md)

## Context

Volvisor needs a storage class between raw host-local NVMe and Ceph:

- lower write latency than a quorum/durable cluster path;
- a local data path on the host executing the VM;
- a recoverable peer replica;
- a migration model that does not force every foreground write to wait for a
  remote disk.

Kubedo previously operated a hyperconverged Xen design with an important
property worth preserving.

Each Xen host exported/mounted the VM storage through a **localhost NFS
endpoint**. The replicated filesystem underneath was GlusterFS. The important
property was not GlusterFS itself: every hypervisor used the same host-local
presentation model. During migration the VM moved to another host, and the
destination again opened a local endpoint backed by that host's synchronized
copy.

Conceptually:

```
old host A:
VM -> localhost NFS -> host-A replica

old host B:
VM -> localhost NFS -> host-B replica
```

That architecture produced useful locality and made the storage attachment
shape independent of VM placement.

GlusterFS is not selected for Volvisor. Its file-level distributed filesystem
and healing/split-brain model add complexity that is unnecessary for the
Volvisor block-storage problem. The historical implementation is retained only
as evidence that the **stable local endpoint + synchronized backing replica**
pattern is operationally useful.

Modern NVMe block technologies let Volvisor implement the same principle below
the filesystem layer and with explicit write ordering, sequence numbers,
fencing and migration barriers.

## Decision

### 1. Preserve the local-endpoint invariant

A `replicated-async` volume has one stable identity but may have replicas on
multiple Storage Cells.

The workload host always consumes the volume through a host-local presentation
endpoint. The exact mechanism is deliberately not fixed by this ADR.

Candidate realizations include:

- host-local NVMe-oF/TCP over a private Storage Cell link;
- NVMe-oF/RDMA on capable hosts;
- vhost-user-blk backed by SPDK;
- a temporary kernel block frontend for the first prototype.

The normative property is:

> VM placement changes the local serving replica, not the volume identity
> visible to the O3K/CellHV volume attachment model.

The endpoint must not be an unrestricted tenant network listener.

### 2. Steady-state foreground acknowledgement is local-first

For normal operation on the primary replica:

1. validate the volume writer lease/generation;
2. persist the write locally according to the volume's flush/FUA contract;
3. assign/commit a monotonically ordered replication sequence;
4. enqueue the write/range into the replication stream;
5. acknowledge the foreground write without waiting for remote durable media;
6. apply and durably advance replicas asynchronously.

The acknowledgement contract is therefore intentionally weaker than
synchronous replication.

A host failure can lose writes that were acknowledged locally but had not
reached another replica. The API, metrics and documentation must expose this as
a non-zero RPO. No implementation may market or report `replicated-async` as
RPO=0 steady-state storage.

### 3. Replication state is ordered and observable

Every volume has, at minimum, monotonic state equivalent to:

```
volume_generation
writer_generation
local_committed_seq
replica_received_seq
replica_durable_seq
```

Sequence values may be implemented as log positions, epochs plus offsets, or
another monotonic representation, but the provider must be able to prove
whether a target replica contains every committed write through a migration
barrier.

A replica may be stale; it must never silently claim to be current.

### 4. Planned live migration temporarily changes the rule

Steady-state replication is asynchronous. **Planned migration is not allowed to
transfer storage ownership asynchronously.**

The storage handoff is:

```
normal operation
  source local write + async replication
          |
          v
migration prepare
  ensure target replica exists and is healthy
          |
          v
memory pre-copy may run while normal writes continue
          |
          v
storage convergence
  increase/catch up replication until target approaches source
          |
          v
final stop window
  pause/quiesce VM writes
  flush source
  establish barrier sequence B
  wait until target durable_seq >= B
          |
          v
writer handoff
  revoke/fence source generation
  grant target a strictly newer writer generation
  activate target local endpoint
          |
          v
resume VM on destination
```

A successful planned migration therefore has a storage handoff with no
acknowledged write missing at the target, despite asynchronous steady-state
replication.

If the target cannot reach the barrier inside the migration downtime/timeout
policy, migration must abort and the source remains authoritative. The
implementation must prefer a failed migration over ambiguous dual ownership.

### 5. Single writer is mandatory

`replicated-async` v1 is single-writer block storage.

At most one cell/attachment generation may accept writes for a volume.

Writer authority requires a fencing token/generation that is validated on every
write path or at an equally strong admission boundary. A stale primary that
returns after partition must not become writable merely because it still has
the volume data.

No "last writer wins", timestamp arbitration, automatic dual-primary mode or
file-level merge is permitted.

### 6. Unplanned failover is different from planned migration

If the source host disappears before its latest acknowledged writes are
replicated, another replica may be behind.

The system must report:

- last known source committed sequence, if available;
- candidate replica durable sequence;
- whether RPO loss is possible or proven;
- the fencing state of the former writer.

Automatic promotion is permitted only when the policy and evidence prove it is
safe. Otherwise failover requires an explicit policy/administrative decision
that acknowledges possible loss of the unreplicated tail.

The system must never disguise data loss as a normal migration.

### 7. Crash recovery uses dirty-range/log replay, not full-copy by default

The replication engine must retain enough state to distinguish synchronized
from unsynchronized regions or log positions. Reconnecting a lagging replica
must resynchronize the dirty delta when correctness can be proven.

A full volume copy is allowed as a conservative fallback but must not be the
only normal recovery mechanism for multi-terabyte NVMe volumes.

### 8. GlusterFS is rejected as the Volvisor engine

GlusterFS was useful in the historical system because it supplied replicated
backing data below a stable localhost NFS presentation. It is not the selected
Volvisor engine.

Reasons:

- Volvisor requires a block contract, not a distributed POSIX filesystem;
- file/metadata healing semantics are unnecessary overhead for VM block
  volumes;
- split-brain diagnosis and healing are operationally complex;
- Volvisor needs explicit sequence/fencing/barrier semantics that can be
  reasoned about per block volume;
- the fast tier should be able to exploit modern NVMe and NVMe-oF paths.

The reusable idea is the local presentation invariant, not the GlusterFS
implementation.

### 9. The replication engine remains an R&D decision

This ADR fixes semantics, not the engine.

The implementation must be selected after comparing at least:

- DRBD 9/LINSTOR as a mature block-replication baseline;
- SPDK bdev/NVMe-oF with a Volvisor-owned replication journal;
- a kernel NVMe-oF or io_uring/O_DIRECT prototype that proves semantics before
  committing to a userspace poll-mode engine;
- Ceph RBD as a durability/performance control, not as the target semantics.

See the R&D document for the evaluation matrix.

## Why not make all writes synchronous?

Synchronous replication can provide RPO=0 for a single peer failure, but it
also puts remote network and remote-media latency on every foreground write.
That is a different storage class and overlaps the purpose of
`cluster-durable`.

Volvisor intentionally separates:

- `local-direct`: device-local and unreplicated;
- `replicated-async`: local-first with a bounded/observable replication lag;
- `cluster-durable`: durable shared Ceph storage.

Planned migration is allowed to pay a temporary synchronization cost because
migration is a control operation, not the common write path.

## Consequences

Positive:

- preserves local I/O locality across VM placement;
- planned migration can be storage-lossless without making every normal write
  synchronous;
- exact replica lag becomes measurable;
- block-level resync is a better fit for VM disks than file-level healing;
- the frontend can evolve independently from the replication engine.

Negative / accepted risks:

- unplanned failover can lose an acknowledged tail;
- migration requires a short storage barrier and therefore has a finite minimum
  downtime;
- fencing correctness becomes critical;
- a custom SPDK replication engine, if selected, is substantial correctness
  work and must not be confused with an ordinary cache.

## Non-goals

- concurrent multi-writer filesystems;
- automatic conflict merge;
- pretending asynchronous replication is durable cluster storage;
- making `local-direct` migratable through this mechanism;
- selecting GlusterFS as an implementation dependency.
