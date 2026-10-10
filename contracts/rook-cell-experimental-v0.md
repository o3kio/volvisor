# Experimental Volvisor Rook Cell contract v0

Status: **PROPOSED, unsupported feature flag**
Date: 2026-10-09
Applies to: optional managed-Ceph `rook-cell-experimental` mode; not external-Ceph RBD adapter
Normative design: [ADR-0008](../docs/adr/0008-rook-only-hyperconverged-cells.md)
Verification: [POC](../docs/poc/rook-cells/README.md)

## Distinct resource ownership

`HostId -> CellId -> Kubernetes NodeUID -> PhysicalDeviceId -> Ceph OSD ID` is an audited binding. Physical HostId and placement failure domain must not be derived solely from potentially replaced Kubernetes node name; NodeUID may change after legitimate re-join but must reconcile against CellId/HostId. Cell guest Node name must be stable. Rook controls Ceph daemon lifecycle but **not** PCI device ownership, cell CPU, VM memory, destructive authorization or guest image lifecycle.

## Device lending and sharing

Sharing a volvisor-claimed device with a cell (Rook) is an **explicit lending operation**, never an implicit handover or a released claim — decided by [ADR-0008](../docs/adr/0008-rook-only-hyperconverged-cells.md) (its "Device lending and reclaim" section) and scoped to the POC. The operator surface's shape:

```text
LendDevice(device_id, borrower_cell_id, expected_claim_generation, operation_id)
ReclaimDevice(device_id, expected_lend_generation, operation_id,
              force?{ operator_authorization, residue_check })
DeviceOwnership(device_id)
  -> { claimed_by: volvisor, claim_generation,
       lent?: { borrower_cell_id, lend_generation, recorded_at, evidence_ref? } }
```

Invariants (each one is POC evidence, below):

- **Lend is journal-before-mutate** and idempotent by `operation_id` + exact request hash; replays return the recorded response, a differing body under the same id is a typed conflict. A failed or unknown-outcome lend leaves the device **unlent** (fail-closed), resolved by reconciling the journaled intent — never by inference from observed state.
- **The claim is not surrendered.** Lending adds a lend generation to the existing ownership record; it does not release the claim. There is **no double-ownership window**: claim, lend and reclaim are generations of one record, and at no point do two principals both hold authority over the device.
- **The lent state is operator-visible**: `DeviceOwnership` answers, typed, who holds the device and whether (and to whom) any of it is lent.
- **A lent device is frozen for volvisor mutations**: destructive role changes, reformat, re-claim and a second lend are typed refusals while a lend is live (`WRITER_ALREADY_ACTIVE`-class semantics at the device layer; the refusal names the live lend).
- **Reclaim needs a release or a recorded force**: the borrower must have released the device (cell teardown observed), or the operator-authorized force path runs — whose record carries a **residue check** (the device's observed state inspected and recorded; foreign residue quarantines the device, never silently reuses it). Force reclaim is an explicit recorded operation, not a cleanup shortcut.
- **Unknown state fails closed** everywhere in the surface: a device whose lending state cannot be determined is not lent, reclaimed or mutated; it is quarantined pending reconciliation.

These invariants are what keep AGENTS rules 7 and 19 true while a cell guest holds the device. Nothing here authorizes production Rook support (ADR-0008's gate wording); the surface exists so the POC can *prove* sharing rather than assume it.

### POC evidence requirements (device lending)

The lending surface is proven by scenarios, each recorded with the POC's evidence discipline (exact SHA pinning, run records, "not run" distinct from "not implemented"):

1. **Lend → visible ownership → use → teardown → reclaim**: a claimed device is lent to a cell, `DeviceOwnership` shows the lend to an operator, a Rook OSD is deployed on the lent device inside the cell, the cell is torn down, and the reclaim completes with its residue check recorded.
2. **Refusals**: a double-lend of a lent device, and a volvisor mutation of a lent device, are both refused typed — never silent.
3. **Force reclaim with residue**: an operator-authorized force reclaim after an unclean borrower exit records the residue check and quarantines on foreign residue.
4. **Fail-closed on unknown state**: a crash between the lend journal and its effect (or an unreadable ownership record) leaves the device unlent-but-quarantined, never inferred lent or reclaimed.

## Cell requirements

A cell VM is infrastructure, not a tenant VM. Host admission atomically reserves the chosen vCPUs, pinned physical CPU capacity (if exclusive reservation claimed), fixed RAM + host overhead, separate Ceph-independent root/system state, and a complete IOMMU-isolated NVMe PCI function. The agent refuses duplicate/ambiguous device claims and refuses accidental host access while the cell owns the device.

Volvisor's management record must include:
```text
CellSpec {
 host_id, cell_id, claimed_controller_device_ids[], claim_generation,
 vm_image_digest, kernel_digest, vm_generation, vcpu_count,
 physical_cpu_reservation?, memory_bytes_fixed, balloon_enabled=false,
 root_state_volume_id (not on managed Ceph),
 trusted_network_refs[], kubernetes_cluster_uid, expected_node_name,
 intended_physical_failure_domain, admission_policy_generation,
 rook_version_pin, ceph_image_pin
}
CellStatus {
 boot_observation, guest_node_uid, guest_nvme_identifiers[],
 k8s_node_ready, osd_ids[], osd_health, mon_quorum,
 claimed_capacity, observed_rss/cpu/oom, peer_reachability,
 replica_placement_proof, ownership_generation, state, evidence_ref?
}
```
Do not serialize kubelet credentials, Ceph keys or private TLS material inside plans/journals/logs.

## Disk capacity semantics

Exclusive VFIO of a PCI-addressable NVMe controller grants guest visibility to the controller's **real** namespace capacities. `disk_size_bytes` may describe or reserve the actual physical capacity, but must not advertise a smaller enforced virtual disk size. A capped logical OSD backing disk uses a separate explicitly selected virtio-blk LV implementation and cannot claim physical VFIO passthrough, latency or hardware failure evidence. An independently assignable PCI function can form another safe boundary only with tested IOMMU isolation. Reject a cell specification requesting arbitrary subdevice VFIO capacity limits.

## State transitions and fail-closed invariants

The cell lifecycle uses the **canonical state vocabulary defined in [ADR-0008](../docs/adr/0008-rook-only-hyperconverged-cells.md)** (same SCREAMING_CASE convention as the migration states of the v2 contracts): `DISCOVERED -> CLAIMED -> VM_PROVISIONED -> BOOTED -> NODE_JOINED -> ATTESTED -> ELIGIBLE_FOR_ROOK -> OSD_PREPARED -> CEPH_READY` with `DRAINING`, `SHUTDOWN`/`UPGRADING`, `RECOVERED` and `QUARANTINED`.

- **ATTESTED** includes physical PCI claim versus guest NVMe by-id mapping, expected guest image, node identity and topology. A Kubernetes `Ready` event is not enough.
- **ELIGIBLE_FOR_ROOK** additionally requires the exact taint/label/admission controls and approved POC CephCluster explicit storage node list.
- **CEPH_READY** requires healthy expected OSD set and Ceph MON/quorum state, not just running pod processes.
- A cell may not relocate to another host while its VFIO device stays on the original physical host.
- Crash recovery reinstantiates the same claimed controller and persistent `/var/lib/rook` metadata; never wipe or allocate a new OSD merely because a pod is missing.
- Destructive disk role changes require separate explicit scoped user authorization and stable hardware identity proof.
- A failed cell cannot trigger broad `kubectl delete node` / Rook cleanup as a generic retry. Cleanup never destroys data on an unresolved generation.
- If the Ceph cluster depends on the cell's root disk or Kubernetes API and cannot start with Ceph down: `NO_GO_DEPENDENCY_CYCLE`.
- Only trusted Rook/Ceph pods and essential node infrastructure allowed in the cell; a taint is only a scheduling aid. Enforcement/admission/Pod security must be proven.
- Node `Allocatable` reflects kubelet reserves/eviction; physical hypervisor still enforces its independent no-overcommit admission and memory limits.

## Rook interface

Pinned, upstream Rook only. `CephCluster`: explicit nodes + device paths; placement affinity + tolerations; bounded resources; MON count and physical topology constraints. Rook operator running externally is not itself a cell workload. Rook may create OSD prepare/cleanup jobs, MON/MGR, exporter, crashcollector and required privileged pods in the reserved namespace. Allowlist must cover observed required components, **not** grant unbounded general workload scheduling.

If a Rook fork is required, record reproducible upstream incompatibility and evaluate alternatives before changing this contract. A local patch is a stop/decision gate, not automatic implementation authorization.

## Evidence

No deployed support claim until all POC C01–C14 tests pass on three distinct physical servers, including real PCI/VFIO isolation and failure-domain audit; provisional benchmark criteria are reviewed before runs. Nested virtio-blk functional proof is insufficient for PCI/Ceph HA gate. Record exact product/source/kernel/guest image and test harness, all failure injections, data checksum oracle and rollback outcomes.

## Explicit non-goals

Not a microVM-per-pod Kubernetes runtime, not a kubelet replacement, not a Kubernetes control-plane bootstrapper, not arbitrary pod hosting, not OSD per RBD volume, not replacing Ceph's own CRUSH/replication, not a guarantee that Volvisor can migrate VFIO OSD microVMs across compute hosts.
