# ADR-0007 — DRBD 9 as nearline reference replication backend

Status: Proposed / prototype selection, NOT production acceptance
Decision-accepted: pending (record acceptance date and accepting authority here)
Date: 2026-10-09
Related: [ADR-0003](0003-tiered-volume-virtualization.md), [ADR-0004](0004-nearline-replication-and-mobility.md), [SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md), [nearline v2 contract](../../contracts/nearline-replication-v2.md), [R&D comparison](../research/replicated-async-rnd.md)

## Decision

**Select DRBD 9 as the first real replication engine to prototype for `nearline-replicated`.** Retain a versioned, provider-neutral Volvisor `ReplicationProvider` boundary. DRBD can operate over ordinary LVM logical volumes and exposes a Linux replicated block device. This provides existing asynchronous/synchronous protocols, resync/dirty bitmaps, multi-peer replication, role control, and quorum. It does not provide a complete O3K/CellHV VM-migration transaction or magically make a disk on host A accessible for concurrent writes on B.

DRBD 9 **is the initial implementation candidate**, not a mandatory dependency for all `native-local` or `ceph-rbd` volumes. Initial `nearline-replicated` operation is `replication_mode=async` (DRBD Protocol A), with an **explicit optional** synchronous protection profile (DRBD Protocol C) and a separately evaluated Protocol B (semi-synchronous). The effective contract and cost of each profile must be visible. Do not silently change the class's RPO or write acknowledgement policy.

This does not reverse the decision to keep optional per-host Storage Cells. We must choose where DRBD lives and benchmark both options rather than pretending they have the same data path.

## Proposed engine boundary

```text
Volvisor VolumeId / VM attachment / durable plan
              |
ReplicationProvider { Create, Inspect, Seed, Attach, Pause,
                      Promote, Demote, ConfigurePolicy,
                      TrackSync, BeginHandoff, Recover, Delete }
              |
    +---------+-----------+--------------+
    |                     |              |
 drbd9-direct      drbd9-cell          future engines
 (host kernel)     (cell kernel)       (not selected)
    |                     |
 /dev/drbdN             /dev/drbdN
    |                     |
 host LV             cell-owned LV
```

Implement `DRBDProvider` as a controlled adapter for DRBD user-space admin/status commands and events, with durable Volvisor generation/ownership metadata. **Do not write DRBD's replication or quorum mechanisms anew.** No backend API may bypass scoped destructive authorization, foreign-device rejection or strict single-writer ownership.

DRBD may use multiple peers, but never confuse the number of configured DRBD nodes with number of durable data copies (diskless quorum voters do not store customer data). Do not hardcode a maximum host count from a per-resource peer limit.

## Deployment variant A — host-kernel DRBD (reference prototype)

```text
host A                                             host B
VM -> CHV virtio-blk                                no guest writer
          |
     /dev/drbd100 (Primary) --- DRBD protocol A --> /dev/drbd100 (Secondary)
          |                                              |
     local LV on NVMe A                             local LV on NVMe B
```

Advantages: true host-local ordinary kernel block path, Cloud Hypervisor host block frontend, no extra QSD/SPDK/NVMe-oF hop; natural match for LV capacity, read cache and guest flush/FUA tests; DRBD owns dirty bitmap, resync and network replication.

Costs: extra out-of-tree DRBD 9 module and kernel-version support on the hypervisor, privileged ownership/role management, kernel crash/failure blast radius. The older in-tree DRBD implementation alone should not be assumed to provide every DRBD 9 feature. Pin module, kernel and userspace versions. This variant does **not** require the Storage Cell VM for foreground replicated I/O. It is compatible with the v2 principle that the control plane is unified but not every backend's data path must be.

## Deployment variant B — DRBD inside the per-host Storage Cell

```text
VM -> CHV -> private local NVMe-oF/TCP or other qualified frontend
          -> Storage Cell DRBD Primary -> cell-owned LV/NVMe
                                  |
                              DRBD peer link
                                  |
                         other Storage Cell DRBD Secondary
```

Advantages: DRBD kernel/module/restarts isolated from the hypervisor base OS, independent cell image upgrade and backend lifecycle.
Costs: requires a secure host-to-cell block export, additional I/O latency/CPU and extra failure point; CHV volume-mapping and migration must preserve endpoint identity. This variant is acceptable **only after** qualified private frontend tests.

The v2 spec's default depiction of a nearline Storage Cell is **one deployment candidate**, not proof that a DRBD host-kernel prototype violates the volume contract. Both variants must preserve ownership and established-I/O guarantees. Never let host and cell claim the same LV or DRBD device simultaneously.

## DRBD replication profiles

The canonical v2 `replication.mode` field is `async | semi-sync | sync`; `async-local` and `sync-durable` below are descriptive profile labels, **not additional wire enum values**. `replication.engine=drbd9` selects this adapter.

| Public profile | DRBD protocol | Guest write ACK semantics | Typical role |
|---|---|---|---|
| `async` (async-local) | A | source local completion + replication packet in TCP send buffer; peer disk not awaited | low-latency nearline, possible RPO tail |
| `semi-sync` | B | source local completion + peer memory arrival; remote durable media not awaited | optional experiment; not RPO=0 |
| `sync` (sync-durable) | C | source plus remote disk completion | stronger host-failure protection, adds network + peer-media latency |

Qualification must check cache mode, write barriers, flush/FUA, local mirror semantics and error admission. `sync-durable` is *not* magic zero-loss for simultaneous failures, lost quorum or unsafe write caches; its advertised guarantee is conditional on both durable disks, correct DRBD state and surviving acknowledged copy.

An **operator can request a profile** per volume at creation. Runtime switching A/B/C requires explicit operation, quorum/health checks, correct in-flight write-drain and protocol reconfiguration tests. Do not promise no-downtime profile changes until validated. For `async-local`, failures during network disconnect may have missing acknowledged writes at surviving remote peer.

**Volvisor and DRBD must own different responsibilities.** DRBD manages block replication, durable disk/peer state and quorum semantics; Volvisor owns tenant resource identity, placement, policy, O3K VM attachment, independent local mirror devices, and all-VMM-disk cutover transaction.

## Local SSD failure protection is separate

```text
/dev/drbd100
      |
  LVM-backed extent / logical volume
      |
 optional md/DM mirror
    /       \
 NVMe-1   NVMe-2
```

A local RAID1/LVM mirror under a DRBD backing LV can survive a single local SSD failure **if** the selected layout and recovery tooling have passed qualification. Without local protection, a disk failure may invalidate the active replica; DRBD can still provide remote peer data, but any promotion requires writer fencing and data-loss classification. A mirror leg is never a host replica. A remote peer is never automatic protection of the last Protocol A ACK.

## DRBD quorum and witness

Use DRBD quorum/fencing as first-line protection against split brain; a third diskless node can serve as an independent witness in a two-data-node topology. Quorum is not equivalent to remote durable replication. In Protocol A, quorum may allow an active source to keep writing while its data peer is absent under an explicit degraded policy, so an unexpected source loss can lose those writes. Define policy for no-quorum I/O (`suspend-io` versus `io-error`), quorum-minimum-redundancy, active writer admission, and manual possible-loss promotion.

Auto-promotion is a DRBD capability, not a permission for uninstructed VM launch. Avoid competing `drbd-reactor`, LINSTOR auto-controller and Volvisor schedulers all trying to move one VM at once. Reconcile resource state against Volvisor attachment generation before permitting a DRBD primary to be opened by a VMM.

## Planned Cloud Hypervisor live migration is a separate hard gate

A key restriction: DRBD single-primary cannot ordinarily be demoted while a VMM has the DRBD device open. DRBD virtualization live-migration guidance (with QEMU/KVM) often enables **temporary dual-primary** (Protocol C with fencing) so both endpoints can open the shared block device during handoff. Dual-primary is not equivalent to safe multi-writer data semantics; it is **prohibited by default** by Volvisor's current v2 contract.

First evaluate a strict single-primary handoff with a VMM API/integration that can:
1. Prepare B's local replica and VM memory state without opening B's disk writable.
2. Pause/drain source VM block I/O, complete all pending source ACKs and prove B is `UpToDate` through the paused durable source boundary (connection and disk states alone are not a substitute for a proof of the final in-flight write/flush boundary).
3. Close/relinquish source's DRBD writer reference so `drbdadm secondary` can succeed **without killing accepted writes**.
4. Fence old source authority and persist cutover decision; promote B only after the old writer is truly prevented from admitting writes.
5. Open target DRBD primary through the selected frontend, resume the VM on B, and reconcile the old source as Secondary.

**If Cloud Hypervisor cannot safely do steps 1–4, DRBD-based nearline live VM migration remains unsupported.** We may separately research a tightly bounded temporary dual-primary/Protocol C handoff with independent fencing and only one guest write executor, but that needs a new accepted contract and adversarial evidence; do not turn it on by default. The general DRBD concept of dual-primary must not silently weaken our single-writer requirement.

A change from DRBD A to C by itself is **not** the migration proof. Replication convergence and precise final cutover still require data and authority barriers. Also plan consistent all-disk cutover for multi-volume VMs.

## Alternatives

| Candidate | Advantages | Mismatch/risks | Volvisor role |
|---|---|---|---|
| DRBD 9 directly | mature Protocol A/B/C, local block device, bitmap resync, quorum, multi-peer | kernel driver lifecycle, CHV migration handoff | **first implementation candidate** |
| LINSTOR-managed DRBD | automates LVM/ZFS allocations, DRBD resource placement/lifecycle and APIs | separate controllers/satellites; overlap with Volvisor scheduler/control plane | **fallback management layer**, not a different replication engine |
| DRBD Reactor | lightweight DRBD-based HA service promotion/monitoring | uncoordinated VM placement can conflict with O3K | optional coordinated integration |
| Mayastor io-engine | SPDK/NVMe-oF, data-plane component reuse | existing replicated Nexus ACKs synchronous; Kubernetes-centric lifecycle | second R&D candidate; sync tier alternative |
| Ceph RBD / RBD mirroring | proven Ceph data engine and cross-cluster async DR | clustered RBD latency/path, mirroring requires Ceph clusters; not host-local LV replication | separate Ceph backend / DR, not nearline's DRBD replacement |
| ZFS or btrfs snapshot send | periodic incremental copy | not an ordered live block mirror, does not prove lossless planned guest cutover | backup/DR only |
| Clean io_uring / SPDK engine | exact Volvisor-specific protocol potential | owns redo log, ordering, crash replay, fencing, migration safety | NO-GO unless DRBD/Mayastor measured gaps demand it |

LINSTOR is a DRBD resource-management layer, not a DRBD-free alternative. Prefer bare DRBD + Volvisor-owned provisioning as first proof because Volvisor already owns placements, but evaluate LINSTOR if automation, multi-thousand-volume scale, update safety or operational support justify using it.

## Prototype and go/no-go evidence

R0: On two real hosts with one dedicated LV each, create one DRBD 9 resource, use Protocol A, expose a mapped /dev/drbd device to CHV guest, run fio with flush/FUA and monotonic-checksum verification. Record exact kernel/module/DRBD/CHV source versions.

R1: Add independent third diskless quorum witness. Induce network partitions, lost witness, stale old primary, peer loss and return. Capture primary election/fencing, guest I/O outcome, source acknowledged writes, peer state and loss classification.

R2: Compare Protocol A, B, C under 4K and 128K random/sequential, p50/p99/p999, CPU/core, network egress, backlog and resync under guest load. Match backing NVMe and flush settings.

R3: Introduce qualified local mirror below one replica. Inject a single SSD failure, simultaneous peer loss and rebuild contention. Ensure media failure does not invent DRBD authority.

R4: Test exact Cloud Hypervisor source-device close/DRBD demote, target promote and resume sequencing, with failure injection at every boundary; **no published live-migration support** before proving this. Test multiple attached writable disks together.

R5: Compare a cell-VM implementation through a private NVMe-oF frontend against host-kernel DRBD, then weigh isolation vs latency/reliability.

R6: Benchmark LINSTOR-managed DRBD deployment/resource lifecycle and Mayastor standalone io-engine before committing to a proprietary nearline replication engine.

**No-go** for production if primary fencing is ambiguous, stale writers can write, target can be falsely marked synchronized, ACK semantics are untruthful, or VMM closure/demotion cannot be coordinated. DRBD maturity does not replace O3K VM-storage integration evidence.

## References

- DRBD 9 guide: https://linbit.com/drbd-user-guide/drbd-guide-9_0-en/
- DRBD kernel module source/license: https://github.com/LINBIT/drbd
- DRBD Reactor: https://github.com/LINBIT/drbd-reactor
- DRBD utils configuration: https://github.com/LINBIT/drbd-utils
- LINSTOR: https://linbit.com/downloads/
- OpenEBS Mayastor I/O path: https://openebs.io/docs/main/user-guides/replicated-storage-user-guide/replicated-pv-mayastor/additional-information/io-path-description
- Ceph RBD mirroring: https://docs.ceph.com/en/latest/rbd/rbd-mirroring/
- DRBD `secondary` when device open: https://manpages.debian.org/trixie/drbd-utils/drbdsetup-9.0.8.en.html
