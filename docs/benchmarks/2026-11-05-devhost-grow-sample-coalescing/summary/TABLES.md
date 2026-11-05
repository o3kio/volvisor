# Benchmark tables

Generated from the raw JSON records by `scripts/bench/summarize.py`.
Percentiles are nearest-rank over every recorded request; the CSV
`*-ops.csv` files are a deterministic latency sample (first 5,000 per
operation).

## Micro (concurrency 1)

| operation | count | errors | min_ms | mean_ms | p50_ms | p95_ms | p99_ms | max_ms |
|---|---|---|---|---|---|---|---|---|
| attach | 150 | 0 | 1.272 | 1.454 | 1.435 | 1.621 | 1.841 | 2.035 |
| capabilities | 100 | 0 | 0.094 | 0.111 | 0.109 | 0.131 | 0.151 | 0.245 |
| create | 72 | 0 | 174.919 | 193.185 | 195.092 | 205.691 | 277.665 | 277.665 |
| delete | 72 | 0 | 134.032 | 154.456 | 154.491 | 174.937 | 184.826 | 184.826 |
| detach | 150 | 0 | 1.282 | 1.462 | 1.445 | 1.624 | 1.793 | 1.942 |
| grow | 30 | 0 | 194.93 | 209.998 | 205.589 | 215.673 | 225.624 | 225.624 |
| inspect | 400 | 0 | 0.097 | 0.122 | 0.115 | 0.163 | 0.183 | 0.254 |
| list | 100 | 0 | 0.101 | 0.112 | 0.108 | 0.129 | 0.146 | 0.15 |
| report | 150 | 0 | 1.285 | 1.45 | 1.435 | 1.611 | 1.694 | 1.747 |

## Journal and state-file measurements (derived; not separately timed)

| metric | start_bytes | end_bytes | delta_bytes | mutations | bytes_per_mutation | peak_bytes |
|---|---|---|---|---|---|---|
| journal | 0 | 562677 | 562677 | 624 | 901.726 |  |
| state | 270 | 1378 | 1108 |  |  | 20484 |

### State-file size versus live volumes

| live_volumes | state_bytes | journal_bytes | last_create_ms |
|---|---|---|---|
| 1 | 1973 | 507612 | 194.942 |
| 8 | 6138 | 515525 | 185.14 |
| 32 | 20484 | 542714 | 195.356 |

## Mixed concurrency sweep

| label | concurrency | admission_cap | queue_timeout_s | rate_limit_per_s | duration_s | ops | ops_per_sec | success_ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | accepted_p95_ms | read_p95_ms | write_p95_ms | http_503 | http_429 | network_errors | rss_first_kib | rss_last_kib | rss_max_kib | fds_first | fds_last | fds_max | journal_delta_bytes | state_delta_bytes | state_max_bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| c1 | 1 | 256 | 5.0 | 0 | 30.001 | 54419 | 1813.922 | 1813.922 | 0.0 | 0.155 | 1.461 | 1.569 | 1.461 | 0.186 | 1.541 | 0 | 0 | 0 | 13568.0 | 53684.0 | 53684.0 | 15.0 | 15.0 | 15.0 | 14759697.0 | 10.0 | 1036.0 |
| c128 | 128 | 256 | 5.0 | 0 | 40.793 | 20732 | 508.225 | 508.225 | 0.0 | 0.819 | 154.553 | 11384.61 | 154.553 | 6.462 | 10772.171 | 0 | 0 | 0 | 699036.0 | 790128.0 | 790964.0 | 204.0 | 127.0 | 343.0 | 4880698.0 | 8488.0 | 121652.0 |
| c32 | 32 | 256 | 5.0 | 0 | 30.027 | 77253 | 2572.763 | 2572.763 | 0.0 | 0.442 | 45.742 | 94.226 | 45.742 | 1.049 | 81.615 | 0 | 0 | 0 | 160944.0 | 669036.0 | 682480.0 | 107.0 | 46.0 | 109.0 | -38101980.0 | -6811.0 | 31184.0 |
| c8 | 8 | 256 | 5.0 | 0 | 30.004 | 115135 | 3837.267 | 3837.267 | 0.0 | 0.378 | 7.431 | 11.533 | 7.431 | 0.839 | 10.509 | 0 | 0 | 0 | 58036.0 | 141796.0 | 141796.0 | 25.0 | 22.0 | 33.0 | 31347085.0 | -413.0 | 7496.0 |
| cap32-c128 | 128 | 32 | 1.0 | 0 | 17.514 | 9916 | 566.184 | 533.296 | 0.058088 | 30.987 | 1001.577 | 3075.746 | 146.205 | 1001.177 | 2048.11 | 576 | 0 | 0 | 131216.0 | 152312.0 | 152444.0 | 183.0 | 49.0 | 183.0 | 2047630.0 | -1094.0 | 104704.0 |
| rate50-c64 | 64 | 256 | 5.0 | 50 | 15.914 | 1761 | 110.656 | 50.395 | 0.544577 | 1.846 | 9.222 | 13.834 | 6.38 | 8.441 | 12.751 | 0 | 959 | 0 | 87340.0 | 93464.0 | 93464.0 | 13.0 | 13.0 | 16.0 | 178758.0 | 4366.0 | 44781.0 |

## Shortened soak

| duration_s | workers | ops | ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | rss_first_kib | rss_last_kib | rss_max_kib | rss_kib_per_second | fds_first | fds_last | fds_max | fds_per_second | journal_first_bytes | journal_last_bytes | journal_delta_bytes | state_first_bytes | state_last_bytes | state_delta_bytes | restarts | restart_healthy_seconds |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1500.002 | 4 | 6815052 | 4543.361 | 1.2e-05 | 0.3 | 2.905 | 3.768 | 88500.0 | 2580376.0 | 2580376.0 | 1325.3444 | 23.0 | 17.0 | 25.0 | -0.000973 | 21675535.0 | 54075830.0 | 32400295.0 | 3650.0 | 3317.0 | -333.0 | 1 | 0.648 |

## Restart replay

| scenario | iteration | journal_bytes_before | state_bytes_before | stop_seconds | healthy_seconds | state_bytes_after | journal_bytes_after | listed_volumes | missing_volumes |
|---|---|---|---|---|---|---|---|---|---|
| restart-post-soak | 1 | 54296692 | 198 | 0.342 | 0.609 | 198 | 54296692 | 0 |  |
| restart-post-soak | 2 | 54296692 | 198 | 0.164 | 0.627 | 198 | 54296692 | 0 |  |
| restart-pre-soak | 1 | 563927 | 198 | 0.02 | 0.083 | 198 | 563927 | 0 |  |
| restart-pre-soak | 2 | 563927 | 198 | 0.003 | 0.102 | 198 | 563927 | 0 |  |
| restart-pre-soak | 3 | 563927 | 198 | 0.003 | 0.081 | 198 | 563927 | 0 |  |
| soak-restart | 1 | 53969783 | 3301 | 0.345 | 0.648 | 3301 | 53969783 | 4 |  |
