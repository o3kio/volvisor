# Benchmark tables

Generated from the raw JSON records by `scripts/bench/summarize.py`.
Percentiles are nearest-rank over every recorded request; the per-operation
`*-ops.csv` samples (first 5,000 per operation) are not committed, and these
tables are derived from the exact JSON aggregates.

## Micro (concurrency 1)

| operation | count | errors | min_ms | mean_ms | p50_ms | p95_ms | p99_ms | max_ms |
|---|---|---|---|---|---|---|---|---|
| attach | 150 | 0 | 1.197 | 1.34 | 1.321 | 1.49 | 1.561 | 1.588 |
| capabilities | 100 | 0 | 0.103 | 0.112 | 0.11 | 0.126 | 0.134 | 0.134 |
| create | 72 | 0 | 134.674 | 158.379 | 155.068 | 175.382 | 197.115 | 197.115 |
| delete | 72 | 0 | 93.964 | 115.215 | 114.235 | 124.622 | 134.705 | 134.705 |
| detach | 150 | 0 | 1.186 | 1.348 | 1.322 | 1.532 | 1.744 | 1.838 |
| grow | 30 | 0 | 144.577 | 173.763 | 174.752 | 194.974 | 194.983 | 194.983 |
| inspect | 400 | 0 | 0.098 | 0.114 | 0.112 | 0.134 | 0.188 | 0.268 |
| list | 100 | 0 | 0.102 | 0.118 | 0.116 | 0.136 | 0.209 | 0.223 |
| report | 150 | 0 | 1.186 | 1.341 | 1.32 | 1.536 | 1.661 | 1.762 |

## Journal and state-file measurements (derived; not separately timed)

| metric | start_bytes | end_bytes | delta_bytes | mutations | bytes_per_mutation | peak_bytes |
|---|---|---|---|---|---|---|
| journal | 0 | 562677 | 562677 | 624 | 901.726 |  |
| state | 270 | 1378 | 1108 |  |  | 20484 |

### State-file size versus live volumes

| live_volumes | state_bytes | journal_bytes | last_create_ms |
|---|---|---|---|
| 1 | 1973 | 507612 | 154.647 |
| 8 | 6138 | 515525 | 154.815 |
| 32 | 20484 | 542714 | 175.14 |

## Mixed concurrency sweep

| label | concurrency | admission_cap | queue_timeout_s | rate_limit_per_s | duration_s | ops | ops_per_sec | success_ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | accepted_p95_ms | read_p95_ms | write_p95_ms | http_503 | http_429 | network_errors | rss_first_kib | rss_last_kib | rss_max_kib | fds_first | fds_last | fds_max | journal_delta_bytes | state_delta_bytes | state_max_bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| c1 | 1 | 256 | 5.0 | 0 | 30.0 | 58565 | 1952.155 | 1952.155 | 0.0 | 0.146 | 1.357 | 1.474 | 1.357 | 0.175 | 1.445 | 0 | 0 | 0 | 13356.0 | 56456.0 | 56456.0 | 14.0 | 14.0 | 15.0 | 15819591.0 | 92.0 | 1036.0 |
| c128 | 128 | 256 | 5.0 | 0 | 31.138 | 7333 | 235.502 | 235.502 | 0.0 | 462.278 | 1349.487 | 2117.379 | 1349.487 | 1288.505 | 1552.261 | 0 | 0 | 0 | 182136.0 | 191244.0 | 191244.0 | 143.0 | 21.0 | 143.0 | 1905687.0 | 9645.0 | 97270.0 |
| c32 | 32 | 256 | 5.0 | 0 | 30.014 | 25900 | 862.937 | 862.937 | 0.0 | 5.624 | 200.594 | 529.079 | 200.594 | 179.332 | 388.84 | 0 | 0 | 0 | 142112.0 | 159772.0 | 159772.0 | 47.0 | 46.0 | 47.0 | 6208123.0 | -737.0 | 24864.0 |
| c8 | 8 | 256 | 5.0 | 0 | 30.004 | 104816 | 3493.445 | 3493.445 | 0.0 | 0.535 | 5.928 | 6.927 | 5.928 | 1.254 | 6.635 | 0 | 0 | 0 | 59100.0 | 135284.0 | 135284.0 | 23.0 | 22.0 | 23.0 | 28555905.0 | -563.0 | 6725.0 |
| cap32-c128 | 128 | 32 | 1.0 | 0 | 15.063 | 3417 | 226.852 | 226.852 | 0.0 | 471.629 | 1313.393 | 1739.303 | 1313.393 | 1282.189 | 1535.852 | 0 | 0 | 0 | 183784.0 | 193352.0 | 193352.0 | 143.0 | 141.0 | 143.0 | 822395.0 | 2653.0 | 100562.0 |
| rate50-c64 | 64 | 256 | 5.0 | 50 | 15.745 | 1759 | 111.72 | 51.255 | 0.541217 | 3.697 | 192.768 | 207.884 | 194.54 | 184.496 | 200.79 | 0 | 952 | 0 | 170660.0 | 179392.0 | 179392.0 | 13.0 | 13.0 | 23.0 | 196431.0 | 8364.0 | 49043.0 |

## Shortened soak

| duration_s | workers | ops | ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | rss_first_kib | rss_last_kib | rss_max_kib | rss_kib_per_second | fds_first | fds_last | fds_max | fds_per_second | journal_first_bytes | journal_last_bytes | journal_delta_bytes | state_first_bytes | state_last_bytes | state_delta_bytes | restarts | restart_healthy_seconds |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1500.003 | 4 | 5424744 | 3616.49 | 1.3e-05 | 0.437 | 3.41 | 4.669 | 169152.0 | 1950048.0 | 2323028.0 | 815.0673 | 19.0 | 17.0 | 21.0 | -0.000391 | 58483131.0 | 27982748.0 | -30500383.0 | 3228.0 | 3055.0 | -173.0 | 1 | 0.552 |

## Restart replay

| scenario | iteration | journal_bytes_before | state_bytes_before | stop_seconds | healthy_seconds | state_bytes_after | journal_bytes_after | listed_volumes | missing_volumes |
|---|---|---|---|---|---|---|---|---|---|
| restart-post-soak | 1 | 28147713 | 198 | 0.241 | 0.391 | 198 | 28147713 | 0 |  |
| restart-post-soak | 2 | 28147713 | 198 | 0.114 | 0.367 | 198 | 28147713 | 0 |  |
| restart-pre-soak | 1 | 563927 | 198 | 0.02 | 0.103 | 198 | 563927 | 0 |  |
| restart-pre-soak | 2 | 563927 | 198 | 0.003 | 0.102 | 198 | 563927 | 0 |  |
| restart-pre-soak | 3 | 563927 | 198 | 0.003 | 0.102 | 198 | 563927 | 0 |  |
| soak-restart | 1 | 42744729 | 3385 | 0.285 | 0.552 | 3385 | 42744729 | 4 |  |
