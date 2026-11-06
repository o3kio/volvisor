# Control-plane benchmark report — 2026-11-04 post-fix, single-host development lab

**Status:** single-host development-lab measurement after the Phase C
state-substrate fixes. Not production evidence, not an SLA, not a data-path
benchmark, and not the issue's 24-hour soak. Read the caveats in §10 before
using any number.

- **Exact source:** `aab773b7b58448e41ba22b21372785b3c58e7381`
  (`volvisord aab773b`), **worktree clean** at run time
  (`worktree_dirty: no`).
- **Run window:** 2026-11-04T17:02:12Z .. 2026-11-04T17:32:25Z. The release
  binary was built before the run; the run used `--skip-build`.
- **Host:** one development host (`dev`), Ubuntu 24.04.5 LTS, kernel
  `6.8.0-142-generic`, 16 vCPUs on an AMD EPYC 9554P, 31.3 GiB RAM, QEMU
  virtual disks. Daemon and all clients on `127.0.0.1`.
- **Binary:** `target/release/volvisord`, SHA256
  `238895a42097fd06f33e30f941873b3a9ca82b623efaf2a60c6f375d2d1edf4a`,
  built with `cargo build --release --locked -p volvisord` and
  `CARGO_BUILD_JOBS=4`.
- **Provenance:** [SCRIPTS.txt](SCRIPTS.txt).
- **Comparator:** [comparison/comparison.md](comparison/comparison.md),
  generated against [2026-11-03-devhost](../2026-11-03-devhost/REPORT.md)
  with `scripts/bench/compare.py` (manifests verified on both sides).

## 1. Scope and what changed

This run repeats the 2026-11-03 harness shape after the Phase C fixes:

- **#64 / ADR-0012 attachment churn.** The retired-id Bloom filter saturated
  on a churned volume and then refused every fresh attach id with
  `IDEMPOTENCY_CONFLICT`. The strictly increasing per-volume
  `attachment_generation` is now the detach/report fence; the filter is
  legacy decode-only. The pre-fix conflict storm is preserved as a
  labelled [invalid-run artifact](../2026-11-04-devhost-invalid-prefix/README.md).
- **#64 journal compaction.** Compaction failures now back off
  exponentially instead of being retried on every append, and tombstone
  membership is O(1) instead of a linear scan. The journal must stay bounded
  under sustained writes.
- **#62 state substrate.** The provider state file should stay small under
  attachment churn instead of growing with the volume's lifetime attach
  count.

The benchmark can show **behaviour and resource shape**, not correctness
under crash. Correctness evidence is the test suite and conformance checks at
`aab773b`, not this run.

## 2. What was executed

| Scenario | Shape | Result | Raw record |
|---|---|---|---|
| Micro | concurrency 1: create/delete, attach→report→detach, grow, inspect, list, capabilities; state-file scaling at 1/8/32 live volumes | 0 errors in 1,224 recorded ops | [micro.json](json/micro.json), [micro.csv](summary/micro.csv) |
| Mixed sweep | 1 / 8 / 32 / 128 in-flight, 70 % read-ratio target, 30 s per tier | 0 errors; throughput falls above 8 in-flight | [mixed-c1](json/mixed-c1.json), [c8](json/mixed-c8.json), [c32](json/mixed-c32.json), [c128](json/mixed-c128.json) |
| Admission cap probe | 128 in-flight, cap 32, 1 s queue timeout, 15 s (diagnostic) | queued, no `503`; see §6 | [mixed-cap32-c128.json](json/mixed-cap32-c128.json) |
| Rate-limit shedding probe | 64 in-flight, 50 req/s burst 50, 15 s (diagnostic) | 952 × `429 RATE_LIMITED`; 51.255 accepted ops/s | [mixed-rate50-c64.json](json/mixed-rate50-c64.json) |
| Shortened soak | 25 min, 4 workers, continuous attach/report/detach/grow, restart at 12 min, per-second resource samples | completed; state bounded, journal bounded, RSS high-water filed | [soak.json](json/soak.json) (per-second timeline not committed) |
| Restart replay | 3 pre-soak restarts, 1 mid-soak restart, 2 post-soak restarts | healthy in 0.102 s – 0.391 s; mid-soak replay listed 4/4 volumes | [restart-pre-soak.json](json/restart-pre-soak.json), [restart-post-soak.json](json/restart-post-soak.json) |

Every acknowledged mutation used the daemon's real journal append + fsync
and provider-state rewrite + fsync. There are no mocks on the write path.

## 3. Command

```bash
CARGO_BUILD_JOBS=4 cargo build --release -p volvisord --locked
sudo scripts/bench/run-all.sh \
  --run-dir /tmp/opencode/volvisor-bench-phasec-final-20261104 \
  --skip-build --sweep-seconds 30 --soak-minutes 25
```

`--skip-build` is deliberate: it makes the executed binary the one built from
the recorded clean tree rather than a binary rebuilt inside the harness.

## 4. Capacity table: 1/8/32/128 mixed in-flight

Exact values from [summary/concurrency.csv](summary/concurrency.csv). Product
default admission cap is 256 for the four tiers.

| tier | in-flight | ops | ops/s | error rate | p50 ms | p95 ms | p99 ms | read p95 ms | write p95 ms | 503 | 429 | RSS max KiB | fds max | state max B |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| c1 | 1 | 58,565 | 1952.155 | 0.0 | 0.146 | 1.357 | 1.474 | 0.175 | 1.445 | 0 | 0 | 56,456 | 15 | 1,036 |
| c8 | 8 | 104,816 | 3493.445 | 0.0 | 0.535 | 5.928 | 6.927 | 1.254 | 6.635 | 0 | 0 | 135,284 | 23 | 6,725 |
| c32 | 32 | 25,900 | 862.937 | 0.0 | 5.624 | 200.594 | 529.079 | 179.332 | 388.84 | 0 | 0 | 159,772 | 47 | 24,864 |
| c128 | 128 | 7,333 | 235.502 | 0.0 | 462.278 | 1349.487 | 2117.379 | 1288.505 | 1552.261 | 0 | 0 | 191,244 | 143 | 97,270 |

All 196,614 recorded requests returned 2xx; no `503`, `429` or network error
occurred in the default-cap sweep. Durations were 30.0 s, 30.004 s, 30.014 s
and 31.138 s; the c128 timeline records 126 ops in the final second, consistent
with in-flight drain rather than additional load, though the bundle does not
separate join/teardown from load.

The `state max B` column is the largest state file observed during the tier.
It is not a clean function of live volumes, because each mixed worker owns one
volume and continuously attaches/detaches. The important comparison is with
2026-11-03, where c1 reached **154,820 B** with one live volume; here c1
reaches **1,036 B**, and even c128 stays below **98 KiB**.

Derived observations:

- 8 in-flight is 1.79× the 1-in-flight throughput. 32 in-flight is 0.247× the
  8-in-flight throughput, and 128 in-flight is 0.273× the 32-in-flight
  throughput. End-to-end, c128 is 0.121× c1.
- p95 latency grows from 1.357 ms (c1) to 5.928 ms (c8), 200.594 ms (c32) and
  1349.487 ms (c128). The c128 p95 is 994.5× the c1 p95.
- Read p95 and write p95 converge at c32/c128: reads do not stay fast while
  writes are in flight. This is consistent with a single provider-state lock
  and whole-state fsync serializing both classes, as in the 2026-11-03 run.
- Throughput **falls** above 8 in-flight. This is a control-plane ceiling, not
  a measured CPU ceiling: 16 vCPUs were available and no CPU-saturation
  metric was sampled.

## 5. Micro (concurrency 1)

Exact values from [summary/micro.csv](summary/micro.csv).

| operation | count | errors | min ms | mean ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| create | 72 | 0 | 134.674 | 158.379 | 155.068 | 175.382 | 197.115 | 197.115 |
| delete | 72 | 0 | 93.964 | 115.215 | 114.235 | 124.622 | 134.705 | 134.705 |
| attach | 150 | 0 | 1.197 | 1.340 | 1.321 | 1.490 | 1.561 | 1.588 |
| report | 150 | 0 | 1.186 | 1.341 | 1.320 | 1.536 | 1.661 | 1.762 |
| detach | 150 | 0 | 1.186 | 1.348 | 1.322 | 1.532 | 1.744 | 1.838 |
| grow | 30 | 0 | 144.577 | 173.763 | 174.752 | 194.974 | 194.983 | 194.983 |
| inspect | 400 | 0 | 0.098 | 0.114 | 0.112 | 0.134 | 0.188 | 0.268 |
| list | 100 | 0 | 0.102 | 0.118 | 0.116 | 0.136 | 0.209 | 0.223 |
| capabilities | 100 | 0 | 0.103 | 0.112 | 0.110 | 0.126 | 0.134 | 0.134 |

LVM command cost dominates create/delete/grow; attach/report/detach are state
mutations at roughly 1.3 ms p50; inspect/list/capabilities are sub-millisecond
p95 at this dataset. These are unchanged in shape from 2026-11-03.

## 6. Diagnostic probes

Exact values from [summary/concurrency.csv](summary/concurrency.csv).

| probe | in-flight | cap | queue timeout | rate limit | duration | ops | ops/s | accepted ops/s | error rate | p95 ms | 503 | 429 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| cap32-c128 | 128 | 32 | 1 s | 0 | 15.063 s | 3,417 | 226.852 | 226.852 | 0.0 | 1313.393 | 0 | 0 |
| rate50-c64 | 64 | 256 | 5 s | 50/s | 15.745 s | 1,759 | 111.72 | 51.255 | 0.541217 | 192.768 | 0 | 952 |

- **Cap 32, 128 in-flight:** no `503` and no error. The absence of `503`
  means no request waited out the 1 s queue timeout; server-side queue wait
  itself was not sampled. This probe does not claim cap 32 sheds load.
- **Rate limit 50/s:** 952 `429 RATE_LIMITED`, accepted throughput 51.255
  ops/s, all-attempt error rate 0.541217. This is real admission shedding.
  The client backed off and reconnected after each `429`; the harness
  behavior is not a performance fix.

## 7. Shortened soak (25 minutes, 4 workers)

Exact values from [summary/soak.csv](summary/soak.csv) and
[json/soak.json](json/soak.json).

| metric | value |
|---|---:|
| duration | 1500.003 s (25.000 min) |
| workers | 4 |
| total ops | 5,424,744 |
| throughput | 3616.49 ops/s |
| successful throughput | 3616.443 ops/s |
| accepted ops | 5,424,674 |
| error rate | 1.3 × 10⁻⁵ |
| errors | 70, all `NETWORK_ERROR` at the deliberate restart (66 `ConnectionRefusedError`, 4 `RemoteDisconnected`) |
| HTTP errors / conflicts | **0** |
| p50 / p95 / p99 | 0.437 / 3.410 / 4.669 ms |
| attach / report / detach | 361,726 / 361,725 / 361,723; the only error is 1 report request caught by the restart `NETWORK_ERROR` |
| grow | 512, 0 errors |
| inspect / list / capabilities | 2,712,406 / 1,300,198 / 326,454; 68 and 1 restart `NETWORK_ERROR` respectively, rest 0 |
| restarts | 1 (at 12 min) |
| mid-soak healthy time | 0.552 s |
| mid-soak replay | 4/4 volumes listed, state 3,385 B, journal 42,744,729 B |

The 70 network errors are the client observing the daemon stop/start at the
planned restart. They are not provider or admission errors; the soak status is
`OK` and `status_counts` is `{200: 5,424,674, 0: 70}`.

### Resources

| metric | first | last | delta | min | max |
|---|---:|---:|---:|---:|---:|
| RSS KiB | 169,152 | 1,950,048 | +1,780,896 | 135,472 | **2,323,028** |
| journal bytes | 58,483,131 | 27,982,748 | −30,500,383 | 8,388,384 | **75,497,833** |
| state bytes | 3,228 | 3,055 | −173 | 2,529 | 3,675 |
| fds | 19 | 17 | −2 | 13 | 21 |

The journal stayed bounded: it never exceeded **75.5 MB (72.0 MiB)** and ended
at **28.0 MB (26.7 MiB)**, versus 271.4 MB (258.8 MiB) before. The effective
(default) `max_log_bytes` is 64 MiB (the benchmark config sets no journal
section); after the first compaction the trigger additionally requires 64 MiB
of growth since the last compaction, so the log can overshoot the bound by up
to that floor between rewrites. The observed 75.5 MB maximum is consistent
with that hysteresis. The state file never exceeded **3,675 B** and ended at
**3,055 B**, versus 2.52 MB (2.41 MiB) before.

**RSS is the open residual.** It rose to **2,323,028 KiB (≈2.22 GiB)** and did
not fall back to its start value. The peak is not monotonic and coincides with
a post-compaction journal minimum: at elapsed 1184.355 s RSS was
2,323,028 KiB while the journal was 8,388,384 B, its minimum for the run and
consistent with a just-completed compaction. At the journal maximum
(75,497,833 B, elapsed 863.986 s) RSS was 830,572 KiB. That is consistent
with allocator/compaction transients, but a leak is neither proven nor ruled
out. No RSS cap, fix or "leak" claim is made.

### State-file size versus live volumes

Exact values from [summary/state-scale.csv](summary/state-scale.csv):

| live volumes | state B | journal B | last create ms |
|---:|---:|---:|---:|
| 1 | 1,973 | 507,612 | 154.647 |
| 8 | 6,138 | 515,525 | 154.815 |
| 32 | 20,484 | 542,714 | 175.14 |

State grows with live volume count but not with attachment history. The
2026-11-03 bundle measured 6,486 / 11,708 / 29,678 B for the same 1/8/32
shape; the reduction is the retired-id tombstone/vector no longer being
written.

### Daemon log (operational finding)

The default daemon log at INFO level is **unbounded**. It covers the whole
~30-minute run (micro, sweeps, probes, soak and restarts), not only the
25-minute soak:

| metric | value |
|---|---:|
| uncompressed bytes | 2,572,981,477 (≈2.4 GiB) |
| lines | 9,072,853 |
| INFO lines | 9,072,376 |
| WARN lines | 477 |
| compressed bundle copy | 77,732,175 B |

All 477 WARN lines are `grow notification outstanding; retried next pass`.
There is no rotation or size cap. This is an operational finding, not a
durable-state bug, and no logging change is claimed here. The bundle stores
the log as `raw/daemon.daemon.log.gz` because the plaintext file exceeds forge
limits; `gunzip` reproduces the run log and no measurement file was edited.

## 8. Restart replay

Exact values from [summary/restart.csv](summary/restart.csv).

| scenario | journal before | state before | stop s | healthy s | state after | journal after | listed | missing |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| restart-pre-soak #1 | 563,927 | 198 | 0.020 | 0.103 | 198 | 563,927 | 0 | — |
| restart-pre-soak #2 | 563,927 | 198 | 0.003 | 0.102 | 198 | 563,927 | 0 | — |
| restart-pre-soak #3 | 563,927 | 198 | 0.003 | 0.102 | 198 | 563,927 | 0 | — |
| soak-restart | 42,744,729 | 3,385 | 0.285 | **0.552** | 3,385 | 42,744,729 | **4** | — |
| restart-post-soak #1 | 28,147,713 | 198 | 0.241 | 0.391 | 198 | 28,147,713 | 0 | — |
| restart-post-soak #2 | 28,147,713 | 198 | 0.114 | 0.367 | 198 | 28,147,713 | 0 | — |

Every restart preserved the journal and state byte-for-byte (post-restart
sizes equal pre-restart sizes) and the mid-soak restart recovered all four
live volumes. Recovery time here is readiness of the HTTP listener plus a
successful list, not a data-path recovery objective.

## 9. Before/after comparison

Generated by `scripts/bench/compare.py`; full tables in
[comparison/comparison.md](comparison/comparison.md). Both manifests were
verified and both runs are clean-tree.

| metric | 2026-11-03 | 2026-11-04 post-fix | delta |
|---|---:|---:|---:|
| soak ops | 1,202,500 | 5,424,744 | +351.1 % |
| c1 ops/s | 1665.787 | 1952.155 | +17.2 % |
| c8 ops/s | 2293.7 | 3493.445 | +52.3 % |
| c32 ops/s | 899.759 | 862.937 | −4.1 % |
| c128 ops/s | 241.716 | 235.502 | −2.6 % |
| soak journal end | 271,399,822 B | 27,982,748 B | −89.7 % |
| soak state end | 2,522,793 B | 3,055 B | −99.88 % |
| soak RSS delta | +621,716 KiB | +1,780,896 KiB | +186.4 % |

Both soaks recorded **0 attach errors and 0 attachment conflicts**; the
178/70 network errors are client reconnects at the deliberate restart. The
operation mix is nearly identical (both ≈50 % inspect, ≈24 % list, ≈6.7 % each
attach/report/detach), so the +351.1 % operation delta is a throughput/latency
change, not a different acceptance mix: the pre-fix soak ran at 801.656 ops/s
with p95 16.804 ms, the post-fix soak at 3616.49 ops/s (4.51× the ops/s) with
p95 3.410 ms, consistent with the pre-fix run rewriting a growing 2.5 MB
provider-state file per mutation. The comparison tool reports raw counts and
per-minute normalisation; it does not apply a fixed-dataset correction. The
sweep rows are the cleaner comparison, and their shape is unchanged: c8
improves, c32/c128 regress slightly, high in-flight latency remains dominated
by serialization.

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
- **RSS high-water is unresolved.** See §7. Do not present it as a leak or as
  fixed.
- **The INFO log is unbounded.** See §7. No rotation is implemented.
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
| [raw/daemon.daemon.log.gz](raw/daemon.daemon.log.gz) | gzip daemon log (2.4 GiB raw) |
| [records/](records/) | lab loop/VG identity and pre/post VG listings |
| [configs/](configs/) | sanitized benchmark configs (tokens replaced) |

The per-operation CSV samples and per-second timelines are not committed; the
JSON aggregates and summary tables retain the measured values.
