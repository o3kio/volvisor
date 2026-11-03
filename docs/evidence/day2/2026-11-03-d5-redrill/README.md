# D5 post-fix re-drill evidence — 2026-11-03, single-host development lab

This bundle is a **post-fix re-drill of D5 (disk full / ENOSPC)** on one
development host. It re-runs the executable drill behind the state-save
finding [#58](https://github.com/o3kio/volvisor/issues/58) after the fix
merged as [PR #69](https://github.com/o3kio/volvisor/pull/69), and re-confirms
the journal-filesystem path. It is **not** a new campaign, not production
evidence, and not a release artifact.

- **Product code:** merged `main`
  `1863a7ac250d20246c328a2fd4c4acca330e3693` = PR #69 ("Fix #58: fail closed
  when LVM create state save fails").
- **Harness / drill commit:** `940722ea3322df0a47834c307e2e0081c57e0b5e`
  (`git describe --tags --always` = `940722e`). Three commits sit on top of
  `1863a7a`: `c9458423b018e8293da543b04a79b65ee79df726` rewrote D5 part B,
  `863c4c83f90cee27055e297226d416a7daef9148` fixed the lab-teardown ownership
  guard, and `940722e` counted `cap` failures in the parent shell, marked D3's
  intentional transport failure with `CAP_EXPECT_RC`, hardened the tmpfs setup
  and assertions (including short-write accounting and a baseline-`Ready`
  assertion), escalated a TERM-ignoring daemon to KILL so it cannot leak the
  lab, and added the Part A `INTERNAL`/torn-append assertions.
- **Product code is unchanged between `1863a7a` and `940722e`.**
  `git diff --name-only 1863a7a 940722e -- crates/ Cargo.toml Cargo.lock
  release-scope.toml` is empty, and `git diff --stat 1863a7a 940722e` shows
  only `docs/runbooks/day-2/scripts/d3.sh`,
  `docs/runbooks/day-2/scripts/d5.sh` and
  `docs/runbooks/day-2/scripts/lib.sh`.
- **Raw drill header:** `# source_sha:
  940722ea3322df0a47834c307e2e0081c57e0b5e`, `# worktree_dirty: no`,
  `# runner: uid=0`, window 2026-11-03T20:21:59Z .. 2026-11-03T20:22:02Z.
- **Host:** `dev`, kernel `6.8.0-142-generic` (both from the raw header).
  `x86_64`, 16 CPUs and 31 GiB were collected at bundle-authoring time on the
  same host; the drill does not record CPU count, memory or architecture.
- **Binary:** `target/debug/volvisord`, debug profile, SHA256
  `b40a97c307194270d7cfd69bb1463ad2a2cc2ef0b43f48950862a37434f602f5`,
  `--version` = `volvisord 940722e`. Only `volvisord` was used;
  `volvisor-witnessd` was not used. See [BINARIES.txt](BINARIES.txt).
- **Harness hashes:** the `d5.sh`, `lib.sh` and `d3.sh` bytes are bound to this
  run because they are tracked at `940722e`, the same commit the raw header
  records, and the run declared a clean worktree. See [SCRIPTS.txt](SCRIPTS.txt).
- **Tags:** none. `git tag --list` is empty.
- **Bundle commit:** the commit that contains this directory is a docs-only
  child of `940722e`. Its own SHA is not embedded (self-reference). If the PR
  is squash-merged, that merge commit is the durable one: the same harness
  bytes and product tree are in it, and [SCRIPTS.txt](SCRIPTS.txt) gives the
  SHA256 values to verify. If `940722e` is not reachable from the merged
  history, verify the scripts against the commit that contains this bundle
  rather than treating `940722e` as still fetchable.

The pre-fix bundle [`docs/evidence/day2/2026-11-03-devhost/`](../2026-11-03-devhost/README.md)
stays pinned to `e551d26af7cdedb468797ff8049e0b91b9ab416d` and is **not edited**
by this work. It remains the historical record of the finding; this bundle is
the post-fix re-drill.

## What ran

| Part | Fault | Result | Raw record |
|---|---|---|---|
| A | Journal filesystem ENOSPC, fake provider | **PASS** — 500, volume 404, torn append truncated, retry 200 | [raw/05-enospc.log](raw/05-enospc.log) |
| B | Provider-state filesystem ENOSPC, LVM on a loop VG, journal on ordinary storage | **PASS** — 500, old state byte-identical, LV compensated, same-process and restart 404, terminal replay, new `operation_id` 200/Ready | [raw/05-enospc.log](raw/05-enospc.log) |

The log contains 51 `ASSERT PASS` lines; the assertion output closes with
`ASSERT SUMMARY: 0 failures`, followed only by teardown output and the
lab-teardown line. The separate orchestrator capture of the process stdout
([raw/orchestrator-console.txt](raw/orchestrator-console.txt)) ends with
`D5 COMPLETE`, which the script prints only on its success branch;
[raw/orchestrator-run.txt](raw/orchestrator-run.txt) records the command, the
start and end timestamps and exit status 0.

## Layout

```text
README.md          this file
REPORT.md          pre-stated pass condition, identity, observations, verdict, limits
ENVIRONMENT.txt    host, toolchain, run window, absent capabilities
BINARIES.txt       build identity and binary SHA256 / --version
SCRIPTS.txt        harness hashes and why they are byte-bound this time
MANIFEST.sha256    SHA256 over every other file in this bundle
raw/               unmodified drill log, per-incarnation daemon logs, orchestrator stdout and run record, post-run capture
records/           request/response JSON, pad records, state snapshots, last.status
journal/           the LVM daemon journal (orchestrator copy after the run;
                   see ENVIRONMENT.txt "Raw log retention")
```

## Limits and non-claims

- **One host.** Every daemon, client and the lab ran on `127.0.0.1` on `dev`.
  No second physical host, no independent failure domain, no multi-host path.
- **Synthetic ENOSPC only.** The full filesystems are 256 KiB tmpfs mounts.
  This is not real disk, inode, thin-pool data or LVM metadata exhaustion. The
  harness verifies each mount is `tmpfs` before filling it and caps the filler
  at 512 KiB, so a failed mount cannot fill the host filesystem.
- **Loop LVM.** Part B uses a loop-backed volume group created by the lab; it
  is not a real NVMe, SAN, Ceph or DRBD backend.
- **Debug build.** No release build, no tagged artifact, no packaged install.
- **Normal path only.** Part B fault-injects the normal uncommitted-save
  failure. The cleanup-failure, post-rename carve-out and unreadable-durable-
  reread branches were **not** fault-injected here; they are covered by the
  deterministic tests added in #69. See [REPORT.md](REPORT.md).
- **No concurrency, no kill.** There was no concurrent writer and no `kill -9`
  during the failure window; the daemon was stopped gracefully between phases.
- **Not production evidence.** No SLA, no zero-RPO claim, no release or tag,
  and no change to any gate verdict in
  [`docs/releases/pre-production-readiness.md`](../../../releases/pre-production-readiness.md).
  The support posture is unchanged in
  [`SUPPORT_MATRIX.md`](../../../../SUPPORT_MATRIX.md).

Procedures and per-area verdicts: [day-2 runbook index](../../../runbooks/day-2/README.md),
[ENOSPC runbook](../../../runbooks/day-2/enospc.md). The harness lives in
[`docs/runbooks/day-2/scripts/`](../../../runbooks/day-2/scripts/).
