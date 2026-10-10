# ADR-0009 — Distribution and packaging for Linux servers

Status: Proposed (P7 implementation scope)
Decision-accepted: pending (record acceptance date and accepting authority here)
Note: the decision itself is already recorded by the readiness plan D5
(PR #17); this ADR is its normative elaboration, and its status flips to
Accepted when the P7 completion gate passes. Two deltas from D5's summary
shape are deliberate extensions recorded here: the witness daemon ships in
the same package with its own unit (and its own `--version`/
`--check-config`), and package-signing infrastructure is narrowed to
signed git tags + `SHA256SUMS` for P7.
Date: 2026-10-10
Related: [ADR-0007](0007-drbd9-nearline-replication-provider.md) (the DRBD kernel-module reality the dependency metadata must carry), [SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md) §12 (implementation sequence, P7), [readiness plan](../plans/2026-10-10-post-p5-readiness-questions.md) §5/D5 (the decision this ADR normatively records), [post-P5 implementation plan](../plans/2026-10-10-post-p5-implementation-plan.md) (P7 staging)

## Context

The repository builds a cargo workspace with CI (fmt/clippy/test/doc plus an
LVM-gated integration job) but has no packages, no service units, no release
process, no version stamping and no install-smoke gates. The tool must be
installable on Linux servers — that is a *distribution* requirement, and it
arrives at P7 deliberately: every later real-host phase (Tier V, the Rook-cell
POC) deploys through what this ADR defines rather than ad-hoc binary copies.

What already exists and the design builds on: the daemon takes a TOML
`--config` file; the `--api-socket` convention assumes `/run/volvisor/vms`;
the tracing stack emits structured JSON events; both binaries (`volvisord`,
`volvisor-witnessd`) live in one crate. What does **not** exist: `--version`
stamping, config validation as a standalone surface, service units, packages,
release CI.

## Decision

### Artifacts

- `.deb` packages (Debian/Ubuntu) and `.rpm` packages (EL/Fedora), built in
  release CI, plus version-stamped release binaries for each supported
  target.
- The version derives from `git describe` and is compiled in at build time;
  a new `volvisord --version` (and `volvisor-witnessd --version`) prints it
  (P7 scope — no such flag exists today).
- One package ships both binaries; the service model below separates their
  units.

### Filesystem layout

| Path | Contents |
|---|---|
| `/etc/volvisor/` | configuration; a documented example config ships with the package (the daemon already requires a TOML `--config` path) — see the [configuration documentation](../configuration.md) and the annotated [examples/volvisor.toml](../../examples/volvisor.toml) / [examples/witnessd.toml](../../examples/witnessd.toml) |
| `/var/lib/volvisor/` | persistent state: journals, provider state, the witness registry |
| `/run/volvisor/` | runtime sockets — the P4b `--api-socket` convention already assumes `/run/volvisor/vms` |
| logs | via journald (the tracing stack already emits structured events; the units route them to the journal, no separate log files) |

### Service model

- `volvisord.service` and `volvisor-witnessd.service`, separate units, both
  binaries from the one package.
- Root is required (device claiming, DRBD administration, socket paths);
  hardening directives (`ProtectSystem=`, `PrivateTmp=`, capability
  bounding, `ReadWritePaths=` for the state/run directories) are applied
  where the device paths the daemon must touch allow them — the units
  document any directive deliberately omitted rather than silently
  weakening the set.
- Readiness: `Type=notify`; the daemon emits `sd_notify` `READY=1` only
  after startup reconciliation completes (journal replay and provider
  reconcile, the existing startup order) — a daemon that is up but not
  reconciled is not ready. P7 adds this; today no readiness signal exists.
- Restart policy (supervised restart into the reconciling startup path) and
  `EnvironmentFile=` support.

### `--check-config` (the install smoke surface)

P7 adds `volvisord --check-config <path>` (and the witnessd equivalent):
load and validate the configuration file, print the typed error or exit 0.
This is the smoke surface both package tests and operators use — it proves
the installed binary runs and parses its config without needing devices.
Every field is documented in [docs/configuration.md](../configuration.md);
the annotated examples above are validated by a repository test, so the
examples and the validator cannot drift apart.

### Dependency honesty

- `lvm2`: hard dependency.
- `drbd-utils` 9: **recommended**, not hard — because the DRBD 9 kernel
  module is a host property the package cannot install: mainline kernels
  carry DRBD 8.4.11 only; DRBD 9 is out-of-tree from LINBIT; Debian and
  SLES ship 9.x kmods in their repositories; RHEL needs ELRepo or LINBIT.
  The package metadata and documentation state this kernel reality — they
  do not pretend a userspace package provides the kernel side.
- `ceph-common`: optional, for the `ceph-rbd` class.

### Release gates

- Release CI builds the packages and smoke-tests them in containers:
  install, `volvisord --version`, `volvisor-witnessd --version`,
  `--check-config` on the shipped example config, and unit start where the
  container sandbox allows it (device-dependent paths are exercised only
  where sandboxable; what cannot be sandboxed is recorded, not skipped
  silently).
- The release notes carry the campaign certification for the exact commit:
  `campaign-summary --all --check` exit 0 (the P5 completion gates as a
  process exit code) over the tagged commit's evidence run.

### Provenance

- Signed git tags; `SHA256SUMS` for every released artifact.
- The exact build toolchain (compiler version, target triples, build
  environment) is recorded with each release — reproducible-build best
  effort: the record exists even where bit-reproducibility is not
  achievable.
- Distro repository hosting and package signing infrastructure are out of
  P7 scope (best-effort later; the artifacts themselves are complete
  without them).

## Consequences

- The install path becomes reproducible and testable: an install failure is
  a release-gate failure, not a user's discovery.
- Every later phase (Tier V drives, the Rook POC) deploys through the same
  artifacts it tests.
- The DRBD 9 kernel-module reality is stated where an operator looks for it
  (package metadata), keeping AGENTS rule 12 honest at the distribution
  layer.

## Non-goals

- **OCI images** — deferred; not required by any P6–P9 phase.
- **Any production-support claim.** Packaging is a distribution fact, not a
  durability claim: an installed package says nothing about real-media
  failure behavior, which stays behind the Tier R/Tier V gates and
  SPEC-0002 §11. "Not run" remains distinct from "not implemented" in every
  release artifact.
