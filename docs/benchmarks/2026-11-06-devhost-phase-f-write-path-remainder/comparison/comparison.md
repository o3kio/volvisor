# Benchmark comparison

> **Caveat:** Single-host loop-LVM control-plane measurements. These runs are not production capacity, SLA, durability, or data-path evidence. A dirty worktree cannot support a clean-SHA claim.

Source SHAs and dirty flags below are read from each run's
`ENVIRONMENT.txt`; the soak durations are read from each `json/soak.json`.

## Sources

| run | directory | source SHA | worktree dirty | manifest | soak duration |
|---|---|---|---|---|---|
| before | `docs/benchmarks/2026-11-05-devhost-phase-e-worker-handoff` | `7204a53198457bf69aa9797db67708a696270142` | no | verified | 1500.001 s (25.000 min) |
| after | `docs/benchmarks/2026-11-06-devhost-phase-f-write-path-remainder` | `2edcecd9b3d4c555831e6d4db1337e3dbcff6a15` | no | verified | 1500.003 s (25.000 min) |

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
| mixed-c1 | 1 | ops/s | 1837.885 | 1826.312 | -11.573 | -0.629691194 |  |
| mixed-c1 | 1 | p95 ms | 1.449 | 1.451 | 0.002 | 0.138026225 |  |
| mixed-c1 | 1 | read p95 ms | 0.186 | 0.188 | 0.002 | 1.075268817 |  |
| mixed-c1 | 1 | write p95 ms | 1.529 | 1.531 | 0.002 | 0.1308044474 |  |
| mixed-c1 | 1 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c8 | 8 | ops/s | 3824.485 | 3743.924 | -80.561 | -2.106453549 |  |
| mixed-c8 | 8 | p95 ms | 7.372 | 7.401 | 0.029 | 0.3933803581 |  |
| mixed-c8 | 8 | read p95 ms | 0.822 | 0.846 | 0.024 | 2.919708029 |  |
| mixed-c8 | 8 | write p95 ms | 10.401 | 10.372 | -0.029 | -0.2788193443 |  |
| mixed-c8 | 8 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c32 | 32 | ops/s | 2436.35 | 2311.046 | -125.304 | -5.143103413 |  |
| mixed-c32 | 32 | p95 ms | 46.539 | 45.273 | -1.266 | -2.720299104 |  |
| mixed-c32 | 32 | read p95 ms | 0.986 | 1.001 | 0.015 | 1.521298174 |  |
| mixed-c32 | 32 | write p95 ms | 82.691 | 80.68 | -2.011 | -2.431945435 |  |
| mixed-c32 | 32 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c128 | 128 | ops/s | 462.899 | 419.873 | -43.026 | -9.294900183 |  |
| mixed-c128 | 128 | p95 ms | 144.116 | 138.175 | -5.941 | -4.122373643 |  |
| mixed-c128 | 128 | read p95 ms | 8.198 | 8.461 | 0.263 | 3.208099536 |  |
| mixed-c128 | 128 | write p95 ms | 9151.103 | 9145.947 | -5.156 | -0.05634293484 |  |
| mixed-c128 | 128 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |

## Soak

| metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|
| duration (s) | 1500.001 | 1500.003 | 0.002 | 0.0001333332444 |  |
| ops | 7112020 | 7040538 | -71482 | -1.005087162 |  |
| RSS first (KiB) | 80580 | 75716 | -4864 | -6.03623728 |  |
| RSS last (KiB) | 2088456 | 2668132 | 579676 | 27.75619884 |  |
| RSS delta (KiB) | 2007876 | 2592416 | 584540 | 29.11235554 |  |
| journal first (bytes) | 19741027 | 17801776 | -1939251 | -9.823455487 |  |
| journal last (bytes) | 41194835 | 25563609 | -15631226 | -37.94462583 |  |
| journal delta (bytes) | 21453808 | 7761833 | -13691975 | -63.82072124 |  |
| state first (bytes) | 3812 | 3413 | -399 | -10.46694648 |  |
| state last (bytes) | 3485 | 2977 | -508 | -14.57675753 |  |
| state delta (bytes) | -327 | -436 | -109 | 33.33333333 |  |

## Normalised per minute

The soak durations differ, so duration-sensitive quantities are
also shown per minute. These rows are derived from the recorded
durations, ops and resource deltas; they are not additional
measurements.

| metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|
| ops / min | 284480.6103 | 281620.9568 | -2859.653588 | -1.005219155 | normalised per minute (soak durations differ) |
| RSS delta (KiB / min) | 80314.98646 | 103696.4326 | 23381.44615 | 29.11218339 | normalised per minute (soak durations differ) |
| journal delta (bytes / min) | 858151.7479 | 310472.6991 | -547679.0488 | -63.82076948 | normalised per minute (soak durations differ) |
| state delta (bytes / min) | -13.07999128 | -17.43996512 | -4.35997384 | 33.33315556 | normalised per minute (soak durations differ) |

## Reading this comparison

- `NOT RECORDED` means the harness JSON did not contain that value; it
  is never treated as zero.
- A percentage change with a zero before value is undefined and is
  shown as `n/a (zero baseline)` in Markdown; the CSV cell is empty.
- The sweep changes both in-flight work and the number of live volumes
  (each worker owns one volume). It is not a fixed-dataset comparison.
- These are control-plane results only. No guest I/O or data path is
  measured, and no production SLA is implied.
