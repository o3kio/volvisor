# Benchmarks

This directory holds benchmark methodology and published evidence bundles.

| Document | Status | Scope |
|---|---|---|
| [Control-plane benchmark evidence — 2026-11-03 devhost](2026-11-03-devhost/REPORT.md) | Single-host dev-lab measurement | `volvisord` HTTP Volume API control plane: 1/8/32/128 in-flight mixed latency/throughput, admission and rate-limit probes, a shortened 25-minute soak, resource growth and restart replay |
| [Invalid pre-fix soak artifact — 2026-11-04](2026-11-04-devhost-invalid-prefix/README.md) | **INVALID measurement, diagnostic provenance only** | Preserves the soak aggregate behind ADR-0012's 1,413,505 spurious `IDEMPOTENCY_CONFLICT` count; must not be used for capacity or regression claims |
| [Control-plane benchmark evidence — 2026-11-04 post-fix](2026-11-04-devhost-postfix/REPORT.md) | Single-host dev-lab measurement | Post-ADR-0012 re-run of the same 1/8/32/128 sweep and shortened 25-minute soak, with before/after comparison |
| [Control-plane benchmark evidence — 2026-11-04 D3 slice 2](2026-11-04-devhost-slice2/REPORT.md) | Single-host dev-lab measurement | Clean-tree re-run at `c4b0b13` of the same 1/8/32/128 sweep and shortened 25-minute soak after releasing the provider state lock for attach/detach/report; same comparator, no throughput-gain claim |
| [Control-plane benchmark evidence — 2026-11-05 D3 slice-2 grow-lock fix](2026-11-05-devhost-slice2-fix/REPORT.md) | Single-host dev-lab measurement | Clean-tree re-run at `8029df8` of the same 1/8/32/128 sweep and shortened 25-minute soak after releasing the provider state lock across `lvextend`; c32 write p95 590.847 → 206.203 ms and c128 6330.548 → 3413.886 ms; c128 grow tail not fixed |
| [Control-plane benchmark evidence — 2026-11-05 capacity-domain grow admission](2026-11-05-devhost-capacity-domain-fix/REPORT.md) | Single-host dev-lab measurement | Clean-tree re-run at `4290b84` of the same 1/8/32/128 sweep and shortened 25-minute soak after moving the grow capacity sample out of the state lock and adding the allocation-epoch protocol; c32 write p95 206.203 → 82.927 ms, c128 read p95 87.276 → 6.584 ms; c128 grow tail remains LVM-bound and unresolved |
| [Control-plane benchmark evidence — 2026-11-05 per-VG grow capacity-sample coalescing](2026-11-05-devhost-grow-sample-coalescing/REPORT.md) | Single-host dev-lab measurement; **measured regression** | Clean-tree re-run at `75beea3` (code-identical to `a07203e`) of the same 1/8/32/128 sweep and shortened 25-minute soak after per-VG single-flight `vgs`; c128 throughput 655.326 → 508.225 ops/s (−22.45%), grow p50 8,338.505 → 11,335.103 ms while p95 19,633.371 → 12,396.977 ms; not adopted on throughput or envelope grounds and reverted locally at `59f64cc` |
| [Control-plane benchmark evidence — 2026-11-05 Phase E create/delete lock scope](2026-11-05-devhost-phase-e-write-lock-scope/REPORT.md) | Single-host dev-lab measurement; **measured mixed-throughput regression** | Clean-tree re-run at `2c5b065` (E1 `8cfa4c2` delete, E2 `2c5b065` create) of the same 1/8/32/128 sweep and shortened 25-minute soak after releasing the state lock across `lvcreate`/`lvremove`; c8/c32/c128 −2.35/−5.10/−6.77%, soak −7.12%, c128 p95 117.870 → 100.558 ms; sweep and soak contain no create/delete, so no delta is attributed to E1/E2 and no throughput gain is claimed |
| [Control-plane benchmark evidence — 2026-11-05 Phase E worker handoff](2026-11-05-devhost-phase-e-worker-handoff/REPORT.md) | Single-host dev-lab measurement; **c128 drop measured but not attributed** | Clean-tree re-run at `7204a53` (E1/E2 `2c5b065` plus the create/delete `run_blocking_body` handoff; intervening commits are evidence/doc corrections) of the same 1/8/32/128 sweep and shortened 25-minute soak; create p50/p95 flat in both same-SHA samples, delete +0.12%/+0.28% in the measured sample but +6.34%/+5.90% in an explicitly non-evidence same-SHA repeat (delete overhead unresolved), c1/c8 +0.82%/+1.81%, c32 −1.61%, c128 −24.24%; the timed sweep and soak contain no create/delete and a non-evidence alternating A/B shows c128 427.5–626.0 ops/s across the four runs of both arms (A 439.5–626.0, B 427.5–432.8), so no sweep/soak delta is attributed to the handoff and no gain or zero-regression claim is made |
| [Control-plane benchmark evidence — 2026-11-06 Phase F write-path remainder](2026-11-06-devhost-phase-f-write-path-remainder/REPORT.md) | Single-host dev-lab measurement; **c128 drop measured but not attributed** | Clean-tree re-run at `2edcecd` of the same 1/8/32/128 sweep and shortened 25-minute soak after the Phase F create/delete lock-scope remainder; deterministic gate tests for move/delete/create/failed-create compensation and #63/#64 code-present verification; c1/c8/c32/c128 −0.63/−2.11/−5.14/−9.29% and soak −1.005% measured but **not attributed** because the timed load contains no create/delete/move (c128 p99 11,800.496 ms and 43.425 s duration recorded); no gain or zero-regression claim |
| [Engine-comparison methodology](engine-comparison-methodology.md) | Normative methodology; decision gate closed | How a storage-engine comparison would be measured and decided; records that no measurements exist to carry a build decision |

## Evidence rules

- A benchmark is not a durability or production-support proof (AGENTS
  rule 12). Bundles state their exact source SHA, worktree dirtiness,
  host, kernel, build profile, and what was **not** run.
- Exact JSON aggregates, the derived summary tables and a `MANIFEST.sha256`
  are committed with every bundle; authored summaries must not alter
  measured files.
- Per-operation CSV samples and per-second timelines are regenerable and
  are not committed for new bundles (`docs/benchmarks/**/csv/` is ignored in
  `.gitignore`). Bundles published before that rule keep their tracked CSV
  files. Percentiles are nearest-rank over every recorded request; the JSON
  aggregates are exact, and any retained CSV sample is not the percentile
  source.

The committed harness for the 2026-11-03 bundle is
[`scripts/bench/`](../../scripts/bench/).
