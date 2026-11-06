# Bus-factor risks and mitigations

Status: **living risk register** for the knowledge-concentration work in
[#50](https://github.com/o3kio/volvisor/issues/50). The structural mitigations
below are delivered. The organizational mitigations — named per-area owners,
two independent release/restore sign-offs, documented hardware-lab access and
14 consecutive clean nightly campaign runs — are **not** delivered. Nothing
here changes the support posture in [SUPPORT_MATRIX.md](../SUPPORT_MATRIX.md):
Volvisor remains an implemented prototype with no production-support claim.

## Recorded context

[#50](https://github.com/o3kio/volvisor/issues/50) records roughly 114k lines,
281 commits in about four weeks and four committers (239/32/9/1). That is
historical issue context, not a live repository metric. It describes the
problem this page tracks: release and recovery knowledge currently depends on
a small set of people.

The repository does not record a per-area human owner. The owning GitHub
organization is `o3kio`; the current write-access collaborators are
`@senolcolak` and `@zoorpha`. [`.github/CODEOWNERS`](../.github/CODEOWNERS)
lists those two as **provisional structural coverage owners** for every
workspace crate and the docs/contracts/CI surfaces. That makes review routing
explicit; it does **not** make either person the subject-matter owner of every
area, and it does not satisfy #50's ownership-map intent by itself. A bare
organization handle is not a valid GitHub CODEOWNERS owner, so `@o3kio` cannot
stand in for a user or team.

## Risk register

| # | Risk | Why it matters | Mitigation in place | Remaining gap / action |
|---|---|---|---|---|
| 1 | **Release is single-person knowledge** | A signed tag, package build, real-backend gate and evidence bundle require knowing the release workflow, `release-scope.toml`, signing material and failure triage. | [Release workflow](../.github/workflows/release.yaml), [release-scope.toml](../release-scope.toml), [packaging guide](../packaging/README.md), [CONTRIBUTING.md](../CONTRIBUTING.md) and the v0.1.0 checklist document the pipeline. | No second maintainer has independently completed a release. #47 remains blocked on a signing key. Record two names, date, commit and result when it is done. |
| 2 | **Restore/recovery is single-person knowledge** | A wrong restore can destroy the only durable state. The runbooks are explicit, but an undrilled reader is not equivalent to an independent operator. | [Day-2 runbook index](runbooks/day-2/README.md), [backup and restore](runbooks/day-2/backup-and-restore.md), [schema policy](schema-compatibility.md), [state-divergence runbook](runbooks/day-2/lvm-state-divergence.md) and the recorded single-host drills. | No second maintainer has independently completed a restore drill. D2/D3 are partial and D8 host loss is not run. Record two independent sign-offs; do not infer them from the existing single-host bundle. |
| 3 | **Undocumented "ask person X" steps** | A recovery procedure that only works with a person on the call is not a procedure. | Release and recovery commands, prerequisites, fail-closed rules and evidence locations are in-tree; `CONTRIBUTING.md` points at them. | A review of the release and restore paths against #50's acceptance criterion is still needed. Any step that names a person instead of a command, credential path or decision rule must be replaced or recorded as an explicit organizational prerequisite. |
| 4 | **No named per-area ownership** | Structural CODEOWNERS coverage does not tell a contributor who understands journaling, witness fencing, DRBD, Ceph, LVM recovery or packaging. | Every crate and surface has an explicit CODEOWNERS pattern; [ARCHITECTURE.md](../ARCHITECTURE.md) maps responsibilities. | Assign a primary and backup for each area and replace the provisional handles with named users/teams. Until then, treat all areas as unassigned for expertise purposes. |
| 5 | **Ceph/DRBD/campaign knowledge depends on external rigs** | The real suites need a cluster, a DRBD peer, dedicated runners and destructive test resources. That knowledge is not reproducible from a laptop. | [Real-backend rig contract](testing/real-backends.md), one-command `scripts/test-*.sh` gates with typed `--check` failures, self-hosted runner labels (`volvisor-ceph`, `volvisor-drbd`) and the campaign Tier S/R/V split. | The repository defines **no shared hardware lab, inventory, booking process or credential path**. Ceph/DRBD remain `not-run`; a second person cannot reproduce those runs without externally provisioned infrastructure. Document lab ownership and access outside this repository, or keep the suites explicitly not run. |
| 6 | **Durable-state and schema recovery knowledge** | Journal replay, provider state files, migration records and witness journals are authority-bearing. Hand-editing or guessing is destructive. | [Schema compatibility policy](schema-compatibility.md), versioned stores, N-1 fixtures, upgrade/refusal tests, [journal/state corruption](runbooks/day-2/journal-and-state-corruption.md) and [ENOSPC](runbooks/day-2/enospc.md) runbooks. | Cross-train a second person on the store/version matrix and a restore from a real backup. The open schema-policy gaps (`grow-notifications.json`, `ownership.json`) are documented, not solved. |
| 7 | **Witness/fencing and migration handoff knowledge** | These paths encode single-writer safety. A mistaken "fix" can create split brain or silently resume source writes. | [Nearline contract v2](../contracts/nearline-replication-v2.md), [ARCHITECTURE.md](../ARCHITECTURE.md) §3, [lost-witness runbook](runbooks/day-2/lost-witness.md), deterministic campaign scenarios and Tier V runs. | Storage-side lost-witness fencing and three-physical-host evidence are not run. A second maintainer needs the contract plus a safe simulation path before owning this area. |
| 8 | **CI, packaging and evidence-pipeline knowledge** | A release can fail or silently weaken a gate if the workflow graph, release scope or artifact checks are misunderstood. | `ci.yaml`, `real-backends.yaml`, `release.yaml`, the non-bypassable release gate graph check, `campaign-summary`, and the evidence standards. | Assign an owner for CI/release and one for evidence. Rehearse a failed-gate and a missing-rig run so the failure path, not only the happy path, is familiar. |
| 9 | **Assurance knowledge is thin** | The absence of broad fuzzing/interleaving coverage is easy to misread as "tested enough". | `proptest` state-machine tests, four `cargo-fuzz` targets with a committed seed corpus and deterministic corpus-regression tests, journal pin/compaction and retained-listener interleaving regressions, test-only journal fault injection, provider crash seams, campaign crash hooks and the Tier S oracle. | No model-checking suite. The campaign has no 14 consecutive clean nightly runs, so the flake rate is unpublished. A bounded fuzz run is not production-support evidence. |
| 10 | **Documentation drifts from implementation** | A stale map sends a new contributor to the wrong crate or an obsolete contract. | README now links this page, [ARCHITECTURE.md](../ARCHITECTURE.md) and [CONTRIBUTING.md](../CONTRIBUTING.md); crate inventory is checked against `cargo metadata`; ADR statuses are explicit. | Keep the crate map and the state/schema matrix updated when a crate is added or a store changes. A documentation check is not automated. |

## Delivered structural mitigations

- [ARCHITECTURE.md](../ARCHITECTURE.md): 13-crate map, dependency direction,
  Volume API request flow, witness/peer/migration paths, invariants, crash
  model, authoritative state locations and the AGENTS rule 12 non-production
  statement.
- [CONTRIBUTING.md](../CONTRIBUTING.md): build, formatting/lint/doc gates, the
  actual test tiers, fake versus privileged suites, the fuzz targets and
  corpus regression, hardware prerequisites, review checklist, PR expectations
  and the schema-change procedure.
- [`fuzz/`](../fuzz): four coverage-guided targets (`journal_frame`,
  `journal_record`, `lvm_state`, `request_validation`), a committed seed
  corpus also run by deterministic `fuzz_corpus` tests, and bounded nightly
  workflows (`.github/workflows/fuzz.yaml`,
  `.github/workflows/campaign-flake.yaml`). These collect evidence; they do
  not close the 14-night criterion.
- [`.github/CODEOWNERS`](../.github/CODEOWNERS): explicit coverage for all 13
  workspace crates plus contracts, docs, CI, scripts, tests, packaging and
  examples.
- Existing normative material: v2 ADRs, SPEC-0002, the v2 contracts,
  [SUPPORT_MATRIX.md](../SUPPORT_MATRIX.md), the day-2 runbooks and the
  [schema policy](schema-compatibility.md).

## Outstanding actions

These are organizational or evidence actions; none is claimed as complete.

1. **Assign per-area primary and backup owners.** Replace the provisional
   CODEOWNERS handles with named users or teams. Suggested areas: types/journal
   and API contract; LVM provider and state recovery; Ceph; DRBD and witness;
   handoff/migration; TLS/auth/admission; packaging/release/CI; campaign and
   evidence.
2. **Two maintainers independently complete a release and a restore drill.**
   Record both names, the date, commit/artifact, exact commands and result in
   [the readiness report](releases/pre-production-readiness.md). This depends
   on #46 and #47 and is not satisfied by the existing single-host drill.
3. **Document hardware-lab access without inventing it.** If a shared Ceph or
   DRBD lab exists outside the repository, record who operates it and how a
   contributor requests a run. Otherwise continue to report those suites as
   **not run**.
4. **Close the 14-night flake criterion.** [#49](https://github.com/o3kio/volvisor/issues/49)
   requires no open campaign flake for 14 consecutive nightly runs and a
   published flake rate. That evidence does not exist yet.
5. **Cross-train and rotate.** Have the backup owner run a release, a restore,
   a witness-loss drill and a failed real-backend gate before treating the
   area as covered.

## What is not a mitigation

- CODEOWNERS with the same two handles on every path is coverage, not
  distributed expertise.
- A runbook that has only been executed by its author is not independently
  validated.
- A skipped real-backend suite, a fake-provider test or a Tier S scenario is
  not real-hardware evidence.
- This page does not mark #50 complete, does not sign off a release or restore
  drill, and does not change any readiness gate verdict.
