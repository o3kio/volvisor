# Benchmark comparison

> **Caveat:** Single-host loop-LVM control-plane measurements. These runs are not production capacity, SLA, durability, or data-path evidence. A dirty worktree cannot support a clean-SHA claim.

Source SHAs and dirty flags below are read from each run's
`ENVIRONMENT.txt`; the soak durations are read from each `json/soak.json`.

## Sources

| run | directory | source SHA | worktree dirty | manifest | soak duration |
|---|---|---|---|---|---|
| before | `docs/benchmarks/2026-11-05-devhost-capacity-domain-fix` | `4290b8425baf35f73f5df80b76fc9a3a75f39583` | no | verified | 1500.002 s (25.000 min) |
| after | `docs/benchmarks/2026-11-05-devhost-phase-e-write-lock-scope` | `2c5b0659a68b5ce6b9286c5c1cd1df7a92f8ebb1` | no | verified | 1500.002 s (25.000 min) |

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
| mixed-c1 | 1 | ops/s | 1825.419 | 1822.948 | -2.471 | -0.1353661817 |  |
| mixed-c1 | 1 | p95 ms | 1.457 | 1.462 | 0.005 | 0.3431708991 |  |
| mixed-c1 | 1 | read p95 ms | 0.188 | 0.183 | -0.005 | -2.659574468 |  |
| mixed-c1 | 1 | write p95 ms | 1.545 | 1.549 | 0.004 | 0.2588996764 |  |
| mixed-c1 | 1 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c8 | 8 | ops/s | 3846.899 | 3756.659 | -90.24 | -2.345785528 |  |
| mixed-c8 | 8 | p95 ms | 7.478 | 7.54 | 0.062 | 0.8290986895 |  |
| mixed-c8 | 8 | read p95 ms | 0.861 | 0.81 | -0.051 | -5.923344948 |  |
| mixed-c8 | 8 | write p95 ms | 10.374 | 10.66 | 0.286 | 2.756892231 |  |
| mixed-c8 | 8 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c32 | 32 | ops/s | 2609.257 | 2476.245 | -133.012 | -5.097696394 |  |
| mixed-c32 | 32 | p95 ms | 46.913 | 46.651 | -0.262 | -0.5584805917 |  |
| mixed-c32 | 32 | read p95 ms | 0.958 | 0.895 | -0.063 | -6.576200418 |  |
| mixed-c32 | 32 | write p95 ms | 82.927 | 81.996 | -0.931 | -1.122674159 |  |
| mixed-c32 | 32 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c128 | 128 | ops/s | 655.326 | 610.988 | -44.338 | -6.765792903 |  |
| mixed-c128 | 128 | p95 ms | 117.87 | 100.558 | -17.312 | -14.68736744 |  |
| mixed-c128 | 128 | read p95 ms | 6.584 | 4.691 | -1.893 | -28.75151883 |  |
| mixed-c128 | 128 | write p95 ms | 5078.906 | 4297.266 | -781.64 | -15.38992846 |  |
| mixed-c128 | 128 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |

## Soak

| metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|
| duration (s) | 1500.002 | 1500.002 | 0 | 0 |  |
| ops | 7134530 | 6626734 | -507796 | -7.117441513 |  |
| RSS first (KiB) | 93844 | 88020 | -5824 | -6.206044073 |  |
| RSS last (KiB) | 1705896 | 1362124 | -343772 | -20.15199051 |  |
| RSS delta (KiB) | 1612052 | 1274104 | -337948 | -20.96383988 |  |
| journal first (bytes) | 23927809 | 21521776 | -2406033 | -10.05538368 |  |
| journal last (bytes) | 49560395 | 17862039 | -31698356 | -63.95904633 |  |
| journal delta (bytes) | 25632586 | -3659737 | -29292323 | -114.2776737 |  |
| state first (bytes) | 3896 | 3005 | -891 | -22.86960986 |  |
| state last (bytes) | 3147 | 2977 | -170 | -5.40197013 |  |
| state delta (bytes) | -749 | -28 | 721 | -96.26168224 |  |

_Per-minute normalisation was not applied: the two soak durations
are equal or were not recorded._

## Reading this comparison

- `NOT RECORDED` means the harness JSON did not contain that value; it
  is never treated as zero.
- A percentage change with a zero before value is undefined and is
  shown as `n/a (zero baseline)` in Markdown; the CSV cell is empty.
- The sweep changes both in-flight work and the number of live volumes
  (each worker owns one volume). It is not a fixed-dataset comparison.
- These are control-plane results only. No guest I/O or data path is
  measured, and no production SLA is implied.
