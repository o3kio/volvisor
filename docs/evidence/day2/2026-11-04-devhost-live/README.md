# Day-2 drill evidence — 2026-11-04 single-host dev lab

This bundle records four retained runs on one Linux development host:

1. **D1 backup/restore** with current v3 assertions: whole-directory backup,
   destructive drift, restore, byte-identical state/journal checks,
   attachment/revision/evidence survival, the bounded tombstone filter, and
   the behavioral retired-id refusal.
2. **D2 v3 same-binary schema policy**: pre-versioning → v3 upgrade, legacy
   tombstone import, synthetic future-version refusal, restore-and-serve.
3. **D2 live cross-binary upgrade and rollback**: a real old binary
   (`8c2fc1a`, LVM schema 2) → new binary (`aad1720`, schema 3) → typed old
   refusals → restore the whole v2 directory and serve the original volume
   with the old binary.
4. **The D2/D3/D8 readiness preflight**, which is a prerequisite report and
   not drill evidence.

It is the evidence behind the verdicts in
[`docs/runbooks/day-2/`](../../../runbooks/day-2/README.md). Read
[REPORT.md](REPORT.md) first; it states what each drill observed and what the
bundle does not prove.

## What this is

- **D1:** volumes `vol-d1-a`/`vol-d1-b`/`vol-d1-c`, a tombstone from
  detaching `att-d1-b`, a stopped tar backup, destructive drift, a restore,
  asserted byte-identical state/journal digests, and a
  `409 IDEMPOTENCY_CONFLICT` for reattaching the retired id. The v3 filter
  count is asserted, not the removed v1/v2 vector. Each of the three daemon
  sessions has its own retained log.
- **D2:** the repository's populated-directory compatibility test plus a live
  same-binary policy drill. The future-schema file is deliberately
  hand-edited test data; the raw log records its construction. The binary
  digest is captured after the test build, immediately before the daemon runs.
- **D2 live:** the old binary created and reopened the baseline state, the
  new binary served and upgraded it, the old binary refused both newer
  artifacts typed (both refused files hash-verified unchanged), and the
  stopped backup restored byte-for-byte. The post-upgrade journal is retained
  in `records/`.
- **Preflight:** local tool/commit/loop checks, the old build
  (`--check-old-build`), and honest NOT RUN verdicts for D3 and D8.

## What this is not

- Not multi-host, not a rolling upgrade, not a packaged/systemd upgrade, not
  a data-plane test, not real NVMe or a real Ceph/DRBD backend.
- Not D3 storage-side fencing and not D8 total host loss; both are **NOT RUN**.
- Not production support, an SLA, zero-RPO, a signed release or a 24h soak.
- Not a clean-caller-worktree run: Phase B was uncommitted, so D1 and D2 used
  a `aad1720-dirty` caller build. Only the D2 live NEW binary was built from a
  clean disposable worktree, and that is recorded explicitly.

## Run binding and layout

`SCRIPTS.txt` lists the harness hashes. Each raw drill log header records the
`harness_sha256` and `lib_sha256` at run time, and they match. `BINARIES.txt`
records the run-time binary versions and sha256 values. `ENVIRONMENT.txt`
records the run directories, windows, host, absent capabilities and the
preflight result. `TOOLCHAIN.txt` records the assembly-time toolchain output
(not a drill log).

| Path | Contents |
|---|---|
| `REPORT.md` | Verdicts, steps, observations, disclosures, limits |
| `ENVIRONMENT.txt` | Host, run windows, source identity, preflight |
| `TOOLCHAIN.txt` | Assembly-time command/version output |
| `BINARIES.txt` | OLD/NEW/caller binary versions and sha256 |
| `SCRIPTS.txt` | Harness hashes + run binding |
| `raw/01-backup-restore-v3.log` | D1 drill log |
| `raw/02-schema-upgrade-v3.log` | D2 v3 drill log |
| `raw/02b-live-upgrade-rollback.log` | D2 live drill log |
| `raw/*.stdout.log` | Harness stdout including the final `exit=0` |
| `raw/d1-daemon-*.daemon.log` | D1's three daemon sessions |
| `raw/d2-future.daemon.log` | D2 v3 future-schema refusal |
| `raw/d2live-*.daemon.log` | D2 live sessions and both refusals |
| `raw/preflight-all.log` | All-areas preflight (`--check-old-build`) |
| `raw/preflight-d2-strict.log` | D2-only strict preflight |
| `records/` | State/journal copies, HTTP records, binary identities, digests, backup tar |
| `MANIFEST.sha256` | sha256 of every file in this bundle except itself |

The four retained run directories were:

```text
/tmp/opencode/volvisor-d2live-final3-20261104T151044Z     (D2 live)
/tmp/opencode/volvisor-d2-final3-20261104T145816Z         (D2 v3)
/tmp/opencode/volvisor-d1-final3-20261104T145858Z         (D1)
/tmp/opencode/volvisor-preflight-final3-20261104T145906Z  (preflight)
```

Each `raw/*.stdout.log` ends with the harness's final `exit=0`. The D2 live
stdout transcript also contains `exit=1` lines from the two expected-refusal
caps (`CAP_EXPECT_RC=1`), whose assertions pass. The raw drill logs end with
`ASSERT SUMMARY: 0 failures` and their completion marker.

Intermediates that the raw logs do not echo (the lab's v1 seed, a few record
copies, and the Step 4b fresh-journal construction) are executed by the
hash-bound harness; their effects are covered by the logged digests and
assertions. The evidence bundle is self-contained; the `/tmp` working
directories need not survive.

## Verification

From this directory:

```sh
sha256sum -c MANIFEST.sha256
```

Expected: every line ends `OK`.

## Evidence-standard notes

- **Operator:** uid 0 on `dev`; no personal identity is recorded.
- **Secrets:** no authorization headers, bearer values, tokens, private keys,
  or curl configs are included. Disposable token values exist only in the
  harness scripts, not in this bundle.
- **Raw logs are unedited.** Corrections were made to the harness and then
  the full drills were re-run; the corrections are listed in
  [REPORT.md](REPORT.md#harness-corrections-disclosed).
- **No product code was changed for the drills.** Phase B changes are tests,
  scripts and documentation; the product source under test is `aad1720`.
