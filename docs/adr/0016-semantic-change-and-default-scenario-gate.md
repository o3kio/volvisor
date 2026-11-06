# ADR-0016 — Durable-semantics changes require an ADR, a migration and a default scenario

Status: Proposed (decision recorded; enforcement via the default scenario
catalog and CI pending acceptance)
Decision-accepted: pending (record acceptance date and accepting authority here)
Date: 2026-11-06
Related: [ADR-0012](0012-monotonic-attachment-generation-fence.md),
[ADR-0013](0013-durable-attachment-generation-allocator.md),
[ADR-0014](0014-journal-bounds-and-exhaustion-contract.md),
[ADR-0015](0015-write-path-critical-section-invariant.md),
[schema compatibility policy](../schema-compatibility.md),
[SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md),
[Phase F evidence](../benchmarks/2026-11-06-devhost-phase-f-write-path-remainder/REPORT.md),
issues [#62](https://github.com/o3kio/volvisor/issues/62),
[#63](https://github.com/o3kio/volvisor/issues/63),
[#64](https://github.com/o3kio/volvisor/issues/64)

## Context

Volvisor's durable state is authoritative recovery input. A running daemon
may hold state written by an older binary; a newer binary may be rolled back;
an operator may restore a backup; a crash may leave a durable intent that the
next start must finish or refuse. A change that looks local — adding a field,
changing an allocator, tightening a retention bound — can reinterpret
existing durable state or make an old daemon misread a new file.

The existing rules already require journal-before-mutate, fail-closed
refusals and schema N-1 fixtures (AGENTS rule 8,
[docs/schema-compatibility.md](../schema-compatibility.md)). What has been
missing is a single gate that says: if a change touches durable semantics,
the change must carry a decision record, a compatibility story and an
executable scenario, and the default suite must be green before merge.

Phase F exposed exactly this risk twice: the attachment generation resets on
delete + same-id recreate (ADR-0013), and the journal's configured bounds are
soft while a persistent compaction failure can grow the log (ADR-0014).
Neither is a bug introduced by Phase F, but both show that a durable
semantic can drift while functional tests stay green.

## Decision

**A change that alters durable semantics must ship as a reviewed decision,
not as an incidental code edit.** The gate applies when a change touches any
of:

- an on-disk schema, record version, or the meaning of an existing field;
- generation, epoch, fence, allocator or tombstone semantics;
- journal retention, compaction, replay, idempotency or expiry behavior;
- crash recovery, intent adoption, compensation or startup reconciliation;
- the write-path critical-section invariant in ADR-0015;
- configuration that changes a durable bound or its failure behavior.

Such a change must include all of the following in the same change set:

1. **A decision record.** An ADR (or an amendment to an existing ADR) that
   states the new semantics, the reason, the alternatives and the non-claims.
   The ADR may be `Proposed` while implementation proceeds, but the change
   must cite it and the readiness record must keep the residual visible.
2. **A compatibility and migration statement.** State what an older binary
   does with the new state and what the new binary does with old state. A
   schema change bumps the version and adds an N-1 fixture per the
   [compatibility policy](../schema-compatibility.md). A meaning change
   without a version bump is allowed only when old and new binaries interpret
   every existing byte identically.
3. **A fail-closed refusal path.** Every new state or bound must say what
   happens at the limit, on a partial write, on a stale generation and on a
   replay conflict. Silent drop, best-effort eviction and "should not happen"
   are not acceptable answers.
4. **A default scenario.** At least one exact test in the
   [default integration scenario catalog](../testing/default-integration-scenarios.md)
   proves the new semantics and its refusal path. The scenario is added to
   the catalog and passes through `scripts/test-default-scenarios.sh`.
5. **Rollback and replay evidence.** State how an operator returns to the
   previous binary (state restore plus matching binary, per the compatibility
   policy) and what replay does with a durable intent written by the new
   code. The evidence is a test, a drill or a recorded run; a plan alone is
   not evidence.
6. **A green default suite.** `cargo fmt --all --check`, clippy with
   `-D warnings`, `cargo test --workspace --locked` and
   `scripts/test-default-scenarios.sh` pass at the change's SHA.

A change that cannot satisfy an item must say so explicitly, keep the
affected capability out of the default path or behind a documented refusal,
and record the gap in the readiness report. It must not quietly ship a
best-effort behavior.

## Why this is not bureaucracy

- **It targets a real failure mode.** The cost of a durable semantic change
  is paid at recovery time, often on a different host, binary and operator.
  The checklist forces that cost to be considered before merge.
- **It is proportional.** A typo fix or a pure refactor needs none of it.
  The trigger is a change to durable meaning or failure behavior.
- **It is executable.** The default scenario requirement is enforced by CI,
  not by a prose promise. A scenario that is renamed or ignored fails the
  gate.
- **It preserves existing rules.** It adds a decision-and-evidence gate; it
  does not weaken journal-before-mutate, fencing, idempotency or
  fail-closed behavior.

## Enforcement

- `docs/testing/default-integration-scenarios.md` is the normative catalog.
  `docs/testing/default-integration-scenarios.tsv` is its machine-readable
  test list. `scripts/test-default-scenarios.sh` verifies that every exact
  test exists, runs it with `--exact`, and fails if it is missing, ignored or
  not passing.
- The `check` job in `.github/workflows/ci.yaml` runs the script on every
  change, after the workspace test suite.
- `CONTRIBUTING.md` and `AGENTS.md` state the rule; the pull-request
  checklist asks for the ADR link, the migration statement and the scenario
  ID.
- Removing or renaming a catalog test requires the same review as changing
  the semantics it protects.

## Consequences

- Durable-semantics changes are slightly slower to land, by design.
- The catalog becomes a maintained surface. A stale test fails CI instead of
  silently disappearing.
- The catalog is control-plane evidence, not a production-support claim. A
  green default suite does not qualify a real backend, a 24-hour soak or a
  multi-host deployment.

## Alternatives rejected

1. **Rely on reviewers to remember the rules.** Rejected: the Phase F
   residuals show that correctness-relevant semantics can survive many green
   functional runs.
2. **A mandatory ADR for every change.** Rejected: too broad; it would bury
   the decisions that matter and encourage boilerplate.
3. **A prose-only default-scenario convention.** Rejected: a renamed or
   `#[ignore]`d test would pass review silently. The exact-name runner is
   required.
4. **Gate on a clean-host benchmark for every change.** Rejected: wrong
   instrument. Durability and compatibility are proven by deterministic
   tests and drills, not by throughput measurement.
