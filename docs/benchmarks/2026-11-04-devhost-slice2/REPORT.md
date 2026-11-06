# Control-plane benchmark report — 2026-11-04 D3 slice 2, single-host development lab

**Status:** single-host development-lab measurement of the Phase D D3 slice-2
write path. Not production evidence, not an SLA, not a data-path benchmark,
and not the issue's 24-hour soak. Read the caveats in §10 before using any
number.

- **Exact source:** `c4b0b1303594c494bb26d3643d952434eda8ec34`
  (`volvisord c4b0b13`), **worktree clean** at run time
  (`worktree_dirty: no`).
- **Run window:** 2026-11-04T23:10:13Z .. 2026-11-04T23:40:32Z. The release
  binary was built before the run; the run used `--skip-build`.
- **Host:** one development host (`dev`), Ubuntu 24.04.5 LTS, kernel
  `6.8.0-142-generic`, 16 vCPUs on an AMD EPYC 9554P, 31.3 GiB RAM, QEMU
  virtual disks. Daemon and all clients on `127.0.0.1`.
- **Binary:** `target/release/volvisord`, SHA256
  `1f86136093792c161e85d9c8924bc965b635e54d157d975a9780a2d2ddc81bf6`,
  built with `cargo build --release --locked -p volvisord` and
  `CARGO_BUILD_JOBS=4`.
- **Provenance:** [SCRIPTS.txt](SCRIPTS.txt).
- **Comparator:** [comparison/comparison.md](comparison/comparison.md),
  generated against
  [2026-11-04-devhost-postfix](../2026-11-04-devhost-postfix/REPORT.md)
  with `scripts/bench/compare.py` (manifests verified on both sides).

## 1. Scope and what changed

This run repeats the Phase C harness shape at the final D3 slice-2 commit.
Slice 2 changes only the provider write path:

- **Attach, detach and an applied attachment report release the provider
  state lock** after capturing the mutation and reserving a monotonic
  sequence, then wait for that sequence's permanent watermark. One leader
  writes the newest capture once (a file `sync_all` plus a directory
  `sync_all`) and resolves every covered waiter.
- **Create, grow, delete, move and admin** remain synchronous: they hold the
  state lock and lead the write inline, as in slice 1.
- **Admission resolves an outstanding capture before deriving an answer.**
  Asynchronous attach/detach/report wait without the state lock; synchronous
  callers use `lock_state_stable`. This closes the same-id replay window
  where a journaled success could be derived from a write whose durability
  was still unproven.
- Reads were already served from a published snapshot and do not take the
  provider state lock; this run therefore measures the resulting split
  between reads served from that snapshot and writes that still serialize.

Correctness evidence for this change is the test suite at `c4b0b13`, not
this run. The harness can show behaviour and resource shape, not crash
consistency or fencing under partition.

## 2. What was executed

| Scenario | Shape | Result | Raw record |
|---|---|---|---|
| Micro | concurrency 1: create/delete, attach→report→detach, grow, inspect, list, capabilities; state-file scaling at 1/8/32 live volumes | 0 errors in 1,224 recorded ops | [micro.json](json/micro.json), [micro.csv](summary/micro.csv) |
| Mixed sweep | 1 / 8 / 32 / 128 in-flight, 70 % read-ratio target, 30 s per tier | 0 errors; throughput ceiling unchanged | [mixed-c1](json/mixed-c1.json), [c8](json/mixed-c8.json), [c32](json/mixed-c32.json), [c128](json/mixed-c128.json) |
| Admission cap probe | 128 in-flight, cap 32, 1 s queue timeout, 15 s (diagnostic) | 77 × `503`; shedding is visible this run | [mixed-cap32-c128.json](json/mixed-cap32-c128.json) |
| Rate-limit shedding probe | 64 in-flight, 50 req/s burst 50, 15 s (diagnostic) | 896 × `429 RATE_LIMITED`; 50.388 accepted ops/s | [mixed-rate50-c64.json](json/mixed-rate50-c64.json) |
| Shortened soak | 25 min, 4 workers, continuous inspect/attach/report/detach/grow, restart at 12 min, per-second resource samples | completed; state bounded, journal bounded, RSS high-water filed | [soak.json](json/soak.json) (per-second timeline not committed) |
| Restart replay | 3 pre-soak restarts, 1 mid-soak restart, 2 post-soak restarts | healthy in 0.082 s – 0.547 s; mid-soak replay listed 4/4 volumes | [restart-pre-soak.json](json/restart-pre-soak.json), [restart-post-soak.json](json/restart-post-soak.json) |

Every acknowledged mutation used the daemon's real journal append + fsync
and provider-state rewrite + fsync. There are no mocks on the write path.

## 3. Command

```bash
CARGO_BUILD_JOBS=4 cargo build --release -p volvisord --locked
sudo scripts/bench/run-all.sh \
  --run-dir /tmp/opencode/volvisor-bench-slice2-20261104T231012Z \
  --skip-build --sweep-seconds 30 --soak-minutes 25
```

`--skip-build` is deliberate: the executed binary is the one built from the
recorded clean tree, not a binary rebuilt inside the harness.

## 4. Capacity table: 1/8/32/128 mixed in-flight

Exact values from [summary/concurrency.csv](summary/concurrency.csv). The
product default admission cap is 256 for the four tiers.

| tier | in-flight | ops | ops/s | error rate | p50 ms | p95 ms | p99 ms | read p95 ms | write p95 ms | 503 | 429 | RSS max KiB | fds max | state max B |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| c1 | 1 | 55,367 | 1845.526 | 0.0 | 0.155 | 1.457 | 1.577 | 0.186 | 1.546 | 0 | 0 | 54,664 | 15 | 1,034 |
| c8 | 8 | 102,335 | 3410.746 | 0.0 | 0.391 | 7.467 | 11.753 | 0.853 | 10.47 | 0 | 0 | 132,376 | 23 | 6,618 |
| c32 | 32 | 25,774 | 858.371 | 0.0 | 0.475 | 197.17 | 778.185 | 1.188 | 590.847 | 0 | 0 | 159,028 | 47 | 25,037 |
| c128 | 128 | 7,151 | 237.088 | 0.0 | 1.005 | 3553.448 | 7284.378 | 197.815 | 6330.548 | 0 | 0 | 188,788 | 143 | 96,344 |

All 190,627 recorded sweep requests returned 2xx; no `503`, `429` or network
error occurred in the default-cap sweep. Durations were 30.001 s, 30.004 s,
30.027 s and 30.162 s.

Derived observations:

- **The throughput ceiling is unchanged.** Against
  [2026-11-04-devhost-postfix](../2026-11-04-devhost-postfix/REPORT.md), c1
  is −5.5 %, c8 −2.4 %, c32 −0.5 % and c128 +0.7 %. This run does **not**
  establish a throughput gain from releasing the state lock; the differences
  are at or near single-run variance.
- **The latency split changed.** Read p95 at c32 fell from 179.332 ms to
  1.188 ms and at c128 from 1288.505 ms to 197.815 ms. Reads were already
  snapshot reads at both SHAs; the improvement is consistent with async
  waiters no longer pinning runtime workers across the fsync, but the harness
  does not attribute delay. Write p95 moved the other way: c32
  388.84 → 590.847 ms and c128 1552.261 → 6330.548 ms. Overall p95 at c128
  therefore rose (1349.487 → 3553.448 ms) even though overall p50 fell
  (462.278 → 1.005 ms).
- **Plausible cause (not proven by this harness):** each mixed worker
  periodically issues `grow`, which still holds the state lock across an LVM
  resize (~200 ms median). Async writers yield their runtime worker while
  writers that need the state lock still queue behind `grow`; the harness
  does not isolate those effects. This is an observation, not a measured
  cause and not a claim that a specific code path is at fault.
- 8 in-flight is 1.85× the 1-in-flight throughput; 32 is 0.252× the
  8-in-flight throughput, and 128 is 0.276× the 32-in-flight throughput.
- Throughput **falls** above 8 in-flight, as before. This is a control-plane
  ceiling, not a measured CPU ceiling: 16 vCPUs were available and no
  CPU-saturation metric was sampled.

The `state max B` column is the largest state file observed during the tier.
It is not a clean function of live volumes, because each mixed worker owns
one volume and continuously attaches/detaches. Even c128 stays below
**96 KiB**.

## 5. Micro (concurrency 1)

Exact values from [summary/micro.csv](summary/micro.csv).

| operation | count | errors | min ms | mean ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| create | 72 | 0 | 144.585 | 157.076 | 155.153 | 175.196 | 187.09 | 187.09 |
| delete | 72 | 0 | 103.683 | 117.191 | 114.396 | 127.88 | 144.612 | 144.612 |
| attach | 150 | 0 | 1.251 | 1.487 | 1.473 | 1.711 | 1.83 | 1.952 |
| report | 150 | 0 | 1.253 | 1.473 | 1.448 | 1.688 | 1.773 | 1.815 |
| detach | 150 | 0 | 1.269 | 1.48 | 1.455 | 1.67 | 1.732 | 1.796 |
| grow | 30 | 0 | 154.594 | 177.115 | 175.107 | 195.535 | 205.348 | 205.348 |
| inspect | 400 | 0 | 0.097 | 0.117 | 0.109 | 0.16 | 0.182 | 0.269 |
| list | 100 | 0 | 0.102 | 0.116 | 0.111 | 0.139 | 0.179 | 0.195 |
| capabilities | 100 | 0 | 0.091 | 0.107 | 0.104 | 0.131 | 0.137 | 0.181 |

LVM command cost dominates create/delete/grow; attach/report/detach are state
mutations at roughly 1.45 ms p50 (about 0.13–0.15 ms above the post-fix
baseline, within single-run variance); inspect/list/capabilities are
sub-millisecond p95 at this dataset.

## 6. Diagnostic probes

Exact values from [summary/concurrency.csv](summary/concurrency.csv).

| probe | in-flight | cap | queue timeout | rate limit | duration | ops | ops/s | accepted ops/s | error rate | p95 ms | 503 | 429 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| cap32-c128 | 128 | 32 | 1 s | 0 | 15.613 s | 3,337 | 213.738 | 208.806 | 0.023075 | 1407.547 | 77 | 0 |
| rate50-c64 | 64 | 256 | 5 s | 50/s | 15.5 s | 1,677 | 108.195 | 50.388 | 0.534287 | 195.622 | 0 | 896 |

- **Cap 32, 128 in-flight:** 77 requests observed the 1 s queue timeout and
  returned `503`, unlike the post-fix run where none did. This is a
  diagnostic shape with a non-default cap, not a capacity result; it shows
  shedding is reachable at this setting.
- **Rate limit 50/s:** 896 `429 RATE_LIMITED`, accepted throughput
  50.388 ops/s, all-attempt error rate 0.534287. This is real admission
  shedding. The client backed off and reconnected after each `429`; the
  harness behaviour is not a performance fix.

## 7. Shortened soak (25 minutes, 4 workers)

Exact values from [summary/soak.csv](summary/soak.csv) and
[json/soak.json](json/soak.json).

| metric | value |
|---|---:|
| duration | 1500.002 s (25.000 min) |
| workers | 4 |
| total ops | 6,929,746 |
| throughput | 4619.824 ops/s |
| successful throughput | 4619.773 ops/s |
| accepted ops | 6,929,669 |
| error rate | 1.1 × 10⁻⁵ |
| errors | 77, all `NETWORK_ERROR` at the deliberate restart (74 `ConnectionRefusedError`, 2 `RemoteDisconnected`, 1 `BrokenPipeError`) |
| HTTP errors / conflicts | **0** |
| p50 / p95 / p99 | 0.302 / 2.845 / 3.591 ms |
| attach / report / detach | 462,041 / 462,037 / 462,037; the only attach error is 1 request caught by the restart `NETWORK_ERROR` |
| grow | 512, 0 errors |
| inspect / list / capabilities | 3,464,911 / 1,661,169 / 417,039; 76 restart `NETWORK_ERROR` on inspect, the rest 0 |
| restarts | 1 (at 12 min) |
| mid-soak healthy time | 0.547 s |
| mid-soak replay | 4/4 volumes listed, state 3,140 B, journal 42,885,689 B |

The 77 network errors are the client observing the daemon stop/start at the
planned restart. They are not provider or admission errors; the soak status
is `OK` and `status_counts` is `{200: 6,929,669, 0: 77}`.

The recorded read ratio is 0.7999 of all operations (target 0.6), the same
actual ratio as the post-fix baseline. Operation counts scaled with total
ops; the mix is therefore comparable, but this is still one run on a
non-fixed dataset.

### Resources

| metric | first | last | delta | min | max |
|---|---:|---:|---:|---:|---:|
| RSS KiB | 164,940 | 3,108,756 | +2,943,816 | 135,692 | **3,236,184** |
| journal bytes | 56,646,344 | 43,412,309 | −13,234,035 | 8,358,531 | **75,497,689** |
| state bytes | 3,312 | 3,401 | +89 | 2,537 | 3,684 |
| fds | 19 | 18 | −1 | 13 | 21 |

The journal stayed bounded: it never exceeded **75,497,689 B (72.0 MiB)** and
ended at **43,412,309 B (41.4 MiB)**. The effective (default) `max_log_bytes`
is 64 MiB; after the first compaction the trigger additionally requires
64 MiB of growth since the last compaction, so the log can overshoot the
bound by up to that floor between rewrites. The observed maximum is
consistent with that hysteresis. The state file never exceeded **3,684 B**
and ended at **3,401 B**; it does not grow with attachment history.

**RSS is the open residual, and it is larger in this run.** RSS rose to a
peak of **3,236,184 KiB (≈3.09 GiB)** and ended at 3,108,756 KiB, versus a
peak of 2,323,028 KiB and an end of 1,950,048 KiB in the post-fix baseline.
The peak sample (elapsed 1390.956 s) is followed one second later by the
journal's smallest value in the final five minutes (8,393,141 B at elapsed
1391.958 s) with RSS still at its peak, which is consistent with
allocator/compaction transients, but a leak is neither proven nor ruled out.
No RSS cap or fix is claimed.

### Daemon log (operational finding)

The default daemon log at INFO level is **unbounded**. It covers the whole
~30-minute run, not only the 25-minute soak:

| metric | value |
|---|---:|
| uncompressed bytes | 2,306,552,235 (≈2.15 GiB) |
| lines | 8,575,422 |
| INFO lines | 8,574,997 |
| WARN lines | 425 |
| compressed bundle copy | 60,577,546 B |

All 425 WARN lines are `grow notification outstanding; retried next pass`.
There is no rotation or size cap. This is an operational finding, not a
durable-state bug, and no logging change is claimed here. The bundle stores
the log as `raw/daemon.daemon.log.gz` because the plaintext file exceeds
forge limits; `gunzip` reproduces the run log and no measurement file was
edited.

## 8. Restart replay

Exact values from [summary/restart.csv](summary/restart.csv).

| scenario | journal before | state before | stop s | healthy s | state after | journal after | listed | missing |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| restart-pre-soak #1 | 563,927 | 198 | 0.020 | 0.082 | 198 | 563,927 | 0 | — |
| restart-pre-soak #2 | 563,927 | 198 | 0.003 | 0.102 | 198 | 563,927 | 0 | — |
| restart-pre-soak #3 | 563,927 | 198 | 0.003 | 0.102 | 198 | 563,927 | 0 | — |
| soak-restart | 42,885,689 | 3,140 | 0.402 | **0.547** | 3,140 | 42,885,689 | **4** | — |
| restart-post-soak #1 | 44,251,356 | 198 | 0.361 | 0.527 | 198 | 44,251,356 | 0 | — |
| restart-post-soak #2 | 44,251,356 | 198 | 0.164 | 0.506 | 198 | 44,251,356 | 0 | — |

Every restart preserved the journal and state byte-for-byte (post-restart
sizes equal pre-restart sizes) and the mid-soak restart recovered all four
live volumes. Recovery time here is readiness of the HTTP listener plus a
successful list, not a data-path recovery objective.

## 9. Before/after comparison

Generated by `scripts/bench/compare.py`; full tables in
[comparison/comparison.md](comparison/comparison.md). Both manifests were
verified and both runs are clean-tree. The baseline is
`aab773b7b58448e41ba22b21372785b3c58e7381` (2026-11-04 post-fix), not the
older pre-fix run.

| metric | 2026-11-04 post-fix (`aab773b`) | 2026-11-04 D3 slice 2 (`c4b0b13`) | delta |
|---|---:|---:|---:|
| c1 ops/s | 1952.155 | 1845.526 | −5.5 % |
| c8 ops/s | 3493.445 | 3410.746 | −2.4 % |
| c32 ops/s | 862.937 | 858.371 | −0.5 % |
| c128 ops/s | 235.502 | 237.088 | +0.7 % |
| c32 read p95 | 179.332 ms | 1.188 ms | −99.3 % |
| c128 read p95 | 1288.505 ms | 197.815 ms | −84.6 % |
| c32 write p95 | 388.84 ms | 590.847 ms | +52.0 % |
| c128 write p95 | 1552.261 ms | 6330.548 ms | +307.8 % |
| soak ops | 5,424,744 | 6,929,746 | +27.7 % |
| soak ops/s | 3616.49 | 4619.824 | +27.7 % |
| soak p95 | 3.410 ms | 2.845 ms | −16.6 % |
| soak journal end | 27,982,748 B | 43,412,309 B | +55.1 % |
| soak state end | 3,055 B | 3,401 B | +11.3 % |
| soak RSS delta | +1,780,896 KiB | +2,943,816 KiB | +65.3 % |

Both soaks recorded **0 attach conflicts and 0 HTTP errors**; the 70/77
network errors are client reconnects at the deliberate restart. The operation
mix is comparable (both ≈80 % reads by the recorded actual ratio; the soak
inspects before every write), so the +27.7 % soak operation delta is a
rate/latency change rather than a different acceptance mix. It is still a
single-run, non-fixed-dataset observation: the soak also grew RSS 65 % more,
and no capacity or efficiency claim follows from it.

**The mixed sweep is the cleaner comparison, and it shows no throughput
ceiling change.** What changed is the latency composition: read p95 fell
sharply while write p95 rose at c32/c128. Reads were already snapshot reads;
the split is consistent with async writers yielding runtime workers while
synchronous grow still holds the state lock, but the harness does not
attribute it. That trade-off is the measured result of the slice, not
evidence of a throughput win.

## 10. Caveats and NOT RUN

- **One host.** One `volvisord`, one client host, loopback only. No
  multi-host, no network hop, no second failure domain.
- **Loop-backed LVM.** The VG is a file-backed loop device, not a physical
  disk; the journal and state share the same ext4 filesystem as the loop
  image.
- **Control plane only.** No guest I/O, no block-data proxying, no
  filesystem/fio workload, no data-path IOPS/latency.
- **Not the 24-hour soak.** 25.0 minutes with one restart. The 24 h soak is
  **NOT RUN**.
- **No real Ceph/DRBD/physical-disk LVM.** `drbdadm`, `rbd`, `ceph` and `fio`
  were absent; the Ceph and DRBD providers were not exercised.
- **Dataset is not fixed across tiers.** Each mixed worker owns one volume;
  live volume count and attachment churn differ per tier.
- **Not a correctness proof.** The harness cannot prove crash consistency,
  fencing under partition, or data durability. Those are covered (or not) by
  the test suites and the support matrix.
- **No throughput gain is claimed.** The sweep ceiling is unchanged and the
  write tail worsened; the soak rate increased but RSS grew more. Do not
  present any of these as a capacity improvement.
- **RSS high-water is unresolved.** See §7. Do not present it as a leak or as
  fixed.
- **The INFO log is unbounded.** See §7. No rotation is implemented.
- **The cap-32 probe is diagnostic.** Its 77 `503` responses are the probe's
  configured 1 s queue timeout, not the default-cap sweep.
- **No production/SLA claim.** No number here is a capacity plan, support
  commitment, RPO/RTO statement, or release approval.

## Appendix: files

| path | content |
|---|---|
| [ENVIRONMENT.txt](ENVIRONMENT.txt) | host, kernel, toolchain, binary digest, absent capabilities |
| [comparison/comparison.md](comparison/comparison.md) | before/after tables and per-minute normalisation |
| [summary/TABLES.md](summary/TABLES.md) | all generated tables in one file |
| [summary/concurrency.csv](summary/concurrency.csv) | exact mixed/probe aggregates |
| [summary/soak.csv](summary/soak.csv) | exact soak aggregate |
| [summary/journal-state.csv](summary/journal-state.csv) | micro journal/state measurements |
| [summary/state-scale.csv](summary/state-scale.csv) | state size versus live volumes |
| [summary/restart.csv](summary/restart.csv) | restart replay rows |
| [json/](json/) | exact aggregate JSON per scenario |
| [raw/00-orchestration.log](raw/00-orchestration.log) | unmodified orchestration trace |
| [raw/daemon.daemon.log.gz](raw/daemon.daemon.log.gz) | gzip daemon log (2.15 GiB raw) |
| [records/](records/) | lab loop/VG identity and pre/post VG listings |
| [configs/](configs/) | sanitized benchmark configs (tokens replaced) |

The per-operation CSV samples and per-second timelines are not committed; the
JSON aggregates and summary tables retain the measured values.
