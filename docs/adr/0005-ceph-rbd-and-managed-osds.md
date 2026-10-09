# ADR-0005 — Ceph RBD volumes versus managed OSD infrastructure

Status: Proposed
Decision-accepted: pending (record acceptance date and accepting authority here)
Date: 2026-10-09
Related: [ADR-0003](0003-tiered-volume-virtualization.md), [SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md)

## The important distinction

Ceph **OSD** is a storage daemon consuming enrolled physical media. A tenant **RBD volume** is a logical image stored in a Ceph pool and served by RADOS across OSDs. One OSD is not one VM volume. Volvisor has *two separable Ceph responsibilities*:
1. `ceph-rbd` **volume adapter**: connect to a Ceph cluster, create images and map/detach them for Cloud Hypervisor workloads.
2. **Optional managed infrastructure provider**: claim physical disks and operate Ceph OSD daemons, perhaps in host Storage Cell VMs.

Do not gate the useful volume adapter on implementing a full new Ceph orchestrator.

## Decision: existing Ceph clusters first

Phase A is an adapter to an existing, already operated Ceph cluster. O3K/CellHV own tenant volume records and policy. The adapter:
- verifies Ceph cluster FSID, pool identity, namespace, service credentials and effective pool/health policy;
- maps `VolumeId` to a backend RBD image ID, never relying on mutable image name alone;
- provisions, authorizes, attaches, resizes or deletes only images it can prove it owns;
- enables safe single-writer attachment and explicit snapshot/clone support only after capability/evidence qualification;
- exports Ceph health, pool usable capacity, active client mappings and errors without converting Ceph's health into a Volvisor-made guarantee;
- leaves MON/MGR/OSD topology, CRUSH placement, failure recovery and upgrades under the existing cluster owner.

Reference v0 Cloud Hypervisor path is a **host-level Ceph RBD block mapping** using a supported kernel RBD client or qualified rbd-nbd where necessary, then virtio-blk to the VM. This must be bench-tested for cache durability, flush/FUA, reconnect, mapping recovery and live migration. `librbd` is a future frontend integration candidate, **not** a Cloud Hypervisor API feature to assume exists. Credentials and kernel mappings are host-scoped; guest tenants do not get Ceph admin keys.

A Ceph backend may outlive compute host loss, but VM live migration still needs guest/VMM/frontend compatibility and correct RBD image exclusive-writer handoff. The storage adapter must never assume that RBD makes any arbitrary guest device migratable.

## Decision: OSD operation is a separate, higher-risk phase

Phase B may deploy OSDs on disks assigned to Storage Cell VMs, but only through an accepted follow-up operations design. That design must establish:
- independent Ceph control quorum/MON/MGR placement and seed/bootstrap sequence;
- cluster ID/OSD ID/BlueStore device ownership and explicit data-destruction tokens;
- consistent CRUSH host/rack/room failure domains **representing physical hosts**, not hidden Storage Cell VM IDs;
- data-device passthrough and truthful media health/SMART reporting;
- NUMA and network data path capacity, and Storage Cell VM resource reservations;
- cell restart, hypervisor restart, host loss, disk loss and upgrade workflows;
- Ceph `size/min_size`, EC profiles, near-full/full, recovery/backfill and blocked I/O semantics;
- avoiding a circular dependency where the Storage Cell VM boot disk or critical quorum state requires the Ceph service that same cell is supposed to recover;
- blast-radius limits: one Storage Cell failure should not inadvertently remove excessive OSD capacity and break quorum/availability.

An external Ceph cluster needs none of this managed-OSD work. An OSD may run in a dedicated service VM or directly on infrastructure where evidence proves that is more reliable than a consolidated Storage Cell. The 'exactly one Storage Cell per host' proposal is **not** an inflexible requirement for managed Ceph in v2.

## Experimental Rook-only Cell deployment (separate GO/NO-GO decision)

[ADR-0008](0008-rook-only-hyperconverged-cells.md) defines a distinct **optional experimental** managed-OSD mode in which each hyperconverged physical host launches a fixed-resource Cloud Hypervisor storage-worker microVM with exclusively passed NVMe controller(s). Each guest joins an *independent existing Kubernetes cluster* as a labeled/tainted Node; **unpatched upstream Rook** initially selects explicit cell Nodes, devices and resources. The Rook operator/Kubernetes API stay outside these cells. A cell may run necessary kubelet/containerd/CNI agents in addition to Ceph pods, but no general tenant workloads.

This is not a new storage class: workloads still consume `ceph-rbd` images. It is an alternative to external Ceph infrastructure for running the **OSD/MON/MGR service**, and must never be assumed production-ready based on Rook's ordinary host-storage support. [POC gate](../poc/rook-cells/README.md) requires real PCI/VFIO isolation, 3 physical host CRUSH domains, resource reservation, pod scheduling policy, failure recovery and baseline performance. No Rook fork before evidence.

## Failure claims and migration

Ceph durability is defined by actual cluster policy, configured placement and healthy state. The backend may be unavailable or unsafe under loss of too many OSDs, MON quorum, full pool, or violated failure-domain topology. Never claim every `ceph-rbd` image is generically RPO=0 without describing the Ceph committed-write and client cache/flush contract.

Volvisor's optional *local mirror* for native/nearline is not automatically inserted beneath Ceph OSD disks; by default Ceph itself handles distributed copies or erasure coding. The user should be able to place `native-local`, `nearline-replicated` and `ceph-rbd` volumes on the same VM with explicit effective protection. Any attached pinned local volume still blocks general VM live migration.

## Acceptance

Phase A: prove image isolation, multi-tenant auth, attach/single writer, cache+flush integrity, host/client restart and reconnect, Ceph health degradation, OSD loss, cluster outage, detach/delete replay and Cloud Hypervisor migration with all attached disks.

Phase B: additionally require exact hardware/OSD lineage, safe creation and replacement, quorum and CRUSH topology checks, loss of a Storage Cell with several OSDs, rolling upgrades, no bootstrap dependency cycle, recovery-thrash/capacity tests and upgrade rollback.

External references (informative):
- https://docs.ceph.com/en/latest/rbd/
- https://docs.ceph.com/en/latest/rados/operations/add-or-rm-osds/

Neither phase is implemented or production-certified by this ADR.
