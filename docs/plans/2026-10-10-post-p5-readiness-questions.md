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
[the post-P5 implementation plan](2026-10-10-post-p5-implementation-plan.md)
(created by the follow-up bundle in §7 — the link goes live when that
bundle merges).

## 1. Are all implementation plans finished? What remains?

**Answer: SPEC-0002 §12's items P0–P4 are delivered; three §12 residues and
a recorded backlog remain.** (Numeral caution — resolved by the renumber
this plan's D1 authorized, which PR #18 applied: SPEC-0002 §12 now carries
the repo's own phase labels. Historically, the repo's phase labels P0–P5
mapped onto §12's previous P0–P4 — the repo's "P5" campaign *is* that
numbering's P4 crash/failure item — and §12's previous P5 was a different
thing: the engine-comparison consideration gate, now P8.)

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

1. **§12's engine-comparison consideration item (the previous numbering's
   P5, now P8)** — "compare Mayastor/io_uring/SPDK
   alternatives; only build a new engine when justified"
   ([ADR-0007](../adr/0007-drbd9-nearline-replication-provider.md) pins DRBD
   9 as the selected prototype engine). This is a *measured-justification*
   gate, not automatic work: it stays closed until a Tier-R/Tier-V
   benchmark exists to measure against (see §2 and §6 of this document —
   the packaging and real-VMM phases come first precisely so the
   comparison can be run on installable, real-VMM software).
2. **§12 P3's local-mirror experiment residue** — "nearline DRBD baseline
   and isolated frontends + local mirror experiment": the baseline and
   isolated frontends shipped (PR #5); the local mirror is recorded
   not-delivered in the nearline §10 note ("no mirror implementation
   exists to fault"). A mirror can only be honestly faulted on real
   media/devices, so the experiment is decided inside P8 (§6): design and
   fault it on the real-host/media tier, or explicitly decline it with a
   recorded reason. It is not silently deferred.
3. **§12's managed-Ceph item (the previous numbering's P6, now P10;
   managed-Ceph OSD placement ADR + production gate)** —
   unstarted, and correctly so: SPEC-0002 gates it behind the Rook-cell
   POC outcome (§3 below) and a production-support decision that no
   evidence yet carries.

Recorded backlog. Items with existing artifacts are pinned there (the
grant_set wedge in its evidence record and the campaign report's findings;
the local mirror and Tier R in the nearline §10 note; the `IN_DOUBT`
stall-nuance question and the F1 barrier-time lineage re-check in the
campaign report's findings section, per the nearline note's "own plan/PR,
never silently" rule). Two intermittently observed test flakes have no
other recording artifact, so this plan is their pin — both with their
evidence status stated honestly:

- the **`grant_set` wedge** — a witness kill inside the grant commit parks
  a migration safely at `destination_authorized` forever while the retry
  task spins at its 5 s tick (fail-closed stall; remedy is a design change:
  re-resolvable peer acts, or a promote path that does not route through
  the failed grant op) — **resolved in PR #23 (P6-A part 3): the remedy
  taken is re-resolvable peer acts — a recorded failure of a re-issuable
  peer act (the four internal peer routes: migration-derived operation
  ids, a total landed-ness inspection, an idempotent re-execution at
  every layer) is re-evaluated through that inspection instead of
  replayed verbatim: proven landed → the proven outcome supersedes the
  recorded failure; proven not landed → the act re-executes under its
  idempotency discipline (the witness batch replays its recorded
  outcome — never a second epoch); an inspection error surfaces typed
  (the stale failure is never re-served as a terminal answer). The
  strict routes (volume and mobility operations: consumer-supplied ids,
  operator-judgment refusals) keep verbatim failure replay; genuinely
  unresolvable failures (row 12b's stable typed promote refusal) still
  park, through honest re-execution reproducing the refusal. The
  campaign's row-7 wedge cells and row 8 now prove the convergence
  (epoch exactly 2, the acknowledged prefix byte-exact through the
  healed completion), restoring the honest "migration completes under
  witness faults" claim; the same PR resolves the recorded `IN_DOUBT`
  stall-nuance question in the contract's favor (a
  destination_authorized-shaped stall is self-resolvable by
  construction while the witness-side grant remains live — the 60 s
  lease TTL; a park that outlives the minted leases (an unpromoted
  grant-hold lease is never renewed) spins honestly with no re-mint
  path, and the remedy directions — renewing unpromoted grant-hold
  leases during a park, or a fresh-grant epoch when the recorded
  grant's lease is provably dead — are a recorded follow-up, not
  shipped; so the canonical-state-plus-stall-detail reading holds
  and `IN_DOUBT` stays reserved for genuinely unresolvable stalls) —
  the diagnosis is `docs/plans/2026-10-10-grant-set-wedge-diagnosis.md`**;
- the **renewal-deadline flake** in `volvisor-drbd`
  (`an_unreachable_witness_defers_renewal_until_the_deadline`): observed
  intermittently in earlier full-suite runs (roughly 1-in-20; 0/15 in
  isolation), *not* reproduced in this plan's review round; the suspected
  root cause (the kit's `server.handle.abort()` aborts the axum serve
  without draining in-flight renewals) is unproven, so the fix must start
  from either a reproduction or a drain fix that is provably correct
  regardless — **resolved in PR #21 (P6-A part 1): the suspicion was
  right in class, refined in mechanism (the abort's close of the
  lingering keep-alive connections is asynchronous, so a renewal
  dispatched over the client's pooled connection could complete inside
  that window); the kit's abort is replaced by a graceful-shutdown drain
  whose completion proves no connection remains serviceable**;
- the **kill-matrix startup race** in the campaign suite
  (`row_5_consumer_mobility_kill_matrix`, the post-restart "rolled back to
  aborted" assertion in `rows_4_7.rs`): reproduced roughly 2–3 of 6
  full-suite runs during this plan's review round — the assertion races
  the daemon's async startup reconciliation — **resolved in PR #21
  (P6-A part 1): the retry task's startup pass runs concurrently with
  the serve and its `try_lock` defers behind the test's own fresh-prepare
  drive, so the single-shot observation became the same bounded poll the
  transfer and abort cells already use**;
- the **row-3 oracle budget sensitivity** (disclosed in PR #20's
  certification, not previously pinned here): the §3.2 budget bound for
  the kill-and-recover oracle rows (5 s) sat on top of the peer-grant
  cells' designed duration — one full migration-retry tick (5 s, the
  production recovery path the row proves) — with zero headroom, so
  suite-load excursions crossed it (5031 ms against the 5000 ms bound;
  4955–4960 ms isolated). **Resolved in PR #21 (P6-A part 1): the P5
  plan §3.2 bound for row 3 is amended to 8 s with the derivation
  recorded there; every other row's bound is unchanged.**
- the **row-12b lease-state observation race** (the campaign suite,
  `assert_w1_w5_vol` in `rows_8_14.rs`): the W1–W5 helper read the
  witness view once, so a lease-state transition still in flight at
  assert time — the failed promote's fail-closed release
  (Live → Revoked), which the row's epoch-level bounded-wait does not
  cover — failed the check. Observed twice, both in row-12b
  isolated/parallel contexts (PR #22's early run, never reproduced
  then; 1-of-6 in PR #23's review round); never in a certified run.
  **Fixed in PR #23's review round: the helper now bounded-polls to
  the FULL expected view (epoch and holder included, so the strength
  is the single-shot assert's — a wrong epoch never satisfies the
  poll) with the last observed state in the failure message; the
  rows_4_7 twin (`assert_w1_w5`) carries the same class and is
  hardened identically (its current call sites all assert
  transition-free shapes — class hardening, not a reproduced flake);
  row 12b then ran 15× isolated, all green.**
- the **shared operation-id namespace** (found in PR #23's review;
  pre-existing since the derived peer ids landed in stage B2): a
  consumer-supplied volume operation id crafted to equal a derived
  peer id (the `mig-api-{tag}-{16hex}` shape) with a differing
  payload forces `IDEMPOTENCY_CONFLICT` on the peer route — the
  journal's step-2 hash check refuses before any mutation. DoS-class,
  not corruption: no state is mutated on the conflict and each route
  re-verifies its own preconditions beyond the journal. No
  reproduction exists (the collision requires a deliberately crafted
  consumer id — the derivation is deterministic from the migration id,
  so an authenticated consumer who knows the target migration can
  compute and pre-register the colliding id; it never happens by
  accident because ordinary consumer ids do not carry the
  `mig-api-{tag}-{16hex}` shape); the remedy direction (a namespaced
  peer-id derivation, or a per-route journal domain) is
  recorded here for the P6 hardening triage — pinned so it is not
  silently dropped;
- the **Tier R real-host drive** — the 8 gates are scaffolded, skipped
  honestly, and fail loudly under `VOLVISOR_CAMPAIGN_TIER=R`.

**Decision (D1):** the next phases are P6–P9 as defined in §6 (native
online operations, packaging/distribution, the real-VMM verification tier,
the Rook-cell device-sharing POC), with the old P6 (managed Ceph) renumbered
to P10 and kept last before any production gate. SPEC-0002 §12 is renumbered
accordingly, and §12 P3's local-mirror residue is scheduled explicitly
into P8 (decided there, not silently deferred). The P5 consideration item
(new-engine comparison) stays gated on P8's real-VMM benchmarks. The
grant_set wedge fix, both recorded test flakes, and the two recorded
campaign follow-ups (the `IN_DOUBT` stall-nuance question, the F1
barrier-time lineage re-check) are all triaged in P6's hardening slice —
each either fixed or explicitly declined with a recorded reason, never
silently dropped; the wedge fix specifically restores the honest
"migration completes under witness faults" claim.

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
  The file's own scope note
  marks rows 13–17, 22 and 23 as end-to-end (both daemons, real peer
  HTTP); the remaining rows (18–21) drive the same two-daemon rig with
  narrower
  focuses (restore faults and coordinator re-drive, snapshot-dir refusal,
  eligibility fencing, idempotency). Fault cells inject at the `FakeVmm`
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
detection, and the resize notification from P6 (§4, D4). Normative home:
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
- [ADR-0008](../adr/0008-rook-only-hyperconverged-cells.md) — its decision
  gate: a small three-physical-host POC "must prove bare-metal VFIO claim,
  Kubernetes node registration/placement/admission, Rook OSD/MON/MGR
  deployment, RBD I/O and storage client reachability, failure recovery,
  CRUSH physical placement, resource ceilings and performance vs Rook
  directly on the same hardware," and passing nested functional tests
  alone "can only authorize physical POC, **not production acceptance**."

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

**Answer: not today — by contract in v0, and ADR-0006 (which designs it) is
Proposed / R&D-gated. Of its three designed relocation options, exactly one
is implementable now (Option A, same-VG `pvmove`); Option B stays gated
behind its own proof and Option C is rejected. The online-grow VMM
notification step is separately implementable, version-gated.**

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
    verify, source extents freed only after verified relocation).
    **Delivered in P6-C (PR #24)**: `POST /v2/volumes/{volume_id}/move-backing`
    under the advertised `same_vg_extent_move` capability —
    journal-before-mutate, the honest state subset
    `PREPARING | COPYING | COMPLETE | IN_DOUBT` (the kernel-internal
    mirror makes `MIRROR_READY`/`PIVOTED` unobservable through `lvs`;
    scope refusals are typed errors before any state exists), one fenced
    generation bump landing with the verified `COMPLETE` record, unknown
    outcomes — an out-of-band `pvmove --abort` included — parked
    `IN_DOUBT` with the source intact and new work refused typed, and no
    consumer abort route in this slice.
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
construction, and volvisor now orchestrates it: P6-C, PR #24), **yes for
online grow with
notification** (small, version-gated), and **not yet claimable for
cross-VG/pool whole-volume moves** (QSD path needs its POC first).

**Decision (D4): P6 implements the first slice of ADR-0006 —**

1. **Grow-notification**: `GrowVolume` on an attached native-local volume
   completes the VMM resize-disk step through `ChRemoteVmm` (pinned CHV
   version verified at startup, typed refusal when the version is not
   proven), with the ADR's partial-failure semantics (backend grew,
   notification failed → retry the notification, never shrink to undo).
2. **Same-VG evacuation**: implement the *already-contracted* operation —
   volume-api-v2 §4A defines `MoveVolumeBackingOnline` with the
   `same_vg_extent_move` / `same_host_live_backing_move` capability split
   and the pvmove-same-VG restriction — with exactly one qualified
   capability scope in this phase: `same_vg_extent_move` via `pvmove`,
   journal-before-mutate, source extents freed only after verified
   relocation and ownership reconciliation (the contract's
   never-generic-FAILED / `IN_DOUBT` rule applies).
   `same_host_live_backing_move` is advertised nowhere until the QSD path
   passes its acceptance suite; out-of-scope moves are refused typed
   (fail-closed, never silent — the refusal code is settled in the §7
   contract update). No new operation name is introduced.
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
| P6 | Native online operations: grow-notification + same-VG relocation (+ the P6 hardening slice: the grant_set wedge fix, both recorded test flakes, and the two recorded campaign follow-ups triaged — each fixed or explicitly declined with a recorded reason) | Completes an existing contract promise; no external dependencies; the wedge fix restores the honest "completes under witness faults" claim |
| P7 | Packaging & distribution (ADR-0009) | The installability requirement; also the vehicle every later real-host phase deploys through |
| P8 | Tier V real-VMM verification (ADR-0010), the engine-comparison consideration gate (the previous §12 numbering's P5), and the local-mirror experiment decision | Validates the migration story end-to-end against a real VMM; produces the benchmarks the new-engine comparison needs; the local-mirror experiment (§12 P3 residue) is decided here — real media/device faulting is the only honest way to fault a mirror, so it is designed and faulted on the real-host/media tier (the Tier R drive, or a Tier V host where the media is real) or explicitly declined with a recorded reason |
| P9 | Rook-cell device-sharing POC (lending surface + three-host exact-SHA scenarios) | The operator-sharing question; explicitly POC-only |
| P10 | Managed-Ceph OSD placement ADR + production gate (was P6) | Last: gated on P9's outcome and a production-support decision the evidence must carry |

Each phase gets its own detailed plan (the P5 shape: scope, stages, PR map,
completion gates) before implementation, and each PR keeps the
review-until-clean loop.

## 7. Document changes this plan authorizes (the follow-up bundle)

1. **ADR-0006** — status update: first slice accepted (grow-notification +
   same-VG `pvmove` evacuation over the contracted
   `MoveVolumeBackingOnline` surface), QSD path remains R&D-gated behind
   its acceptance suite.
2. **ADR-0008** — update: the device-lending surface decision
   (lend/reclaim on claimed devices, visible ownership, no
   double-ownership window) and its POC gate wording, matching the
   contract change in item 7.
3. **ADR-0009 (new)** — distribution & packaging (§5 above).
4. **ADR-0010 (new)** — Tier V: the real-VMM verification tier, extending
   the P5 tier/claim discipline.
5. **SPEC-0002 §12** — renumbered/extended implementation sequence (§6),
   with the local-mirror residue explicitly scheduled into P8.
6. **volume-api-v2** — no new operation: pin the P6 scope of
   `MoveVolumeBackingOnline` (§4A) to the `same_vg_extent_move`
   capability, settle the typed-refusal code for out-of-scope moves, and
   record the grow-notification semantics for attached volumes (the
   `guest_notification_status` retry rule).
7. **rook-cell-experimental-v0** — the device-lending/sharing semantics
   section + the POC evidence requirements for it.
8. **nearline-replication-v2 §10** — the Tier V line in the
   implementation-status ledger (verified-against-real-VMM is a distinct
   claim from Tier S and from production support).
9. **The post-P5 implementation plan**
   ([2026-10-10-post-p5-implementation-plan.md](2026-10-10-post-p5-implementation-plan.md))
   — the staged plan for P6–P9. The file does not exist yet; this bundle
   creates it, so the link goes live when the bundle merges.

## 8. Honesty rules carried forward

AGENTS rules 4/11/12 unchanged: nothing in P6–P9 adds a claim the evidence
does not carry. In particular: packaging does not claim production
support; Tier V does not claim real-media durability; the Rook POC does
not authorize production Rook support; same-VG relocation does not claim
cross-pool mobility. Every phase's completion gate includes its evidence
record shape, and "not run" is always distinct from "not implemented".
