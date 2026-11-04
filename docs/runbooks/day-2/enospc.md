# Disk full / ENOSPC during a journal or state write — day-2 runbook

**Status: DRILLED on `e551d26`; re-run post-fix at product `1863a7a` and
harness `940722e` for the normal uncommitted-save path.** The provider-state
gap found by the drill
([#58](https://github.com/o3kio/volvisor/issues/58)) is fixed and merged in
[PR #69](https://github.com/o3kio/volvisor/pull/69). D5 was re-drilled after
the fix on one host (both parts: the journal path and the provider-state normal
path). The cleanup-failure, post-rename carve-out, clear-save and
unreadable-durable-reread branches were not fault-injected by the re-drill;
they are covered by the deterministic tests added in #69/#70. See the
[post-fix re-drill bundle](../../evidence/day2/2026-11-03-d5-redrill/REPORT.md).
The create path now also persists a durable create intent **before**
`lvcreate` ([#70](https://github.com/o3kio/volvisor/issues/70)), and
grow/delete/move have their own durable anchors; the full per-mutation
contracts and operator tails are in
[lvm-state-divergence.md](lvm-state-divergence.md). This page keeps the ENOSPC
procedure and the create-specific tails.

- **Journal ENOSPC:** fail-closed. HTTP 500, no volume, torn append
  truncated on restart, retry succeeds after space is freed.
- **Provider-state ENOSPC:** HTTP 500; for an uncommitted save the old
  durable state is intact. After the #58 fix, an uncommitted save compensates
  the just-created LV and removes the in-memory entry (GET is 404, no phantom
  volume); if the compensating `lvremove` fails, the LV remains as an
  operator-visible orphan and the error names the manual remediation. In the
  rare case where the durable state already contains this volume's record
  (typically the save's rename landed before a later step failed), the
  durable record, memory and LV keep the volume and GET returns it. If the
  durable re-read itself failed, the provider cannot say whether a record
  survived; it attempts the checked `lvremove` and leaves the file for the
  operator (if the removal also fails, the 500 names `manual lvremove`). A
  surviving record shows the volume `Failed` with a missing LV after
  restart when the removal succeeded; if the LV remains alongside the
  record, it loads as a served volume. Do not auto-delete in that case.

Historical evidence (pre-fix D5):
[D5 raw log](../../evidence/day2/2026-11-03-devhost/raw/05-enospc.log),
[Report D5](../../evidence/day2/2026-11-03-devhost/REPORT.md#d5--enospc-on-journal-and-provider-state-writes).
Post-fix coverage: `lvm_create_state_save_enospc_rolls_back_lv_and_converges_across_restart`
in `crates/volvisor-lvm/tests/integration_lvm.rs`.

## Scope and non-goals

This page covers ENOSPC on:

- the filesystem holding `<journal_dir>` (journal append);
- the filesystem holding the LVM provider state file (`lvm-state.json`).

It does not cover inode exhaustion, thin-pool data exhaustion, LVM metadata
area exhaustion, Ceph `nearfull`/`full`, or DRBD disk-full behavior. Those
need their own drills. The ENOSPC here is filesystem-level, injected with a
256 KiB tmpfs in the lab.

## RPO / RTO

- **Journal ENOSPC:** a failed attempt is never acknowledged as durable; no
  acknowledged state is lost; the torn append is removed on restart. The
  failed create is 404 when nothing executed (the intent append failed, or
  the provider call failed without landing anything — the section C
  carve-out excepted); if `GET` is 200, the volume is real and served.
  **RTO:** free space, restart, verify, retry with a new `operation_id`;
  seconds in the lab.
- **State ENOSPC:** the failed create is not acknowledged as durable.
  **RPO:** the failed create (except the carve-out, where the record did
  become durable and the volume is served). Post-fix, the just-created LV is
  compensated and the in-memory entry is rolled back; only a failing
  compensating `lvremove` leaves an orphan that needs manual cleanup.
  **RTO:** free space + restart + retry; add one manual `lvremove` for the
  cleanup-failure tail. The normal uncommitted-save path was re-drilled
  post-fix at product `1863a7a` / harness `940722e`: the failed create stayed
  non-durable, the LV was compensated, and same-process plus post-restart GET
  returned 404. The cleanup-failure, carve-out, clear-save and indeterminate
  branches were not fault-injected in that re-drill; they remain the
  six-outcome procedure in section C below and are covered by tests, not
  by an end-to-end drill.

## Preconditions

API examples below use an owner-only curl config; do not put a bearer token on
the command line or leave it in shell history.

```sh
umask 077
printf 'header = "Authorization: Bearer %s"\n' "$VOLVISOR_ADMIN_TOKEN" > ~/.volvisor-admin.curl
chmod 600 ~/.volvisor-admin.curl
```

- You can see `df -h` and `df -i` for the journal and state filesystems.
- You have authority to stop/start the daemon and to `lvremove` an orphan LV
  after confirming it is not in durable state.
- A recent backup exists for the state filesystem.
- Know which filesystem holds what. The packaged default puts the journal and
  LVM state under `/var/lib/volvisor/journal`; a deployment may separate them.
  The lab deliberately separated them so only one path hit ENOSPC.

## A. Detect

```sh
df -h /var/lib/volvisor/journal
df -i /var/lib/volvisor/journal
journalctl -u volvisord.service -n 100 --no-pager | grep -i 'no space\|ENOSPC\|failed to write'
```

Typical 500 bodies (observed):

```json
{"code":"INTERNAL","message":"failed to write journal record: No space left on device (os error 28)"}
```

```json
{"code":"INTERNAL","message":"failed to write /var/lib/volvisor/journal/lvm-state.json.tmp: No space left on device (os error 28)"}
```

The detail distinguishes the two paths: `failed to write journal record` vs
`failed to write .../lvm-state.json.tmp`. A delayed-allocation filesystem can
instead report `failed to fsync journal record` after the bytes were written;
treat it as "the frame may already be durable on disk", not as "nothing
happened".

> **Temp-name update (since `a8c23d6`).** New writes use
> `.volvisor-<16-hex>.<pid>.<seq>.tmp` in the state file's directory, so a
> current failure reads `failed to write .../.volvisor-...tmp`. The legacy
> `<state>.tmp` name can still appear during an upgrade (an older binary or a
> leftover). Match both when grepping. The observed messages above are
> historical drill output and are left unchanged.

## B. Journal filesystem ENOSPC

1. Do not retry in a tight loop. Free space first.

```sh
# find the consumer; the journal grows with mutations
du -sh /var/lib/volvisor/journal
df -h /var/lib/volvisor/journal
```

2. Check the failed volume's state (below), then stop the daemon. The stop is
   what lets replay truncate the torn append.

```sh
curl -sS -o /dev/null -w '%{http_code}\n' \
  --config ~/.volvisor-admin.curl \
  http://127.0.0.1:8080/v2/volumes/$VOLUME_ID
```

Expected for the failed create: `404`. If `GET` is `200`, the volume is real
and served (for example a section C carve-out): verify it, do not re-create
it, and do not reuse the failed `operation_id`.

```sh
systemctl stop volvisord.service
```

3. Free space. Do not delete the journal or state file. Remove a filler,
   rotate unrelated logs, or extend the filesystem.

4. Restart and verify the journal was truncated back to its valid prefix.

```sh
systemctl start volvisord.service
curl -fsS http://127.0.0.1:8080/healthz
stat -c '%s' /var/lib/volvisor/journal/journal.log
sha256sum /var/lib/volvisor/journal/journal.log
```

Expected: health `{"status":"ok"}`, and the size is the pre-failure valid
prefix. A `failed to fsync journal record` flavor may instead leave the
complete frame in place, so the size can exceed the pre-failure prefix. D5
observed 12060 before the failed append, 12288 immediately after (the torn
append), and 12060 after restart, with the prefix SHA256 unchanged
(`41bcc95a...`).

5. Retry the original operation **with the same `operation_id`** only when
   **both** hold: step 2 observed a `404`, and the failed create's 500 body
   was the intent-append error (`failed to write journal record`). Then
   nothing was executed and the retry must create one volume, not two.
   If the 500 body names the LVM provider instead, the intent **was**
   journaled and the provider call failed; a same-`operation_id` retry does
   not re-execute — it replays the recorded 500 when the failure outcome
   fit in the journal, and fails closed as `OPERATION_IN_DOUBT` when it did
   not. The same applies to `failed to fsync journal record`: the frame may
   already be on disk — if it survived, the retry fails closed as
   `OPERATION_IN_DOUBT`; if it was lost, the retry re-executes having created
   nothing the first time (at most one volume either way). If step 2 observed
   a `200`, the volume exists: verify the volume and LV, do not re-create,
   and expect the same id not to re-execute. In every other case, close the
   operation with a **new `operation_id`** and the same payload after
   inspecting journal/state.

```sh
# send the same create request that failed
curl -fsS --config ~/.volvisor-admin.curl \
  -H 'content-type: application/json' --data-binary @create.json \
  http://127.0.0.1:8080/v2/volumes | jq .
```

Expected: HTTP 200 for the retry; D5 observed `vol-d5-full` durable with 24
valid frames and no second copy. A retry that returns a conflict means the
intent was already durable (or the volume exists): inspect the journal/state
and retry only with a **new `operation_id`**.

6. Verify the failed volume id is now present exactly once and its LV exists.

```sh
curl -fsS --config ~/.volvisor-admin.curl \
  http://127.0.0.1:8080/v2/volumes/$VOLUME_ID | jq .
lvs --noheadings -o lv_name,vg_name,lv_size
```

## C. Provider-state filesystem ENOSPC

The save is atomic up to the rename: a failure before it preserves the old
`lvm-state.json` and removes its own temp file (since `a8c23d6`, the bounded
`.volvisor-*` name; the legacy `<state>.tmp` may still appear during an
upgrade). Since #70 a create has **two** save
boundaries — the durable create intent before `lvcreate`, then the volume
record after it. After the #58 fix, an uncommitted final save is compensated
with a checked `lvremove` and the in-memory entry is rolled back. Six
outcomes are possible, so still inspect before assuming anything:

- **Intent-save failure (before `lvcreate`):** 500, old state byte-identical,
  GET 404, and **no LV was created**. If the intent save's rename did land,
  the durable intent is left for startup, which observes no intent-owned LV
  and drops it. Retry with a new `operation_id`; nothing ran.
- **Uncommitted final save (normal):** 500, old state byte-identical, GET 404,
  no LV for the failed volume id (the LV was compensated with `lvremove`).
- **Clear-save failure (issue #70):** after a known command failure or a
  compensated final save, the clear save that removes the durable create
  intent also fails. The intent is kept in memory; startup classifies an
  absent LV by dropping the intent and adopts a present LV only at or above
  the requested size. See the create tail in
  [lvm-state-divergence.md](lvm-state-divergence.md).
- **Cleanup failure:** 500, GET 404, the LV remains, and the error detail
  names `manual lvremove <vg>/<lv>`. The in-memory entry is gone, so the LV
  is absent from provider state.
- **Carve-out (the durable state already contains this volume's record —
  typically the save's rename landed before a later step failed):** 500, GET
  200, the durable record/LV/memory all hold the volume. Do not compensate
  it; the volume is served, but its crash durability was not confirmed.
- **Indeterminate (the durable re-read failed):** 500, GET 404, and the
  provider could not read the durable file, so it cannot say whether a
  record survived. It attempts the same checked `lvremove`; if that also
  fails, the 500 names `manual lvremove`. Either way, treat the combined
  case as indeterminate: a surviving valid record appears as `Failed` with
  a missing LV after restart when the removal succeeded, or loads as a
  served volume if the LV remains; a corrupt file needs the corruption
  runbook. Do not remove the LV until the state file is readable and proves
  the record absent.

The two-save ordering postdates the D5 re-drill (`1863a7a`, PR #69), which
exercised the single-save compensated path. On current code a fill-first
ENOSPC surfaces at the **intent** save (no LV is ever created); the
compensated final-save path above is covered by deterministic tests on
current code, not by that drill.

**Do not assume "HTTP 500" means "nothing happened."**

1. Record what exists before touching anything:

```sh
sha256sum /var/lib/volvisor/journal/lvm-state.json
stat -c '%s %n' /var/lib/volvisor/journal/lvm-state.json
lvs --noheadings -o lv_name,vg_name,lv_size
curl -sS -o /tmp/after-500.json -w '%{http_code}\n' \
  --config ~/.volvisor-admin.curl \
  http://127.0.0.1:8080/v2/volumes/$VOLUME_ID
cat /tmp/after-500.json
```

Expected post-fix:

- **intent-save failure:** the state hash and size are unchanged, `GET`
  returns 404, and no LV named for `$VOLUME_ID` is present (`lvcreate` never
  ran);
- **normal (uncommitted final save):** the state hash and size are unchanged,
  `GET` returns 404, and no LV named for `$VOLUME_ID` is present;
- **carve-out:** `GET` returns 200, the LV is present, and the durable state
  contains the volume;
- **cleanup failure:** `GET` returns 404 but the LV is present, and the 500
  detail names `manual lvremove <vg>/<lv>`;
- **clear-save failure (issue #70):** a known command failure left a durable
  intent that the clear save could not remove, or the compensating clear save
  after a failed final save could not remove it. `GET` returns 404; the
  intent is kept in memory. If the LV is absent, restart drops the intent; if
  present, startup adopts it only at or above the requested size (see
  [lvm-state-divergence.md](lvm-state-divergence.md));
- **indeterminate:** `GET` returns 404 and the durable file could not be
  read; no LV exists, or the LV remains if the compensating `lvremove` also
  failed. Compare a backup before acting; a valid surviving record will load
  as a `Failed` volume with a missing LV, or as a served volume if the LV
  remains.

Historical D5 observation (pre-fix, `e551d26`):

- old state hash `687daef0...e15a4b2`, size 1023, unchanged;
- the new LV `vol-vol-d5-lvm-full-58c0134e` present in `lvs`;
- `GET` of the failed volume id returned **200** from memory;
- after restart, the same `GET` returned **404** while the LV remained.

2. Free space, then stop and restart the daemon:

```sh
systemctl stop volvisord.service
# free space here
systemctl start volvisord.service
curl -fsS http://127.0.0.1:8080/healthz
```

3. Re-check the volume and LV against the six outcomes above:

   - **intent-save failure:** `GET` is still 404, no LV exists, and any
     durable intent whose LV is absent is dropped at startup; nothing ran, so
     a new `operation_id` retry is safe;
   - **normal:** `GET` is still 404 and no LV for the failed volume id exists;
   - **cleanup failure:** `GET` is 404 and the LV is still present — go to
     step 4;
   - **carve-out:** `GET` is 200, the LV is present, and the durable state
     contains the volume. Do not remove it; confirm it appears exactly once
     in `GET /v2/volumes`, then resume at step 5's carve-out bullet;
   - **clear-save failure:** `GET` is 404. A retained create intent is
     classified at startup: an absent LV drops it, a present LV is adopted
     only at or above the requested size. Follow the create tail in
     [lvm-state-divergence.md](lvm-state-divergence.md);
   - **indeterminate:** if the durable file still cannot be read, the daemon
     will not have started (the step-2 `healthz` check failed) — escalate
     with the state and LVM output. If it now reads: `GET` is 404 when no
     record survived (a remaining LV is the step-4 orphan tail), and 200
     when a record survived (the volume is `Failed` if its LV was removed,
     or served if the LV remains). Do not retry or delete anything until
     the file proves the record absent.

   Anything else is a state/LVM disagreement: stop and investigate before
   serving.

4. Reconcile an orphan LV **only after** confirming it is absent from the
   durable state and no consumer uses it. This is the cleanup-failure tail;
   never do it for the carve-out (whose state record and LV must stay):

```sh
# Read-only checks first.
jq '.volumes | keys' /var/lib/volvisor/journal/lvm-state.json
lvs --noheadings -o lv_name,vg_name,lv_size

# Then, for the confirmed orphan (name from lvs):
lvremove --yes vg-example/vol-vol-example-<hash>
```

If you are unsure, keep the LV and escalate. Deleting an LV whose state
record is missing from a stale backup is data loss.

5. Retry only after space is restored:

   - **intent-save failure, normal or cleanup failure:** use a **new
     `operation_id`** with the same payload. Reusing the failed
     `operation_id` does not re-execute:
     it replays the recorded failure when the journal had space to record
     the outcome (the separated layout), and fails closed as
     `OPERATION_IN_DOUBT` when the journal shares the full filesystem and
     could not record it. The new-`operation_id` retry is correct either
     way.
   - **carve-out:** `GET` already returns the volume; a new `operation_id`
     with the same payload returns the same volume idempotently.
   - **indeterminate:** do not retry; investigate the unreadable durable
     file and compare a backup first.

   Verify exactly one durable volume record for the retried volume id and one
   LV for it; pre-existing volumes and their LVs are unchanged.

Outside the indeterminate case the old durable state is safe. The states
that need operator attention are the carve-out (served but not
crash-durable-confirmed), the cleanup-failure orphan (visible in `lvs`,
absent from provider state), and the indeterminate case (unreadable durable
file, with or without the LV).

## D. Prevention

- Monitor free space and inodes on the journal and state filesystems; alert
  well before full, not at 100%.
- Keep the journal and provider state on a filesystem with headroom and a
  reserved-blocks margin for root; do not co-locate with bulk VM data.
- Alert on any `INTERNAL` 500 whose detail contains `No space left on device`.
- Keep a recent stopped backup; a corrupt state file is a different failure,
  but the same backup is the recovery path.
- The lab's journal ENOSPC needed **padding with real creates** until the last
  page had <500 bytes free; a small append can otherwise be absorbed by page
  slack and never hit ENOSPC. Record that when reproducing.

## Pass condition

- Journal ENOSPC: 500, no acknowledged volume lost, torn append truncated to
  the valid prefix on restart, no phantom state. A same-`operation_id` retry
  creates exactly one volume only for an intent-append 500 with GET 404.
  When the 500 named the provider (intent durable, call failed) or `GET` was
  200, the volume is verified rather than re-created and the same id does
  not re-execute (verbatim replay or `OPERATION_IN_DOUBT`); the operation is
  closed with a new `operation_id`.
- State ENOSPC, intent-save failure: 500, old state file byte-identical,
  GET 404, **no LV created**, and a retry with a new `operation_id` creates
  exactly one volume.
- State ENOSPC, normal: 500, old state file byte-identical, GET 404, no
  orphan LV, restart 404, and a retry with a new `operation_id` creates
  exactly one volume.
- State ENOSPC, cleanup failure: 500 names `manual lvremove`, GET 404, the
  LV is present and absent from provider state; the operator removes it and
  a retry with a new `operation_id` succeeds.
- State ENOSPC, carve-out: 500, GET 200, durable state/LV/memory agree; a
  new `operation_id` with the same payload returns the same volume.
- State ENOSPC, indeterminate: 500, GET 404, and the durable file could not
  be read; the LV may remain if the compensating `lvremove` also failed.
  This is a stop-and-investigate outcome, not a pass; it must be recorded and
  escalated.
- No hand-edited state file, no truncated complete journal frame.

## What was drilled

D5 part A: 12060-byte journal with 228 bytes of slack, tmpfs filled, create
500, volume 404, restart truncated 12288→12060 with the same prefix hash,
retry 200. Part B (historical, pre-fix): 1023-byte state, tmpfs filled,
create 500, old state unchanged, in-memory GET 200, LV present, restart GET
404, LV still present, new create 200. Raw:
[pre-fix raw log](../../evidence/day2/2026-11-03-devhost/raw/05-enospc.log).

**Post-fix re-drill (normal uncommitted-save path), product `1863a7a` /
harness `940722e`:**

> The Part B messages quoted below name the historical fixed
> `lvm-state.json.tmp`. Since `a8c23d6` a current build names the bounded
> `.volvisor-<16-hex>.<pid>.<seq>.tmp` instead; the legacy name may still
> appear during an upgrade. The drilled evidence is left unchanged.

- Part A re-confirmed: journal 12060 bytes with 228 bytes of slack, SHA256
  `41bcc95aa1a34a1f39aada03e9f805117f7bf3756a7cc718a55ee5f2c247e172`;
  tmpfs filled (filler 249856 bytes); create 500/`INTERNAL` with
  `failed to write journal record: No space left on device (os error 28)`;
  volume 404; journal 12288 immediately after; restart truncated it to 12060
  with the same SHA256; same-`operation_id` retry 200/`vol-d5-full`;
  `valid_frames=24 valid_prefix=13166 file_size=13166`.
- Part B: baseline state 1023 bytes, SHA256
  `687daef08cb3932b9063fbf90d01a6771490c557daff8a30d5a2f6b77e15a4b2`;
  create 500/`INTERNAL` naming
  `.../lab-d5/statefs/lvm-state.json.tmp: No space left on device (os error
  28)` and no `manual lvremove`; old state byte-identical; no failed-volume
  record; no `.tmp`; the failed-volume LV
  `vol-vol-d5-lvm-full-58c0134e` removed, leaving only
  `vol-vol-d5-lvm-baseline-3498a2bf`; same-process GET 404 and list exactly
  the baseline; stop while full, free, restart: still 404, baseline still
  200/`Ready`, state still 1023 bytes / same SHA256, still no record, no
  `.tmp`, only the baseline LV; same `op-d5-lvm-full` replay 500 byte-identical
  (asserted with `cmp`) with daemon
  log `replaying recorded outcome without executing`; new
  `op-d5-lvm-full-newop` 200/`vol-d5-lvm-full`/`Ready`/`provisioned_bytes=67108864`; final state 1767
  bytes, SHA256
  `43e090d7635dfe855a48f31d4b225c98d9f80c5ada364e0aa4730cb520557e1d`,
  exactly 2 volume records and 2 LVs, no `.tmp`. Part A also asserted the
  refused create was `INTERNAL` and that the failed append grew the journal
  before ENOSPC, so the truncate-back check cannot pass vacuously. The run
  produced 51 `ASSERT PASS` lines and `ASSERT SUMMARY: 0 failures`; the
  separate orchestrator record shows exit status 0. Raw:
  [post-fix raw log](../../evidence/day2/2026-11-03-d5-redrill/raw/05-enospc.log),
  [post-fix report](../../evidence/day2/2026-11-03-d5-redrill/REPORT.md).

The post-fix state path is also exercised by
`lvm_create_state_save_enospc_rolls_back_lv_and_converges_across_restart`
in the real-LVM suite (256 KiB tmpfs, real ENOSPC, real `lvremove`, restart
and retry), plus deterministic tests for the cleanup-failure, carve-out and
unreadable-reread branches. Those are code-level regression tests, not a
full re-run of every branch.

**Limits:** filesystem ENOSPC only (not inode/thin-pool/metadata); fake
provider for the journal; loop LVM for state; single host; no concurrent
writer; the pre-fix bundle's D5 part B observations describe the old code and
must not be used as post-fix expected behavior. The post-fix re-drill covers
the normal uncommitted-save path only; its cleanup-failure, carve-out and
indeterminate branches remain test-covered, not drilled.
