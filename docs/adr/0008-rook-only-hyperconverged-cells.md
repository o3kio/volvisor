# ADR-0008 — Experimental Rook-only Ceph hyperconverged Volvisor Cells

Status: **Proposed / experimental / production NO-GO**
Date: 2026-10-09
Decision-accepted: pending (record acceptance date and accepting authority here)
Builds on: [ADR-0003](0003-tiered-volume-virtualization.md), [ADR-0005](0005-ceph-rbd-and-managed-osds.md), [SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md)
POC: [Rook Cell go/no-go](../poc/rook-cells/README.md)
Scope: **optional managed Ceph OSD infrastructure provider**; does not replace existing-cluster `ceph-rbd` adapter

## Context

Volvisor proposes a hyperconverged appliance-like model: the physical hypervisor uses Cloud Hypervisor to start a reserved, privileged `volvisor-cell` microVM. The cell owns a pinned CPU/RAM budget and dedicated physical storage devices. Kubernetes sees each cell as a worker **Node**; Rook runs Ceph pods only on the enrolled cell nodes. This lets compute workload VMs and the Ceph data plane share chassis without sharing privileged host software or storage ownership.

**This is not a promise that Rook can run 'as a pod' without Kubernetes.** Rook's workloads require a genuine kubelet, container runtime, CNI/networking, compatible Linux guest kernel, Kubernetes API and control-plane connectivity. These run in each cell VM. The Kubernetes cluster's independent control-plane nodes are NOT hosted on the Ceph volume being bootstrapped. Rook operator/CRDs/controllers normally run on separate Kubernetes management nodes; OSD/MON/MGR run on cell nodes. CSI node plugins run on the Kubernetes *consumer* nodes that need CSI, rather than automatically in every cell. Kubernetes system pods needed to support the node are permitted even though tenant workloads are prohibited.

## Decision

Add `ceph_deployment_mode=rook-cell-experimental` behind a disabled-by-default feature gate, not a fourth volume class and not a production promise. A Volvisor-managed physical host can create one **dedicated storage-worker microVM** under Cloud Hypervisor, with pinned identity, immutable boot image, bounded vCPUs and RAM, its own persistent local system/state disk, one or more **exclusively VFIO-passed PCI/NVMe controllers**, and a trusted storage network. Only Rook/Ceph workloads and an explicitly allowlisted minimum of node system services may execute inside the cells.

```text
                         independent Kubernetes API/control plane
                                    |            |
                            external Rook Operator + CRDs
                                    |
                            Kubernetes Node objects
                         (volvisor-cell-a, -b, -c)
                                    |
physical hypervisor A         physical hypervisor B        physical hypervisor C
| O3K workload VM(s)         | O3K workload VM(s)        | O3K workload VM(s)
| Cloud Hypervisor           | Cloud Hypervisor          | Cloud Hypervisor
|  + volvisor-cell-a VM      |  + volvisor-cell-b VM     |  + volvisor-cell-c VM
|       kubelet/containerd   |       kubelet/containerd  |       kubelet/containerd
|       CNI + Rook/Ceph pods |       CNI + Rook/Ceph pods|       CNI + Rook/Ceph pods
|       /dev/nvmeX from VFIO |       /dev/nvmeX from VFIO|       /dev/nvmeX from VFIO
|       guest root/state     |       guest root/state    |       guest root/state
|       private NVMe ========|======= Ceph cluster ======|======= private NVMe
```

Kubernetes node names must be stable and unique, bound to Volvisor `HostId/StorageCellId`; no second registered cell can claim the same physical host's failure domain. The physical host stays outside the cell's Kubernetes scheduling identity unless independently enrolled for unrelated purposes.

### Ownership and execution boundaries

**Volvisor host agent:** verifies physical stable device identity and IOMMU group ownership, journals explicit destructive-authority intent, detaches host driver, binds VFIO, creates Cloud Hypervisor cell and mounts an independent persistent guest boot/state disk. It pins approved guest image digest/firmware/kernel, cgroup/affinity/CPU shares, memory and host network, and holds no simultaneous host OS access to the passed PCI controller.

**Cell guest:** boots trusted image, joins Kubernetes as `volvisor-cell-N` using short-lived bound bootstrap credentials, starts kubelet and minimum container runtime/CNI, exposes guest PCI NVMe controller(s) and persistent `/var/lib/rook` state (not Ceph RBD backed). A guest is not granted arbitrary host admin privileges beyond the passed IOMMU-isolated hardware.

**Kubernetes:** sees three ordinary worker Nodes with trusted labels, physical topology mapping and taints; Rook storage pods are pinned to them. The storage pods still need Linux kernel features, privileged init/probing and raw-device access supported by Rook host-cluster mode. Run no general tenant, build or arbitrary operator workloads there. Namespace RBAC alone and `NoSchedule` taints alone are **insufficient security controls**; enforce with policy admission + restricted service accounts + node-affinity/taint controls. Allowlist explicitly required `kube-system` CNI/kube-proxy agents and Rook prepare/cleanup/discovery/exporter jobs. Unprivileged users cannot label cells or add tolerations to bypass policies.

**Rook:** uses `CephCluster.spec.storage.useAllNodes=false`, `useAllDevices=false`, explicitly named guest Kubernetes nodes and *guest-visible stable by-id disk paths*; any unlisted device must remain untouched. Use CephCluster `placement.all` node affinity/tolerations to restrict Rook-created daemons; account for `prepareosd` and cleanup job placement and separate operator Helm scheduling. MON count 3, no multiple MONs per node, MGR replicas 2 in the POC. A replicated RBD pool with `failureDomain=host`, size 3 has three distinct **physical** host domains, one OSD each. Never collapse 3 cells on the same physical host into three CRUSH hosts. Audit actual CRUSH tree before creating customer RBD images.

## Resource and topology guarantees

Example **POC** resource envelope per physical host (not performance-optimized production sizing): cell VM `vcpu=6`, `memory=16Gi` with ballooning/automatic shrinking disabled, fixed ephemeral/boot local disk size `40Gi`, persistent `/var/lib/rook` state on host-independent-of-Ceph local backing (may be a protected root partition), and exactly one exclusively passed NVMe PCI controller with one blank OSD data device. Real physical host reserves 6 CPU threads/cores as validated plus 16Gi + virtualization/NUMA overhead **before** admitting tenant compute VMs; do not promise truly exclusive physical cores without host CPU affinity/cpuset and disabling host vCPU oversubscription.

Within each cell, kubelet `systemReserved`, `kubeReserved`, eviction hard threshold, and `maxPods` cap the pod allocatable. Example Rook memory requests/limits: OSD 4Gi, MON 1/2Gi, MGR 1/2Gi, with spare for privileged prepare jobs, exporter, CNI, OSD rebuild and node system. These are POC starting points, **not calculated Ceph optimum**. Pod cgroup limits are nested within VM RAM; they do not replace host admission. Restrict OSD provisioning concurrency; reject upgrade/placement if no capacity headroom. Rook derives `osd_memory_target` from memory resource declarations, but the actual process can use more memory under some conditions; measure resident, OOM kills and recovery pressure.

**Strict disk size versus VFIO is a hard constraint:** whole-controller PCI passthrough exposes the physical controller and its actual namespace capacity to the guest. Volvisor can strictly allocate **which device** belongs to a cell, but cannot use VFIO to impose an arbitrary smaller per-guest virtual block-disk size on a shared controller. For strict logical byte caps or multiple per-tenant slices, use a deliberately separate `virtio-blk` disk with a fixed-size host LV/block backend (not hardware PCI passthrough), or qualified truly isolated device functions. Never advertise a sub-capacity limit for a VFIO raw disk. The POC selects whole physical OSD devices, and validates each hardware capacity as a non-overcommittable allocation.

Only the **PCI-addressable controller/function** can be VFIO-passed with IOMMU isolation. Ordinary individual NVMe namespaces on one shared controller are not independent VFIO units. For nested virtualization tests that cannot VFIO-pass a controller, use an exclusive `virtio-blk` disk backed by a sacrificial host block device, but label this as **functional-only** evidence. Such a test does not validate production PCI passthrough latency, reset safety, IOMMU, SMART, trim or firmware health.

## Networking

The cell must have stable, host-unique storage IPs and routable authenticated links to Kubernetes management and Ceph clients. Keep O3K tenant overlays and storage backplanes separated. The simplest POC starts with Rook's normal Kubernetes pod-network provider for bootstrap. A **separate, fresh or controlled follow-up POC run** must assess `network.provider=host` (inside guest, *not bare-metal hypervisor*) with pinned Ceph public/cluster network CIDRs and dedicated VM storage interfaces. Rook's host-network mode reduces overlay overhead but changes security/routing and MON reachability; do not dynamically toggle networking on an active Ceph cluster without a migration plan. No host network policy bypass or wide listening on tenant NICs. Monitor p99 guest/host network overhead, OSD replication, bandwidth contention and host data-plane CPU.

## Cell lifecycle state machine

```text
DISCOVERED -> CLAIMED -> VM_PROVISIONED -> BOOTED -> NODE_JOINED
  -> ATTESTED -> ELIGIBLE_FOR_ROOK -> OSD_PREPARED -> CEPH_READY
  -> DRAINING -> SHUTDOWN/UPGRADING -> RECOVERED
                         \-> QUARANTINED
```

Cell reschedule is **not** a generic Kubernetes pod reschedule: physical NVMe ownership binds the cell to its physical host. Kubernetes marks a failed cell Node NotReady but must **not** relocate an OSD tied to a PCI device on a different host. On process/host crash, Volvisor reconciles exact PCI identity, cell image/state disk identity and original Kubernetes node name, then reboots the cell and lets Rook recover OSD IDs. Never auto-reinitialize a recognized BlueStore disk or delete unknown `/var/lib/rook` metadata.

Coordinate physical-host maintenance: Ceph `ok-to-stop`/OSD flags, Rook PDB, MON quorum, host CPU and existing tenant VM reservations; O3K may not drain a cell whose loss compromises cluster safety. Avoid Ceph-dependency cycles: Volvisor host services, Kubernetes API/etcd, cell images and guest root/state boot path must remain functional even when the Ceph cluster is down.

### Trust / security

Pin Cloud Hypervisor, guest image, kernel, containerd, Rook, Ceph versions and Kubernetes API compatibility. A compromised storage guest must not rebind arbitrary host NVMe: VFIO/IOMMU and host agent enforce hardware boundary, and joining kubelet identity does not allow arbitrary resource changes. Kubernetes API access in guests is least-privilege; no reusable administrative kubeconfigs baked into images. Avoid exposing raw volume metadata or Ceph keys through Volvisor plans/logs. Data-device identifiers are stable and cryptographically bound to cell claim plans; no wildcard device discovery or destructive adoption.

## Rook patch policy

**No Rook fork/patch in POC phase 0.** Rook already has placement/toleration, explicit-node/raw-disk selection and resource knobs required for a first attempt. If the POC discovers a concrete functional mismatch, reproduce it on a pinned upstream Rook release and propose a minimal upstream extension/patch only after evaluating existing CRD, chart, pod scheduling and topology facilities. Local forks add long-term support costs and are not assumed prerequisites.

Potential extension triggers: privileged OSD preparation rejects passed NVMe identity, mandatory discovery DaemonSets cannot be constrained, OSD topology encodes guest identity incorrectly relative to physical failure domain, or network/resource reconciliation cannot preserve safe cell lifecycle. Each requires evidence, not speculation.

## Consequences, exclusions and decision

Benefits: familiar Rook/Ceph lifecycle inside an isolated per-host storage appliance; host privilege isolated from Ceph code; hardware I/O direct with VFIO; Ceph OSDs and compute VMs coexist on the chassis, while O3K provides host-level resource arbitration.

Costs: running a Kubernetes worker stack in *every* storage microVM, a new network and reboot failure boundary, extra RAM/CPU reserved for Rook/Ceph, OSD restart amplification, guest/kernel/driver interactions, coupled compute/storage host maintenance and scheduling complexity. For small deployments the operational/resource tax may outweigh isolation value. This is a **research hypothesis**, not a Nutanix-class HCI production claim.

Non-goals: a full Kubernetes control plane inside every cell, running arbitrary tenant pods, treating a cell like a freely relocatable Kubernetes worker, using underlying Ceph RBD for cell bootstrap, blindly auto-claiming disks, changing Ceph on-disk layout, assuming Rook patch needed, production HA claims from three nested VMs on one host.

**Decision gate:** [small three-physical-host POC](../poc/rook-cells/README.md) must prove bare-metal VFIO claim, Kubernetes node registration/placement/admission, Rook OSD/MON/MGR deployment, RBD I/O and storage client reachability, failure recovery, CRUSH physical placement, resource ceilings and performance vs Rook directly on the same hardware. Passing nested functional tests alone can only authorize physical POC, **not production acceptance**. On failure, keep the existing Ceph adapter and reconsider bare-metal Rook or service VM approach without adding an intrusive fork.
