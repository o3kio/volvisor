# Benchmark comparison

> **Caveat:** Single-host loop-LVM control-plane measurements. These runs are not production capacity, SLA, durability, or data-path evidence. A dirty worktree cannot support a clean-SHA claim.

Source SHAs and dirty flags below are read from each run's
`ENVIRONMENT.txt`; the soak durations are read from each `json/soak.json`.

## Sources

| run | directory | source SHA | worktree dirty | manifest | soak duration |
|---|---|---|---|---|---|
| before | `docs/benchmarks/2026-11-05-devhost-slice2-fix` | `8029df8e8f6e1856499927df6f434422894005ad` | no | verified | 1500.581 s (25.010 min) |
| after | `docs/benchmarks/2026-11-05-devhost-capacity-domain-fix` | `4290b8425baf35f73f5df80b76fc9a3a75f39583` | no | verified | 1500.002 s (25.000 min) |

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
| mixed-c1 | 1 | ops/s | 1729.154 | 1825.419 | 96.265 | 5.567173311 |  |
| mixed-c1 | 1 | p95 ms | 1.542 | 1.457 | -0.085 | -5.51232166 |  |
| mixed-c1 | 1 | read p95 ms | 0.199 | 0.188 | -0.011 | -5.527638191 |  |
| mixed-c1 | 1 | write p95 ms | 1.668 | 1.545 | -0.123 | -7.374100719 |  |
| mixed-c1 | 1 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c8 | 8 | ops/s | 3118.787 | 3846.899 | 728.112 | 23.34599958 |  |
| mixed-c8 | 8 | p95 ms | 9.125 | 7.478 | -1.647 | -18.04931507 |  |
| mixed-c8 | 8 | read p95 ms | 1.007 | 0.861 | -0.146 | -14.49851043 |  |
| mixed-c8 | 8 | write p95 ms | 15.597 | 10.374 | -5.223 | -33.48720908 |  |
| mixed-c8 | 8 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c32 | 32 | ops/s | 1830.005 | 2609.257 | 779.252 | 42.58196016 |  |
| mixed-c32 | 32 | p95 ms | 88.969 | 46.913 | -42.056 | -47.27039755 |  |
| mixed-c32 | 32 | read p95 ms | 1.481 | 0.958 | -0.523 | -35.31397704 |  |
| mixed-c32 | 32 | write p95 ms | 206.203 | 82.927 | -123.276 | -59.78380528 |  |
| mixed-c32 | 32 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c128 | 128 | ops/s | 440.376 | 655.326 | 214.95 | 48.81056188 |  |
| mixed-c128 | 128 | p95 ms | 1836.718 | 117.87 | -1718.848 | -93.58257501 |  |
| mixed-c128 | 128 | read p95 ms | 87.276 | 6.584 | -80.692 | -92.45611623 |  |
| mixed-c128 | 128 | write p95 ms | 3413.886 | 5078.906 | 1665.02 | 48.77198594 |  |
| mixed-c128 | 128 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |

## Soak

| metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|
| duration (s) | 1500.581 | 1500.002 | -0.579 | -0.03858505472 |  |
| ops | 6640950 | 7134530 | 493580 | 7.432370369 |  |
| RSS first (KiB) | 60628 | 93844 | 33216 | 54.78656726 |  |
| RSS last (KiB) | 2186760 | 1705896 | -480864 | -21.98979312 |  |
| RSS delta (KiB) | 2126132 | 1612052 | -514080 | -24.17911964 |  |
| journal first (bytes) | 9019147 | 23927809 | 14908662 | 165.3001332 |  |
| journal last (bytes) | 75496834 | 49560395 | -25936439 | -34.35433994 |  |
| journal delta (bytes) | 66477687 | 25632586 | -40845101 | -61.44182032 |  |
| state first (bytes) | 3523 | 3896 | 373 | 10.58756741 |  |
| state last (bytes) | 3147 | 3147 | 0 | 0 |  |
| state delta (bytes) | -376 | -749 | -373 | 99.20212766 |  |

## Normalised per minute

The soak durations differ, so duration-sensitive quantities are
also shown per minute. These rows are derived from the recorded
durations, ops and resource deltas; they are not additional
measurements.

| metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|
| ops / min | 265535.1494 | 285380.8195 | 19845.67011 | 7.473839208 | normalised per minute (soak durations differ) |
| RSS delta (KiB / min) | 85012.35188 | 64481.99402 | -20530.35786 | -24.14985282 | normalised per minute (soak durations differ) |
| journal delta (bytes / min) | 2658077.918 | 1025302.073 | -1632775.845 | -61.42693688 | normalised per minute (soak durations differ) |
| state delta (bytes / min) | -15.03417676 | -29.95996005 | -14.92578329 | 99.27901958 | normalised per minute (soak durations differ) |

## Reading this comparison

- `NOT RECORDED` means the harness JSON did not contain that value; it
  is never treated as zero.
- A percentage change with a zero before value is undefined and is
  shown as `n/a (zero baseline)` in Markdown; the CSV cell is empty.
- The sweep changes both in-flight work and the number of live volumes
  (each worker owns one volume). It is not a fixed-dataset comparison.
- These are control-plane results only. No guest I/O or data path is
  measured, and no production SLA is implied.
