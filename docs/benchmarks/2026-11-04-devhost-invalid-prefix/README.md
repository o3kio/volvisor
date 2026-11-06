# Invalid pre-fix soak artifact — 2026-11-04, single-host development lab

**Status: INVALID MEASUREMENT — diagnostic provenance only.** Do not use any
number in this directory as capacity, latency, throughput or regression
evidence. It is preserved solely because ADR-0012 and the Phase C plan cite
the `IDEMPOTENCY_CONFLICT` count from this run.

This is a compact artifact of a 25-minute soak that ran on the Phase C
pre-fix tree. It is **not** a published benchmark bundle: the full run
directory was ~3.2 GiB (dominated by the default-INFO daemon log), and the run
is invalid as a measurement because a correctness regression dominated it.

- **Exact source:** `331391e9c4e9d348c2efaae99098425e597b745e`
  (`test(bench): add before/after control-plane run comparison`).
- **Worktree at run time:** clean (`worktree_dirty: no`).
- **Binary:** release `volvisord`, SHA256
  `05b011ad744059bf28ed727393187ca7f37f0472fbd03146d9fe18d557463c85`.
- **Run window:** 2026-11-04T15:48Z .. 2026-11-04T16:18Z.
- **Host:** one development host (`dev`, Ubuntu 24.04.5 LTS, kernel
  `6.8.0-142-generic`, 16 vCPUs, QEMU virtual disks); daemon and clients on
  `127.0.0.1`; loop-backed LVM; control plane only.

## Why it is invalid

The Phase A retired-attachment Bloom filter saturates on a churned volume.
After saturation every fresh attachment id is a false positive, so the provider
refuses it with `IDEMPOTENCY_CONFLICT`. This run recorded:

| metric | value |
|---|---:|
| soak duration | 1500.002 s (25 min) |
| attach attempts | 1,425,668 |
| `IDEMPOTENCY_CONFLICT` | **1,413,505** (99.1 % of attach attempts) |
| other errors | 117 client-side network errors at the deliberate restart |
| total operations | 7,248,650 |
| reported throughput | 4832.428 ops/s (invalid: dominated by refused attaches) |
| provider state | first 11,556 B, last 10,953 B (max 12,091 B) — small because most attaches never happened |
| journal | first 52,101,048 B, max 75,497,972 B, last 24,670,598 B (bounded by compaction) |

Because 99.1 % of attach attempts were refused before exercising the intended
path, the throughput and latency numbers describe a rejection storm, not the
control-plane workload. ADR-0012 replaced the filter with the strictly
increasing per-volume `attachment_generation`; the valid post-fix measurement
is the [2026-11-04 post-fix bundle](../2026-11-04-devhost-postfix/REPORT.md).

## What is committed

Only the files needed to reproduce the cited conflict count are included:

```text
README.md          this provenance note
ENVIRONMENT.txt    host, source SHA, worktree state, binary digest
json/soak.json     exact soak aggregate and per-operation counts
summary/soak.csv   generated soak summary row
raw/00-orchestration.log  unmodified orchestration trace
MANIFEST.sha256    SHA256 over every other file here
```

The full daemon log, per-second timeline and other scenario records are not
committed: they are invalid for capacity purposes and the log is multi-gigabyte.
The files that are committed were copied unmodified from the run directory.
Verify them with `sha256sum -c MANIFEST.sha256` in this directory.

## Non-claims

This artifact is not production evidence, an SLA, a durability proof, the
issue's 24-hour soak, real-hardware or multi-host evidence, or a data-path
measurement. It exists only to keep the cited regression count auditable.
