# Backup and restore — day-2 runbook

**Status: DRILLED.**
Evidence: [D1 raw log](../../evidence/day2/2026-11-04-devhost-live/raw/01-backup-restore-v3.log),
[Report D1](../../evidence/day2/2026-11-04-devhost-live/REPORT.md#d1--backup-drift-restore).
Drilled at product source `aad1720`, dirty caller worktree, one host, stopped
daemon, loop-device LVM, with pre-ADR-0012 v3 tombstone-filter assertions. That
bundle is a historical record; the current harness asserts the attachment
generation fence instead, so re-run D1 for current-binary evidence. This page
is the procedure; the linked log is the historical result.

## Scope and non-goals

This procedure backs up and restores the **control-plane durable state**:

```text
<journal_dir>/journal.log              intent journal (binary frames)
<journal_dir>/journal.lock             single-writer lock (not durable data)
<journal_dir>/lvm-state.json           LVM provider state (default path)
<journal_dir>/ceph-state.json          if the ceph provider is used
<journal_dir>/drbd-state.json          if the drbd provider is used
<journal_dir>/migrations/              migration records
<journal_dir>/peer-preparations/       peer target preparations
<journal_dir>/grow-notifications.json  grow notifications (if present)
<journal_dir>/ownership.json           lending/ownership state (if present)
```

It does **not** back up:

- VM disk data inside the LVs;
- LVM metadata (use `vgcfgbackup` and keep a copy off-host);
- Ceph/RBD image contents or Ceph cluster state;
- DRBD resource data;
- the witness state directory (`<witness_state_dir>`), which is a separate
  authority journal and must be stopped and copied separately;
- TLS private keys, bearer tokens or configuration secrets (protect those in
  your secret store; never put them in this bundle).

A control-plane restore can describe a volume whose data or backend LV no
longer exists. The consistency step below is mandatory.

## RPO / RTO

- **RPO:** every control-plane mutation after the stopped backup is lost. D1
  demonstrated this: a volume created after the backup returned 404 after
  restore.
- **RTO:** 0.323 s from the start of the restore procedure to a healthy API
  on the lab host in the 2026-11-04 run, plus the time to reconcile backend
  LVs and validate the data path. That figure is not a production SLA.

## Preconditions

API examples below use an owner-only curl config; do not put a bearer token on
the command line or leave it in shell history.

```sh
umask 077
printf 'header = "Authorization: Bearer %s"\n' "$VOLVISOR_ADMIN_TOKEN" > ~/.volvisor-admin.curl
chmod 600 ~/.volvisor-admin.curl
```

- Root on the host, `lvm2`, `tar` with xattr/ACL support, `flock`, `jq`,
  `sha256sum`.
- A backup destination outside the state filesystem. A backup on the same
  disk as the journal is not a backup.
- Known `<journal_dir>` (packaged default `/var/lib/volvisor/journal`) and
  `<witness_state_dir>` (packaged default
  `/var/lib/volvisor-witness/state`).
- Maintenance authority: the daemon and, for a coherent multi-store backup,
  the witness must be stopped.

## A. Consistent backup

Set the paths once. The commands are copy-pasteable; adjust the defaults.

```sh
STATE_DIR=/var/lib/volvisor/journal
WITNESS_STATE_DIR=/var/lib/volvisor-witness/state
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
BACKUP=/var/backups/volvisor/$STAMP
install -d -m 0700 "$BACKUP"
```

1. Stop the writer (and the witness if you are also backing up its state).

```sh
systemctl stop volvisord.service
# only if backing up the witness authority state in the same window:
systemctl stop volvisor-witnessd.service
```

2. Prove no writer holds the lock. `flock -n` must exit 0. Exit 1 means a
   process still owns the directory: **stop**, find it, and do not tar.

```sh
flock -n "$STATE_DIR/journal.lock" -c true
```

Expected: no output, exit status 0. Check it explicitly:

```sh
flock -n "$STATE_DIR/journal.lock" -c true && echo "lock free"
```

Expected: `lock free`.

3. Record a relative checksum manifest and the LVM inventory.

```sh
( cd "$STATE_DIR" && find . -type f -print0 | sort -z | xargs -0 sha256sum ) \
  > "$BACKUP/state.sha256"
lvs --noheadings -o lv_name,vg_name,lv_size > "$BACKUP/lvs.txt"
vgs --noheadings -o vg_name,vg_size,vg_free > "$BACKUP/vgs.txt"
for vg in $(vgs --noheadings -o vg_name); do
  vgcfgbackup -f "$BACKUP/vg-$vg.cfg" "$vg"
done
```

4. Create the tar with the journal directory's name preserved, and record its
   digest. `--xattrs --acls` matters if your files carry them; the daemon's
   own files are owner-only, but the whole directory is the unit of restore.

```sh
tar --xattrs --acls -C "$(dirname "$STATE_DIR")" -cpf "$BACKUP/state.tar" "$(basename "$STATE_DIR")"
sha256sum "$BACKUP/state.tar" | tee "$BACKUP/state.tar.sha256"
```

Expected shape (D1 observed):

```text
80d35e9e31fa299ab856c706ea171cc7b7277042bac4047fbc89cd8430dd204c  /var/backups/volvisor/.../state.tar
```

5. If you also need the witness authority, copy its stopped state directory
   the same way.

```sh
tar --xattrs --acls -C "$(dirname "$WITNESS_STATE_DIR")" \
  -cpf "$BACKUP/witness-state.tar" "$(basename "$WITNESS_STATE_DIR")"
sha256sum "$BACKUP/witness-state.tar" | tee "$BACKUP/witness-state.tar.sha256"
```

6. Restart and verify the daemon is healthy.

```sh
systemctl start volvisord.service
# systemctl start volvisor-witnessd.service   # if stopped above
curl -fsS http://127.0.0.1:8080/healthz
```

Expected: `{"status":"ok"}` (the default listener may differ; use your
configured loopback address). A backup is not complete until the daemon is
healthy again.

## B. Restore

**Never restore over a running daemon, and never delete the current directory
until the restored one is verified.** Rename it aside.

1. Stop the writer(s).

```sh
systemctl stop volvisord.service
systemctl stop volvisor-witnessd.service   # if restoring witness state too
```

2. Verify the backup before touching the live directory.

```sh
cd "$BACKUP"
sha256sum -c state.tar.sha256
sha256sum -c witness-state.tar.sha256   # only if you backed up the witness
```

Expected: `state.tar: OK` (and `witness-state.tar: OK` when applicable).

3. Move the current state aside. This is the rollback copy.

```sh
mv "$STATE_DIR" "$STATE_DIR.drifted.$(date -u +%Y%m%dT%H%M%SZ)"
```

4. Extract and verify the restored files in place.

```sh
tar --xattrs --acls -C "$(dirname "$STATE_DIR")" -xpf "$BACKUP/state.tar"
( cd "$STATE_DIR" && sha256sum -c "$BACKUP/state.sha256" )
```

The manifest paths are relative (`./journal.log`, …), so run the check from
`$STATE_DIR`. Expected: every line ends `OK`.

5. Start and verify.

```sh
systemctl start volvisord.service
curl -fsS http://127.0.0.1:8080/healthz
```

Then inspect every volume and attachment against the backup's expected state:

```sh
curl -fsS --config ~/.volvisor-admin.curl \
  http://127.0.0.1:8080/v2/volumes | jq .
```

Check at least: volume ids, `state`, `attachments[].state`,
`attachments[].state_revision`, `attachments[].evidence`,
`attachments[].attachment_generation`, and the volume generation. The
`retired_attachment_filter` is legacy decode-only (ADR-0012) and may be absent
from files written by current binaries. D1 expected and observed:

```json
{
  "state": "active",
  "state_revision": 2,
  "evidence": "guest_confirmed"
}
```

and `vol-d1-b` retains its pre-detach volume generation. The attachment-id
tombstone filter is legacy (ADR-0012): new writes do not populate it, and the
provider does not consult it. The surviving fence is the **strictly increasing
per-volume `attachment_generation`**: re-attaching `att-d1-b` succeeds as a new
instance with generation `3`, and a delayed report carrying the retired
generation `1` returns HTTP 409 `STALE_GENERATION`. That is the behavioral
check; do not assert the old `IDEMPOTENCY_CONFLICT` on id reuse.

## C. Backend reconciliation (mandatory)

The restored control-plane state may not include LVs created or mutated after
the backup. The daemon does not delete backend resources on restore, and it
must not guess.

```sh
lvs --noheadings -o lv_name,vg_name,lv_size
```

Compare `lvs.txt` from the backup with the live list. For each LV that exists
but has no volume in the restored state:

- confirm no VM still uses it (the API returns 404 for the volume id);
- if it is a post-backup orphan, remove it with `lvremove` **after** the
  restored daemon is healthy and you have recorded the decision;
- if it is a real volume whose state record is missing, do not delete it;
  restore a newer backup or recover the record through a reviewed procedure.

D1 explicitly observed the post-backup `vol-d1-c` LV still present after
restore. That is the expected control-plane-only behavior, not a successful
data restore.

## Pass condition

- `flock -n` exits 0 before the backup;
- the tar digest is recorded;
- restored file checksums match;
- the daemon becomes healthy;
- attachment state/revision/evidence and the volume generation match the backup;
- post-backup operations are absent (404);
- a re-attach records a strictly greater generation and a delayed report from
  the retired generation is `STALE_GENERATION`;
- every extra LV is reconciled and recorded.

## If it fails

- **Lock is busy:** find the owner (`fuser -v "$STATE_DIR/journal.lock"` or
  `lsof`), stop it, and retry. Do not delete a live lock file.
- **Checksum mismatch:** do not start from the bad copy. Keep the drifted
  directory and the failed backup, and restore an older verified backup.
- **Daemon refuses the restored state:** read the typed error. A future schema
  means the backup is newer than the binary (install the matching binary, do
  not patch); a parse error means the backup is incomplete/corrupt (restore
  another copy). Do not hand-edit JSON.
- **Attachment state is wrong:** stop before serving the volume. A wrong
  single-writer view is a data-integrity risk, not a cosmetic error.

## What was drilled

D1 created A/B/C, attached A and B, reported A active, detached B, stopped the
daemon, copied the state directory, drifted by creating C, restored, and
re-inspected. The 2026-11-04 evidence bundle predates ADR-0012 and therefore
records the retired-id `409 IDEMPOTENCY_CONFLICT` plus the v3 filter count:
[raw/01-backup-restore-v3.log](../../evidence/day2/2026-11-04-devhost-live/raw/01-backup-restore-v3.log).
The current harness instead asserts the generation fence (re-attach generation
`3`, delayed generation `1` → `STALE_GENERATION`). Re-run the drill to produce
evidence for the current binary; the historical bundle is not evidence for the
new fence.

**Limits:** single host; control-plane only; no witness quiesce in the drill;
no VM-disk or LVM-metadata restore; loop-device LVM. A real disaster-recovery
restore also needs the data and LVM metadata backup and a cross-host test.
