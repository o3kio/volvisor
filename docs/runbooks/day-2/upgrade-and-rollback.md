# Upgrade and rollback across schema versions — day-2 runbook

**Status: PARTIAL.**

- **DRILLED:** the schema compatibility test, upgrade-on-write, future-version
  refusal, and same-binary reopen. Evidence:
  [D2 raw log](../../evidence/day2/2026-11-03-devhost/raw/02-schema-upgrade.log).
- **NOT RUN — REQUIRES a second Volvisor binary/tag built from the other
  schema-version commit.** No older tagged binary exists on this host and no
  release tag exists. A single binary reading its own prior format is not a
  live old-binary → new-binary upgrade, and it is not a cross-version
  rollback.

Read the normative policy first:
[durable-state schema compatibility](../../schema-compatibility.md).

## Scope and non-goals

This page covers:

- pre-upgrade compatibility checks;
- the upgrade-on-write behavior;
- refusal of durable state written by a newer build;
- the only supported rollback shape: restore the pre-upgrade state and run
  the matching older binary.

It does **not** cover a live in-place downgrade (there is none), a rolling
multi-host upgrade, package upgrade/downgrade scriptlets, or a real
cross-version run. Those require the second binary and, for multi-host, a real
cluster.

## RPO / RTO

- **Upgrade on write:** no loss in the supported-version path. The file is
  read in place and rewritten at the current version on the first successful
  mutation/save; loading alone does not mutate it.
- **Future-version refusal:** no loss; the file is left byte-for-byte intact.
  The daemon refuses to start rather than partially decode it.
- **Rollback:** **RPO = the pre-upgrade backup point.** Every operation after
  that backup is discarded. Backend resources created or mutated after the
  backup must be reconciled by the operator. **RTO** = restore + old-binary
  start + reconciliation; not measured for a real deployment.
- **There is no in-place downgrade.** Do not hand-edit `schema_version`, strip
  fields, or truncate the journal to make an older binary accept it.

## Preconditions

API examples below use an owner-only curl config; do not put a bearer token on
the command line or leave it in shell history.

```sh
umask 077
printf 'header = "Authorization: Bearer %s"\n' "$VOLVISOR_ADMIN_TOKEN" > ~/.volvisor-admin.curl
chmod 600 ~/.volvisor-admin.curl
```

- A verified backup of the **whole** state directory, taken while stopped:
  [backup-and-restore.md](backup-and-restore.md).
- The new binary and a copy of the current/old binary. Record both
  `--version` values and SHA256 digests.
- Maintenance window and authority to stop every writer.
- The schema compatibility test passes at the new commit:

```sh
cargo test --locked -p volvisord --test schema_upgrade -- --nocapture
```

Expected at `e551d26` (observed):

```text
running 2 tests
test each_covered_store_refuses_a_newer_schema_on_open ... ok
test a_populated_pre_versioning_state_directory_upgrades_in_place ... ok
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

This test is **compatibility evidence**, not a live binary upgrade. It proves
the decoder policy against the committed fixtures with one build.

## A. Pre-upgrade inspection

Stop the daemon and inspect the versions actually on disk. Do this for every
covered store you use, not just LVM.

```sh
systemctl stop volvisord.service
flock -n /var/lib/volvisor/journal/journal.lock -c true && echo "lock free"

jq '.schema_version // "pre-versioning (baseline 1)"' /var/lib/volvisor/journal/lvm-state.json
jq '.schema_version // "pre-versioning (baseline 1)"' /var/lib/volvisor/journal/ceph-state.json 2>/dev/null || true
jq '.schema_version // "pre-versioning (baseline 1)"' /var/lib/volvisor/journal/drbd-state.json 2>/dev/null || true
```

Expected: a missing field means baseline 1. A value **greater than** the new
binary's supported version is a stop: the new binary will refuse it, and you
must install a build that supports it. A value of 0 is invalid.

Record the pre-upgrade hashes:

```sh
sha256sum /var/lib/volvisor/journal/* 2>/dev/null
```

Expected: one line per file. Keep this with the backup.

## B. Upgrade

1. Take the stopped backup from [backup-and-restore.md](backup-and-restore.md).
2. Install the new binary. Do not start it against the live directory until
   the backup is verified.
3. Start it and check health and the typed startup result.

```sh
systemctl start volvisord.service
curl -fsS http://127.0.0.1:8080/healthz
systemctl status volvisord.service --no-pager
```

Expected: `{"status":"ok"}` and an active unit. A typed startup refusal is a
stop; read the detail, do not patch the file.

4. Prove upgrade-on-write with a harmless mutation, then inspect the file.

```sh
# Use your normal create/attach smoke with a disposable volume id.
curl -fsS --config ~/.volvisor-admin.curl \
  -H 'content-type: application/json' \
  -d '{"api_version":"volvisor.volume.v2","operation_id":"op-upgrade-smoke","project_id":"ops","volume_id":"vol-upgrade-smoke","class":"native-local","size_bytes":67108864}' \
  http://127.0.0.1:8080/v2/volumes | jq .

jq '.schema_version' /var/lib/volvisor/journal/lvm-state.json
```

Expected at this schema: `1`. The first successful save is what rewrites the
file; a mere restart may leave a pre-versioning file untouched.

D2 observed exactly this after the first write:

```json
{
  "volumes": {
    "vol-upgrade-n1": {
      "runtime": {
        "attachment": {
          "state": "active",
          "state_revision": 3,
          "evidence": "guest_confirmed"
        }
      }
    }
  }
}
```

with `schema_version=1` and tombstones preserved as
`["att-retired-oldest","att-retired-newer"]`. Those values came from the
committed N-1 fixtures plus the tombstone fixture; they are the evidence that
the upgrade did not drop attachment state or retired ids.

5. Exercise the real workload before considering the upgrade complete. Keep
   the pre-upgrade backup until then.

## C. Future-version refusal (safety check, not a rollback)

Never do this on a live directory. In a lab, a file declaring a newer version
must be refused and left untouched:

```sh
# lab only: copy, set schema_version 2, then start the old/supported binary
jq '.schema_version = 2' lvm-state.json > lvm-state.v2.json
mv lvm-state.v2.json lvm-state.json
/path/to/volvisord --config /path/to/volvisord.toml
```

Expected (observed):

```text
Error: Config("lvm provider construction failed: INTERNAL: unsupported
durable-state schema version: lvm state .../lvm-state.json declares
schema_version 2, but this build supports at most 1; refusing to open it
partially (install a matching or newer build, or restore a pre-upgrade
backup)")
```

and the file hash/size must be unchanged. A future journal frame behaves the
same way: typed refusal, **no truncation** (see
[journal-and-state-corruption.md](journal-and-state-corruption.md)).

## D. Rollback (restore, not downgrade)

There is no in-place downgrade. The supported procedure is:

1. Stop every writer, including the witness if its state is part of the
   rollback.
2. Restore the pre-upgrade state directory exactly as in
   [backup-and-restore.md](backup-and-restore.md), verifying checksums.
3. Install and start the binary that matches the restored schema. If the old
   binary sees a newer schema, it must refuse; do not force it.
4. Reconcile backend resources created after the backup (they are outside the
   restored control-plane state).
5. Verify attachments and tombstones, then resume workloads.

Expected during a correct rollback: the restored daemon starts cleanly, the
backup's attachment state/revision/evidence and tombstones are present, and
operations after the backup are absent (HTTP 404). Expected if you try to run
the old binary against the newer directory: exit 1 with the typed
`unsupported durable-state schema version` error.

## Pass condition

- The compatibility test passes at the new commit.
- Pre-upgrade versions are recorded and no store is newer than the new binary.
- A stopped backup exists and verifies.
- The daemon starts and is healthy on the new binary.
- The first successful write records the supported version without losing
  attachment state/revision/tombstones.
- A newer-schema file is refused typed and left byte-identical.
- A rollback uses restore + matching binary, never an in-place version edit.

## What was drilled, and what was not

D2 ran the two-test compatibility suite, assembled a pre-versioning LVM state
from the N-1 and tombstone fixtures, created the matching LVs, started the
daemon, performed one write, and observed `schema_version=1`,
`state_revision=3`, and the two tombstones intact. It then set
`schema_version=2`, saw exit 1 with the typed error and an unchanged hash,
restored version 1, and started the same binary again.

**NOT RUN — REQUIRES a second Volvisor binary/tag.** The drill did not swap
binaries, did not run an older build against a newer directory, and did not
measure a real rollback. A release tag does not currently exist; the
`--version` stamps in the evidence are `e551d26`.

**Limits:** LVM store only for the live file; Ceph/DRBD/migration/peer stores
are covered by the compatibility test, not by a live provider drill. Single
host; no multi-host rolling upgrade.
