# Grow-tail decision memo — 2026-11-05

Status: **Accepted** (2026-11-05; accepting authority: repository owner).
This is a decision memo, not an ADR, not a code change, and not a new
measurement.
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

The journal anchor is fast, but the queue is **inside** the provider capacity
admission that runs after it. `ops::execute` writes the operation's journal
intent a median of **2 ms** after the client starts (mean 25 ms). The
provider then runs `prepare_grow_admission`, which samples the VG with
`vgs`, reserves capacity, and retries whenever the global capacity epoch
has moved. In the non-evidence instrumented runs, at c128 that loop takes
**5.0–6.3 s mean**, dominated by `vgs` samples that are invalidated and
retried: 409–411 grows needed 1,201–1,620 attempts, 783–1,082 of them
stale, with a tail to 20 attempts.
The provider's own durable grow intent save is **~1.7–1.9 ms**;
`lvextend` and the verifying `lvs` are **~1.8–2.4 s each**; the state
commit is ~65–72 ms; notification and journal outcome are **~2 ms or
less**. Grow completions remain flat at **~11–12 per second** at c8, c32
and c128 even as in-flight grows rise from ~7 to ~104, and latency scales
at **~83–93 ms per in-flight grow**. The measured envelope stands; the
attribution does not: the tail is a provider capacity-sampling convoy, not
a fixed LVM-body ceiling.

**Decision: accept and document the measured per-VG envelope for this
slice.** The post-acceptance diagnostic reopens the body-optimization
question: the largest removable cost is the `vgs` retry convoy, so a
focused fix is now justified. Do **not** add a blind bound; design a
coalesced per-VG capacity sample (preferred) or a measured per-VG bound,
then run tests, adversarial review and a fresh clean benchmark. See
[the phase diagnostic](2026-11-05-grow-tail-phase-diagnostic.md).

> **Correction (2026-11-05, post-acceptance).** As accepted, this memo
> described the 2 ms journal-intent split as "admission" and treated the
> rest as an LVM body. That was wrong about the provider: the provider's
> capacity-admission loop is the largest phase. The measured envelope and
> the accepted no-blind-bound position stand; the body attribution and the
> "no optimization now" conclusion are superseded by the diagnostic.

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
not raise completion rate. The candidate raised the serialized
provider-path ceiling from about **8.4/s (118 ms)** to about **11–12/s
(83–93 ms)**, which is a real improvement, but it is still a ceiling.

## 2. Phase attribution: the journal intent is not the provider admission cost

> The split below is at the `ops::execute` journal intent. It is not the
> provider's capacity-admission cost; see the correction above and the
> [phase diagnostic](2026-11-05-grow-tail-phase-diagnostic.md). The
> "post-intent" column includes the provider capacity loop, `lvextend`,
> `lvs`, state commit and notification.

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
4. Split each grow's client latency at the interpolated journal-intent time:
   **journal intent** (request start → `ops::execute` intent) and
   **post-intent work** (intent → response).

| c128 run | journal-intent mean / p50 / p95 | post-intent work mean / p50 / p95 | total mean |
|---|---|---|---|
| repeat A | 25 ms / 2 ms / 279 ms | 9.422 s / 9.070 s / 16.862 s | 9.447 s |
| repeat B | 25 ms / 2 ms / 74 ms | 9.700 s / 9.772 s / 16.164 s | 9.726 s |

The journal-intent phase exceeds half of the total latency for **0.9 %**
(repeat A) and **0.7 %** (repeat B) of grows. Its p99 is long for a few
requests (6.95 s and 4.16 s), consistent with a small number of grows
caught behind a sampled-state revalidation or a create; it is not the
population tail. The post-intent work carries essentially all of the c128
latency.

This is a non-evidence diagnostic, not a published measurement. It rejects
the "journal intent is the tail" hypothesis: the median journal-intent cost
is 2 ms against ~9 s of post-intent work. It does **not** show that the
queue is outside admission: the post-intent work begins with the provider
capacity-admission loop, which the phase diagnostic measures at
**5.0–6.3 s mean**.

## 3. What the post-intent work contains

After the journal intent, one grow runs:

- `prepare_grow_admission`: up to 32 capacity-sample rounds, each running
  `vgs`, reserving capacity and checking the global capacity epoch; at
  c128 this is the largest phase (**5.0–6.3 s mean**), with 783–1,082
  stale-sample attempts across 409–411 grows and a tail to 20 attempts;
- `lvextend` on the LV (LVM metadata, per-VG serialization);
- a size-verifying `lvs`;
- `commit_grown_size`: the provider state lock plus a durable state save;
- `notify_grow`: the grow-notification store save (the benchmark's
  attachments have no addressable VMM disk, so this records a pending
  notification);
- the journal outcome append and the HTTP response.

The phase diagnostic attributes the largest share to **`vgs` capacity
sampling and its stale-epoch retries**. `lvextend` and `lvs` are also
contended (**~1.8–2.4 s each**), but smaller. The provider's own durable
grow-intent save is **~1.7–1.9 ms**, the state commit ~65–72 ms, and the
journal outcome append **< 1 ms**; those are not the c128 bottleneck.

The exact split inside a single `vgs` call — LVM global-lock wait versus
process execution — is still not separated. The observed completion waves
are consistent with the sample-retry convoy; they do not change the
flat-throughput result.

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

### A. Accept and document the envelope (accepted for the envelope)

Record that, on one loop-backed VG, grow completions are ~11–12/s, grow
latency is approximately `in-flight × 83–93 ms`, and the measured envelope
is an operating point, not a validated ceiling. Operators with grow-heavy
clients should spread volumes across independently lockable VGs and enable
per-principal rate limiting. Do not add a blind admission bound.

Why: the protocol change already delivered its measurable gain (ceiling
~8.4/s → ~11–12/s; c128 grow completions 255 → 438), the default-cap sweep
recorded **zero 503s** and non-grow p95 ~200 ms. This option settles the
envelope, not the body: the diagnostic attributes the largest removable
cost to the `vgs` sample-retry convoy, so option C is reopened. The earlier
provider-level bulkhead A/B does not settle a per-VG sample coalescer.

### B. Bound per-VG grow admission

Cap concurrent grow bodies or capacity samples per VG and return a typed
refusal (or queue) for excess. This now directly targets the convoy and is
a measured candidate, not a rejected posture. It changes client-visible
behavior and needs the pre-journal vs post-intent decision in §4; it is not
selected without measurement.

### C. Coalesce the capacity sample (selected)

Use one in-flight `vgs` sample per VG, shared by concurrent grows and still
validated against the epoch. Retries join the current in-flight round or
start one; a completed sample is not cached, so the next admission attempt
samples fresh. This removes redundant samples without weakening
durable-first intent, fail-closed recovery or idempotency. The diagnostic
attributes the largest removable cost here. This memo did not implement it;
the implementation status is recorded in §9.

### D. Asynchronous grow handle

A contract change (operation handle, polling, retention, idempotency) and out
of scope for this slice.

## 6. Decision and conditions

1. The measured per-VG envelope is accepted (2026-11-05, repository owner).
   No blind grow bound is added.
2. The envelope is documented in
   [admission-control.md](../admission-control.md), the
   [Phase D D3 addendum](../plans/2026-11-04-phase-d-control-plane-hardening.md)
   and the [readiness ledger](../releases/pre-production-readiness.md):
   the ~11–12/s per-VG completion rate, the `in-flight × 83–93 ms` scaling,
   the journal intent at ~2 ms median, the provider capacity admission at
   5.0–6.3 s mean, and `lvextend`/`lvs` at ~1.8–2.4 s each at c128.
3. The phase-timing diagnostic has now been run
   ([non-evidence note](2026-11-05-grow-tail-phase-diagnostic.md)). It
   attributes the largest share to the `vgs` sample-retry convoy. Option C
   was subsequently implemented and measured as recorded in §9. The fresh
   clean benchmark is a c128 regression; it did not support the change on
   throughput or envelope grounds, and it was reverted locally at `59f64cc`
   (code-only revert of `a07203e`), restoring the `4290b84` behavior.
4. Keep **#62 open**. Create and delete still hold the provider state lock
   across their CLI work; the grow body is not fully independent of provider
   bookkeeping. No gate verdict changes.
5. Keep the published bundle as the primary evidence. The repeats, the
   diagnostic and this memo are non-evidence.

## 7. What would change this decision

- The phase-diagnostic trigger was met, and option C was implemented and
  re-measured. The fresh bundle showed a c128 throughput regression (grow
  median up, upper tail down), so the change was reverted at `59f64cc`.
  Reopening it requires a mechanism this bundle can measure; another
  unmeasured implementation is not justified.
- A sustained grow-heavy run at the default cap produces 503s or material
  non-grow tail degradation: then revisit option B with a pre-journal refusal
  and a contract note.
- A product requirement adopts a grow latency SLO or async operation handle:
  that is a contract decision, not a defect fix.

## 8. What this memo does not claim

- It is **not** a new benchmark. The phase split is derived from same-SHA
  repeat journals; the post-acceptance diagnostic is a separate
  non-evidence instrumented run.
- It does **not** claim LVM is the sole cause. The diagnostic attributes the
  largest share to `vgs` sampling and stale-epoch retries; the split inside
  a single `vgs` call is not separated.
- It does **not** claim the 2 ms journal-intent split is the provider
  admission cost; the diagnostic corrects that.
- It does **not** claim a bound would improve throughput, or that the c128
  aggregate write p95 improved. The aggregate p95 remains a grow-value
  quantile and is not like-for-like with `8029df8`.
- It is a single-host, loop-backed-LVM, control-plane observation. It is not
  production capacity, an SLA, a durability result, or the 24-hour soak.
  The 24-hour soak is **NOT RUN**.
- It does not accept any ADR, change an issue state, or change a readiness
  gate. Gate 1, 2 and 5 remain **NOT MET**; gates 3 and 4 remain **PARTIAL**;
  the overall verdict remains **NOT PRE-PRODUCTION READY**.

## 9. Implementation status (addendum, 2026-11-05)

Option C is implemented locally at `a07203e4965aef9e5776d96d496a09b3304437a1`
in the native-local LVM provider. The implementation is **per round**, not a
per-epoch cache: one caller runs `vgs` for a VG, concurrent grow admissions
wait for exactly that round and share its success or typed failure, and a
caller arriving after the round clears starts a fresh sample. Each call —
sample or wait — consumes one `GROW_ADMISSION_ATTEMPTS` attempt, a waiter
never re-samples internally, and a failed round is shared instead of retried
once per waiter. The allocation-epoch checks are unchanged, so a shared
sample cannot double-spend capacity; a sampling panic publishes a typed
failure and clears the slot.

The change passed the workspace tests, `cargo fmt` and workspace clippy, and
two independent adversarial reviews (one final delta review); no Critical or
High findings remained.

It was then measured in a fresh clean single-host dev-lab run at `75beea3`
(code-identical to `a07203e`) against `4290b84`; the bundle is
[2026-11-05-devhost-grow-sample-coalescing](../benchmarks/2026-11-05-devhost-grow-sample-coalescing/REPORT.md).
**The measured result is a c128 regression, not an improvement.** Aggregate
c128 throughput fell **655.326 → 508.225 ops/s (−22.45%)**; the grow median
rose **8,338.505 → 11,335.103 ms**; the 25-minute soak fell **4,756.348 →
4,543.361 ops/s (−4.48%)**. The per-grow upper tail narrowed (p95
**19,633.371 → 12,396.977 ms**; max **34,044.828 → 15,251.096 ms**), but the
grow completion rate fell **11.118 → 10.615/s** and the aggregate write p95
rose because the grow distribution flattened at a higher median, not because
the tail worsened (the bundle explains why that p95 is not like-for-like).

This measurement did **not** support keeping the change on throughput or
envelope grounds, and option C was reverted locally at `59f64cc` (a
code-only revert of `a07203e`); the current tree matches `4290b84` for
`crates/volvisor-lvm`, so `4290b84` is again the current measured envelope.
The bundle has no per-grow `vgs` attempt counter, so the mechanism is not
attributed. `#62` stays open and no gate verdict changes.
