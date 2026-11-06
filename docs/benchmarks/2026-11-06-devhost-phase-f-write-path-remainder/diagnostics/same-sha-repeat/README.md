# NON-EVIDENCE: same-SHA repeat

**This is a NON-EVIDENCE same-SHA repeat.** It was not used to select,
replace or support the measured sample. It is cited only to disclose
run-to-run variance. It has **no soak**.

This directory must not be read as a measured result or used for any
performance claim. The measured record is the parent bundle; only that
bundle is evidence.

## Run identity

| field | value |
|---|---|
| run directory | `/tmp/opencode/volvisor-bench-phase-f-repeat-20261106T125655Z` |
| source SHA | `2edcecd9b3d4c555831e6d4db1337e3dbcff6a15` |
| worktree dirty (ENVIRONMENT.txt) | `no` |
| binary | `target/release/volvisord` |
| binary SHA256 | `a09ec03d8157a527b17c3a7111cee62522cc711ab6b42b93d6b697ddff2c2e7a` |
| binary version | `volvisord 2edcecd` |
| run window | `2026-11-06T12:56:56Z` .. `2026-11-06T13:02:57Z` |
| soak | none (`--skip-soak`); no soak JSON, CSV or soak restart records exist |

The same source SHA and identical binary digest as the measured run mean this
repeat exercised the same build. It is still a separate sample, not a
replacement measurement.

## Micro create/delete percentiles (this repeat only)

Nearest-rank percentiles from `summary/TABLES.md` / `summary/micro.csv` in
this directory:

| operation | count | errors | p50 ms | p95 ms |
|---|---|---|---|---|
| create | 72 | 0 | 215.203 | 235.718 |
| delete | 72 | 0 | 174.623 | 194.684 |

These numbers are disclosed only to show how much the untimed micro phase
moves between two runs of the same binary on this host. They are not a
before/after comparison and support no performance claim.

## What was copied

Copied unchanged (`cp -p`) from the repeat run, with source manifest
verification passing before copying:

- `ENVIRONMENT.txt`
- every file under `json/` (8 aggregates: micro, mixed-c1/c8/c32/c128,
  mixed-cap32-c128, mixed-rate50-c64, restart-pre-soak)
- `summary/TABLES.md`

The repeat's `MANIFEST.sha256` and `configs/` were deliberately not copied.
This directory contains no daemon log, raw orchestration log, CSV sample or
soak record. The parent `MANIFEST.sha256` was resealed after authoring and
covers this file.
