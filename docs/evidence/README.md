# Evidence standards

Status: **governs evidence bundles; no nearline multi-host or Rook-cell
evidence exists yet.**

This directory holds committed evidence for claims Volvisor makes about real
hardware and real failure behavior. It is deliberately separate from the
simulation campaign's `target/campaign-evidence/` tree and from a run's raw
working directory. Read this page before adding a bundle or citing one.

## Current state (honest, as of this commit)

- **Nearline multi-host campaign (#37): NO EVIDENCE EXISTS.** There is no
  `docs/evidence/nearline/` directory. The runbook is ready
  ([`docs/runbooks/nearline-multihost-campaign.md`](../runbooks/nearline-multihost-campaign.md)),
  and the preflight is ready (`scripts/preflight-nearline.sh`), but no
  three-physical-host run has been executed or recorded here.
- **Rook-cell POC (#38): NO EVIDENCE EXISTS.** There is no
  `docs/evidence/rook-cell/` directory. The POC design and preflight are ready
  ([`docs/poc/rook-cells/README.md`](../poc/rook-cells/README.md),
  `scripts/preflight-rook.sh`), but no GO/NO-GO result has been produced.
- **Tier V evidence exists** under [`docs/evidence/tier-v/`](tier-v/) and is
  scoped exactly as its READMEs state: real Cloud Hypervisor processes at
  recorded versions, with the storage below the VMM seam simulated. It is not
  real-media or multi-host nearline evidence.
- **Transport/TLS evidence exists** under
  [`docs/evidence/transport-tls/`](transport-tls/2026-11-02-devhost-netns/).
  It is a **same-host** netns/veth run: a real rustls listener, private-CA
  verification, insecure-bind refusal, and an encrypted capture with no
  token/path/header plaintext. It is not a second physical host, a
  cross-host witness/peer hop, or a multi-host deployment; the bundle records
  that its binary came from a dirty worktree at base `a6591d9`.
- Tier S/R campaign records, when produced, remain under the git-ignored
  `target/campaign-evidence/` unless a separately reviewed committed bundle is
  added. A skipped Tier R record is a recorded gate, not evidence that a
  scenario ran.

No preflight script, runbook, plan, or ADR is evidence. A preflight passing
means only that the environment *might* be eligible; it produces no campaign
result.

## What counts as evidence

Evidence is a durable, reviewable record of an **executed** scenario at one
**exact source SHA**, sufficient for an independent reviewer to reconstruct
what was run and why the verdict follows.

A scenario record must contain all of the following:

1. **Exact identity**
   - repository commit SHA (`git rev-parse HEAD`) and whether the worktree was
     dirty; a dirty run is recorded as dirty and cannot support a clean-SHA
     claim;
   - every daemon/witness/VMM/DRBD/kernel/container/CNI version and binary
     digest used;
   - physical host identity (stable machine identity, roles, failure-domain
     labels) and the exact topology.
2. **Command and time provenance**
   - the exact command, its arguments, the host it ran on, UTC start/end
     timestamps, exit status, stdout and stderr;
   - the operator/automation identity that issued the command;
   - no reconstructed "what we probably ran" text.
3. **Fault injection**
   - what was injected, where, at which observable step, how it was verified
     to have taken effect, and how it was removed;
   - for a crash, which process was killed and how (e.g. `kill -9`), not a
     graceful stop unless graceful stop is the scenario.
4. **Observed system state**
   - daemon logs and journal excerpts, copied raw (not paraphrased), with file
     paths and line ranges;
   - witness `inspect` output and the applicable authority/lease/epoch state;
   - DRBD/Ceph/Kubernetes command output as the scenario requires;
   - the final `inspect`/`ObserveHandoff` projection after recovery;
   - for nearline, the write-trace oracle verdict: acknowledged writes,
     verified-at-destination writes, missing/corrupt blocks, honestly measured
     tail, and the boundary source (data path, not the coordinator's claim).
5. **Verdict**
   - a per-scenario `PASS` / `FAIL` / `IN_DOUBT` / `NOT_RUN` verdict against a
     pre-stated pass condition;
   - the reasoning and evidence references for that verdict;
   - every failure filed as an issue, with the issue link in the report;
   - no silent waiver and no "known flake" exemption.
6. **Integrity**
   - a `MANIFEST.sha256` over the bundle's files (or equivalent digest list);
   - a `README.md` stating what the bundle is, what it is not, and how it was
     produced;
   - a `REPORT.md` summary that cites the raw records rather than replacing
     them.

## The exact-SHA rule

Every scenario in a bundle names the same exact commit or explicitly records
that it ran at a different commit. A bundle at commit `X` cannot be cited as
evidence for code at commit `Y`. Rebuilding or cherry-picking after the run
does not move the evidence to the new SHA; run again.

If the worktree was dirty, say so in the report and in every record. Do not
delete the dirty marker to make a claim look cleaner.

## No simulation, no substitution

- Simulation (Tier S) proves the implemented logic under a bounded injected
  fault space. It is **not** real-media, real-DRBD, real-host, or real-Ceph
  evidence, and must never be filed as such.
- A Tier V record proves behavior against a real VMM at recorded versions, with
  the storage below the VMM seam simulated. It is not nearline multi-host
  evidence.
- A nested Rook/Ceph smoke test on one physical host does not prove distinct
  host failure domains, VFIO isolation, or HA. It never closes a #38 gate.
- A benchmark is not a durability proof. A happy-path migration is not a
  split-brain proof. A healthy `ceph -s` is not a failed-host result.
- Do not fabricate logs, timestamps, command output, host roles, or verdicts.
  If a command was not run, write `NOT_RUN` and why.

## No silent waivers

A scenario that cannot run is recorded as `NOT_RUN` with the concrete blocker
(hardware, module, tooling, authority, time). It is not deleted, and the overall
verdict is not upgraded. A failure is filed as an issue; the campaign may not
mark a required scenario pass by narrowing its pass condition after the fact.

Environment differences (for example a witness on a third failure domain
instead of a fourth) are recorded as a documented equivalent with the exact
deviations, the risk they introduce, and who accepted it. "Documented
equivalent" is not a blanket permission to shrink the topology silently.

## Artifact layout

Committed bundles are date-stamped run directories:

```text
docs/evidence/<campaign>/<YYYY-MM-DD>[-<suffix>]/
  README.md                 what this is / is not, environment, how to reproduce
  REPORT.md                 per-scenario results, verdict, blockers, issue links
  MANIFEST.sha256           digests of every file in the bundle
  records/                  structured per-scenario records (JSON/Markdown)
  raw/                      unmodified command output, daemon logs, journal excerpts
  oracle/                   write-trace or checksum verification inputs/results
  decision/                 signed operator verdict/decision, if the campaign defines one
```

Rules for the layout:

- Raw files are immutable once recorded. Corrections go in a new file or a
  clearly marked errata note; never rewrite a raw log to match a narrative.
- Secrets, kubeconfigs, bearer tokens, private keys, real peer shared secrets
  and unscrubbed Pod environment are **never** committed. Collect raw evidence
  outside the repository, then curate it.
- Commands, timestamps, host roles and SHAs are recorded in the bundle, not
  only in a PR description or chat.
- Large binary dumps/backups may be stored outside the repository with a
  recorded digest, path and retention owner; state that in `README.md`.
- `docs/evidence/nearline/` and `docs/evidence/rook-cell/` are created only
  by a real campaign run whose bundle is ready for review. Preflight/runbook
  work must not create placeholder result directories.

## Campaign-specific normative sources

- Nearline: [contract v2 §10](../../contracts/nearline-replication-v2.md),
  [ADR-0007](../adr/0007-drbd9-nearline-replication-provider.md),
  [P5 campaign plan](../plans/2026-10-10-p5-aggressive-failure-campaign.md),
  [runbook](../runbooks/nearline-multihost-campaign.md).
- Rook-cell: [POC GO/NO-GO design](../poc/rook-cells/README.md),
  [ADR-0008](../adr/0008-rook-only-hyperconverged-cells.md),
  [Rook-cell contract](../../contracts/rook-cell-experimental-v0.md).

## Reviewer checklist

Before approving an evidence PR:

- [ ] The bundle names one exact SHA (and the dirty state, if any).
- [ ] Every required scenario has a record or an explicit `NOT_RUN` blocker.
- [ ] Raw logs/commands/timestamps exist for every `PASS`; the report does not
      merely narrate them.
- [ ] The oracle verdict is present and consistent with the raw data.
- [ ] No simulation/VMM-only result is presented as real-host evidence.
- [ ] No required failure was waived; failures link to filed issues.
- [ ] Secrets/addresses are scrubbed or the bundle is not committed.
- [ ] The overall verdict is no stronger than the evidence, and no production
      support is claimed.
