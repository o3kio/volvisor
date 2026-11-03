# Control-plane benchmark report — 2026-11-03, single-host development lab

**Status:** single-host development-lab measurement. Not production evidence,
not an SLA, not a data-path benchmark, and not the issue's 24-hour soak. Read
the caveats in §9 before using any number.

- **Exact source:** `e3aa365ea65b865c8310a42ea6b459b038f0ab24`
  (`volvisord e3aa365`), **worktree clean** at run time
  (`worktree_dirty: no`).
- **Run window:** 2026-11-03T12:53:38Z .. 2026-11-03T13:23:35Z. The outer
  command started at 2026-11-03T12:52:50Z and included the release build.
- **Host:** one development host (`dev`), Ubuntu 24.04.5 LTS, kernel
  `6.8.0-142-generic`, 16 vCPUs on an AMD EPYC 9554P, 31.3 GiB RAM, QEMU
  virtual disks. Daemon and all clients on `127.0.0.1`.
- **Binary:** `target/release/volvisord`, SHA256
  `670c21259a7474fba0450f779f61967401871fbc2cb47d21da9ab40b0db8a58b`,
  built with `cargo build --release --locked -p volvisord` and
  `CARGO_BUILD_JOBS=4`.
- **Environment:** [ENVIRONMENT.txt](ENVIRONMENT.txt). `drbdadm`, `rbd`,
  `ceph`, `fio`, a second physical host and physical-disk LVM were absent.
- **Evidence:** [README.md](README.md), [summary/TABLES.md](summary/TABLES.md),
  [json/](json/), [csv/](csv/), [raw/](raw/),
  [MANIFEST.sha256](MANIFEST.sha256).

## 1. Question

Issue #43 asks for a reproducible control-plane benchmark of the real
`volvisord` HTTP Volume API v2: create/delete, attach/detach, report, grow,
inspect, journal append, state save and restart replay; a 1/8/32/128
in-flight mixed read/write sweep; and a soak with continuous operations and
a restart. The measured question here is narrower than a production
capacity claim:

> What does one `volvisord` on one host, backed by a loop-backed LVM VG,
> actually do to control-plane latency, throughput, memory, file descriptors,
> journal size and state size as in-flight concurrency and run time grow?

## 2. Method

- **Lab.** `scripts/bench/run-all.sh` creates a disposable 80 GiB
  loop-backed LVM VG (`records/lab.env`, `records/main.loop`) and tears down
  only that VG. The LVM provider skips loop devices during discovery, so the
  run pre-seeds `lvm-state.json` with a lab-only operator device claim
  pointing at the loop device. This is a lab scaffold, not a production
  claim path.
- **Durability path is real.** Every acknowledged mutation goes through the
  daemon's journal append + `fsync` and the provider's state rewrite +
  `fsync`; there is no mock on the write path. Journal and state live under
  the run directory on the same ext4 filesystem as the loop image.
- **Micro (concurrency 1).** 40 create/delete pairs, 150 attach → report →
  detach cycles, 30 grows, 400 inspects, 100 lists, 100 capability reads,
  plus a state-file scaling pass at 1, 8 and 32 live volumes. The scaling
  pass measures end-to-end create latency and the state/journal file sizes;
  it does **not** isolate the state-save syscall.
- **Mixed sweep.** 1, 8, 32 and 128 in-flight clients for 30 s each, 70 %
  reads / 30 % writes by target ratio. Actual ratios are recorded in
  `summary/concurrency.csv`. Each worker owns one volume, so the live-volume
  set grows with the tier (1, 8, 32, 128 volumes); this is a deliberate
  load shape, not a fixed-dataset sweep.
- **Admission cap probe (diagnostic).** 128 in-flight, daemon
  `max_concurrent_requests = 32`, queue timeout 1 s, 15 s. This is not the
  product default cap (256); it tests the cap path.
- **Rate-limit shedding probe (diagnostic).** 64 in-flight, daemon
  `rate_limit_per_second = 50`, burst 50, 15 s. The client backs off and
  reconnects after a shed response; `success_ops_per_sec` counts only 2xx
  responses.
- **Shortened soak.** 25.0 minutes (1500.019 s), 4 workers, continuous
  attach/report/detach/grow, one restart at 12 minutes. Per-second samples
  of RSS, fds, journal bytes and state bytes are in
  [csv/soak-timeline.csv](csv/soak-timeline.csv). The soak target was 60 %
  reads, but each write is preceded by an inspect; the recorded actual read
  ratio was 0.7997.
- **Restart replay.** Three stop/start iterations before the soak, one
  stop/start during the soak, and two after the soak. Time-to-healthy is
  measured until `/healthz` returns 200; the mid-soak restart also lists
  the volumes visible after replay.
- **Percentiles.** Nearest-rank over every recorded request in the JSON
  aggregates. `*-ops.csv` files are a deterministic prefix sample (first
  5,000 requests per operation) for review; they are not the percentile
  source.

## 3. Reproduce

From the recorded source SHA, with root and `lvm2`/loop support:

```bash
cd /root/git/volvisor-wt/w2-43
sudo scripts/bench/run-all.sh \
  --run-dir /tmp/opencode/volvisor-bench-20261103T125250Z
```

This builds release `volvisord` with `CARGO_BUILD_JOBS=4`, runs the full
suite (no `--skip-soak`), and tears down its own VG. The run directory used
for this bundle was `/tmp/opencode/volvisor-bench-20261103T125250Z`. Verify
the bundle with:

```bash
cd docs/benchmarks/2026-11-03-devhost
sha256sum -c MANIFEST.sha256
```

The measurement harness (`run-all.sh`, `bench.py`, `summarize.py`) and the
run use the same SHA (`e3aa365`). `publish.sh` gained a post-run size guard:
the run's 606 MiB daemon log is stored as `raw/daemon.daemon.log.gz` because
forges reject individual files above 100 MiB. The bundle is a copy of the
run directory plus this authored report; no measurement file was edited.

## 4. Capacity table: 1/8/32/128 mixed in-flight

Exact values from [summary/concurrency.csv](summary/concurrency.csv). The
product default admission cap is 256 for these four tiers.

| tier | in-flight | ops | ops/s | error rate | p50 ms | p95 ms | p99 ms | read p95 ms | write p95 ms | 503 | 429 | RSS max KiB | fds max | state max B |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| c1 | 1 | 49974 | 1665.787 | 0.0 | 0.144 | 1.77 | 1.936 | 0.174 | 1.892 | 0 | 0 | 50260.0 | 15.0 | 154820.0 |
| c8 | 8 | 68815 | 2293.7 | 0.0 | 2.526 | 6.454 | 8.866 | 4.787 | 7.878 | 0 | 0 | 104828.0 | 23.0 | 214312.0 |
| c32 | 32 | 27000 | 899.759 | 0.0 | 9.824 | 191.244 | 369.003 | 187.38 | 199.061 | 0 | 0 | 125988.0 | 47.0 | 107633.0 |
| c128 | 128 | 7381 | 241.716 | 0.0 | 443.875 | 1251.564 | 1651.803 | 1245.738 | 1257.975 | 0 | 0 | 147220.0 | 144.0 | 151730.0 |

All 153,170 recorded requests returned 2xx; no `503`, `429` or network
error occurred in the default-cap sweep. Durations were 30.0 s, 30.002 s,
30.008 s and 30.536 s respectively; the small excess is client join/teardown
time, not extra load. The `state max B` column is the largest state file
observed during the tier. It is **not** a clean function of live volumes:
the mixed workers attach/detach repeatedly, and every detached attachment id
is retained in provider state (finding B3), so c1 reaches 154,820 B with one
live volume while c32 reaches 107,633 B. See §8.

Derived observations (from the exact values above):

- 8 in-flight is 1.377× the 1-in-flight throughput, but 32 in-flight is
  0.392× the 8-in-flight throughput and 128 in-flight is 0.269× the
  32-in-flight throughput. End-to-end, c128 is 0.145× c1.
- p95 latency grows from 1.77 ms (c1) to 6.454 ms (c8), 191.244 ms (c32)
  and 1251.564 ms (c128). The c128 p95 is 707.1× the c1 p95.
- Read p95 and write p95 converge at c32/c128: reads do not stay fast while
  writes are in flight. This is consistent with a single provider-state lock
  and whole-state fsync serializing both classes.

This is a control-plane ceiling, not a CPU ceiling: 16 vCPUs were available
and the process was not CPU-saturated; throughput **fell** as in-flight work
rose, which points at serialized durable writes and lock wait.

## 5. Micro (concurrency 1)

Exact values from [summary/micro.csv](summary/micro.csv).

| operation | count | errors | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|
| create | 72 | 0 | 144.016 | 164.455 | 174.616 | 174.616 |
| delete | 72 | 0 | 113.09 | 123.511 | 133.714 | 133.714 |
| attach | 150 | 0 | 1.32 | 1.52 | 1.637 | 1.912 |
| report | 150 | 0 | 1.301 | 1.489 | 1.536 | 1.614 |
| detach | 150 | 0 | 1.295 | 1.58 | 1.76 | 1.797 |
| grow | 30 | 0 | 164.083 | 184.196 | 184.242 | 184.242 |
| inspect | 400 | 0 | 0.117 | 0.156 | 0.232 | 0.296 |
| list | 100 | 0 | 0.12 | 0.147 | 0.154 | 0.231 |
| capabilities | 100 | 0 | 0.106 | 0.135 | 0.173 | 0.178 |

The LVM command cost dominates create/delete/grow; attach/report/detach are
state mutations at roughly 1.3 ms p50. Inspect/list/capabilities are
sub-millisecond p95 at this dataset.

### Journal append and state save

Exact values from [summary/journal-state.csv](summary/journal-state.csv):

| metric | start B | end B | delta B | mutations | bytes/mutation | peak B |
|---|---:|---:|---:|---:|---:|---:|
| journal | 0 | 562847 | 562847 | 624 | 901.998 | — |
| state | 270 | 269 | -1 | — | — | 29678 |

The journal is append-only and fsynced per acknowledged mutation; the
derived **901.998 bytes per mutation** is file growth divided by recorded
mutations, not an isolated append syscall measurement. The state file is
rewritten and fsynced per mutation; its end size returns to the empty-state
size because the cleanup deleted the micro volumes.

### State-file size versus live volumes

Exact values from [summary/state-scale.csv](summary/state-scale.csv):

| live volumes | state B | journal B | last create ms |
|---:|---:|---:|---:|
| 1 | 6486 | 506532 | 123.904 |
| 8 | 11708 | 514445 | 144.251 |
| 32 | 29678 | 541634 | 164.455 |

From 1 to 32 live volumes the create p50 rose from 123.904 ms to 164.455 ms
(+40.551 ms, derived +32.7 %) while the state file grew from 6,486 B to
29,678 B. This is a small dataset by production standards; the point is the
direction and the whole-file rewrite, not a fitted curve.

## 6. Shortened soak (25 minutes, not 24 hours)

Exact values from [summary/soak.csv](summary/soak.csv) and
[csv/soak-timeline.csv](csv/soak-timeline.csv). Four workers, one restart at
12 minutes. The soak ran **1500.019 s (25.0 min)**, not 24 h.

| metric | measured |
|---|---:|
| ops | 1,202,500 |
| ops/s | 801.656 |
| error rate | 0.000148 |
| p50 / p95 / p99 ms | 2.262 / 16.804 / 26.25 |
| RSS first / last / max KiB | 128896.0 / 750612.0 / 750612.0 |
| RSS slope KiB/s | 412.9999 |
| fds first / last / max / min | 19.0 / 18.0 / 19.0 / 9.0 |
| journal first / last / delta B | 45959969.0 / 271399822.0 / 225439853.0 |
| state first / last / delta B | 5911.0 / 2522793.0 / 2516882.0 |
| restarts / restart healthy s | 1 / 1.799 |

The 178 errors (0.000148) are all client-side network errors at the
mid-run restart: 174 `ConnectionRefusedError` and 4 `RemoteDisconnected`.
There were no HTTP error responses and no 5xx/4xx in the soak; the daemon
was deliberately stopped for the restart and the client observed the gap.
The 25.0-minute run is a shortened soak, so this is not a leak proof for
24 hours.

Derived growth (from the exact bytes above; one run, one host):

- **RSS:** +621,716 KiB = **607.1 MiB** over 25.0 min, slope
  **412.9999 KiB/s ≈ 24.29 MiB/min**. This is not flat. At the observed
  slope it is ≈1.42 GiB/h (derived). Filed as finding B2.
- **Journal:** +225,439,853 B = **215.0 MiB** over 25.0 min,
  **150,291.33 B/s ≈ 8.6 MiB/min**; at the observed rate ≈12.1 GiB/day
  (derived). The journal has no rotation or compaction in this build.
  Filed as finding B2.
- **Provider state:** +2,516,882 B = **2.4 MiB** over 25.0 min with only
  four live soak volumes; the state file reached 2,522,793 B. The growth is
  driven by retained attachment history, not by four volume records. Filed
  as finding B3.
- **FDs:** 19 → 18, max 19, min 9. No fd growth was observed in this
  25-minute run.

Mid-soak restart (13:10:22Z): stop 0.422 s, healthy **1.799 s**, journal
178,260,132 B, state 1,465,696 B, replay exposed **4/4** expected volumes,
no missing volumes ([json/soak.json](json/soak.json)).

## 7. Restart replay

Exact values from [summary/restart.csv](summary/restart.csv). Stop time is
SIGTERM → process exit; healthy time is start → `/healthz` 200.

| scenario | iteration | journal before B | state before B | stop s | healthy s | state after B | listed volumes | missing volumes |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| restart-pre-soak | 1 | 562847 | 269 | 0.02 | 0.103 | 269 | 0 | — |
| restart-pre-soak | 2 | 562847 | 269 | 0.003 | 0.102 | 269 | 0 | — |
| restart-pre-soak | 3 | 562847 | 269 | 0.003 | 0.102 | 269 | 0 | — |
| soak-restart | 1 | 178260132 | 1465696 | 0.422 | 1.799 | 1465696 | 4 | — |
| restart-post-soak | 1 | 271435753 | 269 | 0.763 | 2.813 | 269 | 0 | — |
| restart-post-soak | 2 | 271435753 | 269 | 0.464 | 2.669 | 269 | 0 | — |

The pre-soak journal is the micro journal (562,847 B, empty state after
micro cleanup). The mid-soak restart is the meaningful replay-correctness
check: it replayed a 178,260,132 B journal and exposed all four live soak
volumes. The post-soak journal is 271,435,753 B (**258.9 MiB**); replay to a
healthy listener took 2.669–2.813 s. The post-soak state is 269 B and
`listed_volumes` is 0 **because the soak cleanup deleted the four volumes
before this restart**, not because replay lost them; the mid-soak row is the
non-empty state check. Replaying ~259 MiB in ~2.7 s is a useful single-host
lab observation, not a production RTO.

## 8. Findings and filed bottlenecks

These are measured behaviors, not tuning changes. No production behavior was
changed to improve any number.

### B1 — Global provider-state lock + whole-state fsync serializes control-plane work — [#62](https://github.com/o3kio/volvisor/issues/62)

Evidence: §4 (throughput falls 1665.787 → 2293.7 → 899.759 → 241.716 ops/s
as in-flight rises 1 → 8 → 32 → 128; p95 1.77 → 6.454 → 191.244 →
1251.564 ms), §5 (state rewrite cost grows with live volumes) and §6 (state
file reaches 2.4 MiB, so every mutation rewrites/fsyncs a larger file).
Source: `crates/volvisor-lvm/src/provider.rs:263` (`state: Mutex<LvmState>`),
`:336-337` (`persist` under the lock), and
`crates/volvisor-lvm/src/state.rs:313-380` (serialize, `sync_all`, rename,
directory `sync_all`). The read path also takes `lock_state`
(e.g. `provider.rs:363`), so reads wait behind a writer holding the lock
across the fsync.

### B2 — Journal retention grows without bound in memory and on disk — [#64](https://github.com/o3kio/volvisor/issues/64)

Evidence: §6 RSS +621,716 KiB in 25.0 min (412.9999 KiB/s) and journal
+225,439,853 B in 25.0 min, with flat fds (19 → 18). Source:
`crates/volvisor-journal/src/journal.rs:106`
(`operations: HashMap<OperationId, OperationState>`) and
`:590-605` (`apply_record` inserts/updates every replayed operation; no
eviction path exists). The log is append-only with per-record `sync_all`
(`journal.rs:466-496`); there is no rotation/compaction/truncation beyond a
torn-tail repair. Restart replay of the resulting 259 MiB journal took
2.669–2.813 s in §7, so the disk growth also raises future restart time.

### B3 — Retired attachment ids accumulate without bound in provider state — [#63](https://github.com/o3kio/volvisor/issues/63)

Evidence: §6 state 5,911 → 2,522,793 B (2.4 MiB) with only four live soak
volumes; §4 state max is not a function of live volume count. Source:
`crates/volvisor-lvm/src/provider.rs:801-802` appends every detached
attachment id to `retired_attachment_ids`, and
`crates/volvisor-lvm/src/state.rs:124` stores the full vector in every
state save. Because the whole state is rewritten and fsynced per mutation,
this growth amplifies B1: the same four volumes cost more to mutate as
their attachment history grows. No cap, retention window or tombstone
compaction was observed.

The two diagnostic probes are reported for completeness, not as separate
defects:

- **Cap 32, 128 in-flight, 1 s queue timeout:** 3626 ops, 238.045 ops/s,
  p50 447.461 ms, p95 1317.53 ms, p99 1886.251 ms, 0 × 503, 0 errors.
  The cap queued rather than shed because measured queue wait stayed under
  the 1 s timeout. The run does **not** claim that cap 32 sheds; it shows
  the default-cap sweep and this cap probe both pay the same serialization
  cost.
- **Rate limit 50/s, 64 in-flight:** 50.781 accepted ops/s, 946 × 429,
  error rate 0.539031, accepted p95 197.172 ms, all-attempt p95 194.304 ms,
  p99 388.736 ms, 0 network errors. This is real admission shedding. The
  client backed off and reconnected after each 429; without that, the
  harness observed `BrokenPipeError` on keep-alive reuse after a shed
  response, so the reconnect is a harness behavior, not a performance fix.

## 9. Caveats and NOT RUN

- **One host.** One `volvisord`, one client host, loopback only. No
  multi-host, no network hop, no second failure domain.
- **Loop-backed LVM.** The VG is a file-backed loop device, not a physical
  disk. The provider skips loop devices during discovery; the run pre-seeds
  a lab-only device claim. The journal and state files share the same ext4
  filesystem as the loop image; no separate journal/state disk was tested.
- **Control plane only.** No guest I/O, no data-path proxying, no
  filesystem/fio workload, no block-device IOPS or data-path latency.
- **No production SLA or support claim.** These are development-lab numbers.
  They do not change `SUPPORT_MATRIX.md` or any release-scope gate.
- **No real Ceph, DRBD or physical-disk LVM.** `drbdadm`, `rbd`, `ceph` and
  `fio` were absent; `ENVIRONMENT.txt` records this.
- **24-hour soak NOT RUN.** The soak was shortened to 25.0 minutes with one
  mid-run restart. The issue's 24 h soak is explicitly **not** claimed; the
  observed growth rates are one-run observations, not a 24 h leak proof.
- **No clean live-upgrade test.** One binary at one SHA; no old → new
  binary transition or rollback.
- **Dataset is not fixed across tiers.** Each mixed worker owns one volume,
  so the live-volume set is 1/8/32/128 at the four tiers. This is a
  deliberate load shape; it is not a fixed-dataset scaling curve. The
  state-size column is further confounded by retained attachment history
  (B3).
- **Soak read ratio.** The soak target was 60 % reads, but every write is
  preceded by an inspect; the recorded actual read ratio was 0.7997. The
  801.656 ops/s is therefore read-heavy, not a 50/50 or write-heavy rate.
- **Restart network errors.** The 178 soak errors are client-side
  connection errors during the deliberate 0.422 s daemon stop, not server
  error responses.
- **Post-soak replay state.** The post-soak state is 269 B because the
  soak cleanup deleted the live volumes; the mid-soak restart (§6/§7) is the
  non-empty replay check.
- **Cap probe.** Cap 32 with a 1 s queue timeout did not produce 503s in
  this run; queue wait stayed under 1 s. Shedding is demonstrated by the
  50 req/s rate-limit probe, not by the cap probe.

## 10. Artifact map

| Artifact | Contents |
|---|---|
| [summary/TABLES.md](summary/TABLES.md) | all generated tables in one file |
| [summary/concurrency.csv](summary/concurrency.csv) | exact mixed/probe aggregates |
| [summary/soak.csv](summary/soak.csv) | exact soak aggregate |
| [summary/restart.csv](summary/restart.csv) | exact restart-replay rows |
| [summary/micro.csv](summary/micro.csv) | exact micro per-operation rows |
| [summary/journal-state.csv](summary/journal-state.csv) | derived journal/state growth |
| [summary/state-scale.csv](summary/state-scale.csv) | state size at 1/8/32 volumes |
| [csv/soak-timeline.csv](csv/soak-timeline.csv) | per-second RSS/fd/journal/state |
| [csv/*-ops.csv](csv/) | first 5,000 samples per operation |
| [json/](json/) | exact per-scenario aggregates and metadata |
| [raw/](raw/) | orchestration log and the gzip-compressed daemon log |
| [records/](records/) | lab loop/VG identity and pre/post VG listings |
| [configs/](configs/) | sanitized configs; run token replaced with `<redacted>` |
| [MANIFEST.sha256](MANIFEST.sha256) | SHA256 over every other file |
