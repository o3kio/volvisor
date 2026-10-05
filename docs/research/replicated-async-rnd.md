# Replicated-async R&D — local endpoint, NVMe replication and migration

Status: Research / non-normative
Date: 2026-10-05

Normative semantics are defined by ADR-0002 and contracts/replicated-async-v1.md.
This document compares implementation paths; it does not select a production engine.

## 1. Target properties

Volvisor needs a fast block tier where foreground writes normally complete at
local-NVMe latency, at least one peer replica follows asynchronously, the VM
always consumes a host-local presentation, and planned live migration can force
the target replica to a lossless synchronization barrier before ownership moves.

Unplanned host loss is allowed to expose a non-zero RPO, but the exposure must
be observable and never disguised as cluster-durable storage. Dual writer is
forbidden.

## 2. Historical Kubedo/Xen prototype

A previous Kubedo hyperconverged Xen design used this shape:

~~~
host A: VM -> localhost NFS -> replicated backing data on host A
host B: VM -> localhost NFS -> replicated backing data on host B
~~~

GlusterFS supplied the replicated backing store.

The valuable property was not GlusterFS. The useful invariant was that each
hypervisor always reopened the same storage identity through a local endpoint.
During migration, the destination used its own local endpoint backed by the
synchronized destination copy.

The architecture lesson is:

**Keep the workload attachment local and stable; move and synchronize authority
behind that endpoint.**

Volvisor should preserve that principle below the filesystem layer.

## 3. Why GlusterFS is not selected

GlusterFS solves a distributed POSIX filesystem problem. Volvisor needs a
single-writer block-volume fast tier with explicit sequence, fencing and
migration-barrier semantics.

Gluster's own documentation includes data, metadata and entry/GFID split-brain
states and manual repair workflows:

https://docs.gluster.org/en/main/Troubleshooting/resolving-splitbrain/

Arbiter volumes exist specifically to reduce split-brain risk:

https://docs.gluster.org/en/main/Administrator-Guide/arbiter-volumes-and-quorum/

Those mechanisms are valid for Gluster's problem space but add filesystem
namespace/healing complexity that Volvisor does not need.

The old prototype remains design evidence for the local-endpoint idea only.

## 4. Modern local-endpoint equivalent

The literal localhost NFS mount should become a stable local block presentation.

Reference v0 shape:

~~~
Cloud Hypervisor workload
        |
     virtio-blk
        |
host stable device path
/dev/volvisor/<volume-id>
        |
host NVMe initiator
        |
private host-to-Storage-Cell link
        |
NVMe-oF/TCP target
        |
Storage Cell
        |
local NVMe replica
~~~

The private link is host-local in topology even if it is not literal loopback.
It must never be exposed to tenant networks.

Before migration, the same VolumeId is prepared on the destination:

~~~
source:      /dev/volvisor/V -> source local replica
destination: /dev/volvisor/V -> destination local replica
~~~

The canonical attachment identity stays stable while serving ownership changes.

## 5. Planned migration synchronization

Normal mode:

~~~
guest write
 -> source local durable commit
 -> sequence N
 -> ACK
 -> replicate N asynchronously
~~~

While Cloud Hypervisor performs memory pre-copy, source writes continue and the
destination continuously catches up.

Final handoff:

~~~
pause/quiesce workload writes
 -> flush source
 -> B = source committed sequence
 -> wait until destination durable sequence >= B
 -> fence source writer generation
 -> grant destination generation+1
 -> activate destination local endpoint
 -> resume migrated VM
~~~

This is the modern block/NVMe successor to the old rule: synchronize the
backing copy, then reopen the local endpoint on the target.

Steady state remains asynchronous. A successful planned migration is allowed to
temporarily pay synchronous convergence cost.

## 6. Candidate A — DRBD 9 / LINSTOR baseline

DRBD is the strongest existing correctness baseline because it already
distinguishes asynchronous, semi-synchronous and synchronous block replication.

DRBD documents:

- Protocol A: local completion plus outbound replication buffering;
- Protocol B: remote-memory/semi-synchronous completion;
- Protocol C: remote durable synchronous completion.

Reference:
https://linbit.com/drbd-user-guide/drbd-guide-9_0-en/

The DRBD guide also covers virtualization/live-migration scenarios. Volvisor
does not need to adopt dual-primary semantics; the important point is that
block replication, resynchronization and migration are mature comparison
targets.

Strengths:

- mature block-level replication;
- dirty-block resync;
- explicit sync/async semantics;
- useful failure and performance baseline.

Concerns:

- kernel-centric integration and lifecycle;
- less control over a future SPDK/GPU-oriented datapath;
- dependency, support and licensing implications need separate review.

R&D role: **mandatory baseline**.

If a clean Volvisor engine cannot demonstrate a material reason to differ from
DRBD, Volvisor should not invent one.

## 7. Candidate B — OpenEBS Replicated PV Mayastor reference

Mayastor is the closest public technical reference to the proposed NVMe
architecture.

Current OpenEBS documentation describes:

- a Rust I/O engine;
- SPDK bdev-based pools and replicas;
- NVMe-oF TCP presentation;
- a per-node Nexus;
- local plus remote replicas;
- remote replica access through SPDK NVMe-oF;
- synchronous N-way mirroring for replicated volumes.

References:

https://openebs.io/docs/user-guides/replicated-storage-user-guide/replicated-pv-mayastor/additional-information/io-path-description

https://github.com/openebs/mayastor

This validates the feasibility of:

~~~
local NVMe + SPDK + NVMe-oF + per-node I/O engine + remote replicas
~~~

It is not a drop-in Volvisor implementation because its lifecycle is
Kubernetes/CSI-oriented and replicated foreground writes are synchronous,
whereas Volvisor's target class is local-first asynchronous.

The project is Apache-2.0 at project level. Volvisor should still default to a
clean implementation unless source reuse is explicitly reviewed for provenance
and licensing.

R&D role: **architecture and performance reference**.

## 8. Candidate C — custom SPDK replication bdev

SPDK supplies the key low-level building blocks:

- userspace NVMe access;
- pluggable bdev modules;
- NVMe-oF TCP and RDMA targets/initiators;
- logical-volume primitives.

References:

https://spdk.io/doc/bdev.html

https://spdk.io/doc/nvmf.html

https://spdk.io/doc/bdev_module.html

A possible Volvisor engine:

~~~
                  frontend
                     |
              replicated bdev
              /             \
        local child       replication journal
            |                   |
         NVMe bdev          peer stream
                                |
                           remote bdev
~~~

A replication record needs at least writer generation, sequence, offset,
length, checksum, operation flags and payload/data reference.

Normal write:

1. reject stale writer generation;
2. write local data;
3. persist local ordering/journal state required for replay;
4. advance local committed sequence;
5. ACK;
6. stream ordered records to peers;
7. peer validates, applies, persists and advances durable sequence.

Reconnect should replay retained log first, then tracked dirty ranges, with
full-copy as a conservative fallback.

Advantages:

- exact fit for Volvisor semantics;
- maximum control of local ACK path;
- natural future NVMe-oF/RDMA path;
- SPDK runtime is isolated inside the Storage Cell.

Risks:

- Volvisor owns crash-consistent journal design;
- ordering and replay;
- replica divergence detection;
- fencing;
- resync;
- checksums;
- on-disk format/versioning.

A fast benchmark is not sufficient evidence.

R&D role: **preferred long-term research path only if baselines prove a
material advantage**.

## 9. Candidate D — semantics-first O_DIRECT/io_uring prototype

Before SPDK, prove the distributed state machine with a simpler engine:

~~~
raw block device
   |
O_DIRECT / io_uring
   |
Volvisor replication journal
   |
TCP peer replication
~~~

Advantages:

- easier debugging;
- less runtime integration complexity;
- proves sequence, barrier and fencing semantics;
- creates a clean comparison for SPDK.

Disadvantage:

- lower ultimate CPU efficiency/latency ceiling.

R&D role: **recommended first clean Volvisor prototype**.

Do not optimize the NVMe driver before proving the authority state machine.

## 10. Candidate E — Ceph RBD control

Ceph is not the target implementation for replicated-async. It is the durable
class and the performance/control baseline.

The question is whether a separate fast tier produces enough benefit to justify
its weaker RPO contract and additional code.

Benchmark Ceph for the same VM disk workloads, migration flows and recovery
load. If Volvisor replicated-async produces only marginal gains, a custom
engine is not justified.

## 11. Frontend alternatives

### NVMe-oF/TCP over a private host-cell link

Recommended first frontend.

Pros:

- standard Linux initiator;
- guest can still receive ordinary virtio-blk;
- stable host-local device mapping;
- kernel and SPDK target interoperability;
- straightforward benchmark/debug path.

Cons:

- TCP and virtual-network overhead in the local host-to-cell hop.

### NVMe-oF/RDMA

Later fast path.

Pros:

- lower CPU cost and latency on suitable hardware;
- strong fit for inter-cell replication.

Cons:

- NIC/network prerequisites and RoCE operational complexity.

### vhost-user-blk

Cloud Hypervisor supports vhost-user-blk backends such as SPDK:

https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/device_model.md

It is promising, but a Storage Cell VM cannot be treated as though it were an
ordinary host process. Socket ownership, isolation and migration semantics must
be designed first.

### NFS

Historical control only; not the intended fast-tier frontend.

## 12. Cloud Hypervisor live migration

Cloud Hypervisor supports remote live migration and configurable downtime and
timeout:

https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/live_migration.md

Volvisor must coordinate storage and memory migration. It must not assume the
VMM replicates storage.

Required phases:

~~~
PREPARE_TARGET_STORAGE
PRECOPY_MEMORY
CONVERGE_STORAGE
QUIESCE_AND_BARRIER
TRANSFER_WRITER_LEASE
RESUME_DESTINATION
DEMOTE_SOURCE
~~~

If the storage barrier cannot fit within the allowed downtime/timeout, migration
must abort and the source must remain authoritative.

## 13. Comparison matrix

| Candidate | Latency potential | Async fit | Migration/barrier fit | Engineering risk | O3K/Cell fit | Role |
|---|---:|---:|---:|---:|---:|---|
| Historical Gluster + NFS | 2 | 3 | 4 | 3 | 2 | rejected reference |
| DRBD 9 | 3 | 5 | 5 | 2 | 3 | mandatory baseline |
| Mayastor | 5 | 2 (sync-first) | 4 | 3 | 3 | architecture reference |
| O_DIRECT/io_uring custom | 4 | 5 | 5 | 4 | 5 | first clean prototype |
| SPDK custom bdev | 5 | 5 | 5 | 5 | 5 | long-term candidate |
| Ceph RBD | 2-4 | different contract | 5 | 1 for Volvisor | 5 | durable/control baseline |

Scores are hypotheses and must be replaced by measurements.

## 14. Recommended R&D sequence

### R0 — prove the local-endpoint invariant

Two Storage Cells, one stable VolumeId, no GlusterFS. Demonstrate that a VM can
consume the same canonical volume identity from either host.

### R1 — DRBD baseline

Measure 4K/16K/128K latency, sequential bandwidth, async replication lag,
resync cost, planned migration downtime and source-failure behavior.

### R2 — clean Volvisor semantics prototype

Use O_DIRECT/io_uring and TCP. Implement writer generations, sequence journal,
replica progress, dirty-range tracking, migration barrier and crash replay.

### R3 — adversarial failure campaign

Induce process SIGKILL, Storage Cell reboot, host power loss, network
partition, stale control replay, destination loss during barrier and old-source
return after promotion.

No dual writer is the primary gate.

### R4 — SPDK experiment

Replace the I/O/transport engine without changing the contract or failure
tests. Measure p50/p99/p999 latency, CPU per IOPS/GB/s, random write,
sequential bandwidth and performance during resync.

If SPDK does not materially beat the simpler engine, do not carry the
complexity.

### R5 — RDMA

Only after TCP semantics are proven.

## 15. Go/no-go criteria

Proceed with a custom engine only if all are true:

1. no dual-writer result in the adversarial suite;
2. successful planned migration loses no acknowledged writes;
3. possible-loss failover is classified honestly;
4. resync is incremental and bounded;
5. established I/O survives consumer-control-plane outage;
6. target fast-tier workloads materially outperform Ceph;
7. the custom path provides a reason not to simply use DRBD;
8. operations are simpler than adopting a Kubernetes-centric engine.

## 16. Recommendation

Prototype the semantics first with a simple block engine. Use DRBD as the
correctness baseline and Mayastor as the closest NVMe architecture reference.
Move to a clean SPDK replication bdev only after fencing/migration behavior is
proven and benchmarks show a meaningful reason to own the extra correctness
surface.

This preserves the successful idea from the historical Xen deployment without
carrying GlusterFS forward.
