# D5 post-fix re-drill — ENOSPC on journal and provider-state writes

## 1. Verdict

**Verdict: PASS for the pre-stated normal-path contract on one development
host.** D5 was re-run after the #58 fix merged as PR #69. Part A (journal
ENOSPC, fake provider) re-confirmed: the failed append is refused with HTTP
500/`INTERNAL`, the volume is 404, the torn append is truncated back to the
byte-identical valid prefix on restart, and the retry succeeds. Part B
(provider-state ENOSPC, LVM on a loop VG) now fails closed on the normal
uncommitted-save path: the old durable state stays byte-identical, the
just-created LV is compensated, the volume is 404 in the same process and
after restart, a replayed `operation_id` returns the recorded 500 without
executing, and a new `operation_id` succeeds exactly once. The
cleanup-failure, post-rename carve-out and unreadable-durable-reread branches
were **not** fault-injected by this drill; they are test-covered (section 7).

## 2. Pre-stated pass condition

The post-#58 contract asserted by
[`d5.sh`](../../../runbooks/day-2/scripts/d5.sh) at `940722e`, stated before the
run:

**Part A — journal filesystem (fake provider):**

1. the create returns HTTP 500 with `code=INTERNAL` (both asserted) and a
   message containing `No space left on device`;
2. the failed volume id is absent (GET 404);
3. the tmpfs is verified as `tmpfs`, filled to zero KiB available, and the
   pre-failure journal size/SHA256 are recorded;
4. the failed append grew the journal past the pre-failure size, then after
   freeing space and restarting the torn append is truncated back to the
   pre-failure size recorded in step 3 and the valid-prefix SHA256 is
   unchanged;
5. the same create retried with the same `operation_id` returns HTTP 200 and
   the volume id is `vol-d5-full`;
6. the final journal parses with `valid_prefix == file_size`; the parser exits
   nonzero if any trailing bytes remain.

**Part B — provider-state filesystem (LVM on a loop VG, journal on ordinary
storage):**

1. the state tmpfs is verified as `tmpfs` and filled to zero KiB available;
   the create returns HTTP 500/`INTERNAL` with a message containing
   `No space left on device` and the exact state `.tmp` path;
2. the message does **not** contain `manual lvremove` — the provider is
   expected to compensate, not to hand cleanup to the operator — and does
   **not** contain `were kept`, so this is the uncommitted-save branch, not
   the post-rename carve-out;
3. the old durable state file is byte-identical (same size and SHA256), holds
   no failed-volume record, and no `*.tmp` residue remains;
4. the just-created LV for the failed volume is removed; only the baseline LV
   remains;
5. in the same process, GET of the failed volume is 404 and
   `GET /v2/volumes` lists exactly the baseline;
6. after a stop-while-full, free-space and restart: the failed volume is still
   404, the baseline is still 200/`Ready` (both asserted), the durable state
   is still the same size/SHA256 with no failed-volume record, no `.tmp`
   residue, and `lvs` still shows only the baseline;
7. replaying the same failed `operation_id` after space is free returns HTTP
   500 with a **byte-identical body** and does not execute; the drill asserts
   the daemon log contains `replaying recorded outcome without executing`;
8. a new `operation_id` with the same payload returns HTTP 200,
   `volume_id=vol-d5-lvm-full`, `state=Ready`, and
   `provisioned_bytes=67108864`;
9. the final durable state holds exactly two volume records, `lvs` holds
   exactly two LVs total and exactly one LV for the new volume (baseline + the
   new volume's LV), no `*.tmp` remains, and the baseline is retained.

## 3. Identity and provenance

- **Product code:** merged `main`
  `1863a7ac250d20246c328a2fd4c4acca330e3693` — PR #69, "Fix #58: fail closed
  when LVM create state save fails".
- **Harness / drill commit:** `940722ea3322df0a47834c307e2e0081c57e0b5e`.
  It is three commits on top of `1863a7a`:
  `c9458423b018e8293da543b04a79b65ee79df726` ("D5: assert the post-#58
  fail-closed state-save contract"),
  `863c4c83f90cee27055e297226d416a7daef9148` ("D5 harness: make the lab
  teardown ownership guard work") and
  `940722ea3322df0a47834c307e2e0081c57e0b5e` ("D5 harness: count cap failures
  and harden the ENOSPC setup; adds the Part A `INTERNAL`/torn-append
  assertions, a baseline-`Ready` assertion after restart, short-write
  accounting, and KILL escalation for a TERM-ignoring daemon").
- **Product diff:** empty.
  `git diff --name-only 1863a7a 940722e -- crates/ Cargo.toml Cargo.lock
  release-scope.toml` produces no output. `git diff --stat 1863a7a 940722e`
  lists only `docs/runbooks/day-2/scripts/d3.sh`,
  `docs/runbooks/day-2/scripts/d5.sh` and
  `docs/runbooks/day-2/scripts/lib.sh`. The product code in this run is
  therefore identical to the merged revision; the version stamp differs only
  because `HEAD` was the harness commit at build time.
- **Raw drill header:**
  `# source_sha: 940722ea3322df0a47834c307e2e0081c57e0b5e`,
  `# worktree_dirty: no`, `# runner: uid=0`,
  start `2026-11-03T20:21:59Z`, end `2026-11-03T20:22:02Z`.
  `worktree_dirty` is derived from `git status --porcelain` (tracked changes
  and untracked, non-ignored files); a recorded `no` covers all three scripts
  and the product tree here.
- **Binary:** `target/debug/volvisord`, debug profile, built with
  `cargo build --locked` in the clean worktree. SHA256
  `b40a97c307194270d7cfd69bb1463ad2a2cc2ef0b43f48950862a37434f602f5`;
  `--version` = `volvisord 940722e`. Only `volvisord` was used;
  `volvisor-witnessd` was not used. See [BINARIES.txt](BINARIES.txt).
- **Script hashes at `940722e`**, matching the worktree files:
  `d5.sh`
  `3ba417118f6017f8788a007ab5da60ab03a5f7dd05a53eadf41dd1310d026e76`,
  `lib.sh`
  `32fea568975b6ccebd5d9c2563eeaea982378f204cc82203f9e65c8b4f9b0b67`,
  `d3.sh`
  `46ef59267ba9f5c8a978a46bb911809917539c436051020fed18d67e2c620607`.
  See [SCRIPTS.txt](SCRIPTS.txt).
- **Tags:** none. `git tag --list` is empty;
  `git describe --tags --always` = `940722e`.
- **Bundle commit:** the commit that contains this directory is a docs-only
  child of `940722e`; its own SHA is not embedded (self-reference). If the PR
  is squash-merged, that merge commit is the durable one. The harness bytes and
  product tree are the same, and [SCRIPTS.txt](SCRIPTS.txt) gives the SHA256
  values to verify. If `940722e` is not reachable from the merged history, use
  the commit that contains this bundle and check those hashes.
- **Run window:** 2026-11-03T20:21:59Z .. 2026-11-03T20:22:02Z (3 s). The
  separate [raw/orchestrator-run.txt](raw/orchestrator-run.txt) records the
  command, start `2026-11-03T20:21:59Z`, end `2026-11-03T20:22:02Z` and exit
  status 0; the post-run orchestrator capture is stamped
  `2026-11-03T20:22:04Z`.
- **Host:** `dev`, kernel `6.8.0-142-generic` (both from the raw header).
  `x86_64`, 16 CPUs and 31 GiB were collected at bundle-authoring time on the
  same host, not recorded by the drill. See [ENVIRONMENT.txt](ENVIRONMENT.txt).

## 4. What ran

**Lab topology.** The drill used two separate labs in one script.

- **Part A:** the journal directory lived on its own 256 KiB tmpfs
  (`$RUN/lab-d5/journalfs`); the provider was the fake provider, so no LVM
  was involved. A baseline volume was created and the journal was padded with
  ten real creates until the last page had little slack, then the tmpfs was
  filled to 100%.
- **Part B:** a loop-backed volume group `vg-d5lvm` on `/dev/loop0` with the
  LVM provider state on a second 256 KiB tmpfs (`$RUN/lab-d5/statefs`) and the
  journal on ordinary storage (`$RUN/lab-d5/lvm-journal`). The journal and
  state were deliberately separated so only the state save hits ENOSPC after
  `lvcreate`. A baseline volume was created, the state tmpfs was filled to
  100%, and the failed create was then attempted.

**Part A sequence:** baseline create → journal padding → `df` → fill tmpfs
with `filler.bin` → record pre-failure journal size/SHA256 → attempt
`vol-d5-full` → check 404 → stop daemon → remove filler → restart → check
truncation → retry the same create → stop → parse the journal.

**Part B sequence:** create baseline → snapshot durable state → fill state
tmpfs → attempt `vol-d5-lvm-full` → inspect response, state, `lvs`, same-process
GET and list → snapshot the first daemon log → stop while the tmpfs is still
full → free filler → restart → check 404, baseline 200/`Ready`, state, `lvs` →
replay the same failed `operation_id` → new `operation_id` with the same
payload → final state, `lvs`, residue check → teardown.

**Post-run capture.** The orchestrator recorded the command and start
timestamp in [raw/orchestrator-run.txt](raw/orchestrator-run.txt) before the
drill, then appended the end timestamp and exit status after it finished
(exit 0). After the drill it captured `vgs`, `pvs`, `losetup -a` and a mount
grep for the run directory. That capture is an orchestrator check after the
drill, **not** a drill assertion; it is included as
[raw/post-run-cleanup.txt](raw/post-run-cleanup.txt).

## 5. Observed

### Part A — journal ENOSPC

- Journal before the failed append: size **12060**, SHA256
  `41bcc95aa1a34a1f39aada03e9f805117f7bf3756a7cc718a55ee5f2c247e172`
  (228 bytes of last-page slack).
- tmpfs filled to **100%**, with `df` showing **0** KiB available (asserted);
  the filler wrote **249856** bytes and stopped with `No space left on device`.
- Create refused with HTTP **500** and body:

  ```json
  {"code":"INTERNAL","message":"failed to write journal record: No space left on device (os error 28)"}
  ```

- The failed volume was **404** (`GET /v2/volumes/vol-d5-full`).
- Journal size immediately after the failed append: **12288** (the torn
  append).
- Daemon stopped, filler removed, daemon restarted. The journal was truncated
  back to **12060** with the **same** SHA256
  `41bcc95a...c247e172`; the drill asserted both.
- Retry with the same `operation_id` returned HTTP **200** and
  `vol-d5-full` (state `Ready`).
- Final journal parse: `valid_frames=24 valid_prefix=13166 file_size=13166`.

### Part B — provider-state ENOSPC

- Baseline durable state: size **1023**, SHA256
  `687daef08cb3932b9063fbf90d01a6771490c557daff8a30d5a2f6b77e15a4b2`.
- Baseline LV: `vol-vol-d5-lvm-baseline-3498a2bf`. Failed-volume LV name
  derived by the drill: `vol-vol-d5-lvm-full-58c0134e`.
- State tmpfs filled to **100%**, with `df` showing **0** KiB available
  (asserted); the filler wrote 258048 bytes.
- Create returned HTTP **500**/`INTERNAL` with the run-time absolute path in
  the message:

  ```json
  {"code":"INTERNAL","message":"failed to write /tmp/opencode/volvisor-d5-redrill-20261103T202159Z/lab-d5/statefs/lvm-state.json.tmp: No space left on device (os error 28)"}
  ```

  The body contains no `manual lvremove` and no `were kept`.
- Old state remained byte-identical: same size **1023** and same SHA256
  `687daef0...e15a4b2`; no failed-volume record; no `*.tmp` residue.
- `lvs` after the failure showed only the baseline LV — the failed-volume LV
  is absent, consistent with the compensating `lvremove` in #69.
- In the **same process**: `GET /v2/volumes/vol-d5-lvm-full` returned **404**;
  `GET /v2/volumes` listed exactly `vol-d5-lvm-baseline`.
- Daemon stopped **while the state tmpfs was still full**, filler removed,
  daemon restarted.
- After restart: failed volume still **404**; baseline still **200** with
  `{"volume_id":"vol-d5-lvm-baseline","state":"Ready","provisioned_bytes":67108864}`;
  state still size 1023 / SHA256 `687daef0...e15a4b2`; still no failed-volume
  record; no `.tmp`; only the baseline LV.
- Same failed `operation_id` (`op-d5-lvm-full`) replayed after space was free:
  HTTP **500**, byte-identical body to the first failure (the two records have
  the same SHA256,
  `17745ac97f71f43932a2ba61d3b2eaecbed6262947a166b01a6aa4befb3da184`).
  The daemon log records (excerpt):

  ```text
  "message":"replaying recorded outcome without executing","kind":"create_volume","operation_id":"op-d5-lvm-full"
  ```

  Durable state still held one volume and `lvs` still one LV.
- New `operation_id` (`op-d5-lvm-full-newop`) with the same payload: HTTP
  **200**, `volume_id=vol-d5-lvm-full`, `state=Ready`,
  `provisioned_bytes=67108864`.
- Final durable state: **1767** bytes, SHA256
  `43e090d7635dfe855a48f31d4b225c98d9f80c5ada364e0aa4730cb520557e1d`;
  exactly **2** volume records; `lvs` exactly **2** LVs (baseline +
  `vol-vol-d5-lvm-full-58c0134e`); no `*.tmp`; baseline retained.
- Clean teardown: `lab teardown tag=d5-lvm loop=/dev/loop0 vg=vg-d5lvm`.
  The post-run capture shows `vgs`, `pvs` and `losetup -a` empty and no mount
  of the run directory (the mount grep exited 1). This is the orchestrator's
  after-the-fact check, not a drill assertion.

The log contains **51** `ASSERT PASS` lines; the assertion output closes with
`ASSERT SUMMARY: 0 failures`, followed only by teardown output and the
lab-teardown line. The drill log itself has no `D5 COMPLETE` marker; the
separate orchestrator capture of the process stdout
([raw/orchestrator-console.txt](raw/orchestrator-console.txt)) ends with
`D5 COMPLETE`, printed only on the script's success branch, and
[raw/orchestrator-run.txt](raw/orchestrator-run.txt) records exit status 0.

Because `start_volvisord` opens each daemon log with `>`, the restart
truncates it. The drill snapshots the first incarnation before each restart:
[raw/d5-daemon.pre-restart.log](raw/d5-daemon.pre-restart.log) and
[raw/d5-lvm-daemon.pre-restart.log](raw/d5-lvm-daemon.pre-restart.log). The
current-incarnation LVM log
([raw/d5-lvm-daemon.daemon.log](raw/d5-lvm-daemon.daemon.log)) holds the
post-restart replay line quoted above.

## 6. Interpretation and change from the pre-fix drill

The pre-fix bundle at
`e551d26` ([`docs/evidence/day2/2026-11-03-devhost/`](../2026-11-03-devhost/REPORT.md))
observed, for the same state-save ENOSPC:

- the old durable state byte-identical (same 1023 bytes and SHA256);
- **`GET` of the failed volume returned 200 from memory in the same process**;
- the just-created LV `vol-vol-d5-lvm-full-58c0134e` **survived** as an
  orphan;
- after restart the GET returned 404 while the LV remained.

This run observes, for the normal uncommitted-save path:

- the same byte-identical old durable state;
- **`GET` returns 404 in the same process**, and the list contains only the
  baseline;
- **the LV is removed** by the compensating `lvremove`;
- after restart the volume is still 404, the baseline still serves, and the
  LV is still absent;
- the failed `operation_id` is terminal: replay is a byte-identical 500 that
  explicitly does not execute.

That is the behavior #58 asked for: HTTP 500 means the create did not happen,
in the API, in memory and in the backend.

The baseline state SHA256 is the **same** as the pre-fix run. That is
expected: the baseline request/seed is the same, so the durable baseline
serialization is identical. It is not evidence that the two runs are the same
run, and the post-failure state differs (pre-fix: orphan LV, in-memory 200;
post-fix: no orphan, 404).

## 7. Verdict and scope

- **Part A: DRILLED and PASS.** Journal ENOSPC re-confirmed post-fix.
- **Part B: DRILLED and PASS for the normal uncommitted-save path only**, on
  one host. The drill fault-injected a failure to write the state `.tmp`
  file before the rename.

**Not fault-injected by D5 in this run:** the cleanup-failure branch (the
compensating `lvremove` itself fails), the post-rename carve-out (the durable
state already contains the record), and the unreadable-durable-reread branch
(the provider cannot read the durable file to decide). Those branches are
**test-covered**, not re-drilled. The deterministic tests added in #69 are:

- `create_save_failure_rolls_back_lv_and_memory_and_converges_across_restart`
- `create_save_failure_with_a_failing_lvremove_surfaces_the_orphan`
- `create_save_failure_after_rename_is_honored_not_compensated`
- `create_save_failure_with_an_unreadable_durable_file_still_compensates`

plus the real-LVM test
`lvm_create_state_save_enospc_rolls_back_lv_and_converges_across_restart`.

"Test-covered" is weaker than "drilled": the tests exercise the branches
deterministically, but they are not an end-to-end drill with an external
observer on a real host. The carve-out test uses a seeded durable record, not
a real rename-then-directory-fsync failure; the missing real fault seam is
[#70](https://github.com/o3kio/volvisor/issues/70) item 3, not part of this
re-drill. The runbook's four-outcome procedure therefore stays in force; do
not collapse it to the normal case.

## 8. RPO / RTO observations

- The failed create was never acknowledged as durable and no durable **volume**
  record for it exists at any point in this run.
- The just-created LV was compensated (removed) before the 500 was returned;
  there is no orphan to reconcile on the normal path.
- The old durable state and the baseline volume were untouched, and the
  baseline was served after restart.
- The whole drill window was **3 s** (2026-11-03T20:21:59Z ..
  2026-11-03T20:22:02Z), including both parts, two daemon restarts and the
  replays; the orchestrator record shows exit 0 at 2026-11-03T20:22:02Z.

These are **single-host lab observations**, not an SLA and not a general
zero-RPO claim. They say nothing about a real disk that fails, a concurrent
writer, a crash during the save, or a multi-host deployment.

## 9. Limits and non-claims

- One development host; every daemon and client on `127.0.0.1`.
- Linux loop-device LVM, not real NVMe/SAN/Ceph/DRBD.
- ENOSPC injected with 256 KiB tmpfs mounts; no real disk, inode, thin-pool
  data or LVM metadata exhaustion.
- Debug build; `cargo build --locked`; no release artifact or packaged
  install.
- No concurrent writer; no `kill -9` during the failure window; the daemon was
  stopped gracefully between phases.
- Only the normal uncommitted-save path was fault-injected. The cleanup-failure,
  carve-out and unreadable-durable-reread branches are test-covered, not
  drilled.
- No multi-host, no Ceph, no DRBD, no real storage backend.
- **Not production evidence.** No SLA, no zero-RPO claim, no release or tag.
- Gate verdicts in
  [`docs/releases/pre-production-readiness.md`](../../../releases/pre-production-readiness.md)
  are unchanged.
- [#70](https://github.com/o3kio/volvisor/issues/70) remains open for the
  separate out-of-scope work: persist divergence in grow/delete/move, the
  kill-9 `lvcreate`→persist orphan window, and a real post-rename
  directory-fsync fault seam. Those are not the three non-injected #58
  branches, which are test-covered.
- **Harness residual risks.** The drill asserts the contract, but the harness
  itself has known limits: `pick_port` binds then releases an ephemeral port,
  so a concurrent process could take it before the daemon binds (a startup
  failure, not a false pass); assertions match exact daemon message
  substrings, so a rewording fails the drill rather than silently passing;
  a stopped daemon gets a 10 s TERM budget before KILL; `df -k` parsing
  assumes a single-line device name; and the Part A padding loop assumes
  4 KiB pages and frames smaller than 500 bytes, so a violated assumption
  surfaces as a loud failure at the 500 expectation or at the
  truncate-back/SHA check. Separately, the new append-growth assertion stops
  the truncate-back check from passing vacuously when the journal layer
  refuses before writing any bytes. None of these weakens the #58 assertions
  in this run.
- Not pre-production ready.

## 10. Reproduce

Check out the merged commit that contains this bundle (it carries the same
harness bytes as the pre-merge drill commit; `940722e` is equivalent if it is
still reachable), verify the three script hashes against
[SCRIPTS.txt](SCRIPTS.txt), then build and run:

```sh
git checkout <commit-containing-this-bundle>   # or 940722e if reachable
sha256sum docs/runbooks/day-2/scripts/d5.sh \
          docs/runbooks/day-2/scripts/lib.sh \
          docs/runbooks/day-2/scripts/d3.sh
# must equal the three SHA256 values in SCRIPTS.txt
cargo build --locked

sudo REPO=/path/to/volvisor \
  BIN=/path/to/volvisor/target/debug \
  RUN=/tmp/opencode/volvisor-d5-redrill-$(date -u +%Y%m%dT%H%M%SZ) \
  bash docs/runbooks/day-2/scripts/d5.sh

(cd docs/evidence/day2/2026-11-03-d5-redrill && sha256sum -c MANIFEST.sha256)
```

The harness records `source_sha` and `worktree_dirty` at the start of the
drill. It creates disposable loop-backed volume groups and tmpfs mounts; do
not run it on a host with production volume groups. The run directory contains
bearer-token curl configs and is not part of this bundle.

## 11. Cross-references

- Pre-fix bundle (pinned to `e551d26`, not edited):
  [`docs/evidence/day2/2026-11-03-devhost/`](../2026-11-03-devhost/README.md),
  [D5 report section](../2026-11-03-devhost/REPORT.md#d5--enospc-on-journal-and-provider-state-writes),
  [pre-fix raw log](../2026-11-03-devhost/raw/05-enospc.log).
- Procedure and four-outcome pass condition:
  [`docs/runbooks/day-2/enospc.md`](../../../runbooks/day-2/enospc.md).
- Finding: [#58](https://github.com/o3kio/volvisor/issues/58).
- Fix: [PR #69](https://github.com/o3kio/volvisor/pull/69), merged at
  `1863a7ac250d20246c328a2fd4c4acca330e3693`.
- Separate out-of-scope work (not the three test-covered #58 branches):
  [#70](https://github.com/o3kio/volvisor/issues/70) — grow/delete/move
  persist divergence, the kill-9 `lvcreate`→persist orphan window, and a real
  post-rename directory-fsync fault seam.
- Runbook index: [`docs/runbooks/day-2/README.md`](../../../runbooks/day-2/README.md).
- Readiness report (basis remains `main` at `575acfc`):
  [`docs/releases/pre-production-readiness.md`](../../../releases/pre-production-readiness.md).
