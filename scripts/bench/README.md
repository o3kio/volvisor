# Benchmark harness (#43)

Reproducible, single-host control-plane benchmarks for `volvisord` over the
real HTTP Volume API v2. The harness is bash + Python 3 only: it adds no Rust
dependencies and never edits `Cargo.toml`/`Cargo.lock`.

**This is a development-lab harness, not a production capacity claim.** It
runs one daemon on one host against a disposable loop-backed LVM VG. It does
not exercise the data path, a real Ceph cluster, DRBD, a second physical host,
or a production disk. The 24-hour soak from issue #43 is **NOT RUN**; see the
published bundle for the shortened 20–30 minute soak.

## Requirements

- Linux root with `lvm2`, loop devices, `util-linux`, `curl`, `jq`,
  `python3` (stdlib only) and `sha256sum`.
- A release `volvisord` binary. The default is
  `target/release/volvisord` (`run-all.sh` builds it with
  `CARGO_BUILD_JOBS=4` when missing).
- No pre-existing VG is touched. The harness creates a VG named
  `vgb-<unix-seconds>` on its own loop device and removes only that VG.

## Published run

```bash
# Full published run (release build + sweep + 25-minute soak).
sudo scripts/bench/run-all.sh

# A short local smoke run (no soak; use a fresh run dir).
sudo scripts/bench/run-all.sh --run-dir /tmp/opencode/volvisor-bench-smoke \
  --skip-soak --sweep-seconds 5
```

The default run writes to `/tmp/opencode/volvisor-bench-<UTC>`. It exits
non-zero on the first failed scenario and always tears down its own lab.

Useful options:

| Option | Meaning |
|---|---|
| `--run-dir DIR` | Output directory (default timestamped). |
| `--soak-minutes N` | Shortened soak length (default `25`; the issue's 24h soak is not attempted). |
| `--sweep-seconds N` | Mixed tier duration (default `30`). |
| `--skip-soak` | Skip the soak (micro + sweep + restart only). |
| `--skip-build` | Reuse an existing release binary. |
| `--keep-lab` | Do not tear down the loop VG (debugging only). |

## What runs

1. **Micro (concurrency 1):** create/delete, attach → report → detach,
   grow, inspect, list, capabilities. The LVM journal and provider state
   files are the real files; every acknowledged mutation uses the daemon's
   fsync path.
2. **Restart replay:** stop/start the daemon three times and record
   time-to-healthy with the accumulated journal/state.
3. **Mixed sweep:** 1/8/32/128 in-flight clients for 30 s each, 70% reads /
   30% writes by target ratio (actual ratio recorded). Records p50/p95/p99,
   throughput, error rate, HTTP status/error codes, RSS, fds, journal bytes
   and state bytes.
4. **Admission cap probe (diagnostic):** a separate 15 s run at 128
   in-flight with `max_concurrent_requests = 32` and a 1 s queue timeout.
   This shows whether the cap queues or sheds; a queue timeout longer than
   the observed wait produces queueing instead of `503`, which is recorded
   honestly rather than assumed.
5. **Rate-limit shedding probe (diagnostic):** a separate 15 s run at 64
   in-flight against `rate_limit_per_second = 50` (burst 50). Expect
   `429 RATE_LIMITED` responses; `success_ops_per_sec` reports only accepted
   (2xx) work, while `ops_per_sec` counts every attempt.
6. **Shortened soak:** 25 minutes (configurable) with four workers doing
   continuous attach/report/detach/grow, a restart at the midpoint, and
   per-second RSS/fd/journal/state samples. The restart records the volume
   set visible after replay.
7. **Post-soak restart replay:** two more stop/start rounds on the larger
   journal.

The timings are end-to-end HTTP latency. A rejected request is still a
recorded sample (`http_503`/`http_429` columns and `error_rate`); for a
shedding probe use `success_ops_per_sec` and `accepted_p95_ms` for capacity,
not the all-attempt latency. The client reconnects after a shed response
because an early admission rejection can leave a keep-alive connection
half-closed.

## Outputs

```
RUN/
  ENVIRONMENT.txt          host, kernel, toolchain, source SHA, dirty flag
  configs/                 actual configs with secrets; never publish raw
  json/                    exact aggregates (all-request percentiles)
  csv/                     per-op latency sample + per-second timeline
  raw/                     orchestration log and daemon log
  records/                 lab VG/loop identity, pre/post VG listings
  summary/                 generated CSV tables and TABLES.md
  MANIFEST.sha256          checksums over the run directory
```

Percentiles are nearest-rank over **every** recorded request. The
`*-ops.csv` files are a deterministic prefix sample (first 5,000 requests per
operation) so the bundle stays reviewable; the JSON aggregate is exact.

## Publishing

`publish.sh` copies the reproducible evidence into a docs bundle and redacts
the bearer/claim token. It refuses to copy a file containing the token.

```bash
scripts/bench/publish.sh \
  --run-dir /tmp/opencode/volvisor-bench-2026-11-03T... \
  --dest docs/benchmarks/2026-11-03-devhost
```

`REPORT.md` is authored by the operator from `summary/TABLES.md` and the raw
records; the publish script does not invent a report.

## Comparing two runs

`compare.py` compares two completed run directories by their aggregate JSON
records:

```bash
python3 scripts/bench/compare.py \
  --before /tmp/opencode/volvisor-bench-BEFORE \
  --after /tmp/opencode/volvisor-bench-AFTER \
  --out /tmp/opencode/bench-comparison
```

It writes `comparison.csv` and `comparison.md` under `--out`. The Markdown
report includes the two source SHAs and dirty flags from `ENVIRONMENT.txt`,
the mixed 1/8/32/128 concurrency rows (ops/s, p95, read p95, write p95 and
error rate), the soak resource table, and a derived "normalised per minute"
section when the two soak durations differ. Add `--json` to also print a
machine-readable summary to stdout.

Every input directory must contain a valid `MANIFEST.sha256`; the tool
verifies it before reading records, requires every `json/*.json` it is about
to compare to be listed and verified, and refuses to write a report if any
checksum, missing file or malformed entry fails. `--skip-manifest-check`
exists only for tests/debugging. Use `sha256sum -c MANIFEST.sha256` to check a
bundle independently.

If the two runs contain different sets of recognised scenarios (for example a
partial after-run), the tool still exits 0 but prints a warning to stderr and
adds a prominent warning to the Markdown report. Each record's harness
`status` is listed in the report; a `SETUP_FAILED` record is therefore visibly
different from a merely omitted metric.

Missing JSON values are rendered as `NOT RECORDED` in Markdown and left empty
in CSV; they are never treated as zero. These comparisons remain single-host
loop-LVM control-plane results: they are not production capacity, SLA,
durability or data-path evidence, and dirty-worktree results cannot support a
clean-SHA claim.

## Caveats

- One host, one loop-backed VG, control plane only.
- No data-path proxying and no guest I/O is measured.
- No real Ceph/DRBD/physical-disk LVM, no second host, no production SLA.
- Journal append and state save are not separately observable through the
  API; the harness reports their measured file growth and labels the
  latency as end-to-end (provider operation + journal + state fsync).
- Numbers from a dirty worktree are reported as dirty and cannot support a
  clean-SHA claim.
