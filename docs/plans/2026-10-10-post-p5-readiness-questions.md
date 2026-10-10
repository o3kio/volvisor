# Post-P5 implementation readiness — five questions, their answers, and the document changes they require

Status: normative for the post-P5 phases (this document answers the five
readiness questions with repo evidence, records the decisions, and enumerates
the ADR/SPEC/contract changes + the implementation plan that follow from them)
Date: 2026-10-10
Builds on: [P5 plan](2026-10-10-p5-aggressive-failure-campaign.md) (PRs
#11–#16, complete), [P4b plan](2026-10-09-p4b-vmm-storage-handoff.md)
(PRs #9–#10), [P3 plan](2026-10-09-p3-drbd-nearline-baseline.md) (PR #5),
[SPEC-0002](../../docs/specs/SPEC-0002-volvisor-volume-virtualization.md)
§12 (implementation sequence), [ADR-0006](../adr/0006-online-resize-and-live-local-block-relocation.md),
[ADR-0008](../adr/0008-rook-only-hyperconverged-cells.md), AGENTS rules
4/11/12/13.

P0–P5 are implemented and merged (main at `a6bf632`: 855 tests, the campaign
certification `CG1–CG5` green, no production-support claims anywhere). This
document is the honest readiness review the next phases start from: each of
the five questions below is answered with file-level evidence, each answer
ends in a decision, and §7 maps every decision to the normative documents
that must change before implementation begins. The implementation plan
itself (staged, gated, PR-mapped) follows in
[the post-P5 implementation plan](2026-10-10-post-p5-implementation-plan.md).

## 1. Are all implementation plans finished? What remains?

**Answer: the SPEC-0002 §12 sequence is finished through P5; two sequence
items and four recorded backlog items remain.**

Done and merged:

| Phase | Deliverable | Evidence |
|---|---|---|
| P0/P1 | Control plane, journal, provider abstraction, LVM provider, Volume API v2 | PR #3 |
| P2 | External-cluster Ceph RBD adapter | PR #4 |
| P3 | DRBD 9 nearline baseline (protocol, lifecycle, suspend-io, resync) | PR #5 |
| P4a | Witness, leases, fencing, adoption | PRs #6–#7 |
| P4b | VMM/storage handoff: barrier state machine, coordinator, peer routes, `ch-remote` adapter, e2e matrix | PRs #8–#10 |
| P5 | Aggressive failure campaign: oracle, harness, 59-cell kill matrix, injections, certified evidence | PRs #11–#16 |

Remaining from SPEC-0002 §12 as written:

1. **P5's consideration item** — "compare Mayastor/io_uring/SPDK
   alternatives; only build a new engine when justified"
   ([ADR-0007](../adr/0007-drbd9-nearline-replication-provider.md) pins DRBD
   9 as the selected prototype engine). This is a *measured-justification*
   gate, not automatic work: it stays closed until a Tier-R/Tier-V
   benchmark exists to measure against (see §2 and §6 of this document —
   the packaging and real-VMM phases come first precisely so the
   comparison can be run on installable, real-VMM software).
2. **P6 (managed-Ceph OSD placement ADR + production gate)** — unstarted,
   and correctly so: SPEC-0002 gates it behind the Rook-cell POC outcome
   (§4 below) and a production-support decision that no evidence yet
   carries.

Recorded backlog (each pinned in its evidence record / commit message,
never silently dropped):

- the **`grant_set` wedge** — a witness kill inside the grant commit parks
  a migration safely at `destination_authorized` forever while the retry
  task spins at its 5 s tick (fail-closed stall; remedy is a design change:
  re-resolvable peer acts, or a promote path that does not route through
  the failed grant op);
- the **renewal-deadline flake** in `volvisor-drbd`
  (`an_unreachable_witness_defers_renewal_until_the_deadline`, ~1/20 full
  suites; the kit's `server.handle.abort()` does not drain in-flight
  renewals);
- the **local mirror** — named not-delivered in the nearline §10 note (no
  mirror implementation exists to fault);
- the **Tier R real-host drive** — the 8 gates are scaffolded, skipped
  honestly, and fail loudly under `VOLVISOR_CAMPAIGN_TIER=R`.

**Decision (D1):** the next phases are P6–P9 as defined in §6 (native
online operations, packaging/distribution, the real-VMM verification tier,
the Rook-cell device-sharing POC), with the old P6 (managed Ceph) renumbered
to P10 and kept last before any production gate. SPEC-0002 §12 is renumbered
accordingly. The P5 consideration item (new-engine comparison) stays gated
on P8's real-VMM benchmarks. The grant_set wedge fix is folded into the
nearline hardening slice of P6 (it blocks honest "migration completes under
witness faults" claims); the flake is fixed opportunistically in the first
phase that touches the kit.

## 2. Do we have test scenarios proving live migration with a VMM works on nearline-replicated?

**Answer: yes, extensively — against the real daemons, real HTTP, real
witness and a FAKE VMM; no tier exists that runs a REAL Cloud Hypervisor.
That gap is the next verification phase.**

What exists (all in the default test suite, 855 tests):

- **`crates/volvisord/tests/migration_e2e.rs`** — the end-to-end matrix over
  a real daemon pair: each side a real `volvisor_api::router` served by
  axum over TCP, a real loopback `volvisor_witness::server`, the real
  HTTP peer path between daemons, the real coordinator/handoff
  composition, and two `FakeVmm` instances sharing one snapshot root.
  `FakeVmm` instances sharing one snapshot root. The matrix rows 13–23
  (19 tests: happy path, snapshot faults, the crash-between-X cells,
  dead-source adoption, restore faults and coordinator re-drive,
  eligibility fencing, idempotency, partial-target promotion, disk-path
  rewrite) are fully end-to-end; fault cells inject at the `FakeVmm`
  knobs. This proves the migration *protocol* (prepare →
  transfer → pause → snapshot → barrier → peer grant → promote → restore
  → resume → complete) including its failure and recovery paths.
- **`crates/volvisor-provider/tests/vmm_tests.rs`** — the REAL `ch-remote`
  adapter (`ChRemoteVmm`) tested argv-exactly against a scripted
  `FakeRunner`: pause, info, snapshot, delete, restore (with real
  filesystem I/O for the `config.json` disk-path rewrite), resume,
  including the `--api-socket` convention and `file://` URL spelling.
- **The P5 campaign** (`crates/volvisor-campaign`) — the 59-cell generated
  kill matrix and rows 8–14 drive migrations under hard kills at every
  durable-mutation boundary, adversarial injections (stale writes, forged
  barriers, witness divergence, partitions, the 25-cycle abort storm),
  every recovery asserted against the full invariant set with the
  byte-ground-truth oracle. The certification (`campaign-summary --check`)
  is green.

What does NOT exist: any test that starts a **real Cloud Hypervisor
process** and drives the migration through real `ch-remote` against a real
VMM state machine (real pause timing, real snapshot files, real
restore-with-disk-rewrite, real resume). The seam is proven on both sides —
the adapter's argv against a scripted runner, the protocol against the fake
VMM — but not the two together. This is exactly the P5 plan's Tier R
discipline: "not run" must never read as "works".

**Decision (D2): P8 introduces Tier V (real-VMM verification)** — an
env-gated test tier (`VOLVISOR_TEST_VMM=1`) requiring a real `ch-remote`
binary and a real Cloud Hypervisor, extending the campaign's tier model and
evidence records (`tier: "V"`). Without the env var, every Tier V scenario
emits an explicit `skipped` gate record (the matrix shows the gate, never a
hole); with it set but the VMM absent, the tier fails loudly. The scenario
set is small and bounded: the migration happy path end-to-end, the
pause/snapshot kill windows against a real VMM, snapshot/restore divergence
detection, and the resize notification from P6 (§5). Normative home:
ADR-0010 (new) + a Tier V note in the nearline §10 implementation-status
ledger. Tier V does not claim production support — it claims "verified
against a real VMM process at recorded versions".

## 3. Do we have deep test scenarios including a Rook deployment, to see whether devices volvisor owns on the host can be shared with the operator?

**Answer: no. Nothing is implemented and nothing has been run; the Rook
cell mode is POC-only by contract, and the POC directory contains design
and example inputs only.**

Evidence:

- `docs/poc/rook-cells/` — README marked **"DESIGN + EXAMPLE INPUTS ONLY;
  not run"**: the three-physical-host topology, sacrificial NVMe
  controllers, exact-SHA version lock, example CephCluster/CephBlockPool
  manifests, the evidence-collection script.
- [rook-cell-experimental-v0](../../contracts/rook-cell-experimental-v0.md)
  — the contract's sections (distinct resource ownership, cell
  requirements, capacity semantics, fail-closed invariants, Rook
  interface, evidence, non-goals) are all requirements *on a future POC*,
  not descriptions of tested behavior.
- [ADR-0008](../adr/0008-rook-only-hyperconverged-cells.md) — "The Rook
  Cell mode remains POC-only until exact-SHA three-physical-host tests
  prove all lifecycle, quorum/CRUSH topology, disk/VFIO isolation,
  cleanup, CPU/memory budget, node scheduling/admission, fail/restart and
  benchmark gates."

The specific question — *can the devices volvisor owns on the host be
shared with the operator (Rook)?* — is not just untested, it is
**undesigned at the surface level**: volvisor's device model is built on
exclusive claims (WWN/serial-derived identities, explicit claiming under
destructive authorization, foreign state never adopted — AGENTS rule 7).
Rook needs the opposite shape for its OSDs: a device *deliberately
lent* to a cell guest under explicit authorization, with visible
ownership, reclaim semantics, and no double-ownership window. No
lending/delegation surface, no tests, no evidence exists.

**Decision (D3): P9 designs and proves the device-lending surface as a
POC**, in three ordered parts:

1. **The surface** (small, testable in Tier S without Rook): an explicit
   lend/reclaim operation on claimed devices — journal-before-lend, the
   lent state visible in the ownership record, refuse to mutate/lend a
   lent device, refuse to reclaim a device the borrower has not released
   (or a force path with operator authorization and a recorded residue
   check), fail-closed on unknown state. This is a contract change
   (rook-cell-experimental-v0 gains the device-sharing semantics section)
   and an ADR-0008 update — it does NOT touch the volume classes.
2. **The POC scenarios** (deep, per the user's question): the
   three-physical-host exact-SHA deployment of the existing POC design —
   Rook operator on the external control plane, one OSD per cell on
   volvisor-lent devices, then the contract's gate list: lifecycle,
   quorum under cell loss, disk/VFIO isolation, cleanup including lent
   device reclaim, admission, fail/restart, benchmarks. Evidence follows
   the P5 record discipline (a run directory + a report; "not run" is
   distinct from "not implemented").
3. **The gate**: POC-only status unchanged until the gates pass; nothing
   in this work authorizes production Rook support or the managed-Ceph
   (P10) path.

## 4. Can native-local migrate its underlying device to another device without interruption?

**Answer: not today — by contract in v0, and ADR-0006 (which designs it)
is Proposed / R&D-gated. Two of its three designed options are
implementable now; the third stays gated behind its own proof.**

Evidence:

- [volume-api-v2](../../contracts/volume-api-v2.md) / SPEC-0002: the
  `native-local` class pins live migration while attached in v0
  (`MIGRATION_UNSUPPORTED_LOCAL_STORAGE` is returned fail-closed).
- [ADR-0006](../adr/0006-online-resize-and-live-local-block-relocation.md)
  distinguishes the three operations and designs:
  - **Online grow**: LV growth is implemented (P0: `lvextend` + verified
    effective size), but the **VMM capacity notification step is not**
    (no `resize-disk` call exists anywhere in the crates; ADR-0006
    specifies Cloud Hypervisor `PUT /api/v1/vm.resize-disk`, which needs
    a pinned CHV release carrying upstream fix PR #7948).
  - **Same-host relocation, Option A (LVM `pvmove`, same VG)**: online by
    LVM's design — the dm identity stays stable, the VMM never pivots.
    Volvisor's job is orchestration + the safety contract (journal,
    verify, source extents freed only after verified relocation). Not
    implemented.
  - **Option B (QSD `blockdev-mirror` + pivot over vhost-user-blk)**:
    general per-volume pool-to-pool moves. Explicitly gated: "QSD command
    availability is not sufficient evidence… a proof-of-concept must show
    uninterrupted fio with read/write verification, lossless clean
    cutover and safe handling of power-loss at each stage." Not
    implemented, correctly.
  - **Option C (custom mover)**: rejected by the ADR — do not write a
    generic block copier for live guest writes.

So the honest answer to "without interruption": **yes for same-VG extent
evacuation** (`pvmove` is an online kernel operation — no interruption by
construction, once volvisor orchestrates it), **yes for online grow with
notification** (small, version-gated), and **not yet claimable for
cross-VG/pool whole-volume moves** (QSD path needs its POC first).

**Decision (D4): P6 implements the first slice of ADR-0006 —**

1. **Grow-notification**: `GrowVolume` on an attached native-local volume
   completes the VMM resize-disk step through `ChRemoteVmm` (pinned CHV
   version verified at startup, typed refusal when the version is not
   proven), with the ADR's partial-failure semantics (backend grew,
   notification failed → retry the notification, never shrink to undo).
2. **Same-VG evacuation**: a typed `RelocateVolume` surface on the
   provider/API with exactly one implemented scope — same-VG extent
   relocation via `pvmove` — journal-before-mutate, source freed only
   after verified relocation, `RELOCATION_UNSUPPORTED_SCOPE` for
   everything else (cross-VG, cross-pool, other classes — fail-closed,
   never silent). Volume API v2 gains the operation + the refusal code;
   ADR-0006's status moves from Proposed to "first slice accepted;
   QSD path remains R&D-gated".
3. **The QSD mirror/pivot POC stays out of P6** — it is the explicit
   follow-up gate, run only with the ADR's acceptance suite (uninterrupted
   fio with verification, power-loss at each stage, restart/reconcile),
   and only if same-VG evacuation proves insufficient in practice.

## 5. What is the packaging strategy — the tool must be installable on Linux servers?

**Answer: none exists yet. The repository builds a cargo workspace with CI
(fmt/clippy/test/doc + an LVM-gated integration job); there are no
packages, no service units, no release process, no version stamping and no
install-smoke gates.**

**Decision (D5): P7 delivers distribution & packaging per a new ADR-0009**,
shape summarized here (the ADR is normative):

- **Artifacts**: `.deb` (Debian/Ubuntu) and `.rpm` (EL/Fedora) built from
  release CI, plus version-stamped release binaries; version derives from
  git describe and is compiled in (`volvisord --version` prints it).
- **Filesystem layout**: config `/etc/volvisor/` (documented example),
  state `/var/lib/volvisor/`, runtime sockets `/run/volvisor/` (the P4b
  `--api-socket` convention already assumes `/run/volvisor/vms`),
  logs via journald (the tracing stack already emits structured events).
- **Service**: `volvisord.service` systemd unit — hardened (root-required
  for device claiming/DRBD, with `ProtectSystem=strict`-style directives
  where the device paths allow), `Type=notify`-ready readiness gate
  (startup reconciliation complete before `READY=1`), restart policy,
  `EnvironmentFile` support.
- **Dependencies**: `lvm2` (hard), `drbd-utils` 9 (recommended — the DRBD
  9 kernel module is a host property: mainline kernels carry 8.4 only;
  Debian/SLES ship 9.x kmods, RHEL needs ELRepo/LINBIT — the package
  documents this instead of pretending), `ceph-common` (optional, the
  ceph-rbd class).
- **Release gates**: release CI builds the packages and smoke-tests them
  in containers — install, `volvisord --version`, `--check-config`, unit
  start in a namespace sandbox where possible — and the release notes
  carry the campaign certification (`campaign-summary --check` exit 0)
  for the exact commit. No production-support claim is added by
  packaging; installability is a distribution fact, not a durability
  claim.
- **Provenance**: signed packages/tags; the exact toolchain recorded
  (reproducible-build best effort).

## 6. The implementation order (SPEC-0002 §12 renumbering)

| Phase | Name | Why here |
|---|---|---|
| P6 | Native online operations: grow-notification + same-VG relocation (+ the grant_set wedge fix, the flake fix) | Completes an existing contract promise; no external dependencies; the wedge fix restores the honest "completes under witness faults" claim |
| P7 | Packaging & distribution (ADR-0009) | The installability requirement; also the vehicle every later real-host phase deploys through |
| P8 | Tier V real-VMM verification (ADR-0010) + the P5 engine-comparison consideration gate | Validates the migration story end-to-end against a real VMM; produces the benchmarks the new-engine comparison needs |
| P9 | Rook-cell device-sharing POC (lending surface + three-host exact-SHA scenarios) | The operator-sharing question; explicitly POC-only |
| P10 | Managed-Ceph OSD placement ADR + production gate (was P6) | Last: gated on P9's outcome and a production-support decision the evidence must carry |

Each phase gets its own detailed plan (the P5 shape: scope, stages, PR map,
completion gates) before implementation, and each PR keeps the
review-until-clean loop.

## 7. Document changes this plan authorizes (the follow-up bundle)

1. **ADR-0006** — status update: first slice accepted (grow-notification +
   same-VG `pvmove` evacuation, with the safety contract and the
   `RelocateVolume` surface), QSD path remains R&D-gated behind its
   acceptance suite.
2. **ADR-0009 (new)** — distribution & packaging (§5 above).
3. **ADR-0010 (new)** — Tier V: the real-VMM verification tier, extending
   the P5 tier/claim discipline.
4. **SPEC-0002 §12** — renumbered/extended implementation sequence (§6).
5. **volume-api-v2** — the `RelocateVolume` operation (native-local,
   same-VG scope only in this phase) with typed refusals
   (`RELOCATION_UNSUPPORTED_SCOPE`), and the grow-notification semantics
   for attached volumes.
6. **rook-cell-experimental-v0** — the device-lending/sharing semantics
   section + the POC evidence requirements for it.
7. **nearline-replication-v2 §10** — the Tier V line in the
   implementation-status ledger (verified-against-real-VMM is a distinct
   claim from Tier S and from production support).
8. **The post-P5 implementation plan**
   ([2026-10-10-post-p5-implementation-plan.md](2026-10-10-post-p5-implementation-plan.md))
   — the staged plan for P6–P9.

## 8. Honesty rules carried forward

AGENTS rules 4/11/12 unchanged: nothing in P6–P9 adds a claim the evidence
does not carry. In particular: packaging does not claim production
support; Tier V does not claim real-media durability; the Rook POC does
not authorize production Rook support; same-VG relocation does not claim
cross-pool mobility. Every phase's completion gate includes its evidence
record shape, and "not run" is always distinct from "not implemented".
