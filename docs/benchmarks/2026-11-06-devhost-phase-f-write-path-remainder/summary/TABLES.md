# Benchmark tables

Generated from the raw JSON records by `scripts/bench/summarize.py`.
Percentiles are nearest-rank over every recorded request; the CSV
`*-ops.csv` files are a deterministic latency sample (first 5,000 per
operation).

## Micro (concurrency 1)

| operation | count | errors | min_ms | mean_ms | p50_ms | p95_ms | p99_ms | max_ms |
|---|---|---|---|---|---|---|---|---|
| attach | 150 | 0 | 1.28 | 1.423 | 1.396 | 1.614 | 1.742 | 1.844 |
| capabilities | 100 | 0 | 0.093 | 0.1 | 0.096 | 0.115 | 0.121 | 0.131 |
| create | 72 | 0 | 194.799 | 209.0 | 205.42 | 225.682 | 307.833 | 307.833 |
| delete | 72 | 0 | 154.089 | 170.881 | 174.612 | 185.304 | 205.002 | 205.002 |
| detach | 150 | 0 | 1.282 | 1.426 | 1.411 | 1.591 | 1.658 | 1.679 |
| grow | 30 | 0 | 205.258 | 227.997 | 225.496 | 245.507 | 245.649 | 245.649 |
| inspect | 400 | 0 | 0.096 | 0.113 | 0.11 | 0.132 | 0.153 | 0.274 |
| list | 100 | 0 | 0.1 | 0.11 | 0.106 | 0.126 | 0.13 | 0.132 |
| report | 150 | 0 | 1.262 | 1.403 | 1.387 | 1.554 | 1.642 | 1.655 |

## Journal and state-file measurements (derived; not separately timed)

| metric | start_bytes | end_bytes | delta_bytes | mutations | bytes_per_mutation | peak_bytes |
|---|---|---|---|---|---|---|
| journal | 0 | 562677 | 562677 | 624 | 901.726 |  |
| state | 270 | 1378 | 1108 |  |  | 20484 |

### State-file size versus live volumes

| live_volumes | state_bytes | journal_bytes | last_create_ms |
|---|---|---|---|
| 1 | 1973 | 507612 | 205.348 |
| 8 | 6138 | 515525 | 205.153 |
| 32 | 20484 | 542714 | 225.708 |

## Mixed concurrency sweep

| label | concurrency | admission_cap | queue_timeout_s | rate_limit_per_s | duration_s | ops | ops_per_sec | success_ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | accepted_p95_ms | read_p95_ms | write_p95_ms | http_503 | http_429 | network_errors | rss_first_kib | rss_last_kib | rss_max_kib | fds_first | fds_last | fds_max | journal_delta_bytes | state_delta_bytes | state_max_bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| c1 | 1 | 256 | 5.0 | 0 | 30.001 | 54791 | 1826.312 | 1826.312 | 0.0 | 0.155 | 1.451 | 1.56 | 1.451 | 0.188 | 1.531 | 0 | 0 | 0 | 13540.0 | 54108.0 | 54108.0 | 14.0 | 14.0 | 15.0 | 14878399.0 | -387.0 | 1174.0 |
| c128 | 128 | 256 | 5.0 | 0 | 43.425 | 18233 | 419.873 | 419.873 | 0.0 | 0.931 | 138.175 | 11800.496 | 138.175 | 8.461 | 9145.947 | 0 | 0 | 0 | 691136.0 | 767416.0 | 770076.0 | 205.0 | 70.0 | 397.0 | 4239791.0 | 12219.0 | 117069.0 |
| c32 | 32 | 256 | 5.0 | 0 | 30.025 | 69389 | 2311.046 | 2311.046 | 0.0 | 0.452 | 45.273 | 93.864 | 45.273 | 1.001 | 80.68 | 0 | 0 | 0 | 158508.0 | 669384.0 | 682828.0 | 109.0 | 21.0 | 109.0 | -39367824.0 | -5646.0 | 30216.0 |
| c8 | 8 | 256 | 5.0 | 0 | 30.003 | 112328 | 3743.924 | 3743.924 | 0.0 | 0.39 | 7.401 | 11.478 | 7.401 | 0.846 | 10.372 | 0 | 0 | 0 | 58572.0 | 140324.0 | 140324.0 | 37.0 | 22.0 | 37.0 | 30557045.0 | -1094.0 | 7756.0 |
| cap32-c128 | 128 | 32 | 1.0 | 0 | 18.551 | 9240 | 498.082 | 472.208 | 0.051948 | 39.264 | 1001.303 | 3317.28 | 784.878 | 1000.386 | 2682.82 | 480 | 0 | 0 | 122196.0 | 145640.0 | 145640.0 | 205.0 | 88.0 | 205.0 | 1863277.0 | 896.0 | 105600.0 |
| rate50-c64 | 64 | 256 | 5.0 | 50 | 15.555 | 1748 | 112.376 | 50.788 | 0.548055 | 1.787 | 8.951 | 14.951 | 7.533 | 7.607 | 12.249 | 0 | 958 | 0 | 75960.0 | 81596.0 | 81596.0 | 13.0 | 13.0 | 16.0 | 181377.0 | 5756.0 | 45743.0 |

## Shortened soak

| duration_s | workers | ops | ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | rss_first_kib | rss_last_kib | rss_max_kib | rss_kib_per_second | fds_first | fds_last | fds_max | fds_per_second | journal_first_bytes | journal_last_bytes | journal_delta_bytes | state_first_bytes | state_last_bytes | state_delta_bytes | restarts | restart_healthy_seconds |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1500.003 | 4 | 7040538 | 4693.683 | 8e-06 | 0.304 | 2.864 | 3.606 | 75716.0 | 2668132.0 | 2762300.0 | 1383.3125 | 21.0 | 18.0 | 25.0 | -0.001245 | 17801776.0 | 25563609.0 | 7761833.0 | 3413.0 | 2977.0 | -436.0 | 1 | 0.425 |

## Restart replay

| scenario | iteration | journal_bytes_before | state_bytes_before | stop_seconds | healthy_seconds | state_bytes_after | journal_bytes_after | listed_volumes | missing_volumes |
|---|---|---|---|---|---|---|---|---|---|
| restart-post-soak | 1 | 26375128 | 198 | 0.261 | 0.366 | 198 | 26375128 | 0 |  |
| restart-post-soak | 2 | 26375128 | 198 | 0.114 | 0.364 | 198 | 26375128 | 0 |  |
| restart-pre-soak | 1 | 563927 | 198 | 0.02 | 0.103 | 198 | 563927 | 0 |  |
| restart-pre-soak | 2 | 563927 | 198 | 0.003 | 0.102 | 198 | 563927 | 0 |  |
| restart-pre-soak | 3 | 563927 | 198 | 0.003 | 0.102 | 198 | 563927 | 0 |  |
| soak-restart | 1 | 32111061 | 3224 | 0.262 | 0.425 | 3224 | 32111061 | 4 |  |
