# Benchmark comparison

> **Caveat:** Single-host loop-LVM control-plane measurements. These runs are not production capacity, SLA, durability, or data-path evidence. A dirty worktree cannot support a clean-SHA claim.

Source SHAs and dirty flags below are read from each run's
`ENVIRONMENT.txt`; the soak durations are read from each `json/soak.json`.

## Sources

| run | directory | source SHA | worktree dirty | manifest | soak duration |
|---|---|---|---|---|---|
| before | `docs/benchmarks/2026-11-05-devhost-phase-e-write-lock-scope` | `2c5b0659a68b5ce6b9286c5c1cd1df7a92f8ebb1` | no | verified | 1500.002 s (25.000 min) |
| after | `docs/benchmarks/2026-11-05-devhost-phase-e-worker-handoff` | `7204a53198457bf69aa9797db67708a696270142` | no | verified | 1500.001 s (25.000 min) |

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
| mixed-c1 | 1 | ops/s | 1822.948 | 1837.885 | 14.937 | 0.8193870588 |  |
| mixed-c1 | 1 | p95 ms | 1.462 | 1.449 | -0.013 | -0.8891928865 |  |
| mixed-c1 | 1 | read p95 ms | 0.183 | 0.186 | 0.003 | 1.639344262 |  |
| mixed-c1 | 1 | write p95 ms | 1.549 | 1.529 | -0.02 | -1.291155584 |  |
| mixed-c1 | 1 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c8 | 8 | ops/s | 3756.659 | 3824.485 | 67.826 | 1.805487269 |  |
| mixed-c8 | 8 | p95 ms | 7.54 | 7.372 | -0.168 | -2.228116711 |  |
| mixed-c8 | 8 | read p95 ms | 0.81 | 0.822 | 0.012 | 1.481481481 |  |
| mixed-c8 | 8 | write p95 ms | 10.66 | 10.401 | -0.259 | -2.429643527 |  |
| mixed-c8 | 8 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c32 | 32 | ops/s | 2476.245 | 2436.35 | -39.895 | -1.611108755 |  |
| mixed-c32 | 32 | p95 ms | 46.651 | 46.539 | -0.112 | -0.2400805985 |  |
| mixed-c32 | 32 | read p95 ms | 0.895 | 0.986 | 0.091 | 10.16759777 |  |
| mixed-c32 | 32 | write p95 ms | 81.996 | 82.691 | 0.695 | 0.8476023221 |  |
| mixed-c32 | 32 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c128 | 128 | ops/s | 610.988 | 462.899 | -148.089 | -24.23762823 |  |
| mixed-c128 | 128 | p95 ms | 100.558 | 144.116 | 43.558 | 43.31629507 |  |
| mixed-c128 | 128 | read p95 ms | 4.691 | 8.198 | 3.507 | 74.76017907 |  |
| mixed-c128 | 128 | write p95 ms | 4297.266 | 9151.103 | 4853.837 | 112.9517465 |  |
| mixed-c128 | 128 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |

## Soak

| metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|
| duration (s) | 1500.002 | 1500.001 | -0.001 | -6.666657778e-05 |  |
| ops | 6626734 | 7112020 | 485286 | 7.323154966 |  |
| RSS first (KiB) | 88020 | 80580 | -7440 | -8.452624404 |  |
| RSS last (KiB) | 1362124 | 2088456 | 726332 | 53.32348597 |  |
| RSS delta (KiB) | 1274104 | 2007876 | 733772 | 57.59121704 |  |
| journal first (bytes) | 21521776 | 19741027 | -1780749 | -8.274173098 |  |
| journal last (bytes) | 17862039 | 41194835 | 23332796 | 130.6278415 |  |
| journal delta (bytes) | -3659737 | 21453808 | 25113545 | -686.2117414 |  |
| state first (bytes) | 3005 | 3812 | 807 | 26.85524126 |  |
| state last (bytes) | 2977 | 3485 | 508 | 17.06415855 |  |
| state delta (bytes) | -28 | -327 | -299 | 1067.857143 |  |

## Normalised per minute

The soak durations differ, so duration-sensitive quantities are
also shown per minute. These rows are derived from the recorded
durations, ops and resource deltas; they are not additional
measurements.

| metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|
| ops / min | 265069.0066 | 284480.6103 | 19411.60377 | 7.323226515 | normalised per minute (soak durations differ) |
| RSS delta (KiB / min) | 50964.09205 | 80314.98646 | 29350.89441 | 57.5913221 | normalised per minute (soak durations differ) |
| journal delta (bytes / min) | -146389.2848 | 858151.7479 | 1004541.033 | -686.2121323 | normalised per minute (soak durations differ) |
| state delta (bytes / min) | -1.119998507 | -13.07999128 | -11.95999277 | 1067.857921 | normalised per minute (soak durations differ) |

## Reading this comparison

- `NOT RECORDED` means the harness JSON did not contain that value; it
  is never treated as zero.
- A percentage change with a zero before value is undefined and is
  shown as `n/a (zero baseline)` in Markdown; the CSV cell is empty.
- The sweep changes both in-flight work and the number of live volumes
  (each worker owns one volume). It is not a fixed-dataset comparison.
- These are control-plane results only. No guest I/O or data path is
  measured, and no production SLA is implied.
