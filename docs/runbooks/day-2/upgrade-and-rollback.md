# Upgrade and rollback across schema versions — day-2 runbook

**Status: DRILLED (single-host lab) — real cross-binary upgrade and
restore-based rollback; schema policy and future-version refusal also
drilled.**

- **DRILLED:** a real old binary built at `8c2fc1a` (LVM state schema 2,
  journal record version 1) upgraded a v2 state directory to schema 3 with
  the current binary at `aad1720` (journal record version 2). The original
  volume survived; the old binary refused the v3 state and the v2 journal
  with the typed unsupported-version error; restoring the whole stopped v2
  backup let the old binary serve the original volume again, and the
  post-backup volume was absent.
- **DRILLED separately:** the current binary's same-binary v3 policy drill
  (pre-versioning → v3 upgrade-on-write, v4 refusal) is [`d2.sh`](scripts/d2.sh).
- **NOT RUN:** a rolling multi-host upgrade, packaged/systemd upgrade and
  downgrade scriptlets, live Ceph/DRBD/migration/peer store upgrades, and a
  data-plane workload during the swap. There is still no release tag.

Evidence:
[2026-11-04 live cross-binary report](../../evidence/day2/2026-11-04-devhost-live/REPORT.md),
[raw D2 live log](../../evidence/day2/2026-11-04-devhost-live/raw/02b-live-upgrade-rollback.log),
[raw D2 v3 policy log](../../evidence/day2/2026-11-04-devhost-live/raw/02-schema-upgrade-v3.log).
Read the normative policy first:
[durable-state schema compatibility](../../schema-compatibility.md).

## Scope and non-goals

This page covers:

- pre-upgrade compatibility checks;
- the upgrade-on-write behavior;
- refusal of durable state written by a newer build;
- the only supported rollback shape: stop, restore the pre-upgrade state
  directory, and start the matching older binary.

It does **not** cover a live in-place downgrade (there is none), a rolling
multi-host upgrade, package upgrade/downgrade scriptlets, or a production
workload. The live drill is single-host and loop-backed.

## RPO / RTO

- **Upgrade on write:** no loss in the supported-version path. The file is
  read in place and rewritten at the current version on the first successful
  mutation/save; loading alone does not mutate it (asserted in the live
  drill).
- **Future-version refusal:** no loss; the file is left byte-for-byte intact.
  The daemon refuses to start rather than partially decode it.
- **Rollback:** **RPO = the pre-upgrade backup point.** Every operation after
  that backup is discarded. Backend resources created or mutated after the
  backup must be reconciled by the operator; the restored daemon does not
  invent their records. **RTO** = restore + old-binary start +
  reconciliation; not measured for a real deployment.
- **There is no in-place downgrade.** Do not hand-edit `schema_version`, strip
  fields, or truncate the journal to make an older binary accept it.

## Preconditions

API examples use an owner-only curl config; do not put a bearer token on the
command line or leave it in shell history.

```sh
umask 077
printf 'header = "Authorization: Bearer %s"\n' "$VOLVISOR_ADMIN_TOKEN" > ~/.volvisor-admin.curl
chmod 600 ~/.volvisor-admin.curl
```

- A verified backup of the **whole** state directory, taken while stopped:
  [backup-and-restore.md](backup-and-restore.md). This is not optional
  pedantry: the new build also stamps the journal's `record_version`, so
  restoring only `lvm-state.json` can leave an older binary refusing the
  newer journal.
- The new binary and a copy of the current/old binary. Record both
  `--version` values and SHA256 digests.
- Maintenance window and authority to stop every writer.
- The schema compatibility test passes at the new commit:

```sh
cargo test --locked -p volvisord --test schema_upgrade -- --nocapture
```

Expected at `aad1720` (observed in the 2026-11-04 bundle): 4 passed, 0 failed,
including `a_v2_stamped_n_minus_one_lvm_state_imports_tombstones_and_upgrades_on_write`
and `a_v1_stamped_n_minus_one_lvm_state_upgrades_on_write`.

For the repeatable lab drill, use the harness instead of hand-building:

```sh
sudo RUN=/tmp/opencode/volvisor-d2live-$(date -u +%Y%m%dT%H%M%SZ) \
  bash docs/runbooks/day-2/scripts/d2-live.sh
```

It builds the old binary at `OLD_COMMIT` (default `8c2fc1a`) in a disposable
worktree, builds the current commit in a clean worktree when the caller's
worktree is dirty, and removes both worktrees afterwards. A preflight report
is available with `scripts/preflight.sh --only d2` (add `--check-old-build`
to build the old commit during preflight).

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
must install a build that supports it. A value of 0 is invalid. The current
LVM ceiling is **3**; journal `record_version` is **2**.

Record the pre-upgrade hashes:

```sh
sha256sum /var/lib/volvisor/journal/* 2>/dev/null
```

Expected: one line per file. Keep this with the backup.

## B. Upgrade

1. Take the stopped backup from [backup-and-restore.md](backup-and-restore.md).
   Back up the **whole directory**, not one store file.
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

4. Prove loading alone does not rewrite the file, then prove upgrade-on-write
   with a harmless mutation.

```sh
# Before the mutation: the old file's version is still what is on disk.
jq '.schema_version' /var/lib/volvisor/journal/lvm-state.json

# Use your normal create/attach smoke with a disposable volume id.
curl -fsS --config ~/.volvisor-admin.curl \
  -H 'content-type: application/json' \
  -d '{"api_version":"volvisor.volume.v2","operation_id":"op-upgrade-smoke","project_id":"ops","volume_id":"vol-upgrade-smoke","class":"native-local","size_bytes":67108864}' \
  http://127.0.0.1:8080/v2/volumes | jq .

jq '.schema_version' /var/lib/volvisor/journal/lvm-state.json
```

Expected with the current build: `4`. A v3 binary writes `3`, a v2 build
writes `2`; the old `8c2fc1a` build in the live drill wrote `2` on its first
create. The first successful save is what rewrites the file; a mere restart
may leave an older file untouched.

5. Exercise the real workload before considering the upgrade complete. Keep
   the pre-upgrade backup until then.

## C. Future-version refusal (safety check, not a rollback)

Never do this on a live directory. In a lab, a file declaring a newer version
must be refused and left untouched:

```sh
# lab only: copy, set schema_version 5 (one above the current v4), then start
jq '.schema_version = 5' lvm-state.json > lvm-state.v5.json
mv lvm-state.v5.json lvm-state.json
/path/to/volvisord --config /path/to/volvisord.toml
```

Expected for the current v4 build:

```text
Error: Config("lvm provider construction failed: INTERNAL: unsupported
durable-state schema version: lvm state .../lvm-state.json declares
schema_version 5, but this build supports at most 4; refusing to open it
partially (install a matching or newer build, or restore a pre-upgrade
backup)")
```

and the file hash/size must be unchanged. The live drill also ran the reverse
cross-version check: the old `8c2fc1a` binary refused a v3 LVM state with
`declares schema_version 3, but this build supports at most 2`, and refused
the post-upgrade directory's journal with
`journal record ... declares schema_version 2, but this build supports at
most 1`. Both refusals left the files byte-identical. A future journal frame
behaves the same way: typed refusal, **no truncation** (see
[journal-and-state-corruption.md](journal-and-state-corruption.md)).

## D. Rollback (restore, not downgrade)

There is no in-place downgrade. The supported procedure is:

1. Stop every writer, including the witness if its state is part of the
   rollback.
2. Restore the pre-upgrade **whole state directory** exactly as in
   [backup-and-restore.md](backup-and-restore.md), verifying checksums. Use
   the backup from before the first new-binary write, not a copy taken after
   the upgrade.
3. Install and start the binary that matches the restored schema. If the old
   binary sees a newer schema, it must refuse; do not force it.
4. Reconcile backend resources created after the backup (they are outside the
   restored control-plane state). The live drill left the post-backup LV in
   the VG; restoring state did not delete it.
5. Verify attachments (state/revision/evidence and generation) and any legacy
   tombstone filter, then resume workloads.

Lab-equivalent commands (from `d2-live.sh`, after stopping the new daemon):

```sh
rm -rf "$JOURNAL"
mkdir -p "$JOURNAL"
tar -C "$JOURNAL" -xf "$RUN/records/d2live-pre-upgrade-state.tar"
sha256sum "$JOURNAL/lvm-state.json"   # must match the pre-upgrade digest
"$OLD_BIN" --config "$OLD_CFG"
```

Expected during a correct rollback: the restored daemon starts cleanly, the
backup's attachment state/revision/evidence and generation are present, any
legacy tombstone filter from an older state file is intact but not consulted,
and operations after the backup are absent (HTTP 404). Expected if you try to
run the old binary against the newer directory: exit 1 with the typed
`unsupported durable-state schema version` error.

## Pass condition

- The compatibility test passes at the new commit.
- Pre-upgrade versions are recorded and no store is newer than the new binary.
- A stopped whole-directory backup exists and verifies.
- The old binary serves the original volume on the v2 state.
- The new binary opens that state without rewriting it on load, then a write
  stamps v3 without losing the original volume.
- A v3 state and a v2 journal are refused typed by the old binary and left
  byte-identical.
- A rollback restores the pre-upgrade directory, the old binary opens it and
  serves the original volume, and a post-backup volume is absent.
- The rollback uses restore + matching binary, never an in-place version edit.

## What was drilled, and what was not

On 2026-11-04 the harness built two real binaries in disposable worktrees:

| Role | Commit | State ceiling | Journal ceiling | `--version` | SHA256 |
|---|---|---|---|---|---|
| OLD | `8c2fc1a5f8bac1fe8d02844191fc9bd9cb61cd9c` | LVM v2 | v1 | `8c2fc1a` | `ec6211874560c7b5cfeb4e38c72fb23bdc77a63d81692374da904fc1237672ab` |
| NEW | `aad17201f03bfdfcec4b74d863b805bc7fa47c76` | LVM v3 | v2 | `aad1720` | `6ebac3f9c5bf3bee53caa7764ccffdbd8ebeea146a164a9aa511c6beaac633ab` |

The caller's worktree was dirty, so NEW was built from a clean worktree at
`aad1720`; the raw log records the dirty caller flag and the clean build
worktree. Both worktrees were removed after the run.

Observed in the passing run (`ASSERT SUMMARY: 0 failures`):

1. OLD opened the lab-seeded v1 state; its create produced
   `schema_version=2` and the original volume `vol-d2live-original`
   (64 MiB) was served. Pre-versioning (a file with no `schema_version`) is
   covered by `d2.sh`, not this drill.
2. OLD reopened that v2 file and still served the original volume, with
   `schema_version` still 2. This proves the downgrade target can read the
   state before any upgrade.
3. The stopped directory was backed up: v2 state
   `702165cd0c04cca2aedadf28873898015bcf4fc9c962c4b6d8fb8bb7552aa68a`,
   v1 journal
   `acd0cd627bb5f16152589169afbcca4af764d11a2435cd340041ea3f7c25af9c`.
4. NEW opened the v2 state, served the original volume, and the file was
   still v2 before the mutation. A second create stamped v3
   (`9aa8aaf282f9fad4bd0098612ed416ce650db80a2248f5d1f7c5cfecaa5b7374`)
   and the original volume's size and generation were unchanged.
5. OLD against the post-upgrade directory exited 1 with the typed journal
   `unsupported durable-state schema version: journal record ... declares
   schema_version 2, but this build supports at most 1` refusal; the v3
   state hash and the v2 journal hash were unchanged.
6. OLD against a fresh journal plus a copy of the v3 LVM state exited 1 with
   `declares schema_version 3, but this build supports at most 2`; the copy
   hash was unchanged.
7. Restoring the tar restored the v2 state and v1 journal byte-for-byte; OLD
   started, served `vol-d2live-original`, and `vol-d2live-upgrade` returned
   HTTP 404. The post-backup LV remained in `lvs` for operator
   reconciliation.

**NOT RUN:** a rolling multi-host upgrade, package/systemd scriptlets, Ceph/
DRBD/migration/peer store live upgrades, a real data workload during the swap,
and a released artifact. **Limits:** one host, one kernel, debug builds,
loop-device LVM, LVM store only. This is not production support or a
zero-downtime claim.
