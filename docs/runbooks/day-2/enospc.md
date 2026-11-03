# Disk full / ENOSPC during a journal or state write — day-2 runbook

**Status: DRILLED, with a filed finding.**

- **Journal ENOSPC:** fail-closed. HTTP 500, no volume, torn append
  truncated on restart, retry succeeds after space is freed.
- **Provider-state ENOSPC:** HTTP 500 and the old durable state is intact,
  **but** the create's LV is leaked and the running process serves the volume
  from memory until restart. Filed as
  [#58](https://github.com/o3kio/volvisor/issues/58). This is a real
  behavior gap, not a drill artifact.

Evidence: [D5 raw log](../../evidence/day2/2026-11-03-devhost/raw/05-enospc.log),
[Report D5](../../evidence/day2/2026-11-03-devhost/REPORT.md#d5--enospc-on-journal-and-provider-state-writes).

## Scope and non-goals

This page covers ENOSPC on:

- the filesystem holding `<journal_dir>` (journal append);
- the filesystem holding the LVM provider state file (`lvm-state.json`).

It does not cover inode exhaustion, thin-pool data exhaustion, LVM metadata
area exhaustion, Ceph `nearfull`/`full`, or DRBD disk-full behavior. Those
need their own drills. The ENOSPC here is filesystem-level, injected with a
256 KiB tmpfs in the lab.

## RPO / RTO

- **Journal ENOSPC:** the failed create is not visible (404); no acknowledged
  state is lost; the torn append is removed on restart. **RTO:** free space,
  restart, retry; seconds in the lab.
- **State ENOSPC:** the failed create is not durable, but its LV exists and
  the running process may serve a phantom volume. **RPO:** the failed create
  (the API returned 500, so it was never acknowledged as durable).
  **RTO:** free space + restart + orphan-LV reconciliation. The capacity leak
  persists until the operator removes the LV.

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
`failed to write .../lvm-state.json.tmp`.

## B. Journal filesystem ENOSPC

1. Do not retry in a tight loop. Free space first.

```sh
# find the consumer; the journal grows with mutations
du -sh /var/lib/volvisor/journal
df -h /var/lib/volvisor/journal
```

2. Confirm the failed volume is absent, then stop the daemon. The stop is what
   lets replay truncate the torn append.

```sh
curl -sS -o /dev/null -w '%{http_code}\n' \
  --config ~/.volvisor-admin.curl \
  http://127.0.0.1:8080/v2/volumes/$VOLUME_ID
```

Expected for the failed create: `404`.

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
prefix. D5 observed 12060 before the failed append, 12288 immediately after
(the torn append), and 12060 after restart, with the prefix SHA256 unchanged
(`41bcc95a...`).

5. Retry the original operation **with the same `operation_id`**. The retry
   must create one volume, not two.

```sh
# send the same create request that failed
curl -fsS --config ~/.volvisor-admin.curl \
  -H 'content-type: application/json' --data-binary @create.json \
  http://127.0.0.1:8080/v2/volumes | jq .
```

Expected: HTTP 200 for the retry; D5 observed `vol-d5-full` durable with 24
valid frames and no second copy. If the retry returns a conflict, inspect the
journal/state before trying another id.

6. Verify the failed volume id is now present exactly once and its LV exists.

```sh
curl -fsS --config ~/.volvisor-admin.curl \
  http://127.0.0.1:8080/v2/volumes/$VOLUME_ID | jq .
lvs --noheadings -o lv_name,vg_name,lv_size
```

## C. Provider-state filesystem ENOSPC (known gap)

The failed save is atomic: the old `lvm-state.json` is preserved and the
`.tmp` is removed. But `lvcreate` already ran, the new volume was inserted
into the in-memory state, and the error path does not roll either back.

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

D5 observed, immediately after the 500:

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

3. Re-check the volume and LV. Expect 404 for the failed create (it was never
   durable) and the LV still present. If the volume unexpectedly exists, stop
   and investigate before serving.

4. Reconcile the orphan LV **only after** confirming it is absent from the
   durable state and no consumer uses it:

```sh
# Read-only checks first.
jq '.volumes | keys' /var/lib/volvisor/journal/lvm-state.json
lvs --noheadings -o lv_name,vg_name,lv_size

# Then, for the confirmed orphan (name from lvs):
lvremove --yes vg-example/vol-vol-example-<hash>
```

If you are unsure, keep the LV and escalate. Deleting an LV whose state
record is missing from a stale backup is data loss.

5. Retry the create with the same operation id after space is restored, and
   verify one durable volume. D5 observed a subsequent create succeed (HTTP
   200) with the daemon not wedged.

**Until [#58](https://github.com/o3kio/volvisor/issues/58) is fixed, treat
every state-save 500 as "LV may exist, API may lie until restart."** The old
durable state is safe; the capacity and the temporary identity are not.

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

- Journal ENOSPC: 500, failed volume 404, torn append truncated to the valid
  prefix on restart, retry creates exactly one volume, no phantom state.
- State ENOSPC: 500, old state file byte-identical, **and** the operator
  records the in-memory/LV divergence, restarts (volume 404), reconciles the
  orphan LV, and verifies a later create.
- No hand-edited state file, no truncated complete journal frame.

## What was drilled

D5 part A: 12060-byte journal with 228 bytes of slack, tmpfs filled, create
500, volume 404, restart truncated 12288→12060 with the same prefix hash,
retry 200. Part B: 1023-byte state, tmpfs filled, create 500, old state
unchanged, in-memory GET 200, LV present, restart GET 404, LV still present,
new create 200. Raw:
[raw/05-enospc.log](../../evidence/day2/2026-11-03-devhost/raw/05-enospc.log).

**Limits:** filesystem ENOSPC only (not inode/thin-pool/metadata); fake
provider for the journal; loop LVM for state; single host; no concurrent
writer; the state-save gap is a code finding, not a passing behavior.
