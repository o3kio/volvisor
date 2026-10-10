# ADR-0010 — Tier V: real-VMM verification of the migration story

Status: Proposed (P8 implementation scope)
Decision-accepted: pending (record acceptance date and accepting authority here)
Date: 2026-10-10
Related: [P5 plan](../plans/2026-10-10-p5-aggressive-failure-campaign.md) §0 (the tier model and claim discipline this extends), [readiness plan](../plans/2026-10-10-post-p5-readiness-questions.md) §2/D2 (the gap this closes), [post-P5 implementation plan](../plans/2026-10-10-post-p5-implementation-plan.md) (P8 staging), [ADR-0006](0006-online-resize-and-live-local-block-relocation.md) (the resize notification Tier V re-verifies), [nearline contract](../../contracts/nearline-replication-v2.md) §10 (the evidence ledger this tier joins)

## Context

The migration story is proven on both sides of the real-VMM seam but never
across it: `crates/volvisord/tests/migration_e2e.rs` drives the full protocol
(prepare → transfer → pause → snapshot → barrier → peer grant → promote →
restore → resume → complete, with failure and recovery paths) against two
`FakeVmm` instances over real daemons, real HTTP and a real loopback witness;
`crates/volvisor-provider/tests/vmm_tests.rs` drives the real `ch-remote`
adapter (`ChRemoteVmm`) argv-exactly against a scripted runner. No test
starts a **real Cloud Hypervisor process** and drives a migration through
real `ch-remote` against a real VMM state machine — real pause timing, real
snapshot files, real restore-with-disk-rewrite, real resume. Under the P5
tier discipline that gap must read as "not run", never as "works".

The P5 campaign's tier model already separates **Tier S** (simulation,
implemented) from **Tier R** (real-host, hardware-gated, scaffolded with
honest skip records). Tier V is the tier between them: the VMM process is
real, the media and the replication engine may still be simulated. It is
introduced at P8 because P7's packaging is what makes the tested software
installable and version-pinned on the Tier V host.

## Decision

### The env gate

`VOLVISOR_TEST_VMM=1` claims real-VMM execution. It requires a real
`ch-remote` binary and a real Cloud Hypervisor binary on `PATH`; both paths
are overridable by environment variables (`VOLVISOR_TEST_VMM_CH_REMOTE`,
`VOLVISOR_TEST_VMM_CLOUD_HYPERVISOR`) so a host with non-standard
installations can still claim the tier.

- **Without the variable** (the default): every Tier V scenario emits an
  explicit `skipped` evidence record with its reason — the coverage matrix
  shows the gate, never a hole. This is the Tier R pattern
  (`crates/volvisor-campaign/src/tier_r.rs`), applied unchanged: "not run"
  is distinct from "not implemented", and absence is never silent.
- **With the variable set but a binary absent**: the tier **fails loudly** —
  a claimed environment that cannot back the claim is a test failure, never
  a silent skip.

### Evidence records

Tier V records extend the campaign's schema with `tier: "V"` and record the
VMM binary version **and SHA256** of every binary the run drove (the
`--version` output plus the file hash), so a record names the exact VMM it
verified against — AGENTS rule 12's version pinning at the verification
tier. The record shape otherwise follows the campaign's discipline: run
directory, per-scenario records, `REPORT.md` renderable from the records
alone, the completion gates extended to the Tier V rows.

### The bounded scenario set

Tier V is deliberately small; it verifies the seam, not the whole Tier S
space (which stays simulated):

1. **Migration happy path end-to-end** against a real VMM: the full
   protocol above with real pause, snapshot, restore and resume.
2. **The pause/snapshot kill windows**: kills at the real-VMM boundaries
   (pause issued / snapshot taken / restore written), recovery and
   reconciliation checked against the invariant set.
3. **Snapshot/restore divergence detection**: a divergent or stale snapshot
   is detected and refused — the restore path never accepts a snapshot that
   does not match the migration's recorded state.
4. **The P6 resize notification**: `GrowVolume` on an attached volume
   completes the real `vm.resize-disk` call against the pinned,
   startup-verified VMM version (ADR-0006 first slice), including the
   retry-notification partial-failure path.

### Claim vocabulary

Tier V's claim is exactly: **"verified against a real VMM process at
recorded versions."** It is distinct from Tier S (simulation — no real VMM)
and from production support (which additionally requires real-host failure
evidence per SPEC-0002 §11). Tier V does **not** cover real-media
durability: media-level flush/FUA, power loss and SSD loss stay behind the
Tier R gates — a Tier V host may still run simulated storage below the real
VMM. No Tier V artifact may carry a production-support claim, and the tier
must not be presented as closing the Tier R gates.

## Consequences

- The "does live migration work with a real VMM" question gets a tier that
  answers it with evidence instead of inference, without waiting for real
  hardware (a CI machine or a laptop with Cloud Hypervisor installed can
  run it).
- The P8 engine-comparison benchmark (readiness plan §6) runs on the same
  installable, version-pinned software Tier V verifies.
- The campaign's coverage matrix gains a third tier row per scenario
  family; the gates keep every tier's absence honest.

## Non-goals

- Real hardware, real media or real DRBD (Tier R's scope, unchanged).
- The full Tier S fault space re-run against a real VMM — Tier V verifies
  the seam with a bounded set, not the whole matrix.
- Any production-support claim.
