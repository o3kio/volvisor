# Production-readiness closure plan (2026-11-06)

Status: working plan. This document sequences the work needed to move the five
pre-production gates in [#36](https://github.com/o3kio/volvisor/issues/36)
from their recorded verdicts to supported verdicts. It is **not evidence**, it
does not change a gate, and it does not convert an accepted ADR into a proven
implementation.

Related: [pre-production readiness](../releases/pre-production-readiness.md),
[open-issue remediation](2026-11-06-open-issue-remediation.md),
[nearline multi-host campaign](../runbooks/nearline-multihost-campaign.md),
[real-backend suites](../testing/real-backends.md),
[Rook-cell POC](../poc/rook-cells/README.md),
[v0.1.0 checklist](../releases/v0.1.0-checklist.md),
[evidence standards](../evidence/README.md),
[default integration scenarios](../testing/default-integration-scenarios.md),
[ADR-0013](../adr/0013-durable-attachment-generation-allocator.md),
[ADR-0014](../adr/0014-journal-bounds-and-exhaustion-contract.md),
[ADR-0015](../adr/0015-write-path-critical-section-invariant.md),
[ADR-0016](../adr/0016-semantic-change-and-default-scenario-gate.md).

## Baseline (2026-11-06, `3fa984a`)

| Gate | Verdict | What is missing |
|---|---|---|
| 1 Evidence | **NOT MET** | Three-physical-host campaign (#37), Rook-cell C01–C14 (#38), green Ceph/DRBD suites in a gating rig (#39). |
| 2 Release | **NOT MET** | Signing key, signed `v0.1.0` tag, GitHub release, clean-host install from tagged deb/rpm, support matrix matching evidence (#47). |
| 3 Security | **PARTIAL** | Cross-host witness/peer TLS capture, live credential rotation, HSM/TPM decision. Same-host netns/veth capture exists. |
| 4 Operability | **PARTIAL** | D3 storage-side DRBD fencing, D8 total host loss, rolling multi-host and packaged upgrade/rollback. D1/D2/D4/D5/D6/D7 are drilled single-host. |
| 5 Consumer | **NOT MET** | CHV-side change and a pin to a tagged Volvisor release; CHV ceiling stays `advertised` (#48). |

Local durable-semantics work landed at `3fa984a`: ADR-0013 schema-v4
provider-wide attachment-generation allocator (#63) and ADR-0014 hard journal
ceiling plus cannot-fit refusal (#64). Both issues stay open for the residuals
and evidence below. The default gate is 58 exact scenarios, 60 s/test and
300 s warm total.

## 1. Gate closure definitions

A gate closes only at one exact commit with a clean worktree and a published,
reviewed evidence bundle. "Documentation says so", a same-host substitute, a
simulation, or a distribution tag does not close a gate.

| Gate | Exact closure evidence |
|---|---|
| 1 Evidence | `docs/evidence/nearline/<date>/REPORT.md` + manifest for N0–N11 (including N11 24 h soak); `docs/evidence/rook-cell/<date>/` with C01–C14 and a signed GO/NO-GO; green `ceph` and `drbd` real-backend runs at the same SHA, with `release-scope.toml` flipped to `supported` only after those runs. |
| 2 Release | Signed annotated `v0.1.0` tag; GitHub release with deb, rpm, tarball, `SHA256SUMS` and release notes built by `release.yaml`; clean-host install and service start from those artifacts; support matrix containing only claimed providers. |
| 3 Security | Cross-host TLS/mTLS capture on separate physical hosts including witness and peer hops; untrusted-certificate and plain-listener refusals; no cleartext bearer token in the capture; live credential rotation drill; recorded HSM/TPM decision. |
| 4 Operability | Drilled D3 storage-side DRBD fencing, D8 total host loss/BMC loss, and a rolling multi-host plus packaged upgrade and rollback, each with a report and honest RPO/RTO bounds. |
| 5 Consumer | CHV-side integration change merged in the CHV repository; capability negotiation and `advertised` report against a pinned tagged Volvisor build; no `active` without guest evidence; pin and version matrix recorded. |

## 2. Workstreams

### W0 — close locally-actionable durable-state and contract gaps

No hardware required. This work must not weaken a gate or add a production
claim; every durable-semantics change still needs an ADR/amendment, a
compatibility and rollback statement, a fail-closed path and an exact default
scenario (ADR-0016).

| Item | Deliverable | Exit criterion |
|---|---|---|
| #62 state substrate | ADR/amendment for per-record or segmented durable state with one single-writer commit protocol; migration and rollback; implementation; scenarios for crash and concurrent create/delete/grow. | No whole-file rewrite/fsync or single provider-state lock on the measured write path; bounded tail latency and journal/RSS growth measured, not asserted; default gate green. |
| #63 residual | Pre-upgrade control: runbook step requiring a drain and no volume-id reuse across the v3→v4 boundary, or an owner-accepted residual with its precise risk. Real-backend evidence for the allocator. | Residual recorded with an explicit acceptance, or eliminated by a compatible design; #63 evidence boxes link exact runs. |
| #64 growth | 24 h soak on a real backend with journal/RSS/state growth limits; ceiling and cannot-fit behavior exercised under load; thresholds documented. | Soak report at an exact SHA shows no unbounded growth and no unreported refusal; the ceiling refusal is observed or explicitly not triggered with rationale. |
| #45 | Contract amendment for a resumable long-operation handle and distinct per-route mutation timeouts; implementation and default scenarios. | Amendment accepted; handle and timeouts implemented with fail-closed semantics and tests. |
| #49 | Keep corpus/interleaving/nightly jobs green; publish the flake-rate report. | 14 consecutive nightly runs with a recorded, published flake rate (cannot be compressed). |
| #50 | `ARCHITECTURE.md`, `CODEOWNERS`, contributor guide and review checklist. | Documents reviewed; the two-maintainer sign-off remains pending until W3/W5. |

Sequencing: the #62 design and the #45 amendment can start immediately and in
parallel. Implementation of #62 must land before the 24 h scale/soak claim is
credible, because the measured state-lock serialization is exactly what the
soak is meant to bound.

### W1 — provision the evidence rig (external)

This is the long pole. It is procurement and operations work, not a code
change.

Required topology, from the campaign runbook:

- Three physical hosts A/B/C in distinct power/network failure domains, plus a
  witness W in a fourth.
- A and B each with a dedicated sacrificial NVMe/LV for the nearline backing
  volume; BMC or documented power control for unplanned loss; network fault
  injection.
- A shared snapshot directory outside the data path; DRBD port 7100 and
  witness port 9101 reachable as documented.
- A Ceph cluster with a dedicated test pool and least-privilege client
  keyring (Volvisor is an RBD adapter, never an OSD manager).
- Three storage microVMs on three physical hosts with real VFIO and
  host-domain CRUSH placement for the Rook cell, not three VMs on one chassis.
- Cloud Hypervisor source/destination hosts and guests with recorded versions
  and image digests.
- Self-hosted runners labelled `volvisor-ceph` and `volvisor-drbd`, with the
  documented repository variables and secrets.

Deliverable: `bash scripts/preflight-nearline.sh
/etc/volvisor/nearline-inventory.env` and `scripts/preflight-rook.sh
/etc/volvisor/rook-cell-inventory.env` green at the target SHA, plus a
recorded inventory and runner registration. The completed inventory is never
committed.

Exit criterion: preflight green; all rig versions and failure domains
recorded; no "equivalent topology" claimed without the documented deviation,
risk and accepting operator.

### W2 — execute Gate 1 evidence

| Issue | Run | Evidence |
|---|---|---|
| #37 | N0–N11 from the campaign runbook, including N11 24 h soak, at one exact SHA. | Curated `docs/evidence/nearline/<date>/` with report, raw session log, oracle, per-scenario decisions, manifest. |
| #38 | C01–C14 Rook-cell matrix. | Curated `docs/evidence/rook-cell/<date>/`, per-criterion results, signed GO/NO-GO or NO-GO. |
| #39 | `./scripts/test-ceph.sh` and `./scripts/test-drbd.sh` on the registered runners at the same SHA. | Green CI run; then flip `release-scope.toml` providers to `supported`; a scheduled green run (not only a manual dispatch). |
| #43 | Real-backend 24 h soak and credible throughput/p95 numbers. | Benchmark report with exact hardware, versions, workload, errors, capacity and soak growth; no SLA claim. |
| #44 | Real migration trace and 24 h soak observation. | Trace bundle validated with `scripts/validate-traces.py`; health/latency evidence at the same SHA. |

Exit criterion: gate 1 closes at one SHA; every scenario is PASS or an
explicitly recorded NOT_RUN with a blocking issue, and a required NOT_RUN keeps
the gate NOT MET.

### W3 — close Gate 2 (release)

1. Configure an offline or HSM-backed signing key in a permitted release
   environment; never create the tag from an unkeyed host.
2. Run `release.yaml` by `workflow_dispatch` on the release candidate; confirm
   the real-backend matrix matches `release-scope.toml`.
3. Verify the release commit: full workspace suite, clippy, fmt, docs, supply
   chain, MSRV, LVM suite and every `supported` provider suite.
4. Create the signed annotated tag, verify it, push it, and let the release
   workflow publish the artifacts, `SHA256SUMS`, notes and support matrix.
5. Install the tagged deb and rpm on a clean host, start the packaged unit,
   run `--check-config`, and record the install/start evidence.
6. Record the support matrix: only providers with a green exact-SHA suite are
   claimed; `not-run` providers stay explicit non-claims.

Exit criterion: gate 2 closes. A distribution tag alone does **not** close
gate 1 or #36.

### W4 — close Gate 3 (security)

- Repeat the transport capture with the witness and peer on separate physical
  hosts, not netns/veth on one kernel.
- Prove HTTPS/mTLS for witness and peer hops, untrusted-certificate refusal,
  plain non-loopback refusal without `allow_insecure`, and no cleartext bearer
  token or private key in the capture.
- Drill live credential rotation across the daemon, witness and peer.
- Record the HSM/TPM decision (adopt, or accept software-key storage with the
  exact risk and compensating controls).

Exit criterion: gate 3 closes with the cross-host capture and rotation record.

### W5 — close Gate 4 (operability)

- D3: witness loss with storage-side DRBD fencing, not only service
  outage/restart.
- D8: total host loss on A or B using real power/BMC loss; re-admission and
  recovery recorded; no dual writer.
- Rolling multi-host upgrade and packaged upgrade/rollback, including the
  schema v4 path and the future-version refusal.
- Re-run D1/D2/D4/D5/D6/D7 at the release commit if the release changes
  packaging or durable state.

Exit criterion: gate 4 closes with drilled reports and honest RPO/RTO bounds;
every `NOT_RUN` is a blocking issue, not a waiver.

### W6 — close Gate 5 (consumer)

- Make the CHV-side change in the CHV repository: negotiate the capability,
  report `advertised`, and refuse `active` without guest evidence.
- Pin CHV to the tagged Volvisor build from W3; record the version matrix and
  integration test.
- Keep the CHV ceiling at `advertised`; a pin update is a separate reviewed
  change in that repository, never a docs edit here.

Exit criterion: gate 5 closes with the CHV change merged and the pin recorded.

### W7 — exit review (#36)

- Re-run every gate at the exact release commit.
- Produce the gate-to-evidence report: each gate verdict, the exact bundle and
  SHA, and every unmet item as **not met**.
- Update `docs/releases/pre-production-readiness.md`, the support matrix and
  the release notes; record residual risks and accepted exceptions.
- No waiver, no substitution, no "known flake" waiver.

## 3. Sequence and milestones

```text
W0 design (#62 ADR, #45 amendment, #50 docs, #49 nightly clock)
        |
        +--> W1 rig provisioning + preflight --------+
        |                                            |
        |                                            v
        |                                  W2 Gate 1 runs (#37/#38/#39/#43/#44)
        |                                            |
        +--> #62 implementation ---------------------+
                                                     |
                                                     v
                                          W3 release dry run -> signed tag (#47)
                                                     |
                             +-----------------------+-----------------------+
                             v                                               v
                   W4 Gate 3 cross-host security                   W5 Gate 4 drills
                             |                                               |
                             +-----------------------+-----------------------+
                                                     v
                                          W6 CHV change + pin (#48)
                                                     |
                                                     v
                                          W7 exit review (#36)
```

| Milestone | Entry | Exit |
|---|---|---|
| M0 local gaps | W0 design started | #62 ADR and #45 amendment accepted; default gate green; #63 residual control recorded. |
| M1 rig ready | Hardware available | Both preflights green; runners registered; versions recorded. |
| M2 Gate 1 | M1 | Campaign + Rook + Ceph/DRBD + 24 h soak evidence at one SHA; gate 1 verdict updated. |
| M3 release candidate | M0 + W2 claimed suites green | Release dry run green; signed tag and clean-host install recorded; gate 2 verdict updated. |
| M4 security/operability | M1 + M3 | Cross-host capture, rotation, D3/D8/rolling upgrade drilled; gates 3/4 verdicts updated. |
| M5 consumer | M3 tag exists | CHV change merged and pinned; gate 5 verdict updated. |
| M6 exit review | M2–M5 | #36 gate-to-evidence report; no unmet gate waived; final verdict recorded. |

W4 and W5 can overlap W2 once the hosts exist. W6 cannot start before M3.
#49's 14-night clock cannot be compressed, but it can start only after the
campaign-flake workflow is on the default branch. The workflow is currently
branch-only, so PR #73 must merge first; it cannot start at M0 as a local
action.
#43's 24 h soak belongs to W2 and may be repeated at the release candidate if
the release changes the measured path.

## 4. Issue-to-workstream map

| Issue | Workstream | Gate |
|---|---|---|
| #37 nearline campaign | W1, W2 | 1 |
| #38 Rook cell | W1, W2 | 1 |
| #39 real-backend rig | W1, W2 | 1 |
| #43 benchmarks/soak | W2 | 1, 3 |
| #44 observability trace | W2 | 1, 4 |
| #46 day-2 drills | W5 | 4 |
| #47 signed release | W3 | 2 |
| #48 CHV pin | W6 | 5 |
| #49 fuzz/flake | W0 | — (assurance) |
| #50 knowledge | W0, W3, W5 | — (support) |
| #62 state substrate | W0 | 1 (scale/soak credibility) |
| #63 allocator residual | W0, W2 | 1 |
| #64 journal bounds | W0, W2 | 1, 4 |

## 5. Evidence standard

Every closing bundle must satisfy [the evidence standards](../evidence/README.md):

- exact `git rev-parse HEAD` and a clean worktree (`git status --porcelain`
  empty); a dirty tree cannot support a clean-SHA claim;
- exact versions of the daemon, witness, kernel, DRBD module/utilities,
  Cloud Hypervisor, guest image and fio;
- operator identity, roles and failure domains; any topology deviation with
  its risk and accepting operator;
- raw command/output/exit-status logs, inspect and authority output, oracle
  verdicts and fault windows;
- a manifest with hashes; no measured JSON/CSV edited after the run; repeats
  and diagnostics labeled non-evidence;
- secrets redacted at curation time, never on a command line;
- curated under `docs/evidence/` only after a real run, with the CI artifact
  retained for its 90-day window and copied into the permanent bundle.

## 6. Risks and stop rules

- **Hardware is the critical path.** If the rig is not provisioned, gates 1,
  3 and 4 stay NOT MET. Single-host, loop-LVM, simulated storage or three VMs
  on one chassis are not substitutes.
- **#63's upgrade window is not fully closable from a v3 file.** A volume
  deleted before the v4 file was written has no durable generation record.
  Close it only by an explicit operator control (drain and no volume-id reuse
  across the boundary) or an owner-accepted residual; never call it fixed.
- **#62 changes durable semantics.** It needs an accepted ADR/amendment,
  migration and rollback, a fail-closed path and default scenarios before
  merge. Do not hide it in a performance change.
- **Release scope is a claim.** Flipping `ceph-rbd` or `nearline-replicated`
  to `supported` without a green suite is blocked by design; do not bypass the
  gate to make a release.
- **A distribution tag is not pre-production readiness.** Gate 2 and #36 are
  separate decisions; do not use one to close the other.
- **The 24 h soak and 14-night flake rate cannot be shortened.** They run in
  the evidence/scheduled lanes, not the default catalog.
- **CHV is a separate repository.** Do not edit or pin it from this
  workstream without its own reviewed change.
- **No silent waiver.** A required scenario that is NOT_RUN keeps its gate
  NOT MET and is linked to a blocking issue.

## 7. Immediate next actions

1. Draft the #62 state-substrate ADR/amendment and the #45 contract amendment
   for review; both unblock later work and neither needs hardware.
2. Publish the rig request: three physical hosts + fourth-domain witness,
   BMC/power control, NVMe, Ceph test pool/keyring, DRBD peer, three cell
   hosts with VFIO, and the two self-hosted runner labels.
3. #50's structural documentation slice is already delivered on this branch;
   #49's nightly clock starts only after the campaign-flake workflow reaches
   the default branch (PR #73 must merge first).
4. Run both preflights as soon as the rig exists; fix every failure before
   scheduling any campaign scenario.
5. Only then start W2, W3 and the 24 h soak, in that order.

## 8. Non-claims

This plan is not production-support evidence, an SLA, a zero-RPO or durability
guarantee, a capacity or throughput claim, a hardware or multi-host
qualification, a release approval, or a waiver. Gate verdicts remain
**1/2/5 NOT MET**, **3/4 PARTIAL**, overall **NOT PRE-PRODUCTION READY** until
the exact evidence above lands and the exit review records a new verdict.
