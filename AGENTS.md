# AGENTS.md — Volvisor contributor and agent rules

## Read first (current v2 direction)

1. docs/adr/0003-tiered-volume-virtualization.md
2. docs/adr/0004-nearline-replication-and-mobility.md
3. docs/adr/0005-ceph-rbd-and-managed-osds.md
4. docs/adr/0006-online-resize-and-live-local-block-relocation.md
5. docs/specs/SPEC-0002-volvisor-volume-virtualization.md
6. contracts/volume-api-v2.md
7. contracts/nearline-replication-v2.md
8. docs/reviews/2026-10-09-volume-architecture-review.md

ADR-0001/0002, SPEC-0001 and v1 contracts are **superseded, unimplemented proposals**; keep for lineage. When an old draft contradicts v2, v2 is authoritative. Where v2 is silent, v1 operational invariants (storage backplane transport, control-plane outage behavior, peer authentication and secrets, endpoint isolation, observability truthfulness) continue to bind — see SPEC-0002 section 13. Each v2 ADR records its acceptance status in its header; do not treat a pending ADR as an accepted decision. Do not quietly map legacy class names to incompatible new semantics.

## Non-negotiable rules

1. A disk, pool, logical volume, replica, VM attachment and PCI passthrough lease are distinct resources with independent identity, generations, and ownership.
2. One native host disk may supply many logical volumes; `native-local` must never be reduced to whole-controller VFIO.
3. A local mirror does **not** count as a remote host replica; remote async replication does not ensure preservation of the latest ACKed writes on host loss.
4. Never weaken single-writer fencing, quorum or error reporting to make a test pass.
5. An `IN_DOUBT` handoff after source revocation must not be 'fixed' by blindly restarting source writes.
6. Migration eligibility is VM-wide, across all attached volumes and VMM devices; do not assert storage-safe handoff from memory migration success.
7. Discovery is always read-only, hardware identity is never solely `/dev/nvmeXnY` or BDF, and no destructive adoption of foreign state occurs.
8. Every privileged mutation journals intent and must be idempotent/fail-closed on stale generation and replay conflicts.
9. A Ceph OSD uses physical devices; VM block volumes are RBD images. Do not create an OSD per volume or implement a replacement Ceph engine.
10. Storage Cells are not on the native-local foreground I/O path in v2; placing one there requires a separately accepted design. Do not introduce a second mandatory tenant-facing control plane.
11. Strong guest flush/FUA, crash consistency, media failure and thin pool exhaustion must be tested explicitly.
12. Match every production-support claim to exact implementation source, hardware, VMM/backend versions and real-host failure evidence.
13. Do not reuse or translate third-party source without explicit provenance/license review; prefer existing DRBD/Ceph components before new engines.
14. Keep *online capacity growth*, *same-host backing relocation* and *cross-host VM live migration* separate. Native LV online growth does not require a block-copy job; no generic live cross-pool copy is implied by LVM pvmove.
15. Do not put an additional QSD/userspace proxy on all native foreground I/O solely to make it movable. Treat QSD/vhost-user-blk mirror/pivot as an optional, version-pinned, crash-tested backend.

## Implementation order

Stable identity/volume contract -> native logical volumes -> external Ceph RBD adapter -> DRBD nearline baseline -> witness/fencing and full VMM/storage handoff -> aggressive failure campaign -> consider standalone Mayastor/clean io_uring/SPDK only with measured justification -> separately designed managed OSD infrastructure.

A benchmark is not a durability proof. A happy-path migration is not a split-brain proof. No class is called production-supported until its exact implementation passes the failure and evidence requirements in SPEC-0002 and the relevant v2 contract.
