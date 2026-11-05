# Control-plane benchmark report — 2026-11-05 D3 slice-2 grow-lock fix, single-host development lab

**Status:** single-host development-lab measurement of the Phase D D3 slice-2
write-tail fix. Not production evidence, not an SLA, not a data-path
benchmark, and not the issue's 24-hour soak. Read the caveats in §11 before
using any number.

- **Exact source:** `8029df8e8f6e1856499927df6f434422894005ad`
  (`volvisord 8029df8`), **worktree clean** at run time
  (`worktree_dirty: no`).
- **Run window:** 2026-11-05T09:29:39Z .. 2026-11-05T10:00:20Z. The release
  binary was built before the run; the run used `--skip-build`.
- **Host:** one development host (`dev`), Ubuntu 24.04.5 LTS, kernel
  `6.8.0-142-generic`, 16 vCPUs on an AMD EPYC 9554P, 31.3 GiB RAM, QEMU
  virtual disks. Daemon and all clients on `127.0.0.1`.
- **Binary:** `target/release/volvisord`, SHA256
  `18f260c2ef878985f23fdcc15a659d36db87ff7209477756e98638ff0e28b941`,
  built with `CARGO_BUILD_JOBS=4 cargo build --release -p volvisord --locked`.
- **Provenance:** [SCRIPTS.txt](SCRIPTS.txt).
- **Comparator:** [comparison/comparison.md](comparison/comparison.md),
  generated against
  [2026-11-04-devhost-slice2](../2026-11-04-devhost-slice2/REPORT.md)
  (`c4b0b13`, the run that recorded the regression) with
  `scripts/bench/compare.py`; both manifests verified. The older clean-tree
  pre-regression baseline is
  [2026-11-04-devhost-postfix](../2026-11-04-devhost-postfix/REPORT.md)
  (`aab773b`) and is cited separately below.

## 1. Scope and what changed

The D3 slice-2 run measured a high-in-flight **write**-latency regression:
write p95 at c32 rose from 388.84 ms to 590.847 ms, and at c128 from
1552.261 ms to 6330.548 ms, while throughput stayed flat. This run measures
the fix for that tail.

The change is confined to the LVM provider grow path:

- `grow_volume` no longer holds the provider state mutex across the long
  `lvextend` and size-verification commands. It now **prepares** under the
  lock (capacity check, in-memory capacity reservation, durable grow intent),
  runs `lvextend` with the lock released, verifies the observed size without
  the lock, and **commits** by reacquiring the lock and re-validating the
  exact durable intent and volume generation.
- The durable grow intent remains the crash-recovery anchor. While it is
  live, fresh attach, detach, delete, move start and move completion are
  refused typed; **reports stay admitted** because they never move the
  volume generation or size.
- Because the coarse state lock also made capacity-check-plus-allocation
  atomic, an in-memory per-VG reservation now prevents a concurrent create
  or another grow from spending the same free extents during the
  released-lock window.
- A second concurrent grow of the same volume is refused by a non-blocking
  drive set; a non-zero `lvextend` exit clears the intent only when `lvs`
  proves the LV is still at its from-size.
- The move reconcile pass defers a verified completion for the one volume
  whose grow is live (recording `deferred_while_growing`) and continues
  classifying the other records, instead of aborting the whole pass.

Everything else in the write path is unchanged from `c4b0b13`: attach,
detach and an applied report still release the state lock after capturing the
mutation; create, delete, move and admin still hold it across their
multi-save/compensation paths. Correctness evidence for the fix is the test
suite at `8029df8`, not this run. The harness can show behaviour and resource
shape, not crash consistency or fencing under partition.

## 2. Diagnosis that preceded the fix

The slice-2 report recorded the write tail but did not attribute it. Before
changing code, a diagnostic run disabled grows in the mixed workers and kept
every other operation. That run measured:

- c128 write p95 **6.33 s → 0.57 s**, throughput **237 → 2,522 ops/s**;
- c32 write p95 **590 ms → 85 ms**.

The only removed work was the grow path, and the grow path's serializer was
the provider state mutex held across `lvextend` (≈200 ms median in the micro
run). That diagnostic identified the cause; it is **not** published bundle
evidence and no number from it is used in the tables below.

This run is the independent before/after confirmation with all operations
enabled: the same `c4b0b13` harness shape, measured at the fix SHA.

## 3. What was executed

| Scenario | Shape | Result | Raw record |
|---|---|---|---|
| Micro | concurrency 1: create/delete, attach→report→detach, grow, inspect, list, capabilities; state-file scaling at 1/8/32 live volumes | 0 errors in 1,224 recorded ops | [micro.json](json/micro.json), [micro.csv](summary/micro.csv) |
| Mixed sweep | 1 / 8 / 32 / 128 in-flight, 70 % read-ratio target, 30 s per tier | 0 errors in 213,728 requests; c32/c128 write tail reduced | [mixed-c1](json/mixed-c1.json), [c8](json/mixed-c8.json), [c32](json/mixed-c32.json), [c128](json/mixed-c128.json) |
| Admission cap probe | 128 in-flight, cap 32, 1 s queue timeout, 15 s (diagnostic) | 16 × `503`; shedding visible | [mixed-cap32-c128.json](json/mixed-cap32-c128.json) |
| Rate-limit shedding probe | 64 in-flight, 50 req/s burst 50, 15 s (diagnostic) | 960 × `429 RATE_LIMITED`; 50.869 accepted ops/s | [mixed-rate50-c64.json](json/mixed-rate50-c64.json) |
| Shortened soak | 25 min, 4 workers, continuous inspect/attach/report/detach/grow, restart at 12 min, per-second resource samples | completed; state bounded, journal bounded, RSS high-water filed | [soak.json](json/soak.json), [soak-timeline.csv](csv/soak-timeline.csv) |
| Restart replay | 3 pre-soak restarts, 1 mid-soak restart, 2 post-soak restarts | healthy in 0.082 s – 0.264 s; mid-soak replay listed 4/4 volumes | [restart-pre-soak.json](json/restart-pre-soak.json), [restart-post-soak.json](json/restart-post-soak.json) |

Every acknowledged mutation used the daemon's real journal append + fsync
and provider-state rewrite + fsync. There are no mocks on the write path.
The mixed workers exercised real grows at every tier (4 / 32 / 128 / 255
grow calls at c1 / c8 / c32 / c128), so the changed path was live during the
sweep.

## 4. Command

```bash
CARGO_BUILD_JOBS=4 cargo build --release -p volvisord --locked
sudo scripts/bench/run-all.sh \
  --run-dir /tmp/opencode/volvisor-bench-fix-8029df8-20261105T092939Z \
  --skip-build --sweep-seconds 30 --soak-minutes 25
```

`--skip-build` is deliberate: the executed binary is the one built from the
recorded clean tree, not a binary rebuilt inside the harness.

## 5. Capacity table: 1/8/32/128 mixed in-flight

Exact values from [summary/concurrency.csv](summary/concurrency.csv). The
product default admission cap is 256 for the four tiers.

| tier | in-flight | ops | ops/s | error rate | p50 ms | p95 ms | p99 ms | read p95 ms | write p95 ms | 503 | 429 | RSS max KiB | fds max | state max B |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| c1 | 1 | 51,875 | 1729.154 | 0.0 | 0.162 | 1.542 | 1.713 | 0.199 | 1.668 | 0 | 0 | 51,672 | 15 | 1,034 |
| c8 | 8 | 93,579 | 3118.787 | 0.0 | 0.411 | 9.125 | 19.084 | 1.007 | 15.597 | 0 | 0 | 122,844 | 27 | 6,514 |
| c32 | 32 | 54,957 | 1830.005 | 0.0 | 0.522 | 88.969 | 284.504 | 1.481 | 206.203 | 0 | 0 | 175,404 | 51 | 25,022 |
| c128 | 128 | 13,317 | 440.376 | 0.0 | 1.123 | 1836.718 | 3889.606 | 87.276 | 3413.886 | 0 | 0 | 201,084 | 147 | 96,372 |

All 213,728 recorded sweep requests returned 2xx; no `503`, `429` or network
error occurred in the default-cap sweep. Durations were 30.000 s, 30.005 s,
30.031 s and 30.240 s.

Derived observations:

- **The grow-lock serialization is gone.** Against the regression run
  (`c4b0b13`, §10), c32 write p95 fell **590.847 → 206.203 ms** (−65.1 %)
  and c128 write p95 fell **6330.548 → 3413.886 ms** (−46.1 %). Throughput
  rose **858.371 → 1830.005 ops/s** at c32 (+113.2 %) and
  **237.088 → 440.376 ops/s** at c128 (+85.7 %). The c32 write tail is now
  below the pre-regression baseline (388.84 ms); the c128 write tail is
  reduced but **still above** that baseline (1552.261 ms). See §10.
- **The read/write split is unchanged in direction.** Reads were already
  snapshot reads and remain fast: read p95 at c32 is 1.481 ms (1.188 ms at
  `c4b0b13`) and at c128 is 87.276 ms (197.815 ms at `c4b0b13`).
- **c1/c8 throughput fell.** c1 is −6.3 % and c8 −8.6 % against `c4b0b13`
  (−11.4 % and −10.7 % against the older `aab773b` baseline). The changed
  grow path is exercised only 4 / 32 times at these tiers, and micro grow p95
  is essentially unchanged (195.535 → 195.571 ms), but this harness does not
  attribute run-to-run drift; the drop is recorded, not explained.
- 8 in-flight is 1.80× the 1-in-flight throughput; 32 is 0.587× the
  8-in-flight throughput, and 128 is 0.241× the 32-in-flight throughput.
- Throughput still falls above 8 in-flight. This is a control-plane
  ceiling, not a measured CPU ceiling: 16 vCPUs were available and no
  CPU-saturation metric was sampled.

The `state max B` column is the largest state file observed during the tier.
Even c128 stays below **96 KiB**.

## 6. Micro (concurrency 1)

Exact values from [summary/micro.csv](summary/micro.csv).

| operation | count | errors | min ms | mean ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| create | 72 | 0 | 134.82 | 163.203 | 165.146 | 180.017 | 200.925 | 200.925 |
| delete | 72 | 0 | 93.905 | 120.297 | 115.245 | 135.207 | 155.041 | 155.041 |
| attach | 150 | 0 | 1.299 | 1.511 | 1.467 | 1.805 | 1.849 | 2.012 |
| report | 150 | 0 | 1.291 | 1.493 | 1.448 | 1.752 | 1.964 | 2.006 |
| detach | 150 | 0 | 1.297 | 1.501 | 1.491 | 1.756 | 1.869 | 2.01 |
| grow | 30 | 0 | 164.77 | 181.041 | 184.937 | 195.571 | 207.017 | 207.017 |
| inspect | 400 | 0 | 0.097 | 0.115 | 0.111 | 0.145 | 0.188 | 0.296 |
| list | 100 | 0 | 0.113 | 0.125 | 0.121 | 0.144 | 0.222 | 0.232 |
| capabilities | 100 | 0 | 0.105 | 0.119 | 0.117 | 0.137 | 0.184 | 0.262 |

LVM command cost still dominates create/delete/grow. Single grow latency is
unchanged from `c4b0b13` (p95 195.535 → 195.571 ms): the fix moves the state
lock out of the command window, it does not make `lvextend` faster.
Inspect/list/capabilities remain sub-millisecond p95 at this dataset.

## 7. Diagnostic probes

Exact values from [summary/concurrency.csv](summary/concurrency.csv).

| probe | in-flight | cap | queue timeout | rate limit | duration | ops | ops/s | accepted ops/s | error rate | p95 ms | 503 | 429 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| cap32-c128 | 128 | 32 | 1 s | 0 | 15.468 s | 6,821 | 440.966 | 439.932 | 0.002346 | 747.848 | 16 | 0 |
| rate50-c64 | 64 | 256 | 5 s | 50/s | 15.687 s | 1,758 | 112.064 | 50.869 | 0.546075 | 54.379 | 0 | 960 |

- **Cap 32, 128 in-flight:** 16 requests observed the 1 s queue timeout and
  returned `503` (77 in the regression run; 0 in the `aab773b` baseline).
  This is a diagnostic shape with a non-default cap, not a capacity result.
- **Rate limit 50/s:** 960 `429 RATE_LIMITED`, accepted throughput
  50.869 ops/s, all-attempt error rate 0.546075. This is real admission
  shedding; the client backs off and reconnects.

## 8. Shortened soak (25 minutes, 4 workers)

Exact values from [summary/soak.csv](summary/soak.csv) and
[json/soak.json](json/soak.json).

| metric | value |
|---|---:|
| duration | 1500.581 s (25.010 min) |
| workers | 4 |
| total ops | 6,640,950 |
| throughput | 4425.586 ops/s |
| successful throughput | 4425.555 ops/s |
| accepted ops | 6,640,904 |
| error rate | 7 × 10⁻⁶ |
| errors | 46, all `NETWORK_ERROR` at the deliberate restart (43 `ConnectionRefusedError`, 3 `RemoteDisconnected`) |
| HTTP errors / conflicts | **0** |
| p50 / p95 / p99 | 0.307 / 3.0 / 4.038 ms |
| read p50 / p95 | 0.268 / 0.685 ms |
| write p50 / p95 | 2.516 / 4.011 ms |
| attach / report / detach | 442,721 / 442,719 / 442,717; the only attach error is 1 request caught by the restart `NETWORK_ERROR` |
| grow | 512, 0 errors; p50 257.356 ms, p95 373.744 ms |
| inspect / list / capabilities | 3,320,497 / 1,592,035 / 399,749; 44 + 1 restart `NETWORK_ERROR`, the rest 0 |
| restarts | 1 (at 12 min) |
| mid-soak healthy time | 0.264 s |
| mid-soak replay | 4/4 volumes listed, state 2,718 B, journal 12,941,814 B |

The 46 network errors are the client observing the daemon stop/start at the
planned restart. They are not provider or admission errors; the soak status
is `OK` and `status_counts` is `{200: 6,640,904, 0: 46}`.

The recorded read ratio is 0.7999 of all operations (target 0.6), the same
actual ratio as the prior runs. Operation counts scale with total ops; the
mix is therefore comparable, but this is still one run on a non-fixed
dataset.

### Resources

| metric | first | last | delta | min | max |
|---|---:|---:|---:|---:|---:|
| RSS KiB | 60,628 | 2,186,760 | +2,126,132 | 60,628 | **2,538,456** |
| journal bytes | 9,019,147 | 75,496,834 | +66,477,687 | 7,818,542 | **75,497,693** |
| state bytes | 3,523 | 3,147 | −376 | 2,537 | 3,950 |
| fds | 21 | 17 | −4 | 17 | 25 |

The journal stayed bounded: it never exceeded **75,497,693 B (72.0 MiB)** and
ended at **75,496,834 B (72.0 MiB)**. The effective (default)
`max_log_bytes` is 64 MiB; after the first compaction the trigger
additionally requires 64 MiB of growth since the last compaction, so the log
can overshoot the bound by up to that floor between rewrites. The observed
maximum is consistent with that hysteresis. The state file never exceeded
**3,950 B** and ended at 3,147 B; it does not grow with attachment history.

**RSS remains elevated.** RSS reached a peak of **2,538,456 KiB (≈2.42 GiB)**
and ended at 2,186,760 KiB. That is below the regression run's peak of
3,236,184 KiB, but above the 2,323,028 KiB peak at `aab773b`. The harness
does not attribute RSS growth; a leak is neither proven nor ruled out, and
no RSS cap or fix is claimed here.

### Daemon log (operational finding)

The default daemon log at INFO level is still **unbounded**:

| metric | value |
|---|---:|
| uncompressed bytes | 2,223,082,241 (≈2.07 GiB) |
| lines | 8,263,668 |
| INFO lines | 8,263,085 |
| WARN lines | 583 |
| ERROR lines | 0 |
| compressed bundle copy | 58,555,280 B |

All 583 WARN lines are `grow notification outstanding; retried next pass`
(the expected retry for an attached volume whose guest notification is not
yet confirmed). There is no rotation or size cap. This is an operational
finding, not a durable-state bug, and no logging change is claimed here. The
bundle stores the log as `raw/daemon.daemon.log.gz` because the plaintext
file exceeds forge limits; `gunzip` reproduces the run log and no measurement
file was edited.

## 9. Restart replay

Exact values from [summary/restart.csv](summary/restart.csv).

| scenario | journal before | state before | stop s | healthy s | state after | journal after | listed | missing |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| restart-pre-soak #1 | 563,927 | 198 | 0.020 | 0.103 | 198 | 563,927 | 0 | — |
| restart-pre-soak #2 | 563,927 | 198 | 0.003 | 0.102 | 198 | 563,927 | 0 | — |
| restart-pre-soak #3 | 563,927 | 198 | 0.003 | 0.082 | 198 | 563,927 | 0 | — |
| soak-restart | 12,941,814 | 2,718 | 0.284 | **0.264** | 2,718 | 12,941,814 | **4** | — |
| restart-post-soak #1 | 8,397,224 | 198 | 0.201 | 0.205 | 198 | 8,397,224 | 0 | — |
| restart-post-soak #2 | 8,397,224 | 198 | 0.031 | 0.203 | 198 | 8,397,224 | 0 | — |

Every restart preserved the journal and state byte-for-byte (post-restart
sizes equal pre-restart sizes) and the mid-soak restart recovered all four
live volumes. Recovery time here is readiness of the HTTP listener plus a
successful list, not a data-path recovery objective.

## 10. Before/after comparison

Generated by `scripts/bench/compare.py`; full tables in
[comparison/comparison.md](comparison/comparison.md). Both manifests were
verified and both runs are clean-tree. The primary baseline is the regression
run `c4b0b1303594c494bb26d3643d952434eda8ec34` (2026-11-04 D3 slice 2).
The pre-regression baseline `aab773b7b58448e41ba22b21372785b3c58e7381`
(2026-11-04 post-fix) is shown for context only; its comparator was generated
outside this bundle but reads the same two verified manifests.

| metric | slice 2 (`c4b0b13`) | fix (`8029df8`) | delta | post-fix (`aab773b`) | fix vs `aab773b` |
|---|---:|---:|---:|---:|---:|
| c1 ops/s | 1845.526 | 1729.154 | −6.3 % | 1952.155 | −11.4 % |
| c8 ops/s | 3410.746 | 3118.787 | −8.6 % | 3493.445 | −10.7 % |
| c32 ops/s | 858.371 | 1830.005 | **+113.2 %** | 862.937 | +112.1 % |
| c128 ops/s | 237.088 | 440.376 | **+85.7 %** | 235.502 | +87.0 % |
| c32 read p95 | 1.188 ms | 1.481 ms | +24.7 % | 179.332 ms | −99.2 % |
| c128 read p95 | 197.815 ms | 87.276 ms | −55.9 % | 1288.505 ms | −93.2 % |
| c32 write p95 | 590.847 ms | 206.203 ms | **−65.1 %** | 388.84 ms | −47.0 % |
| c128 write p95 | 6330.548 ms | 3413.886 ms | **−46.1 %** | 1552.261 ms | +119.9 % |
| soak ops/s | 4619.824 | 4425.586 | −4.2 % | 3616.49 | +22.4 % |
| soak p95 | 2.845 ms | 3.0 ms | +5.4 % | 3.410 ms | −12.0 % |
| soak journal end | 43,412,309 B | 75,496,834 B | +73.9 % | 27,982,748 B | +169.8 % |
| soak state end | 3,401 B | 3,147 B | −7.5 % | 3,055 B | +3.0 % |
| soak RSS delta | +2,943,816 KiB | +2,126,132 KiB | −27.8 % | +1,780,896 KiB | +19.4 % |

All soaks recorded **0 attach conflicts and 0 HTTP errors**; the 46 (this
run) / 77 (`c4b0b13`) / 70 (`aab773b`) network errors are client reconnects
at the deliberate restart. The operation mix is comparable (all ≈80 % reads
by the recorded actual ratio).

**What the comparison supports.** Removing the state lock from the
`lvextend` window is followed by a large c32/c128 throughput recovery and a
large reduction of the c32/c128 write tail. At c32 the write tail is now
better than the pre-regression baseline; at c128 it is 46 % better than the
regression run but still about 2.2× the older baseline. The harness does not
attribute the remaining c128 tail (candidate contributors include the grow
intent's own fsyncs at prepare/commit, the synchronous writer funnel, or the
harness mix), so no claim is made that the write tail is fully eliminated.

**What it does not support.** This is not a general efficiency or capacity
win: c1 and c8 throughput fell in this run, the soak rate fell 4.2 % versus
the regression run, the c128 write tail remains above the old baseline, and
the soak journal and RSS are single-run observations.

## 11. Caveats and NOT RUN

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
  the test suites and the support matrix. The grow-lock fix's correctness
  evidence is the `8029df8` test suite, including the four new regression
  tests (lock release, report admission, capacity reservation, and the
  two-volume move-completion deferral).
- **No blanket throughput-gain claim.** The c32/c128 recovery is measured
  against the run that recorded the regression; c1/c8 moved down, and the
  c128 write tail is still above the pre-regression baseline.
- **RSS high-water is unresolved.** See §8. Do not present it as a leak or as
  fixed.
- **The INFO log is unbounded.** See §8. No rotation is implemented.
- **The cap-32 probe is diagnostic.** Its 16 `503` responses are the probe's
  configured 1 s queue timeout, not the default-cap sweep.
- **Accepted code residuals (not measured here).** A live grow intent whose
  LV is absent or observed mid-size needs a same-request retry, a restart, or
  an operator; the move reconcile pass defers a verified completion while
  such an intent persists (no cross-record starvation); move admission
  capacity still ignores grow reservations (pre-existing); create and delete
  still hold the state lock across `lvcreate`/`lvremove` (pre-existing); a
  post-poison alignment reload and the unbounded log remain. None of these is
  a production claim.
- **No production/SLA claim.** No number here is a capacity plan, support
  commitment, RPO/RTO statement, or release approval. Readiness gates are
  unchanged by this bundle.

## Appendix: files

| path | content |
|---|---|
| [ENVIRONMENT.txt](ENVIRONMENT.txt) | host, kernel, toolchain, binary digest, absent capabilities |
| [comparison/comparison.md](comparison/comparison.md) | before/after tables against the slice-2 regression run |
| [summary/TABLES.md](summary/TABLES.md) | all generated tables in one file |
| [summary/concurrency.csv](summary/concurrency.csv) | exact mixed/probe aggregates |
| [summary/soak.csv](summary/soak.csv) | exact soak aggregate |
| [summary/journal-state.csv](summary/journal-state.csv) | micro journal/state measurements |
| [summary/state-scale.csv](summary/state-scale.csv) | state size versus live volumes |
| [summary/restart.csv](summary/restart.csv) | restart replay rows |
| [json/](json/) | exact aggregate JSON per scenario |
| [csv/](csv/) | per-second timelines and deterministic per-op samples |
| [raw/00-orchestration.log](raw/00-orchestration.log) | unmodified orchestration trace |
| [raw/daemon.daemon.log.gz](raw/daemon.daemon.log.gz) | gzip daemon log (2.07 GiB raw) |
| [records/](records/) | lab loop/VG identity and pre/post VG listings |
| [configs/](configs/) | sanitized benchmark configs (tokens replaced) |
