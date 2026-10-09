> **SUPERSEDED v1 draft — historical reference (2026-10-09).** The current proposed architecture is [ADR-0003](../adr/0003-tiered-volume-virtualization.md), [ADR-0004](../adr/0004-nearline-replication-and-mobility.md), and [SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md). Do not interpret v1 storage-class or provider contracts as compatible aliases for v2.

# Storage Cell v0 design review

Status: Review complete
Date: 2026-10-05
Reviewed branch: design/storage-cell-v0

## Verdict

**GO for prototype implementation and R&D.**

**NO-GO for any production durability, live-migration or performance claim**
until the evidence gates in SPEC-0001 and the contracts pass.

The architecture is internally coherent after review:

- local-direct has exclusive PCI ownership and bypasses the Storage Cell data
  path;
- replicated-async is explicitly non-zero-RPO in steady state;
- planned migration cannot succeed without a durable destination barrier and
  single-writer fencing;
- cluster-durable delegates distributed durability to Ceph rather than creating
  a new Ceph-like engine;
- no fixed 4/2/2 device ratio is embedded in the API.

## Corrections made during review

### 1. VFIO assignment boundary

The initial wording was too loose about passing an NVMe "controller/namespace"
through VFIO.

Corrected rule:

- the isolation unit is a PCI-addressable controller/function;
- ordinary namespaces on one shared controller are not independent VFIO
  isolation boundaries;
- namespace-level direct assignment is only valid where hardware exposes a
  separately assignable function with safe IOMMU isolation.

### 2. Runtime writer authority

The first draft made the internal replicated metadata/lease service sound
optional.

Corrected rule:

- replicated-async must preserve enough durable/fenced runtime state to keep
  current writer authority and replica progress correct during consumer
  control-plane outage;
- unsafe promotions/topology changes may fail closed if authority cannot be
  established.

### 3. Mayastor was initially treated too narrowly

Mayastor is Kubernetes-oriented as a packaged product, but its io-engine is a
separable Rust/SPDK data plane with direct Nexus APIs and management interfaces.

The R&D plan now requires a standalone Mayastor io-engine experiment before
Volvisor commits to writing a new SPDK engine.

Its main semantic mismatch remains important: current replicated Nexus writes
are synchronous, while Volvisor replicated-async requires local-first ACK plus
planned synchronous convergence.

## Reviewed invariants

### Device ownership

PASS at design level.

A device has exactly one role and one owner. Foreign or ambiguous ownership
fails closed. Destructive operations require explicit authority.

### local-direct

PASS at design level.

No Storage Cell foreground I/O. No transparent cross-host migration in v0.
The class is honestly host/device-local.

### replicated-async acknowledgement

PASS at design level.

ACK means local durable completion plus ordered replication state; it does not
mean remote durable completion.

### replicated-async migration

PASS at design level.

A successful handoff requires:

1. target replica preparation;
2. convergence;
3. source quiesce/flush;
4. barrier sequence;
5. target durable proof through the barrier;
6. source fencing;
7. newer target writer generation;
8. destination resume.

Failure/timeout aborts rather than weakening the barrier.

### unplanned failover

PASS at design level.

The design distinguishes safe, possible-loss and unsafe promotion. It does not
pretend failover equals migration.

### cluster-durable

PASS at architecture level.

Ceph is the durable engine. Volvisor must not silently downgrade the class to
replicated-async.

## Deliberately unresolved implementation choices

These are not design defects; they are gated decisions.

### Replication engine

Candidates:

- DRBD 9 baseline;
- standalone Mayastor io-engine reuse;
- clean O_DIRECT/io_uring Volvisor prototype;
- custom SPDK bdev only if justified by evidence.

### Host-local frontend

Initial preferred experiment: NVMe-oF/TCP over an isolated host-to-Storage-Cell
link, presented to the workload through a stable host block path.

vhost-user-blk and RDMA remain later candidates.

### Runtime lease/metadata implementation

The contract requires fencing semantics but does not prematurely select Raft,
an embedded quorum store or another mechanism.

### Ceph provider lifecycle

OSD ownership is in scope. Exact MON/MGR placement/bootstrap/upgrade requires a
follow-up design before implementation.

## Highest-risk area

The highest-risk component is not NVMe I/O performance. It is
replicated-async authority correctness:

- stale writer rejection;
- partition recovery;
- crash replay;
- migration barrier ordering;
- possible-loss failover classification.

The prototype should therefore optimize first for observable sequence/fencing
correctness, then for SPDK/RDMA latency.

## Final recommendation

Implement in this order:

1. provider plan + stable device discovery/ownership;
2. Storage Cell lifecycle;
3. local-direct real-host evidence;
4. replicated-async R0 local-endpoint invariant;
5. DRBD baseline;
6. standalone Mayastor io-engine experiment;
7. clean semantics prototype only if required;
8. adversarial failure campaign;
9. SPDK/RDMA optimization only after correctness;
10. Ceph provider integration under a separate lifecycle decision.

Do not merge an implementation that collapses the three storage classes into
performance labels or weakens their failure semantics.
