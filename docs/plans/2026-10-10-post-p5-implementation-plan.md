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

Execution order (operator direction, 2026-10-10): delivery runs
easy-first — P7-A, P6-B, P6-A (flakes → triage → wedge), P6-C,
P7-B/C, P8-A/B/C, P9-A/B. The stage scopes below are unchanged by
this ordering; only the sequence in which they open differs.

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
  under witness faults" claim — **delivered in PR #23 (P6-A part 3):
  re-resolvable peer acts (the precise diagnosis is
  `docs/plans/2026-10-10-grant-set-wedge-diagnosis.md`), with the row-7
  wedge cells and row 8 re-derived to prove the convergence and the
  campaign findings recording the fix; the recorded `IN_DOUBT`
  stall-nuance question is resolved by the same delivery (see below)**;
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
  reason — **the F1 re-check shipped in PR #22 (P6-A part 2) and the
  `IN_DOUBT` stall-nuance question is resolved by PR #23's wedge fix
  (P6-A part 3): with a destination_authorized-shaped stall
  self-resolvable by construction while the witness-side grant
  remains live (the 60 s lease TTL), the nearline §6 reading holds as
  written — the canonical state plus a stall detail for resolvable
  stalls, `IN_DOUBT` reserved for genuinely unresolvable ones (row
  12b's shape); no contract change is needed, and the campaign's
  findings record the resolution. The follow-up that fix recorded —
  a park that outlives the minted leases spinning honestly with no
  re-mint path — is closed in PR #33 (the convergence-horizon
  remedy): grant-hold lease renewal during parks (uniform-liveness
  coupled) plus the gated fresh-grant re-mint behind three
  witness-authoritative gates, per the diagnosis doc's
  convergence-horizon section**.

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
VMM capacity-notification step through the existing `ChRemoteVmm`
controller's REST call over the per-VM API socket
(`PUT /api/v1/vm.resize-disk`; Cloud Hypervisor's `ch-remote` CLI has no
resize-disk subcommand — the resize-disk API is REST-only): the Cloud
Hypervisor version is **pinned
and verified at startup** (upstream PR #7948 required for externally grown
host block devices; vhost-user-blk resize remains out of scope); an
unproven version refuses the **notification** typed with the recorded
reason — the grow itself succeeds and the response reports
`retry_required` — instead of growing silently un-notified. Partial-failure semantics per the contract: the
backend may grow before the VMM/guest is notified; the provider **retries
the notification, never shrinks to undo** — `guest_notification_status`
becomes a real state machine (`notified` / `retry_required` /
`not_applicable`) instead of today's placeholder (no notification path
exists; an attached volume always reports `retry_required`).

**Test/evidence shape.** Byte-exact tests against the UDS transport (the
`vmm_tests.rs` pattern): the resize-disk request's exact bytes — request
line, headers and JSON body over the per-VM API socket (the API is
REST-only; `ch-remote` has no resize-disk subcommand); the version-gate
probe stays argv-exact (`<binary> --version` through the runner); the
version-gate refusal; the retry path.
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

**Delivered in PR #24 (P6-C).** The route is
`POST /v2/volumes/{volume_id}/move-backing` (journaled, idempotent by
`operation_id`) over the LVM provider's five-phase drive — admission,
target qualification, source derivation, pvmove start with the honest
failed-start classification, and supervision — plus the daemon's 5-s move
retry reconcile pass. The state map is the honest subset
`PREPARING | COPYING | COMPLETE | IN_DOUBT` plus `FAILED` written by the
retry reconcile alone (the verified-rollback resolution of a parked
`IN_DOUBT` — see the remediation note below; `MIRROR_READY`/`PIVOTED`
are kernel-internal and unobservable through `lvs`; scope and admission
refusals are typed errors raised before any state exists); the completion
verifies the relocation to the record's **own** target PV (a foreign
off-target relocation parks `IN_DOUBT`, never a completion), and one
fenced generation bump lands with the verified `COMPLETE` record. The
evidence: 33 provider-semantics tests (`move_tests.rs`, argv-exact FakeRunner), 5 daemon-level e2e rows
(`move_e2e.rs`), the campaign's row 16 (six durable-boundary kill/abort
scenarios in `move_rows.rs`, the LVM sibling harness with the campaign's
`Evidence` records), and four env-gated real-LVM integration tests
(`VOLVISOR_TEST_LVM=1`, LVM 2.03.16(2)): evacuation under concurrent
guest I/O with byte-exact checksum verification, the restart-mid-move
re-attach through pvmove's own "Detected pvmove in progress" semantics,
the out-of-band abort while down parking `IN_DOUBT` with the source
intact, and the mid-supervision abort parking without ever freeing the
source. The stage used the real-LVM integration path for the world and
the fake for the deterministic fault clock — both, recorded here per the
test/evidence clause.

**PR #24 review remediation (same branch, follow-up commits).** One
MAJOR, two MINORs and four NITs, all applied: (MAJOR) `finish_complete`
is now idempotent — the durable record is re-checked under the state
lock, so two overlapping supervisions of one in-flight move and a retry
reconcile whose snapshot predates a concurrent completion both answer
the stored `COMPLETE` record and the generation bumps exactly once,
pinned by two new concurrency tests (one drives two overlapping
re-attach supervisions; one choreographs the stale-snapshot reconcile
against a completing drive through a runner-level pause seam);
(MINOR) `IN_DOUBT` gained its rollback exit — the retry reconcile
resolves a parked record whose world verifiably shows the pre-move
placement to a `FAILED` record (written by the reconcile alone; a fresh
operation is admitted and supersedes it), while the mixed-placement
abort stays parked; (MINOR) contract §4A's in-band outcome wording now
names `COPYING`; (NITs) the idempotent `COMPLETE` answer is documented
as the record's historical truth (no world re-observation), an LV
unobservable across the bound (3 consecutive passes) parks `IN_DOUBT`
typed instead of silently spinning, the fresh-start path world-scans
`lvs -a`'s hidden `pvmove*` mirror rows and refuses typed when a
foreign move is already active on the source PV (verified against LVM
2.03.16: the second scoped pvmove attaches and its arguments are
ignored), and the admission match handles `FAILED` and the mirror-path
states explicitly (the latter a typed internal refusal, never a silent
fresh drive). The confirmation round left one residual MINOR, also
applied: `park_in_doubt` never parks over a stored `COMPLETE` — every
park caller judges on a pre-lock snapshot or a world observation that
may itself have failed transiently (the finishing path's observe-Err
corner), so the durable record is re-checked under the park's own
lock and a landed completion is answered as-is, never regressed to
`IN_DOUBT` (which the next pass would re-complete with a second
generation bump); the reconcile's park-shaped report arms say
"parked" only when the record actually parked. Pinned by a
transient-failure choreography (a competing finish path whose
verification observe fails once through the scripted-world seam,
over a stored `COMPLETE` — answered, preserved, and the next pass
bumps nothing).

## 2. P7 — packaging and distribution (ADR-0009)

### P7-A: `--version`, `--check-config`, config documentation

**Scope.** `volvisord --version` and `volvisor-witnessd --version` print
the compiled-in version (derived from `git describe` in release builds).
`volvisord --check-config <path>` (and the witnessd equivalent) loads and
validates a configuration file and exits 0 or a typed error. A documented
example config ships in the repository (the future `/etc/volvisor/`
content): [docs/configuration.md](../configuration.md),
[examples/volvisor.toml](../../examples/volvisor.toml) and
[examples/witnessd.toml](../../examples/witnessd.toml).

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

**Recorded follow-up (from the delivered run) — RESOLVED in PR #32.**
The first recorded host run's binary (cloud-hypervisor v43.0.0)
predates the upstream `vm.resize-disk` API route, so the resize
scenario's notified leg was asserted by contract only (a 2xx means
`notified`; a typed refusal means a durable `retry_required` carrying
it) and that run recorded the refusal branch (HTTP 404). PR #32
resolved it: the scenario was re-run against the upstream v51.1
release binary (downloaded from the release asset, pinned by its
SHA256 cross-checked against the platform-published asset digest)
and the `notified` leg is recorded from a real observation — the
real `PUT /api/v1/vm.resize-disk` answered 2xx and the grow
converged `notified`, with the version gate at the route's boundary
(51.0.0) recorded both ways: proven against v51.1, and unproven
against the same minimum probed on the real v43.0.0 binary —
fail-closed before any VMM call. The re-run surfaced and fixed the
adapter's request body (the pre-route field name; v51.1's pinned
OpenAPI says `id` + `desired_size` in bytes — the live typed 400
that exposed the drift is recorded as the run's finding). The
second recorded run is committed under
`docs/evidence/tier-v/2026-10-11-devhost-v51/`; the empirical
bounds it leaves behind: v43.0.0 lacks the route (typed 404, first
run), v51.1 carries it (notified, second run), and upstream's own
pinned OpenAPI places the route's introduction between v49.0
(absent) and v50.0 (present) — a document-verified bound between
the two live-verified points. This was never P8-B work — P8-B is
the engine-comparison methodology, a different scope.

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

**Resolution (PR #28).** The methodology is delivered at
[docs/benchmarks/engine-comparison-methodology.md](../benchmarks/engine-comparison-methodology.md)
— purpose, the honesty spine (per-engine durability contracts stated
alongside every number; a benchmark is not a durability proof), matched
topology and the pin list, the workload matrix (matched queue depths,
CPU cost, flush/FUA variants, resync-under-foreground, the operational
behaviors, all through the real VMM frontend), the campaign's run
discipline, and the five-criterion decision gate. The decision is
recorded: **the gate stays closed — do not build a new engine now.**
No benchmark can honestly run in this phase (this development sandbox
has no DRBD 9 kernel module, no Mayastor/SPDK installation, and no
matched hardware — not two physical hosts of the same model with the
same NVMe SKUs); the measurements that could carry a build decision do
not exist, and the still-closed state is the decision, recorded in the
methodology doc (§7), the nearline §10 ledger's benchmark row, and
SPEC-0002 §12's P8 line. The gate reopens only by executing the
methodology on matched hardware — the reopen condition is the
measurements' existence, not a better argument.

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

**Resolution (PR #28).** Declined, with the recorded reason and the
reopenable sketch:
[ADR-0011](../adr/0011-local-mirror-experiment-decline.md). No
local-mirror implementation exists and none is authorized by any accepted
contract or ADR suite; the durability need is carried by cross-host
nearline replication, and a local mirror's value proposition (surviving
single-device loss without cross-host failover) is a new availability tier
no consumer has contracted for; the validating fault classes
(ADR-0007's R3: single leg loss, repair, rebuild contention) only exist
on real media, which none of the delivered tiers provides (Tier S's
storage is a block map with no device granule; Tier V's devhost storage
below the VMM seam is simulated); and the design space where a mirror
could return is already reserved by ADR-0006's acceptance-suite gate —
whose suite already contains the mirror-relevant cells (4K random
writes during mirror, source SSD failure), without requiring the QSD
pivot itself. The sketch — mirror layout, the fault classes and
their evidence shapes, what the real-host/media tier would have to run —
is recorded in the ADR's appendix so the residue is closed but
reopenable: the reopen condition is a consumer contracting for the
availability tier AND a mirror design passing ADR-0006's gate. The
nearline §10 ledger's mirror line and SPEC-0002 §12's P3 row now record
the decided state; ADR-0007's Related list cross-links the decision so
R3's readers find its status.

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

## Terminal state (2026-10-10)

The closing record: every deliverable stage of this plan is merged, and
the one stage that is not deliverable in this environment says so below.
Each stage's letter carries its own resolution notes inline; this section
is the whole arc at one glance, so a reader of the plan does not
reconstruct it from the per-stage notes. This section records — it
asserts nothing the artifacts do not already carry.

| Stage | PR(s) | Outcome | Evidence |
|---|---|---|---|
| P6-A hardening slice | #21–#23, #33 | delivered | the flake fixes and the row-3 budget amendment (#21), the F1 lineage re-check (#22), the grant_set wedge fix — re-resolvable peer acts — with the campaign's rows 7–8 re-derived ([the diagnosis](2026-10-10-grant-set-wedge-diagnosis.md); the healed cells' records in the campaign suite's `rows_8_14.rs` and `summary.rs`), and the convergence-horizon closure (#33) — grant-hold lease renewal during parks plus the gated fresh-grant re-mint, with the wedge-horizon scenarios proving the convergences and the honest boundaries (the diagnosis doc's horizon section; the campaign suite's `wedge_horizon.rs`) |
| P6-B grow-notification | #20 | delivered | the `vm.resize-disk` path with the pinned version gate and the real `guest_notification_status` machine (`volvisor-provider`'s `grow_tests.rs` + the byte-exact `vmm_tests.rs`) |
| P6-C same-VG pvmove | #24 | delivered | `MoveVolumeBackingOnline` end to end: 33 provider-semantics tests (`volvisor-lvm`'s `move_tests.rs`), 5 daemon rows (`volvisord`'s `move_e2e.rs`), the campaign's row 16 (`move_rows.rs`), the `VOLVISOR_TEST_LVM=1` integration quartet |
| P7-A version/config | #19 | delivered | `--version` / `--check-config` surfaces, [configuration.md](../configuration.md) + the `examples/` configs, asserted in CI |
| P7-B units + readiness | #25 | delivered | `packaging/volvisord.service` + `volvisor-witnessd.service`: `Type=notify`, `READY=1` only after startup reconciliation, hardening with omissions documented |
| P7-C packaging + release CI | #26 | delivered | `.github/workflows/release.yaml` + `packaging/smoke.sh`: deb and rpm built, installed and smoke-tested in containers |
| P8-A Tier V | #27, #32 | delivered | the env-gated substrate and four bounded scenarios (`volvisor-campaign`'s `tier_v`), the first recorded host run committed at [docs/evidence/tier-v/2026-10-10-devhost/REPORT.md](../evidence/tier-v/2026-10-10-devhost/REPORT.md), and the recorded follow-up resolved in #32 — the resize notified leg proven against the pinned upstream v51.1 binary ([docs/evidence/tier-v/2026-10-11-devhost-v51/REPORT.md](../evidence/tier-v/2026-10-11-devhost-v51/REPORT.md)) |
| P8-B engine-comparison gate | #28 | decided — gate closed | [the methodology](../benchmarks/engine-comparison-methodology.md) §7: no benchmark can honestly run in this phase, so no new engine is built; the gate reopens only by executing the methodology on matched hardware |
| P8-C local-mirror experiment | #28 | decided — declined | [ADR-0011](../adr/0011-local-mirror-experiment-decline.md), with the reopenable sketch in its appendix |
| P9-A lending surface | #29 | delivered | the contract's six lending invariants, one daemon-level Tier S test each (`volvisor-api`'s `lending_tests`), the contract's operator shape and the implementation agreeing |
| P9-B three-host POC | — | hardware-gated | the [POC README](../poc/rook-cells/README.md)'s not-run markers are the gate list; ADR-0008's decision list is unchanged |
| P10 | — | not authorized | §5 above — gated on P9's outcome and a production-support decision no evidence yet carries |

**In aggregate.** The phase delivered native-local online operations
complete — ADR-0006's first slice whole: grow-notification (#20) and
same-VG `pvmove` evacuation (#24), each with the durable-boundary fault
discipline the P5 campaign established; the hardening slice that closed
the recorded backlog's flakes, the F1 re-check and the grant_set wedge
(#21–#23); packaging end-to-end — version/config surfaces, hardened
systemd units whose readiness follows reconciliation, and deb + rpm
artifacts built, installed and smoke-tested by release CI (#19, #25,
#26); Tier V with a recorded real-VMM run on a devhost (#27); and the
two P8 decisions recorded closed — the engine-comparison gate stays
closed for want of honest measurements, and the local-mirror experiment
is declined with its reopenable sketch (#28); and the device-lending
surface of the rook-cell contract, daemon-tested to its six invariants
(#29); and, after the closing record was first drafted, the shared
operation-id namespace hardening (F5 of PR #23's review) landed as PR
#31 — derived peer ids in a reserved, consumer-unreachable namespace —
followed by the Tier V resize notified leg proven against the pinned
upstream v51.1 binary (#32, a second committed evidence bundle) and the
convergence-horizon closure (#33, grant-hold lease renewal during parks
plus the gated fresh-grant re-mint); and, after this record's first
draft, the corrected attachment-state reporting design of issue #34 is
proposed as draft PR #35 — a hypervisor-neutral consumer-evidence route
proven against the native-local LVM provider and the conformance fake,
with no claim of a consumer integration, upstream VMM adoption, or merge.
The workspace suite stands at 1084 tests, and the campaign
certification (CG1–CG5) passed at every stage's head — the honesty
rules of §0 held throughout: no artifact of P6–P9 claims production
support, real-media durability, or anything else its evidence does not
carry.

**What remains gated, and the exact gates.** P9-B needs three physical
hosts (KVM/IOMMU, one sacrificial NVMe controller each, an external
Kubernetes control plane) — the POC README's not-run markers are the
gate list, ADR-0008's decision list is unchanged, and the lending
surface's real observation leg and the volvisord runtime wiring arrive
with that work, not before it. The P8-A recorded follow-up (the resize
scenario's notified leg against a route-carrying binary) was open when
this section was first drafted and is now **resolved in PR #32**: the
`notified` leg is recorded from a real observation against the pinned
upstream v51.1 binary, with the version-gate boundary recorded both
ways (`docs/evidence/tier-v/2026-10-11-devhost-v51/`). The grant_set
wedge's convergence horizon was open when this section was first
drafted and is now **resolved in PR #33**: a park that outlives the
60 s lease TTL no longer spins without recourse — the destination
renews the grant-hold leases of its live pre-promote preparations
during the park, and when the leases are dead anyway the grant act
re-issues behind three witness-authoritative gates under a fresh
derived operation id (the diagnosis doc's horizon section records the
remedy, the campaign suite's `wedge_horizon.rs` proves the
convergences and the honest boundaries). One backlog pin the phase
leaves open, recorded rather than silently dropped: the Tier R
real-host drive (scaffolded, skipped honestly, hardware-gated — the
campaign's gate records and the nearline §10 note carry its state). The
shared operation-id namespace hardening (F5 of PR #23's review) was
left open when this section was first drafted and is now **resolved in
PR #31** — derived peer ids render in the reserved `derived:` namespace,
unreachable from the consumer surface, with typed refusals at every
journaling consumer route.

**P10.** Not authorized by this plan (§5 above) and still not
authorized by anything the phase produced: it is gated on P9's outcome
and a production-support decision the evidence must carry. Nothing in
P6–P9 changes that — by §0's rule, no artifact of this phase may carry
the claim that would open it.
