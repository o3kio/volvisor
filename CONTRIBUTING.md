# Contributing to Volvisor

Status: contributor guide for the **implemented prototype**. Volvisor makes no
production-support claim; read [AGENTS.md](AGENTS.md) and
[SUPPORT_MATRIX.md](SUPPORT_MATRIX.md) before changing behavior or writing a
claim.

## Read this first

1. [AGENTS.md](AGENTS.md) — the non-negotiable engineering rules.
2. [ARCHITECTURE.md](ARCHITECTURE.md) — crate map, request flow, invariants,
   failure model and state locations.
3. [docs/START_HERE.md](docs/START_HERE.md) — what works today and how to try
   it safely.
4. The normative v2 documents for the area you are changing:
   [Volume API v2](contracts/volume-api-v2.md),
   [nearline replication v2](contracts/nearline-replication-v2.md),
   [SPEC-0002](docs/specs/SPEC-0002-volvisor-volume-virtualization.md) and the
   relevant ADR under [docs/adr/](docs/adr/).

Each ADR records its own acceptance status. Do not treat a proposed or pending
ADR as accepted, and do not quietly reinterpret a superseded v1 document.

## Build

Rust **1.85 or newer** is required (edition 2024; CI also runs the declared
MSRV). `rust-toolchain.toml` selects stable for normal local work.

```sh
cargo build --locked
```

The workspace builds without privileged devices, a cluster, a DRBD peer or a
VMM. The daemons are `volvisord` and `volvisor-witnessd`:

```sh
cargo build --release --locked -p volvisord --bins
./target/release/volvisord --config examples/volvisor.toml --check-config
./target/release/volvisor-witnessd --config examples/witnessd.toml --check-config
```

A `--check-config` pass proves the binary runs and accepts that configuration;
it does not prove storage exists or is safe to mutate.

## Formatting, linting and documentation

Run the same gates CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --document-private-items
```

MSRV check (the same command CI runs on Rust 1.85):

```sh
cargo check --workspace --locked --all-targets
```

Supply-chain check (CI pins `cargo-deny` 0.20.2):

```sh
cargo install cargo-deny --locked --version 0.20.2
cargo deny check
```

The workspace denies `unsafe_code`, `unwrap_used`, `expect_used`, `panic` and
`dbg_macro` in production code. Test-only crates and test modules have
documented allowances; do not widen them to production code.

## Test tiers actually present

The repository uses the evidence tiers defined by the campaign/ADR vocabulary:
Tier S (simulation), Tier R (real hosts/media, scaffolded), Tier V (real VMM,
simulated storage below the seam). These are evidence tiers, not a claim that
every tier has been run.

| Tier / suite | What runs | Command | Hardware / prerequisites |
|---|---|---|---|
| Workspace unit + integration | All default tests: API, journal, providers over fakes/testkits, handoff, witness, TLS, daemon e2e, campaign Tier S and skipped Tier R/V gate records | `cargo test --workspace --locked` | None beyond a normal Linux build host |
| Provider conformance | Each provider's `tests/conformance.rs` over the shared kit | Included in `cargo test --workspace --locked` | None |
| Property/state-machine | `proptest` state-machine suite for provider semantics | `cargo test -p volvisor-provider --test state_machine` | None |
| Journal replay/faults | Replay, torn-tail and injected append failures | `cargo test -p volvisor-journal`; fault injection is feature-gated (`--features test-faults`, test-only) | None |
| Daemon end-to-end | Router → journal → fake provider / fake DRBD world, including migration, grow and move paths | `cargo test -p volvisord` | None |
| Campaign Tier S | Simulated failure campaign rows, oracle, evidence rendering | `cargo test -p volvisor-campaign` | None |
| Campaign Tier R | Real-host scenario gates; **scaffolded and skipped by default** | `VOLVISOR_CAMPAIGN_TIER=R cargo test -p volvisor-campaign --test tier_r` | Real host/media; a claimed `R` with missing toolchain fails loudly. Do not claim Tier R from a default run |
| Campaign Tier V | Four bounded real-VMM scenarios | `VOLVISOR_TEST_VMM=1 cargo test -p volvisor-campaign --test tier_v` | Real `cloud-hypervisor`/`ch-remote`, KVM, guest kernel/initramfs; storage below the VMM seam stays simulated |
| Campaign certification | Renders the coverage matrix and completion gates over a run directory | `cargo run -p volvisor-campaign --bin campaign-summary -- --all --check` | Existing `target/campaign-evidence/<run-id>/` records |
| Real LVM | Loop-device integration suite and evidence script | `sudo ./scripts/test-lvm.sh` (preferred) or `sudo -E env "PATH=$PATH" VOLVISOR_TEST_LVM=1 cargo test --locked -p volvisor-lvm --features integration-lvm` | Root, `lvm2`, `util-linux`, loop devices, device-mapper, tmpfs mount permission, ≥1 GiB free; disposable resources |
| Real Ceph | RBD lifecycle against an external cluster | `sudo -E ./scripts/test-ceph.sh` (preferred) or `VOLVISOR_TEST_CEPH=1 cargo test --locked -p volvisor-ceph --features integration-ceph --test integration_ceph -- --nocapture --test-threads=1` | `ceph`/`rbd`, `rbd` kernel module, root for `rbd map`, dedicated pool, FSID/monitor/user/keyring configuration |
| Real DRBD | Single-primary lifecycle against a provisioned peer | `sudo -E ./scripts/test-drbd.sh` (preferred) or `VOLVISOR_TEST_DRBD=1 cargo test --locked -p volvisor-drbd --features integration-drbd --test integration_drbd -- --nocapture --test-threads=1` | DRBD 9 kernel module and `drbd-utils` on both hosts, dedicated VG, pre-provisioned fresh peer LV, shared-secret file |

Every real-backend script supports `--check`, exits `2` with a typed message
when its rig is missing, and writes JUnit/log evidence under
`target/real-backends/<suite>/`. A skipped suite is not a pass. See
[docs/testing/real-backends.md](docs/testing/real-backends.md) for the full
rig contract, runner labels, secrets/variables and triage.

### Fake vs privileged suites

- **Fake/no-hardware:** the default workspace suite, provider conformance,
  API/daemon e2e, the DRBD testkit, campaign Tier S, and Tier R/V *skip*
  records. These are safe on a development machine and must stay runnable
  without root.
- **Privileged/destructive:** the LVM/Ceph/DRBD integration suites. They
  create, map, grow, move or delete real resources. Run them only against
  disposable loop devices, a dedicated test pool/VG and a non-production
  cluster. Never point them at production storage.

### Fuzzing, interleaving and fault injection

Issue [#49](https://github.com/o3kio/volvisor/issues/49) added four
coverage-guided fuzz targets in a standalone `fuzz/` workspace:
`journal_frame`, `journal_record`, `lvm_state` and `request_validation`.
Their committed seed corpus under [`fuzz/corpus/`](fuzz/corpus) is also
exercised deterministically by the `fuzz_corpus` integration tests in
`volvisor-journal`, `volvisor-lvm` and `volvisor-types`, so a known input
regresses in the normal `cargo test --workspace --locked` gate.
`.github/workflows/fuzz.yaml` runs each target with a bounded budget
nightly and on manual dispatch; see [`fuzz/README.md`](fuzz/README.md).

[`scripts/flake-rate.py`](scripts/flake-rate.py) computes the observed
campaign flake rate and the 14-consecutive-night flag from the JSONL samples
the campaign-flake workflow uploads. The procedure, output semantics and
exit codes are in
[docs/testing/flake-rate.md](docs/testing/flake-rate.md). **No observed
flake rate is published and issue #49's 14-night criterion is NOT MET**;
one green run, the fuzz corpus and synthetic fixtures are not the criterion,
and a rate must not be published without the downloaded nightly artifact
history.

Deterministic interleaving regressions cover the journal pin/compaction
race (including the expired-operation retry paths), scheduled compaction
between an intent and its outcome, and the campaign harness's
retained-listener stop/restart semantics.

Also present:

- `proptest` state-machine testing in
  [`crates/volvisor-provider/tests/state_machine.rs`](crates/volvisor-provider/tests/state_machine.rs),
  with the committed regression seed
  [`state_machine.proptest-regressions`](crates/volvisor-provider/tests/state_machine.proptest-regressions);
- journal append fault injection behind the test-only `test-faults` feature;
- provider state-save crash seams and the campaign's deterministic crash hooks
  (test-only; inert in production);
- the campaign's write-trace oracle and generated kill rows (Tier S).

A bounded fuzz run and a green campaign sample are **not** a 14-night
flake-rate claim. Issue #49's "no open campaign flake for 14 consecutive
nightly runs" criterion remains open until those runs are recorded and
published; `.github/workflows/campaign-flake.yaml` collects the samples but
does not satisfy the criterion by itself. Run
[`scripts/flake-rate.py`](scripts/flake-rate.py) over the downloaded
artifact history (not the current run alone) before stating a rate or a
`criterion_met` value; see
[docs/testing/flake-rate.md](docs/testing/flake-rate.md).

### Hardware-lab access

**The repository does not define or grant access to a shared hardware lab.**
There is no lab inventory, booking process or credential path in-tree. What
exists is the rig contract in
[docs/testing/real-backends.md](docs/testing/real-backends.md):

- LVM can run on any suitable Linux host with root and loop/device-mapper
  support.
- Ceph and DRBD require externally operated infrastructure and self-hosted
  GitHub runners with the documented labels (`volvisor-ceph`, `volvisor-drbd`).
  Provisioning that infrastructure is an operator responsibility outside this
  repository.

If you do not have such a rig, use the fake/testkit suites and report the real
suites as **not run**. Never simulate a hardware result or mark a missing rig
as a pass.

## Review checklist

Before requesting review:

- [ ] The change respects [AGENTS.md](AGENTS.md); fencing, quorum, generation
      checks and error reporting were not weakened to make a test pass.
- [ ] The change preserves journal-before-mutate, fsync-before-ack and
      stale-generation fail-closed behavior where relevant.
- [ ] New or changed durable state follows
      [docs/schema-compatibility.md](docs/schema-compatibility.md), including a
      version decision and an N-1 fixture when the shape changes.
- [ ] No secret material (tokens, keys, keyrings, shared secrets) is logged,
      journaled or committed. Journal payload redaction still covers new
      credential fields.
- [ ] Tests are at the right tier. Fake suites are not presented as real-host
      evidence; a fake or simulated run is labeled as such.
- [ ] Documentation and claims are updated together. No production-support,
      SLA, zero-RPO or multi-host claim is made beyond recorded evidence
      (AGENTS rule 12).
- [ ] New links in Markdown resolve, and ADR statuses are respected.
- [ ] `cargo fmt`, clippy with `-D warnings`, workspace tests and docs pass.
      Add the relevant real-backend or campaign command/result if the change
      touches those surfaces, or state that it was not run and why.
- [ ] The PR is focused, names the issue it closes/advances, and includes the
      exact commands used.

## Commits and pull requests

- Keep commits focused and reviewable; separate mechanical moves from behavior
  changes where practical.
- Write commit messages that state **why** the change is needed and what
  invariant/contract it preserves. There is no repository-specific DCO or
  sign-off rule defined in-tree; do not invent one.
- Do not commit generated benchmark/campaign output or local evidence unless
  it is a reviewed evidence bundle following
  [docs/evidence/README.md](docs/evidence/README.md).
- Do not commit secrets, keyrings, tokens, private keys, real host inventories
  or unscrubbed logs.
- A PR that changes a support claim must link the exact evidence, SHA and
  hardware/versions in [SUPPORT_MATRIX.md](SUPPORT_MATRIX.md) and
  [release-scope.toml](release-scope.toml).
- Review routing is in [.github/CODEOWNERS](.github/CODEOWNERS). Per-area human
  ownership still needs to be assigned; see
  [docs/bus-factor.md](docs/bus-factor.md).

## Changing a durable-state schema

Follow [docs/schema-compatibility.md](docs/schema-compatibility.md) exactly.
The short version:

1. bump the affected store's version constant (or `RECORD_VERSION` for the
   journal) for any serialized-shape change, including additive fields;
2. keep the last released shape under `tests/fixtures/schema/` as an N-1 (and
   N-2 where a real N-2 exists — do not fabricate one);
3. add decode and upgrade-on-write tests, plus a future-version refusal test
   that proves the file is left intact;
4. update the matrix and the store's documentation;
5. run the full workspace gates above.

There is no in-place downgrade. Rollback requires stopping the daemon and
restoring a pre-upgrade backup of the whole state directory (journal plus all
provider states, migration records and peer preparations) with a matching
binary.

## Operational procedures

Do not hand-edit durable state, truncate a complete journal frame or change a
`schema_version` to force a start. Use the day-2 runbooks:

- [Backup and restore](docs/runbooks/day-2/backup-and-restore.md)
- [Upgrade and rollback](docs/runbooks/day-2/upgrade-and-rollback.md)
- [Lost witness](docs/runbooks/day-2/lost-witness.md)
- [Journal and state corruption](docs/runbooks/day-2/journal-and-state-corruption.md)
- [ENOSPC](docs/runbooks/day-2/enospc.md)
- [Crash loop and lock](docs/runbooks/day-2/crash-loop-and-lock.md)
- [Credential and certificate rotation](docs/runbooks/day-2/credential-and-cert-rotation.md)
- [Host loss and rejoin](docs/runbooks/day-2/host-loss-and-rejoin.md)
- [LVM state divergence](docs/runbooks/day-2/lvm-state-divergence.md)
