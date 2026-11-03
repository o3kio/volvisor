# Daemon crash loop and single-writer lock contention — day-2 runbook

**Status: DRILLED.**
Evidence: [D6 raw log](../../evidence/day2/2026-11-03-devhost/raw/06-crash-loop-and-lock.log),
[Report D6](../../evidence/day2/2026-11-03-devhost/REPORT.md#d6--crash-loop-and-single-writer-lock-contention).
Source `e551d26`, clean worktree, one host, loop-device LVM.

## Scope and non-goals

This page covers:

- a `volvisord` crash loop caused by `SIGKILL` (including an in-flight
  request), and the restart/replay checks;
- a second daemon or witness refusing to start on a state directory already
  locked by a live writer.

It does not cover a distributed lock across hosts, a stale NFS lock, a kernel
OOM kill (same recovery, but the cause is different), or a power cut. A
`SIGKILL` is a process crash, not a host loss.

## RPO / RTO

- **RPO:** the in-flight request may fail. D6 observed `curl` exit 52 / HTTP
  `000` for every killed create; none is claimed to have completed. Durable
  prior state survived all five kills. Retry the operation with its original
  `operation_id`; do not invent a new id.
- **RTO:** replay + health on restart. D6 did not separately time it; the
  harness waits up to 30 s.
- **Lock contention:** the second process must fail closed; the first keeps
  serving. No RPO impact.

## Preconditions

API examples below use an owner-only curl config; do not put a bearer token on
the command line or leave it in shell history.

```sh
umask 077
printf 'header = "Authorization: Bearer %s"\n' "$VOLVISOR_ADMIN_TOKEN" > ~/.volvisor-admin.curl
chmod 600 ~/.volvisor-admin.curl
```

- Root, `flock`, `lvs`, `jq`, `curl`, and a copy of the state directory before
  destructive investigation.
- The lock file is `<journal_dir>/journal.lock` (witness:
  `<witness_state_dir>/journal.lock`).
- You know how to stop the unit and read its logs:
  `systemctl status volvisord.service`, `journalctl -u volvisord.service`.

## A. Diagnose the crash loop

1. Record the unit state and recent exits:

```sh
systemctl status volvisord.service --no-pager
journalctl -u volvisord.service -n 120 --no-pager
```

Look for a repeated start and a typed error. A restart count that climbs with
the same error is a crash loop, not a transient.

2. Distinguish the causes:

- `open journal: ... failed to parse` / `unsupported durable-state schema
  version` → [journal-and-state-corruption.md](journal-and-state-corruption.md)
  or [upgrade-and-rollback.md](upgrade-and-rollback.md), not this page.
- `already locked by another writer` → section C.
- provider construction / LVM command failure → check `lvs`, `vgs`, device
  visibility, and the ENOSPC page before starting again.
- no error, process exits immediately → check the binary digest, config
  `--check-config`, permissions on the state directory, and the service unit.

3. Run `--check-config`; it catches configuration and path errors without
   touching durable state:

```sh
/usr/bin/volvisord --config /etc/volvisor/volvisor.toml --check-config
```

Expected: exit 0 and a summary ending `configuration check passed`.

## B. Recover from a SIGKILL crash loop

1. Stop the unit so no restart races your inspection:

```sh
systemctl stop volvisord.service
```

2. Confirm the lock is free (a dead process releases `flock` automatically):

```sh
flock -n /var/lib/volvisor/journal/journal.lock -c true && echo "lock free"
```

Expected: `lock free`. If it is busy, go to section C; do not delete the lock
file.

3. Copy the state aside and verify the journal has no trailing garbage:

```sh
STATE_DIR=/var/lib/volvisor/journal
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
cp -a "$STATE_DIR" "$STATE_DIR.before-crash-recovery.$STAMP"
```

Use the frame parser from
[journal-and-state-corruption.md](journal-and-state-corruption.md) (or the
bundle's
[records/d4-frames.py](../../evidence/day2/2026-11-03-devhost/records/d4-frames.py)):

```text
valid_frames=11 valid_prefix=5826 file_size=5826
```

The critical check is `valid_prefix == file_size`. If it is not, the journal
has a torn tail; follow the corruption runbook instead of starting.

4. Start the daemon and verify health:

```sh
systemctl start volvisord.service
curl -fsS http://127.0.0.1:8080/healthz
```

Expected: `{"status":"ok"}`.

5. Inspect the durable volumes and attachments, not just health:

```sh
curl -fsS --config ~/.volvisor-admin.curl \
  http://127.0.0.1:8080/v2/volumes | jq .
```

D6 expected and observed after every one of five kills:

```json
{
  "state": "active",
  "state_revision": 2,
  "evidence": "guest_confirmed"
}
```

for `att-d6-base` on `vol-d6-base`.

6. Retry the in-flight operation with the **same** `operation_id`. The journal
   idempotency registry should replay it if it committed, or create it once if
   it did not. Do not issue a new operation id and then wonder why there are
   two resources.

7. Reconcile the backend. A crash between `lvcreate` and the durable save can
   leave an LV whose state record is absent (the same class as the ENOSPC
   finding, [#58](https://github.com/o3kio/volvisor/issues/58)):

```sh
lvs --noheadings -o lv_name,vg_name,lv_size
jq '.volumes | keys' /var/lib/volvisor/journal/lvm-state.json
```

Do not delete an LV you cannot explain. If the volume is absent from state and
no consumer uses it, record the decision and remove it; otherwise escalate.

## C. Lock contention: a second writer on the same directory

1. The expected refusal is typed and immediate. D6 observed:

```text
Error: Config("open journal: INTERNAL: journal directory
/var/lib/volvisor/journal is already locked by another writer
(single-writer enforcement): Resource temporarily unavailable (os error 11)")
```

Exit status 1. The same rule applies to the witness:

```text
Error: Config("witness journal open failed: INTERNAL: journal directory
/var/lib/volvisor-witness/state is already locked by another writer
(single-writer enforcement): Resource temporarily unavailable (os error 11)")
```

2. Identify the holder before doing anything:

```sh
fuser -v /var/lib/volvisor/journal/journal.lock
# or: lsof /var/lib/volvisor/journal/journal.lock
```

3. If the holder is the intended `volvisord`, do nothing. The first daemon
   must keep serving; D6 verified HTTP 200 after the refused second start.

4. If the holder is a dead/unknown process, confirm it is gone:

```sh
ps -fp "$(fuser /var/lib/volvisor/journal/journal.lock 2>/dev/null)"
```

A dead process cannot hold an `flock`; if the lock is still busy, the process
is alive (or the filesystem is not local). Do not `rm` the lock file as a
"fix": the inode may be recreated and the real holder keeps its lock, giving a
false sense of safety.

5. Never run two daemons on one state directory "for HA". The lock is a
   single-writer guard, not a leader election. HA needs the multi-host design;
   see [host-loss-and-rejoin.md](host-loss-and-rejoin.md), which is NOT RUN.

## Pass condition

- The crash loop has a typed cause, not an unexplained exit.
- The lock is free before restart; no lock file is deleted while a writer is
  alive.
- The journal opens with `valid_prefix == file_size`.
- The daemon is healthy and the expected volumes/attachments are served with
  the same state/revision/evidence.
- The in-flight operation is retried idempotently; backend orphans are
  reconciled. **NOT DRILLED:** D6 did not re-issue the killed operations and
  does not observe backend-orphan reconciliation. Treat this item as
  procedure-only until a drill performs the same-`operation_id` retry.
- A second daemon/witness on the same directory exits 1 with
  `already locked by another writer`, and the first stays healthy.

## What was drilled

D6 created and attached `vol-d6-base`, reported it active, then killed the
daemon with `SIGKILL` five times while a create was in flight. Every client
saw exit 52 / status 000; every restart parsed the whole journal
(`valid_prefix == file_size`) and served the same active/revision-2/
`guest_confirmed` attachment. A second daemon and a second witness both
exited 1 with the typed lock refusal; the first services stayed healthy. Raw:
[raw/06-crash-loop-and-lock.log](../../evidence/day2/2026-11-03-devhost/raw/06-crash-loop-and-lock.log).

**Limits:** same-host `SIGKILL`, no power cut; local filesystem `flock`, not a
distributed lock; loop-device LVM; no concurrent multi-host writer; the
in-flight request is allowed to fail and is not an exactly-once claim. The
in-flight create was not retried with the same `operation_id` (that
pass-condition item is marked NOT DRILLED above).
