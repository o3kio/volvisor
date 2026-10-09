> **SUPERSEDED v1 draft — historical reference (2026-10-09).** The current proposed architecture is [ADR-0003](../docs/adr/0003-tiered-volume-virtualization.md), [ADR-0004](../docs/adr/0004-nearline-replication-and-mobility.md), and [SPEC-0002](../docs/specs/SPEC-0002-volvisor-volume-virtualization.md). Do not interpret v1 storage-class or provider contracts as compatible aliases for v2.

# Volvisor provider contract v1

Status: Proposed  
Version: 1  
Applies to: Volvisor Storage Cell v0
Superseded-by: [SPEC-0002](../docs/specs/SPEC-0002-volvisor-volume-virtualization.md) (pool/device ownership and lifecycle) and [Volume API v2](volume-api-v2.md); v1 operational invariants not restated there remain binding per SPEC-0002 section 13

Related:

- [ADR-0001](../docs/adr/0001-volvisor-storage-cell-architecture.md)
- [SPEC-0001](../docs/specs/SPEC-0001-storage-cell-v0.md)
- [Storage-class semantics v1](storage-class-semantics-v1.md)
- [Replicated-async v1](replicated-async-v1.md)

This document is normative. If descriptive design text disagrees with this
contract, this contract wins until it is deliberately versioned.

## 1. Provider boundary

The Volvisor provider realizes an accepted host storage plan.

It does not own tenant/project canonical models. O3K or CellHV owns those
models and compiles them into provider plans.

The provider is the only component permitted to perform Volvisor-owned device
role transitions on a host.

## 2. Mandatory provider inputs

A plan must bind:

- storage domain identity;
- local host identity;
- plan generation;
- exact Storage Cell image digest/version;
- exact stable device identities and requested roles;
- peer/membership input required by the selected provider;
- storage transport/network input;
- provider-specific references that contain no plaintext secrets.

Unknown required fields, unsupported versions and malformed identities fail
closed.

## 3. Stable hardware identity

The provider must never treat any of the following as sufficient ownership
identity by itself:

- `/dev/nvme0n1` or another Linux device node;
- PCI BDF;
- probe order;
- human-friendly model name.

The provider must derive and persist a stable identity from immutable or
device-stable identifiers where available and re-verify current observations
before mutation.

A BDF may be stored as an observation used for the current boot.

If the provider cannot prove that the current device is the previously claimed
device, it must not write to it.

## 4. Exclusive role ownership

Exactly one Volvisor role may own a device at one time:

- DirectPool;
- AsyncPool;
- DurablePool.

A physical controller/function cannot simultaneously be owned by a Storage Cell
and a workload VM.

A role transition requires:

1. current ownership proof;
2. no active attachment or dependent volume;
3. a newer accepted generation;
4. explicit destructive authorization when old data can be destroyed;
5. journal-before-mutate;
6. post-mutation observation proving the intended owner.

## 5. Journal-before-mutate

Before any privileged/destructive mutation, the provider writes a durable
intent record containing enough identity and generation information to
distinguish:

- planned current work;
- a replay after crash;
- stale work;
- foreign state.

The provider must make apply/reconcile idempotent.

A process/host restart between journal and mutation, or between mutation and
final state publication, must converge or fail closed without silently
claiming unrelated hardware.

## 6. Foreign-state rejection

Foreign state is never automatically adopted or deleted.

Examples:

- a device has signatures not owned by the current claim;
- an NVMe controller is already bound to an unknown VFIO consumer;
- an SPDK/NVMe-oF subsystem uses a Volvisor-derived name but is not proven by
  the ownership journal;
- a Ceph OSD signature exists but does not match the expected cluster/OSD
  identity;
- a volume endpoint exists with a mismatched generation.

The provider reports foreign state and refuses mutation until an explicit
recovery/adoption procedure proves intent.

Names are hints, never ownership proof.

## 7. Destructive operations

The following are destructive operations:

- filesystem/partition signature removal;
- NVMe format/sanitize;
- namespace recreation;
- Ceph OSD initialization;
- pool reinitialization;
- discard/zero operations intended to erase prior tenant data.

Discovery and ordinary plan application must not imply destructive consent.

Destructive operations require an explicit claim/action token bound to
`DeviceId`, host, generation and intended role.

## 8. Storage Cell VM contract

The provider must create at most one current Storage Cell VM per host/storage
domain unless a later contract explicitly supports multiple cells.

The cell must be bound to:

- host identity;
- image digest;
- cell generation;
- explicit cell-owned devices;
- reserved CPU and memory;
- storage network identity.

A restarted cell receives exactly the devices proven owned by the current
plan. It must never probe-and-adopt arbitrary host disks.

The provider must prefer NUMA-local CPU/memory placement for the cell relative
to owned NVMe/storage NICs and must expose when this is impossible.

## 9. local-direct contract enforcement

Before attaching a DirectPool device to a workload, the provider must prove:

- current DirectPool claim;
- no Storage Cell attachment;
- no other workload attachment;
- safe IOMMU isolation;
- expected device identity;
- current attachment generation.

On detach, the provider does not make the device reusable until it has proven
the old VM no longer owns the device and the configured sanitization/reuse
policy has completed.

The provider must never insert a Storage Cell proxy into the local-direct
foreground data path.

## 10. replicated-async provider enforcement

The provider must not report a replicated-async volume healthy unless the
replication engine can identify:

- current writer generation;
- current primary host/cell;
- each known replica;
- each replica's progress boundary;
- degraded/lagging state.

Any promotion or planned migration must satisfy
`contracts/replicated-async-v1.md`.

## 11. cluster-durable provider enforcement

The reference durable engine is Ceph.

Volvisor must not reinterpret a degraded Ceph volume as replicated-async or
claim durability beyond the underlying Ceph state.

Ceph data devices are cell-owned DurablePool devices.

Creation of a Ceph OSD on a device is destructive and follows section 7.

## 12. Control-plane outage behavior

Loss of O3K/CellHV connectivity alone must not cause the provider to:

- detach healthy established volumes;
- revoke a valid writer;
- destroy a local-direct attachment;
- stop a healthy Storage Cell;
- reinitialize devices.

The provider may reject new allocations, role changes, promotions and other
authority-changing operations while canonical authority is unavailable.

## 13. Peer authentication

Storage runtime peers must authenticate each other.

Unauthenticated peer discovery is never authority to replicate data, join
metadata quorum or receive volume contents.

Peer credentials and private keys must not appear in ordinary logs, plan JSON,
command-line arguments where avoidable, or ownership journals.

## 14. Fencing

Every authority-changing operation uses monotonically increasing generations or
an equivalent fencing token.

A stale generation must not regain write authority after:

- process restart;
- host restart;
- network partition heal;
- failed migration;
- control-plane reconnection.

A provider that cannot prove the current writer must stop admitting writes for
that volume rather than guess.

## 15. Endpoint isolation

Host-local storage endpoints must be inaccessible to unrelated tenants.

A loopback/private-link implementation must not accidentally expose NVMe-oF,
NBD, NFS or another storage protocol on a wildcard tenant-facing address.

Network reachability is not authorization; host/volume identity checks remain
mandatory.

## 16. Cleanup

Removal must be dependency ordered.

The provider removes only objects proven owned by the current Volvisor
generation.

A failed cleanup records residual state and remains retryable.

A cleanup error must not trigger broad best-effort deletion of similarly named
devices/subsystems.

## 17. Observability truthfulness

Metrics/state must distinguish:

- unknown;
- healthy/current;
- degraded;
- stale;
- possible-loss;
- unsafe/ambiguous;
- foreign/quarantined.

"Healthy" must never mean merely "process is running".

Replication lag and Ceph health must not be hidden behind a generic green cell
status.

## 18. Conformance requirements

The executable conformance suite must eventually cover at least:

- plan validation;
- stable-device mismatch rejection;
- replay after journal-before-mutate crash points;
- duplicate ownership rejection;
- foreign-state rejection;
- destructive-action token binding;
- cell restart preserving device ownership;
- direct attach/detach exclusivity;
- stale generation rejection;
- endpoint isolation;
- replicated-async writer fencing;
- cleanup idempotency.

Real-host evidence is separately required for VFIO, NVMe failure behavior,
migration, Ceph integration and performance.

## 19. Versioning

Breaking changes to provider plan shape, journal semantics, ownership
invariants, destructive-operation authority or fencing require a new contract
version and a migration note.
