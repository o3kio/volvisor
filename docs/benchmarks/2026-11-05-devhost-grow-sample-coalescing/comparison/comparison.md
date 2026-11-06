# Benchmark comparison

> **Caveat:** Single-host loop-LVM control-plane measurements. These runs are not production capacity, SLA, durability, or data-path evidence. A dirty worktree cannot support a clean-SHA claim.

Source SHAs and dirty flags below are read from each run's
`ENVIRONMENT.txt`; the soak durations are read from each `json/soak.json`.

## Sources

| run | directory | source SHA | worktree dirty | manifest | soak duration |
|---|---|---|---|---|---|
| before | `docs/benchmarks/2026-11-05-devhost-capacity-domain-fix` | `4290b8425baf35f73f5df80b76fc9a3a75f39583` | no | verified | 1500.002 s (25.000 min) |
| after | `/tmp/opencode/volvisor-bench-coalesce-20261105` | `75beea399e593c561104d4d0bb473c88d4d8b93e` | no | verified | 1500.002 s (25.000 min) |

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
| mixed-c1 | 1 | ops/s | 1825.419 | 1813.922 | -11.497 | -0.6298280011 |  |
| mixed-c1 | 1 | p95 ms | 1.457 | 1.461 | 0.004 | 0.2745367193 |  |
| mixed-c1 | 1 | read p95 ms | 0.188 | 0.186 | -0.002 | -1.063829787 |  |
| mixed-c1 | 1 | write p95 ms | 1.545 | 1.541 | -0.004 | -0.2588996764 |  |
| mixed-c1 | 1 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c8 | 8 | ops/s | 3846.899 | 3837.267 | -9.632 | -0.2503834907 |  |
| mixed-c8 | 8 | p95 ms | 7.478 | 7.431 | -0.047 | -0.6285102969 |  |
| mixed-c8 | 8 | read p95 ms | 0.861 | 0.839 | -0.022 | -2.555168409 |  |
| mixed-c8 | 8 | write p95 ms | 10.374 | 10.509 | 0.135 | 1.301330249 |  |
| mixed-c8 | 8 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c32 | 32 | ops/s | 2609.257 | 2572.763 | -36.494 | -1.398635704 |  |
| mixed-c32 | 32 | p95 ms | 46.913 | 45.742 | -1.171 | -2.49610982 |  |
| mixed-c32 | 32 | read p95 ms | 0.958 | 1.049 | 0.091 | 9.498956159 |  |
| mixed-c32 | 32 | write p95 ms | 82.927 | 81.615 | -1.312 | -1.58211439 |  |
| mixed-c32 | 32 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |
| mixed-c128 | 128 | ops/s | 655.326 | 508.225 | -147.101 | -22.44699585 |  |
| mixed-c128 | 128 | p95 ms | 117.87 | 154.553 | 36.683 | 31.12157462 |  |
| mixed-c128 | 128 | read p95 ms | 6.584 | 6.462 | -0.122 | -1.852976914 |  |
| mixed-c128 | 128 | write p95 ms | 5078.906 | 10772.171 | 5693.265 | 112.0962861 |  |
| mixed-c128 | 128 | error rate | 0 | 0 | 0 | n/a (zero baseline) | percentage change undefined (before is zero) |

## Soak

| metric | before | after | delta abs | delta % | note |
|---|---|---|---|---|---|
| duration (s) | 1500.002 | 1500.002 | 0 | 0 |  |
| ops | 7134530 | 6815052 | -319478 | -4.47791235 |  |
| RSS first (KiB) | 93844 | 88500 | -5344 | -5.694556924 |  |
| RSS last (KiB) | 1705896 | 2580376 | 874480 | 51.26221059 |  |
| RSS delta (KiB) | 1612052 | 2491876 | 879824 | 54.57789203 |  |
| journal first (bytes) | 23927809 | 21675535 | -2252274 | -9.412788275 |  |
| journal last (bytes) | 49560395 | 54075830 | 4515435 | 9.1109746 |  |
| journal delta (bytes) | 25632586 | 32400295 | 6767709 | 26.40275546 |  |
| state first (bytes) | 3896 | 3650 | -246 | -6.314168378 |  |
| state last (bytes) | 3147 | 3317 | 170 | 5.40197013 |  |
| state delta (bytes) | -749 | -333 | 416 | -55.54072096 |  |

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
