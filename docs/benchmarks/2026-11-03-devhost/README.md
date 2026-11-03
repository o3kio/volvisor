# Control-plane benchmark evidence — 2026-11-03, single-host development lab

This bundle records the reproducible benchmark run behind issue
[#43](https://github.com/o3kio/volvisor/issues/43). It is a **single-host
development-lab** record: one `volvisord` on one host, a disposable
loop-backed LVM volume group, control-plane HTTP only. It is **not**
production evidence, **not** a data-path benchmark, **not** an SLA, and
**not** the issue's 24-hour soak (a shortened 25-minute soak ran).

- **Exact source:** `e3aa365ea65b865c8310a42ea6b459b038f0ab24`
  (`git describe --tags --always` = `e3aa365`).
- **Worktree at run time:** clean (`worktree_dirty: no` in
  [ENVIRONMENT.txt](ENVIRONMENT.txt) and every raw JSON record).
- **Run window:** 2026-11-03T12:53:38Z .. 2026-11-03T13:23:35Z
  (the outer command, including the release build, started at
  2026-11-03T12:52:50Z).
- **Host:** one development host (`dev`, Ubuntu 24.04.5 LTS, kernel
  `6.8.0-142-generic`, 16 vCPUs on an AMD EPYC 9554P, 31.3 GiB RAM,
  QEMU virtual disks). Every client and the daemon ran on `127.0.0.1`.
- **Binary:** release build `volvisord e3aa365`, SHA256
  `670c21259a7474fba0450f779f61967401871fbc2cb47d21da9ab40b0db8a58b`,
  built with `cargo build --release --locked -p volvisord` and
  `CARGO_BUILD_JOBS=4`.
- **Environment and absent capabilities:** [ENVIRONMENT.txt](ENVIRONMENT.txt).
  `drbdadm`, `rbd`, `ceph`, `fio` and a second physical host were absent.

The measurement harness (`run-all.sh`, `bench.py`, `summarize.py`) is
committed under [`scripts/bench/`](../../../scripts/bench/) at the recorded
source SHA `e3aa365`. This bundle is a copy of the run directory; the
unmodified raw records are under `json/`, `csv/` and `raw/`. The run's
daemon log is 606 MiB uncompressed, above forge file-size limits, so
`publish.sh` stores it as `raw/daemon.daemon.log.gz` (gzip, timestamp
omitted). `gunzip` reproduces the run's log; no measurement file was edited.

## What was executed

| Scenario | Shape | Result | Raw record |
|---|---|---|---|
| Micro | concurrency 1: create/delete, attach→report→detach, grow, inspect, list, capabilities; state-file scaling at 1/8/32 live volumes | 0 errors | [micro.json](json/micro.json), [micro.csv](summary/micro.csv) |
| Mixed sweep | 1 / 8 / 32 / 128 in-flight, 70 % reads / 30 % writes by target, 30 s per tier | 0 errors; severe contention above 1 in-flight | [mixed-c1](json/mixed-c1.json), [c8](json/mixed-c8.json), [c32](json/mixed-c32.json), [c128](json/mixed-c128.json) |
| Admission cap probe | 128 in-flight, cap 32, 1 s queue timeout, 15 s (diagnostic) | queued, no `503`; see report | [mixed-cap32-c128.json](json/mixed-cap32-c128.json) |
| Rate-limit shedding probe | 64 in-flight, 50 req/s burst 50, 15 s (diagnostic) | 946 × `429 RATE_LIMITED`; 50.781 accepted ops/s | [mixed-rate50-c64.json](json/mixed-rate50-c64.json) |
| Shortened soak | 25 min, 4 workers, continuous attach/report/detach/grow, restart at 12 min, per-second resource samples | soaks; RSS, journal and state grew (filed) | [soak.json](json/soak.json), [soak-timeline.csv](csv/soak-timeline.csv) |
| Restart replay | 3 pre-soak restarts, 1 mid-soak restart, 2 post-soak restarts | healthy in 0.102 s – 2.813 s; mid-soak replay exposed 4/4 volumes | [restart-pre-soak.json](json/restart-pre-soak.json), [restart-post-soak.json](json/restart-post-soak.json) |

Every acknowledged mutation used the daemon's real journal append + fsync
and provider-state rewrite + fsync. There are no mocks on the write path.

## Capacity in one line

At 30 s per tier with the product default admission cap of 256, measured
throughput was **1665.787 ops/s at 1 in-flight**, **2293.7 ops/s at 8**,
**899.759 ops/s at 32**, and **241.716 ops/s at 128**; p95 latency was
**1.77 ms → 6.454 ms → 191.244 ms → 1251.564 ms** respectively. See
[REPORT.md](REPORT.md) §4 for the full table and [REPORT.md](REPORT.md) §8
for the filed bottlenecks.

## What this bundle is not

- **Not production evidence** and not a support-matrix change.
- **Not multi-host.** There is no second physical host, no real Ceph, no
  DRBD, no physical-disk LVM, and no independent failure domain.
- **Not a data-path benchmark.** No guest I/O, no block-data proxying, no
  filesystem or fio workload, no IOPS/latency of the disk path.
- **Not the 24-hour soak.** The shortened soak ran for 25.0 minutes, with
  one mid-run restart. The 24 h soak is **NOT RUN**.
- **Not an SLA.** All numbers are one host, one loop-backed VG, one process.
  The loop image and the journal/state files share the same ext4 filesystem;
  no separate journal disk was used.
- **Not a clean live-upgrade test.** One binary at one SHA; no old/new
  binary transition.

## Layout

```text
README.md          this file
REPORT.md          measured tables, observations, caveats and bottlenecks
ENVIRONMENT.txt    host, kernel, toolchain, binary digest, absent capabilities
MANIFEST.sha256    SHA256 over every other file in the bundle
configs/           sanitized benchmark configs (tokens replaced)
json/              exact aggregates and per-scenario metadata
csv/               per-second timelines and deterministic per-op samples
raw/               orchestration log and gzip daemon log
records/           lab loop/VG identity and pre/post VG listings
summary/           generated CSV tables and TABLES.md
```

Percentiles are nearest-rank over every recorded request. The
`*-ops.csv` files are a deterministic prefix sample (first 5,000 requests
per operation); the JSON aggregates are exact. The run token was replaced
with `<redacted>` in `configs/`; the bundle has no bearer or claim token.

## Reproduction

The command is recorded in [REPORT.md](REPORT.md) §3. It is:

```bash
sudo scripts/bench/run-all.sh \
  --run-dir /tmp/opencode/volvisor-bench-20261103T125250Z
```

The run directory named above is the workspace copy used to build this
bundle; the committed bundle is the durable evidence copy. Verify the
bundle with `sha256sum -c MANIFEST.sha256`.
