# volvisor

Volvisor is the shared **storage virtualization layer** for O3K and CellHV.

It turns storage devices already present in a hyperconverged host into three
explicit storage classes:

- `local-direct` — exclusive physical NVMe passthrough for the lowest-latency,
  host-local failure domain;
- `replicated-async` — host-local block storage with asynchronous peer
  replication and a synchronous planned-migration barrier;
- `cluster-durable` — Ceph-backed durable cluster storage.

The core runtime unit is a per-host **Storage Cell** VM under Cloud Hypervisor,
plus a deliberately small privileged host agent for discovery, VFIO ownership,
fencing and host-local presentation.

## Documentation map

| Document | Purpose |
|---|---|
| [ADR-0001](docs/adr/0001-volvisor-storage-cell-architecture.md) | Storage Cell architecture and the three storage classes |
| [ADR-0002](docs/adr/0002-replicated-async-local-endpoint-and-migration.md) | Local-endpoint invariant, async replication and planned-migration barrier |
| [SPEC-0001](docs/specs/SPEC-0001-storage-cell-v0.md) | Storage Cell v0 technical specification and evidence gates |
| [Provider contract v1](contracts/volvisor-provider-v1.md) | Device ownership, mutation, fencing and lifecycle invariants |
| [Storage-class semantics v1](contracts/storage-class-semantics-v1.md) | External failure/acknowledgement semantics |
| [Replicated-async contract v1](contracts/replicated-async-v1.md) | Engine-independent replication and migration correctness contract |
| [Engineering design](docs/design.md) | Consolidated explanatory design |
| [Replicated-async R&D](docs/research/replicated-async-rnd.md) | Gluster/Xen historical pattern and DRBD/Mayastor/SPDK alternatives |
| [Design review](docs/reviews/storage-cell-v0-review.md) | Adversarial review, corrections, open decisions and prototype verdict |

## Status

The documents are currently **Proposed**.

They intentionally make no production-support, durability, migration or
performance claim until an implementation passes the exact-SHA real-host
evidence gates defined by SPEC-0001 and the relevant contracts.

## Design principle

Volvisor follows the same engineering instinct as O3K Fabric: compose proven
low-level primitives, put strict O3K contracts around ownership and lifecycle,
and write new distributed machinery only where an existing primitive does not
match the required semantics.

For v0:

- direct storage uses VFIO/PCI passthrough;
- durable cluster storage uses Ceph;
- new R&D is concentrated on the local-first `replicated-async` tier.
