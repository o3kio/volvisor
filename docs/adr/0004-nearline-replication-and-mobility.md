# ADR-0004 — Nearline replication, local protection, and migration authority

Status: Proposed
Decision-accepted: pending (record acceptance date and accepting authority here)
Date: 2026-10-09
Revises: ADR-0002 (retains the local-endpoint lesson; replaces handoff/failure details where inconsistent)
Related: [ADR-0003](0003-tiered-volume-virtualization.md), [Nearline contract v2](../../contracts/nearline-replication-v2.md)

## Context

Nearline is **not** merely a disk synchronization service. It is a single-writer block volume with host-local presentation, independent optional local media protection, ordered host-to-host replication, and a provable migration handoff.

The previous Xen/NFS/Gluster design taught that each compute host can open a stable local endpoint. That historical success is **not** evidence of durable ACKs, split-brain fencing, zero-RPO failover, or modern Cloud Hypervisor compatibility. GlusterFS is not selected.

## Decision 1 — local device failure and remote host failure are separate

Topology example, not a requirement:

```text
host A: VM -> host-local frontend -> Storage Cell A -> local mirror A1/A2
                                                       |
                                               ordered async stream
                                                       v
host B: prepared local frontend -> Storage Cell B -> local mirror B1/B2
```

Protection axes:
- `local_protection=none|mirror` (additional layouts require separate qualification): handles a media failure **within a host**, with health and rebuild state.
- `remote_replicas` and `replication_mode=async`: handles placement on other host failure domains, with lag and explicit failover policy.
- Neither axis makes the other redundant. A host-local mirror cannot survive host loss. An async remote copy can lack the newest acknowledged writes.
- Mirror leg loss/repair is **not** a writer election. No cross-host promotion merely because a local disk failed if the surviving local mirror remains authoritative.
- Mirror implementation must define write atomicity, flush/FUA ordering, failure behavior and degraded admission. RAID0/striping is not local protection. Ceph OSD media should follow Ceph's own redundancy mechanism, not this local-mirror policy by default.

## Decision 2 — journal-before-ACK and exact progress proof

Write path must provide a durable, replayable ordering between payload, changed-range/operation metadata, and sequence commit. A sequence is not a durability proof by itself.

At ACK:
1. the active writer holds current fenced authority;
2. guest-visible write data plus recovery metadata required for accurate resync are locally committed according to the advertised cache/flush/FUA contract;
3. the committed ordering boundary is crash-recoverable;
4. the peer send queue may lag without blocking the steady-state ACK.

The implementation may batch/group-commit only if it preserves guest write ordering, FUA/barrier semantics, and accurate durable-prefix reporting. An ACK means **local** durability, never peer durability.

All operations that affect bytes or visibility (`write`, `zero`, `discard`, `resize`, snapshot boundaries and flush) need ordered replay or must be rejected until safely implemented. Reconnect must not race old dirty extents against newer writes; snapshots, epoch-tagged dirty bitmaps, or an equivalent proof are required for incremental recovery.

Each replica reports at least `received`, `applied`, and `durable` progress with a lineage/epoch. `durable_seq >= B` is valid only when it describes a **contiguous, authenticated, exact-source-epoch prefix including all operations through B**. Reject accidental comparisons across writer epochs.

## Decision 3 — planned migration couples VMM and storage transactions

A safe, testable attempt has a unique `migration_id`, source and target VM identity, volume generation, fencing epoch and monotonic state.

1. **PREPARE**: verify all VM disks and frontend devices are migration-compatible; reserve target compute resources; provision destination local replica and authenticated host-local endpoint in **read-only/nonwriter** mode; verify capacity and failure-domain placement.
2. **PRECOPY**: start Cloud Hypervisor memory migration/pre-copy while the source remains sole writer; stream delta to target. Rate-limit background rebuild and memory pre-copy, not blindly the only stream needed for catch-up.
3. **CONVERGE**: watch dirty-rate versus replication throughput. Raise replication priority or apply controlled source I/O throttling where policy permits. Abort early if target cannot converge within disk/network/VM downtime budgets.
4. **QUIESCE**: coordinate a VMM pause that actually stops guest writes and drains in-flight virtio queues. Filesystem/application quiesce is an **optional, stronger** consistency mode. A VM pause alone is not an application-consistent backup.
5. **BARRIER**: flush source block path, fix durable source boundary B, prove destination has the exact durable prefix through B. Freeze source admission for the handoff.
6. **FENCE/COMMIT**: durably retire source writer epoch; enforce revocation at the source data path and authority service; commit a newer destination writer epoch **only if old epoch cannot admit writes**; promote/open target endpoint.
7. **RESUME**: complete VMM migration, verify destination I/O and attachment identity, then demote/rebuild old source. Publish success only after data and VMM completion are both proven.

**Never assert that 'throttling synchronization' achieves a correct migration.** The crucial effect is **catch-up plus a final durable barrier**. Throttling secondary/background traffic may protect guest latency, but insufficient catch-up means no migration.

The phase names above are descriptive. The canonical migration state vocabulary is defined in [nearline contract v2 section 6](../../contracts/nearline-replication-v2.md): `PREPARED`, `PRECOPY`, `QUIESCED`, `BARRIER_DURABLE`, `SOURCE_REVOKED`, `DESTINATION_AUTHORIZED`, `VM_RESUMED`, `COMPLETE`. `CONVERGE` is a rate-control policy inside `PRECOPY`, and `FENCE/COMMIT` is the `SOURCE_REVOKED -> DESTINATION_AUTHORIZED` pair with `IN_DOUBT` possible between them.

## Decision 4 — failure states are not a single abort rule

- **Before durable writer-transfer commit**: cancel target reservation; source may resume only after proving source authority is still valid; discard stale attempt identifiers.
- **After source revocation but before destination promotion**: state is `IN_DOUBT`; source **must not** simply resume. Recover using durable authority log and winner-proof, or remain unavailable for operator intervention.
- **After destination authority commit**: never roll back to the original writer generation. If destination VMM fails, recover/promote using a new fenced epoch; do not attempt unsafe source reactivation.
- **Network partition mid-handoff**: freeze rather than allow dual writer; lease expiry alone is insufficient unless enforced at every write admission point and renewal uses independent quorum.
- **Crash at any boundary**: durable operation identity and state allow idempotent resume/reconcile without inventing ownership.
- A migration cancelled by the VMM must not leave a writable target. A storage handoff committed by Volvisor must not be reported as an ordinary pre-handoff VMM abort.

Migration is a joint VM/storage state machine. A VMM that cannot externally coordinate the critical pause/commit sequencing **does not qualify** for Volvisor nearline live migration.

## Decision 5 — unplanned failover has independent safety requirements

An asynchronous source may ACK writes that never reached the peer. Even if the old source can be fenced, the surviving copy may lose an acknowledged tail. Promotion needs *both*:
- **single-writer proof** (old source cannot write), usually third-party fencing/STONITH or a quorum/witness-backed lease enforcement path; and
- **data-loss classification** (candidate's durable prefix compared with the best known acknowledged boundary).

Classify:
- `SAFE_CURRENT`: full authoritative tail provably durable at destination **and** old writer fenced;
- `POSSIBLE_LOSS`: old writer fenced but acknowledged tail cannot be proved present; explicit policy/operator authorization required, with recorded exposed boundary;
- `UNSAFE`: old writer fencing, epoch lineage, or destination integrity unproven; reject.

A pair of hosts with two data copies and no independent arbiter cannot safely infer which side is primary during partition. A third witness/quorum voter or external fencing authority is a design prerequisite for **automatic** primary failover. The witness need not store user payloads. The actual voting/lease technology is undecided; prove behavior before promising availability.

Loss of consumer O3K control plane alone should not terminate established I/O, but losing necessary *runtime* write-lease quorum may require self-fencing. Availability cannot override single-writer safety.

## Decision 6 — data/locality and scale limits

A destination may require initial seeding of terabytes. `host-local` means local **serving** after safe promotion, not zero cost to build the replica. Limit replications, memory pre-copy and local mirror rebuild with separate resource budgets. Collect replication egress, ingress, dirty rate, projected catch-up time, local mirror rebuild pressure, write latency p99/p999 and evidence of backpressure.

With multiple nearline replicas, choose explicitly which are part of the migration durability barrier. Target proof is mandatory; other replicas may remain behind under documented policy. Degraded creation/attachment without enough failure domains is possible only under explicit opt-in.

## Evidence gates

Before calling this tier supported, run real-host fault injection:
- disk failure with local mirror (and without it);
- WAL/dirty-map crash at each ACK boundary, power-loss replay and flush/FUA ordering;
- network partitions, quorum loss and stale primary returning after promotion;
- peer corruption, stale epoch replay and idempotent incremental resync;
- large initial seed, high dirty write rate, rebuild versus replication contention;
- failed VMM pre-copy; quiesce timeout; barrier timeout; source loss before/after transfer; destination death after transfer;
- combined source/destination/authority crash at every commit boundary;
- multi-disk VM: atomic eligibility and consistent cut across **all** attached writable volumes;
- correctness oracle verifies guest acknowledged writes and the advertised RPO.

No synthetic benchmark substitutes for these gates. Select DRBD 9 as initial behavior baseline, explore standalone Mayastor reuse, and consider new io_uring/SPDK machinery only if the reusable solutions cannot meet the validated contract.
