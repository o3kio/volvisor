# Benchmark comparison

> **Caveat:** Single-host loop-LVM control-plane measurements. These runs are not production capacity, SLA, durability, or data-path evidence. A dirty worktree cannot support a clean-SHA claim.

Source SHAs and dirty flags below are read from each run's
`ENVIRONMENT.txt`; the soak durations are read from each `json/soak.json`.

## Sources

| run | directory | source SHA | worktree dirty | manifest | soak duration |
|---|---|---|---|---|---|
| before | `docs/benchmarks/2026-11-04-devhost-slice2` | `c4b0b1303594c494bb26d3643d952434eda8ec34` | no | verified | 1500.002 s (25.000 min) |
| after | `docs/benchmarks/2026-11-05-devhost-slice2-fix` | `8029df8e8f6e1856499927df6f434422894005ad` | no | verified | 1500.581 s (25.010 min) |

### Record statuses

The harness `status` field for every aggregate JSON record. A
`SETUP_FAILED` or other non-OK status is therefore not confused with
an omitted metric.

| scenario | before status | after status |
|---|---|---|
| mixed-c1 | OK | OK |
| mixed-c8 | OK | OK |
| mixed-c32 | OK | OK |
| mixed-c128 | OK | OK |
| soak | OK | OK |
| micro | OK | OK |
| mixed-cap32-c128 | OK | OK |
| mixed-rate50-c64 | OK | OK |
| restart-post-soak | OK | OK |
| restart-pre-soak | OK | OK |

## Mixed concurrency sweep

Concurrency rows are the harness's `mixed-c1`, `mixed-c8`, `mixed-c32`
and `mixed-c128` aggregates. p95 values are nearest-rank across every
recorded request.

| scenario | in-flight | metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|---|---|
| mixed-c1 | 1 | ops/s | 1845.526 | 1729.154 | -116.372 | -6.305627772 |  |
| mixed-c1 | 1 | p95 ms | 1.457 | 1.542 | 0.085 | 5.833905285 |  |
| mixed-c1 | 1 | read p95 ms | 0.186 | 0.199 | 0.013 | 6.989247312 |  |
| mixed-c1 | 1 | write p95 ms | 1.546 | 1.668 | 0.122 | 7.891332471 |  |
| mixed-c1 | 1 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c8 | 8 | ops/s | 3410.746 | 3118.787 | -291.959 | -8.559974856 |  |
| mixed-c8 | 8 | p95 ms | 7.467 | 9.125 | 1.658 | 22.20436588 |  |
| mixed-c8 | 8 | read p95 ms | 0.853 | 1.007 | 0.154 | 18.05392732 |  |
| mixed-c8 | 8 | write p95 ms | 10.47 | 15.597 | 5.127 | 48.96848138 |  |
| mixed-c8 | 8 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c32 | 32 | ops/s | 858.371 | 1830.005 | 971.634 | 113.1951103 |  |
| mixed-c32 | 32 | p95 ms | 197.17 | 88.969 | -108.201 | -54.87700969 |  |
| mixed-c32 | 32 | read p95 ms | 1.188 | 1.481 | 0.293 | 24.66329966 |  |
| mixed-c32 | 32 | write p95 ms | 590.847 | 206.203 | -384.644 | -65.10044055 |  |
| mixed-c32 | 32 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c128 | 128 | ops/s | 237.088 | 440.376 | 203.288 | 85.74369011 |  |
| mixed-c128 | 128 | p95 ms | 3553.448 | 1836.718 | -1716.73 | -48.31166799 |  |
| mixed-c128 | 128 | read p95 ms | 197.815 | 87.276 | -110.539 | -55.87998888 |  |
| mixed-c128 | 128 | write p95 ms | 6330.548 | 3413.886 | -2916.662 | -46.07282024 |  |
| mixed-c128 | 128 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |

## Soak

| metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|
| duration (s) | 1500.002 | 1500.581 | 0.579 | 0.03859994853 |  |
| ops | 6929746 | 6640950 | -288796 | -4.167483195 |  |
| RSS first (KiB) | 164940 | 60628 | -104312 | -63.24239117 |  |
| RSS last (KiB) | 3108756 | 2186760 | -921996 | -29.65803685 |  |
| RSS delta (KiB) | 2943816 | 2126132 | -817684 | -27.77632841 |  |
| journal first (bytes) | 56646344 | 9019147 | -47627197 | -84.0781481 |  |
| journal last (bytes) | 43412309 | 75496834 | 32084525 | 73.90651578 |  |
| journal delta (bytes) | -13234035 | 66477687 | 79711722 | -602.3236451 |  |
| state first (bytes) | 3312 | 3523 | 211 | 6.370772947 |  |
| state last (bytes) | 3401 | 3147 | -254 | -7.46839165 |  |
| state delta (bytes) | 89 | -376 | -465 | -522.4719101 |  |

## Normalised per minute

The soak durations differ, so duration-sensitive quantities are
also shown per minute. These rows are derived from the recorded
durations, ops and resource deltas; they are not additional
measurements.

| metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|
| ops / min | 277189.4704 | 265535.1494 | -11654.32103 | -4.204460224 | normalised per minute (soak durations differ) |
| RSS delta (KiB / min) | 117752.483 | 85012.35188 | -32740.13111 | -27.80419596 | normalised per minute (soak durations differ) |
| journal delta (bytes / min) | -529360.6942 | 2658077.918 | 3187438.612 | -602.1298232 | normalised per minute (soak durations differ) |
| state delta (bytes / min) | 3.559995253 | -15.03417676 | -18.59417202 | -522.3088991 | normalised per minute (soak durations differ) |

## Reading this comparison

- `NOT RECORDED` means the harness JSON did not contain that value; it
  is never treated as zero.
- A percentage change with a zero before value is undefined and is
  shown as `n/a (zero baseline)` in Markdown; the CSV cell is empty.
- The sweep changes both in-flight work and the number of live volumes
  (each worker owns one volume). It is not a fixed-dataset comparison.
- These are control-plane results only. No guest I/O or data path is
  measured, and no production SLA is implied.
