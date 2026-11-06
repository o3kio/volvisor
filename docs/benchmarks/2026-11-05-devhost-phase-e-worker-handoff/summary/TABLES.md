# Benchmark tables

Generated from the raw JSON records by `scripts/bench/summarize.py`.
Percentiles are nearest-rank over every recorded request; the per-operation
`*-ops.csv` samples (first 5,000 per operation) are not committed, and these
tables are derived from the exact JSON aggregates.

## Micro (concurrency 1)

| operation | count | errors | min_ms | mean_ms | p50_ms | p95_ms | p99_ms | max_ms |
|---|---|---|---|---|---|---|---|---|
| attach | 150 | 0 | 1.337 | 1.435 | 1.415 | 1.55 | 1.803 | 1.908 |
| capabilities | 100 | 0 | 0.093 | 0.105 | 0.099 | 0.128 | 0.146 | 0.162 |
| create | 72 | 0 | 174.886 | 197.701 | 195.561 | 215.488 | 216.092 | 216.092 |
| delete | 72 | 0 | 134.812 | 157.71 | 154.685 | 174.961 | 185.291 | 185.291 |
| detach | 150 | 0 | 1.274 | 1.423 | 1.414 | 1.536 | 1.564 | 1.57 |
| grow | 30 | 0 | 195.085 | 214.694 | 215.252 | 235.501 | 235.86 | 235.86 |
| inspect | 400 | 0 | 0.096 | 0.113 | 0.109 | 0.143 | 0.158 | 0.327 |
| list | 100 | 0 | 0.101 | 0.117 | 0.111 | 0.146 | 0.156 | 0.194 |
| report | 150 | 0 | 1.331 | 1.426 | 1.407 | 1.573 | 1.619 | 1.631 |

## Journal and state-file measurements (derived; not separately timed)

| metric | start_bytes | end_bytes | delta_bytes | mutations | bytes_per_mutation | peak_bytes |
|---|---|---|---|---|---|---|
| journal | 0 | 562677 | 562677 | 624 | 901.726 |  |
| state | 270 | 1378 | 1108 |  |  | 20484 |

### State-file size versus live volumes

| live_volumes | state_bytes | journal_bytes | last_create_ms |
|---|---|---|---|
| 1 | 1973 | 507612 | 205.475 |
| 8 | 6138 | 515525 | 195.296 |
| 32 | 20484 | 542714 | 195.857 |

## Mixed concurrency sweep

| label | concurrency | admission_cap | queue_timeout_s | rate_limit_per_s | duration_s | ops | ops_per_sec | success_ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | accepted_p95_ms | read_p95_ms | write_p95_ms | http_503 | http_429 | network_errors | rss_first_kib | rss_last_kib | rss_max_kib | fds_first | fds_last | fds_max | journal_delta_bytes | state_delta_bytes | state_max_bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| c1 | 1 | 256 | 5.0 | 0 | 30.0 | 55137 | 1837.885 | 1837.885 | 0.0 | 0.154 | 1.449 | 1.554 | 1.449 | 0.186 | 1.529 | 0 | 0 | 0 | 13680.0 | 54196.0 | 54196.0 | 15.0 | 14.0 | 15.0 | 14935955.0 | 170.0 | 1034.0 |
| c128 | 128 | 256 | 5.0 | 0 | 39.799 | 18423 | 462.899 | 462.899 | 0.0 | 0.91 | 144.116 | 9937.928 | 144.116 | 8.198 | 9151.103 | 0 | 0 | 0 | 690636.0 | 760856.0 | 762704.0 | 231.0 | 44.0 | 397.0 | 4288081.0 | 7057.0 | 119056.0 |
| c32 | 32 | 256 | 5.0 | 0 | 30.026 | 73154 | 2436.35 | 2436.35 | 0.0 | 0.454 | 46.539 | 95.879 | 46.539 | 0.986 | 82.691 | 0 | 0 | 0 | 161248.0 | 669212.0 | 682656.0 | 109.0 | 46.0 | 109.0 | -39237306.0 | -7306.0 | 30517.0 |
| c8 | 8 | 256 | 5.0 | 0 | 30.005 | 114753 | 3824.485 | 3824.485 | 0.0 | 0.376 | 7.372 | 11.418 | 7.372 | 0.822 | 10.401 | 0 | 0 | 0 | 58704.0 | 142160.0 | 142160.0 | 37.0 | 21.0 | 37.0 | 31228058.0 | -1262.0 | 7758.0 |
| cap32-c128 | 128 | 32 | 1.0 | 0 | 17.287 | 9125 | 527.864 | 500.097 | 0.052603 | 40.613 | 1001.183 | 3073.286 | 636.96 | 1000.594 | 2749.151 | 480 | 0 | 0 | 126056.0 | 149688.0 | 150016.0 | 174.0 | 19.0 | 205.0 | 1858728.0 | -8.0 | 107055.0 |
| rate50-c64 | 64 | 256 | 5.0 | 50 | 15.856 | 1761 | 111.063 | 50.707 | 0.543441 | 1.803 | 9.206 | 18.727 | 7.438 | 8.173 | 14.31 | 0 | 957 | 0 | 81212.0 | 85908.0 | 85908.0 | 13.0 | 13.0 | 16.0 | 195786.0 | 5681.0 | 45842.0 |

## Shortened soak

| duration_s | workers | ops | ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | rss_first_kib | rss_last_kib | rss_max_kib | rss_kib_per_second | fds_first | fds_last | fds_max | fds_per_second | journal_first_bytes | journal_last_bytes | journal_delta_bytes | state_first_bytes | state_last_bytes | state_delta_bytes | restarts | restart_healthy_seconds |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1500.001 | 4 | 7112020 | 4741.343 | 1e-05 | 0.298 | 2.866 | 3.613 | 80580.0 | 2088456.0 | 2842396.0 | 842.0957 | 23.0 | 18.0 | 25.0 | -0.001128 | 19741027.0 | 41194835.0 | 21453808.0 | 3812.0 | 3485.0 | -327.0 | 1 | 0.546 |

## Restart replay

| scenario | iteration | journal_bytes_before | state_bytes_before | stop_seconds | healthy_seconds | state_bytes_after | journal_bytes_after | listed_volumes | missing_volumes |
|---|---|---|---|---|---|---|---|---|---|
| restart-post-soak | 1 | 42051299 | 198 | 0.261 | 0.507 | 198 | 42051299 | 0 |  |
| restart-post-soak | 2 | 42051299 | 198 | 0.114 | 0.506 | 198 | 42051299 | 0 |  |
| restart-pre-soak | 1 | 563927 | 198 | 0.02 | 0.103 | 198 | 563927 | 0 |  |
| restart-pre-soak | 2 | 563927 | 198 | 0.003 | 0.081 | 198 | 563927 | 0 |  |
| restart-pre-soak | 3 | 563927 | 198 | 0.003 | 0.102 | 198 | 563927 | 0 |  |
| soak-restart | 1 | 44223953 | 2718 | 0.342 | 0.546 | 2718 | 44223953 | 4 |  |
