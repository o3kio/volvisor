# Benchmark comparison

> **Caveat:** Single-host loop-LVM control-plane measurements. These runs are not production capacity, SLA, durability, or data-path evidence. A dirty worktree cannot support a clean-SHA claim.

Source SHAs and dirty flags below are read from each run's
`ENVIRONMENT.txt`; the soak durations are read from each `json/soak.json`.

## Sources

| run | directory | source SHA | worktree dirty | manifest | soak duration |
|---|---|---|---|---|---|
| before | `docs/benchmarks/2026-11-03-devhost` | `e3aa365ea65b865c8310a42ea6b459b038f0ab24` | no | verified | 1500.019 s (25.000 min) |
| after | `docs/benchmarks/2026-11-04-devhost-postfix` | `aab773b7b58448e41ba22b21372785b3c58e7381` | no | verified | 1500.003 s (25.000 min) |

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
| mixed-c1 | 1 | ops/s | 1665.787 | 1952.155 | 286.368 | 17.19115349 |  |
| mixed-c1 | 1 | p95 ms | 1.77 | 1.357 | -0.413 | -23.33333333 |  |
| mixed-c1 | 1 | read p95 ms | 0.174 | 0.175 | 0.001 | 0.5747126437 |  |
| mixed-c1 | 1 | write p95 ms | 1.892 | 1.445 | -0.447 | -23.62579281 |  |
| mixed-c1 | 1 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c8 | 8 | ops/s | 2293.7 | 3493.445 | 1199.745 | 52.30609932 |  |
| mixed-c8 | 8 | p95 ms | 6.454 | 5.928 | -0.526 | -8.149984506 |  |
| mixed-c8 | 8 | read p95 ms | 4.787 | 1.254 | -3.533 | -73.80405264 |  |
| mixed-c8 | 8 | write p95 ms | 7.878 | 6.635 | -1.243 | -15.77811627 |  |
| mixed-c8 | 8 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c32 | 32 | ops/s | 899.759 | 862.937 | -36.822 | -4.092429195 |  |
| mixed-c32 | 32 | p95 ms | 191.244 | 200.594 | 9.35 | 4.889042271 |  |
| mixed-c32 | 32 | read p95 ms | 187.38 | 179.332 | -8.048 | -4.295015477 |  |
| mixed-c32 | 32 | write p95 ms | 199.061 | 388.84 | 189.779 | 95.33710772 |  |
| mixed-c32 | 32 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c128 | 128 | ops/s | 241.716 | 235.502 | -6.214 | -2.57078555 |  |
| mixed-c128 | 128 | p95 ms | 1251.564 | 1349.487 | 97.923 | 7.824050548 |  |
| mixed-c128 | 128 | read p95 ms | 1245.738 | 1288.505 | 42.767 | 3.43306538 |  |
| mixed-c128 | 128 | write p95 ms | 1257.975 | 1552.261 | 294.286 | 23.39362865 |  |
| mixed-c128 | 128 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |

## Soak

| metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|
| duration (s) | 1500.019 | 1500.003 | -0.016 | -0.001066653156 |  |
| ops | 1202500 | 5424744 | 4222244 | 351.1221622 |  |
| RSS first (KiB) | 128896 | 169152 | 40256 | 31.23138034 |  |
| RSS last (KiB) | 750612 | 1950048 | 1199436 | 159.7944078 |  |
| RSS delta (KiB) | 621716 | 1780896 | 1159180 | 186.4484749 |  |
| journal first (bytes) | 45959969 | 58483131 | 12523162 | 27.24797747 |  |
| journal last (bytes) | 271399822 | 27982748 | -243417074 | -89.68947445 |  |
| journal delta (bytes) | 225439853 | -30500383 | -255940236 | -113.5292774 |  |
| state first (bytes) | 5911 | 3228 | -2683 | -45.38995094 |  |
| state last (bytes) | 2522793 | 3055 | -2519738 | -99.87890406 |  |
| state delta (bytes) | 2516882 | -173 | -2517055 | -100.0068736 |  |

## Normalised per minute

The soak durations differ, so duration-sensitive quantities are
also shown per minute. These rows are derived from the recorded
durations, ops and resource deltas; they are not additional
measurements.

| metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|
| ops / min | 48099.39074 | 216989.326 | 168889.9353 | 351.1269741 | normalised per minute (soak durations differ) |
| RSS delta (KiB / min) | 24868.325 | 71235.69753 | 46367.37253 | 186.4515303 | normalised per minute (soak durations differ) |
| journal delta (bytes / min) | 9017479.899 | -1220012.88 | -10237492.78 | -113.5294217 | normalised per minute (soak durations differ) |
| state delta (bytes / min) | 100674.0048 | -6.91998616 | -100680.9248 | -100.0068737 | normalised per minute (soak durations differ) |

## Reading this comparison

- `NOT RECORDED` means the harness JSON did not contain that value; it
  is never treated as zero.
- A percentage change with a zero before value is undefined and is
  shown as `n/a (zero baseline)` in Markdown; the CSV cell is empty.
- The sweep changes both in-flight work and the number of live volumes
  (each worker owns one volume). It is not a fixed-dataset comparison.
- These are control-plane results only. No guest I/O or data path is
  measured, and no production SLA is implied.
