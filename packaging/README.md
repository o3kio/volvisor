# Packaging (P7: systemd units, sd_notify readiness, .deb/.rpm packages)

This directory holds the two service units, the packaging scripts, and
the packaging notes. The package definitions live in
`crates/volvisord/Cargo.toml` (`[package.metadata.deb]` for cargo-deb,
`[package.metadata.generate-rpm]` for cargo-generate-rpm); the
build-and-smoke pipelines are `.github/workflows/release.yaml` (the
release) and the `package-*` jobs in `.github/workflows/ci.yaml`
(per-change packaging regression gates).

## What the packages install where

| Path | Contents |
|---|---|
| `/usr/bin/volvisord`, `/usr/bin/volvisor-witnessd` | the two binaries from the one package (ADR-0009: one package, separate units) |
| `/usr/lib/systemd/system/volvisord.service` | from `volvisord.service` here |
| `/usr/lib/systemd/system/volvisor-witnessd.service` | from `volvisor-witnessd.service` here |
| `/etc/volvisor/volvisor.toml` | from `examples/volvisor.toml` (annotated; must pass its own `--check-config` — a repository test pins that) |
| `/etc/volvisor/witnessd.toml` | from `examples/witnessd.toml` (same discipline) |
| `/usr/share/doc/volvisor/` | `docs/configuration.md` as `configuration.md`, this file as `packaging.md`, and `LICENSE` |
| `/etc/volvisor/volvisor.env` | the operator's `EnvironmentFile` (optional; the units spell it with `-`, absent is fine) — never shipped |
| `/var/lib/volvisor/`, `/var/lib/volvisor-witness/` | created by the units' `StateDirectory=` at start, not by the package |

### The config-file policy: annotated examples as real configs, conffiles

The example configs ship **as the real config paths the units
reference** — an install is complete and `systemctl start` reaches a
working `ExecStartPre` (which runs `--check-config`, not a start) —
and the "never clobber the operator" guarantee is the package
manager's own, not a maintainer script:

- **deb**: files under `/etc/` are dpkg conffiles. An upgrade of an
  unmodified config updates it silently; a locally modified config is
  preserved with a prompt, never silently overwritten.
- **rpm**: the same assets carry `%config(noreplace)`. An upgrade of a
  locally modified config installs the new one beside it as
  `.rpmnew` and leaves the operator's file in place.

The annotated examples contain placeholder secrets and provider
choices on purpose; starting a daemon on an unedited example fails
closed exactly as it should (the drbd example without drbd-utils,
the placeholder witness secret against any real peer).

### No maintainer scripts, nothing auto-enabled

Neither package ships `postinst`/scriptlets and neither enables or
starts anything. After configuring, the operator runs:

```sh
systemctl enable --now volvisord.service volvisor-witnessd.service
```

(first start on a fresh install needs no `daemon-reload`: the units
were never loaded; after a package **upgrade** replaces a unit file,
run `systemctl daemon-reload` before restarting — no upgrade
transitions are tested, see the verification boundary below).

## Dependency honesty (ADR-0009)

- `lvm2` is the one **hard** runtime dependency (`Depends:` /
  `Requires:`): every provider path runs LVM commands.
- `drbd-utils` (**>= 9**) is **recommended**, not required (deb
  `Recommends:`, rpm `Recommends:`): the nearline provider needs it,
  the lvm/ceph paths do not. The **DRBD 9 kernel module is a separate
  reality the package documents rather than pretends**: mainline
  Linux carries DRBD 8.4 only; DRBD 9 is out-of-tree (LINBIT).
  Debian and SLES ship 9.x kernel modules in their kernels; RHEL and
  derivatives need a module from ELRepo or LINBIT. Where no DRBD 9
  module exists, drbd providers fail closed — they do not degrade.
- `ceph-common` is **suggested** (optional): the ceph provider shells
  out to `rbd` only when a ceph volume is configured.

## The version discipline

Both binaries print the P7-A stamp: the raw
`git describe --tags --always --dirty` of the build's checkout
(compiled in by `build.rs`; the crate version is only the fallback
outside a git checkout). `package-version.sh` derives the **package
version from the same describe**, sanitized to what dpkg and rpm
accept in a Version field (no `-`; `+` after the tag so post-tag
commits sort after it; `~g<hash>` when no tag is reachable — dpkg
requires a version to start with a digit, so a bare describe hash
cannot be a version at all — and `.dirty` for a dirty tree). The
build pipelines assert the binary's `--version` equals the
checkout's own describe *before* packaging, so binary and package
always carry one version. Examples:

| describe | package version |
|---|---|
| `v0.1.0` | `0.1.0` |
| `v0.1.0-12-g52ad001` | `0.1.0+12.g52ad001` |
| `v0.1.0-12-g52ad001-dirty` | `0.1.0+12.g52ad001.dirty` |
| `d411c68` (no tags yet) | `0.1.0~gd411c68` |

## The release process

1. A maintainer pushes a **signed** `v*` git tag. **Release tags are
   `v<semver>`** (e.g. `v0.1.0`) — digits and dots, no hyphens. The
   shape is enforced, not just documented: `package-version.sh`
   refuses a hyphen-bearing tag outright (a pre-release like
   `v0.1.0-rc1` would sanitize to `0.1.0+rc1`, which sorts *after*
   the release — the wrong order), and the release workflow's
   `guard` job fails any ref whose mapped version does not lead with
   a digit (a `vtest` tag stops there, before any build or publish
   attempt). Pre-release tags are outside this pipeline's
   convention.
2. `.github/workflows/release.yaml` runs: the full gate set (fmt,
   clippy on both feature sets, workspace tests, cargo doc, the
   real-LVM integration suite), the **campaign certification**
   (`campaign-summary --all --check` over freshly emitted evidence), and
   the **real-backend gate** for every provider declared `supported` in
   [`release-scope.toml`](../release-scope.toml) for the exact tagged
   commit; then the release build
   (x86_64-unknown-linux-gnu — the only target this pipeline
   builds), the `.deb`, the `.rpm`, the binary tarball, `SHA256SUMS`,
   `BUILD_INFO.txt` (the exact toolchain, per ADR-0009's provenance
   discipline: signed tags + checksums, not package signatures), and the
   per-provider log/JUnit evidence bundle; then the container smokes
   below; then the GitHub release with the certification and the
   generated provider scope embedded in its notes.
   Providers declared `not-run` are explicitly **not claimed** by that
   release; publish is refused if a `supported` provider's suite did not
   run and pass. See
   [`docs/testing/real-backends.md`](../docs/testing/real-backends.md).
3. `workflow_dispatch` on any branch runs the identical graph
   **without** creating the release — that is how the pipeline is
   proven before any tag exists.

The per-change `package-build`/`package-smoke` jobs in `ci.yaml`
build and smoke the same packages on every PR and push, so packaging
regressions surface at review time, not release time.

## What the container smoke covers

`smoke.sh` runs as root inside a stock `debian:stable` and
`fedora:latest` container with the built package in the current
directory. It verifies: installation **with dependency resolution**
(the hard `lvm2` must install from the distro repo), the package's
Version field, both binaries' `--version` stamps, `--check-config`
against the shipped example configs at their installed paths, the
shipped file layout (units, configs, docs), and `systemd-analyze
verify` of the installed unit files. One honest quirk: the official
fedora container images set `tsflags=nodocs`, so the smoke drops
that base-image exclusion — it is there to verify what the package
*carries*, and a host that deliberately runs `nodocs` excludes docs
by its own (correct) policy.

What the smoke does **not** cover (recorded, not skipped): **unit
start** — the containers do not run systemd as PID 1; that path was
verified against a real systemd (255.4) in P7-B's sandbox smoke and
is not re-proven per release. No upgrade/downgrade transitions. One
architecture. The `rbd map` carve-out against a live ceph host, and
DRBD against real peers, remain hardware-gated.

The `RuntimeDirectory=`/`StateDirectory=` directives are why this
package ships **no tmpfiles.d rule**: systemd creates `/run/volvisor`
and `/var/lib/volvisor` (and the witness tree) in the unit's own
namespace at start, with the right ownership, and makes them
writable under `ProtectSystem=strict`. The one runtime directory the
daemon depends on that it does not own — LVM's `/run/lock/lvm` — is
lvm2's own tmpfiles.d responsibility, not ours.

## The readiness contract

Both units are `Type=notify`. The daemons send `READY=1` — and only
that — over `$NOTIFY_SOCKET` when, and only when, their startup is
actually complete:

- `volvisord`: journal lock + replay, provider construction and its
  startup reconcile, **and** the API listener bound;
- `volvisor-witnessd`: witness journal open (lock + replay) **and**
  the listener bound.

Every failure path before those points returns without notifying, so
the service manager sees a start failure at its own timeout and
applies `Restart=on-failure` — which re-enters the same idempotent
startup path (replay and reconcile are idempotent by design; that is
the P6 crash discipline). A *send* failure inside the daemon (e.g. a
removed socket) is logged as a structured warning and never fails
startup: under `Type=notify` the manager's timeout is the operator's
signal, and killing a serving daemon over a notification hiccup
would trade availability for no safety.

The implementation is a hand-rolled `sd_notify(3)` client
(`crates/volvisord/src/notify.rs`) with no new dependencies: one
UNIX datagram, `READY=1\n`. An `@`-prefixed `$NOTIFY_SOCKET` is an
abstract-namespace socket spelled with the NUL substitution the
protocol defines. An unset `$NOTIFY_SOCKET` is a documented no-op —
standalone runs are unaffected. The placement is pinned from both
sides in `crates/volvisord/tests/notify_e2e.rs`: a daemon that fails
reconciliation never reports ready, a daemon that is reconciled but
cannot bind never reports ready, and only the fully-started daemon's
`READY=1` datagram exists.

## The write-set research (behind `ReadWritePaths=`)

Under `ProtectSystem=strict` the entire hierarchy is read-only
except `/dev`, `/proc`, `/sys` and the explicitly listed paths, so
the list must cover the external tools' writes, not just the
daemon's own state. The discipline, honestly stated (review finding
F-1): **load-bearing paths are listed unconditionally** — a missing
one should fail the unit loudly at start — while **cache/hints
paths are best-effort with the `-` prefix**: they may not exist on
fresh installs, and losing them degrades to tool warnings, never to
a failed mutation. The list is researched against the tools'
observed behavior on the reference host, not a claim of exhaustive
coverage of every write any distro build might attempt.

- `/var/lib/volvisor`, `/run/volvisor` — the daemon's journals,
  provider state, migration/handoff records, and the runtime socket
  tree (the VMM `--api-socket` convention) respectively.
- `/etc/lvm/archive`, `/etc/lvm/backup` — LVM 2.03 writes a metadata
  archive and a backup on **every VG metadata change** (`lvcreate`,
  `lvremove`, `pvmove`, `vgextend`, ...). These are load-bearing:
  without write access every LVM mutation fails. Both directories
  ship with the `lvm2` package (verified present on the reference
  host, LVM 2.03.16), so they are listed without the `-` prefix.
- `-/etc/lvm/cache` — the LVM device cache (`write_cache_state`);
  commands may rewrite it when device state drifts. The directory
  does not exist on a fresh install (verified: absent on the
  reference host until first written), hence the `-` prefix.
- `-/run/lvm` — LVM's runtime state (`hints`, `pvs_online`,
  `vgs_online`), opened `O_RDWR` on every mutation per LVM 2.03
  (strace-verified in review). Read-only access degrades to
  cache/hints warnings — non-fatal — but the write is real, so the
  path is listed. `-` because lvm creates it at first use.
- `-/run/lock/lvm` — LVM's global lock directory, created by lvm2's
  own tmpfiles.d at boot; `-` for the same reason.
- `-/etc/drbd.d` — where the DRBD provider writes its resource
  configs (the `drbd_config_dir` default). Present exactly when
  `drbd-utils` is installed, hence `-`.
- `-/var/lib/drbd`, `-/run/drbd` — drbd-utils state/runtime
  directories, included conservatively (drbd-utils is not installed
  on the reference host, so these could not be observed directly;
  the cost of including them if unused is nil).
- `-/sys/bus/rbd` — the ceph provider's attach path maps images
  (`rbd map`), which writes `/sys/bus/rbd/add`: a kernel interface,
  not a filesystem write, that `ProtectKernelTunables`'s read-only
  `/sys` would otherwise block. `ReadWritePaths` re-widenes the
  subtree over the read-only mount (the mechanism is verified in
  the P7-B smoke below; the rbd-specific behavior needs a real
  cluster and is P7-C's). `-` because the path exists only where
  the rbd module is loaded.
- The ceph provider's other children (`ceph`, `rbd` CLIs:
  create/remove/ls/info) are cluster-side metadata operations with
  no local filesystem writes (review finding F-6) — no functional
  gap in the write set beyond the sysfs carve-out above.
- `/proc/drbd` is read, never written — readable under strict
  without a listing.

`RestrictAddressFamilies` keeps `AF_NETLINK` for the storage daemon:
`drbdsetup` administers the kernel module over generic netlink, and
LVM builds with libudev integration use kobject-uevent netlink
sockets (the reference host's LVM 2.03.16 defaults
`obtain_device_list_from_udev = 0`, but distro builds vary — keeping
the family is the conservative choice). The witness runs no external
tools, so its unit drops `AF_NETLINK` and can also take
`PrivateDevices=true`.

Directives deliberately omitted (root/device reality), and the
reasoning, are documented in the header comment of each unit — never
silently weakened.

## The verification boundary

What is verified here:

- `systemd-analyze verify` on both units (syntax and directive
  validation), recorded in the P7-B delivery record;
- the readiness ordering, failure discipline and protocol payload
  pinned by the notify unit tests and the `notify_e2e` integration
  tests above;
- a one-time **unit-start smoke on this host's real systemd** (the
  development sandbox runs systemd 255.4 as PID 1 — review finding
  F-3 corrected an earlier claim here that it did not): the built
  binaries staged at `/usr/bin`, the units and a valid
  configuration installed at their real paths, `daemon-reload`,
  `start`, the unit observed reaching **active** through the
  `Type=notify` READY handshake, the API answering, stop, and full
  cleanup. The full transcript lives in the PR body (it is a
  delivery-time observation, not a permanent test — the packaged
  layout and its smoke belong to P7-C). The smoke also verified the
  `ReadWritePaths`-over-`ProtectKernelTunables` re-widening
  mechanism that the ceph `rbd map` carve-out relies on.

What is **not** verified here, on purpose: unit start under the
packaged install (the container smokes above cannot run systemd as
PID 1; the P7-B unit-start smoke on a real systemd is the closest
evidence, with the packaged layout verified by inspection — same
files at the same paths the smoke staged), package upgrade/downgrade
transitions, and provider-specific runtime behavior that needs real
hardware or clusters (the `rbd map` carve-out against a live ceph
host, DRBD against real peers). What cannot be sandboxed there is
recorded, not skipped.
