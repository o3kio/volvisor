# Rook-only Volvisor Cell experimental POC — GO / NO-GO

Status: **DESIGN + EXAMPLE INPUTS ONLY; not run**
Related: [ADR-0008](../../adr/0008-rook-only-hyperconverged-cells.md)
Target: prove feasibility of hyperconverged Rook/Ceph inside fixed-budget Cloud Hypervisor storage microVMs without patching Rook.

## 0. Prerequisites / no destructive surprise

This is **not** a one-command deployment. A qualified operator must provide three separate physical hosts with KVM/IOMMU, one **sacrificial** compatible PCI/NVMe controller each, a trusted Kubernetes control plane external to these cells, an available Rook operator installed on control nodes, network connectivity, disposable test data and verified backups. Never run discovery/OSD preparation against a disk with production data. The example manifest names must be edited to actual **guest** by-id identifiers before apply. Do not enable `useAllDevices` or broad device filters. Manual review of `lsblk -f`, `nvme list`, `lspci -nnk`, `findmnt`, IOMMU groups, ownership journal and proposed VFIO unbind is mandatory.

Version lock: choose and record an actually compatible Kubernetes + Rook + Ceph release combination and pin exact immutable container/image digests. Initial example references Ceph `v19.2.3`, not a product/version recommendation. Record guest/hypervisor kernels, CPU model, CHV version, Rook version, operator image, CNI, containerd and ceph image on every run.

## 1. Minimal topology

```text
physical host A: compute VMs + CHV Volvisor Cell A -> VFIO NVMe A
physical host B: compute VMs + CHV Volvisor Cell B -> VFIO NVMe B
physical host C: compute VMs + CHV Volvisor Cell C -> VFIO NVMe C

cells -> K8s worker nodes with 1 Rook/Ceph OSD each
K8s API + Rook operator -> independent pre-existing management nodes
Rook CephCluster -> 3 MON, 2 MGR, 3 OSD
RBD CephBlockPool -> failureDomain=host, replicated.size=3
client -> non-cell compute node or O3K RBD host adapter
```

First (optional) nested three-cell smoke test on a **single physical host** may validate Kubernetes/Rook API wiring, but not distinct host failure domains, PCI passthrough or HA, and never counts as the GO/NO-GO hardware gate.

Reference conservative cell budget: 6 vCPUs pinned/charged to physical host allocation, 16 GiB fixed RAM with ballooning disabled, local 40 GiB persistent root/system disk **not on Ceph**, persist guest `/var/lib/rook` across cell restarts, 1 entire IOMMU-safe PCI NVMe controller dedicated to Ceph, separate VM storage data-path network. Adapt after measuring actual per-OSD and MON/MGR requirements. A guest disk's size is not restricted by Rook pod limits; Volvisor claims physical device capacity by stable identity and explicitly passes the whole controller. Reserve real host resources before tenants receive compute capacity.

## 2. Bring-up in order (not yet automated)

1. Provision independent, long-lived Kubernetes API/etcd nodes, DNS, authenticated CNI and routing from cells and Ceph clients. Avoid self-hosting API/etcd or cell root/state on this Ceph cluster.
2. Pin guest image with Linux NVMe, VFIO guest support, kubelet/containerd, Ceph kernel/userland prerequisites and diagnostics. Cell has no admin kubeconfig baked in.
3. Volvisor host agent discovers each physical NVMe, verifies stable ID and IOMMU isolation, journals explicit controller claim and binds VFIO before CHV `--device` passthrough. No controller may be simultaneously claimed by host native storage/other VM.
4. Allocate cell fixed CPU/RAM, local root/state volume and NIC(s); boot and verify `lspci -nnk`, `lsblk -o NAME,SERIAL,SIZE,TYPE,FSTYPE,MOUNTPOINTS`, exact by-id path, NVMe firmware/health and clean signatures inside guest. Show physical host cannot still issue NVMe writes to it.
5. Join each cell to external Kubernetes control plane with stable node name. Require three `Ready` Node objects after CNI/bootstrap. Apply topology labels and taint from trusted management automation **after configuring taint tolerations for essential `kube-system` CNI agents**, so network does not deadlock.
6. Configure admission policy: only approved Rook/Ceph namespace/service accounts and preapproved kube-system DaemonSets can schedule on `volvisor-cell` nodes. Prove a tenant Pod with an attempted toleration is refused. Taints alone do not secure nodes.
7. Install Rook operator outside cells, from pinned release. Ensure operator itself is not scheduled to storage cells; configure any cluster-wide discovery/CSI daemonset node policy.
8. Inspect exact guest by-id symlinks, then edit [CephCluster example](cephcluster-poc.yaml) with actual values and apply. Confirm Rook OSD prepare jobs/OSD pods and MON/MGR stay on correct cell nodes. No spontaneous OSD on guest root/system disk. Keep `cleanupPolicy` destructive actions disabled.
9. Apply [RBD pool example](cephblockpool-poc.yaml) after health; inspect actual CRUSH tree and ensure three **distinct physical host** domains with one OSD each. Read/write a test RBD image via O3K/Volvisor client path outside cell, with checksums and fio.
10. Benchmark against **direct Rook on the same hardware and equal OSD memory/CPU budgets**, controlling Ceph/RBD settings, network, device/media, fio jobs and recovery. Do not interpret peak IOPS without p99, CPU, throttling and recovery load.

## 3. Example enrollment plan

[Example cell claim and resource plan](cell-plan.example.yaml) is illustrative YAML **not an executable current Volvisor API**. An implementation must create/version the machine-readable plan separately, with immutable storage device IDs and trusted host/guest identity verification before any mutation.

The Kubernetes CephCluster CR is the **real Rook integration surface**. Disk paths are guest-visible and must be revalidated on every cell restart; the host claim is an independent Volvisor transaction. Rook's RBD StorageClass is optional for Kubernetes CSI consumers; O3K's direct Ceph RBD volume adapter can use an existing cluster with host-level mapping and appropriate Ceph credentials without running guest workloads inside cells.

## 4. Fault injection and GO/NO-GO matrix

| ID | Test | PASS condition |
|---|---|---|
| C01 | 3 cells on **three** physical hosts; actual controller VFIO assignment | explicit device and IOMMU ownership, guest NVMe stable, host cannot mutate assigned disk |
| C02 | restart cell without wipe; kubelet rejoins same Node | identical claimed NVMe, old Ceph FSID/OSD IDs and MON state survive |
| C03 | Rook only + trusted minimal Kubernetes infra | unauthorized tenant Pod rejected; allowed OSD/prepare/MON/MGR scheduled correctly |
| C04 | 3 OSD/3 MON/2 MGR, size-3 host-domain RBD pool | Ceph healthy and real CRUSH OSD leaves map to 3 independent physical chassis |
| C05 | cold read/write through non-cell RBD client | checksums match; read/write flush and reconnect semantics verified |
| C06 | sudden one physical host power-loss, keep two hosts running | no split brain; MON quorum maintained; RBD data remains correct and I/O behavior matches policy |
| C07 | single cell VM crash / restart | OSD returns without reformat and Ceph data stays intact; no host passthrough owner conflict |
| C08 | remove/return guest storage NIC and isolate Kubernetes API | documented network outage and control-plane outage behavior, no invalid destructive mutation |
| C09 | OSD disk loss, replace **only after scoped explicit authority** | no automatic reuse of unknown disk, controlled recover/replace from remaining replicas |
| C10 | overload memory, CPU, Ceph recovery/backfill and tenant compute contention | cell bounded by reserved physical budget; never causes unapproved host overcommit; no host OOM or silent VM eviction |
| C11 | Rook/Ceph rolling upgrade and pinned cell image replacement | no lost OSD identity or unplanned loss of quorum, rollback/recovery documented |
| C12 | direct-bare-metal Rook baseline with identical hardware/policy | performance and ops penalty measured and disclosed |
| C13 | independence cycle (K8s API/root disk unavailable; Ceph down) | host-agent and cell bootstrap not dependent on their own Ceph |
| C14 | ceph public/cluster traffic isolated from tenant network | accessible from intended compute/RBD clients only; storage path not accidentally tenant reachable |

**Hard NO-GO** if C01, C02, C03, C04, C05, C06, C07, C09, C10 or C13 fail. Any failure involving dual hardware ownership, unexpected wipe, duplicate physical CRUSH domain, quorum split-brain or self-dependency is an immediate stop. The POC cannot be marked GO when any required test is `NOT_RUN`.

**Performance provisional thresholds for evaluation (not claims):** fixed, published workload profile, median of at least 3 independent runs. Seek >=80% direct-on-host Rook throughput/IOPS under matched resources; p99 latency <=1.3x direct baseline; incremental CPU cost <=20% of baseline (excluding measurement noise), and reserved cell memory/CPU consumption within the agreed per-host budget. Failures trigger explicit tradeoff review rather than silent pass. These thresholds are initial *decision criteria* to approve/reject with stakeholders **before** measurements; no benchmarks have been performed. Separate network/pod-overlay overhead from VFIO/VM overhead by testing guest `fio` directly against the passed NVMe as well as RBD.

**Conditional GO for further R&D**, not production: all C01–C14 PASS, no unsupported Rook patch necessary, performance within accepted thresholds or a documented accepted exception, and a repeatable reprovision/recovery path with exact commits/versions.

**NO-GO for Rook-cell mode**: require a Rook fork with invasive changes; host cannot VFIO-isolate OSD controllers; cell-only Pod policy cannot be enforced; Ceph cannot survive intended physical-host failure; bootstrap depends on Ceph; resource competition disrupts tenant VMs; or unacceptable latency/ops overhead relative to direct Rook.

## 5. Evidence bundle and follow-up

Each iteration stores:
- immutable HostId/CellId, source commit, guest image/kernel, CHV/Rook/Ceph/K8s/containerd/CNI digests;
- physical NVMe serial/NGUID, IOMMU group, host VFIO bindings, guest by-id mappings and audit logs;
- pre/post `kubectl get nodes,pods -A -o wide`, `ceph status`, `ceph osd tree`, `ceph osd dump`, Ceph pool properties and placement map;
- fio workload JSON, RBD data checksum oracle, host and guest CPU/RSS/IOPS/network p99 data;
- failure timestamps, recovery duration and exact resulting volume state;
- explicit signed operator decision (GO / NO-GO / CONDITIONAL / INCONCLUSIVE).

Rook documentation:
- https://rook.io/docs/rook/latest/CRDs/Cluster/host-cluster/
- https://rook.io/docs/rook/latest/CRDs/Cluster/ceph-cluster-crd/
- https://rook.io/docs/rook/latest/CRDs/Block-Storage/ceph-block-pool-crd/
- https://rook.io/docs/rook/latest/CRDs/Cluster/network-providers/
- https://kubernetes.io/docs/tasks/administer-cluster/reserve-compute-resources/
- https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/vfio.md
