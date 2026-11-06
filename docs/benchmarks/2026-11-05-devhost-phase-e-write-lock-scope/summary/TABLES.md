# Benchmark tables

Generated from the raw JSON records by `scripts/bench/summarize.py`.
Percentiles are nearest-rank over every recorded request; the per-operation
`*-ops.csv` samples (first 5,000 per operation) are not committed, and these
tables are derived from the exact JSON aggregates.

## Micro (concurrency 1)

| operation | count | errors | min_ms | mean_ms | p50_ms | p95_ms | p99_ms | max_ms |
|---|---|---|---|---|---|---|---|---|
| attach | 150 | 0 | 1.225 | 1.471 | 1.433 | 1.717 | 1.851 | 1.906 |
| capabilities | 100 | 0 | 0.104 | 0.112 | 0.109 | 0.125 | 0.135 | 0.138 |
| create | 72 | 0 | 174.518 | 195.106 | 194.991 | 215.837 | 267.711 | 267.711 |
| delete | 72 | 0 | 134.269 | 156.694 | 154.492 | 174.464 | 175.215 | 175.215 |
| detach | 150 | 0 | 1.267 | 1.464 | 1.424 | 1.774 | 1.879 | 1.935 |
| grow | 30 | 0 | 195.057 | 219.03 | 215.407 | 235.593 | 235.881 | 235.881 |
| inspect | 400 | 0 | 0.107 | 0.131 | 0.124 | 0.17 | 0.181 | 0.245 |
| list | 100 | 0 | 0.111 | 0.121 | 0.118 | 0.133 | 0.141 | 0.151 |
| report | 150 | 0 | 1.246 | 1.449 | 1.422 | 1.695 | 1.882 | 1.971 |

## Journal and state-file measurements (derived; not separately timed)

| metric | start_bytes | end_bytes | delta_bytes | mutations | bytes_per_mutation | peak_bytes |
|---|---|---|---|---|---|---|
| journal | 0 | 562677 | 562677 | 624 | 901.726 |  |
| state | 270 | 1378 | 1108 |  |  | 20484 |

### State-file size versus live volumes

| live_volumes | state_bytes | journal_bytes | last_create_ms |
|---|---|---|---|
| 1 | 1973 | 507612 | 184.943 |
| 8 | 6138 | 515525 | 184.951 |
| 32 | 20484 | 542714 | 225.621 |

## Mixed concurrency sweep

| label | concurrency | admission_cap | queue_timeout_s | rate_limit_per_s | duration_s | ops | ops_per_sec | success_ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | accepted_p95_ms | read_p95_ms | write_p95_ms | http_503 | http_429 | network_errors | rss_first_kib | rss_last_kib | rss_max_kib | fds_first | fds_last | fds_max | journal_delta_bytes | state_delta_bytes | state_max_bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| c1 | 1 | 256 | 5.0 | 0 | 30.001 | 54691 | 1822.948 | 1822.948 | 0.0 | 0.153 | 1.462 | 1.576 | 1.462 | 0.183 | 1.549 | 0 | 0 | 0 | 13444.0 | 54056.0 | 54056.0 | 14.0 | 14.0 | 15.0 | 14828289.0 | -74.0 | 1034.0 |
| c128 | 128 | 256 | 5.0 | 0 | 41.639 | 25441 | 610.988 | 610.988 | 0.0 | 0.806 | 100.558 | 8298.483 | 100.558 | 4.691 | 4297.266 | 0 | 0 | 0 | 694340.0 | 773236.0 | 775948.0 | 214.0 | 28.0 | 397.0 | 6374016.0 | 4832.0 | 117510.0 |
| c32 | 32 | 256 | 5.0 | 0 | 30.028 | 74357 | 2476.245 | 2476.245 | 0.0 | 0.427 | 46.651 | 94.6 | 46.651 | 0.895 | 81.996 | 0 | 0 | 0 | 158912.0 | 669360.0 | 681652.0 | 109.0 | 46.0 | 109.0 | -38910939.0 | -5033.0 | 30553.0 |
| c8 | 8 | 256 | 5.0 | 0 | 30.005 | 112718 | 3756.659 | 3756.659 | 0.0 | 0.378 | 7.54 | 11.699 | 7.54 | 0.81 | 10.66 | 0 | 0 | 0 | 58204.0 | 140632.0 | 140632.0 | 37.0 | 22.0 | 37.0 | 30694094.0 | -1127.0 | 7887.0 |
| cap32-c128 | 128 | 32 | 1.0 | 0 | 16.729 | 9595 | 573.565 | 544.872 | 0.050026 | 35.251 | 1001.305 | 3104.188 | 298.223 | 1000.588 | 2530.198 | 480 | 0 | 0 | 131572.0 | 150932.0 | 150932.0 | 205.0 | 64.0 | 205.0 | 1971389.0 | -2645.0 | 105468.0 |
| rate50-c64 | 64 | 256 | 5.0 | 50 | 15.268 | 1732 | 113.439 | 50.825 | 0.551963 | 1.637 | 8.614 | 14.534 | 6.5 | 7.044 | 11.863 | 0 | 956 | 0 | 83836.0 | 92496.0 | 92496.0 | 13.0 | 13.0 | 17.0 | 197086.0 | 5421.0 | 45666.0 |

## Shortened soak

| duration_s | workers | ops | ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | rss_first_kib | rss_last_kib | rss_max_kib | rss_kib_per_second | fds_first | fds_last | fds_max | fds_per_second | journal_first_bytes | journal_last_bytes | journal_delta_bytes | state_first_bytes | state_last_bytes | state_delta_bytes | restarts | restart_healthy_seconds |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1500.002 | 4 | 6626734 | 4417.815 | 7e-06 | 0.301 | 2.929 | 3.89 | 88020.0 | 1362124.0 | 2492976.0 | 393.6136 | 21.0 | 18.0 | 27.0 | -0.000978 | 21521776.0 | 17862039.0 | -3659737.0 | 3005.0 | 2977.0 | -28.0 | 1 | 0.345 |

## Restart replay

| scenario | iteration | journal_bytes_before | state_bytes_before | stop_seconds | healthy_seconds | state_bytes_after | journal_bytes_after | listed_volumes | missing_volumes |
|---|---|---|---|---|---|---|---|---|---|
| restart-post-soak | 1 | 18142395 | 198 | 0.161 | 0.285 | 198 | 18142395 | 0 |  |
| restart-post-soak | 2 | 18142395 | 198 | 0.063 | 0.283 | 198 | 18142395 | 0 |  |
| restart-pre-soak | 1 | 563927 | 198 | 0.02 | 0.103 | 198 | 563927 | 0 |  |
| restart-pre-soak | 2 | 563927 | 198 | 0.003 | 0.081 | 198 | 563927 | 0 |  |
| restart-pre-soak | 3 | 563927 | 198 | 0.003 | 0.081 | 198 | 563927 | 0 |  |
| soak-restart | 1 | 22916775 | 3385 | 0.262 | 0.345 | 3385 | 22917120 | 4 |  |
