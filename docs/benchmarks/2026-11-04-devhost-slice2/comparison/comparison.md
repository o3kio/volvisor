# Benchmark comparison

> **Caveat:** Single-host loop-LVM control-plane measurements. These runs are not production capacity, SLA, durability, or data-path evidence. A dirty worktree cannot support a clean-SHA claim.

Source SHAs and dirty flags below are read from each run's
`ENVIRONMENT.txt`; the soak durations are read from each `json/soak.json`.

## Sources

| run | directory | source SHA | worktree dirty | manifest | soak duration |
|---|---|---|---|---|---|
| before | `docs/benchmarks/2026-11-04-devhost-postfix` | `aab773b7b58448e41ba22b21372785b3c58e7381` | no | verified | 1500.003 s (25.000 min) |
| after | `docs/benchmarks/2026-11-04-devhost-slice2` | `c4b0b1303594c494bb26d3643d952434eda8ec34` | no | verified | 1500.002 s (25.000 min) |

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
| mixed-c1 | 1 | ops/s | 1952.155 | 1845.526 | -106.629 | -5.462117506 |  |
| mixed-c1 | 1 | p95 ms | 1.357 | 1.457 | 0.1 | 7.369196758 |  |
| mixed-c1 | 1 | read p95 ms | 0.175 | 0.186 | 0.011 | 6.285714286 |  |
| mixed-c1 | 1 | write p95 ms | 1.445 | 1.546 | 0.101 | 6.989619377 |  |
| mixed-c1 | 1 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c8 | 8 | ops/s | 3493.445 | 3410.746 | -82.699 | -2.367262115 |  |
| mixed-c8 | 8 | p95 ms | 5.928 | 7.467 | 1.539 | 25.96153846 |  |
| mixed-c8 | 8 | read p95 ms | 1.254 | 0.853 | -0.401 | -31.97767145 |  |
| mixed-c8 | 8 | write p95 ms | 6.635 | 10.47 | 3.835 | 57.79954785 |  |
| mixed-c8 | 8 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c32 | 32 | ops/s | 862.937 | 858.371 | -4.566 | -0.5291232153 |  |
| mixed-c32 | 32 | p95 ms | 200.594 | 197.17 | -3.424 | -1.706930417 |  |
| mixed-c32 | 32 | read p95 ms | 179.332 | 1.188 | -178.144 | -99.33754154 |  |
| mixed-c32 | 32 | write p95 ms | 388.84 | 590.847 | 202.007 | 51.95118815 |  |
| mixed-c32 | 32 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c128 | 128 | ops/s | 235.502 | 237.088 | 1.586 | 0.6734550025 |  |
| mixed-c128 | 128 | p95 ms | 1349.487 | 3553.448 | 2203.961 | 163.3184314 |  |
| mixed-c128 | 128 | read p95 ms | 1288.505 | 197.815 | -1090.69 | -84.64771188 |  |
| mixed-c128 | 128 | write p95 ms | 1552.261 | 6330.548 | 4778.287 | 307.8275496 |  |
| mixed-c128 | 128 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |

## Soak

| metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|
| duration (s) | 1500.003 | 1500.002 | -0.001 | -6.666653333e-05 |  |
| ops | 5424744 | 6929746 | 1505002 | 27.74328153 |  |
| RSS first (KiB) | 169152 | 164940 | -4212 | -2.490068104 |  |
| RSS last (KiB) | 1950048 | 3108756 | 1158708 | 59.41946044 |  |
| RSS delta (KiB) | 1780896 | 2943816 | 1162920 | 65.2997143 |  |
| journal first (bytes) | 58483131 | 56646344 | -1836787 | -3.14071249 |  |
| journal last (bytes) | 27982748 | 43412309 | 15429561 | 55.13954884 |  |
| journal delta (bytes) | -30500383 | -13234035 | 17266348 | -56.61026617 |  |
| state first (bytes) | 3228 | 3312 | 84 | 2.602230483 |  |
| state last (bytes) | 3055 | 3401 | 346 | 11.32569558 |  |
| state delta (bytes) | -173 | 89 | 262 | -151.4450867 |  |

## Normalised per minute

The soak durations differ, so duration-sensitive quantities are
also shown per minute. These rows are derived from the recorded
durations, ops and resource deltas; they are not additional
measurements.

| metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|
| ops / min | 216989.326 | 277189.4704 | 60200.14439 | 27.74336669 | normalised per minute (soak durations differ) |
| RSS delta (KiB / min) | 71235.69753 | 117752.483 | 46516.78547 | 65.2998245 | normalised per minute (soak durations differ) |
| journal delta (bytes / min) | -1220012.88 | -529360.6942 | 690652.1858 | -56.61023725 | normalised per minute (soak durations differ) |
| state delta (bytes / min) | -6.91998616 | 3.559995253 | 10.47998141 | -151.445121 | normalised per minute (soak durations differ) |

## Reading this comparison

- `NOT RECORDED` means the harness JSON did not contain that value; it
  is never treated as zero.
- A percentage change with a zero before value is undefined and is
  shown as `n/a (zero baseline)` in Markdown; the CSV cell is empty.
- The sweep changes both in-flight work and the number of live volumes
  (each worker owns one volume). It is not a fixed-dataset comparison.
- These are control-plane results only. No guest I/O or data path is
  measured, and no production SLA is implied.
