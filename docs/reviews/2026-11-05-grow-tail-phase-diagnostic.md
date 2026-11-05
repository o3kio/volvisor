# Grow-tail phase diagnostic — 2026-11-05

Status: **Non-evidence diagnostic** (temporary instrumentation in a scratch
worktree at `4290b84`; not a benchmark, not a published measurement, not a
production-support claim)

Scope: Phase D D3 slice 2, issue #62. Companion and correction to the
accepted [grow-tail decision memo](2026-11-05-grow-tail-decision.md).

## Why this exists

The decision memo split each grow at the **journal intent** written by
`ops::execute`. That split is real: the journal anchor lands a median of
2 ms after the request starts. But it is not the provider's
capacity-admission cost. The provider writes its own durable grow intent
into the state file later, after it has sampled the VG and reserved
capacity. The memo treated everything after the journal intent as the
"body" and did not separately time the provider's admission loop.

A temporary instrumented build was used to time the provider phases
directly. No production code was changed; the instrumented tree was a
scratch worktree, and both runs are non-evidence.

## Method

- Source: `4290b84` plus uncommitted `tracing` timers in a scratch
  worktree. The published `target/release/volvisord` was not rebuilt or
  replaced.
- Instrumented phases: `admission` (the whole
  `prepare_grow_admission`), per-attempt `adm_state_read`, `adm_vgs`,
  `adm_stale`, `adm_revalidate`, `adm_prepare_grow`,
  `provider_intent_save`; plus `lvextend`, `verify_lvs`, `commit_state`,
  `provider_total`, `notify` and `journal_outcome`.
- Runs:
  - **run2**: full non-soak sweep (micro, c1/c8/c32/c128, diagnostic
    shedding probes), same sequence as the published run; c128 segment
    analysed.
  - **run3**: c128-only repeat on a fresh loop-backed VG.
- Artifacts: run2 at
  `/tmp/opencode/volvisor-phase-timing2-20261105T133433Z`, run3 at
  `/tmp/opencode/volvisor-phase-timing3-20261105T134026Z` (scratch, not
  published). Instrumented binary SHA-256
  `1b7d0ebdf14efd4180c89eb1f1e0ee25c53482cbe41a2ff1a0159e093e370399`;
  published `4290b84` binary SHA-256
  `80ecd1370cd51dbe923c2175385ca424cdab8a27dcaf5a3035e474499cf35d94`.
- Both runs used a fresh disposable 80 GiB loop-backed VG, the production
  journal/state fsync path, and the default admission cap (256, 5 s queue
  timeout). Client latency comes from the unmodified `bench.py` CSV; the
  phase data comes from the daemon log. The instrumented binary has a
  different hash from the published binary.

## Results

| c128 metric | run2 (full sweep) | run3 (c128 only) |
|---|---|---|
| grows | 409 | 411 |
| client grow mean / p50 / p95 | 9,792 / 9,488 / 14,952 ms | 10,124 / 8,529 / 20,780 ms |
| capacity-admission attempts, total | 1,201 | 1,620 |
| stale-sample attempts | 783 | 1,082 |
| maximum attempts for one grow | 13 | 20 |
| provider capacity admission (mean) | 4,995 ms | 6,329 ms |
| `vgs` sample, per grow summed over attempts | 4,844 ms | 6,101 ms |
| state read under lock | 65 ms | 120 ms |
| revalidate under lock | 84 ms | 105 ms |
| `prepare_grow` (reservation + epoch check) | 1.70 ms | 1.93 ms |
| provider state intent save | 1.67 ms | 1.91 ms |
| `lvextend` | 2,372 ms | 1,874 ms |
| `verify_lvs` | 2,353 ms | 1,843 ms |
| `commit_state` (state lock + save) | 64 ms | 72 ms |
| `notify` (notification store save) | 2.3 ms | 2.0 ms |
| journal outcome append | 0.65 ms | 0.83 ms |
| provider total | 9,785 ms | 10,119 ms |
| client mean minus provider/notify/journal | ~4 ms | ~3 ms |

The attempt distribution in run2 was: 16 grows on the first sample, 263
on the second, and the rest on 3–13 attempts. Run3 was similar but
heavier: 14 first-attempt, 173 second-attempt, and a tail to 20 attempts.
The first `vgs` call of the initial wave is the most expensive (mean
~3.0 s in run2); later retries are shorter but numerous. The sum of the
per-attempt `vgs` times accounts for ~97 % of the provider capacity
admission.

The provider's durable grow intent save is **~1.7–1.9 ms**, and the
journal outcome append is **< 1 ms**. The state save and journal are not
the c128 bottleneck.

## Interpretation

1. The memo's "2 ms admission" is the **ops journal intent**, not the
   provider admission cost. Both statements are true, but they describe
   different boundaries.
2. The provider **capacity-admission loop** is the largest c128 phase:
   5.0–6.3 s mean, dominated by `vgs` samples that are invalidated by the
   global capacity epoch and retried.
3. `lvextend` and the size-verifying `lvs` are also contended
   (1.8–2.4 s each), but they are smaller than the sampling convoy.
4. The measured ~11–12 grow completions/s envelope and the client
   latency distribution are unchanged. What changes is the attribution:
   this is a provider capacity-sampling convoy, not a fixed LVM-body
   ceiling.

The mechanism is visible in the code: every grow attempt calls
`vg_capacity` (`vgs`) outside the state lock, then reserves capacity and
checks the global `capacity_epoch`. Every completed grow bumps that
epoch. Under 128 concurrent grows, a sample is usually invalidated before
the reservation check, so the grow loops and calls `vgs` again. The
provider contract deliberately validates freshness this way; the cost is
the retry convoy.

## Limits

- The instrumented binary adds logging and is not the published binary.
  Timings are from the instrumented build and are non-evidence.
- One host, one loop-backed VG, one daemon; not production capacity.
- `adm_vgs` measures the whole `vgs` call, including waiting for LVM's
  global lock and process startup. It does not separate lock wait from
  command execution.
- The runs are not a durability, crash, multi-host or 24-hour-soak test.
  The 24-hour soak remains **NOT RUN**.

## Consequence

The memo's own trigger is met: a diagnostic now attributes the largest
share of the c128 tail to removable provider bookkeeping. The candidate
fixes are, using the memo's option letters:

- **Option C — coalesce capacity samples per VG (preferred).** One in-flight
  `vgs` sample per VG/epoch, shared by concurrent grows, still validated
  against the epoch; retries share the next sample instead of each
  issuing its own `vgs`.
- **Option B — bound per-VG grow admission.** Cap concurrent grow bodies per
  VG, converting excess concurrency into queue or a typed refusal. This
  reduces the convoy directly but changes client-visible behavior and did
  not previously show a throughput gain as a global bulkhead.
- **A third, more invasive option — trust reservation accounting across an
  epoch bump.** More precise per-VG allocation accounting could avoid
  re-sampling when the completed allocation was already reserved.

Any of these is a production-code change. It requires design, tests
(including the create/epoch and durability regressions), adversarial
review, and a **fresh clean benchmark**; the `4290b84` bundle cannot be
reused as evidence for a new SHA. `#62` remains open and no gate verdict
changes.
