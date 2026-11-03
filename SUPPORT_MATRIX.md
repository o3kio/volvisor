# Volvisor support matrix (v0.1.0 preparation)

Status: **implemented prototype; no production support**
Date: 2026-11-03
Basis: `main` at `575acfc` (Wave 2 merged: #41 schema, #40 transport/auth, #42 provider core, #45 admission control, #46 day-2 runbooks, #44 observability, #43 benchmarks)
Intended release: `v0.1.0` — **not tagged, not published**
Gate record: [pre-production readiness report](docs/releases/pre-production-readiness.md)
Normative context: [SPEC-0002 §11](docs/specs/SPEC-0002-volvisor-volume-virtualization.md#11-real-host-acceptance-matrix),
[ADR-0003](docs/adr/0003-tiered-volume-virtualization.md),
[ADR-0004](docs/adr/0004-nearline-replication-and-mobility.md),
[ADR-0005](docs/adr/0005-ceph-rbd-and-managed-osds.md),
[ADR-0007](docs/adr/0007-drbd9-nearline-replication-provider.md),
[ADR-0008](docs/adr/0008-rook-only-hyperconverged-cells.md),
[ADR-0010](docs/adr/0010-tier-v-real-vmm-verification.md)

This matrix is the single answer to "what has actually been shown?" It
does not raise any class above **implemented prototype**. A cell that says
"not run" is an evidence statement, not an implementation statement: the
code may exist and be unit-tested, but it has not passed the real-host
gate that a production claim would require.

## Status vocabulary

| Label | Meaning |
|---|---|
| **Prototype — Tier S** | Exercised against the in-process simulation with injected faults; no real storage, DRBD peer, Ceph cluster, or VMM required. |
| **Prototype — Tier V** | Exercised against a real Cloud Hypervisor process at recorded version/SHA256; storage below the VMM seam is simulated and the run is single-host. Not real-media or multi-host evidence. |
| **Prototype — CI-gated** | An integration suite exists and is wired to a CI/release job; this says the route exists, not that the suite has passed on the release commit. The real-LVM suite is CI-gated. Ceph/DRBD have one-command release-scope gates and self-hosted workflow jobs but no green rig run; they are `not-run` non-claims in [release-scope.toml](release-scope.toml). |
| **Not run** | The scenario has not been executed. Do not infer a result from design, examples, or unit tests. |
| **Blocked** | A prerequisite outside this document is missing. The blocking issue is linked. |
| **No production support** | Applies to every row. No SLA, durability, zero-RPO, availability, or live-migration commitment is made. |

## Class × provider × evidence × deployment shape × gaps

| Class | Provider / engine | Evidence status | Deployment shape covered | Known gaps and blockers |
|---|---|---|---|---|
| `native-local` | LVM over a host volume group (host-kernel block path) | **Prototype.** Unit, provider-conformance, journal/replay, grow and same-VG move tests in [crates/volvisor-lvm](crates/volvisor-lvm) and [crates/volvisor-provider](crates/volvisor-provider). The real-LVM integration suite is **CI-gated** (`lvm-integration` in [ci.yaml](.github/workflows/ci.yaml)) and passed locally on a loop device ([checklist](docs/releases/v0.1.0-checklist.md)). No committed real-host evidence bundle. | Single physical host; operator-designated LVM VG; Cloud Hypervisor `virtio-blk` on the host device. | SPEC-0002 §11 real-host campaign is **not run**: multi-volume pool isolation, stale-attach/sanitization, full thin pool, SSD loss, reboot identity. Local mirror is **declined** ([ADR-0011](docs/adr/0011-local-mirror-experiment-decline.md)). Attached native-local volumes are not live-migratable. **No production support.** |
| `nearline-replicated` | DRBD 9 host-kernel adapter (deployment variant A) + witness/lease fencing | **Prototype — Tier S + Tier V.** Tests in [crates/volvisor-drbd](crates/volvisor-drbd) and [crates/volvisor-witness](crates/volvisor-witness); 91 Tier S records and 4 Tier V scenarios at Cloud Hypervisor v51.1 ([second run](docs/evidence/tier-v/2026-10-11-devhost-v51/), [campaign report](docs/evidence/tier-v/2026-10-11-devhost-v51/REPORT.md)). The real-DRBD suite is a release-scope **`not-run`** provider: no green DRBD rig run exists and nearline is explicitly **not claimed** by this release ([release-scope.toml](release-scope.toml), [real-backends gate](docs/testing/real-backends.md)). The #39 gate machinery is landed and blocks publish if nearline is flipped to `supported` without a passing suite. | Tier V: **one physical development host**, real VMM processes, storage below the VMM seam simulated. Design shape: two data hosts plus a witness on a third failure domain. | **The three-physical-host campaign [#37](https://github.com/o3kio/volvisor/issues/37) is not run.** A copy-pasteable [campaign runbook](docs/runbooks/nearline-multihost-campaign.md) and read-only [preflight script](scripts/preflight-nearline.sh) now exist; they are procedure/tooling, not evidence. There is no real-DRBD, real-media flush/FUA, power-cut, separate-SSD-loss, split-brain, peer-rejoin, or 24 h soak evidence. Ceph/DRBD are **`not-run`** providers in [release-scope.toml](release-scope.toml): no green rig run exists and neither is claimed. The [#39](https://github.com/o3kio/volvisor/issues/39) gate machinery is landed; publish fails if a provider is flipped to `supported` without a passing suite ([real-backends gate](docs/testing/real-backends.md)). Protocol A has a possible acknowledged-write tail on host loss; quorum and witness are not data replication. **No production support.** |
| `ceph-rbd` | Existing external Ceph cluster, driven through the `ceph`/`rbd` CLIs | **Prototype.** Unit, `FakeCeph`, and provider-conformance tests in [crates/volvisor-ceph](crates/volvisor-ceph). The real-Ceph suite is a release-scope **`not-run`** provider: no green Ceph rig run exists and ceph-rbd is explicitly **not claimed** by this release ([release-scope.toml](release-scope.toml), [real-backends gate](docs/testing/real-backends.md)). No committed real-cluster evidence bundle. | Existing externally operated Ceph cluster; host-level RBD mapping; Cloud Hypervisor `virtio-blk`. Volvisor does not operate the cluster's MON/MGR/OSD topology in this mode. | Real-cluster create/map/unmap, credential/auth loss, OSD loss, host loss, degraded/full cluster, upgrade, control-plane outage, and shared-backend live migration are **not run**. Ceph/DRBD are **`not-run`** providers in [release-scope.toml](release-scope.toml): no green rig run exists and neither is claimed. The [#39](https://github.com/o3kio/volvisor/issues/39) gate machinery is landed; publish fails if a provider is flipped to `supported` without a passing suite. **No production support.** |
| `ceph-rbd` via `rook-cell-experimental` (optional managed-OSD deployment mode) | Rook/Ceph inside fixed-resource Cloud Hypervisor cells; Volvisor owns device claim/lend/reclaim | **Design + example inputs only; not run.** The device-lending surface has daemon-level Tier S tests over a durable fake ([lending contract](contracts/rook-cell-experimental-v0.md)), not real-device evidence. | Three **physical** hosts, one exclusively VFIO-passed NVMe controller per cell, cells as Kubernetes worker Nodes, an independent Kubernetes API and Rook operator. | The three-physical-host POC [#38](https://github.com/o3kio/volvisor/issues/38) has **never been executed and there is no GO**. No C01–C14 result exists; VFIO isolation, CRUSH physical-host placement, host loss, memory pressure, and performance are unproven. ADR-0008 is **production NO-GO**. **No production support.** |
| Witness / fencing / VM handoff (cross-cutting) | `volvisor-witnessd`, DRBD authority, barrier state machine, Cloud Hypervisor adapter | **Prototype — Tier S + Tier V.** Tests in [crates/volvisor-witness](crates/volvisor-witness), [crates/volvisor-handoff](crates/volvisor-handoff), and [crates/volvisor-drbd](crates/volvisor-drbd); Tier S authority/fencing/kill matrix; Tier V real-VMM migration, kill-window, snapshot-divergence, and resize scenarios at v51.1. | Tier V: single physical host, real VMM pair, simulated storage/witness worlds. The handoff surface is wired only for the `drbd` provider. | Real-host witness loss, stale-epoch/split-brain, peer partition, source/target crash at every commit boundary, and multi-volume consistent cut are **not run** on real hardware ([#37](https://github.com/o3kio/volvisor/issues/37)). The lvm provider has no mobility surface. **No production support.** |
| API transport, authentication and audit (cross-cutting) | `volvisord` and `volvisor-witnessd` HTTP surfaces; peer/witness clients | **Prototype — CI-tested + same-host lab capture.** Native rustls TLS and optional mTLS for both listeners; HTTPS witness/peer clients with private-CA verification and no silent downgrade; a non-loopback plain-HTTP listener is refused unless `allow_insecure = true`; scoped admin/operator/read-only credentials with constant-time comparison; read authorization when scoped credentials exist; structured audit attribution. Tests in [volvisor-tls](crates/volvisor-tls), [volvisor-api](crates/volvisor-api), and [volvisord](crates/volvisord); same-host netns/veth capture in [docs/evidence/transport-tls](docs/evidence/transport-tls/2026-11-02-devhost-netns/). | Loopback dev; single host with native TLS; documented multi-host shape with HTTPS witness/peer and optional mTLS. | The recorded packet capture is **same-host** netns/veth (non-loopback addresses, one kernel), not a second physical host or a cross-host witness/peer hop. The bundle records a dirty-tree run at base `a6591d9`. Live certificate rotation, HSM/TPM key storage, external terminators, DoS resistance, and identity management are **not run**. The legacy `admin_token`-only mode still leaves reads open; any shared network must configure scoped credentials and TLS ([security guide](docs/security/deployment.md)). **No production support.** |
| Distribution / packaging | `volvisord`, `volvisor-witnessd`, systemd units, deb/rpm | **Prototype.** Local release build, package-version mapping, `--check-config`, deb/rpm build, and container smoke are recorded in the [v0.1.0 checklist](docs/releases/v0.1.0-checklist.md); CI package jobs are wired. | x86_64-unknown-linux-gnu on Debian/Ubuntu and EL/Fedora; one package ships both daemons; units are not auto-enabled. | **No signed tag or GitHub release exists; the distribution tag is blocked in this workstream by the missing signing key** (see checklist). Packaged unit start, upgrade/downgrade transitions, and non-x86_64 architectures are **not verified**. Package provenance is signed tags + `SHA256SUMS`, not package signatures. **No production support.** |
| CHV consumer integration | CellHV agent → node-local `volvisord` Volume API v2 | **Not run against a tagged Volvisor build.** CHV reports at most `advertised`; `active` requires guest confirmation and must never be claimed. | Node-local, loopback-only `volvisord` behind the CHV agent; CHV's class vocabulary is not mapped to Volvisor's. | The CHV pin has **not moved**: it predates Volvisor's attachment-state capability. [#48](https://github.com/o3kio/volvisor/issues/48) must land, and a **tagged Volvisor release must exist first**; until then CHV remains on its current pin. No live-migration support is claimed. **No production support.** |

## Required status statements

These statements are intentionally blunt; do not soften them in release
copy:

- **Nearline has only single-host Tier V evidence.** The three-physical-host
  campaign [#37](https://github.com/o3kio/volvisor/issues/37) is **not run**.
- **The Rook-cell POC has never been executed and there is no GO.** See
  [#38](https://github.com/o3kio/volvisor/issues/38) and
  [ADR-0008](docs/adr/0008-rook-only-hyperconverged-cells.md).
- **The real-backend release gate has landed; Ceph/DRBD are not claimed.**
  [#39](https://github.com/o3kio/volvisor/issues/39) added
  [release-scope.toml](release-scope.toml) and the
  [real-backends gate](docs/testing/real-backends.md). `native-local` is
  `supported` and gated by the LVM suite; `ceph-rbd` and
  `nearline-replicated` are `not-run` and explicitly **not claimed**. No
  green Ceph/DRBD run exists, and publish is blocked if a provider is
  flipped to `supported` without a passing suite.
- **CHV reports at most `advertised`, and its Volvisor pin has not moved.**
  See [#48](https://github.com/o3kio/volvisor/issues/48); a pin is only
  considered after a tagged Volvisor release exists.
- **Transport/auth hardening landed in #40, but the capture is same-host.**
  Native TLS/mTLS, HTTPS witness/peer clients, refusal of non-loopback plain
  HTTP without `allow_insecure`, scoped admin/operator/read-only credentials,
  read authorization and audit attribution are implemented and CI-tested
  ([#40](https://github.com/o3kio/volvisor/issues/40),
  [security guide](docs/security/deployment.md)). The recorded packet capture
  is a **same-host netns/veth pair** on one kernel, not a second physical host
  or a cross-host witness/peer hop
  ([bundle](docs/evidence/transport-tls/2026-11-02-devhost-netns/)). The
  legacy `admin_token`-only mode still leaves reads open; a shared network
  must configure scoped credentials and TLS. Cross-host validation remains
  part of the real-host campaigns and the exit review.
- **No tag or GitHub release exists.** The v0.1.0 distribution tag is
  blocked in this workstream by the missing signing key; the separate
  pre-production exit gate remains unmet and is recorded in the
  [readiness report](docs/releases/pre-production-readiness.md) and
  [checklist](docs/releases/v0.1.0-checklist.md).

## Evidence index

| Evidence | What it proves | What it does not prove |
|---|---|---|
| [Tier V run, 2026-10-11 (CH v51.1)](docs/evidence/tier-v/2026-10-11-devhost-v51/) | Four bounded mobility/resize scenarios against a real VMM at recorded version and SHA256; the v51.1 `vm.resize-disk` notification landed. | Real storage, real DRBD, real media durability, multiple physical hosts, production support. |
| [Tier V run, 2026-10-10 (CH v43.0.0)](docs/evidence/tier-v/2026-10-10-devhost/) | The same bounded scenarios on the older VMM; records the absent `vm.resize-disk` route and the fail-closed retry branch. | Anything about v43 being suitable for grow notification, or any real-host durability. |
| [Campaign report (Tier S/R/V)](docs/evidence/tier-v/2026-10-11-devhost-v51/REPORT.md) | 103 records at commit `1762a9d`: 91 Tier S, 8 Tier R gate records (explicitly **skipped**), 4 Tier V. The release workflow is designed to re-emit the evidence and certification at the tagged commit. | The skipped Tier R gates are **not** passes; no production durability. The report is not evidence for a later commit unless the release certification reruns it. |
| [Transport/TLS same-host run, 2026-11-02](docs/evidence/transport-tls/2026-11-02-devhost-netns/) | The rustls listener with a private CA, untrusted-certificate rejection, refusal of a non-loopback plain-HTTP bind, an encrypted pcap with no bearer token/path/header plaintext, and audit attribution. | A second physical host, cross-host witness/peer TLS, live certificate rotation, HSM/TPM, external terminators, or multi-host deployment. The bundle records a dirty-tree run at base `a6591d9`, not a clean-SHA multi-host result. |
| [ADR-0010](docs/adr/0010-tier-v-real-vmm-verification.md) | The exact Tier V claim vocabulary: "verified against a real VMM process at recorded versions". | Production support or real-media evidence. |
| [POC design and GO/NO-GO](docs/poc/rook-cells/README.md) | The pre-registered C01–C14 criteria for a future run. | Any executed Rook/Ceph result; no GO exists. |
| [release-scope.toml](release-scope.toml) and [real-backends gate](docs/testing/real-backends.md) | Which providers this release claims, and that a missing, skipped, or failed suite for a `supported` provider blocks publish. | A suite result, or any Ceph/DRBD evidence. `ceph-rbd` and `nearline-replicated` are `not-run` non-claims. |
| [Day-2 drill bundle, 2026-11-03](docs/evidence/day2/2026-11-03-devhost/REPORT.md) | Single-host backup/restore, journal/state corruption, ENOSPC, crash-loop/lock and restart-based credential rotation drills, with recorded output and honest RPO/RTO. | Multi-host host loss (D8), live binary upgrade/rollback, storage-side DRBD fencing, or any production recovery guarantee. The bundle predates #42/#45/#44 and does not certify them. |
| [Control-plane benchmark bundle, 2026-11-03](docs/benchmarks/2026-11-03-devhost/REPORT.md) | Single-host loop-backed LVM control-plane throughput/latency at 1/8/32/128 in-flight, admission/rate-limit probes, a 25-minute soak, and restart replay. | Data-path performance, real Ceph/DRBD/NVMe numbers, a 24-hour soak, production capacity, or any SLA. RSS and journal growth in the soak are open findings (#64). |
| [v0.1.0 checklist](docs/releases/v0.1.0-checklist.md) and [readiness report](docs/releases/pre-production-readiness.md) | The distribution-tag gate, the separate pre-production exit gate, the gate-to-evidence mapping, and the local build/smoke commands and results. | That the tag is allowed; the distribution tag is blocked in this workstream by the missing signing key, and the exit gate remains unmet. |

## Claim rules for reviewers

1. A production-support claim requires the exact implementation source,
   hardware, VMM/backend versions, and real-host failure evidence for that
   class (AGENTS rule 12; SPEC-0002 §11).
2. A benchmark is not a durability proof; a happy-path migration is not a
   split-brain proof.
3. `not run` is never a pass. Tier R gate records marked `skipped` are not
   evidence of the gate.
4. Tier V evidence is single-host and uses simulated storage; it does not
   close Tier R.
5. No row in this matrix authorizes a tag, a GitHub release, a CHV pin
   update, or a production deployment.
