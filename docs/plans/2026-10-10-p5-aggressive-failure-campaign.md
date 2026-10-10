# P5 implementation plan — the aggressive failure campaign

Status: normative for the P5 phase (this document is the plan of record;
implementation PRs cite it)
Date: 2026-10-10 (rev 2 — design review findings folded in)
Builds on: [P4a plan](2026-10-09-p4-witness-fencing-authority.md) (PR #7),
[P4b plan](2026-10-09-p4b-vmm-storage-handoff.md) (PR #9 stage B1, PR #10
stage B2), [nearline contract v2](../../contracts/nearline-replication-v2.md)
§10, [SPEC-0002](../../docs/specs/SPEC-0002-volvisor-volume-virtualization.md)
test-gates table and §13, AGENTS rules 4/11/12/16.

P4a/P4b delivered the authority layer and the coordinated migration
transaction, each with its own fault tests — but those tests prove the
scenarios their authors thought of, injected through the seams their
authors built. The implementation order's next item is the **aggressive
failure campaign**: a systematic, adversarial, largely black-box assault
on the running system whose purpose is to *find* violations, not to
confirm expected behavior. Its normative letter is nearline contract §10
("Required evidence") and SPEC-0002's acceptance-gates table; its spirit
is AGENTS rule 12: no claim is made that the evidence does not carry.

## 0. The honesty spine: two tiers, one claim discipline

The campaign is explicitly split into two tiers, and the split is the
first deliverable — nothing else may blur it:

- **Tier S (simulation)** — deterministic scenarios against the fake
  worlds (FakeDrbd, FakeVmm, loopback witness, in-process daemons).
  Tier S covers a **bounded, generated fault space of the implemented
  logic**: every journaled-mutation kill point (§3) and every injection
  in §5, at the granularity and count bounds §3.2 states — not an
  unbounded "every interleaving" claim. Interleavings beyond the
  harness's reach (real timing races, multi-process scheduling,
  media-level persistence) are Tier R or open. Tier S **cannot** prove
  durability on real media, real DRBD, or a real VMM, and says so in
  every artifact it emits.
- **Tier R (real-host, hardware-gated)** — the same scenario families
  driven against real DRBD 9 and a real Cloud Hypervisor on real
  hardware, emitting versioned evidence bundles. Tier R is *scaffolded*
  by this phase (harness, scenario portability, evidence format, env
  gating) but **cannot run** in this repository's environment: no real
  hosts, no real DRBD devices. Every Tier R scenario ships as an
  env-gated test that emits an explicit `skipped` evidence record with
  its reason (never silent absence — the coverage matrix must
  distinguish "not run, no hardware" from "not implemented").
  Production support is claimed **nowhere**; the campaign's completion
  gate includes an explicit statement of what is and is not proven.

This split is the plan's answer to the contract's "no claim of
production support based on simulation or successful happy path alone":
Tier S output is labeled simulation evidence; Tier R output does not
exist yet and is recorded as the open hardware gate.

## 1. Scope

In scope (Tier S, fully implemented this phase):

- The **write-trace oracle**: a continuous tagged writer across
  migrations with acknowledged-write verification — the fake worlds gain
  real data-block semantics (§2) so "the acknowledged prefix is present
  at the destination" is a checked fact, not an asserted one, with a
  real async peer-apply window so tails are nonzero and meaningful.
- The **kill matrix**: hard daemon kills at every journaled-mutation
  boundary of the migration path and the volume-mutation path,
  including store-save boundaries, with restart → reconcile →
  intent-resolution recovery checked against the invariants (W1–W10,
  G1–G5) after every kill.
- The **adversarial injections**: stale source writes after the fence,
  wrong-lineage data at the target, barrier-proof lies, witness
  divergence, replication partitions, concurrent multi-volume cuts
  under fault, repeated abort cycles (the abort storm).
- The **independent harness**: a campaign rig that drives the daemons
  through HTTP only (§4), so the oracle does not share driving seams
  with the code under test.
- The **evidence bundle**: every campaign scenario emits a structured
  record (scenario id, code commit, component versions, fault
  injection, oracle verdict, invariant checks, log references) to a
  collected directory; a summary tool renders the coverage matrix.

Out of scope (recorded, each with its reason):

- **Real-host execution of Tier R** — hardware-gated; the scaffolding
  lands, the runs do not. This is the recorded open gate for any
  production-support claim (nearline §10's final row).
- **Local mirror leg loss / repair / rebuild** (SPEC-0002 gate row,
  nearline §10's "mirror leg repair"/"mirror rebuild") — the DRBD
  prototype topology P3 shipped has **no local mirror** (no legs, no
  repair path, nothing implemented to fault); the mirror model and its
  Tier R gate are recorded as a follow-up requiring a mirror
  implementation first. Recorded here and in the §8 contract note, not
  silently dropped.
- **Performance benchmarking** (§10's benchmark rows: Protocol A
  comparison, Mayastor/Ceph qualification) — a benchmark is not a
  durability proof; benchmarks belong to a separately scoped effort
  after Tier R exists.
- **Saturation and multi-TB scale under real timing** (§10's
  "saturated disk/network", "multi-TB seed", "growing dirty rate" as
  *scale* phenomena) — Tier S models their *logic* (§9 row 12:
  resync concurrent with foreground writes, catch-up gating, the
  lag/lagging window) but not their scale or wall-clock timing; scale
  and saturation are Tier R.
- **Storage Cells / Ceph classes** — the campaign targets the nearline
  (DRBD) class and the handoff, the only classes with a complete
  implemented path; `native-local` and `ceph-rbd` campaign rows are
  recorded as future work once their handoff surfaces exist.
- **Epoch-at-data-level injection** — the fake data path tags blocks
  by **lineage** (data-generation UUID); there is no per-block writer-
  epoch tag to corrupt in Tier S (§5.2 records the boundary). Epoch
  *authority* is already adversarially tested at the witness (P4a W1–W7).
- **The durable dirty-bitmap/meta boundary** (§10's "SIGKILL … during
  dirty bitmap") — the implemented prototype persists **no dirty
  bitmap or resync progress**: `DrbdState` carries roles, generations
  and markers only, and the fake's resync state is in-memory. There is
  no durable bitmap-write boundary to kill against in Tier S. The
  class maps to (i) Tier S row 12: kills **during an in-flight
  resync** (the logical window — the daemon dies while content-copy
  runs; restart reconciles and re-drives it), and (ii) Tier R: real
  DRBD's bitmap persistence in kernel/meta state. Recorded here and
  in the §8 note.
- **Wire-codec fuzzing** — serde `deny_unknown_fields` is already
  exercised by route tests; a dedicated fuzzer is recorded follow-up.

## 2. The oracle substrate: data blocks in the fake world

Today's FakeDrbd models *state* (roles, generations, sync status) but
not *content*. The write-trace oracle needs content, because
"acknowledged writes survive the migration" is a statement about bytes.
This section is the linchpin of the whole campaign; its interface is
specified here to prevent implementation drift.

### 2.1 The fake data path (in `volvisor-drbd-testkit`)

`FakeResource` gains a block store: a `BTreeMap<u64, Block>` keyed by
logical block index, where

```rust
struct Block {
    payload: [u8; 4096],          // the bytes (crc-checked by the oracle)
    lineage: GiSet,               // the data-generation UUID set that wrote it
    applied_at_peer: bool,        // the peer-apply window's per-block state
}
```

The device surface is an explicit handle type (not a real file — the
oracle and the FakeVmm device hooks both open `DeviceHandle`s through
the kit's `open_device(minor)`, which enforces the rules below; the
oracle reads final bytes through the same handles, with a
`read_raw(minor)` escape hatch for post-mortem reads of a fenced or
down resource — bytes are the ground truth, and the plan says so
plainly: assertion reads the world's block maps, injection reaches
inside, but *driving* never does):

- **Writes land only on the Primary** — `open_device` on a Secondary
  resource fails typed (`E_ROFS`); the fake enforces the single-writer
  invariant the real stack enforces.
- **Suspension gates the write path** — a resource in the suspended
  set (the existing `suspend-io` state) refuses writes on its handle;
  the quiesce is real at the data path, not just a flag.
- **Openers and the busy model (rule-17 fidelity)** — the oracle's
  handle models the guest's I/O: it participates in the openers set
  exactly as the FakeVmm's device hooks do while the VM is Running
  (a held device blocks the demote), and the oracle stops writing
  when the VM pauses. A post-mortem or rogue read/write uses
  `read_raw`/`write_raw`, which never touch the openers set.
- **Acknowledgment model** — a write's return is the source-side ack
  (the fake transport is synchronous up to the source's own block
  map). This is deliberately the *Protocol C-shaped* ack; §2.3 adds
  the async window. Ack durability in Tier S means "survives in the
  fake media (the block map)" — an in-process store that survives
  daemon kills by construction; media-level durability is Tier R and
  is stated as such in every evidence record.
- **Peer apply is a lagged queue** — see §2.3.

### 2.2 The write-trace oracle

A campaign-side writer task:

- writes 4 KiB blocks with a tagged header (monotonic sequence, crc32,
  writer id) to the source device at a bounded rate;
- records every *acknowledged* write in an in-memory journal outside
  the daemons;
- after a scenario's terminal state, verifies against the destination
  device: every acknowledged write whose sequence predates the
  scenario's **data-path boundary** (§2.3) is present and crc-correct;
  no acknowledged write is *corrupted* (present with a wrong crc — a
  distinct failure class in the verdict);
- on abort paths, verifies the **source** device still carries every
  acknowledged write, and quantifies the tail honestly (§2.3).

### 2.3 The boundary and the async window (the non-circularity rules)

The oracle's normative letter is only meaningful if two things are
real; both are deliverables of stage A:

1. **The boundary comes from the data path, not the coordinator.** The
   oracle derives the boundary as the last write sequence that
   *succeeded* before its writes began failing — the suspension or the
   role flip the migration actually caused at the device. The
   coordinator's own `BARRIER_DURABLE` history timestamp is then
   **cross-checked against it** (a coordinator that records the
   barrier *early* — before the last write it must cover was
   acknowledged — is the violation this check exists for: writes slip
   past the barrier's protection, and the skew reads negative. The
   opposite failure, a barrier recorded *late* — after a tail it
   should have covered — is invisible to the skew by construction and
   surfaces instead as missing blocks in the byte verdict; the two
   checks together cover both directions). The oracle never takes the
   coordinator's word for what should exist.
- **The peer-apply window is real, and the gate cannot lie while it is
  open.** The fake gains an async replication queue: a source-side
  write is acked when it lands in the source's block map, and is
  applied to the peer's map only when the queue drains. **The coupling
  rule (stage A's deliverable, the anti-circularity hinge):** the
  data-bearing status tokens the convergence gate reads
  (`peer_disk == UpToDate`, "no active resync" — what
  `track_sync`/`replica_caught_up` observe) are **derived from the
  queue's block-apply state**, not set independently — a resource
  reads `UpToDate` only when the queue is fully drained, and
  "resyncing" is true while content-copying is in flight.
  (`connected` remains the independent link-state knob — the
  partition injection of §5.5.) Post-barrier
  drain is performed by the fake's content-copying resync (the system
  path, §2.4), never by the campaign; the campaign's queue control
  (`apply_peer_writes(minor, up_to)`) exists only to shape the
  **pre-quiesce lag**. The contract consequence is modeled exactly
  per AGENTS rule 16: the *acknowledged* set may exceed the
  *peer-applied* set during steady state; the barrier path (the
  existing suspension + `track_sync` convergence + resync) is what
  closes the gap before the cut, and the oracle asserts it did —
  `COMPLETE` with target blocks missing behind the data-path boundary
  is a **caught violation** (and because the gate reads the queue, it
  is a violation in volvisor's convergence logic, not a harness
  artifact), and an abort's honestly reported tail may be genuinely
  nonzero.

### 2.4 Resync and out-of-band writes

- **Resync copies content, not just state**: a resync from the
  `resync_baseline` copies the source's blocks (payload + lineage) to
  the peer; the oracle may only verify blocks the kit marked
  received/applied (§9 row 12 runs foreground writes concurrent with
  it).
- **Out-of-band writes** (the stale-write injection, §5.1) mutate the
  source's map directly through `write_raw` — bypassing the enforced
  `DeviceHandle` path *is the point*: the rogue writer models an actor
  below volvisor's enforcement. The kit exposes `write_raw` for this
  and nothing else.

## 3. The kill matrix

### 3.1 Deterministic kill points

Nearline §10 demands SIGKILL "during ACK, WAL/meta commit, dirty
bitmap, peer stream and replay". The fake-world equivalents are kills
at the durable-mutation boundaries:

- **The journal append hook** — in `volvisor-api`'s ops pipeline
  (`ops.rs::execute_resolvable`, under the existing journal mutex),
  where intent and outcome frames are written. The hook is a
  per-operation **armed table**, not a global counter (a global count
  cannot deterministically target "peer-grant after its intent write"
  in a two-daemon world with background tasks): the rig arms
  `{operation key → crash point}` pairs; the pipeline consults it
  after each durable write of a matching operation and aborts the
  serve task group (§3.3) at the armed point. Crash points: after
  intent, before outcome, after outcome.
- **The store-save hook** — the "meta commit" windows the contract
  means: kills *inside* the migration store's save (between tmp write,
  fsync and rename) and `DrbdState`'s save. The stores gain the same
  test-gated armed-abort seam (a crash between fsync and rename leaves
  the old durable state — the recovery must handle exactly that).
- **The witness's own mutations** — via the existing stop/restart
  (its journal is the durable surface) plus a mid-save variant of the
  same store hook on the witness journal.

Both hooks are `AtomicBool`/table test surfaces in production crates —
the `FakeFailKnobs` precedent — doc-gated with their trust class
recorded: no route, config or input path can set them; only the
constructing test rig can (the modules' doc comments say so, and a
doc-gate test asserts the knobs default to inert).

### 3.2 The matrix

For every journaled operation on the migration path — volume mutations
(create/attach/detach/resize), the consumer mobility routes
(prepare/transfer/abort), the peer routes (prepare/grant/restore-vm/
discard), and the witness's journal — the campaign runs:

1. drive the operation with each armed crash point (intent/effect/
   outcome windows; store-save splits);
2. kill (§3.3), restart the daemon (real state reload, startup
   reconcile, fresh background tasks);
3. assert the invariant set after recovery:
   - no source resume over an unvoided barrier (G5/W9),
   - no authority without a live lease at the witness (W1–W5),
   - replay idempotency: the re-driven operation either completes or
     refuses typed, never double-executes (rule 8),
   - the oracle's acknowledged-write property (§2.2) holds,
   - the cut marker and fence residue are either resolved by the
     coordinator or durably recorded for the operator (D6a/G4).

The matrix is generated, not hand-enumerated. **Bounds, stated here
(they are part of the honesty spine, §0):** per operation family the
kill points are the journaled boundaries (typically 3–6 per operation,
plus 2 store-save splits), not every journal write; the full matrix is
budgeted per family (kill families ≤ 5s each, storm ≤ 10s, oracle
families ≤ 5s — except row 3, the kill-and-recover oracle rows, ≤ 8s
per the amendment below; the whole campaign inside the existing suite
budget ~60s) — anything over budget is a recorded follow-up with its
uncovered points enumerated in the report, never a silent cut.

*Amendment (P6-A part 1, PR #21):* row 3's bound is 8s, not 5s. The
row's peer-grant cells (K4/K5) recover through the **production
migration retry task**, whose tick is 5s — the recovery path the row
exists to prove, not an accident to optimize away — so the cell's
designed duration is one full tick plus the re-drive and the bounded
observation poll (measured 4955–4960ms isolated; 5031ms in one
full-suite run — the load excursion that disclosed the issue). The
original 5s bound sat on top of that designed wait with zero headroom,
which made the budget a tightrope instead of a budget. The amended
bound is derived: one full retry tick (5s) plus ~3s of re-drive,
observation and load headroom.

### 3.3 What "kill" means (the task-group model)

A real `kill -9` removes the process: server, background tasks, locks,
in-memory state — all at once. The campaign's kill must model that, so
the rig's daemon is a **task group** and the group is enumerated
completely (a supervisor that misses the mutation engine is not a kill
model):

- the **serve task** (the axum listener),
- the **lease renewal task** — production's `spawn_renewal_task` is
  private and detached, so the rig re-implements the renewal loop over
  the public provider `renew_leases` surface under its own supervisor
  (consistent with §4's import discipline: the campaign crate uses no
  `volvisord` internals beyond the exported constructors),
- the **migration retry task** (`spawn_migration_retry_task`, already
  `pub`, returns an abortable `JoinHandle`),
- the **transfer drive task** — the detached task `transfer` spawns to
  perform the actual cut. Stage A includes one small, behavior-neutral
  runtime change for this: the drive task is spawned into a **tracked
  registry** on the migration handle (the `JoinHandle` is recorded;
  completed handles are dropped when the drive finishes, so the
  registry never grows; nothing else changes — graceful paths behave
  identically), so the supervisor can abort it with the group. Without this, a mid-cut kill
  would leave a live drive task mutating witness and migration state
  after the "kill" — an interleaving a real `kill -9` cannot produce.
- **in-flight request tasks**: axum handler tasks are not children of
  the serve future. The rig's driving discipline makes this
  deterministic: scenarios drive **one request at a time**, so the
  only in-flight request at kill time is the one whose journal hook
  fired the kill — aborting the group from inside that request is
  exactly a process dying mid-handler. Concurrent-request interleaving
  at kill time is out of the deterministic matrix's scope (recorded;
  it is a scheduling race, Tier R's territory).

The kill fires from inside the armed hook mid-operation — no shutdown
handlers, no graceful drain. Aborting the group releases the
journal's flock (no lingering `Arc` into the shared state holds it —
verified: the `Journal` drops its exclusive lock with the value), so
the restart's re-open succeeds exactly as a real process restart's
would. What survives is exactly what a process kill leaves: the
journal files, the DRBD state file, the witness journal, the migration
records, the fake worlds' block maps. Recovery is asserted from those
artifacts only.

The in-process residue that a task abort *would* leave but a process
kill would not (e.g. the fake worlds living in the same process) is
recorded in §0's Tier S statement: Tier S kills volvisor's tasks, not
the host.

## 4. The independent harness (`crates/volvisor-campaign`)

A new test-support crate, deliberately outside `volvisord`'s dev tree,
containing the rig, the oracle, the kill scheduler, the injection
library and the evidence emitter.

**The composition boundary, stated honestly.** There is no public
`Config` that yields a runnable migration-surface daemon in this
environment (a real drbd provider requires the toolchain and devices;
migration requires the drbd provider). The campaign rig therefore
composes daemons the way `migration_e2e.rs` already does — from the
exported constructors (`wire_migration`, `AppState::new`, `router`,
`Journal::open`, the witness state, the testkit fixtures) over the
injected fake worlds, `FakeVmm`s, and a shared frozen clock (the
deterministic timing the W7 windows, lease expiry and retry loops
need). That composition is the same surface the seam matrix uses; the
campaign's independence is **not** claimed from construction. It comes
from the three disciplines above the composition:

- **HTTP-only driving**: every act goes through the public routes
  (admin consumer routes, peer routes with the peer token, witness
  routes with host credentials). The campaign never calls coordinator
  methods, driver methods or resolve directly; the background retry
  task is the recovery engine, polled through the public observation
  route with bounded waits.
- **Bytes as ground truth**: post-state assertions read the device
  maps (§2.1) and the observation routes an operator has — never
  coordinator internals.
- **Generated scenarios**: the matrix comes from the router table and
  the cut order (§3.2), not from hand-picked paths.

Injection reaches inside (crash hooks, world fail knobs, `write_raw`,
clock); assertion and driving do not. This split is the harness's
contract, tested by construction (the campaign crate imports no
`volvisord` internals beyond the constructors).

## 5. The adversarial injections

Each injection is a lie told to the system, paired with the honest
outcome the contracts demand:

1. **Stale source writes after the fence** (§8's
   "source-after-fence stale writes"): mid-`IN_DOUBT` (post-revoke),
   a rogue writer writes the source's block map via `write_raw`
   (deliberately below the enforced `DeviceHandle` path — the point
   is an actor volvisor cannot see). Honest outcome: the destination
   is untouched; any subsequent promotion attempt of the *source*
   reports `UNSAFE` (fenced, divergent); the migration's own recovery
   never resumes the source VM over the divergence without fenced
   reconciliation.
2. **Wrong-lineage data at the target**: the kit's
   `inject_foreign_blocks` writes payload tagged with a foreign
   data-generation UUID into the target replica's map (right epoch,
   wrong lineage — the shape a botched seed or a stale replica
   presents). Honest outcome: the target's verification (prepare's
   replica-level gate, adoption checks) refuses typed;
   `ObserveHandoff` never reports `COMPLETE` over foreign data.
   (Writer-*epoch* divergence at the data level has no Tier S
   equivalent — no per-block epoch tag exists — and is recorded in
   §1; epoch authority is adversarially covered at the witness.)
3. **Barrier-proof lies**: a forged `RecordBarrier` under a wrong
   epoch or foreign migration id (via the witness's own API with a
   host credential — the W8 surface). Honest outcome: the void's
   participant enumeration ignores foreign-migration barriers
   (migration-id keyed), and proof-present/epoch-mismatch refuses
   typed.
4. **Witness divergence**: the witness journal is rolled back one
   entry (simulating a lost journal write) while the daemons hold
   the newer view. Honest outcome: lease/epoch checks fail closed on
   the next mutation; nothing proceeds on a stale authority view.
5. **Replication partition** (§10's "storage network disconnect" —
   the fake world's existing `peer_online` flag): a mid-migration
   partition between the peers. Honest outcome: the convergence gate
   (`replica_caught_up`/`track_sync`) never reports caught-up over a
   partition; the cut refuses to proceed (or parks honestly) until
   the partition heals; foreground writes continue acking at the
   source (rule 16's async semantics) and the oracle's tail claim
   covers exactly them.
6. **Concurrent multi-volume cuts under fault**: the row-22 shape
   generalized — N participants, one fails promote, the source is
   killed mid-retry, the witness restarts; the recovery converges or
   parks `IN_DOUBT` with the exact participant state reported.
7. **The abort storm**: a bounded loop (25 cycles) of prepare →
   transfer-with-**pre-cut**-fault → abort → verify-recovery. The
   faults rotate through the pre-cut fail matrix only (an abort at or
   past the cut is a typed refusal by construction — G1/D1a — and
   forward-completion under post-cut faults is §9 row 12's business,
   not the storm's). Each cycle uses a **fresh migration id** and
   first asserts the previous cycle's terminal record is intact
   (terminal `Aborted`/`Complete` records are immutable facts; a
   re-prepare under the *same* id is the idempotency-conflict typed
   refusal already pinned by B2 tests). The storm's purpose is state
   residue — journal growth, marker accumulation, lease leaks — and
   recovery-idempotency across every fault shape.

## 6. The evidence bundle

Every scenario emits a JSON record:

```json
{
  "scenario": "kill-matrix/peer-grant/after-intent",
  "tier": "S",
  "commit": "<git sha>", "kernel": "<uname -r>",
  "components": {"volvisord": "…", "drbd-tooling": "fake",
                 "vmm": "fake", "witness": "in-process"},
  "fault": {"kind": "crash-after-journal-write", "at": "peer-grant/after-intent"},
  "oracle": {"acknowledged": 8123, "verified_at_destination": 8123,
             "corrupted": 0, "tail": 14,
             "boundary_source": "data-path"},
  "invariants": {"g5_no_resume_over_barrier": "pass", "…": "pass"},
  "logs": {"journal": "target/campaign-evidence/<run>/logs/a.journal",
           "migration_records": "…", "witness": "…"},
  "outcome": "recovered: ABORTED",
  "duration_ms": 41
}
```

- Records land under `target/campaign-evidence/<run-id>/` (git-
  ignored), with per-scenario **log capture**: the daemon journals,
  migration records and witness journal are copied/snapshotted at
  scenario end (nearline §10's "full logs"); the record references
  them by path.
- A `campaign-summary` renderer emits the §10 coverage matrix (fault
  class × scenario × verdict, with per-family durations and budget
  adherence) and writes `REPORT.md` — the campaign's shippable
  artifact.
- Tier R scenarios run only under `VOLVISOR_CAMPAIGN_TIER=R`; without
  it they emit a `{"scenario": …, "tier": "R", "outcome": "skipped",
  "reason": "no real DRBD/CH hardware in this environment"}` record —
  the matrix shows the gate, not a hole.
- The report's header carries the claim discipline verbatim: Tier S
  proves the implemented logic's behavior under the bounded injected
  fault space (§0/§3.2); it proves nothing about real media, real
  DRBD, or a real VMM; production support is not claimed.

## 7. Honesty and verification rules (rules 4, 11, 12, 16)

- The oracle's pass criteria live in the campaign crate, written
  against the *contracts* (acknowledged-prefix preservation, honest
  tail reporting), with the boundary derived from the data path
  (§2.3), never from the implementation's own records.
- A scenario may only assert what an operator can observe (routes,
  inspect) plus device bytes and the injected fault's own state.
- Any found violation is fixed in the implementation or the fault is
  recorded as a real limitation with its contract consequence —
  never by weakening the scenario (rule 4).
- Rule 16 discipline: the fake's ack model is source-side; no
  zero-RPO claim is made or tested for asynchronous acknowledgement;
  the peer-apply window makes the tail explicit.
- Guest flush/FUA crash-consistency (rule 11) at the *media* level is
  Tier R; Tier S covers the flush *protocol* (the barrier and
  suspension proofs — now gated at the data path, §2.1) as tested in
  B1/B2. The plan records the boundary explicitly in the report.
- The evidence report is the only place campaign results are
  summarized; PR descriptions cite it, not ad-hoc test counts.

## 8. Contract and documentation updates

- `contracts/nearline-replication-v2.md` §10: an
  implementation-status note — the Tier S campaign is implemented
  (with the coverage-matrix reference); Tier R is scaffolded and
  hardware-gated (skipped records emitted, not silence); production
  support is not claimed; and the §10 rows this campaign does **not**
  deliver are enumerated in the note and point at the report's
  recorded gates: benchmarks, local mirror leg loss/repair/rebuild
  (no mirror implementation exists to fault), saturation/multi-TB
  scale, media-level flush/FUA and power loss, the durable
  dirty-bitmap/meta boundary (no persisted bitmap exists; Tier S
  covers kills during in-flight resync only), Storage Cell crash
  (no cell class in the campaign), and O3K control-plane disconnect
  (covered in Tier S only through the witness-divergence proxy,
  row 11).
- This document is the plan of record; the campaign `REPORT.md`
  template lands with stage A.
- No behavior contract changes are planned — the campaign tests the
  contracts as written. Any violation it finds that *requires* a
  contract amendment goes through its own plan/PR, never silently.

## 9. Test matrix (stage acceptance)

Scenario rows are generated (§3.2); the enumerated families and the
§10 mapping (stated in the plan so CG1 is reviewable now, not only in
the report):

| row | family | nearline §10 / SPEC-0002 class | proves |
|-----|--------|-------------------------------|--------|
| 1 | oracle across a happy-path migration | write-trace oracle, planned migration | every acknowledged pre-boundary write is at the destination, crc-correct; boundary cross-check holds |
| 2 | oracle across an aborted migration | write-trace oracle, tail under abort | the source keeps every acknowledged write; the tail (peer-apply lag) is honestly reported, never called loss or silently zero |
| 3 | oracle across kill-and-recover migrations | SIGKILL during WAL/meta commit | the property survives every kill point in the cut |
| 4 | kill matrix: volume mutations | SIGKILL during ACK/WAL commit | replay idempotency, fail-closed generation checks |
| 5 | kill matrix: consumer mobility routes | SIGKILL during WAL commit | 202/201 replay, intent resolution by observation |
| 6 | kill matrix: peer routes | SIGKILL during peer stream/replay | grant/restore idempotency, no half-promote called done |
| 7 | kill matrix: witness journal (stop/restart + mid-save) | SIGKILL during meta commit, quorum loss window | authority fail-closed on divergence; the grant_set mid-commit windows heal — the re-issued peer act converges, epoch exactly 2, prefix byte-exact (the P6-A part 3 wedge fix; the park was the recorded stage-B finding) |
| 8 | stale source write after fence | source-after-fence stale writes | destination untouched — through the witness-down window AND through the healed completion (the wedge fix's convergence); source promotion is `UNSAFE`; no route resumes the source |
| 9 | wrong-lineage injection at the target | wrong-epoch data injection (lineage-shaped, §1) | typed refusal; never `COMPLETE` over foreign data |
| 9b | the same injection landing after prepare, mid-drive (P6-A F1 delivery: the barrier-time lineage re-check) | the post-prepare window of the same lineage-shaped injection | typed refusal at the barrier, pre-cut: the record parks carrying the typed marker, the source stays fenced/intact, the cut never crosses foreign data |
| 10 | forged barrier proofs | wrong-epoch barrier injection | void keys by migration; mismatch refuses typed |
| 11 | witness divergence | control-plane disconnect / stale authority | next mutation fails closed |
| 12 | concurrent multi-volume cut + resync-under-foreground + source-VMM death mid-cut + kills during an in-flight resync (the dirty-bitmap window) | multi-disk final cut; resync while foreground continues; VMM crash; SIGKILL during dirty bitmap (logical window — §1 records the durable boundary as Tier R) | convergence or exact `IN_DOUBT` participant report; the convergence gate never lies over a partition, an in-flight resync, or an un-drained apply queue |
| 13 | replication partition mid-migration | storage network disconnect | no caught-up claim over a partition; cut refuses/parks; async tail covered |
| 14 | abort storm (25 cycles, rotating pre-cut faults) | repeated migration aborts | no state residue, no lease leak, terminal records immutable, fresh-id recovery idempotent |
| 15 | evidence bundle + summary render | exact versions, independent harness, full logs | the coverage matrix exists, is budget-adherent, and the completion gates pass over it |

Tier R rows (env-gated, `skipped` records until hardware): the same
families against real DRBD/CH, plus the rows with no Tier S
equivalent — power cut, real SSD loss, media-level flush/FUA,
saturation and multi-TB scale, local mirror leg loss (once a mirror
implementation exists), the durable dirty-bitmap/peer-stream meta
boundaries, and real kill-timing races (concurrent requests at kill
time, scheduling interleavings).

## 10. Staging (PR map)

The house per-stage adversarial review process applies:

- **Stage A (PR: oracle substrate + harness)**: §2 in full — the
  fake data path (blocks, lineage, handles, suspension gating, the
  peer-apply queue with the gate-coupling rule), the boundary rules,
  the oracle library, the campaign rig (the complete task-group kill
  model of §3.3, including the behavior-neutral drive-task registry
  change and the rig-side renewal loop), rows 1–3 (row 3 implements
  the armed-hook subset it needs — the full generated matrix is
  stage B's), the evidence
  emitter + `REPORT.md` template.
- **Stage B (PR: the kill matrix)**: §3 — the armed hooks (journal
  append, store-save, witness), the generated matrix, rows 4–7.
- **Stage C (PR: the adversarial injections)**: §5 — rows 8–14
  (including the partition and the storm).
- **Stage D (PR: evidence + contract)**: the summary renderer, the
  Tier R env-gated scaffolding with skip records, the nearline §10
  implementation-status note, the final report.

## 11. Completion definition for P5

- **CG1**: every nearline §10 / SPEC-0002 fault class is either
  covered by a Tier S scenario family (the §9 table), or recorded as
  Tier R/hardware-gated or out-of-scope **with its reason in §1/§8
  and the report** — the §9 mapping makes this checkable in the plan,
  not only after implementation.
- **CG2**: the oracle proves acknowledged-prefix preservation across
  complete migrations and honest tail quantification on aborts, with
  the boundary derived from the data path and cross-checked against
  the coordinator's records; verdicts appear in the evidence records.
- **CG3**: the kill matrix covers every journaled mutation stage of
  the migration path at the §3.2 bounds (the generated table and the
  budget adherence are in the report), and every recovery asserts the
  full invariant set.
- **CG4**: every adversarial injection ends in a typed refusal, an
  honest `UNSAFE`/`IN_DOUBT` classification, or a pass — never a
  silent corruption or a generic success.
- **CG5**: the claim discipline holds everywhere: no artifact, test
  name, doc, or commit message claims production support or real-host
  durability from Tier S evidence; the open hardware gate (and the
  enumerated not-delivered §10 rows) is stated in the report and the
  nearline contract note.

Production support is **not** claimed by this phase. The campaign's
deliverable is proof about the implemented logic plus the scaffolding
and recorded gap for the real-host evidence that any future support
claim requires.
