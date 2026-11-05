# Benchmark tables

Generated from the raw JSON records by `scripts/bench/summarize.py`.
Percentiles are nearest-rank over every recorded request; the CSV
`*-ops.csv` files are a deterministic latency sample (first 5,000 per
operation).

## Micro (concurrency 1)

| operation | count | errors | min_ms | mean_ms | p50_ms | p95_ms | p99_ms | max_ms |
|---|---|---|---|---|---|---|---|---|
| attach | 150 | 0 | 1.186 | 1.432 | 1.409 | 1.641 | 1.806 | 1.871 |
| capabilities | 100 | 0 | 0.095 | 0.108 | 0.104 | 0.13 | 0.145 | 0.156 |
| create | 72 | 0 | 154.681 | 181.651 | 184.771 | 195.737 | 205.13 | 205.13 |
| delete | 72 | 0 | 114.253 | 141.496 | 144.307 | 154.612 | 154.787 | 154.787 |
| detach | 150 | 0 | 1.196 | 1.426 | 1.425 | 1.593 | 1.67 | 1.677 |
| grow | 30 | 0 | 184.733 | 200.419 | 195.463 | 215.668 | 216.837 | 216.837 |
| inspect | 400 | 0 | 0.099 | 0.114 | 0.11 | 0.136 | 0.171 | 0.255 |
| list | 100 | 0 | 0.104 | 0.119 | 0.115 | 0.137 | 0.15 | 0.256 |
| report | 150 | 0 | 1.188 | 1.421 | 1.41 | 1.616 | 1.736 | 1.851 |

## Journal and state-file measurements (derived; not separately timed)

| metric | start_bytes | end_bytes | delta_bytes | mutations | bytes_per_mutation | peak_bytes |
|---|---|---|---|---|---|---|
| journal | 0 | 562677 | 562677 | 624 | 901.726 |  |
| state | 270 | 1378 | 1108 |  |  | 20484 |

### State-file size versus live volumes

| live_volumes | state_bytes | journal_bytes | last_create_ms |
|---|---|---|---|
| 1 | 1973 | 507612 | 175.051 |
| 8 | 6138 | 515525 | 174.924 |
| 32 | 20484 | 542714 | 195.73 |

## Mixed concurrency sweep

| label | concurrency | admission_cap | queue_timeout_s | rate_limit_per_s | duration_s | ops | ops_per_sec | success_ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | accepted_p95_ms | read_p95_ms | write_p95_ms | http_503 | http_429 | network_errors | rss_first_kib | rss_last_kib | rss_max_kib | fds_first | fds_last | fds_max | journal_delta_bytes | state_delta_bytes | state_max_bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| c1 | 1 | 256 | 5.0 | 0 | 30.001 | 54765 | 1825.419 | 1825.419 | 0.0 | 0.157 | 1.457 | 1.576 | 1.457 | 0.188 | 1.545 | 0 | 0 | 0 | 13648.0 | 54136.0 | 54136.0 | 14.0 | 14.0 | 15.0 | 14792466.0 | 10.0 | 1036.0 |
| c128 | 128 | 256 | 5.0 | 0 | 39.394 | 25816 | 655.326 | 655.326 | 0.0 | 0.91 | 117.87 | 7693.23 | 117.87 | 6.584 | 5078.906 | 0 | 0 | 0 | 702104.0 | 775320.0 | 776688.0 | 227.0 | 91.0 | 397.0 | 6374128.0 | 11069.0 | 118143.0 |
| c32 | 32 | 256 | 5.0 | 0 | 30.027 | 78347 | 2609.257 | 2609.257 | 0.0 | 0.441 | 46.913 | 96.309 | 46.913 | 0.958 | 82.927 | 0 | 0 | 0 | 160352.0 | 673100.0 | 673100.0 | 109.0 | 14.0 | 109.0 | -36736329.0 | -5961.0 | 30310.0 |
| c8 | 8 | 256 | 5.0 | 0 | 30.004 | 115424 | 3846.899 | 3846.899 | 0.0 | 0.388 | 7.478 | 11.436 | 7.478 | 0.861 | 10.374 | 0 | 0 | 0 | 58744.0 | 142272.0 | 142272.0 | 35.0 | 21.0 | 37.0 | 31366406.0 | -679.0 | 7758.0 |
| cap32-c128 | 128 | 32 | 1.0 | 0 | 16.939 | 10956 | 646.779 | 620.981 | 0.039887 | 38.099 | 1000.911 | 2622.998 | 660.598 | 772.862 | 1638.376 | 434 | 0 | 0 | 136884.0 | 164716.0 | 164924.0 | 203.0 | 61.0 | 205.0 | 2352902.0 | 1990.0 | 106426.0 |
| rate50-c64 | 64 | 256 | 5.0 | 50 | 15.318 | 1734 | 113.198 | 50.593 | 0.553057 | 1.379 | 9.757 | 13.809 | 7.064 | 8.417 | 12.003 | 0 | 959 | 0 | 93476.0 | 99384.0 | 99384.0 | 13.0 | 13.0 | 15.0 | 192871.0 | 3846.0 | 44865.0 |

## Shortened soak

| duration_s | workers | ops | ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | rss_first_kib | rss_last_kib | rss_max_kib | rss_kib_per_second | fds_first | fds_last | fds_max | fds_per_second | journal_first_bytes | journal_last_bytes | journal_delta_bytes | state_first_bytes | state_last_bytes | state_delta_bytes | restarts | restart_healthy_seconds |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1500.002 | 4 | 7134530 | 4756.348 | 1.1e-05 | 0.299 | 2.874 | 3.622 | 93844.0 | 1705896.0 | 2361868.0 | 572.3212 | 25.0 | 18.0 | 25.0 | -0.000937 | 23927809.0 | 49560395.0 | 25632586.0 | 3896.0 | 3147.0 | -749.0 | 1 | 0.629 |

## Restart replay

| scenario | iteration | journal_bytes_before | state_bytes_before | stop_seconds | healthy_seconds | state_bytes_after | journal_bytes_after | listed_volumes | missing_volumes |
|---|---|---|---|---|---|---|---|---|---|
| restart-post-soak | 1 | 50418275 | 198 | 0.301 | 0.608 | 198 | 50418275 | 0 |  |
| restart-post-soak | 2 | 50418275 | 198 | 0.164 | 0.606 | 198 | 50418275 | 0 |  |
| restart-pre-soak | 1 | 563927 | 198 | 0.02 | 0.082 | 198 | 563927 | 0 |  |
| restart-pre-soak | 2 | 563927 | 198 | 0.003 | 0.081 | 198 | 563927 | 0 |  |
| restart-pre-soak | 3 | 563927 | 198 | 0.003 | 0.102 | 198 | 563927 | 0 |  |
| soak-restart | 1 | 55004537 | 2971 | 0.342 | 0.629 | 2971 | 55004537 | 4 |  |
