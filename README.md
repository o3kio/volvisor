# Volvisor

Volvisor is the **volume virtualization and orchestration layer** for O3K and CellHV. It presents one managed block-volume API across **three distinct backends**, while preserving their real locality, durability, and mobility semantics.

| v2 volume class | What it means | Live migration |
|---|---|---|
| `native-local` | One or more logical VM volumes provisioned from host-attached local disks/pools; optional independent local mirror | Pinned while attached in v0 |
| `nearline-replicated` | VM normally reads/writes its serving host's local replica; changes replicate to other hosts asynchronously; optional local mirror | Only after target durable barrier and safe single-writer handoff |
| `ceph-rbd` | RBD images in an existing or optionally managed Ceph cluster; Ceph OSDs own physical backing disks | Shared-backend migration only after VMM/lock/frontend evidence |

Physical NVMe **PCI/VFIO passthrough** is an optional exclusive attachment profile, *not* the same operation as creating a logical native-local volume.

## Why it exists

Conventional stacks often tie a VM to one storage implementation. Volvisor separates **volume identity, protection policy, placement and lifecycle** from the backend. A VM can consume different storage classes using a common management model; it does **not** mean the classes have the same durability or that Volvisor inserts a proxy into every fast path.

```text
                O3K / CellHV (volumes + attachments)
                               |
                    Volvisor volume API
            +------------------+--------------------+
            |                  |                    |
      native-local     nearline-replicated       ceph-rbd
            |                  |                    |
       local LV/pool     local Storage Cell     RBD mappings
            |             + peer replicas            |
       attached NVMe      attached NVMe         Ceph cluster / OSDs
```

Local SSD protection (e.g. a qualified local mirror) and remote host replication are independent. Planned migration requires a verified storage handoff; unplanned failover can lose the async replication tail.

## Current proposal / normative v2 documents

- [ADR-0003 — volume architecture](docs/adr/0003-tiered-volume-virtualization.md)
- [ADR-0004 — nearline replication and migration](docs/adr/0004-nearline-replication-and-mobility.md)
- [ADR-0005 — Ceph RBD versus OSD lifecycle](docs/adr/0005-ceph-rbd-and-managed-osds.md)
- [ADR-0006 — online resize and live local block relocation](docs/adr/0006-online-resize-and-live-local-block-relocation.md)
- [ADR-0007 — DRBD 9 nearline replication backend](docs/adr/0007-drbd9-nearline-replication-provider.md)
- [ADR-0008 — Rook-only hyperconverged Volvisor Cells](docs/adr/0008-rook-only-hyperconverged-cells.md)
- [Experimental Rook Cell contract](contracts/rook-cell-experimental-v0.md)
- [Three-host Rook Cell GO/NO-GO POC](docs/poc/rook-cells/README.md)
- [ADR-0008 — experimental Rook-only hyperconverged cells](docs/adr/0008-rook-only-hyperconverged-cells.md)
- [SPEC-0002 — v2 implementation specification](docs/specs/SPEC-0002-volvisor-volume-virtualization.md)
- [Volume API contract v2](contracts/volume-api-v2.md)
- [Nearline replication contract v2](contracts/nearline-replication-v2.md)
- [2026-10-09 adversarial design review](docs/reviews/2026-10-09-volume-architecture-review.md)

## Historical draft v1 (superseded proposals)

[ADR-0001](docs/adr/0001-volvisor-storage-cell-architecture.md), [ADR-0002](docs/adr/0002-replicated-async-local-endpoint-and-migration.md), [SPEC-0001](docs/specs/SPEC-0001-storage-cell-v0.md), [provider v1](contracts/volvisor-provider-v1.md), [storage classes v1](contracts/storage-class-semantics-v1.md), [replication v1](contracts/replicated-async-v1.md), [engineering design](docs/design.md), [replication research](docs/research/replicated-async-rnd.md), and [prior review](docs/reviews/storage-cell-v0-review.md).

The v1 drafts remain available for traceability and historical design research. The v2 documents replace conflicting class definitions; v1 cannot be reinterpreted silently. Some historical R&D comparisons remain useful but must be revalidated against the pinned implementation versions.

## Status

**Design proposal, not implementation.** No class is production supported; no durability, performance, zero-RPO failover, or live-migration capability is claimed until exact-SHA real-host conformance and fault-injection evidence passes. The first recommended implementations are a host-native logical-volume provider and an adapter to an existing Ceph RBD cluster. Nearline distributed authority is the highest-risk R&D area.

Online growth of a local LV is distinct from **same-host live relocation** of its backing storage, which is itself distinct from cross-host live VM migration. See ADR-0006 for native Cloud Hypervisor resize, same-VG LVM pvmove, and optional QEMU Storage Daemon + vhost-user-blk block mirroring.

For nearline, **DRBD 9 is the first replication-engine prototype candidate**, operating directly above LVM logical volumes (host-kernel path) or from an isolated Storage Cell (via qualified host-to-cell block frontend). Protocol A provides local-first asynchronous replication; Protocol C is an explicit optional synchronous-protection profile. The difficult cross-host Cloud Hypervisor migration handoff is **not** considered solved merely by using DRBD; see ADR-0007.

Optional **experimental managed-Ceph mode**: Volvisor boots fixed-resource Cloud Hypervisor `volvisor-cell` infrastructure VMs with exclusively VFIO-assigned NVMe, joins them as dedicated Kubernetes worker Nodes and permits only Rook/Ceph plus essential node-system workloads. Rook operator/control plane remain independent. This mode is *disabled by default and NOT production supported*; three **physical** host Rook/Ceph failure and performance tests are required. No Rook fork is presumed.
