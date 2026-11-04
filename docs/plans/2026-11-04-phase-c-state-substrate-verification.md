# Phase C — state-substrate verification and hardening

Status: normative for this workstream (the plan of record; implementation
commits cite it)
Date: 2026-11-04
Branch: `phase-a-scaling` (Phase A committed at `aad1720`, Phase B at `d05023f`)
Builds on: [Phase B plan](2026-11-04-phase-b-readiness-residuals.md),
[pre-production readiness report](../releases/pre-production-readiness.md),
[tracker #36](https://github.com/o3kio/volvisor/issues/36),
[control-plane benchmark bundle](../../docs/benchmarks/2026-11-03-devhost/REPORT.md),
[configuration reference](../configuration.md#journal-retention-and-compaction-journal),
[AGENTS rules 4, 8, 11 and 12](../../AGENTS.md).

## What this Phase C is — and is not

The repository's phase labels are not one sequence. SPEC-0002 §6C and
ADR-0005 use **Phase B** for *Volvisor-managed Ceph OSD placement*; the
CellHV repositories use **Phase C** for their standalone runtime. Neither
is this document.

This Phase C is the **state-substrate verification batch** that follows the
Phase A scaling work. Phase A landed three bounded-growth and read-path
changes and opened the corresponding measurement issues
([#62](https://github.com/o3kio/volvisor/issues/62),
[#63](https://github.com/o3kio/volvisor/issues/63),
[#64](https://github.com/o3kio/volvisor/issues/64)), but the only published
control-plane bundle predates those changes (`e3aa365`). The missing work is
therefore verification, not another feature:

- close two review findings in the new journal compaction path;
- add reproducible before/after comparison tooling;
- re-run the single-host control-plane benchmark on the fixed tree and
  publish an honest comparison bundle;
- record the result in the readiness ledger without changing any gate.

It does **not** attempt the hardware, signing, tagged-release, multi-night
or multi-host evidence that the remaining tracker issues require.

## Non-claims

Every deliverable is code hardening, measurement automation, single-host lab
measurement or documentation. None of it is a production-support, SLA,
availability, durability or zero-RPO statement. In particular:

- no 24-hour soak; the published run remains a shortened soak on one host;
- no real Ceph, DRBD, physical-disk LVM, data-path or multi-host evidence;
- no production capacity number, no SLA and no regression guarantee outside
  the measured host;
- no tag, GitHub release, push or pull request;
- no claim that #62/#63/#64 are "fixed in production" beyond the measured
  single-host behavior;
- no change to the five readiness gate verdicts.

## Slices

### C1 — compaction hardening (review findings)

**Gap.** An adversarial review of `f38a08d` found two real cost defects in
the new compaction path:

1. a persistently failing compaction (for example an unopenable compaction
   target or ENOSPC) was retried on **every** append, deep-cloning and
   serializing the whole retained registry under the writer lock while the
   log kept growing;
2. `is_tombstoned` linearly scanned the tombstone ring (default 65,536
   entries) on every first-seen append and on every replayed record.

**Deliverables.**

- a bounded exponential retry backoff for failed compactions that still
  retries promptly after a transient failure and never fails a durable
  append;
- an O(1) tombstone membership index kept consistent with the ordered ring
  used for snapshot serialization, with unchanged no-false-negative
  semantics;
- deterministic regression tests for attempt bounding, duplicate/refresh,
  eviction/re-insertion, snapshot round-trip and brute-force membership
  equivalence.

**Acceptance.** The journal, retention, schema and fail-closed tests pass
with and without `test-faults`; a blocked compaction target produces
bounded attempts; tombstone membership matches the old semantics exactly.

### C2 — before/after comparison tooling

**Gap.** There is no reproducible way to compare two benchmark run
directories, so a post-fix run cannot be related to the published pre-fix
bundle without hand-copied numbers.

**Deliverables.**

- a stdlib-only `scripts/bench/compare.py` that verifies both
  `MANIFEST.sha256` files, reads the real aggregate JSON shapes, and emits
  CSV and Markdown comparisons;
- explicit `NOT RECORDED` handling, zero-baseline handling and per-minute
  normalisation when soak durations differ;
- unit tests for manifest tampering, parsing, normalisation and missing
  metrics; documentation in `scripts/bench/README.md`.

**Acceptance.** The tool refuses a tampered or unreadable run, never invents
a metric, and produces a comparison for the published pre-fix bundle.

### C3 — post-fix measurement and evidence

**Gap.** The published bundle measures `e3aa365`, before the Phase A
changes. No post-fix run exists.

**Deliverables.**

- a clean-tree run of `scripts/bench/run-all.sh` on the Phase C commit
  (release build, 1/8/32/128 sweep, 25-minute shortened soak with restart);
- a published bundle under `docs/benchmarks/` with the raw JSON/CSV,
  `MANIFEST.sha256`, a generated before/after comparison and an
  operator-authored `REPORT.md` recording the exact SHAs, commands, numbers
  and limits;
- explicit statement that the run is single-host, loop-backed, control-plane
  only, and not a 24-hour soak.

**Acceptance.** The manifest verifies; the report's numbers match the raw
JSON; the comparison tool runs against both bundles; no number is copied
from a different run or extrapolated.

### C4 — ledger and non-claim wording

**Deliverables.**

- a Phase C addendum in the readiness report;
- an evidence-index row in `SUPPORT_MATRIX.md`;
- `START_HERE.md` wording that distinguishes the new measurement from the
  old pre-fix bundle;
- no gate verdict change (gates 1, 2 and 5 NOT MET; 3 and 4 PARTIAL).

**Acceptance.** Every claim in the added text is backed by the bundle; the
"no production support" statements remain intact.

## Validation gates

The orchestration pass runs, at minimum:

```sh
cargo fmt --all --check
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
cargo test -p volvisor-journal --features test-faults --locked
cargo doc --locked --no-deps --document-private-items
bash -n docs/runbooks/day-2/scripts/*.sh scripts/*.sh scripts/bench/*.sh
shellcheck -x docs/runbooks/day-2/scripts/*.sh scripts/*.sh scripts/bench/*.sh
python3 -m unittest discover -s scripts/tests -p 'test_*.py'
python3 -m unittest discover -s scripts/bench/tests -p 'test_*.py'
python3 scripts/validate-traces.py docs/operations/traces/*.jsonl
python3 scripts/release-scope.py check
ruby scripts/lib/check-release-graph.rb .github/workflows/release.yaml \
  .github/workflows/real-backends.yaml
```

## Commit shape

Phase C lands as a small number of logical commits on `phase-a-scaling`
after an adversarial review loop over the full diff: the hardening and
tooling first, then the measurement bundle and ledger update. Nothing is
pushed and no pull request is created by this workstream. Phase A and
Phase B commits remain untouched.
