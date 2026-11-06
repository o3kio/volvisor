# Benchmark tables

Generated from the raw JSON records by `scripts/bench/summarize.py`.
Percentiles are nearest-rank over every recorded request; the per-operation
`*-ops.csv` samples (first 5,000 per operation) are not committed, and these
tables are derived from the exact JSON aggregates.

## Micro (concurrency 1)

| operation | count | errors | min_ms | mean_ms | p50_ms | p95_ms | p99_ms | max_ms |
|---|---|---|---|---|---|---|---|---|
| attach | 150 | 0 | 1.251 | 1.487 | 1.473 | 1.711 | 1.83 | 1.952 |
| capabilities | 100 | 0 | 0.091 | 0.107 | 0.104 | 0.131 | 0.137 | 0.181 |
| create | 72 | 0 | 144.585 | 157.076 | 155.153 | 175.196 | 187.09 | 187.09 |
| delete | 72 | 0 | 103.683 | 117.191 | 114.396 | 127.88 | 144.612 | 144.612 |
| detach | 150 | 0 | 1.269 | 1.48 | 1.455 | 1.67 | 1.732 | 1.796 |
| grow | 30 | 0 | 154.594 | 177.115 | 175.107 | 195.535 | 205.348 | 205.348 |
| inspect | 400 | 0 | 0.097 | 0.117 | 0.109 | 0.16 | 0.182 | 0.269 |
| list | 100 | 0 | 0.102 | 0.116 | 0.111 | 0.139 | 0.179 | 0.195 |
| report | 150 | 0 | 1.253 | 1.473 | 1.448 | 1.688 | 1.773 | 1.815 |

## Journal and state-file measurements (derived; not separately timed)

| metric | start_bytes | end_bytes | delta_bytes | mutations | bytes_per_mutation | peak_bytes |
|---|---|---|---|---|---|---|
| journal | 0 | 562677 | 562677 | 624 | 901.726 |  |
| state | 270 | 1378 | 1108 |  |  | 20484 |

### State-file size versus live volumes

| live_volumes | state_bytes | journal_bytes | last_create_ms |
|---|---|---|---|
| 1 | 1973 | 507612 | 154.824 |
| 8 | 6138 | 515525 | 165.184 |
| 32 | 20484 | 542714 | 155.366 |

## Mixed concurrency sweep

| label | concurrency | admission_cap | queue_timeout_s | rate_limit_per_s | duration_s | ops | ops_per_sec | success_ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | accepted_p95_ms | read_p95_ms | write_p95_ms | http_503 | http_429 | network_errors | rss_first_kib | rss_last_kib | rss_max_kib | fds_first | fds_last | fds_max | journal_delta_bytes | state_delta_bytes | state_max_bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| c1 | 1 | 256 | 5.0 | 0 | 30.001 | 55367 | 1845.526 | 1845.526 | 0.0 | 0.155 | 1.457 | 1.577 | 1.457 | 0.186 | 1.546 | 0 | 0 | 0 | 13840.0 | 54664.0 | 54664.0 | 14.0 | 14.0 | 15.0 | 14887587.0 | -157.0 | 1034.0 |
| c128 | 128 | 256 | 5.0 | 0 | 30.162 | 7151 | 237.088 | 237.088 | 0.0 | 1.005 | 3553.448 | 7284.378 | 3553.448 | 197.815 | 6330.548 | 0 | 0 | 0 | 173600.0 | 188788.0 | 188788.0 | 143.0 | 123.0 | 143.0 | 1766602.0 | 6184.0 | 96344.0 |
| c32 | 32 | 256 | 5.0 | 0 | 30.027 | 25774 | 858.371 | 858.371 | 0.0 | 0.475 | 197.17 | 778.185 | 197.17 | 1.188 | 590.847 | 0 | 0 | 0 | 138648.0 | 159028.0 | 159028.0 | 47.0 | 29.0 | 47.0 | 7194529.0 | -64.0 | 25037.0 |
| c8 | 8 | 256 | 5.0 | 0 | 30.004 | 102335 | 3410.746 | 3410.746 | 0.0 | 0.391 | 7.467 | 11.753 | 7.467 | 0.853 | 10.47 | 0 | 0 | 0 | 57348.0 | 132376.0 | 132376.0 | 23.0 | 22.0 | 23.0 | 27926887.0 | -389.0 | 6618.0 |
| cap32-c128 | 128 | 32 | 1.0 | 0 | 15.613 | 3337 | 213.738 | 208.806 | 0.023075 | 459.568 | 1407.547 | 2507.535 | 1443.329 | 901.902 | 2271.921 | 77 | 0 | 0 | 182964.0 | 190620.0 | 190620.0 | 142.0 | 143.0 | 143.0 | 728494.0 | 1536.0 | 101371.0 |
| rate50-c64 | 64 | 256 | 5.0 | 50 | 15.5 | 1677 | 108.195 | 50.388 | 0.534287 | 5.173 | 195.622 | 205.833 | 199.787 | 193.122 | 204.383 | 0 | 896 | 0 | 165700.0 | 175472.0 | 175472.0 | 13.0 | 13.0 | 26.0 | 189617.0 | 8193.0 | 48528.0 |

## Shortened soak

| duration_s | workers | ops | ops_per_sec | error_rate | p50_ms | p95_ms | p99_ms | rss_first_kib | rss_last_kib | rss_max_kib | rss_kib_per_second | fds_first | fds_last | fds_max | fds_per_second | journal_first_bytes | journal_last_bytes | journal_delta_bytes | state_first_bytes | state_last_bytes | state_delta_bytes | restarts | restart_healthy_seconds |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1500.002 | 4 | 6929746 | 4619.824 | 1.1e-05 | 0.302 | 2.845 | 3.591 | 164940.0 | 3108756.0 | 3236184.0 | 1495.0333 | 19.0 | 18.0 | 21.0 | -0.000299 | 56646344.0 | 43412309.0 | -13234035.0 | 3312.0 | 3401.0 | 89.0 | 1 | 0.547 |

## Restart replay

| scenario | iteration | journal_bytes_before | state_bytes_before | stop_seconds | healthy_seconds | state_bytes_after | journal_bytes_after | listed_volumes | missing_volumes |
|---|---|---|---|---|---|---|---|---|---|
| restart-post-soak | 1 | 44251356 | 198 | 0.361 | 0.527 | 198 | 44251356 | 0 |  |
| restart-post-soak | 2 | 44251356 | 198 | 0.164 | 0.506 | 198 | 44251356 | 0 |  |
| restart-pre-soak | 1 | 563927 | 198 | 0.02 | 0.082 | 198 | 563927 | 0 |  |
| restart-pre-soak | 2 | 563927 | 198 | 0.003 | 0.102 | 198 | 563927 | 0 |  |
| restart-pre-soak | 3 | 563927 | 198 | 0.003 | 0.102 | 198 | 563927 | 0 |  |
| soak-restart | 1 | 42885689 | 3140 | 0.402 | 0.547 | 3140 | 42885689 | 4 |  |
