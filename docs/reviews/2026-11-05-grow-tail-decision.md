# Grow-tail decision memo — 2026-11-05

Status: **Proposed** (decision acceptance pending; this is not an ADR, not a
code change, and not a new measurement)
Scope: Phase D D3 slice 2, issue #62, measured tree
`4290b8425baf35f73f5df80b76fc9a3a75f39583`
Related evidence:
[capacity-domain bundle](../benchmarks/2026-11-05-devhost-capacity-domain-fix/REPORT.md)
(primary, `4290b84`) and
[grow-lock-fix bundle](../benchmarks/2026-11-05-devhost-slice2-fix/REPORT.md)
(comparator, `8029df8`).

## Question

The `4290b84` measurement removed the capacity-sample convoy and the
state-lock grow serialization, yet the c128 aggregate write p95 remained a
grow value (5,078.906 ms). Two readings are possible:

1. the grow admission protocol still queues work and needs another bound or
   protocol change; or
2. the tail is the per-volume-group grow body itself, and the honest response
   is to document the operating envelope rather than add another structural
   change.

This memo answers that question from the existing artifacts. It does not run
a new benchmark, change production code, or change issue state.

## Answer in one paragraph

The queue is **after** admission, not in it. In the same-SHA repeat journals,
the durable grow intent is written a median of **2 ms** after the client
starts the request (mean 25 ms), while the remaining body — `lvextend`, size
verification, commit, notification, response — takes a median of **9.1–9.8 s**
at c128. Grow completions are flat at **~11–12 per second** at c8, c32 and
c128 even as in-flight grows rise from ~7 to ~104, and grow latency scales
linearly with in-flight grows at **~83–93 ms per in-flight grow**. That is the
signature of a serialized post-admission bottleneck, not an admission convoy
and not a client artifact. The exact serialized component (LVM CLI, state
save, journal fsync, or notification save) is **not attributable from the
existing records**: the primary run's journal was compacted (frame 1 is a
retention snapshot of 14,774 recent soak-era operations; the c128 grow
intents/outcomes are absent), and the repeat journals retain intent/outcome
records but no phase timestamps.

**Recommendation: accept and document the measured per-VG envelope for this
slice; do not add a per-VG grow bound now.** Before any future optimization,
run a short non-evidence phase-timing diagnostic to attribute the ~83–93 ms
body. If that diagnostic shows provider bookkeeping dominates, revisit with a
focused fix; if LVM dominates, this accept decision is final for the slice.

## 1. What is measured

All values are from the published `4290b84` bundle unless marked
non-evidence. The two same-SHA repeat sweeps used the same binary, host and
cumulative sequence with `--skip-soak`; they are **not** part of the bundle
and are not evidence.

| c128 run | grows | last grow completion | grow completion rate | mean grow latency | mean in-flight grows |
|---|---|---|---|---|---|
| published `4290b84` | 438 | 39.393 s | 11.118/s | 9.383 s | 104.3 |
| repeat A (non-evidence) | 433 | 39.207 s | 11.044/s | 9.447 s | 104.3 |
| repeat B (non-evidence) | 421 | 38.964 s | 10.807/s | 9.726 s | 105.1 |
| comparator `8029df8` | 255 | 30.226 s | 8.436/s | 0.357 s | 3.0 |

The c128 run is work-conserving: after the 30 s sweep stops issuing, **113**
grows (repeat A: 124, repeat B: 126) remain and drain in **9.4 s** (9.2 s,
9.0 s) at about 12 completions/s. There is no stall and no deadlock. The
completion-inclusive throughput (655.326 ops/s) is depressed by that drain;
the first 30 s completed 851.3 ops/s, as already published.

The rate is flat across concurrency:

| tier | grows | last completion | grow completion rate | mean grow latency | mean in-flight | latency / in-flight |
|---|---|---|---|---|---|---|
| c8 | 32 | 2.874 s | 11.13/s | 0.606 s | 6.75 | 90 ms |
| c32 | 128 | 10.622 s | 12.05/s | 2.230 s | 26.88 | 83 ms |
| c128 | 438 | 39.393 s | 11.12/s | 9.383 s | 104.33 | 90 ms |
| repeat A | 433 | 39.207 s | 11.04/s | 9.447 s | 104.33 | 91 ms |
| repeat B | 421 | 38.964 s | 10.81/s | 9.726 s | 105.09 | 93 ms |
| comparator c128 | 255 | 30.226 s | 8.44/s | 0.357 s | 3.01 | 118 ms |

Adding in-flight grows raises latency almost exactly in proportion and does
not raise completion rate. The candidate raised the serialized-body ceiling
from about **8.4/s (118 ms)** to about **11–12/s (83–93 ms)**, which is a real
improvement, but it is still a ceiling.

## 2. Phase attribution: admission is not the tail

The published run's journal was compacted; its c128 grow segment is not
retained. The two repeat runs at the same SHA and binary retain it
(`mix-c128-grow-*` intents and outcomes). The method:

1. Parse the framed journal and pair each `mix-c128-grow-*` intent with its
   outcome.
2. Match the ordered journaled outcomes for grow, attach, report and detach
   to the client's per-operation completion times (6,843 and 6,961 anchors).
3. Interpolate the intent frame's wall time; the interpolation reproduces
   each outcome's client completion time to within **4 ms** (repeat A) and
   **1 ms** (repeat B).
4. Split each grow's client latency at the interpolated durable-intent time:
   **admission** (request start → durable intent) and **body** (intent →
   response).

| c128 run | admission mean / p50 / p95 | body mean / p50 / p95 | total mean |
|---|---|---|---|
| repeat A | 25 ms / 2 ms / 279 ms | 9.422 s / 9.070 s / 16.862 s | 9.447 s |
| repeat B | 25 ms / 2 ms / 74 ms | 9.700 s / 9.772 s / 16.164 s | 9.726 s |

Admission exceeds half of the total latency for **0.9 %** (repeat A) and
**0.7 %** (repeat B) of grows. The admission p99 is long for a few requests
(6.95 s and 4.16 s), which is consistent with a small number of grows caught
behind a sampled-state revalidation or a create; it is not the population
tail. The body carries essentially all of the c128 latency.

This is a non-evidence diagnostic, not a published measurement. It is used
here only to reject the "admission is the tail" hypothesis, which it does
decisively: the median admission cost is 2 ms against a 9 s body.

## 3. What the body contains, and what is not attributed

After the durable intent, one grow runs:

- `lvextend` on the LV (LVM metadata, per-VG serialization);
- a size-verifying `lvs`;
- `commit_grown_size`: the provider state lock plus a durable state save;
- `notify_grow`: the grow-notification store save (the benchmark's
  attachments have no addressable VMM disk, so this records a pending
  notification);
- the journal outcome append and the HTTP response.

Each of these is serialized by a different lock (LVM, provider state,
notification store, journal). The measured body is consistent with the sum
of those serialized steps, but the existing records do not time them
separately: successful LVM calls are not logged with `elapsed_ms`, the
journal has no timestamps, and the state and notification saves are not
traced. **No cause is assigned to the ~83–93 ms body beyond "a serialized
post-admission resource".**

The observed completion waves (multi-second clusters) are recorded but not
attributed. They are consistent with a shared serialized resource in the
body; they do not change the flat-throughput result.

## 4. Contract constraints

From [Volume API v2 §4A and §12](../../contracts/volume-api-v2.md):

- `GrowVolume` is a synchronous, journaled mutation. There is **no
  asynchronous operation handle and no grow latency SLO**.
- Admission refusals (`429`, `503`) are **not journaled** and are safe to
  retry.
- A provider-level refusal raised after the journal intent (the existing
  retry-cap `OVERLOADED`) is **terminal for that `operation_id`**; a retry
  needs a new id.
- A new per-VG grow bound would therefore need a deliberate choice: refuse
  **before** the journal (retry-safe, but the daemon must know the VG and the
  bound before `execute`) or refuse **after** the intent (terminal for the
  id). Either is a contract-level behavior change, not a local optimization.

The contract's long-operation model is explicit that a journaled mutation is
not dropped after the intent. Any bound must preserve that rule.

## 5. Options

### A. Accept and document the envelope (recommended)

Record that, on one loop-backed VG, grow completions are ~11–12/s, grow
latency is approximately `in-flight × 83–93 ms`, and the serialized resource
is post-admission. Operators with grow-heavy clients should spread volumes
across independently lockable VGs and enable per-principal rate limiting.
Add no new admission bound.

Why: the protocol change already delivered its measurable gain (ceiling
~8.4/s → ~11–12/s; c128 grow completions 255 → 438), the default-cap sweep
recorded **zero 503s** and non-grow p95 ~200 ms, and a bound cannot raise a
serialized-body ceiling. The previous provider-level bulkhead A/B already
showed that a concurrency bound added queue delay without raising the
LVM-bound completion rate.

### B. Bound per-VG grow admission now

Cap queue depth or wait time and return a typed refusal. This protects the
shared admission permits and makes latency predictable, but it does not raise
the ~11–12/s ceiling; it converts queue into refusal. At the measured default
cap, no read/attach starvation was observed, so the protection is a posture,
not a measured defect. It also needs the pre-journal vs post-intent decision
in §4.

Not recommended now. Revisit only if a sustained grow flood produces
default-cap 503s, material non-grow p95 degradation, or an explicit latency
SLO.

### C. Optimize the serialized body

If a phase-timing diagnostic shows provider bookkeeping (state save, journal
append, notification store) dominates the ~83–93 ms, reduce or coalesce that
work without weakening durable-first intent, fail-closed recovery or
idempotency. If LVM dominates, option A stands. This is a future,
measurement-first path; it is not authorized by this memo.

### D. Asynchronous grow handle

A contract change (operation handle, polling, retention, idempotency) and out
of scope for this slice.

## 6. Recommendation and conditions

1. **Accept option A for the measured slice.** Do not add a grow bound now.
2. If accepted, document the envelope in
   [admission-control.md](../admission-control.md) (the grow-heavy section
   already records 438 grows in 39.4 s) and in the Phase D D3 addendum: the
   ~11–12/s per-VG completion rate, the `in-flight × 83–93 ms` scaling, and
   the fact that admission is ~2 ms median while the body is ~9 s at c128.
3. Before any body optimization, run a **short non-evidence phase-timing
   diagnostic** at c128 (instrumented or sampled: `lvextend`, `lvs`, state
   intent/commit save, journal append, notification save). Do not change
   production code on an unattributed cause.
4. Keep **#62 open**. Create and delete still hold the provider state lock
   across their CLI work; the grow body is not fully independent of provider
   bookkeeping. No gate verdict changes.
5. Keep the published bundle as the primary evidence. The repeats and this
   memo are non-evidence diagnostics.

## 7. What would change this decision

- A phase diagnostic attributes a majority of the ~83–93 ms body to
  removable provider bookkeeping **and** a test shows the durability
  invariants still hold: then implement option C and re-measure with a fresh
  clean bundle.
- A sustained grow-heavy run at the default cap produces 503s or material
  non-grow tail degradation: then revisit option B with a pre-journal refusal
  and a contract note.
- A product requirement adopts a grow latency SLO or async operation handle:
  that is a contract decision, not a defect fix.

## 8. What this memo does not claim

- It is **not** a new benchmark and adds no measured values. The phase split
  uses same-SHA repeat journals and is explicitly non-evidence.
- It does **not** claim LVM is the sole cause of the body. The exact
  serialized component is unattributed.
- It does **not** claim a bound would improve throughput, or that the c128
  aggregate write p95 improved. The aggregate p95 remains a grow-value
  quantile and is not like-for-like with `8029df8`.
- It is a single-host, loop-backed-LVM, control-plane observation. It is not
  production capacity, an SLA, a durability result, or the 24-hour soak.
  The 24-hour soak is **NOT RUN**.
- It does not accept any ADR, change an issue state, or change a readiness
  gate. Gate 1, 2 and 5 remain **NOT MET**; gates 3 and 4 remain **PARTIAL**;
  the overall verdict remains **NOT PRE-PRODUCTION READY**.
