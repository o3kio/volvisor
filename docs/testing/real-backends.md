# Real-backend test rigs and the release gate

Status: **the LVM suite is runnable on this host; the Ceph and DRBD suites
are implemented and gated but are not green here.** This document records
the rig contract, the exact commands, and the deliberate non-claims. It
does not turn a scheduled run, a local run, or a compile-only run into
production-support evidence (AGENTS rule 12).

The one-command scripts are the only supported entry points:

| Suite | Script | Where it runs |
|---|---|---|
| `lvm` | `scripts/test-lvm.sh` | `ubuntu-latest` (or any Linux with loop + device-mapper) |
| `ceph` | `scripts/test-ceph.sh` | self-hosted `[linux, x64, volvisor-ceph]` |
| `drbd` | `scripts/test-drbd.sh` | self-hosted `[linux, x64, volvisor-drbd]` |

Every script validates prerequisites **before** running cargo and exits
`2` with a typed `FAILED [suite.reason]:` message when a prerequisite is
missing. A test binary that prints its skip note after a run is treated as
a failed gate. There is no `--allow-skip`, no `continue-on-error`, and no
workflow-level bypass.

## Current status (2026-11-02)

| Suite | Status here | Exact blocker |
|---|---|---|
| `lvm` | **Run locally** with `VOLVISOR_TEST_LVM=1`; see the command below. | None on this host (root, LVM 2.03.16, loop and device-mapper present). |
| `ceph` | **Blocked** | `ceph`/`rbd` CLIs absent, no reachable external cluster, no client keyring, and no runner registered with the `volvisor-ceph` label. |
| `drbd` | **Blocked** | No DRBD 9 kernel module, no `drbd-utils`, no second peer host/backing LV, and no runner registered with the `volvisor-drbd` label. |

No Ceph or DRBD green run is claimed by this document, this branch, or
the PR. The Ceph/DRBD scripts were exercised for their **loud failure**
path here; that is not a suite result. The first genuine green run must
be linked from issue evidence with its exact SHA, and `release-scope.toml`
must be flipped to `supported` only after that rig exists.

Local LVM evidence for this branch (2026-11-02, dev host): `./scripts/test-lvm.sh`
exited 0 with `110` tests passing, including all six real-loop-device
integration tests (`test result: ok. 6 passed; 0 failed`); the run's
`target/real-backends/lvm/` evidence is a local artifact and is not
committed. On the same host, `./scripts/test-ceph.sh` and
`./scripts/test-drbd.sh` exited `2` at `ceph.missing-tool.ceph` and
`drbd.missing-tool.drbdadm` respectively — loud prerequisite failures, not
skips and not suite results.

## The release gate (no silent bypass)

`release-scope.toml` is the machine-readable source of truth. It declares
each provider as `supported` or `not-run`:

```toml
schema = 1

[providers.native-local]
status = "supported"
suite  = "lvm"
claim  = "..."

[providers.ceph-rbd]
status = "not-run"
suite  = "ceph"
reason = "..."
```

`scripts/release-scope.py` validates the schema and is the only generator
of the gate inputs:

- `check` -- CI and the release `scope` job fail on an unknown status,
  missing reason, unknown suite, duplicate suite, or ambiguous claim.
- `matrix` -- emits the suites for every `supported` provider. The release
  workflow passes that matrix to the reusable `real-backends.yaml`.
- `notes` -- renders the support matrix embedded in the GitHub release.
  A `not-run` provider is printed as **not claimed**, with its reason.

The release graph is:

```text
guard -> scope -> real-backends (matrix, one reusable-workflow call per supported suite)
                    |
                    +--> real-backends-gate (if: always(), needs scope + matrix)
                    |
                    +--> evidence-bundle (if: always(), downloads real-backends-*)
                              |
build, certification, smoke ---+--> publish (tag only)
```

`publish` (the release job) has a hard `needs` on `real-backends-gate` and
`evidence-bundle`. A supported provider whose runner is missing, whose
secret is absent, whose suite fails, or whose suite is skipped fails the
matrix job; the gate inspects `needs.real-backends.result` and exits
non-zero, so publish never runs. The **only** case where nothing runs is
`supported_count == 0`, which is printed by the gate and by the release
notes as "no provider claimed". An unknown/unparseable scope also fails
the gate, never passes it.

`scripts/lib/check-release-graph.rb` asserts those structural properties
on every CI run: the publish job needs the gate, the gate runs
`always()`, the matrix comes from the scope output, the reusable workflow
calls the one-command scripts, and the Ceph/DRBD jobs still target the
documented labels.

### Why not just `if: success()` on publish?

Because a skipped or cancelled real-backend job would then be inherited
as a successful/skipped dependency, and a missing runner could look like
"nothing to do". The explicit aggregator treats *any* non-`success`
matrix result as a failure unless the scope itself says zero providers
are claimed. Flipping a provider to `supported` therefore makes the next
release require its rig; there is no way to claim it while the suite is
not run.

## Rig topology

### LVM rig (locally runnable)

- One Linux host, root (or passwordless sudo).
- `lvm2`, `util-linux` (`losetup`), device-mapper, loop device support.
- At least 1 GiB free under the evidence/output directory.
- The suite creates a private 256 MiB backing file, a loop device and a
  temporary VG, then removes them. It does not touch an operator VG.
- `pvmove --abort` is host-wide; the suite serializes real-LVM tests on a
  host lock. Do not run two LVM suites concurrently on one host (the
  workflow also has a concurrency group per suite).

### Ceph rig

- An **externally operated** Ceph cluster. Volvisor is an RBD adapter; it
  does not create OSDs, pools, or a replacement Ceph engine (AGENTS rule
  9, ADR-0005).
- A **dedicated** pool for the test images. Never point this at a
  production pool: the suite creates a fixed-name image, maps/grows it,
  moves it to the RBD trash, and asserts the trash placement.
- A client entity with the least-privilege caps below; the provider passes
  the full entity name via `--name` (for example `client.volvisor`).
- Monitor addresses and the exact FSID; the provider refuses a mismatched
  FSID (`FOREIGN_DEVICE_STATE`) before writing anything.
- Runner: `ceph-common` (`ceph`, `rbd`), a kernel with `CONFIG_BLK_DEV_RBD`
  (the `rbd` module is loaded/loadable), `/sys/bus/rbd` present, root for
  `rbd map`, and network access to the monitors/OSDs.
- A keyring delivered by the runner or by the CI secret (see below).
  Cluster health should be `HEALTH_OK`; a non-OK health is retained as a
  warning in the evidence and must be triaged by the storage owner.

### Least-privilege credentials

Suggested least-privilege creation (run by the Ceph administrator, not
committed):

```sh
ceph auth get-or-create client.volvisor \
  mon 'allow r' \
  osd 'allow rwx pool=volvisor-test' \
  mgr 'allow r'
ceph auth get client.volvisor -o /etc/ceph/ceph.client.volvisor.keyring
chmod 600 /etc/ceph/ceph.client.volvisor.keyring
```

### DRBD rig

Two hosts; the Git runner is the primary, and the peer is a second host
on the same L2/L3 replication path.

- Both hosts: DRBD **9** kernel module (mainline 8.4 is rejected), the
  same `drbd-utils` 9.x generation, and `drbdadm`/`drbdsetup`/`drbdmeta`.
- Primary runner: a dedicated VG (for example `volvisor-test`) with at
  least 256 MiB free, and a writable config directory (normally
  `/etc/drbd.d`). The provider writes generated `volvisor-*.res` files
  there and removes them on delete.
- Peer: an operator-pre-provisioned backing LV at the **same**
  `/dev/<vg>/<lv>` path the generated definition names, fresh (never
  seeded), at least as large as the grown size (128 MiB; 256 MiB is the
  safe default). The suite does not provision the peer for you.
- `VOLVISOR_DRBD_NODE_NAME` must equal the primary's `uname -n`; local and
  peer addresses must be IPv4; the peer port is the DRBD replication port.
- A shared-secret file owned by root with mode `0600` (or `0400`).
- This is a single-primary lifecycle test (create, seed, promote, grow,
  demote, delete/Retain). It does **not** authorize temporary dual-primary
  migration (AGENTS rule 17).

## Packages

| Host | Packages / prerequisites |
|---|---|
| LVM runner | `lvm2`, `util-linux`, `device-mapper`, `ca-certificates`, `git`, Rust stable (`rustup`/toolchain action), `python3` |
| Ceph runner | `ceph-common`, kernel `rbd` module (`linux-modules-extra-$(uname -r)` on Ubuntu; distro equivalent), `lvm2`, `util-linux`, `git`, Rust stable, `python3`, `sudo` |
| DRBD primary | `drbd-utils` >= 9, a DRBD 9 kernel module (distribution package or LINBIT/ELRepo), `lvm2`, `util-linux`, `iproute2`, `git`, Rust stable, `python3`, `sudo` |
| DRBD peer | the same DRBD 9 module + `drbd-utils`; the pre-provisioned backing LV |

The GitHub-hosted LVM job installs `lvm2` itself; the self-hosted rigs
must be provisioned (and version-pinned) by the infrastructure owner. The
workflow installs the Rust toolchain via `dtolnay/rust-toolchain@stable`
and uses `Swatinem/rust-cache`; the rig must allow those actions.

## Runner labels and registration

The workflows target exactly:

| Suite | `runs-on` |
|---|---|
| lvm | `ubuntu-latest` |
| ceph | `[self-hosted, linux, x64, volvisor-ceph]` |
| drbd | `[self-hosted, linux, x64, volvisor-drbd]` |

Register **dedicated** runners (not a production storage host). They need:

- the repo checked out by `actions/checkout`, network access to the
  cluster/peer, and the Linux x64 label;
- the package set above;
- root execution or passwordless `sudo` for the script (the workflow uses
  `sudo -E env "PATH=$PATH"`), because `rbd map`, `drbdadm` and
  device-mapper need it;
- no other job sharing the destructive test pool/VG. The per-suite
  `concurrency` groups serialize runs but do not protect a production
  resource from a misconfigured pool.

The missing labels are the exact reason Ceph/DRBD are `not-run` today.
Do not fake a label on a hosted runner: the scripts will fail on the
missing kernel module/CLI/peer, which is the intended behaviour.

## Configuration: repository variables and secrets

Non-secret rig inventory belongs in GitHub **repository variables**; key
material belongs in **secrets**. Nothing in this table is committed, and
no real inventory is stored in this repository.

### Ceph

| Name | Kind | Required | Meaning |
|---|---|---|---|
| `VOLVISOR_CEPH_FSID` | variable | yes | exact `ceph fsid` |
| `VOLVISOR_CEPH_MONS` | variable | yes | comma-separated monitors passed to `-m` |
| `VOLVISOR_CEPH_POOL` | variable | yes | dedicated test pool |
| `VOLVISOR_CEPH_USER` | variable | yes | full entity, e.g. `client.volvisor` |
| `VOLVISOR_CEPH_KEYRING_B64` | secret | optional | base64 of the keyring file; omit if the runner has the conventional keyring |
| `VOLVISOR_CEPH_KEYRING_FILE` | variable/path | optional | mode-0600 keyring path (local runs only) |
| `VOLVISOR_TEST_OUTPUT_DIR` | variable/path | optional | evidence directory |

### DRBD

| Name | Kind | Required | Meaning |
|---|---|---|---|
| `VOLVISOR_DRBD_VG` | variable | yes | dedicated primary VG |
| `VOLVISOR_DRBD_CONFIG_DIR` | variable | yes | writable config dir, normally `/etc/drbd.d` |
| `VOLVISOR_DRBD_NODE_NAME` | variable | yes | primary `uname -n` |
| `VOLVISOR_DRBD_LOCAL_ADDRESS` | variable | yes | primary replication IPv4 |
| `VOLVISOR_DRBD_PEER_NAME` | variable | yes | peer `on` name |
| `VOLVISOR_DRBD_PEER_ADDRESS` | variable | yes | peer `<ipv4-or-host>:<port>` |
| `VOLVISOR_DRBD_SECRET` | secret | optional | shared secret content; script writes a 0600 temp file |
| `VOLVISOR_DRBD_SECRET_FILE` | variable/path | optional | mode-0600 secret path (local runs only) |
| `VOLVISOR_DRBD_MIN_FREE_BYTES` | variable | optional | minimum VG free bytes (default 256 MiB) |

The `ceph`/`drbd` workflows pass secrets through `env`, never through
command-line arguments or committed files. The scripts:

- decode a keyring into `mktemp` with mode `0600`, set
  `CEPH_ARGS=--keyring <temp>`, and remove it in an `EXIT` trap;
- write `VOLVISOR_DRBD_SECRET` to a mode-0600 temp file, set
  `VOLVISOR_DRBD_SECRET_FILE`, and remove it in the same trap;
- refuse a secret file with group/other permission bits;
- never copy key material into `target/real-backends/`.

Rotate the Ceph client key and the DRBD shared secret after a runner is
retired. A leaked key is a storage-security incident, not a CI retry.

## Exact commands

### LVM (locally runnable, and the `native-local` release gate)

```sh
# prerequisites only -- does not run tests
sudo ./scripts/test-lvm.sh --check

# full gate; evidence under target/real-backends/lvm/
sudo env "PATH=$PATH" ./scripts/test-lvm.sh

# the underlying command, for debugging (the script is preferred)
sudo -E env "PATH=$PATH" VOLVISOR_TEST_LVM=1 \
  cargo test --locked -p volvisor-lvm --features integration-lvm
```

### Ceph

```sh
sudo -E env "PATH=$PATH" \
  VOLVISOR_CEPH_FSID="$(ceph fsid)" \
  VOLVISOR_CEPH_MONS="mon-a:6789,mon-b:6789" \
  VOLVISOR_CEPH_POOL=volvisor-test \
  VOLVISOR_CEPH_USER=client.volvisor \
  ./scripts/test-ceph.sh --check

sudo -E env "PATH=$PATH" \
  VOLVISOR_CEPH_FSID=... VOLVISOR_CEPH_MONS=... \
  VOLVISOR_CEPH_POOL=... VOLVISOR_CEPH_USER=client.volvisor \
  ./scripts/test-ceph.sh
```

In CI, the reusable workflow supplies the variables/secrets; a manual
`workflow_dispatch` with `suite: ceph` is the supported rerun. The weekly
schedule runs only suites declared `supported` in `release-scope.toml` (today:
`lvm`), so a not-run provider is not turned into recurring red noise; a
`workflow_dispatch` with `suite: all` deliberately runs every rig for bring-up.

### DRBD

```sh
sudo -E env "PATH=$PATH" \
  VOLVISOR_DRBD_VG=volvisor-test \
  VOLVISOR_DRBD_CONFIG_DIR=/etc/drbd.d \
  VOLVISOR_DRBD_NODE_NAME="$(uname -n)" \
  VOLVISOR_DRBD_LOCAL_ADDRESS=10.0.0.11 \
  VOLVISOR_DRBD_PEER_NAME=peer-a \
  VOLVISOR_DRBD_PEER_ADDRESS=10.0.0.12:7990 \
  VOLVISOR_DRBD_SECRET_FILE=/root/.config/volvisor/drbd-secret \
  ./scripts/test-drbd.sh
```

### Release and scheduled runs

```sh
# prove the graph without publishing
gh workflow run release.yaml --ref main

# run every rig on demand (bring-up; runs even if scope says not-run)
gh workflow run real-backends.yaml --ref main -f suite=all
# or one specific rig
gh workflow run real-backends.yaml --ref main -f suite=ceph

# inspect the last scheduled/dispatch run and its evidence
gh run list --workflow real-backends.yaml --limit 5
gh run view <run-id> --log
gh run download <run-id> -n real-backends-ceph -D /tmp/real-backends-ceph

# local structural validation used by CI
python3 scripts/release-scope.py check
python3 scripts/release-scope.py notes --sha "$(git rev-parse HEAD)" --gate-state required
ruby scripts/lib/check-release-graph.rb \
  .github/workflows/release.yaml .github/workflows/real-backends.yaml
bash -n scripts/test-lvm.sh scripts/test-ceph.sh scripts/test-drbd.sh \
  scripts/lib/real-backends-common.sh
shellcheck -x scripts/test-lvm.sh scripts/test-ceph.sh scripts/test-drbd.sh \
  scripts/lib/real-backends-common.sh
```

## Artifacts and retention

Each `real-backends-<suite>` artifact contains:

| File | Content |
|---|---|
| `run.log` | raw cargo/libtest output (the primary failure evidence) |
| `junit.xml` | JUnit conversion of the same run; a failed preflight is a `<failure>` |
| `preflight.log` | typed prerequisite checks and recorded versions |
| `errors.txt` | machine-greppable typed gate failures, if any |
| `warnings.txt` | non-fatal warnings (for example Ceph `HEALTH_WARN`) |
| `environment.txt` | SHA, describe, kernel, `id`, tool versions, non-secret rig config |
| `command.txt` | the exact cargo command |
| `status.txt` | start/finish UTC, exit code, pass/fail |

Per-run artifacts are retained for **90 days** (the repository maximum in
the workflow). `release.yaml`'s `evidence-bundle` job downloads every
`real-backends-*` artifact and attaches `volvisor-<version>-real-backends-evidence.tar.gz`
to the GitHub release; that asset is the permanent release evidence and
is covered by `SHA256SUMS`. The gate manifest records the commit, run URL,
scope result, matrix result and gate result, so a release can be audited
without trusting the release notes text.

No secrets are uploaded: the evidence writer records configuration names
and versions, never key bytes, keyring contents, or the shared secret.

## Failure triage

| Symptom | First check | Owner | Evidence |
|---|---|---|---|
| `FAILED [ceph.missing-env.*]` | repository variables set? | CI/infra owner | `preflight.log`, run summary |
| `FAILED [ceph.missing-tool.*]` | `ceph-common` and `rbd` module on the runner | Ceph infrastructure owner | runner image record |
| `FAILED [ceph.unreachable]` / `fsid-mismatch` | monitor reachability, keyring caps, exact FSID | Ceph storage owner | `ceph-fsid.stderr`, `preflight.log` |
| `FAILED [ceph.pool-unavailable]` | dedicated pool exists and client caps | Ceph storage owner | `ceph-pool.stderr` |
| `FAILED [ceph.tests-failed]` | test assertion, cluster health, stale image in pool | Volvisor maintainer + Ceph owner | `run.log`, `junit.xml` |
| `FAILED [drbd.kernel-module]` | DRBD 9 module (not 8.4) loaded | DRBD infrastructure owner | `drbdadm --version` in `environment.txt` |
| `FAILED [drbd.vg-missing]` / `vg-too-small` | dedicated VG and free space | DRBD infrastructure owner | `vgs.stderr`, `environment.txt` |
| `FAILED [drbd.node-name-mismatch]` | runner hostname vs variable | CI/infra owner | `preflight.log` |
| `FAILED [drbd.peer-unresolvable]` | peer DNS/address, replication network | DRBD infrastructure owner | `preflight.log` |
| `FAILED [drbd.tests-failed]` | peer backing LV, `drbdadm status`, kernel log | Volvisor maintainer + DRBD owner | `run.log`, `junit.xml` |
| `FAILED [*.suite-skipped]` | env not passed through `sudo -E` | CI/infra owner | `run.log`, `preflight.log` |
| `real-backends-gate` failed | matrix result in the job summary | Volvisor release manager | run URL, `real-backends-evidence` |

The triage path is: **storage owner fixes the rig; Volvisor maintainer
fixes provider/test bugs; CI owner fixes runner/secret plumbing.** The run
URL and artifacts live on the failed workflow run and in the release
evidence bundle. Do not "fix" a failure by flipping the provider to
`not-run` without recording the reason and the withdrawal in the release
notes (that is an explicit non-claim, not a pass). A red gate always
blocks publish; use `workflow_dispatch` to reproduce before re-tagging.

## Limits of this gate

- A green suite is release-scope evidence, not production support. The
  failure campaign and exact-hardware evidence required by SPEC-0002 and
  the relevant v2 contract still apply.
- A scheduled green run at an exact SHA is necessary for a provider claim;
  the release gate runs the suite again at the release commit, because a
  prior scheduled run does not prove the tagged tree.
- The DRBD suite is a single-primary lifecycle test; it does not claim
  dual-primary migration, split-brain immunity, or zero RPO.
- The Ceph suite uses an existing external cluster and a dedicated pool;
  it does not claim OSD management, cluster recovery, or production
  pool policy.
- The `lvm` suite uses loop devices, which are intentionally excluded from
  discovery (no stable WWN/serial). A green loop-device run is not a
  real-NVMe hardware qualification; it proves the provider lifecycle
  against real LVM/driver semantics.
