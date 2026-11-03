# Volvisor

Volvisor is the **volume virtualization and orchestration layer** for O3K and CellHV. It presents one managed block-volume API across **three distinct backends**, while preserving their real locality, durability, and mobility semantics.

> **Maturity: implemented prototype; no production support.** Start with
> [docs/START_HERE.md](docs/START_HERE.md) and the
> [support matrix](SUPPORT_MATRIX.md). The v0.1.0 release is being
> prepared; **no tag or GitHub release exists yet**, and the distribution
> tag is blocked in this workstream by the missing signing key. The separate
> pre-production exit gate is tracked in the
> [v0.1.0 checklist](docs/releases/v0.1.0-checklist.md).

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

## Normative v2 documents

- [ADR-0003 — volume architecture](docs/adr/0003-tiered-volume-virtualization.md)
- [ADR-0004 — nearline replication and migration](docs/adr/0004-nearline-replication-and-mobility.md)
- [ADR-0005 — Ceph RBD versus OSD lifecycle](docs/adr/0005-ceph-rbd-and-managed-osds.md)
- [ADR-0006 — online resize and live local block relocation](docs/adr/0006-online-resize-and-live-local-block-relocation.md)
- [ADR-0007 — DRBD 9 nearline replication backend](docs/adr/0007-drbd9-nearline-replication-provider.md)
- [ADR-0008 — experimental Rook-only hyperconverged cells](docs/adr/0008-rook-only-hyperconverged-cells.md)
- [Experimental Rook Cell contract](contracts/rook-cell-experimental-v0.md)
- [Three-host Rook Cell GO/NO-GO POC](docs/poc/rook-cells/README.md)
- [SPEC-0002 — v2 implementation specification](docs/specs/SPEC-0002-volvisor-volume-virtualization.md)
- [Volume API contract v2](contracts/volume-api-v2.md)
- [Nearline replication contract v2](contracts/nearline-replication-v2.md)
- [2026-10-09 adversarial design review](docs/reviews/2026-10-09-volume-architecture-review.md)
- [P0 implementation plan](docs/plans/2026-10-09-p0-implementation.md)

## Implementation (prototype)

A Rust workspace implements the P0 control plane (stable IDs, device claiming,
single-volume API/attachments, provider conformance — SPEC-0002 section 12)
plus the native-local LVM slice, the external Ceph RBD adapter slice and the
DRBD 9 nearline baseline slice:

```text
crates/volvisor-types      typed domain vocabulary (Volume API v2 mirror)
crates/volvisor-journal    durable intent journal + idempotency registry
crates/volvisor-provider   engine-neutral provider trait, command runner,
                           fake provider + conformance kit
crates/volvisor-lvm        native-local LVM provider (thick LV, P1 slice)
crates/volvisor-ceph       ceph-rbd adapter for an existing external Ceph
                           cluster (P2 slice, ADR-0005 Phase A)
crates/volvisor-drbd       DRBD 9 nearline baseline: the local end of a
                           single-primary replicated resource over an
                           operator-designated VG (P3 slice, ADR-0007)
crates/volvisor-api        HTTP/JSON Volume API v2 server
crates/volvisord           daemon binary
```

Build and test: `cargo build`, `cargo test --workspace`,
`cargo clippy --all-targets` (CI denies warnings). Real-backend suites have
one-command gates with typed, loud failures when their rig is missing:

```sh
sudo ./scripts/test-lvm.sh          # locally runnable (root + lvm2 + loop)
sudo -E ./scripts/test-ceph.sh      # external cluster; self-hosted runner
sudo -E ./scripts/test-drbd.sh      # DRBD 9 peer; self-hosted runner
```

Each script supports `--check`. Ceph needs `VOLVISOR_TEST_CEPH=1` plus
`VOLVISOR_CEPH_*` cluster configuration; DRBD needs `VOLVISOR_TEST_DRBD=1`
plus `VOLVISOR_DRBD_*` host/peer configuration, the drbd-utils toolchain,
the DRBD 9 kernel module and a provisioned peer. The release pipeline refuses
to publish unless every provider declared `supported` in
[`release-scope.toml`](release-scope.toml) ran and passed its suite; providers
declared `not-run` are explicit non-claims. See
[`docs/testing/real-backends.md`](docs/testing/real-backends.md) for rig
topology, runner labels, least-privilege credentials, secret handling,
artifact retention and failure triage. This is prototype software: no
production-support claim is made or implied (AGENTS rule 12).

## Day-2 operations

Day-2 recovery procedures and their drill results are in the
[day-2 runbook index](docs/runbooks/day-2/README.md). The executable drills and
raw records are in the
[2026-11-03 single-host evidence bundle](docs/evidence/day2/2026-11-03-devhost/).

| Area | Recorded verdict |
|---|---|
| Control-plane backup/restore with attachment state, revision and tombstones | **DRILLED** |
| Schema compatibility, upgrade-on-write and future-version refusal | **PARTIAL** — live old-binary → new-binary upgrade/rollback not run |
| Lost/unreachable witness service | **PARTIAL** — storage-side DRBD fencing not run |
| Torn journal tail and corrupt state file | **DRILLED** |
| ENOSPC on journal/state writes | **DRILLED**, with an open state-save finding ([#58](https://github.com/o3kio/volvisor/issues/58)) |
| Crash loop and single-writer lock contention | **DRILLED** |
| Credential and certificate rotation | **DRILLED** — restart-based, no hot reload |
| Total host loss and re-admission | **NOT RUN — REQUIRES** a real multi-host DRBD/Ceph lab, a third failure domain and BMC/fencing |

The drills are single-host dev-lab evidence, not production or multi-host
evidence. They do not change the support posture in
[SUPPORT_MATRIX.md](SUPPORT_MATRIX.md).

## Historical draft v1 (superseded proposals)

[ADR-0001](docs/adr/0001-volvisor-storage-cell-architecture.md), [ADR-0002](docs/adr/0002-replicated-async-local-endpoint-and-migration.md), [SPEC-0001](docs/specs/SPEC-0001-storage-cell-v0.md), [provider v1](contracts/volvisor-provider-v1.md), [storage classes v1](contracts/storage-class-semantics-v1.md), [replication v1](contracts/replicated-async-v1.md), [engineering design](docs/design.md), [replication research](docs/research/replicated-async-rnd.md), and [prior review](docs/reviews/storage-cell-v0-review.md).

The v1 drafts remain available for traceability and historical design research. The v2 documents replace conflicting class definitions; v1 cannot be reinterpreted silently. Some historical R&D comparisons remain useful but must be revalidated against the pinned implementation versions.

## Status

**Implemented prototype; no production support.** The workspace implements
the control-plane and provider slices above, but **no class is production
supported** and no durability, performance, SLA, zero-RPO failover, or
live-migration guarantee is claimed. Evidence is reported in the tiers the
normative documents define: Tier S simulation, Tier R real-host gates (not
run), and Tier V bounded real-VMM runs. Every positive claim below links to
its record; anything not executed is explicitly **not run** or **blocked**.
The full class-by-provider view is [SUPPORT_MATRIX.md](SUPPORT_MATRIX.md).

### Per-class evidence

| Class / area | Implemented today | Recorded evidence | Not run / blocked |
|---|---|---|---|
| `native-local` | LVM provider over a host LVM volume group, stable volume identity, journaled/idempotent mutations, attach/detach/grow/delete and same-VG `pvmove` relocation. | Unit, provider-conformance, journal, and crash/replay tests in [crates/volvisor-lvm](crates/volvisor-lvm) and [crates/volvisor-provider](crates/volvisor-provider); the real-LVM suite is wired as a CI job (`lvm-integration` in [ci.yaml](.github/workflows/ci.yaml)) and is the `native-local` release-scope gate (`supported` in [release-scope.toml](release-scope.toml)); it passed locally on a loop device ([checklist](docs/releases/v0.1.0-checklist.md)). No committed real-host evidence bundle exists. | The SPEC-0002 §11 real-host campaign (SSD loss, thin-pool exhaustion, host power-cut, reboot identity) is **not run**. A local mirror is **declined** ([ADR-0011](docs/adr/0011-local-mirror-experiment-decline.md)). Live migration of an attached native-local volume is unsupported. |
| `nearline-replicated` | DRBD 9 host-kernel adapter (local end), Protocol A/B/C selection, witness/lease fencing, and the DRBD-only migration/handoff surface. | Tests in [crates/volvisor-drbd](crates/volvisor-drbd) and [crates/volvisor-witness](crates/volvisor-witness); Tier S simulated fault campaign ([report](docs/evidence/tier-v/2026-10-11-devhost-v51/REPORT.md): 91 Tier S records); Tier V migration, kill-window, snapshot-divergence, and resize scenarios against real Cloud Hypervisor v51.1 on **one physical development host with simulated storage** ([run](docs/evidence/tier-v/2026-10-11-devhost-v51/)). | The **three-physical-host campaign [#37](https://github.com/o3kio/volvisor/issues/37) is not run**; there is no real-DRBD, real-media, split-brain, power-cut, or 24 h soak evidence. The real-DRBD suite is a release-scope **`not-run`** provider: no green rig run exists and nearline is explicitly **not claimed** by this release ([release-scope.toml](release-scope.toml), [real-backends gate](docs/testing/real-backends.md)). The #39 gate machinery is landed and blocks publish if nearline is flipped to `supported` without a passing suite. Async replication has a possible RPO tail. |
| `ceph-rbd` | Adapter for an **existing, externally operated** Ceph cluster: FSID/pool fail-closed startup, ownership metadata, `exclusive-lock` single-writer attach, growth, delete policy, health/capacity reflection. | Unit, `FakeCeph`, and provider-conformance tests in [crates/volvisor-ceph](crates/volvisor-ceph); the real-Ceph suite is a release-scope **`not-run`** provider ([release-scope.toml](release-scope.toml)). No committed real-cluster evidence bundle exists. | Real-cluster create/map/unmap, auth loss, OSD loss, host loss, Ceph degraded/full, upgrade, and live migration are **not run**. ceph-rbd is explicitly **not claimed**: no green Ceph rig run exists, and publish is blocked if it is flipped to `supported` without a passing suite ([#39](https://github.com/o3kio/volvisor/issues/39)). |
| Rook-cell managed Ceph (experimental deployment mode, not a volume class) | Device-lending/reclaim/quarantine surface and the design plus example manifests. | Daemon-level Tier S tests for the six lending invariants ([lending contract](contracts/rook-cell-experimental-v0.md)); [POC design and GO/NO-GO matrix](docs/poc/rook-cells/README.md). No real-device result. | The three-physical-host POC [#38](https://github.com/o3kio/volvisor/issues/38) has **never been executed; there is no GO**. ADR-0008 remains **production NO-GO**; VFIO, CRUSH physical-host placement, and host loss are unproven. |
| Daemons and packages | `volvisord` / `volvisor-witnessd`, `--version`/`--check-config`, systemd units, deb/rpm metadata, install smoke. | Local build, package, `--check-config`, and container-smoke results are recorded in the [v0.1.0 checklist](docs/releases/v0.1.0-checklist.md); CI has package-build/package-smoke jobs. | No signed tag or GitHub release exists. Packaging does not prove unit start under the packaged install, upgrade/downgrade transitions, or any architecture other than x86_64. |
| API transport, authentication and audit | Native rustls TLS and optional mTLS on the API and witness listeners; HTTPS witness/peer clients; non-loopback plain HTTP refused unless `allow_insecure = true`; scoped admin/operator/read-only credentials; read authorization when scoped credentials exist; structured audit attribution. | Tests in [crates/volvisor-tls](crates/volvisor-tls), [crates/volvisor-api](crates/volvisor-api) and [crates/volvisord](crates/volvisord); same-host netns/veth capture showing certificate verification, insecure-bind refusal, an encrypted pcap with no token/path/header plaintext, and audit attribution ([bundle](docs/evidence/transport-tls/2026-11-02-devhost-netns/)). Deployment guidance is in [docs/security/deployment.md](docs/security/deployment.md). | Cross-host TLS, peer/witness hops on separate physical hosts, live certificate rotation, HSM/TPM key storage and external terminators are **not run**. The capture is same-host and its bundle records a dirty-tree run at base `a6591d9`. The legacy `admin_token`-only mode leaves reads open; configure scoped credentials and TLS on any shared network. |

Tier R gates are recorded as **skipped** in the campaign report, not passed.
Tier V is not production support: it verifies the VMM seam at recorded
versions while the storage below the seam is simulated
([ADR-0010](docs/adr/0010-tier-v-real-vmm-verification.md)). See the
[release notes draft](docs/releases/v0.1.0.md) for the intended v0.1.0
scope and the [checklist](docs/releases/v0.1.0-checklist.md) for the
distribution-tag gate and the separate pre-production exit gate.

Online growth of a local LV is distinct from **same-host live relocation** of its backing storage, which is itself distinct from cross-host live VM migration. See ADR-0006 for native Cloud Hypervisor resize, same-VG LVM pvmove, and optional QEMU Storage Daemon + vhost-user-blk block mirroring.

For nearline, **DRBD 9 is the first replication-engine prototype candidate**, operating directly above LVM logical volumes (host-kernel path) or from an isolated Storage Cell (via qualified host-to-cell block frontend). Protocol A provides local-first asynchronous replication; Protocol C is an explicit optional synchronous-protection profile. The difficult cross-host Cloud Hypervisor migration handoff is **not** considered solved merely by using DRBD; see ADR-0007.

Optional **experimental managed-Ceph mode**: Volvisor boots fixed-resource Cloud Hypervisor `volvisor-cell` infrastructure VMs with exclusively VFIO-assigned NVMe, joins them as dedicated Kubernetes worker Nodes and permits only Rook/Ceph plus essential node-system workloads. Rook operator/control plane remain independent. This mode is *disabled by default and NOT production supported*; three **physical** host Rook/Ceph failure and performance tests are required. No Rook fork is presumed.
