# P5 implementation plan — the aggressive failure campaign

Status: normative for the P5 phase (this document is the plan of record;
implementation PRs cite it)
Date: 2026-10-10
Builds on: [P4a plan](2026-10-09-p4-witness-fencing-authority.md) (PR #7),
[P4b plan](2026-10-09-p4b-vmm-storage-handoff.md) (PR #9 stage B1, PR #10
stage B2), [nearline contract v2](../../contracts/nearline-replication-v2.md)
§10, [SPEC-0002](../../docs/specs/SPEC-0002-volvisor-volume-virtualization.md)
test-gates table and §13, AGENTS rules 4/11/12.

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
  Tier S *exhausts the crash/fault space of the implemented logic*: every
  kill point, every injected lie, every interleaving the harness can
  reach. Tier S **cannot** prove durability on real media, real DRBD, or
  a real VMM, and says so in every artifact it emits.
- **Tier R (real-host, hardware-gated)** — the same scenario scripts
  driven against real DRBD 9 and a real Cloud Hypervisor on real
  hardware, emitting versioned evidence bundles. Tier R is *scaffolded*
  by this phase (harness, scenario portability, evidence format, env
  gating) but **cannot run** in this repository's environment: no real
  hosts, no real DRBD devices. Every Tier R scenario therefore ships as
  an env-gated test that skips (with a recorded reason) until the
  hardware exists. Production support is claimed **nowhere**; the
  campaign's completion gate includes an explicit statement of what is
  and is not proven.

This split is the plan's answer to the contract's "no claim of
production support based on simulation or successful happy path alone":
Tier S output is labeled simulation evidence; Tier R output does not
exist yet and is recorded as the open hardware gate.

## 1. Scope

In scope (Tier S, fully implemented this phase):

- The **write-trace oracle**: a continuous tagged writer across
  migrations with acknowledged-write verification — the fake worlds gain
  real data-block semantics so "the acknowledged prefix is present at
  the destination" is a checked fact, not an asserted one.
- The **kill matrix**: hard daemon kills at every journaled-mutation
  boundary of the migration path and the volume-mutation path, with
  restart → reconcile → intent-resolution recovery checked against the
  invariants (W1–W10, G1–G5) after every kill.
- The **adversarial injections**: stale source writes after the fence,
  wrong-epoch/wrong-lineage data at the target, barrier-proof lies,
  witness divergence, concurrent multi-volume cuts under fault,
  repeated abort cycles (the abort storm).
- The **independent harness**: a campaign rig that drives the daemons
  as a black box (HTTP and process control only — no coordinator
  handles, no testkit internals in the assertions), so the oracle does
  not share implementation seams with the code under test.
- The **evidence bundle**: every campaign scenario emits a structured
  record (scenario id, code commit, component versions, fault
  injection, oracle verdict, invariant checks) to a collected
  directory; a summary tool renders the coverage matrix.
- Contract/doc updates (§8 below): nearline §10 implementation-status
  note, the campaign report itself.

Out of scope (recorded, each with its reason):

- **Real-host execution of Tier R** — hardware-gated; the scaffolding
  lands, the runs do not. This is the recorded open gate for any
  production-support claim (nearline §10's final row).
- **Performance benchmarking** (§10's benchmark rows: Protocol A
  comparison, Mayastor/Ceph qualification) — a benchmark is not a
  durability proof (AGENTS implementation-order note); benchmarks
  belong to a separately scoped effort after Tier R exists.
- **Storage Cells / Ceph classes** — the campaign targets the nearline
  (DRBD) class and the handoff, the only classes with a complete
  implemented path; `native-local` and `ceph-rbd` campaign rows are
  recorded as future work once their handoff surfaces exist.
- **Multi-TB seed and growing-dirty-rate under real timing** — Tier S
  models their *logic* (resync concurrent with foreground writes,
  catch-up gating) but not their scale; scale is Tier R.
- **Fuzzing the wire codecs** — serde `deny_unknown_fields` is already
  exercised by route tests; a dedicated fuzzer is recorded follow-up,
  not campaign scope.

## 2. The oracle substrate: data blocks in the fake world

Today's FakeDrbd models *state* (roles, generations, sync status) but
not *content*. The write-trace oracle needs content, because
"acknowledged writes survive the migration" is a statement about bytes.

### 2.1 Fake-resource data blocks

`FakeResource` gains a block store: a `BTreeMap<u64, [u8; 4096]>`
(logical block index → content) per resource, plus a per-block
lineage tag (the data-generation UUID set at seed/resync time). The
simulated device files the FakeVmm opens are the same store, exposed
as read/write — a guest write to `/dev/drbdN` in the fake world is a
write to the primary resource's block map; a read at the destination
after promotion reads the replica's map.

Rules that keep the model honest (each is a tested property, not a
comment):

- **Writes land only on the Primary** — a write to a Secondary
  resource's device fails `E_ROFS`/typed error (the fake enforces the
  single-writer invariant the real stack enforces).
- **Resync copies content, not just state**: a resync from the
  `resync_baseline` copies the source's blocks (and lineage tags) to
  the peer; `durable_prefix` semantics follow the fake world's
  existing sync model — the oracle may only verify blocks the fake
  marked received/applied.
- **Out-of-band writes** (the stale-write injection, §5.1) mutate the
  source's map *after* the fence; the assertions then check the
  destination map is untouched and the witness classifier reports the
  divergence honestly (`UNSAFE`/`POSSIBLE_LOSS`, never silent
  acceptance).

### 2.2 The write-trace oracle

A campaign-side writer task:

- writes 4 KiB blocks with a tagged header (monotonic sequence, crc32,
  writer id) to the source device at a bounded rate;
- records every *acknowledged* write (the fake transport is
  synchronous: the write's return is the ack) in an in-memory journal;
- after a scenario's terminal state, verifies against the destination
  device: every acknowledged write whose sequence predates the
    scenario's recorded barrier boundary is present and crc-correct;
  no acknowledged write is *corrupted* (present with a wrong crc —
  this is the corruption check, reported as a distinct failure);
- on abort paths, verifies the **source** device still carries every
  acknowledged write (nothing was lost by the failed handoff) and
  quantifies the tail: writes acknowledged after the barrier are
  reported as tail, never as loss, unless the contract says they must
  be there (it does not — async tail loss is honest).

The oracle is a library (`volvisor-campaign` internal) with the
journal kept outside the daemons — it must not read coordinator state
to decide what should exist; only the barrier boundary timestamp
(observable via the migration's public observation route) feeds it.

## 3. The kill matrix

### 3.1 Deterministic kill points

Nearline §10 demands SIGKILL "during ACK, WAL/meta commit, dirty
bitmap, peer stream and replay". The fake-world equivalent is a kill
at every *journaled mutation boundary*. The ops pipeline already
journals intent → execute → outcome; the campaign adds a test-gated
crash hook on the daemon's journal writer:

```rust
/// Test-gated crash injection (production no-ops): the daemon
/// aborts its server task after the Nth journal write completes,
/// before the next request is served. Models a kill -9 between
/// durable mutations.
pub crash_after_writes: AtomicUsize,   // 0 = never
```

- The hook fires *after* a journal write is durably recorded (the
  crash models "the journal survived, the process did not"), and —
  for the intent-without-outcome window — also *before* the outcome
  write via a second knob (`crash_before_outcome_after_writes`).
- Production builds carry the AtomicUsize and a no-op check; there is
  no route or config that sets it — only the test rig (which
  constructs the daemon state in-process) can. This is the same
  discipline as the FakeFailKnobs: a fault matrix is test-surface,
  never operator-surface.

### 3.2 The matrix

For every journaled operation on the migration path — volume
mutations (create/attach/detach/resize), the consumer mobility routes
(prepare/transfer/abort), the peer routes (prepare/grant/
restore-vm/discard), and the witness's own journal (via its existing
stop mechanism) — the campaign runs:

1. drive the operation with `crash_after_writes = k` for k = 1..K
   (K bounded per operation, covering intent/effect/outcome windows);
2. restart the daemon (real state reload, startup reconcile, the
   retry task);
3. assert the invariant set after recovery:
   - no source resume over an unvoided barrier (G5/W9),
   - no authority without a live lease at the witness (W1–W5),
   - replay idempotency: the re-driven operation either completes or
     refuses typed, never double-executes (rule 8),
   - the oracle's acknowledged-write property (§2.2) holds,
   - the cut marker and fence residue are either resolved by the
     coordinator or durably recorded for the operator (D6a/G4).

The matrix is generated, not hand-enumerated: the campaign enumerates
the journaled operations from the router table and the migration
steps from the plan's cut order, and emits one scenario per
(operation × kill point). Scenario counts are bounded (the kill
points are clustered at the journal boundaries, not every write) —
target: the full matrix runs in the existing suite's time budget
(~60s); anything slower is a recorded follow-up.

### 3.3 What "restart" means

The e2e rig's `stop()` aborts the server task — graceful. The
campaign's kill is the same abort but fired from *inside* the journal
hook mid-operation, plus the guarantee that no shutdown handlers run
(no graceful drain, no final flush) — the in-process equivalent of
kill -9. The rig's existing state persistence (journal files, DRBD
state file, witness journal, migration records) is what recovery
reloads; the campaign asserts recovery from those artifacts only.

## 4. The independent harness (`crates/volvisor-campaign`)

A new test-support crate, deliberately outside `volvisord`'s dev
tree, containing:

- the **black-box rig**: spawns both daemons (in-process servers, but
  constructed with only public config — the campaign must not build
  custom coordinator seams), the loopback witness, and the fake
  worlds with data blocks;
- **HTTP-only driving**: every act goes through the public routes
  (admin consumer routes, peer routes with the peer token, witness
  routes with host credentials). The one white-box surface the
  campaign retains is the *fault injection* itself (crash knobs,
  world fail knobs, FakeVmm knobs) — injection must be able to reach
  inside; *assertion* must not;
- **ground truth via observation APIs**: post-state is read through
  the daemons' inspect routes and the witness's inspect (the same
  surface an operator has), plus the fake world's device files for
  the oracle (bytes are the ground truth the system exists to move);
- the **oracle** (§2.2), the **kill scheduler** (§3), the
  **injection library** (§5), and the **evidence emitter** (§6).

The existing `migration_e2e.rs` stays as the seam-level matrix (it
may use white-box resolve); the campaign is the independent layer
above it. A scenario that needs the background retry task to run
must let it run — the campaign polls the public observation route
with bounded waits, never calls resolve directly.

## 5. The adversarial injections

Each injection is a lie told to the system, paired with the honest
outcome the contracts demand:

1. **Stale source writes after the fence** (§8's
   "source-after-fence stale writes"): mid-`IN_DOUBT` (post-revoke),
   a rogue writer writes the source device out-of-band. Honest
   outcome: the destination is untouched; any subsequent promotion
   attempt of the *source* reports `UNSAFE` (fenced, divergent); the
   migration's own recovery path never resumes the source VM over
   the divergence without the operator's fenced-reconciliation.
2. **Wrong-epoch / wrong-lineage data at the target**: the fake
   world's `inject_foreign_blocks` writes blocks tagged with a
   foreign data-generation UUID into the target replica's map.
   Honest outcome: the target's verification (prepare's
   replica-level gate, the adoption checks) refuses typed;
   `ObserveHandoff` never reports `COMPLETE` over foreign data.
3. **Barrier-proof lies**: a forged `RecordBarrier` under a wrong
   epoch or foreign migration id (via the witness's own API with a
   host credential — the W8 surface). Honest outcome: the void's
   participant enumeration ignores foreign-migration barriers
   (migration-id keyed), and proof-present/epoch-mismatch refuses
   typed.
4. **Witness divergence**: the witness journal is rolled back one
   entry (simulating a lost journal write) while the daemons hold
   the newer view. Honest outcome: lease/epoch checks fail closed
   on the next mutation; nothing proceeds on a stale authority view.
5. **Concurrent multi-volume cuts under fault**: the row-22 shape
   generalized — N participants, one fails promote, the source is
   killed mid-retry, the witness restarts; the recovery converges or
   parks `IN_DOUBT` with the exact participant state reported.
6. **The abort storm**: a bounded loop (25 iterations) of
   prepare → transfer-with-fault → abort/recover with the fault
   knobs rotating through the fail matrix. Every iteration asserts
   the full invariant set; the storm's purpose is state residue —
   journal growth, marker accumulation, lease leaks — and the
   re-prepare idempotency after every recovery shape.

## 6. The evidence bundle

Every scenario emits a JSON record:

```json
{
  "scenario": "kill-matrix/peer-grant/after-intent/k=3",
  "tier": "S",
  "commit": "<git sha>", "kernel": "<uname -r>",
  "components": {"volvisord": "…", "drbd-tooling": "fake",
                 "vmm": "fake", "witness": "in-process"},
  "fault": {"kind": "crash-after-journal-write", "at": "peer-grant/after-intent"},
  "oracle": {"acknowledged": 8123, "verified_at_destination": 8123,
             "corrupted": 0, "tail": 14},
  "invariants": {"g5_no_resume_over_barrier": "pass", "…": "pass"},
  "outcome": "recovered: ABORTED",
  "duration_ms": 41
}
```

- Records land under `target/campaign-evidence/<run-id>/` (git-
  ignored); a `campaign-summary` binary (or `cargo test` finalizer)
  renders the §10 coverage matrix (fault class × scenario × verdict)
  and writes `REPORT.md` — the campaign's shippable artifact.
- Tier R records use the same schema with real component versions;
  the env-gated Tier R tests (`VOLVISOR_CAMPAIGN_TIER=R`) emit into
  the same directory. Until hardware exists they emit nothing and
  skip with a recorded reason.
- The report's header carries the claim discipline verbatim: Tier S
  proves the implemented logic's behavior under the injected fault
  space; it proves nothing about real media, real DRBD, or a real
  VMM; production support is not claimed.

## 7. Honesty and verification rules (rules 4, 11, 12)

- The oracle's pass criteria live in the campaign crate, written
  against the *contracts* (acknowledged-prefix preservation, honest
  tail reporting), not against implementation return values.
- A scenario may only assert what an operator can observe (routes,
  inspect, device bytes) plus the injected fault's own state.
- Any found violation is fixed in the implementation or the fault
  is recorded as a real limitation with its contract consequence —
  never by weakening the scenario (rule 4).
- Guest flush/FUA crash-consistency (rule 11) at the *media* level
  is Tier R; Tier S covers the flush *protocol* (the barrier and
  suspension proofs) as already tested in B1/B2 — the plan records
  the boundary explicitly in the report.
- The evidence report is the only place campaign results are
  summarized; PR descriptions cite it, not ad-hoc test counts.

## 8. Contract and documentation updates

- `contracts/nearline-replication-v2.md` §10: an
  implementation-status note — the Tier S campaign is implemented
  (with the coverage matrix reference); Tier R is scaffolded and
  hardware-gated; production support is not claimed.
- `docs/plans/2026-10-10-p5-aggressive-failure-campaign.md` (this
  document) is the plan of record; the campaign `REPORT.md` template
  lands with stage A.
- No behavior contract changes are planned — the campaign tests the
  contracts as written. Any violation it finds that *requires* a
  contract amendment goes through its own plan/PR, never silently.

## 9. Test matrix (stage acceptance)

Scenario rows are generated (§3.2); the enumerated families:

| row | family | proves |
|-----|--------|--------|
| 1 | oracle across a happy-path migration | every acknowledged pre-barrier write is at the destination, crc-correct |
| 2 | oracle across an aborted migration | the source keeps every acknowledged write; tail honestly reported |
| 3 | oracle across kill-and-recover migrations | the property survives every kill point in the cut |
| 4 | kill matrix: volume mutations | replay idempotency and fail-closed generation checks |
| 5 | kill matrix: consumer mobility routes | 202/201 replay, intent resolution by observation |
| 6 | kill matrix: peer routes | grant/restore idempotency, no half-promote called done |
| 7 | kill matrix: witness journal (via stop/restart) | authority fail-closed on divergence |
| 8 | stale source write after fence | destination untouched; source promotion is `UNSAFE` |
| 9 | wrong-epoch/lineage injection | typed refusal; never `COMPLETE` over foreign data |
| 10 | forged barrier proofs | void keys by migration; mismatch refuses typed |
| 11 | witness divergence | next mutation fails closed |
| 12 | concurrent multi-volume cut under rotating faults | convergence or exact `IN_DOUBT` participant report |
| 13 | abort storm (25 cycles, rotating faults) | no state residue, no lease leak, re-prepare idempotent |
| 14 | evidence bundle + summary render | the §10 coverage matrix exists and is truthful |

Tier R rows (env-gated, skip-with-reason until hardware): the same
families against real DRBD/CH, plus power-cut and real-SSD-loss rows
that have no Tier S equivalent.

## 10. Completion definition for P5

- **CG1**: every nearline §10 fault class is either covered by a
  Tier S scenario family or recorded as Tier R/hardware-gated with
  its reason in the report — no class is silently dropped.
- **CG2**: the oracle proves acknowledged-prefix preservation across
  complete migrations and honest tail quantification on aborts, and
  its verdicts appear in the evidence records.
- **CG3**: the kill matrix covers every journaled mutation stage of
  the migration path (the generated table is in the report), and
  every recovery asserts the full invariant set.
- **CG4**: every adversarial injection ends in a typed refusal, an
  honest `UNSAFE`/`IN_DOUBT` classification, or a pass — never a
  silent corruption or a generic success.
- **CG5**: the claim discipline holds everywhere: no artifact, test
  name, doc, or commit message claims production support or real-
  host durability from Tier S evidence; the open hardware gate is
  stated in the report and the nearline contract note.

Production support is **not** claimed by this phase. The campaign's
deliverable is proof about the implemented logic plus the scaffolding
and recorded gap for the real-host evidence that any future support
claim requires.
