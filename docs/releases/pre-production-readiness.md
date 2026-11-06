# Pre-production readiness report — 2026-11-03

Status: **NOT PRE-PRODUCTION READY.** Two of the five gates in
[#36](https://github.com/o3kio/volvisor/issues/36) are partially evidenced;
three are unmet. This document maps every gate to the evidence that exists
and names what does not. It is a readiness record, not a release, a support
statement, or an SLA.

- **Basis:** `main` at `575acfc` (Wave 2 merged). CI run
  [41647509537](https://github.com/o3kio/volvisor/actions/runs/41647509537)
  is green for `check`, `msrv`, `lvm-integration`, `supply-chain`,
  `package-build`, `package-smoke` (Debian/Fedora) and `test-harness`.
- **Tag / release:** **none**. `git tag --list` is empty and no GitHub
  release exists.
- **Claim scope:** [`release-scope.toml`](../../release-scope.toml) marks
  only `native-local` as `supported`; `ceph-rbd` and
  `nearline-replicated` are `not-run` and explicitly **not claimed**. The
  `native-local` suite is a loop-device gate, not real-NVMe qualification.

## Post-report addendum (2026-11-03)

This addendum records work that landed after the report's basis. **The basis
of this report remains `main` at `575acfc`**, and the addendum changes no gate
verdict below.

- [PR #69](https://github.com/o3kio/volvisor/pull/69) merged at
  `1863a7ac250d20246c328a2fd4c4acca330e3693`, fixing
  [#58](https://github.com/o3kio/volvisor/issues/58).
- D5 (ENOSPC) was re-drilled post-fix (both parts) at harness
  `940722ea3322df0a47834c307e2e0081c57e0b5e` (`940722e`) on one development
  host. `git diff --name-only 1863a7a 940722e -- crates/ Cargo.toml
  Cargo.lock release-scope.toml` is empty, so the product code is identical to
  merged `1863a7a`; only the three day-2 harness scripts (`d3.sh`, `d5.sh`,
  `lib.sh`) differ. The raw header records a clean worktree.
- Bundle:
  [`docs/evidence/day2/2026-11-03-d5-redrill/`](../evidence/day2/2026-11-03-d5-redrill/REPORT.md).
  It proves the **normal uncommitted state-save path** fails closed and
  converges, and re-confirms the journal path. It does not fault-inject the
  cleanup-failure, post-rename carve-out, clear-save or
  unreadable-durable-reread branches; those remain test-covered.
- [#70](https://github.com/o3kio/volvisor/issues/70) is addressed by the
  provider-state intent journaling in this change: durable-first
  create/grow/delete intents, startup adoption/roll-forward/completion, move
  persist-point rollback, and a test-only `AfterRenameBeforeDirFsync` seam on
  the real save path. The per-mutation contracts, startup recovery and
  operator tails are documented in
  [lvm-state-divergence.md](../runbooks/day-2/lvm-state-divergence.md). The
  additive intent maps bump the durable LVM schema to v2 under the
  [schema compatibility policy](../schema-compatibility.md) (v1 files still
  load and upgrade on write; an older binary refuses v2 typed). They are
  covered by deterministic simulation tests, **not** a hardware kill-9
  drill, so this changes no gate verdict.
- No gate becomes met; gates 1, 2 and 5 remain **NOT MET**, gates 3 and 4
  remain **PARTIAL**, and the overall status remains **NOT PRE-PRODUCTION
  READY**.

## Post-report addendum (2026-11-04) — Phase B readiness residuals

This addendum records the Phase B "readiness residuals and evidence
automation" batch delivered on branch `phase-a-scaling` after the 2026-11-03
addendum. **It changes no gate verdict below.** Every item is implementation,
test automation, documentation or single-host lab evidence; none is a
production-support, SLA, zero-RPO, hardware or released-artifact claim.

- **#44 observability:** committed traces are now validated structurally and
  semantically by `crates/volvisor-api/tests/trace_evidence.rs` and
  [`scripts/validate-traces.py`](../../scripts/validate-traces.py): one JSON
  object per line, RFC 3339 timestamps, bounded span names, `operation_id`
  correlation, and no test credential or secret-like field. The committed
  `attach.jsonl` passes. The **real migration trace remains NOT RUN** — it
  needs the DRBD/peer/real-VMM rig — and no simulated trace is committed. The
  two deliberately omitted gauges and their reasons are documented in
  [observability.md](../operations/observability.md).
- **#46 day-2:** a real **cross-binary D2 live** drill now exists and passed on
  one host: old `8c2fc1a` (LVM schema 2) → new `aad1720` (schema 3) → typed old
  refusals → restore the whole v2 directory and serve the original volume with
  the old binary. D1 was re-run against the v3 bounded tombstone filter and
  records the behavioral retired-id `409 IDEMPOTENCY_CONFLICT` (pre-ADR-0012;
  the current harness asserts the strictly increasing attachment-generation
  fence instead). D2's same-binary policy drill was corrected to schema v3.
  These are single-host,
  loop-LVM, debug-build results (D1/D2 used a dirty caller build); see
  [the bundle](../evidence/day2/2026-11-04-devhost-live/REPORT.md),
  [backup-and-restore](../runbooks/day-2/backup-and-restore.md) and
  [upgrade-and-rollback](../runbooks/day-2/upgrade-and-rollback.md).
  `preflight.sh` now reports D2/D3/D8 prerequisites, including the old build.
  **D3 storage-side fencing remains NOT RUN** (no DRBD 9 module/peer/witness)
  and **D8 total host loss remains NOT RUN** (no multi-host/BMC/oracle).
- **#48 CHV:** the Volvisor side is delivered in-repo — capability negotiation,
  attachment-state report behavior, a consumer conformance test
  ([`chv_consumer.rs`](../../crates/volvisor-api/tests/chv_consumer.rs)), and an
  [integration guide](../integrations/chv.md). **No CHV/Volvisor pair is
  pinned** and CHV's ceiling remains `advertised`; a pin still requires a
  signed, tagged Volvisor release (#47). `active`/`guest_confirmed` is not
  implemented or claimed.
- **#49 flake:** [`scripts/flake-rate.py`](../../scripts/flake-rate.py)
  computes the observed rate and the 14-consecutive-night criterion from
  `campaign-flake` JSONL artifacts; unit tests cover the criterion edge cases
  and the campaign workflow reports the current run's rate. **No observed
  flake rate is published and the 14-night criterion remains NOT MET** because
  no nightly artifact history exists. New deterministic journal
  compaction/pin interleaving tests landed as well.
- **Validation:** `cargo fmt`, clippy, workspace tests, journal fault-feature
  tests, rustdoc, the fuzz-workspace check, `bash -n` plus plain
  `shellcheck -x` over the day-2 harness, the trace validator and its Python
  unit tests, the flake unit tests and workflow YAML parsing run in the
  batch's validation; the exact commands are in the
  [Phase B plan](../plans/2026-11-04-phase-b-readiness-residuals.md).

No gate becomes met. Gates 1, 2 and 5 remain **NOT MET**, gates 3 and 4
remain **PARTIAL**, and the overall status remains **NOT PRE-PRODUCTION
READY**. No tag, GitHub release, push or pull request is created by this
workstream.

## Post-report addendum (2026-11-04) — Phase C state-substrate verification

This addendum records the Phase C "state-substrate verification and
hardening" batch delivered on branch `phase-a-scaling` after the Phase B
addendum. It follows the [Phase C plan](../plans/2026-11-04-phase-c-state-substrate-verification.md).
**It changes no gate verdict below.** Every item is code hardening,
measurement automation, single-host lab measurement or documentation; none is
a production-support, SLA, capacity, zero-RPO, hardware or released-artifact
claim.

- **C1 — compaction hardening (#64).** A persistently failing compaction now
  backs off exponentially instead of deep-cloning and reserializing the whole
  retained registry under the writer lock on every append, and tombstone
  membership is an O(1) index instead of a linear ring scan. Deterministic
  regressions cover attempt bounding and recovery, duplicate/refresh,
  eviction/re-insertion, snapshot round-trip and brute-force membership
  equivalence. This addresses the two cost defects the review found; **the
  issue is not claimed closed**, because the RSS high-water and the separate
  log-volume finding below remain open.
- **C1b — attachment churn (ADR-0012, #63).** The fixed-size retired-id Bloom
  filter saturated under churn and then refused every fresh attachment id with
  `IDEMPOTENCY_CONFLICT`. A strictly increasing per-volume
  `attachment_generation` is now the detach/report fence; the filter is legacy
  decode-only and a legacy v1/v2 vector is still imported at load. The
  [ADR-0012](../adr/0012-monotonic-attachment-generation-fence.md) header
  records it as **Accepted** (2026-11-06, zoorpha); the contract amendment
  is implemented.
- **C2 — comparison tooling.** `scripts/bench/compare.py` verifies both
  `MANIFEST.sha256` files, reads the real aggregate shapes, refuses a
  tampered/missing/mismatched run, and emits CSV/Markdown with explicit
  `NOT RECORDED` and zero-baseline handling. Its 14 unit tests run in CI.
- **C3 — post-fix measurement.** A clean-tree run at
  `aab773b7b58448e41ba22b21372785b3c58e7381` (binary SHA256
  `238895a42097fd06f33e30f941873b3a9ca82b623efaf2a60c6f375d2d1edf4a`)
  completed (the orchestration log ends with `done:` after a clean teardown)
  and is published as the
  [2026-11-04 post-fix bundle](../benchmarks/2026-11-04-devhost-postfix/REPORT.md)
  with its [before/after comparison](../benchmarks/2026-11-04-devhost-postfix/comparison/comparison.md).
  The pre-fix conflict storm is preserved only as a labelled
  [invalid-run artifact](../benchmarks/2026-11-04-devhost-invalid-prefix/README.md).
  Headline results: 5,424,744 soak ops at 3616.49 ops/s, **0 HTTP errors and
  0 `IDEMPOTENCY_CONFLICT`** (70 client network errors at the deliberate
  restart), state end **3,055 B** (max 3,675 B), journal max **75.5 MB
  (72.0 MiB)** and end **28.0 MB (26.7 MiB)**, mid-soak restart healthy in
  **0.552 s** with 4/4 volumes. Against 2026-11-03, soak ops are +351.1 %,
  c1/c8 ops/s +17.2 %/+52.3 %, c32/c128 −4.1 %/−2.6 %, soak journal end
  −89.7 % and state end −99.88 %.
- **Open residuals (not fixes).** RSS peaked at **2,323,028 KiB (≈2.22 GiB)**
  and did not return to its start value; the peak coincides with a
  post-compaction journal minimum, which is consistent with allocator
  transients but proves neither a leak nor its absence. The default INFO
  daemon log produced **2.57 GB** over the ~30-minute run (including the
  25-minute soak) with no rotation. Neither is a durable-state defect and
  neither is claimed fixed. The run is one host, loop-LVM, control-plane only,
  and a
  shortened **25-minute** soak; the 24-hour soak is **NOT RUN**. Issue **#62**
  is not closed by this measurement.
- **Validation:** `cargo fmt`, workspace tests, clippy and rustdoc with
  `-D warnings`, the journal `test-faults` suite, `bash -n` and
  `shellcheck -x` over the day-2 and bench scripts, and the comparator unit
  tests pass at `aab773b`.

No gate becomes met. Gates 1, 2 and 5 remain **NOT MET**, gates 3 and 4
remain **PARTIAL**, and the overall status remains **NOT PRE-PRODUCTION
READY**. No tag, GitHub release, push or pull request is created by this
workstream.

## Post-report addendum (2026-11-04) — Phase D control-plane hardening

This addendum records the Phase D batch delivered on branch `phase-a-scaling`
after the Phase C addendum. It follows the
[Phase D plan](../plans/2026-11-04-phase-d-control-plane-hardening.md).
**It changes no gate verdict below.** Every item is logging, authentication
posture, documentation or staged write-path hardening (slice 1 landed; slice 2
code landed with no throughput claim until a new clean-tree measurement); none
is a production-support, SLA, capacity, durability or hardware claim.

- **D1 — log volume (#64 residual).** The pre-dispatch `accepting <op>` lines
  and the successful provider-completion line moved from INFO to DEBUG; the
  request audit line, provider failures and journal events remain at INFO or
  above. `docs/operations/observability.md` records the default filter, the
  `RUST_LOG` override and the retention split: the packaged unit writes to
  journald, which owns rotation, and a file-capturing deployment must rotate
  its own stdout. The measured 2.57 GB file was an unrotated capture, not a
  journal or state file. **No log-volume bound is claimed** and no
  benchmark was re-run for this slice.
- **D2 — read posture (gate 3 limitation).** `AuthPolicy::protect_reads`
  now returns true whenever **any** credential is configured, including the
  legacy top-level `admin_token`. Anonymous reads of volumes, capabilities,
  metrics and migration inspection return `UNAUTHORIZED`; the four probe
  paths stay public; a tokenless daemon keeps the documented open-read
  loopback-dev posture and still fails closed for mutations. This is a
  security posture change in an unreleased prototype; read clients using
  only the legacy token must send it. The scoped-role matrix and tokenless
  tests pass.
- **D3 — #62 write path (slice 1 and slice 2 delivered; capacity-domain grow
  admission measured at `4290b84`; per-VG sample coalescing measured at
  `75beea3` and reverted at `59f64cc`; Phase E create/delete lock scope
  delivered and measured at `2c5b065`; create/delete worker handoff measured
  at `7204a53`; c128 grow tail
  open).** Slice 1 code is
  in the tree: the single durable-writer funnel
  and initial startup sweep (`f0e5237`); the bounded hashed temp namespace,
  hardened sweep and per-state-path in-process writer gate (`a8c23d6`);
  canonical-path keying of the temp-name hash (`a068947`); and test/doc-only
  evidence corrections (`913c347`, `aae0e09`, and the slice-1 review-evidence
  corrections through this loop). The `a8c23d6` hash was lexical, so
  `a068947` is what makes a symlink/`./`/`..` spelling share its predecessor's
  temp namespace. Between `a068947` and the slice-2 implementation
  (`8220414`), no production behavior changed: those commits are
  test/doc-only evidence correction. Slice 2 code
  (`8220414`, plus the review-fix commits `1184a27` and `c4b0b13`; the first
  carried the leadership-handoff and poison fixes, the second the admission
  window) releases the provider state lock for attach/detach/report and
  coalesces concurrent captures into one physical save behind a per-lineage
  writer, with permanent watermarks, admission alignment and RAII leadership.
  Admission now resolves an outstanding capture before deriving any replay,
  not-found or no-op answer (async without holding the state lock,
  synchronous via `lock_state_stable`), so a same-id replay can never be
  journaled as a terminal success from uncommitted state. The
  post-implementation reviews found and fixed a critical leadership-handoff
  race, a high phantom-resurrection path and a high same-id replay window;
  deterministic tests now cover all three. Synchronous
  create/grow/delete/move/admin still hold the lock and wait on a condvar
  (accepted residual; not a regression from slice 1). The clean-tree
  benchmark at `c4b0b13` is published in the
  [slice-2 bundle](../benchmarks/2026-11-04-devhost-slice2/REPORT.md)
  (worktree clean; comparator `aab773b`). It records an unchanged
  mixed-sweep ceiling (c1/c8/c32/c128 −5.5/−2.4/−0.5/+0.7 %) and a split
  latency result: read p95 falls sharply at c32/c128 while write p95 rises
  (c128 1552.261 → 6330.548 ms). The 25-minute soak recorded 6,929,746 ops
  at 4619.824 ops/s versus 5,424,744 at 3616.49, but RSS delta also rose;
  that is a single non-fixed-dataset observation, not a capacity claim. No
  throughput-gain claim is made for either slice. The earlier post-fix c32
  −4.1 % / c128 −2.6 % measurement predates slice 1 and is not evidence for
  either slice.
  **Grow-lock fix (2026-11-05, `8029df8`).** A grows-disabled diagnostic
  attributed the slice-2 write tail to `grow_volume` holding the provider
  state lock across `lvextend`. The fix releases the lock across the resize
  and size verification, keeps the durable grow intent as the recovery
  anchor (structural mutations refused typed while live, reports admitted),
  preserves capacity atomicity with an in-memory per-VG reservation, refuses
  a second concurrent grow of one volume, and makes the move reconcile pass
  defer only the growing record. The clean-tree re-measurement is published
  in the
  [slice-2 fix bundle](../benchmarks/2026-11-05-devhost-slice2-fix/REPORT.md)
  (comparator `c4b0b13`). c32 write p95 fell 590.847 → 206.203 ms (−65.1 %)
  and c128 6330.548 → 3413.886 ms (−46.1 %); throughput rose
  858.371 → 1830.005 ops/s at c32 (+113.2 %) and 237.088 → 440.376 at c128
  (+85.7 %). c1/c8 throughput fell 6.3 / 8.6 % and is not attributed. The
  c128 write tail remains about 2.2× the older `aab773b` baseline, so the
  write-tail work is **not** claimed complete; the 25-minute soak recorded
  6,640,950 ops at 4425.586 ops/s with 0 HTTP errors and 0 conflicts.
  **Capacity-domain grow admission and review fixes (2026-11-05,
  `a3ea5abf` + `6519068` + `4290b84`).** The grow capacity sample moved
  outside the provider state lock; an allocation epoch invalidates a sample
  that raced with any allocation; the per-VG reservation and durable grow
  intent publish under the state lock; a capped optimistic-retry budget
  returns a typed refusal; a per-volume drive guard refuses a duplicate
  grow. A same-commit A/B diagnostic led to removing the provider grow
  semaphore; the daemon request admission cap is the remaining **count**
  bound, shared with reads and attaches and not cross-checked against LVM
  capacity. An adversarial review then found and fixed two Medium defects at
  `4290b84`: a create attempt whose `lvcreate` failed or was adopted could
  spend capacity without bumping the epoch, and the grow publish discarded
  the state-lock result on its error path. The clean-tree re-measurement is
  published in the
  [capacity-domain fix bundle](../benchmarks/2026-11-05-devhost-capacity-domain-fix/REPORT.md)
  (comparator `8029df8`). Throughput rose 1,729.154 → 1,825.419 ops/s at c1
  (+5.6 %), 3,118.787 → 3,846.899 at c8 (+23.4 %), 1,830.005 → 2,609.257 at
  c32 (+42.6 %) and 440.376 → 655.326 at c128 (+48.8 %); the 25-minute soak
  recorded 7,134,530 ops at 4,756.348 ops/s with 0 HTTP/provider errors and
  no conflicts. c32 write p95 fell 206.203 → 82.927 ms; c128 read p95 fell
  87.276 → 6.584 ms. The c128 **aggregate** write p95 rose 3,413.886 →
  5,078.906 ms but is not like-for-like: it is a quantile of a bimodal
  mixture and flips between the non-grow and grow populations near the 5 %
  grow-share boundary. **The c128 grow tail is not fixed**; grow completions
  are per-VG-bound (255 → 438 grows, 8.43 → 11.12/s) and the queue
  deepened (p50 338 → 8,339 ms, p95 499 → 19,633 ms). A read-only
  [decision memo](../reviews/2026-11-05-grow-tail-decision.md) accepted this
  as the measured per-VG envelope and added no new bound; the envelope is not
  a validated ceiling or an SLO. A subsequent
  [non-evidence phase diagnostic](../reviews/2026-11-05-grow-tail-phase-diagnostic.md)
  corrected the attribution: the ~2 ms anchor is the `ops::execute` journal
  intent, while the provider capacity-admission loop is 5.0–6.3 s mean,
  dominated by `vgs` samples invalidated by the global capacity epoch and
  retried (783–1,082 stale attempts across 409–411 grows); `lvextend`/`lvs`
  are ~1.8–2.4 s each and the provider intent save is ~1.7–1.9 ms. Option C
  (per-round single-flight `vgs`) was implemented at `a07203e` and measured
  at `75beea3` in the
  [grow-sample-coalescing bundle](../benchmarks/2026-11-05-devhost-grow-sample-coalescing/REPORT.md).
  **The measurement is a c128 regression**: 655.326 → 508.225 ops/s
  (−22.45%), grow p50 8,338.505 → 11,335.103 ms, soak −4.48%, while grow
  p95/max narrowed. It was not adopted on throughput or envelope grounds and
  was reverted locally at `59f64cc` (code-only; the tree matches `4290b84`
  for `crates/volvisor-lvm`), so `4290b84` is again the current measured
  envelope. Two clean same-SHA
  repeats (non-evidence) measured c128 599.405 and 614.158 ops/s and
  aggregate write p95 7,478.820 and 7,586.304 ms, so the published sample is
  at the high end of the observed range.
  **Phase E create/delete lock scope (2026-11-05, E1 `8cfa4c2`, E2
  `2c5b065`).** Delete now saves its durable intent under the provider state
  lock, drops the guard for `blkdiscard`/`lvremove`, and reacquires it only
  to revalidate the exact intent and remove the record. Create saves its
  durable intent under the lock, runs `lvcreate` and its size-verifying
  `lvs` without it, and reacquires it for the final record commit. A create
  takes the existing per-VG capacity reservation under the lock before
  releasing it; a create/delete structural drive map refuses a concurrent
  mutation of the same volume. Deterministic gate tests park
  `lvcreate`/`lvremove` and fail on the pre-change provider; workspace fmt,
  clippy and the full test suite pass at `2c5b065`. The clean-tree
  re-measurement is published in the
  [Phase E bundle](../benchmarks/2026-11-05-devhost-phase-e-write-lock-scope/REPORT.md)
  (comparator `4290b84`). **The measurement is a mixed-throughput
  regression**: c8 3,846.899 → 3,756.659 ops/s (−2.35%), c32 2,609.257 →
  2,476.245 (−5.10%), c128 655.326 → 610.988 (−6.77%), c1 −0.14%; the
  25-minute soak fell 7,134,530 → 6,626,734 ops (4,756.348 → 4,417.815
  ops/s, −7.12%). c128 overall p95 fell 117.870 → 100.558 ms and read p95
  6.584 → 4.691 ms, but the aggregate write p95 (5,078.906 → 4,297.266 ms)
  is a grow-population quantile in both runs and grow p50/p95 rose; it is
  not a like-for-like gain. Micro create/delete p50 rose 5.5%/7.1% in one
  sample (neutral/unattributed). The sweep and soak contain no
  create/delete calls, so none of these deltas is attributed to E1/E2; E3
  does not support a Phase E throughput or latency gain. Residuals: delete's
  `lvs` check and create's `pick_pool_vg` `vgs` sample remain in their
  admission critical sections, and the failed-create compensation's
  `lvremove` remains in the reacquired critical section. At `2c5b065` create
  and delete also still ran their synchronous LVM bodies inline on async
  workers (only grow used the blocking handoff; move's request path runs its
  CLI calls inline); that statement is correct for the E3 measured SHA.
  **Phase E create/delete worker handoff (2026-11-05, `7204a53`).** Both
  wrappers now use the existing `run_blocking_body` `block_in_place` handoff,
  keeping the inline fallback for current-thread runtimes and `LocalSet`;
  move's request-path CLI calls remain inline. Gate tests now exercise both
  same-kind and cross-kind refusal and prove the create capacity reservation
  is released on completion (a 4 MiB create in the 8 MiB remaining), each
  with a targeted negative control. The clean-tree re-measurement is in the
  [worker-handoff bundle](../benchmarks/2026-11-05-devhost-phase-e-worker-handoff/REPORT.md)
  (comparator `2c5b065`): create p50/p95 flat in both same-SHA samples;
  delete +0.12%/+0.28% in the measured sample but +6.34%/+5.90% in an
  explicitly non-evidence same-SHA repeat, so delete overhead is unresolved;
  c1/c8 +0.82%/+1.81%, c32 −1.61%, c128 −24.24%. The timed sweep and soak
  contain no create/delete and the binary differs only in the two wrappers,
  so the c128 drop is **not attributed** to this slice; a non-evidence
  alternating A/B measured c128 427.5–626.0 ops/s across the four runs of
  both arms (A 439.5–626.0, B 427.5–432.8), so the tier is
  variance-dominated on this host. No throughput gain and no zero-regression
  claim is made.
  **#62 remains open** until explicitly instructed, and no gate
  verdict changes. No live evidence changed in this batch: `d5.sh` Part B was
  not re-drilled at this SHA here, so its `.volvisor-` message assertion is
  code-derived until the next drill, and the symlink test panics rather than
  skips where the temp filesystem refuses symlinks.
- **D4 — ADR-0012.** Accepted on 2026-11-06 by zoorpha; this does not change
  the Phase D evidence.

No gate becomes met. Gates 1, 2 and 5 remain **NOT MET**, gates 3 and 4
remain **PARTIAL**, and the overall status remains **NOT PRE-PRODUCTION
READY**. No tag, GitHub release, push or pull request is created by this
workstream.

## Post-report addendum (2026-11-06) — Phase F write-path remainder and #63/#64 verification

This addendum records the Phase F batch delivered on `phase-a-scaling`
after the Phase E work recorded in the Phase D addendum. It follows the
[Phase F plan](../plans/2026-11-06-phase-f-write-path-remainder.md).
**It changes no gate verdict below.** It is a single-host development-lab
control-plane measurement with a shortened 25-minute soak, not the 24-hour
soak; it is not production, data-path, SLA, durability or capacity
evidence.

- **Exact source and build.** `2edcecd9b3d4c555831e6d4db1337e3dbcff6a15`
  (`volvisord 2edcecd`), clean worktree (`worktree_dirty: no`), release
  binary SHA256
  `a09ec03d8157a527b17c3a7111cee62522cc711ab6b42b93d6b697ddff2c2e7a`,
  built with `CARGO_BUILD_JOBS=4 cargo build --release --locked -p
  volvisord` (exit 0). Measured window 2026-11-06T12:20:58Z ..
  2026-11-06T12:52:21Z on one dev host (16 vCPU AMD EPYC 9554P, 31.3 GiB
  RAM, kernel `6.8.0-142-generic`, loop-backed LVM, every client and the
  daemon on `127.0.0.1`; no Ceph, DRBD, `fio` or second host).
- **Implementation.** F1 proves the move drive's **existing** lock scope
  (it is not a new narrowing). F2a delete validates under the lock, runs
  `lvs` outside it, fully revalidates generation/VG/LV/lifecycle/pending-
  grow/unsettled-move, then persists the intent. F2b create samples `vgs`
  outside the lock under the allocation-epoch protocol, bounded by
  `CREATE_ADMISSION_ATTEMPTS = 32`, with a post-reserve epoch re-check and
  `SampleStale` retry. F2c failed-create compensation classifies the
  durable file under the lock, reverts uncommitted memory, drops the guard
  for `lvremove`, then reacquires to roll back/clear the intent. Accepted
  residuals: the failure-only durable re-read in `persist_copying_record`
  stays under the lock; startup `apply_delete_intents` runs the delete CLI
  under the startup lock; move's CLI calls remain inline on async workers.
- **Test evidence.** `cargo fmt --all --check` exit 0; `cargo clippy
  --workspace --all-targets --locked -- -D warnings` exit 0; `cargo test
  --workspace` exit 0 with 78 `test result:` lines, **1,378 passed, 0
  failed, 3 ignored** (65 test binaries + 13 doc-test binaries). All seven
  Phase F gate tests pass individually:
  `move_releases_the_state_lock_across_lvs_observation`,
  `move_releases_the_state_lock_across_pvmove_start`,
  `delete_releases_the_state_lock_across_lvs_existence_check`,
  `delete_refuses_a_volume_that_changed_during_the_lvs_check`,
  `create_releases_the_state_lock_across_vgs_pool_sample`,
  `a_create_re_samples_when_the_allocation_epoch_moves` and
  `create_compensation_releases_the_state_lock_across_lvremove`. All ten
  #63 tests and the listed #64 retention/compaction/tombstone/config tests
  pass. F1/F2 negative controls were reproduced during implementation
  before commit; the F3 run only re-confirms the committed gate tests
  pass, so it is not crash injection.
- **Bundle.** [Phase F write-path remainder bundle](../benchmarks/2026-11-06-devhost-phase-f-write-path-remainder/REPORT.md).
  Both source manifests verified; the published measured `json/` is
  byte-identical to the source run (`diff -rq` exit 0). The raw
  orchestration/daemon logs and per-operation CSVs are intentionally not
  committed (see the bundle's Integrity note).
- **Micro (descriptive only).** create p50 **195.561 → 205.42 ms
  (+5.04%)**, p95 **215.488 → 225.682 (+4.73%)**; delete p50 **154.685 →
  174.612 ms (+12.88%)**, p95 **174.961 → 185.304 (+5.91%)**. The same-SHA
  non-evidence repeat measured create p50/p95 215.203/235.718 ms and
  delete 174.623/194.684 ms. No A/B at `7204a53` was run in this batch, so
  this is **not attributed** to Phase F and is neither a regression nor a
  gain.
- **Sweep (not attributed).** c1 **1,837.885 → 1,826.312 ops/s (−0.63%)**,
  c8 **3,824.485 → 3,743.924 (−2.11%)**, c32 **2,436.350 → 2,311.046
  (−5.14%)**, c128 **462.899 → 419.873 (−9.29%)**. Measured p95
  1.451/7.401/45.273/138.175 ms; p99 1.56/11.478/93.864/**11,800.496** ms.
  c128 completed 18,233 ops in **43.425 s** against a nominal 30 s tier
  (non-evidence repeat 42.676 s, p99 12,523.519 ms). The timed sweep has no
  create/delete/move and Phase F only touches those paths, so no delta is
  **attributed**; no throughput or zero-regression claim.
- **Soak (not attributed).** 1,500.003 s, 4 workers, **7,040,538 ops**,
  **4,693.683 ops/s** (success 4,693.645), error rate **8e-06** (58
  `NETWORK_ERROR`: 55 `ConnectionRefusedError`, 3 `RemoteDisconnected`, at
  the planned restart), p50/p95/p99 **0.304/2.864/3.606 ms**, actual read
  ratio **0.7999** vs target 0.6. Prior `7204a53`: 7,112,020 ops at
  4,741.343 ops/s (−1.005% derived). The soak contains no create/delete;
  the difference is **not attributed**.
- **Resources and restart.** RSS first/last **75,716 → 2,668,132 KiB**,
  high-water **2,762,300 KiB** (prior 2,842,396 KiB, −2.82% derived); fds
  21 → 18, max 25; journal first/last **17,801,776 → 25,563,609 B**, delta
  7,761,833 B, high-water **75,497,904 B** (prior 75,497,758 B); state
  **3,413 → 2,977 B**, high-water 4,083 B. The journal high-water is
  consistent with the documented post-compaction growth floor above the
  64 MiB trigger, not a new unbounded-log finding and not a hard-cap claim;
  the exact peak composition is not separately instrumented. Restart: pre-soak
  0.102–0.103 s (3 rounds), mid-soak **0.425 s with 4/4 volumes and none
  missing**, post-soak 0.366/0.364 s with 0 volumes after cleanup; all
  `OK`. Daemon log 0 `ERROR` and 1,553 `WARN`, all the known
  grow-notification retry for attachments without a `vmm_disk_id`.
- **#63 findings (not fixes).** Schema v3 at
  `crates/volvisor-lvm/src/state.rs:98`; the 8 KiB-bit filter at `:107` is
  decode-only; the legacy vector is imported at `:416-439` but never written
  or consulted. The fence is the strictly increasing per-volume attachment
  generation (`crates/volvisor-lvm/src/provider.rs:3812-3816`, `:3821`,
  `:3832`, `:3894-3898`; `crates/volvisor-provider/src/semantics.rs:232-235`
  refuses `STALE_GENERATION`). Residuals:
  per-volume-lifetime generation-1 reset across delete + same-id recreate
  (`provider.rs:3406`, `:5111`), so a delayed report could match generation
  1 if the same `attachment_id` is reused and no test covers it; the legacy
  wire vector is unbounded at decode; a non-empty legacy filter is
  preserved on save (up to 1 KiB/volume) though never consulted.
  ADR-0012 is **Accepted** (2026-11-06, zoorpha).
- **#64 findings (not fixes).** Retention/compaction/tombstone code is
  present and tested
  (`crates/volvisor-journal/src/journal.rs:37,40,43,46,56,64,74,1412-1417,
  1448-1481,1029-1062,853-869,1073-1124,1743-1753`; config wiring
  `crates/volvisord/src/config.rs:178-223,1374-1401`,
  `crates/volvisord/src/runtime.rs:76`,
  `examples/volvisor.toml:112-116`). Residuals: in-flight intents are never
  evicted; "expiry" is a count window, not a TTL; a persistent compaction
  failure backs off but leaves the log unbounded (metric only); the
  terminal registry may reach 2x `max_retained_operations` between
  compactions and the one-frame planner can reduce effective retention, so
  configured bounds are soft; a 64-bit fingerprint collision can answer
  `OPERATION_EXPIRED`; and the observed journal high-water above the
  trigger is consistent with the documented post-compaction growth floor,
  not a hard cap.
- **Non-evidence repeat.** A same-SHA `--skip-soak` repeat exists at
  `diagnostics/same-sha-repeat/` and is explicitly **NON-EVIDENCE**. It
  was not used to select, replace or support the measured sample and
  supports no performance claim.
- **No gate change.** `#62`, `#63` and `#64` stay **open**; ADR-0012 is
  **Accepted** (2026-11-06, zoorpha); gates 1/2/5 remain **NOT MET**,
  gates 3/4 **PARTIAL**, and the overall status remains **NOT
  PRE-PRODUCTION READY**. The evidence record includes no push, tag, pull
  request or issue-state change; the branch itself was pushed earlier as WIP
  under explicit authorization, and that push is not evidence.

## Post-report addendum (2026-11-06) — durable-semantics decisions and the default scenario gate

This addendum records the architecture decisions and the enforceable
regression gate created from the Phase F findings. It follows the
[Phase F addendum](#post-report-addendum-2026-11-06--phase-f-write-path-remainder-and-6364-verification).
**It changes no gate verdict below and closes no issue.**
It is a documentation, CI and test-enforcement batch: it changes no
provider, journal or daemon behavior, so it does not alter the semantics of
a running system or any existing durable file.

- **Decision acceptance.** ADR-0012, ADR-0013, ADR-0014, ADR-0015 and
  ADR-0016 are **Accepted** (2026-11-06, zoorpha). Acceptance covers the
  decisions only: ADR-0013's durable allocator, ADR-0014's hard ceiling and
  cannot-fit refusal, and the scenarios still owed below are not
  implemented by it. `#62`, `#63` and `#64` remain open.
- **ADR-0013 — durable attachment-generation allocator (Accepted;
  implementation pending).** Records the `#63` finding that the generation
  fence is per volume lifetime: `create_volume` writes generation `1` and
  delete removes the record, so a delete + same-id recreate restarts at
  `1`. The decision is one durable
  monotonic `next_attachment_generation: u64` in LVM state (schema v4), never
  reset, seeded from the maximum persisted volume/attachment generation on v3
  decode, failing closed at `u64::MAX`. The one residual the v3 file cannot
  close is a volume deleted before the upgrade: its consumed generations are
  gone, so a delayed report from that pre-upgrade incarnation can still match
  during the upgrade window. **Not implemented.**
- **ADR-0014 — journal bounds and exhaustion contract (Accepted;
  hard-ceiling implementation pending).** Records that terminal retention is
  bounded and in-flight intents are never evicted, but expiry is a count
  window, configured values are soft targets, and a persistent compaction
  failure can still grow the log. The decision adds a
  hard ceiling above the byte trigger with a typed `JOURNAL_FULL`/503 refusal
  while reads and replay continue, plus refusal metrics; it also requires
  refusing new mutating work when in-flight intents cannot be snapshotted.
  **The hard ceiling, the cannot-fit mutation refusal and their metrics are
  not implemented.**
- **ADR-0015 — write-path critical-section invariant (Accepted; enforced by
  the default catalog).** Records the Phase E/F invariant: the provider state
  lock may span validation,
  reservation, durable save and commit, but never an external `lvcreate`,
  `lvremove`, `lvs`, `vgs`, `pvs`, `pvmove` or `lvextend` on a request path.
  The durable intent plus revalidation is the cross-command fence. The
  failure-only durable re-read that classifies a failed save, the
  single-threaded startup recovery lock, and move's inline CLI worker
  handoff remain accepted exceptions/residuals. This proves the existing
  lock scope; it is not a new narrowing.
- **ADR-0016 — semantic-change and default-scenario gate (Accepted; gate
  enforced in CI).** Requires any durable-semantics change to carry an ADR
  or amendment, a
  compatibility/migration and rollback statement, a fail-closed refusal path,
  an exact-name default scenario, rollback/replay evidence and a green
  default suite before merge.
- **Default scenario catalog.** `docs/testing/default-integration-scenarios.md`
  is the normative catalog and `docs/testing/default-integration-scenarios.tsv`
  is its machine-readable list: 58 exact tests across write-path lock scope
  (WP), fail-closed create/delete and idempotency (FC), attachment
  generation/fence (GF), schema decode (SC), journal retention/replay (JR)
  and durable configuration bounds (CF). `scripts/test-default-scenarios.sh`
  caches `cargo test -- --list`, asserts each exact test exists and is
  documented, then runs it with `--exact --test-threads=1` and requires
  exactly one passing and zero ignored tests. It is time-bounded so acceptance
  stays fast: **60 s per test and 300 s total warm execution** (compilation
  and test-binary listing are outside the budget), and a test over either
  bound fails the gate. Soaks, real-backend suites, campaign tiers and fault
  campaigns are not admitted to this catalog. The `check` CI job runs it
  after `cargo test --workspace --locked`; `test-harness` adds `bash -n` and
  `shellcheck` coverage. The Phase F batch reported **54 passed in 9.0 s
  warm** (slowest 1.4 s); the ADR-0013/0014 addendum records the current
  58-scenario catalog. Timings are a measured baseline, not a CI SLA.
- **New scenarios still owed by the ADRs.** ADR-0013's delete + same-id
  recreate fence (`GF-12`) and ADR-0014's hard-ceiling refusal (`JR-11`,
  `JR-12`) are not yet implemented and therefore not in the catalog. ADR-0014
  also owes a cannot-fit refusal scenario (amending `JR-04`, which pins the
  current append-never-fails behavior for a failed compaction). They must be
  added with their implementations before those ADRs can be considered met.
- **No gate change.** ADR-0012, ADR-0013, ADR-0014, ADR-0015 and ADR-0016
  are **Accepted** (2026-11-06, zoorpha); acceptance does not implement the
  owed work or close issues. `#62`, `#63` and `#64` remain **open**.
  Gates 1/2/5 remain **NOT MET**, gates 3/4 **PARTIAL**, and the overall
  status remains **NOT PRE-PRODUCTION READY**. This batch creates no
  production-support, SLA, zero-RPO, durability, capacity, hardware,
  multi-host, release or 24-hour-soak claim, and no push, tag, pull request
  or issue-state change is part of the evidence record.

## Post-report addendum (2026-11-06) — ADR-0013/0014 local implementation

This addendum supersedes the “New scenarios still owed” bullet in the Phase F
addendum above. The local decision work for `#63` and `#64` is now
implemented and pinned; **no readiness gate changes and neither issue
closes**.

- **ADR-0013 / `#63` — durable attachment-generation allocator (landed
  locally).** LVM state is schema v4 with a persisted provider-wide
  `next_attachment_generation` and per-volume high-water marks. A v3 file
  seeds the allocator from `max(entry, attachment) + 1` only when the field is
  absent/zero; every save re-normalizes it; allocation at `u64::MAX` fails
  closed; create/delete never reset it. Default scenarios `GF-12` (delete +
  same-id recreate never reissues a retired generation) and `SC-09` (v3
  fixture seeds and upgrades to v4) are in the catalog. **Residual:** a volume
  deleted *before* the v4 file was written has no durable record of its
  consumed generations, and no real-backend/multi-host evidence has been
  collected. `#63` stays open.
- **ADR-0014 / `#64` — journal hard ceiling and cannot-fit refusal (landed
  locally).** `hard_ceiling_bytes` is configurable (default `2 *
  max_log_bytes`; explicit non-zero when the byte trigger is disabled; at
  least the trigger). A durable append that would cross it gets one forced
  compaction attempt and is then refused with typed `JOURNAL_FULL` (HTTP 503)
  without evicting in-flight intents, terminal records or tombstones. A
  cannot-fit in-flight/pinned set closes **new-intent** admission while outcome
  appends drain it; a later successful compaction reopens admission. The new
  journal gauges/counters are on `/metrics`, and outcomes classify
  `journal_full` distinctly from append I/O failure. Default scenarios `JR-11`
  and `JR-12` are in the catalog; `JR-04` still pins “a failed compaction alone
  never fails an already-authorized append that fits under the ceiling”.
  **Residual:** no real-backend crash-consistency or 24-hour soak evidence.
  `#64` stays open.
- **Catalog.** The default gate now has **58 exact scenarios** and still
  enforces 60 s per test / 300 s warm total. The measured baseline in this
  batch is 58 passed; timings are a measured baseline, not a CI SLA.
- **No gate change.** `#62`, `#63` and `#64` remain **open**. Gates 1/2/5
  remain **NOT MET**, gates 3/4 **PARTIAL**, overall **NOT PRE-PRODUCTION
  READY**. This batch creates no production-support, SLA, zero-RPO, capacity,
  hardware, multi-host, release or soak claim.

## Gate verdicts

| # | Gate | Verdict | What is missing |
|---|---|---|---|
| 1 | **Evidence** — three physical hosts at an exact SHA with the fault matrix and a published bundle; Rook-cell GO/NO-GO; Ceph and DRBD suites in a gating rig | **NOT MET** | No three-physical-host campaign (#37), no Rook-cell execution (#38), no green Ceph/DRBD rig run (#39). Only single-host Tier V with simulated storage, single-host day-2 drills, and single-host loop-LVM benchmarks exist. |
| 2 | **Release** — signed `v0.1.0` (or later) with deb/rpm, SHA256SUMS, install smoke, and a support matrix matching the evidence | **NOT MET** | No signing key on this host, no signed tag, no GitHub release, and no clean-host install from a tagged artifact (#47). The support matrix itself is accurate and makes only non-claims. |
| 3 | **Security** — explicit transport/auth model for non-loopback deployment; authenticated/encrypted peer and witness traffic; scoped credentials and audit identity | **PARTIAL** | #40 landed native TLS/mTLS, HTTPS witness/peer clients, refusal of plain non-loopback HTTP without `allow_insecure`, scoped credentials, read authorization and audit attribution. The recorded capture is **same-host netns/veth only**; cross-host witness/peer hops, live rotation, and HSM/TPM are not run. Reads now require any configured credential, including the legacy `admin_token`; only the tokenless posture leaves them open. |
| 4 | **Operability** — backup/restore, upgrade/rollback, witness loss, and journal corruption drilled with recorded results | **PARTIAL** | D1 backup/restore, D4 journal corruption, D5 ENOSPC, D6 crash loop/lock and D7 restart-based credential rotation were drilled on one host. D2 now includes a real single-host cross-binary v2→v3 upgrade and restore-based rollback ([bundle](../evidence/day2/2026-11-04-devhost-live/REPORT.md)), but no rolling multi-host or packaged upgrade exists. D3 witness loss is partial (service outage/restart only; no storage-side DRBD fencing). D8 total host loss is **not run**. |
| 5 | **Consumer** — CHV negotiates the capability and reports `advertised` against a pinned release, never `active` without guest evidence | **NOT MET** | The Volvisor-side CHV contract, conformance test and integration guide are delivered, but there is no CHV repository change and no pin update. A pin requires a tagged Volvisor release, which does not exist. CHV's ceiling remains `advertised`; no `active`/guest-confirmed behavior is claimed. |

**Verdict:** the Wave 2 P1 hardening work landed, but the five-gate definition
is unchanged and still requires P0 real-hardware, release and consumer work
that this environment cannot produce. Do not describe this tree as
pre-production, production-ready, or a release candidate.

## Wave 2 delivery record

| Workstream | State | Merge / evidence | Limits that remain |
|---|---|---|---|
| #41 schema evolution | Landed | `712afb7` | — |
| #40 transport/auth | Landed | `ad9efb0` | Same-host capture; cross-host validation open |
| #42 provider consolidation | Landed | `db04182` | Parity suite uses simulated Ceph/DRBD worlds; no real clusters |
| #45 admission control | Landed | `0c853f3` | No resumable operation handle/long-poll (synchronous journaled mutations); read timeout is a fallback with optional per-route overrides; rate limiting is opt-in |
| #46 day-2 runbooks | Landed | `8de06c2`; [bundle](../evidence/day2/2026-11-03-devhost/REPORT.md) | At that merge: 5 drilled, 2 partial, 1 not run; single host; bundle source `e551d26` predates #42/#45/#44. Superseded by the 2026-11-04 addendum: D1 was re-run against the v3 tombstone filter and D2 live is now drilled single-host; D3 storage-side and D8 remain **not run**. D5 filed #58, now fixed by #69 and re-drilled post-fix at harness `940722e` (both parts); the three non-injected branches are test-covered; #70 persist contracts are implemented and deterministically test-covered (not hardware-drilled), documented in `lvm-state-divergence.md` |
| #44 observability | Landed | `5017ea0`; [docs](../operations/observability.md) | State-save duration and lease/epoch gauges deliberately omitted; real migration trace **not run** |
| #43 benchmarks | Landed | `575acfc`; [bundle](../benchmarks/2026-11-03-devhost/REPORT.md) | Single host, loop-backed LVM, control plane only; 25.0 min soak, **not 24 h**; no real Ceph/DRBD/physical-disk LVM |
| #49 harness flake | Partial | `5037db9`; #49 slice in this change | LVM mid-move abort flake fixed; four fuzz targets, a committed seed corpus with deterministic regression tests, journal pin/compaction and retained-listener interleaving regressions, and bounded nightly fuzz/flake workflows are delivered. The **14 consecutive clean nightly campaign runs and published flake rate are still not recorded** |

The detailed workstream state is on
[#36](https://github.com/o3kio/volvisor/issues/36). The measured benchmark
source is `e3aa365`; it remains an ancestor of `main` for inspection.

## Knowledge-concentration workstream (#50)

This section records the documentation and ownership work for
[#50](https://github.com/o3kio/volvisor/issues/50) as of **2026-11-04**. It
changes no gate verdict and is **not** a release, restore or readiness
sign-off.

| #50 item | State | Evidence / limit |
|---|---|---|
| `ARCHITECTURE.md` | **Delivered** | [ARCHITECTURE.md](../../ARCHITECTURE.md): 13-crate map and dependency direction, Volume API v2 request flow, witness/peer/migration paths, invariants, crash/failure model, authoritative state locations and the AGENTS rule 12 non-production statement. Links to the v2 ADRs, SPEC-0002 and the v2 contracts. |
| Ownership map / `CODEOWNERS` | **Structural coverage delivered; per-area assignment outstanding** | [`.github/CODEOWNERS`](../../.github/CODEOWNERS) has an explicit pattern for every one of the 13 workspace crates plus contracts, docs, CI, scripts, tests, packaging and examples. The current write-access collaborators are listed as provisional coverage owners. The repository still records no per-area primary/backup owner; `@o3kio` is an organization and cannot be a CODEOWNERS owner by itself. |
| Contributor guide | **Delivered** | [CONTRIBUTING.md](../../CONTRIBUTING.md): build and CI gates, the real test tiers (workspace, conformance, LVM, Ceph, DRBD, campaign S/R/V), fake versus privileged suites, the four fuzz targets and their corpus regression, hardware prerequisites, review checklist, commit/PR expectations and the schema-change procedure linked to [schema-compatibility.md](../schema-compatibility.md). |
| Bus-factor risks and mitigations | **Delivered** | [docs/bus-factor.md](../bus-factor.md): risk register, delivered structural mitigations, outstanding organizational actions and an explicit "what is not a mitigation" section. |
| Two maintainers independently complete a release and a restore drill | **OUTSTANDING** | No two-person sign-off, names, dates or independent results are recorded. The existing [day-2 drills](../runbooks/day-2/README.md) are single-host and partly partial; D8 host loss is not run. This depends on #46 and #47 and is not satisfied by the existing bundle. |
| No undocumented "ask person X" step in release or recovery | **NOT VERIFIED** | The commands and fail-closed rules are in-tree, but the release/restore paths still depend on a small set of people and have not had an independent operator review. Treat this acceptance criterion as open. |

Related assurance gap, not a #50 checklist item: [#49](https://github.com/o3kio/volvisor/issues/49)
requires **no open campaign flake for 14 consecutive nightly runs** and a
published flake rate. The fuzz targets, committed corpus/regression tests and
deterministic interleaving tests are delivered in this change, and bounded
nightly fuzz/flake workflows collect samples, but the 14-night evidence and
published flake rate **do not exist yet**. This report does not claim that
flake evidence.

**No gate verdict changes:** gates 1, 2 and 5 remain **NOT MET**, gates 3 and
4 remain **PARTIAL**, and the overall status remains **NOT PRE-PRODUCTION
READY**.

## Evidence inventory

| Evidence | Source | Scope | Proves | Does not prove |
|---|---|---|---|---|
| [Tier V, CH v51.1](../evidence/tier-v/2026-10-11-devhost-v51/) | `1762a9d` | One physical development host; storage below the VMM seam simulated | Four bounded VMM scenarios at recorded VMM version/SHA256 | Real media, real DRBD, multiple hosts, production durability |
| [Tier V, CH v43.0.0](../evidence/tier-v/2026-10-10-devhost/) | recorded in bundle | One physical development host; simulated storage | The same scenarios on the older VMM; fail-closed grow-retry branch | Grow notification on v43, real-host durability |
| [Transport/TLS same-host](../evidence/transport-tls/2026-11-02-devhost-netns/) | base `a6591d9`, dirty tree | One kernel; netns/veth peers | TLS/mTLS, untrusted-cert rejection, insecure-bind refusal, encrypted pcap, audit attribution | Cross-host witness/peer hop, live rotation, HSM/TPM |
| [Day-2 drills, 2026-11-03](../evidence/day2/2026-11-03-devhost/REPORT.md) | `e551d26`, clean worktree | One host | D1/D4/D5/D6/D7 drilled; 181-entry manifest verifies | D2/D3 full paths, D8 host loss, live upgrade/rollback in that bundle, DRBD fencing, multi-host |
| [Day-2 D1/D2, 2026-11-04](../evidence/day2/2026-11-04-devhost-live/REPORT.md) | old `8c2fc1a` / new `aad1720`, caller dirty with clean new build worktree | One host; loop LVM; debug builds | D1 v3 backup/restore with bounded tombstone filter and behavioral retired-id 409 (pre-ADR-0012); real old→new v2→v3 upgrade, typed old refusals, byte-identical v2 restore and old-binary serve; same-binary v3 policy | Rolling multi-host or packaged upgrade, data path, D3 storage-side fencing, D8 host loss, production support |
| [D5 post-fix re-drill](../evidence/day2/2026-11-03-d5-redrill/REPORT.md) | `1863a7a` product / `940722e` harness, clean worktree | One host; loop LVM; 256 KiB tmpfs ENOSPC | The normal uncommitted state-save path fails closed and converges: 500 with ENOSPC and the state `.tmp` path, byte-identical old state, LV compensated, 404 in the same process and after restart, terminal same-`operation_id` replay, new-`operation_id` 200/`Ready`, one LV for the new volume and two durable records total, no residue; journal path re-confirmed | The three non-injected tails (cleanup failure, post-rename carve-out, unreadable durable re-read), real disk/inode/thin-pool failure, multi-host, production readiness |
| [Control-plane benchmarks](../benchmarks/2026-11-03-devhost/REPORT.md) | `e3aa365`, clean worktree | One host, loop-backed LVM, loopback HTTP | 1/8/32/128 sweep, 25-min soak, restart replay, 46-entry manifest verifies | Data path, real backends, 24 h soak, production capacity or SLA |
| [Control-plane benchmark, D3 slice 2, 2026-11-04](../benchmarks/2026-11-04-devhost-slice2/REPORT.md) | `c4b0b13`, clean worktree | One host, loop-backed LVM, loopback HTTP | Clean-tree 1/8/32/128 sweep and 25-min soak after releasing the provider state lock; read/write latency split recorded; no throughput-gain claim | Data path, real backends, 24 h soak, production capacity/SLA, throughput improvement |
| [Control-plane benchmark, D3 slice-2 grow-lock fix, 2026-11-05](../benchmarks/2026-11-05-devhost-slice2-fix/REPORT.md) | `8029df8`, clean worktree | One host, loop-backed LVM, loopback HTTP | Clean-tree 1/8/32/128 sweep and 25-min soak at the grow-lock fix; c32 write p95 590.847 → 206.203 ms and c128 6330.548 → 3413.886 ms; c32/c128 throughput +113.2 / +85.7 % versus the regression run | Data path, real backends, 24 h soak, production capacity/SLA, a completed c128 write-tail fix, a blanket throughput gain |
| [Capacity-domain grow admission, 2026-11-05](../benchmarks/2026-11-05-devhost-capacity-domain-fix/REPORT.md) | `4290b84`, clean worktree | One host, loop-backed LVM, loopback HTTP | Clean-tree 1/8/32/128 sweep and 25-min soak; throughput +5.6 / +23.4 / +42.6 / +48.8 % (c128 completion-inclusive and at the high end of a same-SHA repeat range); c32 write p95 206.203 → 82.927 ms; c128 read p95 87.276 → 6.584 ms; c128 aggregate write p95 is a grow value and is not comparable head-to-head | Data path, real backends, 24 h soak, production capacity/SLA, a fixed c128 grow tail, a blanket throughput gain |
| [Per-VG grow capacity-sample coalescing, 2026-11-05](../benchmarks/2026-11-05-devhost-grow-sample-coalescing/REPORT.md) | `75beea3` (code-identical to `a07203e`), clean worktree | One host, loop-backed LVM, loopback HTTP | Clean-tree 1/8/32/128 sweep and 25-min soak after per-VG single-flight `vgs`; c128 throughput 655.326 → 508.225 ops/s (−22.45 %), grow p50 8,338.505 → 11,335.103 ms while p95 19,633.371 → 12,396.977 ms, soak −4.48 %; measured regression, reverted locally at `59f64cc` | Data path, real backends, 24 h soak, production capacity/SLA, a throughput or envelope improvement, an adopted change |
| [Phase E create/delete lock scope, 2026-11-05](../benchmarks/2026-11-05-devhost-phase-e-write-lock-scope/REPORT.md) | `2c5b065` (E1 `8cfa4c2`, E2 `2c5b065`), clean worktree | One host, loop-backed LVM, loopback HTTP | Clean-tree 1/8/32/128 sweep and 25-min soak after releasing the state lock across `lvcreate`/`lvremove`; deterministic create/delete gate tests; c8/c32/c128 −2.35/−5.10/−6.77 %, soak −7.12 %; sweep and soak contain no create/delete, so no delta is attributed to E1/E2 | Data path, real backends, 24 h soak, production capacity/SLA, a Phase E gain, a zero-regression result, concurrent create/delete load |
| [Phase E create/delete worker handoff, 2026-11-05](../benchmarks/2026-11-05-devhost-phase-e-worker-handoff/REPORT.md) | `7204a53`, clean worktree | One host, loop-backed LVM, loopback HTTP | Clean-tree 1/8/32/128 sweep and 25-min soak after routing create/delete bodies through `run_blocking_body`; create p50/p95 flat in both same-SHA samples, delete flat in the measured sample but +6.34 %/+5.90 % in an explicitly non-evidence repeat; gate tests for cross-kind refusal and reservation release with negative controls; c128 −24.24 % measured but not attributed (timed load has no create/delete; non-evidence A/B spans 427.5–626.0 ops/s across four runs of both arms, A 439.5–626.0 and B 427.5–432.8) | Data path, real backends, 24 h soak, production capacity/SLA, a handoff gain, a zero-regression result, concurrent create/delete load, a resolved delete overhead |
| [Phase F write-path remainder, 2026-11-06](../benchmarks/2026-11-06-devhost-phase-f-write-path-remainder/REPORT.md) | `2edcecd`, clean worktree | One host, loop-backed LVM, loopback HTTP | Clean-tree 1/8/32/128 sweep and shortened 25-min soak at the Phase F SHA; deterministic lock-scope gate tests for move/delete/create/failed-create compensation; #63 schema-v3/decode-only filter + generation-fence tests and #64 retention/compaction/tombstone/config tests pass (code present, issues not closed); c1/c8/c32/c128 −0.63/−2.11/−5.14/−9.29 % and soak −1.005 % measured but **not attributed** because the timed load has no create/delete/move (c128 p99 11,800.496 ms and 43.425 s duration recorded); RSS high-water 2,762,300 KiB and journal high-water 75,497,904 B recorded | Data path, real backends, 24 h soak, production capacity/SLA, a Phase F throughput or zero-regression result, that #62/#63/#64 are fixed |
| CI real-LVM gate | `575acfc` | GitHub-hosted runner, loop device | `native-local` suite: 6/6 integration tests, unit suites, clippy/fmt | Real NVMe, Ceph, DRBD, multi-host |

The benchmark bundle records 1/8/32/128 in-flight throughput of
**1665.787 / 2293.7 / 899.759 / 241.716 ops/s** with p95 **1.77 / 6.454 /
191.244 / 1251.564 ms** and zero errors. The shortened 25.0-minute soak ran
1,202,500 operations at 801.656 ops/s; RSS grew **+607.1 MiB**, the journal
**+215.0 MiB**, provider state **+2.4 MiB**, and fds stayed flat (19 → 18).
Those growth signals are filed as
[#64](https://github.com/o3kio/volvisor/issues/64); the global state
lock/fsync serialization is [#62](https://github.com/o3kio/volvisor/issues/62);
retired attachment accumulation is
[#63](https://github.com/o3kio/volvisor/issues/63). None of these is a
solved production issue.

## Exit-review checklist

The exit review in #36 requires a full re-run at the release commit and a
report mapping every gate to evidence. Current status:

| Item | Status |
|---|---|
| `cargo fmt --all --check` | **PASS** in CI `check` at `575acfc` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | **PASS** in CI `check` |
| `cargo test --workspace --locked` | **PASS** in CI `check` |
| `cargo doc --locked --no-deps --document-private-items` | **PASS** in CI `check` (with `RUSTDOCFLAGS=-D warnings`) |
| Supply chain (`cargo-deny`/audit job) | **PASS** in CI `supply-chain` |
| MSRV 1.85 | **PASS** in CI `msrv` |
| Real-LVM suite | **PASS** on a loop device in CI `lvm-integration` |
| Real-Ceph and real-DRBD suites | **NOT RUN** — no rig, no green run |
| Campaign suite | **NOT RUN** — no multi-host hardware |
| Clean-host install smoke from a tagged/signed artifact | **NOT RUN** — no tag exists; CI container smoke is not this check |
| Gate-to-evidence report | **This document** |
| Unmet gates recorded, none waived | **Yes** — gates 1, 2 and 5 not met; 3 and 4 partial |

## Blockers, in order

1. **#47 — signed release and clean-host install.** No signing key is
   configured; no tag or GitHub release exists; the documented local
   deb/rpm/container smoke predates Wave 2 and is not a tagged-artifact
   install.
2. **#48 — CHV consumer path and pin.** The Volvisor-side contract,
   conformance test and in-repo integration guide are delivered, but a pin
   still requires a tagged release first, then a conscious CHV-side change
   and pin update. CHV must stay at or below `advertised`.
3. **#37 / #38 / #39 — real hardware and gating rig.** Three physical hosts,
   a fourth witness failure domain, a Rook-cell execution with C01–C14
   GO/NO-GO, and green Ceph/DRBD suites. None exists.
4. **#46 — D8 host loss and the remaining undrilled halves.** D2 live is now
   drilled single-host (real old→new binary plus restore-based rollback), but
   there is still no second host, BMC or fencing path; D3 storage-side DRBD
   fencing and D8 total host loss are **not run**.
5. **#43 — credible backend numbers and 24 h soak.** Single-host loop-LVM
   only; no real Ceph/DRBD/physical-disk numbers and no 24 h run.
6. **#49 / #62 / #63 / #64 — open assurance and growth findings; #58 fixed.**
   The #49 fuzz targets, corpus regression, deterministic interleaving tests,
   nightly evidence collectors, a reproducible flake-rate calculator and its
   workflow wiring are delivered, but the **14-night flake-rate criterion
   remains open** and no observed rate is published. The ENOSPC state-save leak is fixed
   by [PR #69](https://github.com/o3kio/volvisor/pull/69) (`#58`, merged at
   `1863a7a`), and the normal uncommitted-save path was re-drilled at harness
   `940722e`; the cleanup-failure, carve-out, clear-save and unreadable-reread
   branches remain test-covered, not drilled, and the drill predates #70's
   two-save create (a fill-first fault now hits the intent save; the
   compensated final-save path is test-covered on current code).
   [#70](https://github.com/o3kio/volvisor/issues/70)
   (grow/delete/move persist divergence, the kill-9 `lvcreate`→persist orphan
   window, and a real post-rename directory-fsync fault seam) is implemented
   and deterministically test-covered in this change (the runbook's status line
   names the few defense-in-depth branches covered by construction rather than
   a dedicated fault test); it is not hardware-drilled. See
   [lvm-state-divergence.md](../runbooks/day-2/lvm-state-divergence.md). The
   measured lock/journal/retirement growth remains an open finding at the
   issue level; the Phase C addendum above records the post-fix journal/state
   measurement and its remaining RSS/log residuals. No gate
   verdict changes, and none is a resolved production issue.

## What must not be claimed

- Pre-production readiness, production support, an SLA, a zero-RPO claim, or
  a durability guarantee beyond the recorded single-host evidence.
- A signed tag, GitHub release, or clean-host install: none exists.
- Real Ceph, real DRBD, real NVMe, or multi-host durability: none was run.
- A 24-hour soak or flat long-run memory: the soak was 25 minutes and RSS
  grew.
- CHV `active`/guest-confirmed attachment state or a moved CHV pin: neither
  exists.
- That loop-device benchmark numbers are production capacity.

## Reproduce the checks

```bash
git checkout 575acfc
cargo fmt --all --check
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
cargo doc --locked --no-deps --document-private-items
sudo ./scripts/test-lvm.sh
python3 scripts/release-scope.py check
(cd docs/benchmarks/2026-11-03-devhost && sha256sum -c MANIFEST.sha256)
(cd docs/evidence/day2/2026-11-03-devhost && sha256sum -c MANIFEST.sha256)
```

The D5 post-fix re-drill bundle was added after `575acfc`, so verify it from
the commit that contains this addendum, not from `575acfc`:

```bash
git checkout <commit-containing-this-addendum>
(cd docs/evidence/day2/2026-11-03-d5-redrill && sha256sum -c MANIFEST.sha256)
```

Ceph and DRBD suites remain `not-run`; running them without a provisioned
rig is not a substitute for evidence.
