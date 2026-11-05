# Benchmark tables

Generated from the raw JSON records by `scripts/bench/summarize.py`.
Percentiles are nearest-rank over every recorded request; the CSV
`*-ops.csv` files are a deterministic latency sample (first 5,000 per
operation).

## Micro (concurrency 1)

| operation | count | errors | min_ms | mean_ms | p50_ms | p95_ms | p99_ms | max_ms |
|---|---|---|---|---|---|---|---|---|
| attach | 150 | 0 | 1.299 | 1.511 | 1.467 | 1.805 | 1.849 | 2.012 |
| capabilities | 100 | 0 | 0.105 | 0.119 | 0.117 | 0.137 | 0.184 | 0.262 |
| create | 72 | 0 | 134.82 | 163.203 | 165.146 | 180.017 | 200.925 | 200.925 |
| delete | 72 | 0 | 93.905 | 120.297 | 115.245 | 135.207 | 155.041 | 155.041 |
| detach | 150 | 0 | 1.297 | 1.501 | 1.491 | 1.756 | 1.869 | 2.01 |
| grow | 30 | 0 | 164.77 | 181.041 | 184.937 | 195.571 | 207.017 | 207.017 |
| inspect | 400 | 0 | 0.097 | 0.115 | 0.111 | 0.145 | 0.188 | 0.296 |
| list | 100 | 0 | 0.113 | 0.125 | 0.121 | 0.144 | 0.222 | 0.232 |
| report | 150 | 0 | 1.291 | 1.493 | 1.448 | 1.752 | 1.964 | 2.006 |

## Journal and state-file measurements (derived; not separately timed)

| metric | start_bytes | end_bytes | delta_bytes | mutations | bytes_per_mutation | peak_bytes |
|---|---|---|---|---|---|---|
| journal | 0 | 562677 | 562677 | 624 | 901.726 |  |
| state | 270 | 1378 | 1108 |  |  | 20484 |

### State-file size versus live volumes

| live_volumes | state_bytes | journal_bytes | last_create_ms |
|---|---|---|---|
| 1 | 1973 | 507612 | 165.495 |
| 8 | 6138 | 515525 | 165.716 |
| 32 | 20484 | 542714 | 165.816 |

## Mixed concurrency sweep

| label | concurrency | admission_cap | queue_timeout_s | rate_limit_per_s | duration_s | ops | ops_per_sec | success_ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | accepted_p95_ms | read_p95_ms | write_p95_ms | http_503 | http_429 | network_errors | rss_first_kib | rss_last_kib | rss_max_kib | fds_first | fds_last | fds_max | journal_delta_bytes | state_delta_bytes | state_max_bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| c1 | 1 | 256 | 5.0 | 0 | 30.0 | 51875 | 1729.154 | 1729.154 | 0.0 | 0.162 | 1.542 | 1.713 | 1.542 | 0.199 | 1.668 | 0 | 0 | 0 | 13536.0 | 51672.0 | 51672.0 | 15.0 | 14.0 | 15.0 | 13992952.0 | 7.0 | 1034.0 |
| c128 | 128 | 256 | 5.0 | 0 | 30.24 | 13317 | 440.376 | 440.376 | 0.0 | 1.123 | 1836.718 | 3889.606 | 1836.718 | 87.276 | 3413.886 | 0 | 0 | 0 | 192816.0 | 200536.0 | 201084.0 | 147.0 | 145.0 | 147.0 | 3390056.0 | 6338.0 | 96372.0 |
| c32 | 32 | 256 | 5.0 | 0 | 30.031 | 54957 | 1830.005 | 1830.005 | 0.0 | 0.522 | 88.969 | 284.504 | 88.969 | 1.481 | 206.203 | 0 | 0 | 0 | 130508.0 | 175404.0 | 175404.0 | 51.0 | 46.0 | 51.0 | 14608785.0 | 445.0 | 25022.0 |
| c8 | 8 | 256 | 5.0 | 0 | 30.005 | 93579 | 3118.787 | 3118.787 | 0.0 | 0.411 | 9.125 | 19.084 | 9.125 | 1.007 | 15.597 | 0 | 0 | 0 | 54912.0 | 122844.0 | 122844.0 | 26.0 | 22.0 | 27.0 | 25300959.0 | -949.0 | 6514.0 |
| cap32-c128 | 128 | 32 | 1.0 | 0 | 15.468 | 6821 | 440.966 | 439.932 | 0.002346 | 228.021 | 747.848 | 1276.788 | 727.665 | 467.867 | 1116.272 | 16 | 0 | 0 | 200296.0 | 211316.0 | 211316.0 | 145.0 | 145.0 | 147.0 | 1652963.0 | -4909.0 | 101829.0 |
| rate50-c64 | 64 | 256 | 5.0 | 50 | 15.687 | 1758 | 112.064 | 50.869 | 0.546075 | 3.741 | 54.379 | 211.724 | 62.129 | 50.253 | 62.129 | 0 | 960 | 0 | 625908.0 | 634672.0 | 634672.0 | 13.0 | 13.0 | 13.0 | 198086.0 | 7414.0 | 48705.0 |

## Shortened soak

| duration_s | workers | ops | ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | rss_first_kib | rss_last_kib | rss_max_kib | rss_kib_per_second | fds_first | fds_last | fds_max | fds_per_second | journal_first_bytes | journal_last_bytes | journal_delta_bytes | state_first_bytes | state_last_bytes | state_delta_bytes | restarts | restart_healthy_seconds |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1500.581 | 4 | 6640950 | 4425.586 | 7e-06 | 0.307 | 3.0 | 4.038 | 60628.0 | 2186760.0 | 2538456.0 | 478.8043 | 21.0 | 17.0 | 25.0 | -0.000474 | 9019147.0 | 75496834.0 | 66477687.0 | 3523.0 | 3147.0 | -376.0 | 1 | 0.264 |

## Restart replay

| scenario | iteration | journal_bytes_before | state_bytes_before | stop_seconds | healthy_seconds | state_bytes_after | journal_bytes_after | listed_volumes | missing_volumes |
|---|---|---|---|---|---|---|---|---|---|
| restart-post-soak | 1 | 8397224 | 198 | 0.201 | 0.205 | 198 | 8397224 | 0 |  |
| restart-post-soak | 2 | 8397224 | 198 | 0.031 | 0.203 | 198 | 8397224 | 0 |  |
| restart-pre-soak | 1 | 563927 | 198 | 0.02 | 0.103 | 198 | 563927 | 0 |  |
| restart-pre-soak | 2 | 563927 | 198 | 0.003 | 0.102 | 198 | 563927 | 0 |  |
| restart-pre-soak | 3 | 563927 | 198 | 0.003 | 0.082 | 198 | 563927 | 0 |  |
| soak-restart | 1 | 12941814 | 2718 | 0.284 | 0.264 | 2718 | 12941814 | 4 |  |
