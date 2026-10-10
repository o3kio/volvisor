# Post-P5 implementation plan — P6–P9: native online operations, packaging, Tier V, the Rook-cell POC

Status: normative for the post-P5 phases (this document is the plan of
record; implementation PRs cite it)
Date: 2026-10-10
Builds on: [readiness plan](2026-10-10-post-p5-readiness-questions.md)
(PR #17 — its decisions D1–D5 are settled and this plan stages them;
do not relitigate), [P5 plan](2026-10-10-p5-aggressive-failure-campaign.md)
(PRs #11–#16), [ADR-0006](../adr/0006-online-resize-and-live-local-block-relocation.md)
(first slice accepted), [ADR-0008](../adr/0008-rook-only-hyperconverged-cells.md)
(lending surface), [ADR-0009](../adr/0009-distribution-and-packaging.md),
[ADR-0010](../adr/0010-tier-v-real-vmm-verification.md),
[Volume API v2](../../contracts/volume-api-v2.md) §4A,
[rook-cell-experimental-v0](../../contracts/rook-cell-experimental-v0.md),
[nearline contract](../../contracts/nearline-replication-v2.md) §10,
[SPEC-0002](../../docs/specs/SPEC-0002-volvisor-volume-virtualization.md) §12,
AGENTS rules 4/8/11/12.

P0–P5 are implemented and merged (main at the readiness plan's review
baseline: 855 tests, the campaign certification green, no
production-support claims anywhere). This plan stages everything the
readiness plan authorized: P6 (native online operations + the hardening
slice), P7 (packaging and distribution), P8 (Tier V real-VMM verification,
the engine-comparison gate, the local-mirror decision), P9 (the Rook-cell
device-sharing POC). P10 (managed-Ceph OSD placement, the previous
numbering's P6) stays outside this plan: it is gated on P9's outcome and a
production-support decision no evidence yet carries.

Each phase below is staged; each stage carries its scope, its test/evidence
shape and its completion gate. PR numbers are assigned as stages open —
stages are labeled (P6-A, P6-B, …), never hard-coded to future PR numbers.
The house review-until-clean loop applies to every PR.

## 0. The honesty rules carried forward

[Readiness plan §8](2026-10-10-post-p5-readiness-questions.md), verbatim in
intent: nothing in P6–P9 adds a claim the evidence does not carry.

- Packaging does not claim production support — installability is a
  distribution fact, not a durability claim.
- Tier V does not claim real-media durability — "verified against a real
  VMM process at recorded versions" is its whole claim.
- The Rook POC does not authorize production Rook support — the
  ADR-0008 gate list, now including the lending surface, is what any
  later claim must pass.
- Same-VG relocation does not claim cross-pool mobility — out-of-scope
  moves are typed `MOVE_UNSUPPORTED_SCOPE` refusals.
- Every phase's completion gate includes its evidence record shape, and
  **"not run" is always distinct from "not implemented"**.

Each phase closes by updating the evidence ledger where it lives (the
campaign records, the nearline §10 note, the POC evidence) — never by
silence.

## 1. P6 — native online operations and the hardening slice

### P6-A: the hardening slice (the recorded backlog, triaged)

**Scope.** Every backlog item D1 triaged into P6's hardening slice, each
either **fixed** or **explicitly declined with a recorded reason** — never
silently dropped (the two §1 backlog items *not* triaged here stay
explicitly scheduled elsewhere: the local-mirror residue is the P8-C
decision and the Tier R real-host drive is P8's faulting substrate — see
the readiness plan §6):

- the **`grant_set` wedge** — a witness kill inside the grant commit parks
  a migration safely at `destination_authorized` forever while the retry
  task spins at its 5 s tick. Fail-closed, but a stall: the fix is a design
  change (re-resolvable peer acts, or a promote path that does not route
  through the failed grant op), restoring the honest "migration completes
  under witness faults" claim;
- the **renewal-deadline flake** (`an_unreachable_witness_defers_renewal_until_the_deadline`,
  `volvisor-drbd`): observed ~1-in-20 in earlier full-suite runs, 0/15 in
  isolation, not reproduced in the readiness review; the suspected root
  cause (the kit's `server.handle.abort()` aborting the axum serve without
  draining in-flight renewals) is unproven, so the fix starts from either a
  reproduction or a drain fix that is provably correct regardless;
- the **kill-matrix startup race** (`row_5_consumer_mobility_kill_matrix`,
  the post-restart "rolled back to aborted" assertion in
  `crates/volvisor-campaign/tests/rows_4_7.rs`): reproduced 2–3 of 6
  full-suite runs, racing the daemon's async startup reconciliation;
- the **`IN_DOUBT` stall-nuance question** and the **F1 barrier-time
  lineage re-check** (both recorded in the campaign report's findings
  section): triaged the same way — fixed or declined with a recorded
  reason.

**Test/evidence shape.** A fix lands with a test that fails on the old
behavior (a reproduction for the flakes; a wedge-recovery scenario for the
grant_set design fix). A decline lands as a recorded reason in this plan's
follow-up (the PR body or a plan amendment), stating what was investigated
and why the item stays open — the backlog pin moves, it never disappears.

**Completion gate.** Every item above is closed one way or the other, the
full suite is green, and the campaign re-certifies
(`campaign-summary --all --check` exit 0) — for the wedge fix
specifically, the recovered scenario appears in the evidence with the
witness fault still injected.

### P6-B: grow-notification (ADR-0006 first slice, part 1)

**Scope.** `GrowVolume` on an attached `native-local` volume completes the
VMM capacity-notification step through the existing `ChRemoteVmm` adapter
(`PUT /api/v1/vm.resize-disk`): the Cloud Hypervisor version is **pinned
and verified at startup** (upstream PR #7948 required for externally grown
host block devices; vhost-user-blk resize remains out of scope); an
unproven version refuses the attached grow **typed** instead of growing
silently un-notified. Partial-failure semantics per the contract: the
backend may grow before the VMM/guest is notified; the provider **retries
the notification, never shrinks to undo** — `guest_notification_status`
becomes a real state machine (`notified` / `retry_required` /
`not_applicable`) instead of today's placeholder (no notification path
exists; an attached volume always reports `retry_required`).

**Test/evidence shape.** argv-exact tests against the scripted runner (the
`vmm_tests.rs` pattern): the resize-disk call's exact argv including the
`--api-socket` convention; the version-gate refusal; the retry path.
Integration through `FakeVmm` (the campaign rig's writer/poll shapes): a
grow during attachment flips `retry_required` → `notified` on the retry
tick; a detached grow stays `not_applicable`; the never-shrink rule is
asserted against the backing size after a failed notification.

**Completion gate.** The typed refusals are pinned by tests; the contract's
§4A grow-notification letter (retry rule, version gate, the three field
values) matches the implementation; the ADR-0006 slice clause is satisfied
for part 1.

### P6-C: `MoveVolumeBackingOnline`, same-VG `pvmove` evacuation (ADR-0006 first slice, part 2)

**Scope.** Implement the **already-contracted** operation
([Volume API v2](../../contracts/volume-api-v2.md) §4A) with exactly one
qualified capability scope: `same_vg_extent_move` via LVM `pvmove`.
Journal-before-mutate; the source extents are freed **only after verified
relocation and ownership reconciliation**; the contract's
never-generic-`FAILED`/`IN_DOUBT` rule applies verbatim. Out-of-scope moves
— cross-VG, cross-pool, cross-class, or a capability the backend does not
qualify — are refused typed `MOVE_UNSUPPORTED_SCOPE` (fail-closed, never
silent). `same_host_live_backing_move` is advertised nowhere (the QSD
acceptance suite is the unchanged gate). No new operation name is
introduced.

**Test/evidence shape.** The P5 discipline applied to a new durable path:
kill points at every journaled boundary of the move (journal append,
pvmove start, pvmove completion/verification, source-extent free), each
recovered by restart → reconcile → intent-resolution and checked against
the contract's state vocabulary (`PREPARING | COPYING | MIRROR_READY |
PIVOTED | COMPLETE | FAILED | IN_DOUBT` mapped to what a same-VG move
honestly passes through). An unknown outcome mid-move reads `IN_DOUBT`,
never a generic `FAILED`; a failed verification never frees the source.
The `MOVE_UNSUPPORTED_SCOPE` refusals are pinned for each out-of-scope
shape (cross-VG, cross-pool, cross-class, unqualified capability). The
fake world gains the pvmove seam it needs (or the stage uses the real LVM
integration path — the stage PR decides and records which, per the
integration-test conventions the LVM provider already has).

**Completion gate.** End-to-end same-VG evacuation under I/O with the
durable-boundary fault tests green; the refusal table pinned; the campaign
(or a sibling harness with the same evidence discipline) carries the move's
fault rows; the contract letter and the implementation agree.

## 2. P7 — packaging and distribution (ADR-0009)

### P7-A: `--version`, `--check-config`, config documentation

**Scope.** `volvisord --version` and `volvisor-witnessd --version` print
the compiled-in version (derived from `git describe` in release builds).
`volvisord --check-config <path>` (and the witnessd equivalent) loads and
validates a configuration file and exits 0 or a typed error. A documented
example config ships in the repository (the future `/etc/volvisor/`
content).

**Test/evidence shape.** Unit tests for config validation surfaces; the
`--check-config` exit codes pinned (valid example config → 0; each
invalidity class → its typed error); `--version` non-empty in release
builds (CI asserts it).

**Completion gate.** The three surfaces exist, are tested, and the example
config passes its own `--check-config` in CI.

### P7-B: systemd units, sd_notify readiness, hardening

**Scope.** `volvisord.service` and `volvisor-witnessd.service` (separate
units, both binaries from the one package): `Type=notify` with `READY=1`
emitted **only after startup reconciliation completes** (journal replay
and provider reconcile — the existing startup order); a restart policy
into the reconciling path; `EnvironmentFile=` support; hardening
directives where the device paths allow, with any deliberately omitted
directive documented in the unit, never silently weakened.

**Test/evidence shape.** Unit-file review against ADR-0009's checklist;
container tests where sandboxable (unit start → active, readiness ordering
observed); the readiness emission is asserted to follow reconciliation
(a test daemon that fails reconciliation never reports ready).

**Completion gate.** The units install and start under the packaged
layout; the readiness ordering is proven, not asserted.

### P7-C: deb + rpm packaging, release CI, install-smoke gates

**Scope.** `.deb` (Debian/Ubuntu) and `.rpm` (EL/Fedora) built in release
CI with the dependency metadata ADR-0009 specifies (`lvm2` hard;
`drbd-utils` 9 recommended with the kernel reality documented — mainline
carries DRBD 8.4.11 only, DRBD 9 is out-of-tree from LINBIT, Debian/SLES
ship 9.x kmods, RHEL needs ELRepo/LINBIT; `ceph-common` optional).
Release CI smoke-tests the packages in containers: install,
`volvisord --version`, `--check-config` on the shipped example config,
unit start where sandboxable (what cannot be sandboxed is recorded, not
skipped). Provenance: signed git tags, `SHA256SUMS`. Release notes carry
the campaign certification for the exact commit
(`campaign-summary --all --check` exit 0).

**Test/evidence shape.** The release pipeline's own logs are the evidence:
a green release run shows the packages built, installed and smoke-tested
in containers, with the certification recorded for the tagged commit.

**Completion gate.** A tagged release exists whose notes carry the
certification and whose artifacts passed every smoke gate; the ADR-0009
non-goals (no OCI images, no production-support claim) are respected in
every artifact.

## 3. P8 — Tier V, the engine-comparison gate, the local-mirror decision

### P8-A: the Tier V substrate and bounded scenario set (ADR-0010)

**Scope.** The env gate (`VOLVISOR_TEST_VMM=1`, binary paths overridable),
the `skipped`-by-default gate records (the Tier R pattern — the matrix
shows the gate, never a hole), the `tier: "V"` evidence records carrying
the VMM binary version + SHA256, and the four bounded scenarios: the
migration happy path end-to-end against a real VMM; the pause/snapshot
kill windows; snapshot/restore divergence detection; the P6 resize
notification. Without the variable, every scenario skips honestly; with
it set but the binaries absent, the tier fails loudly.

**Test/evidence shape.** The campaign's record discipline extended: run
directory, per-scenario records, `REPORT.md`, the completion gates
extended to the Tier V rows. The default suite asserts the skip records
exist (never silent absence); a claimed environment without binaries is a
failure.

**Completion gate.** The substrate merges with honest defaults; at least
one real-VMM run's evidence exists (a CI machine or a recorded host with
Cloud Hypervisor installed) or the tier's not-run state is itself recorded
and visible; the nearline §10 ledger's Tier V line moves from "planned" to
the delivered truth, whatever it is.

### P8-B: the engine-comparison benchmark methodology and its decision gate

**Scope.** The methodology for the §12 engine-comparison consideration
gate: measure Mayastor/io_uring/SPDK alternatives against DRBD 9 **on
installable, version-pinned, real-VMM software** (P7's packaging is what
makes this possible — the comparison runs what would actually be
deployed). Matched hardware, workloads and durability contracts (a
benchmark is not a durability proof; never imply apples-to-apples from
different ACK semantics — ADR-0007's alternatives table carries the
mismatch warnings). The decision gate itself: **only build a new engine
when the measurements justify it** — the gate stays closed otherwise, and
closing it is a recorded decision with its evidence, not an
implementation.

**Test/evidence shape.** A methodology document (scenarios, metrics,
topology, version pinning) reviewed like any normative doc; the benchmark
runs recorded with the Tier V/Tier R evidence discipline; the decision
recorded either way.

**Completion gate.** The methodology exists and is reviewable; the
decision (build / do not build) is recorded with the measurements that
carry it. If no benchmark can be run yet, the gate's still-closed state is
recorded — the consideration item is decided, not silently deferred.

### P8-C: the local-mirror experiment decision

**Scope.** The §12 P3 residue: the local mirror experiment ("no mirror
implementation exists to fault" — the nearline §10 note's honest state).
A mirror can only be honestly faulted on real media/devices, so the
decision is: **design the experiment and fault it on the real-host/media
tier** (the Tier R drive, or a Tier V host where the media is real), or
**explicitly decline it with a recorded reason**. Either outcome updates
the nearline §10 ledger's mirror line.

**Test/evidence shape.** If designed: the experiment plan (mirror layout,
fault classes — single leg loss, repair, rebuild contention per ADR-0007's
R3) and its evidence records. If declined: the recorded reason in this
plan's amendment and the ledger.

**Completion gate.** The residue is decided — the ledger no longer carries
it as an open "not delivered, undecided" item.

## 4. P9 — the Rook-cell device-sharing POC

### P9-A: the device-lending surface (Tier S-testable without Rook)

**Scope.** The [rook-cell contract](../../contracts/rook-cell-experimental-v0.md)'s
lending surface as a **Tier S implementation**: `LendDevice` /
`ReclaimDevice` / `DeviceOwnership` over claimed devices —
journal-before-lend, the lent state operator-visible in the ownership
record, refusals of double-lend and mutate-while-lent, reclaim requiring
a release or an operator-authorized force path with a recorded residue
check, fail-closed on unknown state, and **no double-ownership window**
(claim, lend and reclaim are generations of one record). This surface
does not touch the volume classes (the readiness plan D3's scope note).

**Test/evidence shape.** Every invariant of the contract's lending
section gets a Tier S test: the happy lend → visible ownership → release
→ reclaim; each typed refusal; the force path with residue check
(quarantine on foreign residue); the crash-between-journal-and-effect
shape resolving by reconciliation. The POC evidence scenarios that need a
real cell (the Rook OSD leg) are P9-B, not here.

**Completion gate.** All six invariants pinned by tests; the contract's
operator shape and the implementation agree; no production-support
language anywhere in the surface.

### P9-B: the three-physical-host exact-SHA POC scenarios

**Scope.** The existing [POC design](../poc/rook-cells/README.md)
executed: the three-physical-host exact-SHA deployment — Rook operator on
the external control plane, one OSD per cell on volvisor-lent devices —
through the ADR-0008 gate list **including the lending surface**: lend,
operator-visible ownership, reclaim after cell teardown with the residue
check, no adoption of foreign state anywhere in the sequence. Plus the
contract's gate list: lifecycle, quorum under cell loss, disk/VFIO
isolation, cleanup including lent-device reclaim, admission,
fail/restart, benchmarks vs Rook directly on the same hardware.

**Test/evidence shape.** The P5 record discipline on real hosts: a run
directory + a report per scenario family; exact SHA pinning of every
component; "not run" distinct from "not implemented"; every failure
injection and outcome recorded.

**Completion gate.** The POC's evidence bundle exists and the outcome is
recorded against the ADR-0008 decision gate. POC-only status is unchanged
by this work: passing authorizes at most the next decision (the P10
managed-Ceph consideration), never production Rook support. On failure,
the recorded outcome redirects (bare-metal Rook or a service-VM approach)
without an intrusive fork.

## 5. What this plan does not authorize

- P10 (managed-Ceph OSD placement ADR + production gate) — gated on P9's
  outcome and a production-support decision the evidence must carry.
- The QSD mirror/pivot path — behind ADR-0006's acceptance suite, untouched.
- Production Rook support, real-media durability claims, or any
  production-support claim from any P6–P9 artifact (§0 above).
- Distro repository hosting and package-signing infrastructure
  (ADR-0009's best-effort-later scope).
