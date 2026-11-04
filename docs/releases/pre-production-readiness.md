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
  records it as **Proposed, decision acceptance pending**; the contract
  amendment is implemented, not an accepted-policy claim.
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
posture, documentation or staged write-path hardening (slice 1 landed, no
concurrency change); none is a production-support, SLA, capacity, durability
or hardware claim.

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
- **D3 — #62 write path (slice 1 landed).** Slice 1 code is in the tree:
  the single durable-writer funnel and initial startup sweep (`f0e5237`);
  the bounded hashed temp namespace, hardened sweep and per-state-path
  in-process writer gate (`a8c23d6`); canonical-path keying of the temp-name
  hash (`a068947`); and test/doc-only evidence tightening (`913c347`). The
  `a8c23d6` hash was lexical, so `a068947` is what makes a
  symlink/`./`/`..` spelling share its predecessor's temp namespace. This is
  a semantic no-op: callers still hold the provider state lock across every
  save, there is no coalescing, no wire/schema/durability change, and **no
  throughput claim**. The earlier
  post-fix c32 −4.1 % / c128 −2.6 % measurement predates slice 1 and is not
  evidence for it. **#62 remains open**: slice 2 (release the lock) and a new
  clean-tree benchmark at the implementation SHA are not done, so no gate
  verdict changes. No live evidence changed in this batch: `d5.sh` Part B was
  not re-drilled at this SHA here, so its `.volvisor-` message assertion is
  code-derived until the next drill, and the symlink test panics rather than
  skips where the temp filesystem refuses symlinks.
- **D4 — ADR-0012.** Still `Proposed`; accepting it remains an authority
  decision, not an implementation task.

No gate becomes met. Gates 1, 2 and 5 remain **NOT MET**, gates 3 and 4
remain **PARTIAL**, and the overall status remains **NOT PRE-PRODUCTION
READY**. No tag, GitHub release, push or pull request is created by this
workstream.

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
