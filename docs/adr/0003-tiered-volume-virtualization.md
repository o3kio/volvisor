# ADR-0003 — Volume virtualization, local-native, nearline, and Ceph tiers

Status: Proposed (v2 direction; supersedes the class taxonomy in ADR-0001)
Decision-accepted: pending (record acceptance date and accepting authority here)
Date: 2026-10-09
Supersedes: ADR-0001 for volume semantics and storage-class names
Related: [ADR-0004](0004-nearline-replication-and-mobility.md), [SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md), [Volume contract v2](../../contracts/volume-api-v2.md)

## Decision summary

Volvisor is a **storage virtualization and orchestration layer**, not a fourth distributed storage engine. O3K/CellHV asks for a block volume with explicit locality, protection and mobility guarantees; Volvisor selects and operates a supported underlying provider. All three modes share stable volume identity, provisioning, attachment, health and lifecycle contracts, but **not** identical durability or migration promises.

Normative **volume classes** for the v2 proposal:

1. `native-local`: logical block volumes carved from disks physically attached to the workload's host. No Volvisor network replication; optional independent *local disk* protection.
2. `nearline-replicated`: host-local serving replica backed by one or more ordered replicas on different hosts, normally replicated asynchronously. Local device protection is a separate policy. Planned migration is coordinated with target convergence and a single-writer transfer.
3. `ceph-rbd`: RBD images backed by an existing or Volvisor-managed Ceph cluster; OSDs consume enrolled physical disks. Ceph itself provides distributed durability.

The word *nearline* is a Volvisor product term meaning **near the compute host**, not a claim about NL-SAS disks or slower storage. Public APIs use `nearline-replicated` and document the actual replication mode. Do not infer performance from the name.

Old proposed terms map as follows:

| Earlier proposal | v2 meaning |
|---|---|
| `local-direct` | **not** a synonym for `native-local`; its whole-PCI VFIO behavior becomes the optional `pci-passthrough` attachment profile |
| `replicated-async` | `nearline-replicated` with `replication_mode=async` |
| `cluster-durable` | `ceph-rbd` |

Existing v1 contracts are retained as historical proposals, **not silently reinterpreted**. There is no deployed API compatibility claim. Breaking implementation changes require explicit schema negotiation/migration if v1 ever ships.

## The three different layers

```text
                  O3K / CellHV: VM + Volume + Attachment
                                    |
                          Volvisor volume API
               provision / place / attach / protect / migrate
                         /          |           \
                        /           |            \
               native-local   nearline-replicated    ceph-rbd
                 |                   |                 |
              host LVM /        local Storage Cell    Ceph RBD frontend
              block layer      + peer cell(s)         |
                 |                   |            Ceph RADOS cluster
            local NVMe(s)       local NVMe(s)       OSDs on enrolled disks
```

**Physical device ownership** is not **logical volume ownership** is not **VM attachment**. Their identity, state, fencing and cleanup must be separate. One claimed native-local disk may contain many isolated logical volumes. One tenant volume may have several nearline replicas, but exactly one writable authority.

The frontend should prefer ordinary Cloud Hypervisor virtio-blk with a stable host device/file descriptor for v0. The backend may be a local logical volume, private host-to-cell block export, or Ceph RBD mapping. The guest does not need to know backend-specific topology. Neither Volvisor nor the Storage Cell is on the native-local foreground fast path in v2; the Ceph foreground path is the host RBD mapping into the cluster.

## 1. native-local

- Allocate a logical `VolumeId` from a specifically claimed local pool; a pool may cover one device or an explicitly configured local mirror/stripe set.
- v0 reference: Linux LVM thin volumes or regular LVs on the host, plus stable `/dev/volvisor/<id>` by-id mappings. LVM is a **candidate to validate**, not a claim of existing implementation. Raw partition and file-backed experiments may be supported separately; do not rely on ephemeral `/dev/nvmeXnY` names.
- Host agent does privileged provisioning/attachment and ownership checks; steady-state foreground reads/writes traverse the host kernel block stack, not a Volvisor proxy or mandatory Storage Cell.
- Multiple independent VM volumes may share a parent disk/pool with tenant isolation, capacity reservation and no unsafe overcommit. A volume has single-writer attachment semantics by default.
- Writes acknowledged by the block frontend obey its cache/flush policy and local durable-media contract. Loss of the host makes the volume unavailable. Loss of a single disk may destroy the volume unless local protection exists.
- **No live migration** of an attached native-local disk in v0. An explicit offline block-copy/restore/recreate workflow is a distinct feature. Do not represent migration of VM memory as movement of its local storage.
- `pci-passthrough` is a separately requested *attachment profile*: whole isolatable PCI function, VFIO, exclusive use, no logical subdivision or transparent migration. Never call passthrough a routine logical-volume allocation.
- The Storage Cell is **not permitted** on the native-local foreground I/O path in v2. This is an explicit decision, not an omission: it restores the v1 `local-direct` prohibition at logical-volume granularity. Inserting a cell (for example for a cell-managed local pool) requires a separately accepted ADR.

## 2. nearline-replicated

- One active local replica serves the current VM host from a host-local, authenticated endpoint, ideally an isolated host-to-Storage-Cell link. A source and target have different physical devices. Host-local endpoint identity remains stable through attachment changes; it is not a shared concurrent-writer endpoint.
- Peer copies are on separate **host failure domains**; a second SSD in the same host cannot count as host-failure protection.
- Default `replication_mode=async`: foreground acknowledgement after the source's documented local durable commit and durable replication ordering state, **not** peer durability. Thus unplanned host failure has possible nonzero RPO.
- Distinguish local device protection (`none`, `mirror`, or a proven provider layout) from inter-host replication (`remote_replicas`, placement, mode, lag). A local mirror is not a second host replica; a remote replica is not proof of uninterrupted service on local device failure.
- A local SSD failure on a healthy mirrored host should continue locally if the mirror is actually healthy; failure without a local mirror must trigger defined failover/recovery and possible-loss classification. Both are testable, not automatic guarantees.
- During planned VM migration, rate-control memory pre-copy, background rebuild and foreground interference; **do not simply throttle replication until it falls behind**. Prioritize target convergence. The final, paused handoff requires source flush + barrier, target durable proof, source fencing and new target authority before resume.
- Automatic unplanned promotion is a separate policy and requires independent fencing/quorum/witness evidence. Two replicas without an arbiter do **not** establish safe split-brain resolution. See ADR-0004.

## 3. ceph-rbd

- **OSDs are physical-disk-consuming storage daemons; tenant volumes are RBD images, not OSDs.** The Ceph provider maps a Volvisor `VolumeId` to an RBD image, pool, namespace and cluster identity; it does not make a new OSD for each VM volume.
- Support attaching to an **existing Ceph cluster** first. Optional Cell-managed OSDs are a separate infrastructure deployment mode requiring cluster bootstrap, MON/MGR quorum, CRUSH failure-domain layout, BlueStore device ownership and upgrade/failure procedures.
- No extra Volvisor RAID/mirroring below OSDs by default. Ceph replicates/ECs according to pool, `size/min_size`, CRUSH and health; never advertise a blanket RPO=0 or availability guarantee when policy/health does not justify it.
- An RBD client or host mapping is needed between Cloud Hypervisor and Ceph; confirm whether the chosen Cloud Hypervisor frontend supports the mapping and migration workflow. A Storage Cell is **not** inherently required for RBD clients or every OSD deployment.
- Shared-backend VM migration is possible only with compatible frontends, lock/attach handoff, credential management, and validated Cloud Hypervisor migration behavior.
- Do not confuse Ceph RBD *image migration* tooling with live migration of an O3K VM.

## Orthogonal policies, not a single speed slider

A volume request must specify class plus independently modeled:
`size_bytes`, `logical_block_size`, `provisioning`, `placement`,
`local_protection`, `replication`, `durability/flush`, `migration_policy`,
`encryption` and `failure_domain`. Unsupported combinations fail validation.

| Guarantee | native-local | nearline-replicated (async) | ceph-rbd |
|---|---|---|---|
| Virtual volume from disk | yes | yes | RBD image from pool |
| Foreground primarily host-local | yes | yes | Ceph cluster path |
| Local disk mirror option | independent option | independent option | Ceph controls placement; no double mirror by default |
| Remote host replica | no | yes | Ceph policy |
| Host-loss data guarantee | none | possible unreplicated ACK tail | conditional on Ceph policy/health |
| Planned live migration v0 | no | gated by full barrier and VMM integration | gated by shared-RBD/VMM integration |
| Guest multi-writer | no | no | only with explicit backend/guest protocol |

No silent policy downgrade. The same VM may attach volumes from different classes, and the **least migratable attached volume constrains VM live-migration eligibility**.

## Deployment and fault-domain design

Example host with eight NVMe devices: four available to native pools, two allocated to nearline pools, two contributed as Ceph OSD devices. This is *illustrative*, not a fixed ratio, topology, redundancy guarantee or instruction to create eight equal logical volumes. Devices may be aggregated into local mirrors only by explicit policy. Ceph OSD placement depends on full cluster topology and quorum; two Ceph drives on one node alone do not make a durable Ceph cluster.

Host agent remains small and privileged (inventory, claim, lifecycle, host native volume provisioning, frontend mappings, fencing). The optional per-host Storage Cell VM operates nearline data engines and may operate approved Ceph OSDs; no Storage Cell VM is on the native-local foreground path, and placing one there requires a separately accepted design. The consumer control plane owns tenant resource truth, but established data-path IO should survive its outage when runtime authority and backend quorum remain valid.

## Open decisions and evidence

- Choose reference logical-volume engine (LVM-thin vs thick LVM; capacity and crash behavior).
- Validate Cloud Hypervisor device frontend, backing block-device FD, disk hotplug and migration compatibility **against a pinned release**.
- Prototype DRBD 9 as first nearline replication provider, with selectable Protocol A/B/C for explicit async/semi-sync/sync policies. Keep other engines as alternatives and qualify Cloud Hypervisor write-fd handoff before claiming planned live migration. See [ADR-0007](0007-drbd9-nearline-replication-provider.md).
- Decide witness/quorum placement and exactly how stale writers are physically fenced.
- Ceph existing-cluster adapter before any managed-OSD lifecycle claim.
- Test failure modes independently: physical SSD loss, mirror leg loss, primary/peer host loss, cell crash, stale primary after partition, storage-network loss, control-plane loss, interrupted copy and migration failure.

**Status: proposal only.** Neither migration safety, synchronous local durability nor any performance claim has been demonstrated by these documents.
