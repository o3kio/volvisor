# Torn/corrupt journal tail and corrupt state file — day-2 runbook

**Status: DRILLED.**
Evidence: [D4 raw log](../../evidence/day2/2026-11-03-devhost/raw/04-torn-journal-and-corrupt-state.log),
[frame parser](../../evidence/day2/2026-11-03-devhost/records/d4-frames.py) — the
parser used in the drill is in the bundle's `records/`. Source `e551d26`,
clean worktree, one host.

## Scope and non-goals

This page covers four injected shapes:

1. **garbage appended after the last journal frame** (a torn tail);
2. **the last frame cut in half** (power loss mid-append);
3. **a complete, CRC-valid frame that declares an unsupported
   `record_version`** (durable state from a newer build);
4. **a syntactically corrupt provider state file** (`lvm-state.json`).

It does not cover media bit-rot, filesystem-level corruption, a partial page
write *inside* an otherwise complete frame, or a corrupted LVM metadata
region. Those need a different fault injector and, for data, a storage-layer
recovery procedure.

The governing policy is
[durable-state schema compatibility](../../schema-compatibility.md). The short
version: **structurally torn tail → drop only the incomplete tail; complete
but undecodable/unknown/future record → refuse open and leave the file
untouched.** Never truncate a complete frame to make the daemon start.

## Journal frame layout

From `crates/volvisor-journal/src/frame.rs` (little-endian):

```text
+---------+----------+-------------+--------+------------------+
| magic   | sequence | payload_len | crc32  | payload bytes    |
| u32     | u64      | u32         | u32    | payload_len      |
+---------+----------+-------------+--------+------------------+
```

- `magic` = `0x564A4C31` (`VJL1`);
- `sequence` starts at 1 and must be contiguous;
- `crc32` covers the payload (IEEE 802.3, `crc32fast`);
- a bad magic, truncated header, bad length, CRC mismatch or broken sequence
  chain makes replay stop at that offset and truncate the rest.

## RPO / RTO

- **Torn tail:** only the incomplete last record can be dropped. D4's
  acknowledged prior records survived, and replay was idempotent. An
  in-flight mutation may still have reached the backend; check `lvs`.
- **Complete future/unknown record:** no loss, but the daemon refuses to
  start. RPO is the backup point if you must restore; the correct fix is a
  matching/newer binary.
- **Corrupt state file:** RPO = the last verified backup; the daemon will not
  guess. RTO = restore + backend reconciliation, not measured.
- **RTO for a torn tail:** restart to health; seconds in the lab.

## Preconditions

API examples below use an owner-only curl config; do not put a bearer token on
the command line or leave it in shell history.

```sh
umask 077
printf 'header = "Authorization: Bearer %s"\n' "$VOLVISOR_ADMIN_TOKEN" > ~/.volvisor-admin.curl
chmod 600 ~/.volvisor-admin.curl
```

- Root, `python3`, `jq`, `sha256sum`, and a copy of the state directory
  **before** changing anything.
- The daemon is stopped and the lock is free:

```sh
systemctl stop volvisord.service
flock -n /var/lib/volvisor/journal/journal.lock -c true && echo "lock free"
```

Expected: `lock free`.

## A. Triage: journal tail or state file?

Read the typed startup error. It tells you which store refused:

```sh
journalctl -u volvisord.service -n 80 --no-pager
# or, for a direct run:
# /usr/bin/volvisord --config /etc/volvisor/volvisor.toml
```

Typical shapes:

```text
open journal: INTERNAL: unsupported durable-state schema version: journal
record state .../journal.log declares schema_version 2, but this build
supports at most 1; refusing to open it partially ...
```

```text
lvm provider construction failed: INTERNAL: failed to parse provider state
.../lvm-state.json: EOF while parsing a value at line 1 column 45
```

An `unsupported durable-state schema version` is **not** a torn tail. Go to
section D. A parse failure is not repaired by truncating the journal. Go to
section E.

## B. Inspect the journal tail before changing it

Copy the directory aside first. The copy is your rollback and your evidence.

```sh
STATE_DIR=/var/lib/volvisor/journal
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
cp -a "$STATE_DIR" "$STATE_DIR.before-recovery.$STAMP"
sha256sum "$STATE_DIR/journal.log"
```

Use the frame parser. The exact parser used in the drill is
[records/d4-frames.py](../../evidence/day2/2026-11-03-devhost/records/d4-frames.py);
copy it next to your work or adapt the 20-line scanner below.

```sh
python3 - "$STATE_DIR/journal.log" <<'PY'
import json, struct, sys, zlib
path = sys.argv[1]
data = open(path, "rb").read()
off, seq, frames = 0, 1, []
while len(data) - off >= 20:
    magic, fseq, length, crc = struct.unpack_from("<IQII", data, off)
    if magic != 0x564A4C31 or fseq != seq:
        break
    payload = data[off + 20:off + 20 + length]
    if len(payload) != length or (zlib.crc32(payload) & 0xFFFFFFFF) != crc:
        break
    off += 20 + length
    seq += 1
    frames.append(seq - 1)
print(json.dumps({"frames": frames, "valid_prefix": off, "file_size": len(data)}))
PY
```

Expected for a clean journal:

```json
{"frames": [1, 2, 3, 4], "valid_prefix": 2218, "file_size": 2218}
```

Expected for a torn tail (`valid_prefix < file_size`):

```json
{"frames": [1, 2, 3, 4], "valid_prefix": 2218, "file_size": 2282}
```

A `valid_prefix == file_size` journal with a startup failure is **not** a torn
tail. Do not truncate it.

## C. Recover a structurally torn tail

The daemon truncates the structurally invalid tail on open, after the copy is
safe. Start it and verify the result:

```sh
systemctl start volvisord.service
curl -fsS http://127.0.0.1:8080/healthz
stat -c '%s %n' "$STATE_DIR/journal.log"
sha256sum "$STATE_DIR/journal.log"
```

Expected: health `{"status":"ok"}`, the size returns to `valid_prefix`, and
the prefix hash is identical to the pre-damage prefix. D4 observed:

```text
torn garbage tail truncated back to the valid prefix == 2218
torn garbage tail left the valid prefix byte-identical == 127d817f...
```

For the half-frame case, D4 cut a 2218-byte journal to `start+8` = 1704 bytes;
restart truncated it to 1696, the recovered prefix matched the baseline
prefix, a replay returned the original volume, and a new append succeeded.

Then verify the API, not just the file:

```sh
curl -fsS --config ~/.volvisor-admin.curl \
  http://127.0.0.1:8080/v2/volumes | jq .
```

Check the volumes you expect, their attachments' state/revision/evidence, and
the tombstones. Re-send the last operation with its original `operation_id`
if the client is unsure; the journal's idempotency registry should replay it
rather than create a second volume.

If the daemon still refuses after startup, stop. Read the error: a complete
CRC-valid frame with an unsupported record version or a bad payload is
durable state, not a torn tail (section D).

## D. Complete CRC-valid future/unknown frame — do not truncate

If the parser reports `valid_prefix == file_size` and startup says
`unsupported durable-state schema version`, the file is structurally intact
and contains a record this build cannot understand. **Truncating it is silent
data loss.**

D4 appended a complete `record_version=2` frame and observed:

```text
future-journal startup exit=1
Error: Config("open journal: INTERNAL: unsupported durable-state schema
version: journal record state .../journal.log declares schema_version 2, but
this build supports at most 1; refusing to open it partially (install a
matching or newer build, or restore a pre-upgrade backup)")
ASSERT PASS: future frame was not truncated == 2922
```

Correct actions:

1. Install the matching or newer binary that supports the record version, or
2. restore the pre-upgrade backup and run the matching binary, per
   [upgrade-and-rollback.md](upgrade-and-rollback.md).

Do not `truncate`, `dd`, `jq`, or rewrite `record_version`/`schema_version`.
The refusal is the safety property.

## E. Corrupt provider state file — restore, do not patch

A corrupt `lvm-state.json` (truncated JSON, partial write, bad bytes) must
refuse startup. D4 truncated the seeded file to
`{"schema_version":1,"volumes":{"vol-partial":` and observed:

```text
corrupt-state startup exit=1
Error: Config("lvm provider construction failed: INTERNAL: failed to parse
provider state .../lvm-state.json: EOF while parsing a value at line 1 column 45")
ASSERT PASS: corrupt state file untouched (no silent rewrite) == 879d7597...
```

Procedure:

1. Stop the daemon; keep the corrupt file (rename the directory aside, do not
   overwrite it).
2. Restore the whole state directory from the last verified stopped backup,
   following [backup-and-restore.md](backup-and-restore.md).
3. Start and verify attachment state/revision/generation (the tombstone filter
   is legacy decode-only).
4. Reconcile backend LVs: the restored state may not describe LVs created
   after the backup; do not delete an LV whose state you cannot explain.
5. If there is no backup, stop. Do not hand-edit or synthesize JSON. Escalate
   with the corrupt file, the LVM inventory (`lvs`, `vgs`) and the daemon log.

A corrupt file is never "repaired" by deleting it: a missing file is treated
as a first start and would silently lose the volume inventory.

## Pass condition

- A copy of the damaged state exists before any repair.
- A structurally torn tail is truncated to the last complete frame; the valid
  prefix is byte-identical and the daemon is healthy.
- Replay is idempotent and new appends work.
- A complete future/unknown frame is refused, **not** truncated.
- A corrupt state file is refused and left byte-identical; recovery is from a
  verified backup, not a hand edit.
- The post-recovery API shows the expected volumes and attachments, and any
  restored legacy tombstone filter is intact (it is not consulted).

## What was drilled

D4 exercised all four shapes against real journal/state files: 64 garbage bytes
(2218→2282→2218), a half frame (1704→1696), a CRC-valid `record_version=2`
frame (2922, refused, untouched), and a truncated `lvm-state.json` (refused,
hash unchanged). See [raw/04-torn-journal-and-corrupt-state.log](../../evidence/day2/2026-11-03-devhost/raw/04-torn-journal-and-corrupt-state.log)
and [journal/d4-journal.log](../../evidence/day2/2026-11-03-devhost/journal/d4-journal.log).

**Limits:** single host; fake provider for the journal and loop LVM for the
state; no media bit-rot; no Ceph/DRBD state; no concurrent writer during the
fault injection.
