# Day-2 drill evidence — 2026-11-03, single-host development lab

This bundle records the executable day-2 drills behind issue
[#46](https://github.com/o3kio/volvisor/issues/46). It is a **single-host
development-lab** record, not production evidence, not multi-host evidence,
and not a data-plane durability proof.

- **Exact source:** `e551d26af7cdedb468797ff8049e0b91b9ab416d`
  (`git describe --tags --always` = `e551d26`).
- **Worktree at run time:** the raw headers record `worktree_dirty: no`,
  derived from `git status --porcelain` (tracked changes **and** untracked,
  non-ignored files). **The harness scripts are absent from `e551d26`'s
  tree**; had they been present untracked in the recorded worktree, the flag
  would have read `yes`. The raw logs therefore place the harness outside the
  recorded worktree and do not bind its bytes. Its first tracked revision is
  `d5af71f`, committed eight minutes after the run window closed.
  [SCRIPTS.txt](SCRIPTS.txt) records the SHA256 of every script at that first
  tracked revision and states the limit explicitly. The bundle commit itself
  is docs-only and was created after the run; see [REPORT.md](REPORT.md) for
  both identities.
- **Binary identity limit:** the raw drill headers record only `source_sha`
  and `worktree_dirty`; they do **not** record either binary's SHA256 or
  `--version` stamp. [BINARIES.txt](BINARIES.txt) is an authored record
  collected outside the raw captures, so it is corroborating metadata, not a
  byte-level proof anchored in the raw logs.
- **Run window:** 2026-11-03T11:23:29Z .. 2026-11-03T11:23:49Z.
- **Host:** one physical development host (`dev`, Ubuntu kernel
  `6.8.0-142-generic`), every daemon/client on `127.0.0.1`.
- **Binaries:** [BINARIES.txt](BINARIES.txt) — debug builds of `volvisord`
  and `volvisor-witnessd` from the recorded commit.
- **Environment and absent capabilities:** [ENVIRONMENT.txt](ENVIRONMENT.txt).

The procedures that produced this bundle are committed as executable scripts
under [`docs/runbooks/day-2/scripts/`](../../../runbooks/day-2/scripts/). The
human procedure and the honest per-area verdicts are in the
[day-2 runbook index](../../../runbooks/day-2/README.md).

## What was executed

| Drill | Area | Result | Raw record |
|---|---|---|---|
| D1 | Stopped-state control-plane backup, destructive drift, restore | PASS | [01-backup-restore.log](raw/01-backup-restore.log) |
| D2 | Schema compatibility test, upgrade-on-write, future-schema refusal | PASS | [02-schema-upgrade.log](raw/02-schema-upgrade.log) |
| D3 | Witness service stop/restart, lease epoch recovery, successor grant | PASS | [03-lost-witness.log](raw/03-lost-witness.log) |
| D4 | Torn journal tail, half-written frame, future frame, corrupt state file | PASS | [04-torn-journal-and-corrupt-state.log](raw/04-torn-journal-and-corrupt-state.log) |
| D5 | ENOSPC on the journal filesystem and on the provider-state filesystem | PASS, with a filed finding | [05-enospc.log](raw/05-enospc.log) |
| D6 | Five SIGKILLs with in-flight requests, journal lock contention | PASS | [06-crash-loop-and-lock.log](raw/06-crash-loop-and-lock.log) |
| D7 | Scoped token rotation and same-host TLS certificate rotation | PASS | [07-credential-and-cert-rotation.log](raw/07-credential-and-cert-rotation.log) |
| D8 | Total host loss and re-admission | **NOT RUN** | no raw record; see runbook |

Every executed drill ends with `ASSERT SUMMARY: 0 failures`. The scripts assert
observed values; they do not merely print them.

## What this bundle is not

- It is **not** production evidence and does not change
  [SUPPORT_MATRIX.md](../../../../SUPPORT_MATRIX.md).
- It is **not** multi-host. There is no second physical host, no third failure
  domain, and no independent witness host.
- It is **not** a DRBD/Ceph storage-side failover drill. `drbdadm`, the DRBD 9
  kernel module, `rbd` and `ceph` are absent on this host. The witness drill
  covers the witness **service** only; it does not prove that a writer freezes
  after losing its lease.
- It is **not** a real power cut, total host loss, or re-admission test.
- It is **not** a data-plane backup or restore. D1 backs up the control-plane
  state directory (journal + LVM provider state); it does not copy VM disk
  contents. The backing LVs were left in place.
- It is **not** a live old-binary → new-binary upgrade or a cross-version
  rollback. No older tagged binary exists. D2 drills the schema policy,
  upgrade-on-write and future-version refusal with one binary.
- It is **not** a benchmark. The RTO values in [REPORT.md](REPORT.md) are
  single-host lab measurements, not SLAs.

## Layout

```text
README.md          this file
REPORT.md          per-drill pass conditions, observations, verdicts, blockers
ENVIRONMENT.txt    host/toolchain identity and absent capabilities
BINARIES.txt       binary SHA256 digests and --version stamps
SCRIPTS.txt        harness provenance and first-tracked-revision SHA256 values
MANIFEST.sha256    SHA256 over every other file in this bundle
raw/               unmodified captured command stdout/stderr and daemon logs
records/           request/response JSON, status files, frame/state fixtures
journal/           selected on-disk journal files for forensic inspection
```

The raw logs are immutable copies from the run workspace; they contain the
original absolute paths under `/tmp/opencode/d2-46-final`. They are not
rewritten to match a narrative. A few captured stderr lines legitimately end
with a single space (the harness replaced newlines with spaces). The bundle's
`raw/.gitattributes` excludes those captures from `blank-at-eol` warnings so
the bytes stay untouched; that is a whitespace-check exception, not a content
edit. The reproduction scripts use a fresh `$RUN` directory and repo-relative
paths.

## Secrets and scrubbing

The run created disposable bearer tokens, curl configs and a throwaway private
CA and TLS keys outside the repository. None of `configs/`, `certs/`, the CA
private key or the TLS private keys were copied into this bundle. The raw
`openssl s_client` output contains the server's **public** certificate only;
the bundle was scanned for `Bearer `, `PRIVATE KEY`, and the generated token
prefixes before commit. No credential values are present.

## Reproduction

Requirements are Linux root, `lvm2`, loop devices, `util-linux`, `jq`,
`python3`, `openssl` and `curl`. Build the two binaries at the recorded commit,
then run the whole set:

```sh
cd /path/to/volvisor
cargo build
sudo RUN=/tmp/opencode/volvisor-day2-$(date -u +%Y%m%dT%H%M%SZ) \
  bash docs/runbooks/day-2/scripts/run-all.sh
```

The harness records `source_sha` and `worktree_dirty` at the start of every
drill. A dirty worktree (including untracked, non-ignored files) cannot
support the clean-SHA claim this bundle makes. This bundle additionally
discloses that the harness was not present in the recorded worktree at
`e551d26`; see [SCRIPTS.txt](SCRIPTS.txt). The harness is a lab harness: it
starts daemons directly (no systemd) and creates disposable loop-backed
volume groups.

Do **not** run these scripts on a host with production volume groups. Always
set a disposable `RUN` directory and inspect `lvm_lab_setup` before use.
