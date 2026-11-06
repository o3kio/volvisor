# Control-plane benchmark report — 2026-11-05 per-VG grow capacity-sample coalescing, single-host development lab

This bundle records the clean-tree measurement of option C from the
[grow-tail decision memo](../../reviews/2026-11-05-grow-tail-decision.md):
per-VG single-flight `vgs` capacity sampling for grow admission. It is a
**single-host development-lab** record: one `volvisord` on one host, a
disposable loop-backed LVM volume group, control-plane HTTP only. It is
**not** production evidence, **not** a data-path benchmark, **not** an SLA,
and **not** the issue's 24-hour soak (a shortened 25-minute soak ran).

**Post-measurement disposition:** option C was reverted locally at
`59f64cceaa9483760500ed3b68ca7da89ece8ee3` (a code-only revert of
`a07203e`), restoring the `4290b84` behavior. This report remains the
measurement of `75beea3`; it is not evidence for the reverted tree.

- **Exact source:** `75beea399e593c561104d4d0bb473c88d4d8b93e`
  (`volvisord 75beea3`).
- **Production-code commit:** `a07203e4965aef9e5776d96d496a09b3304437a1`
  (the next commit, `75beea3`, only updates documentation; the two trees are
  code-identical).
- **Worktree at run time:** clean (`worktree_dirty: no` in
  [ENVIRONMENT.txt](ENVIRONMENT.txt), `raw/00-orchestration.log` and every
  raw JSON record).
- **Run window:** 2026-11-05T14:27:48Z .. 2026-11-05T14:58:52Z.
- **Binary:** release build, SHA256
  `d00ef55419b5b13d353c5051923d6443040259c0b122af37fe038d41a50293f4`,
  built from the clean tree with
  `CARGO_BUILD_JOBS=4 cargo build --release --locked -p volvisord`.
- **Comparator:** the published
  [`2026-11-05-devhost-capacity-domain-fix`](../2026-11-05-devhost-capacity-domain-fix/REPORT.md)
  bundle at `4290b8425baf35f73f5df80b76fc9a3a75f39583`, the prior measured
  grow-admission envelope.

## 1. Scope and what changed

The non-evidence
[phase diagnostic](../../reviews/2026-11-05-grow-tail-phase-diagnostic.md)
attributed the largest removable share of the c128 grow tail to the
provider capacity-sample loop: 409–411 grows needed 1,201–1,620 `vgs`
attempts, 783–1,082 of them invalidated by the global allocation epoch, for
a 5.0–6.3 s mean admission loop. Concurrent grows each spawned their own
`vgs`, so the samples convoyed and repeatedly went stale.

`a07203e` added **per-VG single-flight sampling** in
`crates/volvisor-lvm/src/provider.rs`:

- The first grow admission for a VG creates a `CapacityRound` and runs
  `vgs`; concurrent admissions for that VG wait for exactly that round and
  share its success or typed failure.
- A caller that arrives after `in_flight` is cleared starts a fresh sample.
  There is no persistent per-epoch cache.
- Each call — sample or wait — consumes one `GROW_ADMISSION_ATTEMPTS`
  attempt; a waiter never re-samples internally, so the 32-attempt liveness
  cap still bounds the grow.
- A failed round is shared instead of retried once per waiter; a sampling
  panic publishes a typed failure, clears the slot and wakes waiters.
- The allocation-epoch checks are unchanged: every caller re-checks the
  epoch after sampling, and `prepare_grow` re-checks it after inserting the
  reservation, so a shared sample cannot double-spend capacity.

The intended effect was to remove redundant `vgs` calls and shrink the grow
tail. This run measures the actual effect on one host. **It is a
regression at c128, not an improvement.** The per-grow tail narrowed, but
the grow median and aggregate throughput worsened.

## 2. Static checks and review at the measured tree

`cargo fmt --all --check`, workspace clippy with `-D warnings`, the full
workspace test suite and `cargo test --locked --workspace` passed at the
measured tree. Two independent adversarial reviews plus a final delta
review found no Critical or High runtime-correctness defect. The Medium
findings they raised — attempt-cap bypass, serial failed-`vgs` retries, a
vacuous unwind test and overstrong freshness claims — were addressed before
the commit. The workspace tests exercise shared success, shared failure,
stale-round sharing without an internal re-sample, fresh sampling per later
attempt, per-VG independence, panic/unwind waking a parked waiter, and two
concurrent grows completing end-to-end (`concurrent_grows_on_one_vg_both_complete`).

This harness is a load generator, not a crash-consistency test. The
durability, fencing and idempotency properties are test-covered, not
load-proven here.

## 3. What was executed

The published run used the committed harness directly:

```
sudo scripts/bench/run-all.sh \
  --run-dir /tmp/opencode/volvisor-bench-coalesce-20261105 \
  --skip-build --sweep-seconds 30 --soak-minutes 25
```

Shape: micro (create/delete, attach/report/detach, grow, inspect, list,
capabilities), a restart replay, the 1/8/32/128 mixed-concurrency sweep at
30 s per tier with the product default admission cap 256, the cap-32 and
50 req/s diagnostic probes, a 25-minute soak with 4 workers and one planned
restart at 12 minutes, and a post-soak restart replay. The 24-hour soak is
**NOT RUN**.

The release binary was built before the harness started, so `--skip-build`
recorded the same clean tree that produced the binary. `ENVIRONMENT.txt`
records the source, dirty flag, binary path and digest; every raw JSON
aggregate records the same source and `worktree_dirty: false`. The lab
teardown log names only the VG this run created (`vgb-1793888868` on
`/dev/loop0`).

## 4. Headline result

| c128 metric | `4290b84` | `75beea3` | delta |
|---|---|---|---|
| completion-inclusive ops/s | 655.326 | **508.225** | **−22.45%** |
| first-30-s ops/s | 851.3 | **682.0** | **−19.9%** |
| duration | 39.394 s | 40.793 s | +3.55% |
| grow requests | 438 | 433 | −1.14% |
| grow p50 | 8,338.505 ms | **11,335.103 ms** | **+35.9%** |
| grow mean | 9,383.072 ms | 10,399.303 ms | +10.8% |
| grow p95 | 19,633.371 ms | **12,396.977 ms** | **−36.9%** |
| grow p99 | 27,901.837 ms | 12,516.242 ms | −55.1% |
| grow max | 34,044.828 ms | 15,251.096 ms | −55.2% |
| grow completion rate | 11.118/s | 10.615/s | −4.52% |
| aggregate write p95 | 5,078.906 ms | **10,772.171 ms** | **+112.1%** |
| aggregate write p99 | 12,807.538 ms | 12,031.906 ms | −6.06% |
| overall p95 | 117.870 ms | 154.553 ms | +31.1% |
| read p95 | 6.584 ms | 6.462 ms | −1.85% |

The distribution changed shape: **the median and mean rose while the upper
tail fell**. The coalescer did not make grows faster; it made them more
uniformly slow. The 25-minute soak shows the same direction at low
concurrency (grow p50 389.358 → 447.356 ms, +14.9%).

## 5. Capacity table: 1/8/32/128 mixed in-flight

Nearest-rank percentiles over every recorded request; values are the
harness's aggregate JSON, not the 5,000-per-op CSV sample.

| tier | candidate ops/s | comparator ops/s | delta | candidate grow p50 / p95 (ms) | comparator grow p50 / p95 (ms) | candidate write p95 (ms) | comparator write p95 (ms) |
|---|---|---|---|---|---|---|---|
| c1 | 1,813.922 | 1,825.419 | −0.63% | 216.257 / 226.293 | 195.467 / 195.666 | 1.541 | 1.545 |
| c8 | 3,837.267 | 3,846.899 | −0.25% | 566.475 / 1,091.583 | 569.102 / 983.675 | 10.509 | 10.374 |
| c32 | 2,572.763 | 2,609.257 | −1.40% | 2,813.082 / 3,299.776 | 2,358.653 / 3,019.586 | 81.615 | 82.927 |
| c128 | 508.225 | 655.326 | **−22.45%** | **11,335.103 / 12,396.977** | **8,338.505 / 19,633.371** | **10,772.171** | **5,078.906** |

The c1/c8/c32 throughput differences are within a few percent and are not
claimed as a coalescer effect. At c128 the loss is much larger than the
observed host/run drift: micro create p50 rose 5.6% and delete p50 rose
7.1%, while c128 aggregate throughput fell 22.4%. The c128 grow p50 rise is
the largest measured per-operation change and is consistent with the
mixed-workload loss: the driver gives each worker a bounded budget of four
grows and issues them probabilistically, so the realized grow count is
similar in this pair (438 → 433) while total ops fell 19.7%. The loss is in
the read/attach/report/detach cycles that did not fit while grow calls
occupied workers longer. This run does not instrument worker time, so the
split between grow blocking and host variation is not measured.

Non-grow ops at c128 fell together while grows stayed constant:

| c128 operation | `4290b84` count | `75beea3` count | change |
|---|---|---|---|
| attach | 2,410 | 1,907 | −20.9% |
| detach | 2,288 | 1,779 | −22.2% |
| report | 2,408 | 1,907 | −20.8% |
| capabilities | 1,799 | 1,421 | −21.0% |
| inspect | 13,921 | 11,262 | −19.1% |
| list | 2,552 | 2,023 | −20.7% |
| grow | 438 | 433 | −1.1% |

## 6. What the c128 numbers mean

### 6.1 The aggregate write p95 is not like-for-like

The aggregate p95 is taken over every write, so its meaning depends on
which operations occupy the top share. The complete per-operation records
(not the CSV sample) show:

| c128 operation | `4290b84` count | `4290b84` p50 | `4290b84` p95 | `4290b84` max | `75beea3` count | `75beea3` p50 | `75beea3` p95 | `75beea3` max |
|---|---|---|---|---|---|---|---|---|
| attach | 2,410 | 20.871 | 211.323 | 819.067 | 1,907 | 24.478 | 231.918 | 974.188 |
| report | 2,408 | 18.615 | 194.448 | 799.911 | 1,907 | 23.280 | 214.704 | 588.786 |
| detach | 2,288 | 20.794 | 205.577 | 741.928 | 1,779 | 28.451 | 251.423 | 1,199.518 |
| grow | 438 | 8,338.505 | 19,633.371 | 34,044.828 | 433 | 11,335.103 | 12,396.977 | 15,251.096 |
| **write** | **7,544** | **22.241** | **5,078.906** | **34,044.828** | **6,026** | **28.897** | **10,772.171** | **15,251.096** |
| grow share of writes | **5.81%** | | | | **7.19%** | | | |

In both runs the slowest non-grow write (`819.067` and `1,199.518` ms) is
far below the aggregate write p95, so both write p95 values are grow
values. They are not the same population point, however: with a 5.81% grow
share the 95th percentile lies near the 14th percentile of the grow
distribution, and with a 7.19% share it lies near the 31st percentile.
Because the candidate's grow distribution is flatter and higher in the
middle, the aggregate value rises even though the grow tail falls. **The
+112.1% write p95 must not be reported as a like-for-like tail regression
or as a tail improvement.** The per-operation grow row is the honest
comparison.

### 6.2 Grow completion timing

All 433 candidate grows and all 438 comparator grows are present in the
recorded per-operation data behind the copied JSON aggregates (the
per-operation CSV cap is 5,000; those CSV samples are not committed).
Relative to the first recorded request:

- candidate: grow starts span 0.022–29.347 s; completions span 0.618–40.791 s;
  305 grow completions fall in the first 30 s.
- comparator: grow starts span 0.018–29.976 s; completions span 0.452–39.392 s;
  325 grow completions fall in the first 30 s.

The candidate therefore completed 20 fewer grows inside the nominal sweep
window and drained longer afterwards. This is consistent with the higher
median grow latency; the harness's completion-inclusive rate includes that
drain, and the first-30-s rate is reported separately so neither is
cherry-picked. This bundle does not record per-grow `vgs` attempt counts or
admission-loop timing, so it cannot attribute the shape change to a
specific coalescer mechanism. Candidate hypotheses — cohort synchronization
on shared rounds, shared stale rounds consuming waiter attempts, or `vgs`
no longer being the removable bottleneck at this composition — remain
unconfirmed.

## 7. Micro (concurrency 1)

`micro.json` status OK: 1,224 operations, 0 errors.

| operation | `4290b84` p50 / p95 (ms) | `75beea3` p50 / p95 (ms) |
|---|---|---|
| create | 184.771 / 195.737 | 195.092 / 205.691 |
| delete | 144.307 / 154.612 | 154.491 / 174.937 |
| grow | 195.463 / 215.668 | 205.589 / 215.673 |
| attach | 1.409 / 1.641 | 1.435 / 1.621 |
| report | 1.410 / 1.616 | 1.435 / 1.611 |
| detach | 1.425 / 1.593 | 1.445 / 1.624 |
| inspect | 0.110 / 0.136 | 0.115 / 0.163 |
| list | 0.115 / 0.137 | 0.108 / 0.129 |
| capabilities | 0.104 / 0.130 | 0.109 / 0.131 |

At concurrency 1 there is a single caller, so per-VG single-flight cannot
reduce anything; the small grow/create/delete increase is consistent with
run-to-run LVM variance (create +5.6%, delete +7.1% p50) rather than a
coalescer effect. The grow p95 is essentially unchanged (215.668 →
215.673 ms).

## 8. Diagnostic probes

Both are diagnostic shapes, not capacity results.

| probe | `4290b84` | `75beea3` |
|---|---|---|
| cap 32, 1 s queue timeout, c128, 15 s | 646.779 ops/s (620.981 successful); 434 × 503, 2 × 409, 1 × 400 | 566.184 ops/s (533.296 successful); **576 × 503** |
| grow in cap-32 probe | 195, 6 refused; p50 2,717.129 / p95 4,265.859 ms | 194, 15 refused; p50 3,079.335 / p95 3,388.222 ms |
| rate 50/s, burst 50, c64, 15 s | 113.198 ops/s (50.593 successful); 959 × 429 | 110.656 ops/s (50.395 successful); 959 × 429 |
| grow in rate probe | 16, 8 refused; p50 6.363 / p95 229.651 ms | 14, 2 refused; p50 237.061 / p95 337.814 ms |

The default-cap sweep recorded zero 503/429. The cap-32 probe starts a
second daemon with a 1 s queue timeout; the 576 `503`s are contention
outcomes under that deliberately undersized cap, not default-cap errors.
The probe points the same way as the sweep: higher grow median, lower upper
tail, lower throughput.

## 9. Shortened soak (25 minutes, 4 workers)

| metric | `4290b84` | `75beea3` | delta |
|---|---|---|---|
| duration | 1,500.002 s | 1,500.002 s | 0 |
| ops | 7,134,530 | 6,815,052 | −4.48% |
| throughput | 4,756.348 ops/s | 4,543.361 ops/s | −4.48% |
| errors | 81 network (1.1e-05) | 81 network (1.2e-05) | same class |
| overall p50 / p95 / p99 | 0.299 / 2.874 / 3.622 ms | 0.300 / 2.905 / 3.768 ms | +0.3% / +1.1% / +4.0% |
| grow count | 512 | 512 | 0 |
| grow p50 / p95 / max | 389.358 / 599.996 / 711.095 ms | 447.356 / 672.707 / 820.748 ms | +14.9% / +12.1% / +15.4% |
| RSS first / max / last (KiB) | 93,844 / 2,361,868 / 1,705,896 | 88,500 / 2,580,376 / 2,580,376 | +9.25% max |
| journal first / last (B) | 23,927,809 / 49,560,395 | 21,675,535 / 54,075,830 | +9.11% last |
| state first / last / max (B) | 3,896 / 3,147 / 4,083 | 3,650 / 3,317 / 4,083 | max unchanged |
| mid-soak restart | 4/4 volumes in 0.629 s (stop 0.342 s) | 4/4 volumes in 0.648 s (stop 0.345 s) | healthy |
| pre-soak restart | 3 iterations, 0.081–0.102 s | 3 iterations, 0.083–0.102 s | healthy |
| post-soak restart | 2 iterations, 0.606–0.608 s | 2 iterations, 0.609–0.627 s | healthy |

The 81 network errors are the client observing the planned mid-soak daemon
stop/start (the same class and count as the comparator); there are no HTTP
or provider errors and no 409 responses. The soak's grow p50/p95 rise is
consistent with the sweep direction. RSS and journal are single-run
observations; a leak is neither proven nor ruled out. The default INFO
daemon log is unbounded (2.2 GiB raw here).

## 10. Restart replay

All restart records are `OK`. The mid-soak restart listed 4/4 volumes with
byte-identical state and journal sizes before/after. Timings are in §9. The
harness is a load generator, not a crash-consistency test.

## 11. Before/after comparison

`comparison/comparison.md` was generated by `scripts/bench/compare.py`
against the `4290b84` bundle; both manifests verified before reading. It
records the sweep and soak deltas above, including the c128 throughput
**−22.45%** and the aggregate write p95 **+112.1%** that §6.1 explains.

### 11.1 What this measurement supports

- The coalescer, as implemented, **does not improve** the measured c128
  mixed-workload throughput or the accepted grow-completion envelope on
  this host. It **is** associated with a narrower per-grow upper tail
  (p95/p99/max) and a higher median.
- The allocation-epoch protocol, the reservation check and the durable
  grow intent are unchanged and remain test-covered at the measured tree;
  this harness does not load-prove them.
- The c128 grow queue remains LVM-per-VG-bound; the coalescer did not raise
  that bound (grow completion rate 11.118 → 10.615/s).

### 11.2 What it does not support

- A throughput, latency, capacity, envelope or production-support claim.
- A claim that the c128 grow tail is fixed: the median and aggregate
  throughput got worse.
- A claim that the sample-retry convoy was removed in production: this
  bundle has no attempt counter and no admission-loop timing.
- A blanket statement that option C is better or worse for every workload:
  one single-host sample per tier is reported. The same-SHA sensitivity
  note below bounds how much weight the single sample can carry.
- The 24-hour soak, multi-host, real Ceph/DRBD, storage-side fencing,
  crash consistency or data-path behavior.

### 11.3 Same-SHA sensitivity (non-evidence)

A sweep-only repeat at the same source and binary (`--skip-soak`) was run
after the published run to test whether the c128 result reproduced. It is
**not** part of this bundle and is **not** evidence: its tree was dirty
(`worktree_dirty: true`) because this bundle's documentation was being
authored while it ran, although the executed binary was the same
`d00ef554…` digest built from clean `75beea3` and no production file was
modified. It is cited only as a sanity check on the single published
sample, never as a clean-SHA measurement.

| c128 run | state | ops/s | first-30-s ops/s | grow n | grow p50 / p95 / max (ms) | write p95 (ms) | read p95 (ms) |
|---|---|---|---|---|---|---|---|
| published `75beea3` | clean | 508.225 | 682.0 | 433 | 11,335.103 / 12,396.977 / 15,251.096 | 10,772.171 | 6.462 |
| repeat (non-evidence) | dirty (docs only) | 500.747 | 620.5 | 394 | 11,453.312 / 12,417.033 / 12,720.780 | 10,854.778 | 6.647 |
| comparator `4290b84` | clean | 655.326 | 851.3 | 438 | 8,338.505 / 19,633.371 / 34,044.828 | 5,078.906 | 6.584 |

The repeat reproduces the published direction and magnitude: about
500 ops/s, grow p50 about 11.4 s and p95 about 12.4 s — far below the
comparator's 655 ops/s and above its 8.3 s median, while the comparator's
upper tail remains broader. Its c1/c8/c32 rates were 1,831.8 / 3,748.5 /
2,391.8 ops/s (published 1,813.9 / 3,837.3 / 2,572.8), consistent with
several percent of host variation. The repeat does not change the
conclusion; it means the published sample is one of two observations in the
same band, not a unique outlier. It cannot support a clean-SHA claim and is
not used as evidence.

## 12. Caveats and NOT RUN

- Single host, loop-backed LVM, loopback HTTP, control plane only. Not a
  data-path benchmark, not production capacity, not an SLA, not a
  durability test.
- The 24-hour soak is **NOT RUN**; the run is 25 minutes with one restart.
- No second physical host, real Ceph, DRBD, physical-disk LVM, witness or
  storage-side fencing. Multi-host loss and D3 storage-side fencing are not
  exercised.
- No crash-consistency fault injection in this harness; the generation
  fence and crash semantics are covered by the test suite, not this run.
- Single non-fixed-dataset runs; one sample per tier. The mixed sweep
  changes both in-flight work and the number of live volumes (each worker
  owns one volume). It is not a fixed-dataset comparison.
- The c128 aggregate write p95 is composition- and run-sensitive; §6.1
  explains why it must not be compared head-to-head.
- RSS and journal are single-run observations. RSS peak is 2,580,376 KiB;
  a leak is neither proven nor ruled out. The default INFO daemon log is
  unbounded (2.2 GiB raw here), an operational finding.
- The cap-32 and rate-50 probes are diagnostic shapes, not capacity
  results.
- Residuals, honestly recorded: the c128 grow median is worse and the
  enqueue/tail question is unresolved; the mechanism is not instrumented;
  shared-cap grow starvation is documented but not solved; create still
  holds the state lock across its `vgs` sample and `lvcreate`/`lvs`;
  cumulative-run RSS/journal effects are not attributed; `block_in_place`
  worker-set cost is not measured; move admission remains outside the
  epoch protocol; retry `resume_pending_grow` and `settle_failed_lvextend`
  run `lvs` under the state lock; in-doubt journal intents accumulate
  across restarts; duplicate-grow refusals are journaled terminal failures.
- #62 stays open. No gate verdict changes. No production-support, SLA,
  zero-RPO, release or hardware/multi-host claim is made.

## Appendix: files

`ENVIRONMENT.txt`, `MANIFEST.sha256`, `README.md`, `REPORT.md`,
`SCRIPTS.txt`, `configs/`, `json/`, `raw/`, `records/`, `summary/`,
`comparison/`. The measured JSON is copied unmodified; per-operation CSV
samples and timelines are not committed. The only post-run transformation is
the token redaction in `configs/` and the `gzip -9 -n` compression of the
oversized raw daemon log. Verify with:

```
sha256sum -c MANIFEST.sha256
```
