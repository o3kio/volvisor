# Benchmark tables

Generated from the raw JSON records by `scripts/bench/summarize.py`.
Percentiles are nearest-rank over every recorded request; the CSV
`*-ops.csv` files are a deterministic latency sample (first 5,000 per
operation).

## Micro (concurrency 1)

| operation | count | errors | min_ms | mean_ms | p50_ms | p95_ms | p99_ms | max_ms |
|---|---|---|---|---|---|---|---|---|
| attach | 150 | 0 | 1.196 | 1.436 | 1.417 | 1.624 | 1.808 | 1.808 |
| capabilities | 100 | 0 | 0.102 | 0.116 | 0.108 | 0.143 | 0.181 | 0.184 |
| create | 72 | 0 | 194.635 | 216.645 | 215.203 | 235.718 | 277.973 | 277.973 |
| delete | 72 | 0 | 154.11 | 177.303 | 174.623 | 194.684 | 194.827 | 194.827 |
| detach | 150 | 0 | 1.226 | 1.461 | 1.439 | 1.622 | 1.772 | 1.815 |
| grow | 30 | 0 | 214.655 | 233.514 | 234.856 | 255.667 | 265.85 | 265.85 |
| inspect | 400 | 0 | 0.106 | 0.123 | 0.117 | 0.148 | 0.188 | 0.286 |
| list | 100 | 0 | 0.113 | 0.128 | 0.123 | 0.152 | 0.166 | 0.229 |
| report | 150 | 0 | 1.244 | 1.438 | 1.417 | 1.667 | 1.816 | 1.821 |

## Journal and state-file measurements (derived; not separately timed)

| metric | start_bytes | end_bytes | delta_bytes | mutations | bytes_per_mutation | peak_bytes |
|---|---|---|---|---|---|---|
| journal | 0 | 562677 | 562677 | 624 | 901.726 |  |
| state | 270 | 1378 | 1108 |  |  | 20484 |

### State-file size versus live volumes

| live_volumes | state_bytes | journal_bytes | last_create_ms |
|---|---|---|---|
| 1 | 1973 | 507612 | 235.068 |
| 8 | 6138 | 515525 | 215.075 |
| 32 | 20484 | 542714 | 215.368 |

## Mixed concurrency sweep

| label | concurrency | admission_cap | queue_timeout_s | rate_limit_per_s | duration_s | ops | ops_per_sec | success_ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | accepted_p95_ms | read_p95_ms | write_p95_ms | http_503 | http_429 | network_errors | rss_first_kib | rss_last_kib | rss_max_kib | fds_first | fds_last | fds_max | journal_delta_bytes | state_delta_bytes | state_max_bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| c1 | 1 | 256 | 5.0 | 0 | 30.0 | 53816 | 1793.843 | 1793.843 | 0.0 | 0.159 | 1.482 | 1.599 | 1.482 | 0.194 | 1.57 | 0 | 0 | 0 | 13348.0 | 53292.0 | 53292.0 | 16.0 | 15.0 | 16.0 | 14609422.0 | -222.0 | 1174.0 |
| c128 | 128 | 256 | 5.0 | 0 | 42.676 | 17756 | 416.064 | 416.064 | 0.0 | 0.937 | 161.083 | 12523.519 | 161.083 | 8.216 | 11243.656 | 0 | 0 | 0 | 695124.0 | 774348.0 | 776044.0 | 230.0 | 76.0 | 397.0 | 4146442.0 | 7307.0 | 119899.0 |
| c32 | 32 | 256 | 5.0 | 0 | 30.092 | 69252 | 2301.375 | 2301.375 | 0.0 | 0.456 | 46.081 | 97.28 | 46.081 | 1.004 | 81.305 | 0 | 0 | 0 | 157780.0 | 686640.0 | 686640.0 | 109.0 | 45.0 | 109.0 | -39400104.0 | -5461.0 | 30338.0 |
| c8 | 8 | 256 | 5.0 | 0 | 30.005 | 110964 | 3698.176 | 3698.176 | 0.0 | 0.378 | 7.592 | 11.719 | 7.592 | 0.829 | 10.626 | 0 | 0 | 0 | 57752.0 | 138612.0 | 138612.0 | 37.0 | 21.0 | 37.0 | 30216873.0 | -912.0 | 7754.0 |
| cap32-c128 | 128 | 32 | 1.0 | 0 | 17.64 | 8652 | 490.472 | 452.377 | 0.07767 | 28.692 | 1001.76 | 3557.081 | 136.642 | 1001.467 | 3141.369 | 672 | 0 | 0 | 120452.0 | 138248.0 | 138468.0 | 190.0 | 52.0 | 205.0 | 1645742.0 | -1059.0 | 104580.0 |
| rate50-c64 | 64 | 256 | 5.0 | 50 | 15.88 | 1760 | 110.833 | 50.442 | 0.544886 | 1.888 | 7.979 | 13.916 | 5.956 | 6.977 | 11.283 | 0 | 959 | 0 | 73780.0 | 80004.0 | 80004.0 | 13.0 | 13.0 | 16.0 | 193569.0 | 4366.0 | 45219.0 |

## Shortened soak

| duration_s | workers | ops | ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | rss_first_kib | rss_last_kib | rss_max_kib | rss_kib_per_second | fds_first | fds_last | fds_max | fds_per_second | journal_first_bytes | journal_last_bytes | journal_delta_bytes | state_first_bytes | state_last_bytes | state_delta_bytes | restarts | restart_healthy_seconds |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|

## Restart replay

| scenario | iteration | journal_bytes_before | state_bytes_before | stop_seconds | healthy_seconds | state_bytes_after | journal_bytes_after | listed_volumes | missing_volumes |
|---|---|---|---|---|---|---|---|---|---|
| restart-pre-soak | 1 | 563927 | 198 | 0.02 | 0.103 | 198 | 563927 | 0 |  |
| restart-pre-soak | 2 | 563927 | 198 | 0.003 | 0.102 | 198 | 563927 | 0 |  |
| restart-pre-soak | 3 | 563927 | 198 | 0.003 | 0.081 | 198 | 563927 | 0 |  |
