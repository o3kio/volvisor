# Phase B — readiness residuals and evidence automation

Status: normative for this workstream (the plan of record; implementation
commits cite it)
Date: 2026-11-04
Branch: `phase-a-scaling` (Phase A committed at `aad1720`)
Builds on: [pre-production readiness report](../releases/pre-production-readiness.md),
[tracker #36](https://github.com/o3kio/volvisor/issues/36),
[Post-P5 implementation plan](2026-10-10-post-p5-implementation-plan.md),
[durable-state schema compatibility](../schema-compatibility.md),
[Volume API v2](../../contracts/volume-api-v2.md),
[day-2 runbooks](../runbooks/day-2/README.md),
[observability guide](../operations/observability.md),
[AGENTS rules 4, 8, 11 and 12](../../AGENTS.md).

## What this Phase B is — and is not

The repository's historical phase labels are not a single sequence. SPEC-0002
§6C and ADR-0005 use **Phase B** for *Volvisor-managed Ceph OSD placement*.
That work is gated on the Rook-cell POC GO ([#38](https://github.com/o3kio/volvisor/issues/38)),
a production-support decision and three physical hosts; none exist here. This
plan does **not** implement, claim or partially claim that Phase B, and it
does not change ADR-0008's production NO-GO.

This Phase B is the **readiness-residual and evidence-automation batch** that
can be implemented and validated honestly on the available single development
host:

- the remaining instrumentation/evidence automation where the existing issue
  work is partial ([#44](https://github.com/o3kio/volvisor/issues/44));
- executable day-2 drill and preflight automation plus the stale-schema
  documentation correction ([#46](https://github.com/o3kio/volvisor/issues/46));
- the Volvisor-side CHV consumer contract, conformance test and published
  guide ([#48](https://github.com/o3kio/volvisor/issues/48));
- reproducible flake-rate reporting and another deterministic interleaving
  test ([#49](https://github.com/o3kio/volvisor/issues/49)).

It deliberately does **not** attempt the hardware, signing, tagged-release or
wall-clock evidence that the remaining tracker issues require.

## Non-claims

Every deliverable in this plan is validation/automation or documentation. None
of it is a production-support, SLA, availability, durability or zero-RPO
statement. In particular:

- no signed tag, GitHub release, package release or CHV pin update;
- no three-physical-host nearline or Rook-cell evidence (#37, #38);
- no real Ceph or DRBD rig run (#39);
- no real 24-hour soak or published production performance number (#43);
- no real migration trace without the DRBD/peer/VMM rig (#44);
- no storage-side DRBD fencing evidence, no total-host-loss drill (#46 D3/D8);
- no pinned/supported CHV/Volvisor pair, and no CHV `active` claim (#48);
- no 14-consecutive-night flake claim and no published observed flake rate
  (#49);
- no two-maintainer sign-off (#50).

A gate that remains unmet stays explicitly unmet in
[`docs/releases/pre-production-readiness.md`](../releases/pre-production-readiness.md)
and [`SUPPORT_MATRIX.md`](../../SUPPORT_MATRIX.md).

## Slices

Each slice owns a disjoint set of files. The central plan, readiness report,
support matrix and CI workflow are consolidated by the orchestration pass, not
by a slice.

### B1 — Observability trace evidence (#44)

**Gap.** An attach trace and its ignored generator exist. A migration trace is
**NOT RUN** because no real DRBD/peer/VMM rig exists. Nothing validates that a
committed trace is well-formed, correlated by `operation_id`, or free of test
credentials.

**Deliverables.**

- a normal integration test that validates every committed
  `docs/operations/traces/*.jsonl` file structurally and semantically;
- an operator/CI trace validator script with the same checks;
- trace documentation that keeps the migration trace explicitly **NOT RUN**
  and names the exact real-rig prerequisites and capture command;
- an explicit record of the two deliberately omitted metric gauges and why no
  honest aggregate exists.

**Acceptance.** The committed attach trace passes validation; a malformed,
uncorrelated or credential-bearing trace fails; no simulated migration trace
is committed as real evidence.

### B2 — Day-2 automation and v3 correction (#46)

**Gap.** D2's live old-binary → new-binary upgrade and rollback were never
run; the existing script expected schema v2 while `LVM_STATE_SCHEMA_VERSION`
is now 3. D3 storage-side fencing and D8 remain blocked on DRBD/peer,
multi-host and BMC/fencing. There is no preflight that states those blockers
mechanically.

**Deliverables.**

- a real cross-binary D2 drill using the schema-v2 commit `8c2fc1a` as the old
  binary and the current commit as the new binary: old opens v2 → new opens
  and upgrades on write to v3 → old refuses v3 → restore the v2 backup → old
  opens and serves the volume;
- the existing same-binary `d2.sh` corrected to v3;
- a preflight script that checks and reports D2-live, D3-storage and D8
  prerequisites, with honest `NOT RUN` output by default and a strict mode for
  a real rig;
- runbook corrections for v3, the recorded D2-live result, the D3/D8 preflight
  and the unchanged blocked halves;
- an evidence bundle with exact SHAs, commands, raw output and limits.

**Acceptance.** D2 live is only marked **DRILLED** if both the old → new and
the restore → old steps actually ran and passed on this host; otherwise the
exact blocked step is recorded. D3 storage-side fencing and D8 remain **NOT
RUN** regardless of preflight output.

### B3 — CHV consumer contract and guide (#48)

**Gap.** Volvisor exposes `/v2/capabilities` and the attachment-state report
route with typed 404/replay behavior, but there is no CHV-shaped consumer
conformance test and no published integration guide. CHV itself is blocked on
a tagged Volvisor release for any pin update.

**Deliverables.**

- an integration test driving the exact CHV sequence: capability negotiation,
  create, attach (`prepared`), report `advertised` with `vmm_configured`,
  inspect, refuse `active` without `guest_confirmed`, stale-generation refusal,
  idempotent replay after an app restart, and the non-advertising typed 404
  path;
- a published integration guide with the capability token, state/evidence
  rules, request/response examples, stale-generation handling, honest version
  matrix and explicit non-claims.

**Acceptance.** The test passes in the normal workspace suite without loop
devices or a real VMM. The guide records that no CHV/Volvisor pair is pinned
and that CHV v1 does not claim `active` or live migration.

### B4 — Flake-rate reporting and interleaving evidence (#49)

**Gap.** Fuzz targets, seeds and the nightly `campaign-flake` collector exist,
but there is no reproducible report that computes a flake rate or evaluates
the 14-consecutive-night criterion, and no published observed rate.

**Deliverables.**

- a stdlib-only report tool that consumes the collector's `iterations.jsonl`
  artifacts and computes samples, failures, flake rate, distinct UTC dates,
  the longest zero-failure date run, duplicates and `criterion_met`;
- unittest coverage for the criterion edge cases, including that multiple
  samples on one date do not count as multiple nights;
- campaign-workflow wiring that publishes the current run's report while
  stating that one run is not the 14-night criterion;
- a testing guide and evidence wording that keep the observed rate **not
  published** and the criterion **NOT MET** until artifact history exists;
- at least one new deterministic journal compaction/pin interleaving test.

**Acceptance.** The report passes its unit tests and validates the fixtures;
the workflow parses; the docs do not infer nights that were not sampled and do
not turn a single green run into evidence.

## Validation gates

The orchestration pass runs, at minimum:

```sh
cargo fmt --all --check
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
cargo test -p volvisor-journal --features test-faults --locked
cargo doc --locked --no-deps --document-private-items
cargo fmt --manifest-path fuzz/Cargo.toml --all --check
cargo check --manifest-path fuzz/Cargo.toml --locked --all-targets
bash -n docs/runbooks/day-2/scripts/*.sh scripts/*.sh
shellcheck -x docs/runbooks/day-2/scripts/*.sh scripts/*.sh
python3 -m unittest discover -s scripts/tests -p 'test_*.py'
python3 scripts/validate-traces.py docs/operations/traces/*.jsonl
python3 scripts/release-scope.py check
ruby scripts/lib/check-release-graph.rb .github/workflows/release.yaml \
  .github/workflows/real-backends.yaml
```

Workflow YAML is parsed before commit. Any run that uses a dirty worktree, a
loop device or a single host says so in its evidence; it is not presented as
real-hardware or clean-SHA evidence unless the recorded source is clean.

## Commit shape

Phase B lands as a small number of logical commits on `phase-a-scaling`, after
an adversarial review loop over the full diff. Nothing is pushed and no pull
request is created by this workstream. Phase A's commits remain untouched.
