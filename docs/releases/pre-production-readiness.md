# Pre-production readiness report — 2026-11-03

Status: **NOT PRE-PRODUCTION READY.** Two of the five gates in
[#36](https://github.com/o3kio/volvisor/issues/36) are partially evidenced;
three are unmet. This document maps every gate to the evidence that exists
and names what does not. It is a readiness record, not a release, a support
statement, or an SLA.

- **Basis:** `main` at `575acfc` (Wave 2 merged). CI run
  [41647509537](https://github.com/o3kio/volvisor/actions/runs/41647509537)
  is green for `check`, `msrv`, `lvm-integration`, `supply-chain`,
  `package-build`, `package-smoke` (Debian/Fedora) and `test-harness`.
- **Tag / release:** **none**. `git tag --list` is empty and no GitHub
  release exists.
- **Claim scope:** [`release-scope.toml`](../../release-scope.toml) marks
  only `native-local` as `supported`; `ceph-rbd` and
  `nearline-replicated` are `not-run` and explicitly **not claimed**. The
  `native-local` suite is a loop-device gate, not real-NVMe qualification.

## Gate verdicts

| # | Gate | Verdict | What is missing |
|---|---|---|---|
| 1 | **Evidence** — three physical hosts at an exact SHA with the fault matrix and a published bundle; Rook-cell GO/NO-GO; Ceph and DRBD suites in a gating rig | **NOT MET** | No three-physical-host campaign (#37), no Rook-cell execution (#38), no green Ceph/DRBD rig run (#39). Only single-host Tier V with simulated storage, single-host day-2 drills, and single-host loop-LVM benchmarks exist. |
| 2 | **Release** — signed `v0.1.0` (or later) with deb/rpm, SHA256SUMS, install smoke, and a support matrix matching the evidence | **NOT MET** | No signing key on this host, no signed tag, no GitHub release, and no clean-host install from a tagged artifact (#47). The support matrix itself is accurate and makes only non-claims. |
| 3 | **Security** — explicit transport/auth model for non-loopback deployment; authenticated/encrypted peer and witness traffic; scoped credentials and audit identity | **PARTIAL** | #40 landed native TLS/mTLS, HTTPS witness/peer clients, refusal of plain non-loopback HTTP without `allow_insecure`, scoped credentials, read authorization and audit attribution. The recorded capture is **same-host netns/veth only**; cross-host witness/peer hops, live rotation, and HSM/TPM are not run. Legacy `admin_token`-only mode still leaves reads open. |
| 4 | **Operability** — backup/restore, upgrade/rollback, witness loss, and journal corruption drilled with recorded results | **PARTIAL** | D1 backup/restore, D4 journal corruption, D5 ENOSPC, D6 crash loop/lock and D7 restart-based credential rotation were drilled on one host. D2 upgrade/rollback is partial (schema policy only; no live binary transition). D3 witness loss is partial (service outage/restart only; no storage-side DRBD fencing). D8 total host loss is **not run**. |
| 5 | **Consumer** — CHV negotiates the capability and reports `advertised` against a pinned release, never `active` without guest evidence | **NOT MET** | No CHV repository change and no pin update. A pin requires a tagged Volvisor release, which does not exist. CHV's ceiling remains `advertised`; no `active`/guest-confirmed behavior is claimed. |

**Verdict:** the Wave 2 P1 hardening work landed, but the five-gate definition
is unchanged and still requires P0 real-hardware, release and consumer work
that this environment cannot produce. Do not describe this tree as
pre-production, production-ready, or a release candidate.

## Wave 2 delivery record

| Workstream | State | Merge / evidence | Limits that remain |
|---|---|---|---|
| #41 schema evolution | Landed | `712afb7` | — |
| #40 transport/auth | Landed | `ad9efb0` | Same-host capture; cross-host validation open |
| #42 provider consolidation | Landed | `db04182` | Parity suite uses simulated Ceph/DRBD worlds; no real clusters |
| #45 admission control | Landed | `0c853f3` | No resumable operation handle/long-poll; one uniform read timeout, not per-route values; rate limiting is opt-in |
| #46 day-2 runbooks | Landed | `8de06c2`; [bundle](../evidence/day2/2026-11-03-devhost/REPORT.md) | 5 drilled, 2 partial, 1 not run; single host; bundle source `e551d26` predates #42/#45/#44. D5 filed #58 |
| #44 observability | Landed | `5017ea0`; [docs](../operations/observability.md) | State-save duration and lease/epoch gauges deliberately omitted; real migration trace **not run** |
| #43 benchmarks | Landed | `575acfc`; [bundle](../benchmarks/2026-11-03-devhost/REPORT.md) | Single host, loop-backed LVM, control plane only; 25.0 min soak, **not 24 h**; no real Ceph/DRBD/physical-disk LVM |
| #49 harness flake | Partial | `5037db9` | LVM mid-move abort flake fixed; fuzz targets, interleaving tests and nightly flake rate remain open |

The detailed workstream state is on
[#36](https://github.com/o3kio/volvisor/issues/36). The measured benchmark
source is `e3aa365`; it remains an ancestor of `main` for inspection.

## Evidence inventory

| Evidence | Source | Scope | Proves | Does not prove |
|---|---|---|---|---|
| [Tier V, CH v51.1](../evidence/tier-v/2026-10-11-devhost-v51/) | `1762a9d` | One physical development host; storage below the VMM seam simulated | Four bounded VMM scenarios at recorded VMM version/SHA256 | Real media, real DRBD, multiple hosts, production durability |
| [Tier V, CH v43.0.0](../evidence/tier-v/2026-10-10-devhost/) | recorded in bundle | One physical development host; simulated storage | The same scenarios on the older VMM; fail-closed grow-retry branch | Grow notification on v43, real-host durability |
| [Transport/TLS same-host](../evidence/transport-tls/2026-11-02-devhost-netns/) | base `a6591d9`, dirty tree | One kernel; netns/veth peers | TLS/mTLS, untrusted-cert rejection, insecure-bind refusal, encrypted pcap, audit attribution | Cross-host witness/peer hop, live rotation, HSM/TPM |
| [Day-2 drills](../evidence/day2/2026-11-03-devhost/REPORT.md) | `e551d26`, clean worktree | One host | D1/D4/D5/D6/D7 drilled; 181-entry manifest verifies | D2/D3 full paths, D8 host loss, live upgrade/rollback, DRBD fencing, multi-host |
| [Control-plane benchmarks](../benchmarks/2026-11-03-devhost/REPORT.md) | `e3aa365`, clean worktree | One host, loop-backed LVM, loopback HTTP | 1/8/32/128 sweep, 25-min soak, restart replay, 46-entry manifest verifies | Data path, real backends, 24 h soak, production capacity or SLA |
| CI real-LVM gate | `575acfc` | GitHub-hosted runner, loop device | `native-local` suite: 6/6 integration tests, unit suites, clippy/fmt | Real NVMe, Ceph, DRBD, multi-host |

The benchmark bundle records 1/8/32/128 in-flight throughput of
**1665.787 / 2293.7 / 899.759 / 241.716 ops/s** with p95 **1.77 / 6.454 /
191.244 / 1251.564 ms** and zero errors. The shortened 25.0-minute soak ran
1,202,500 operations at 801.656 ops/s; RSS grew **+607.1 MiB**, the journal
**+215.0 MiB**, provider state **+2.4 MiB**, and fds stayed flat (19 → 18).
Those growth signals are filed as
[#64](https://github.com/o3kio/volvisor/issues/64); the global state
lock/fsync serialization is [#62](https://github.com/o3kio/volvisor/issues/62);
retired attachment accumulation is
[#63](https://github.com/o3kio/volvisor/issues/63). None of these is a
solved production issue.

## Exit-review checklist

The exit review in #36 requires a full re-run at the release commit and a
report mapping every gate to evidence. Current status:

| Item | Status |
|---|---|
| `cargo fmt --all --check` | **PASS** in CI `check` at `575acfc` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | **PASS** in CI `check` |
| `cargo test --workspace --locked` | **PASS** in CI `check` |
| `cargo doc --locked --no-deps --document-private-items` | **PASS** in CI `check` (with `RUSTDOCFLAGS=-D warnings`) |
| Supply chain (`cargo-deny`/audit job) | **PASS** in CI `supply-chain` |
| MSRV 1.85 | **PASS** in CI `msrv` |
| Real-LVM suite | **PASS** on a loop device in CI `lvm-integration` |
| Real-Ceph and real-DRBD suites | **NOT RUN** — no rig, no green run |
| Campaign suite | **NOT RUN** — no multi-host hardware |
| Clean-host install smoke from a tagged/signed artifact | **NOT RUN** — no tag exists; CI container smoke is not this check |
| Gate-to-evidence report | **This document** |
| Unmet gates recorded, none waived | **Yes** — gates 1, 2 and 5 not met; 3 and 4 partial |

## Blockers, in order

1. **#47 — signed release and clean-host install.** No signing key is
   configured; no tag or GitHub release exists; the documented local
   deb/rpm/container smoke predates Wave 2 and is not a tagged-artifact
   install.
2. **#48 — CHV consumer path and pin.** Requires a tagged release first,
   then a CHV-side capability negotiation, integration test, version matrix
   and conscious pin update. CHV must stay at or below `advertised`.
3. **#37 / #38 / #39 — real hardware and gating rig.** Three physical hosts,
   a fourth witness failure domain, a Rook-cell execution with C01–C14
   GO/NO-GO, and green Ceph/DRBD suites. None exists.
4. **#46 — D8 host loss and the undrilled halves.** No second host, BMC or
   fencing path; no live old→new binary upgrade/rollback; no storage-side
   DRBD fencing.
5. **#43 — credible backend numbers and 24 h soak.** Single-host loop-LVM
   only; no real Ceph/DRBD/physical-disk numbers and no 24 h run.
6. **#49 / #58 / #62 / #63 / #64 — assurance and real findings.** Fuzzing and
   interleaving tests are not implemented; the ENOSPC state-save leak is fixed
   in code by [PR #69](https://github.com/o3kio/volvisor/pull/69) with a
   real-LVM regression test (`#58`), while the measured
   lock/journal/retirement growth remains an open finding. None is a resolved
   production issue.

## What must not be claimed

- Pre-production readiness, production support, an SLA, a zero-RPO claim, or
  a durability guarantee beyond the recorded single-host evidence.
- A signed tag, GitHub release, or clean-host install: none exists.
- Real Ceph, real DRBD, real NVMe, or multi-host durability: none was run.
- A 24-hour soak or flat long-run memory: the soak was 25 minutes and RSS
  grew.
- CHV `active`/guest-confirmed attachment state or a moved CHV pin: neither
  exists.
- That loop-device benchmark numbers are production capacity.

## Reproduce the checks

```bash
git checkout 575acfc
cargo fmt --all --check
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
cargo doc --locked --no-deps --document-private-items
sudo ./scripts/test-lvm.sh
python3 scripts/release-scope.py check
(cd docs/benchmarks/2026-11-03-devhost && sha256sum -c MANIFEST.sha256)
(cd docs/evidence/day2/2026-11-03-devhost && sha256sum -c MANIFEST.sha256)
```

Ceph and DRBD suites remain `not-run`; running them without a provisioned
rig is not a substitute for evidence.
