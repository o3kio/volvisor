# Start here

Status: **implemented prototype; no production support**
Date: 2026-11-02
Intended release: `v0.1.0` — **not tagged, not published**

Volvisor is a Rust workspace that implements a volume virtualization control
plane and three storage-provider slices. It is useful for evaluation,
integration development, and fault-injection work **on disposable test
resources**. It is not ready for production data, production availability
targets, or a support commitment of any kind.

If you only read one more document, read the
[support matrix](SUPPORT_MATRIX.md). It records what is actually proven and
what is explicitly not run.

## What works today

The implementation and its in-repo tests cover:

- **Stable volume identity and privileged mutation discipline**: journaled
  intent, idempotent replay, generation checks, and fail-closed behavior on
  stale/conflicting operations.
- **`native-local` on LVM**: create/list/inspect/attach/detach/grow/delete,
  ownership metadata, and same-VG `pvmove` relocation. The LVM provider has
  a unit/conformance suite and an env-gated real-LVM integration suite wired
  to CI.
- **`ceph-rbd` against an existing external Ceph cluster**: FSID/pool
  fail-closed startup, ownership metadata, `exclusive-lock` single-writer
  attach, growth, delete policy, and health/capacity reflection. The
  `FakeCeph` world plus provider conformance tests run without a cluster.
- **`nearline-replicated` prototype on DRBD 9**: local-end resource
  lifecycle, Protocol A/B/C selection, a witness/lease authority, and the
  DRBD-only migration/handoff surface.
- **The daemons and their packaging surface**: `volvisord` and
  `volvisor-witnessd` build from source, answer `--version`, validate their
  example configurations with `--check-config`, and have deb/rpm metadata
  plus systemd units.
- **Test evidence that is real within its tier**: the Tier S simulated fault
  campaign, and four bounded Tier V scenarios against real Cloud Hypervisor
  v51.1 on one physical development host (storage simulated). See
  [the evidence index](SUPPORT_MATRIX.md#evidence-index).

A `--check-config` pass proves the binary runs and accepts a configuration.
It does **not** prove that the configured storage exists, that a provider
can mutate it, or that a volume is safe to use.

## What does not work (or is not supported)

- **No production support in any class.** There is no SLA, durability
  guarantee, zero-RPO claim, or availability commitment.
- **Nearline is not proven on three physical hosts.** The only executed VMM
  evidence is single-host with simulated storage; campaign
  [#37](https://github.com/o3kio/volvisor/issues/37) is **not run**.
- **Real DRBD and real Ceph integration suites are not release-gated
  yet.** They are env-gated and can skip when infrastructure is absent;
  [#39](https://github.com/o3kio/volvisor/issues/39) tracks making them a
  required release gate.
- **The Rook-cell managed-Ceph mode has never been run.** The POC
  [#38](https://github.com/o3kio/volvisor/issues/38) has no GO;
  [ADR-0008](adr/0008-rook-only-hyperconverged-cells.md) is production
  NO-GO.
- **Non-loopback deployment is not hardened.** The API is plain HTTP, the
  admin token is shared, and peer/witness URLs reject `https://`
  ([#40](https://github.com/o3kio/volvisor/issues/40)). Keep evaluation on
  loopback or an isolated disposable network.
- **CHV is not pinned to a tagged Volvisor build.** CHV reports at most
  `advertised` (never `active` without guest confirmation); the pin update
  is [#48](https://github.com/o3kio/volvisor/issues/48) and waits for a
  tagged release.
- **Attached `native-local` volumes are not live-migratable.** The mobility
  surface is wired only for the `drbd` provider.
- **There is no tag and no GitHub release.** v0.1.0 is a draft; the tag is
  blocked by the [checklist](releases/v0.1.0-checklist.md).

## How to try it safely

Use a disposable VM or a lab host whose disks you can destroy. Do **not**
point Volvisor at production disks, a production Ceph cluster, or a host
whose LVM volume groups contain data you care about. The privileged surface
can claim devices and mutate LVM/DRBD/Ceph state by design.

### 1. Build and run the test suite

Rust 1.85 or newer is required (the repository's `rust-toolchain.toml`
selects stable; CI also checks the declared MSRV).

```sh
git clone https://github.com/o3kio/volvisor.git
cd volvisor
cargo build --locked
cargo fmt --all --check
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
```

Real backends are opt-in and skip without their prerequisites:

```sh
# Needs root, lvm2, and disposable loop/block devices.
VOLVISOR_TEST_LVM=1 cargo test --locked -p volvisor-lvm --features integration-lvm

# Needs VOLVISOR_CEPH_* cluster configuration and ceph-common.
VOLVISOR_TEST_CEPH=1 cargo test --locked -p volvisor-ceph --test integration_ceph

# Needs drbd-utils, the DRBD 9 kernel module, and a provisioned peer.
VOLVISOR_TEST_DRBD=1 cargo test --locked -p volvisor-drbd --test integration_drbd
```

A skipped real-backend test is **not** a pass and is not evidence for a
production claim.

### 2. Check the shipped configuration examples

```sh
cargo build --release --locked -p volvisord --bins
./target/release/volvisord --config examples/volvisor.toml --check-config
./target/release/volvisor-witnessd --config examples/witnessd.toml --check-config
./target/release/volvisord --version
```

The examples contain placeholder tokens and a `drbd` provider shape. They
validate; they are not a working deployment without real LVM/DRBD/witness
resources and generated secrets.

### 3. Run the daemon only against disposable state

Copy an example to a scratch path, replace the placeholder tokens, point
`journal_dir` at a disposable directory, and keep `listen` on loopback. Full
field documentation is in [configuration.md](configuration.md). Do not
enable migration or point peer/witness URLs at shared infrastructure until
the transport/auth gate [#40](https://github.com/o3kio/volvisor/issues/40)
is done.

### 4. Build packages (optional, local only)

The exact commands and the recorded local results are in the
[v0.1.0 checklist](releases/v0.1.0-checklist.md). A local package is a
distribution artifact, not a release: there is no signed tag, no published
checksums, and no support commitment.

## Where the evidence is

| Question | Read |
|---|---|
| What is proven per class, and what is not? | [SUPPORT_MATRIX.md](../SUPPORT_MATRIX.md) |
| What did the simulated fault campaign and real-VMM runs record? | [campaign report](evidence/tier-v/2026-10-11-devhost-v51/REPORT.md), [Tier V run 1](evidence/tier-v/2026-10-10-devhost/), [Tier V run 2](evidence/tier-v/2026-10-11-devhost-v51/) |
| What is the Tier V claim, exactly? | [ADR-0010](adr/0010-tier-v-real-vmm-verification.md) |
| What must a real-host campaign show? | [SPEC-0002 §11](specs/SPEC-0002-volvisor-volume-virtualization.md#11-real-host-acceptance-matrix) |
| What would the Rook-cell POC have to prove? | [POC GO/NO-GO](poc/rook-cells/README.md), [ADR-0008](adr/0008-rook-only-hyperconverged-cells.md) |
| Why is v0.1.0 not tagged yet? | [v0.1.0 checklist](releases/v0.1.0-checklist.md) |

## Release status

The intended v0.1.0 notes are a **draft**
([docs/releases/v0.1.0.md](releases/v0.1.0.md)). The tag is blocked on
nearline three-physical-host evidence (#37), the Rook-cell POC (#38),
release-gated Ceph/DRBD suites (#39), transport/auth hardening (#40), the
CHV consumer path and pin (#48), and a configured signing key. No tag or
GitHub release may be created until those gates pass.
