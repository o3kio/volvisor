# Day-2 drill report — 2026-11-04 single-host lab

**Overall verdict: D1 backup/restore PASS; D2 v3 same-binary policy PASS;
D2 live old→new upgrade and restore-based rollback PASS. D3 storage-side
fencing NOT RUN; D8 total host loss NOT RUN.** This is a single-host,
loop-LVM, debug-build lab result. It is not production support, an SLA,
zero-RPO, a signed release, hardware evidence, a rolling multi-host upgrade,
or a data-plane test.

| Area | Verdict | Basis |
|---|---|---|
| D1 backup/restore | **PASS** | whole stopped directory backed up, live directory drifted and moved aside, restore byte-identical (journal and state asserted), attachment state/revision/evidence and the v3 tombstone filter survived, retired-id reattach `409 IDEMPOTENCY_CONFLICT`, post-backup volume 404 |
| D2 v3 same-binary policy | **PASS** | pre-versioning file → v3 upgrade on first write, legacy tombstone vector imported into `retired_attachment_filter`, synthetic v4 refused typed and left byte-identical, restored file served as `Attached` |
| D2 live old→new upgrade | **PASS** | old `8c2fc1a` (LVM schema 2) → new `aad1720` (schema 3); original volume preserved; v3 written on the new mutation |
| D2 live rollback | **PASS** | whole v2 directory restored byte-identically; old binary served the original volume; post-backup volume 404 |
| D2 live typed refusals | **PASS** | old binary refused the v2 journal with its v1 ceiling and an isolated v3 LVM state with its v2 ceiling; the refused journal and state were hash-verified unchanged |
| D3 storage-side fencing | **NOT RUN** | no DRBD 9 module/`drbdadm`/peer/witness |
| D8 host loss/re-admission | **NOT RUN** | no multi-host/BMC/oracle/shared backend |

## Identity and provenance

- **Product source:** `aad17201f03bfdfcec4b74d863b805bc7fa47c76`
  (`aad1720`). Every raw drill header records this `source_sha`.
- **Old commit:** `8c2fc1a5f8bac1fe8d02844191fc9bd9cb61cd9c` (`8c2fc1a`).
- **Caller worktree:** dirty (Phase B is uncommitted). Every raw drill header
  records `worktree_dirty: yes`. The D2 live **NEW** binary was built from a
  clean disposable worktree at `aad1720`
  (`new_worktree_dirty_at_build=0`, version stamp has no `-dirty`, worktree
  HEAD asserted equal to `NEW_COMMIT`). The D2 live **OLD** worktree HEAD was
  asserted equal to `OLD_COMMIT`. D1 and D2 used the caller
  `target/debug/volvisord` (`volvisord aad1720-dirty`, sha256 `80105b9f…`);
  both drills captured that identity at their own run time (D1 in
  `records/d1-binaries.txt`, D2 in `records/d2-binaries.txt`) and every daemon
  start line records the same digest. This report does not call those runs
  clean-worktree provenance.
- **Harness bytes:** each drill log header records `harness_sha256` and
  `lib_sha256` at run time. They match the current scripts listed in
  [SCRIPTS.txt](SCRIPTS.txt); the retained logs are therefore bound to the
  exact harness bytes that produced them.
- **Run windows:** D2 live `15:10:44Z–15:11:27Z`; D2 v3
  `14:58:16Z–14:58:21Z`; D1 `14:58:58Z–14:59:01Z`; preflight all-areas
  `14:59:06Z` and D2-strict `14:59:25Z`. Exact run directories are in
  [ENVIRONMENT.txt](ENVIRONMENT.txt).
- **Host:** `dev`, kernel `6.8.0-142-generic`, uid 0, one physical host.
- **Binaries:** [BINARIES.txt](BINARIES.txt). OLD
  `ec6211874560c7b5cfeb4e38c72fb23bdc77a63d81692374da904fc1237672ab`
  (`volvisord 8c2fc1a`); NEW
  `6ebac3f9c5bf3bee53caa7764ccffdbd8ebeea146a164a9aa511c6beaac633ab`
  (`volvisord aad1720`); D1/D2 caller build `80105b9f…`
  (`volvisord aad1720-dirty`).

## D1 — backup, drift, restore

The v3 assertion path is exercised in
[raw/01-backup-restore-v3.log](raw/01-backup-restore-v3.log) and ended with
`ASSERT SUMMARY: 0 failures` and `D1 COMPLETE` in that raw log.

| Step | Result | Observation |
|---|---|---|
| Create/attach volumes, detach to leave a tombstone | PASS | pre-backup attachment `active`, `state_revision=2` |
| Stop daemon and prove no writer holds the journal lock | PASS | harness check: the journal lock is free (non-blocking `flock` succeeds) |
| Stopped whole-directory backup | PASS | state `a03326d9…c8aa48`, journal `2ba84f53…5b92a`, tar `80d35e9e…dd204c` |
| Record durable facts | PASS | `runtime.retired_attachment_filter.inserted = 1` for the detached volume |
| Destructive drift, then move live directory aside | PASS | drifted directory moved, never deleted; post-backup volume C created |
| Restore and verify byte-identity | PASS | restored state and journal digests equal the pre-backup digests (asserted, not only printed) |
| Post-restore assertions | PASS | attachment `active`, `state_revision=2`, evidence `guest_confirmed`, tombstone count `1` |
| Retired-id reattach | PASS (refused) | HTTP `409`, code `IDEMPOTENCY_CONFLICT` |
| Post-backup volume | PASS (absent) | HTTP `404` after restore |

The three daemon sessions use distinct tags
(`d1-daemon-create`, `d1-daemon-drift`, `d1-daemon-restore`), so all three
daemon logs are retained rather than the last one overwriting the first.

This is the record that backs the v3 `retired_attachment_filter` wording in
[backup-and-restore.md](../../../runbooks/day-2/backup-and-restore.md). The
conditional is behavioral: the restored filter must refuse the retired id,
not merely contain a bit.

## D2 v3 — same-binary schema policy

`d2.sh` ran against the current binary and ended with
`ASSERT SUMMARY: 0 failures` and `D2 COMPLETE`
([raw log](raw/02-schema-upgrade-v3.log)):

- the populated-directory `schema_upgrade` test passed 4/4;
- a file with no `schema_version` upgraded on first write to
  `schema_version=3`;
- attachment state/revision survived; the legacy `retired_attachment_ids`
  vector was imported into `retired_attachment_filter` (`inserted=2`) and was
  no longer written;
- a **hand-edited synthetic** copy declaring `schema_version=4` was refused
  with `supports at most 3` and left byte-identical (`ab281239…b7ea39`). The
  construction command (`jq '.schema_version = 4'`) is recorded in the raw
  log; no Volvisor version wrote that file;
- restoring the supported file let the same binary serve the volume as
  `Attached` (not `Failed`). The lab rewrites the golden fixture's `vg_name`
  to the disposable lab VG so the fixture LV is actually present.

The daemon binary identity is captured **after** the drill's
`cargo test -p volvisord` step and immediately before the daemon starts,
because that test build can replace `target/debug/volvisord`.

This is the same-binary policy check, not a second binary. The cross-binary
evidence is the D2 live section below.

## D2 live — pre-stated pass condition

The drill passes only if all of the following are observed:

1. OLD starts on the lab-seeded v1 state, creates the original volume, and
   leaves v2 LVM state.
2. OLD reopens that v2 file, still serves the original volume, and does not
   upgrade the schema merely on load.
3. A stopped whole-directory backup exists and no writer holds the journal
   lock at that point.
4. NEW opens the v2 state and serves the original volume **without**
   upgrading the schema or rewriting the state file merely on load.
5. A NEW mutation stamps schema 3 while preserving the original volume.
6. OLD refuses the post-upgrade directory typed and does not rewrite the v3
   state or the v2 journal; an isolated v3 LVM state is refused typed as well.
7. The v2 backup restores byte-for-byte; OLD opens it, serves the original
   volume, and the post-backup volume is absent.

## D2 live — steps and observations

| Step | Result | Observation | Record |
|---|---|---|---|
| Build OLD at `8c2fc1a` | PASS | `cargo build --locked -p volvisord` in a disposable worktree; HEAD asserted | [raw log](raw/02b-live-upgrade-rollback.log) |
| Build NEW at `aad1720` in a clean worktree | PASS | caller dirty; build worktree dirty count 0; no `-dirty` stamp; HEAD asserted | [binaries](records/d2live-binaries.txt) |
| OLD starts, creates `vol-d2live-original` | PASS | HTTP 200; `provisioned_bytes=67108864`, generation 1 | [old inspect](records/d2live-original-old.json) |
| OLD's first write stamps v2 | PASS | `schema_version=2`; original volume in state | [v2 state](records/d2live-state-v2.json) |
| OLD reopens the v2 file before upgrade | PASS | HTTP 200; `provisioned_bytes=67108864`; schema still 2 | [old-v2 inspect](records/d2live-original-old-v2.json) |
| Stop and prove no writer holds the lock | PASS | harness check: the journal lock is free before the backup | [raw log](raw/02b-live-upgrade-rollback.log) |
| Stopped whole-directory backup | PASS | state `702165cd…2aa68a`, journal `acd0cd62…25af9c`, tar `55df9ffd…6346` | [digests](records/d2live-sha256.txt) |
| NEW opens v2 and serves original | PASS | HTTP 200; state hash unchanged across the load; state still v2 | [new inspect](records/d2live-original-new.json) |
| NEW mutation upgrades to v3 | PASS | HTTP 200; `schema_version=3`; original size/generation unchanged; new volume recorded | [v3 state](records/d2live-state-v3.json) |
| Post-upgrade journal retained | PASS | v2-record journal saved as `records/d2live-journal-post-upgrade.log`, sha256 `03fb5e1c…35af10` | [digests](records/d2live-sha256.txt) |
| OLD vs post-upgrade directory | PASS (refused) | exit 1, typed `journal record ... declares schema_version 2, but this build supports at most 1`; v3 state **and** v2 journal hashes unchanged | [refusal log](raw/d2live-old-refusal.daemon.log) |
| OLD vs isolated v3 LVM state | PASS (refused) | exit 1, typed `lvm state ... declares schema_version 3, but this build supports at most 2`; copy hash unchanged | [v3 refusal log](raw/d2live-old-v3-refusal.daemon.log) |
| Restore v2 backup | PASS | state and journal byte-identical to backup source | [digests](records/d2live-sha256.txt) |
| OLD serves original after restore | PASS | HTTP 200, `vol-d2live-original`, generation 1 | [restored inspect](records/d2live-original-restored.json) |
| Post-backup volume absent | PASS | `vol-d2live-upgrade` HTTP 404 `NOT_FOUND` | [404 record](records/d2live-upgrade-after-restore.json) |

The drill ended with `ASSERT SUMMARY: 0 failures` and `D2 LIVE COMPLETE` in
the retained raw log ([raw log](raw/02b-live-upgrade-rollback.log)).
Step 1's old-daemon log is retained separately as
`raw/d2live-old-step1.daemon.log`; Step 5 reuses the `d2live-old` tag and its
log is `raw/d2live-old.daemon.log`. The retained stdout transcript contains
the expected-refusal caps' `exit=1` lines as well as the harness's final
`exit=0`; the refusal caps are `CAP_EXPECT_RC=1` and their assertions pass.

## D2 live — why both refusal checks are recorded

The post-upgrade directory contains **two** newer artifacts: the v3 LVM state
and the v2 journal records written by NEW. The old binary opens the journal
first and refused it typed (`journal record ... declares schema_version 2,
but this build supports at most 1`); the hashes of both the v3 LVM state and
the v2 journal were re-checked and unchanged. To prove the **v3
provider-state** refusal separately, the drill also ran OLD against a fresh
journal directory plus a copy of the v3 LVM state; that exited 1 with
`schema_version 3, but this build supports at most 2`. This is a real copy of
the post-upgrade state, not a hand-edited file; no `schema_version` was
changed to make a step pass. The original v2 journal that Step 4a refused is
retained, so a reviewer can verify its record version from the bundle.

## Rollback / RPO / RTO

- **RPO = the pre-upgrade backup point.** The post-backup volume
  `vol-d2live-upgrade` returned 404 after restore. Its LV remained in `lvs`,
  so an operator must reconcile backend resources created after the backup.
  The restore did not delete it.
- **RTO was not separately measured.** The D1 restore reached a healthy
  serving daemon in `0.323 s` on this lab host; this is not a production RTO
  measurement and excludes backend reconciliation and data-path validation.
- The rollback restored the **whole** state directory. Restoring only
  `lvm-state.json` would leave the newer v2 journal, which the old binary also
  refuses.

## Preflight result

The readiness preflight is a report, not drill evidence. The all-areas run
with `--check-old-build` printed ([log](raw/preflight-all.log)):

```text
D2 live: READY
D3 storage-side: NOT RUN
D8: NOT RUN
VERDICT: NOT RUN (D2 live: READY; D3 storage-side: NOT RUN; D8: NOT RUN)
```

D2 local tools, uid, repository, old commit, old-tree build inputs, old build,
and loop device all passed. D3 was `NOT RUN` for the absent DRBD module,
`drbdadm`, peer, witness, resource name and failure-domain inputs. D8 was
`NOT RUN` for the absent host list, witness, oracle, BMC and storage backend.
A BMC present but unprobed is reported `NOT RUN`, not `PASS`. The
D2-only strict run (`--only d2 --strict --check-old-build`) reported
`VERDICT: READY (D2 live: READY)`, exit 0
([log](raw/preflight-d2-strict.log)).

## Harness corrections (disclosed)

The Phase B review loop found and fixed these harness issues before the
retained runs:

1. `d1.sh` asserted the schema-v1/v2 `retired_attachment_ids` vector that v3
   no longer writes. It now asserts the bounded
   `retired_attachment_filter.inserted` count and keeps the behavioral
   `409 IDEMPOTENCY_CONFLICT` check.
2. `d2.sh` built a golden fixture whose `vg_name` did not match the lab VG,
   so the restored volume was reported `Failed`. The fixture `vg_name` is now
   rewritten to the lab VG; the drill asserts the restored volume is
   `Attached`.
3. `d2.sh` toggled `set +e` even though the harness never enables `-e`; the
   dead toggles were removed and the expected refusal status is captured
   explicitly.
4. Daemon tags were reused across D1 and D2 live steps, truncating earlier
   logs. D1 now has one tag per session; D2 live's Step 1 is
   `d2live-old-step1`; the post-upgrade journal is retained.
5. Completion markers (`D1 COMPLETE`, `D2 COMPLETE`, `D2 LIVE COMPLETE`) are
   now written into the raw logs, not just stdout.
6. Each raw log header now records the harness and `lib.sh` sha256 at run
   time; binary identity is captured into records for D1, D2 and D2 live, and
   every daemon start line records the digest.
7. `stop_pidfile` now counts a still-alive daemon as an assertion failure, and
   the drills hard-stop before a backup/restore if it fails. Each stopped-state
   consistency point is probed by the harness's `assert_no_writer`, which
   takes the journal lock non-blockingly with `flock -n`.
8. D1 and D2 live now assert restored state/journal byte-identity; D2 live
   also asserts the v2 state hash is unchanged across NEW's load and the v2
   journal hash is unchanged across OLD's refusal.
9. D2 captures its binary digest after `cargo test -p volvisord` rebuilt the
   target, so the record names the binary the daemon actually ran.
10. Binary identity asserts build-worktree HEAD equality and pins the
    `--version` stamp to the build tree's `git describe` output, instead of
    relying on the stamp containing a short SHA (a tagged commit's stamp can
    omit the SHA). In the clean-caller path this also prevents a stale
    `target/debug/volvisord` from masquerading as the NEW build.
11. Lab teardown runs before the assertion summary and its failures are
    counted; they are no longer redirected to `/dev/null`.

Only the final passing runs are retained. No product behavior failed, and no
raw log was edited.

## Limits and residual blockers

- **Single host, one kernel, one clock.** No network partition, independent
  failure domain, multi-host witness hop or rolling upgrade.
- **Loop-device LVM.** The provider skips loop devices; the lab seeds an
  operator-provisioned native-pool claim. This is a scaffold.
- **Debug builds.** Release-build behavior at the same SHAs is not claimed.
- **LVM store only** for the live swap. Ceph/DRBD/migration/peer stores are
  covered by the compatibility test, not a live provider drill.
- **No data path.** No tenant workload, guest flush/FUA or VM was attached.
- **D3 storage-side fencing NOT RUN.** Needs a DRBD 9 module, `drbdadm`, a
  real peer and a witness on independent failure domains.
- **D8 total host loss NOT RUN.** Needs >=3 hosts, BMC/fencing, an
  independent oracle and a real shared backend. See
  [host-loss-and-rejoin.md](../../../runbooks/day-2/host-loss-and-rejoin.md).
- **No production support, SLA, zero-RPO or signed-release claim.** The
  support posture stays in `SUPPORT_MATRIX.md`.
