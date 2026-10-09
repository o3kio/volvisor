# Volvisor architecture review — 2026-10-09

Status: Design review and v2 proposal; no implementation or real-host evidence
Provenance: self-review recorded by the PR author before independent review. The verdict below is a recommendation to maintainers, not an independent approval; findings F01-F15 should be re-verified by a reviewer who did not author the v2 documents.
Scope: covers the v2 core (ADR-0003/0004/0005, SPEC-0002, volume-api-v2, nearline-replication-v2) as of 2026-10-09. ADR-0006 (online resize/relocation), ADR-0007 (DRBD 9 provider) and ADR-0008 (Rook cells) were added after this review and are NOT covered by findings F01-F15.
Reviewed repository: `o3kio/volvisor`, `main` at `d8e25b7782571f89382d2adc12ed0010e68d6fe4`
Reviewed files: README, AGENTS, ADR-0001/0002, SPEC-0001, three v1 contracts, engineering design, replicated-async R&D, previous review
Revisions: ADR-0003/0004/0005, SPEC-0002, v2 contracts
Verdict: **GO for controlled v2 prototypes; NO-GO for production claims or generic live-migration promises.**

## What is good in v1

- Correct emphasis on physical device identity, journal-before-mutate, exclusive ownership, foreign-state rejection and explicit destructive authorization.
- Asynchronous steady-state ACK is not marketed as zero RPO; planned handoff requires target durability and fencing.
- Useful historical local-endpoint idea from Xen + localhost NFS + GlusterFS is retained without selecting GlusterFS.
- Existing Ceph is recognized as a data-engine dependency rather than something to reimplement.
- DRBD baseline, Mayastor reuse, simple io_uring state-machine prototype before custom SPDK are sensible research gates.
- A disk-count example is not elevated to architecture.

## Findings and required corrections

| ID | Severity | Finding | Resolution |
|---|---|---|---|
| F01 | Critical product mismatch | `local-direct` only means whole PCI VFIO; user needs **logical volumes** created from native disks | new `native-local` class; VFIO separate `pci-passthrough` profile |
| F02 | Critical | local SSD failure protection is conflated with inter-host replication | independent `local_protection` and `remote_replicas` dimensions |
| F03 | Critical | a migration 'abort' after source fencing could incorrectly re-enable the old writer | `IN_DOUBT` and commit-aware roll-forward; never blind rollback |
| F04 | Critical | 2-node partition may permit writer ambiguity without independent quorum/STONITH | runtime authority/witness/fencing requirement; no automatic unsafe failover |
| F05 | High | target `durable_seq >= B` alone doesn't prove identical data / contiguous ordered prefix | require lineage, epoch, no holes, flushed payload and metadata |
| F06 | High | VM memory migration can race writes and block handoff on multiple attached disks | VMM drain/pause integration and atomic multi-volume write cut |
| F07 | High | 'throttle the sync during migration' can worsen convergence | independent bandwidth control, dirty-rate model, prioritize catch-up, final barrier |
| F08 | High | OSD integration ambiguous: Ceph OSD disks versus tenant RBD images | separate external RBD adapter and optional managed OSD control |
| F09 | High | all storage inside one Storage Cell creates failure amplifier and unnecessary local I/O hop | native host path bypasses cell; optional Ceph deployment separation |
| F10 | High | thin provisioning, ENOSPC, guest flush/FUA, TRIM and tenant sanitization underspecified | v2 pool/volume contract and explicit failure gates |
| F11 | High | changed public semantics would silently conflict with normative v1 contracts | v2 ADR, SPEC and contracts; historical v1 kept intact |
| F12 | Medium | Ceph is called 'durable' without explicit dependency on policy and health | surface actual pool/CRUSH/size/min_size health and degraded states |
| F13 | Medium | direct VFIO devices globally described as non-migratable | default reject ordinary NVMe; conditional exception only for proven migratable variant hardware/driver/VMM |
| F14 | Medium | source crash during peer copy, dirty bitmap rollover and in-place resync races lack detailed proof | journal/epoch/COW consistency and crash-injection contract |
| F15 | Medium | no explicit portability or tier-changing operation contract | capability-based provider interface; cross-class online conversion is a non-goal for v0 |

## Architecture judgement

Three **volume backends** plus optional physical passthrough is the right product abstraction. A single synthetic block datapath for every mode is the wrong goal: it would add overhead to native NVMe and conflate Ceph with local replication. Unify the **control/lifecycle interface** and evidence, not all data paths.

`native-local` is a real virtualization layer when it creates multiple independent logical volumes on an enrolled disk/pool. A whole-controller VFIO lease is valuable for specialist GPU/AI hosts but is neither a virtual volume nor sufficiently granular for general purpose O3K VM disks.

A self-built replication engine is the largest potential distraction. DRBD already offers Protocol A and bitmap-based resync. It is the initial correctness/performance baseline, but actual kernel integration, license/operations and role-switch semantics require experimentation. Mayastor gives an SPDK/NVMe-oF backend reference but its synchronous Nexus behavior is a mismatch to the requested async foreground ACK. Do not fork it to create a new distributed correctness responsibility without decisive test results.

Local mirror is orthogonal to replication. It avoids a needless host failover for a single SSD failure, at the cost of extra capacity and rebuild write pressure. It is an **option**, not the default for every native/nearline volume. Failure-domain and hardware-budget planning are essential.

Ceph should be consumed as RBD first. Reimplementing Ceph orchestration and placing several OSDs into one host appliance VM is an additional project with large operational consequences; it should not block the minimum usable Volvisor product.

## Alternatives considered

| Choice | Strength | Limitation | Recommendation |
|---|---|---|---|
| host LVM thick or thin | existing Linux block abstraction, low extra data path cost, multi-volume | thin metadata full and local failure semantics | v0 native baseline |
| whole NVMe VFIO | very low overhead / full guest driver control | coarse allocation, pinning, tenant sanitize | optional passthrough profile |
| local mirror below volume pool | survives a leg failure | rebuild pressure and capacity cost | optional qualified protection |
| DRBD 9 Protocol A | mature ordered async replication and resync | kernel lifecycle, quorum/role integration | required nearline baseline |
| Mayastor io-engine standalone | reuse SPDK/NVMe-oF/pools | sync replication semantics and operational dependencies | reuse feasibility experiment |
| new io_uring replicator | control of precise state machine | owns WAL, replay, split brain, data integrity | semantics prototype **only if needed** |
| custom SPDK bdev | eventual latency/CPU opportunity | maximal maintenance and proof burden | no-go until functional and performance evidence |
| Ceph RBD existing cluster | proven integration target with backend operational ownership | real-world Ceph policy and outage complexity | v0 Ceph adapter |
| managed Ceph OSDs inside Storage Cell | unified appliance packaging | correlated failure, bootstrap/quorum, upgrade blast radius | separate later ADR/phase |

## Product decisions recorded

- **v2 external volume classes**: `native-local`, `nearline-replicated`, `ceph-rbd`.
- **optional physical profile**: `pci-passthrough`, separate from logical-volume API.
- **protection dimensions**: independent local mirror and inter-host replication, not a single HA flag.
- **planned migration**: nearline only with exact durable target cut and safe writer transfer; Ceph with validated shared backend and VMM frontend; native pinned in v0.
- **failure reporting**: no blanket RPO/RTO; expose definite-current, possible-loss and unsafe/in-doubt.
- **control-plane scope**: O3K/CellHV owns tenant resources; Volvisor host agents/providers own hardware realization and runtime authority. Runtime lease quorum can be separate from tenant-facing control availability.
- **managed Ceph**: optional phase after existing-cluster RBD support.

## Prioritized implementation plan and stop conditions

**Gate 0 — contract and test harness.** Versioned volume API, stable disk inventory/claim, plan generation, exact idempotency, storage errors and separate resource ownership. Stop if a retry can allocate, attach or delete the wrong resource.

**Gate 1 — native-local.** Multiple LVs per claimed disk, plain virtio-blk frontend, single-writer, thin metadata fill/ENOSPC, crash replay, failed sanitation, optional local mirror and real hardware failure. Stop if local disk identity or zeroing cannot be trusted.

**Gate 2 — existing Ceph RBD.** Map/unmap RBD into Cloud Hypervisor, tenant isolation, backend health truth, source/target attach, VMM migration compatibility. Stop if stale mapping can permit simultaneous write.

**Gate 3 — nearline baseline.** Test DRBD Protocol A against desired local-durable ACK contract (especially cache/flush), mirror with and without peer, failed primary and delta resync. Stop if operational complexity outweighs value relative to RBD.

**Gate 4 — authority/VM cutover.** Witness/STONITH, lease self-fencing, coordinated all-disk VMM pause/barrier, in-doubt recovery, partition and source/target death campaign. No live migration claim until these pass.

**Gate 5 — new engine decision.** Compare DRBD, standalone Mayastor, semantics-first io_uring and Ceph with *matched* topology and crash oracle. Develop SPDK bdev only if existing choices demonstrably fail requirements and a custom path offers material measured benefit.

**Gate 6 — managed Ceph OSDs.** Separate accepted Ceph topology, upgrades, recovery, quorum, independent bootstrap path and fault-injection campaign.

## Concrete questions left unresolved (implementation gates, not questions blocking docs)

- Which v0 LVM provisioning profile, and whether `native-local` supports snapshots before v1 GA?
- What local mirror implementation should be qualified and what rebuild reserve per device?
- Where will the third writer-authority voter/witness live during O3K control-plane loss?
- Which pinned Cloud Hypervisor versions and storage frontend permutations truly migrate?
- Is direct NVMe-oF/TCP local link acceptable versus vhost-user for measured CPU/latency?
- Which Ceph cluster policy/CRUSH setup and credential delivery belong to the first supported external adapter?
- What level of source I/O throttling is acceptable before planned migration must abort?

These are **GO-for-research** topics. They must not be converted into product guarantees without evidence.

## External sources to re-check during implementation

- Cloud Hypervisor device model: https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/device_model.md
- Cloud Hypervisor live migration: https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/live_migration.md
- Cloud Hypervisor VFIO: https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/vfio.md
- LINBIT DRBD 9 user guide: https://linbit.com/drbd-user-guide/drbd-guide-9_0-en/
- Ceph RBD: https://docs.ceph.com/en/latest/rbd/
- Ceph OSD operations: https://docs.ceph.com/en/latest/rados/operations/add-or-rm-osds/
