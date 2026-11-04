# Benchmarks

This directory holds benchmark methodology and published evidence bundles.

| Document | Status | Scope |
|---|---|---|
| [Control-plane benchmark evidence — 2026-11-03 devhost](2026-11-03-devhost/REPORT.md) | Single-host dev-lab measurement | `volvisord` HTTP Volume API control plane: 1/8/32/128 in-flight mixed latency/throughput, admission and rate-limit probes, a shortened 25-minute soak, resource growth and restart replay |
| [Invalid pre-fix soak artifact — 2026-11-04](2026-11-04-devhost-invalid-prefix/README.md) | **INVALID measurement, diagnostic provenance only** | Preserves the soak aggregate behind ADR-0012's 1,413,505 spurious `IDEMPOTENCY_CONFLICT` count; must not be used for capacity or regression claims |
| [Control-plane benchmark evidence — 2026-11-04 post-fix](2026-11-04-devhost-postfix/REPORT.md) | Single-host dev-lab measurement | Post-ADR-0012 re-run of the same 1/8/32/128 sweep and shortened 25-minute soak, with before/after comparison |
| [Control-plane benchmark evidence — 2026-11-04 D3 slice 2](2026-11-04-devhost-slice2/REPORT.md) | Single-host dev-lab measurement | Clean-tree re-run at `c4b0b13` of the same 1/8/32/128 sweep and shortened 25-minute soak after releasing the provider state lock for attach/detach/report; same comparator, no throughput-gain claim |
| [Engine-comparison methodology](engine-comparison-methodology.md) | Normative methodology; decision gate closed | How a storage-engine comparison would be measured and decided; records that no measurements exist to carry a build decision |

## Evidence rules

- A benchmark is not a durability or production-support proof (AGENTS
  rule 12). Bundles state their exact source SHA, worktree dirtiness,
  host, kernel, build profile, and what was **not** run.
- Raw JSON/CSV records and a `MANIFEST.sha256` are committed with every
  bundle; authored summaries must not alter measured files.
- Percentiles are nearest-rank over every recorded request; any sampled
  CSV is labeled as a sample, not the percentile source.

The committed harness for the 2026-11-03 bundle is
[`scripts/bench/`](../../scripts/bench/).
