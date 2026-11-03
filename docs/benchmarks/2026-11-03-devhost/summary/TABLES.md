# Benchmark tables

Generated from the raw JSON records by `scripts/bench/summarize.py`.
Percentiles are nearest-rank over every recorded request; the CSV
`*-ops.csv` files are a deterministic latency sample (first 5,000 per
operation).

## Micro (concurrency 1)

| operation | count | errors | min_ms | mean_ms | p50_ms | p95_ms | p99_ms | max_ms |
|---|---|---|---|---|---|---|---|---|
| attach | 150 | 0 | 1.201 | 1.339 | 1.32 | 1.52 | 1.637 | 1.912 |
| capabilities | 100 | 0 | 0.1 | 0.113 | 0.106 | 0.135 | 0.173 | 0.178 |
| create | 72 | 0 | 123.904 | 146.696 | 144.016 | 164.455 | 174.616 | 174.616 |
| delete | 72 | 0 | 93.016 | 108.842 | 113.09 | 123.511 | 133.714 | 133.714 |
| detach | 150 | 0 | 1.162 | 1.322 | 1.295 | 1.58 | 1.76 | 1.797 |
| grow | 30 | 0 | 153.762 | 167.068 | 164.083 | 184.196 | 184.242 | 184.242 |
| inspect | 400 | 0 | 0.096 | 0.123 | 0.117 | 0.156 | 0.232 | 0.296 |
| list | 100 | 0 | 0.109 | 0.124 | 0.12 | 0.147 | 0.154 | 0.231 |
| report | 150 | 0 | 1.197 | 1.316 | 1.301 | 1.489 | 1.536 | 1.614 |

## Journal and state-file measurements (derived; not separately timed)

| metric | start_bytes | end_bytes | delta_bytes | mutations | bytes_per_mutation | peak_bytes |
|---|---|---|---|---|---|---|
| journal | 0 | 562847 | 562847 | 624 | 901.998 |  |
| state | 270 | 269 | -1 |  |  | 29678 |

### State-file size versus live volumes

| live_volumes | state_bytes | journal_bytes | last_create_ms |
|---|---|---|---|
| 1 | 6486 | 506532 | 123.904 |
| 8 | 11708 | 514445 | 144.251 |
| 32 | 29678 | 541634 | 164.455 |

## Mixed concurrency sweep

| label | concurrency | admission_cap | queue_timeout_s | rate_limit_per_s | duration_s | ops | ops_per_sec | success_ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | accepted_p95_ms | read_p95_ms | write_p95_ms | http_503 | http_429 | network_errors | rss_first_kib | rss_last_kib | rss_max_kib | fds_first | fds_last | fds_max | journal_delta_bytes | state_delta_bytes | state_max_bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| c1 | 1 | 256 | 5.0 | 0 | 30.0 | 49974 | 1665.787 | 1665.787 | 0.0 | 0.144 | 1.77 | 1.936 | 1.77 | 0.174 | 1.892 | 0 | 0 | 0 | 12948.0 | 50260.0 | 50260.0 | 14.0 | 15.0 | 15.0 | 13450608.0 | 151723.0 | 154820.0 |
| c128 | 128 | 256 | 5.0 | 0 | 30.536 | 7381 | 241.716 | 241.716 | 0.0 | 443.875 | 1251.564 | 1651.803 | 1251.564 | 1245.738 | 1257.975 | 0 | 0 | 0 | 140728.0 | 147220.0 | 147220.0 | 143.0 | 143.0 | 144.0 | 1860340.0 | 30244.0 | 151730.0 |
| c32 | 32 | 256 | 5.0 | 0 | 30.008 | 27000 | 899.759 | 899.759 | 0.0 | 9.824 | 191.244 | 369.003 | 191.244 | 187.38 | 199.061 | 0 | 0 | 0 | 108544.0 | 125988.0 | 125988.0 | 47.0 | 46.0 | 47.0 | 6718206.0 | 76162.0 | 107633.0 |
| c8 | 8 | 256 | 5.0 | 0 | 30.002 | 68815 | 2293.7 | 2293.7 | 0.0 | 2.526 | 6.454 | 8.866 | 6.454 | 4.787 | 7.878 | 0 | 0 | 0 | 52304.0 | 104828.0 | 104828.0 | 23.0 | 22.0 | 23.0 | 18781532.0 | 205158.0 | 214312.0 |
| cap32-c128 | 128 | 32 | 1.0 | 0 | 15.232 | 3626 | 238.045 | 238.045 | 0.0 | 447.461 | 1317.53 | 1886.251 | 1317.53 | 1316.873 | 1326.709 | 0 | 0 | 0 | 139340.0 | 149564.0 | 149564.0 | 143.0 | 143.0 | 143.0 | 905539.0 | 30527.0 | 144478.0 |
| rate50-c64 | 64 | 256 | 5.0 | 50 | 15.931 | 1755 | 110.162 | 50.781 | 0.539031 | 4.035 | 194.304 | 388.736 | 197.172 | 192.847 | 199.262 | 0 | 946 | 0 | 129396.0 | 139060.0 | 139060.0 | 13.0 | 13.0 | 29.0 | 187622.0 | 18489.0 | 68891.0 |

## Shortened soak

| duration_s | workers | ops | ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | rss_first_kib | rss_last_kib | rss_max_kib | rss_kib_per_second | fds_first | fds_last | fds_max | fds_per_second | journal_first_bytes | journal_last_bytes | journal_delta_bytes | state_first_bytes | state_last_bytes | state_delta_bytes | restarts | restart_healthy_seconds |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1500.019 | 4 | 1202500 | 801.656 | 0.000148 | 2.262 | 16.804 | 26.25 | 128896.0 | 750612.0 | 750612.0 | 412.9999 | 19.0 | 18.0 | 19.0 | -0.000392 | 45959969.0 | 271399822.0 | 225439853.0 | 5911.0 | 2522793.0 | 2516882.0 | 1 | 1.799 |

## Restart replay

| scenario | iteration | journal_bytes_before | state_bytes_before | stop_seconds | healthy_seconds | state_bytes_after | journal_bytes_after | listed_volumes | missing_volumes |
|---|---|---|---|---|---|---|---|---|---|
| restart-post-soak | 1 | 271435753 | 269 | 0.763 | 2.813 | 269 | 271435753 | 0 |  |
| restart-post-soak | 2 | 271435753 | 269 | 0.464 | 2.669 | 269 | 271435753 | 0 |  |
| restart-pre-soak | 1 | 562847 | 269 | 0.02 | 0.103 | 269 | 562847 | 0 |  |
| restart-pre-soak | 2 | 562847 | 269 | 0.003 | 0.102 | 269 | 562847 | 0 |  |
| restart-pre-soak | 3 | 562847 | 269 | 0.003 | 0.102 | 269 | 562847 | 0 |  |
| soak-restart | 1 | 178260132 | 1465696 | 0.422 | 1.799 | 1465696 | 178260132 | 4 |  |
