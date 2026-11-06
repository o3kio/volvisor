# NON-EVIDENCE DIAGNOSTIC — same-SHA `--skip-soak` repeat

**This directory is not measured evidence.** It is a second clean-tree run at
the measured source, executed 42 seconds after the published run ended, with
`--skip-soak`. It was not used to select, replace or support the published
sample, and no number here is the measured result for `7204a53`. The measured
evidence is the bundle root's `json/`, `summary/`, `records/`,
`raw/`, `ENVIRONMENT.txt` and `comparison/`. This repeat is disclosed because
the published report must not imply that no same-SHA repeat exists, and
because its micro delete numbers qualify the published sample's "flat"
delete result.

## Run

- Source: `7204a53198457bf69aa9797db67708a696270142` (`volvisord 7204a53`),
  clean worktree.
- Binary: SHA256
  `db09dff8090eae07a8ea26c70bb3bfcf8200969b82b49d165f72a801eb6a8717`,
  byte-identical to the measured binary.
- Window: 2026-11-05T21:21:42Z .. 2026-11-05T21:27:25Z.
- Shape: micro, 1/8/32/128 sweep at 30 s per tier, the cap-32 and 50 req/s
  probes, and a pre-soak restart replay. **No soak** (`--skip-soak`), so this
  is not a "sweep-only" run.
- The source run's own `MANIFEST.sha256` verified before these records were
  copied; the JSON files here are unmodified copies. The run directory was
  `/tmp/opencode/volvisor-bench-phase-e-m1-repeat-20261105`.

## Results

| tier | ops/s | overall p95 ms | read p95 ms | write p95 ms |
|---|---|---|---|---|
| c1 | 1,829.773 | 1.456 | 0.186 | 1.540 |
| c8 | 3,789.820 | 7.489 | 0.812 | 10.585 |
| c32 | 2,480.580 | 45.560 | 0.985 | 80.968 |
| c128 | 519.539 | 123.894 | 5.660 | 8,340.238 |

Micro create/delete:

| operation | repeat p50 ms | repeat p95 ms | published sample p50/p95 | comparator p50/p95 |
|---|---|---|---|---|
| create | 196.359 | 215.880 | 195.561 / 215.488 (+0.41% / +0.18%) | 194.991 / 215.837 (+0.70% / +0.02%) |
| delete | 164.290 | 184.750 | 154.685 / 174.961 (+6.21% / +5.60%) | 154.492 / 174.464 (+6.34% / +5.90%) |

Probes: cap-32 524.814 ops/s with 481 `OVERLOADED` (5.50%); rate-50 115.806
ops/s with 956 `RATE_LIMITED` (54.60%). Both are diagnostic shapes.

## What this changes about the published claims

- **c128 is still variance-dominated.** The repeat's 519.539 falls between
  the published 462.899 and the A/B high state (626.011), consistent with the
  variance conclusion. It cannot turn the c128 drop into a gain or an
  attributed regression.
- **Micro delete is not flat across samples.** The published sample's delete
  p50/p95 were +0.12%/+0.28% versus the comparator; this repeat's are
  +6.34%/+5.90%. Create is flat in both. With two samples and no A/B micro
  control, the report must say that micro delete overhead is unresolved, not
  that the handoff adds no measurable per-op overhead.
- This repeat is **non-evidence** and is not used to replace the published
  sample. It exists so the reader can verify the numbers quoted in the
  report's section 5 and in this disclosure.

## Files

- `ENVIRONMENT.txt` — source, dirty flag, binary digest, host and toolchain.
- `json/micro.json`, `json/mixed-c{1,8,32,128}.json`,
  `json/mixed-cap32-c128.json`, `json/mixed-rate50-c64.json`,
  `json/restart-pre-soak.json` — unmodified aggregate records.
