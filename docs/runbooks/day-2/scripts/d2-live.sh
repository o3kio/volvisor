#!/usr/bin/env bash
# D2 LIVE: real old-binary -> new-binary upgrade and restore-based rollback.
#
# This is deliberately not a simulation of a version bump. It builds the
# product at OLD_COMMIT (default 8c2fc1a, LVM_STATE_SCHEMA_VERSION = 2) in a
# disposable git worktree and builds NEW_COMMIT (default aad1720, the recorded
# v3 build) in a clean worktree when the caller is dirty or not at NEW_COMMIT.
# It then runs the two binaries
# against the same loop-backed LVM lab:
#
#   1. OLD starts on the lab-seeded schema-v1 state file and writes the
#      original volume, producing v2 state (pre-versioning is covered by
#      d2.sh, not this drill);
#   2. the stopped state directory is backed up (v2 state + v1 journal);
#   3. NEW opens that v2 state, serves the original volume, and a new write
#      upgrades the file to v3 without dropping the original volume;
#   4. OLD is run against the post-upgrade directory and refuses it typed;
#   5. OLD is also run against a pristine journal plus a copy of the v3 LVM
#      state, isolating the v3 provider refusal from the v2 journal record;
#   6. the v2 backup is restored and OLD opens it and serves the original
#      volume again (the post-backup volume is gone).
#
# LIMITS: one physical host, loop-device LVM, debug builds, no real tenant
# data path. This is single-host lab evidence, not a rolling multi-host
# upgrade or a production release claim. d2.sh is the same-binary v4
# schema-policy drill.
#
# This is the historical v2->v3 reproduction at the recorded commits, not a
# drill of the current v4 build.
#
# Env overrides: OLD_COMMIT, NEW_COMMIT (default aad1720, the recorded v3
# build; NEW_COMMIT may override it), REPO, BIN, RUN.
# shellcheck disable=SC1091 # resolved from the script directory at runtime
source "$(dirname "$0")/lib.sh"

TAG="d2live"
OLD_COMMIT="${OLD_COMMIT:-8c2fc1a}"
NEW_COMMIT="${NEW_COMMIT:-aad1720}"
OLD_SHORT="$(git -C "$REPO" rev-parse --short=7 "$OLD_COMMIT" 2>/dev/null || true)"
NEW_SHORT="$(git -C "$REPO" rev-parse --short=7 "$NEW_COMMIT" 2>/dev/null || true)"
OLD_WT=""
NEW_WT=""
ORIGINAL_ID="vol-d2live-original"
UPGRADE_ID="vol-d2live-upgrade"
ORIGINAL_SIZE=67108864

begin_drill "02b-live-upgrade-rollback"

D2LIVE_CLEANED=0
cleanup() {
  [[ "$D2LIVE_CLEANED" -eq 1 ]] && return 0
  D2LIVE_CLEANED=1
  stop_pidfile "$TAG-old-step1" TERM >/dev/null 2>&1 || true
  stop_pidfile "$TAG-old" TERM >/dev/null 2>&1 || true
  stop_pidfile "$TAG-old-v2" TERM >/dev/null 2>&1 || true
  stop_pidfile "$TAG-new" TERM >/dev/null 2>&1 || true
  if ! lvm_lab_teardown "$TAG"; then
    printf 'ASSERT FAIL: lab teardown incomplete (loop/VG may still exist)\n' | tee -a "$LOG"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
  fi
  local wt
  for wt in "$OLD_WT" "$NEW_WT"; do
    [[ -n "$wt" && -d "$wt" ]] || continue
    if ! git -C "$REPO" worktree remove --force "$wt" >/dev/null 2>&1; then
      printf 'ASSERT FAIL: could not remove worktree %s\n' "$wt" | tee -a "$LOG"
      ASSERT_FAILS=$((ASSERT_FAILS + 1))
    fi
  done
  git -C "$REPO" worktree prune >/dev/null 2>&1 || true
}
trap cleanup EXIT

{
  printf '# old_commit: %s\n' "$OLD_COMMIT"
  printf '# new_commit: %s\n' "$NEW_COMMIT"
  printf '# old_short: %s\n' "${OLD_SHORT:-unresolved}"
  printf '# new_short: %s\n' "${NEW_SHORT:-unresolved}"
} >>"$LOG"

[[ -n "$OLD_SHORT" ]] || { printf 'unknown OLD_COMMIT %s\n' "$OLD_COMMIT" | tee -a "$LOG"; exit 1; }
[[ -n "$NEW_SHORT" ]] || { printf 'unknown NEW_COMMIT %s\n' "$NEW_COMMIT" | tee -a "$LOG"; exit 1; }

# ---------------------------------------------------------------------------
# Build the two real binaries.
# ---------------------------------------------------------------------------
OLD_WT="$RUN/src-old-$OLD_SHORT"
OLD_TARGET="$RUN/target-old"
OLD_BIN="$OLD_TARGET/debug/volvisord"
NEW_TARGET="$RUN/target-new"
NEW_BIN=""

cap "create a disposable worktree at old commit $OLD_COMMIT" \
  git -C "$REPO" worktree add --detach "$OLD_WT" "$OLD_COMMIT"
cap "build the old binary from the old worktree" \
  env CARGO_TARGET_DIR="$OLD_TARGET" cargo build \
  --manifest-path "$OLD_WT/Cargo.toml" --locked -p volvisord
[[ -x "$OLD_BIN" ]] || { printf 'old binary missing: %s\n' "$OLD_BIN" | tee -a "$LOG"; exit 1; }

CALLER_HEAD="$(git -C "$REPO" rev-parse HEAD)"
CALLER_DIRTY="$(git -C "$REPO" status --porcelain)"
if [[ -z "$CALLER_DIRTY" ]] && [[ "$CALLER_HEAD" == "$(git -C "$REPO" rev-parse "$NEW_COMMIT")" ]]; then
  say "caller worktree is clean at NEW_COMMIT; use the existing new build under BIN"
  NEW_BIN="$BIN/volvisord"
else
  if [[ -n "$CALLER_DIRTY" ]]; then
    say "caller worktree is dirty; build NEW from a clean worktree at $NEW_COMMIT"
  else
    say "caller worktree is clean but not at NEW_COMMIT; build NEW from a clean worktree at $NEW_COMMIT"
  fi
  NEW_WT="$RUN/src-new-$NEW_SHORT"
  cap "create a clean worktree at new commit $NEW_COMMIT" \
    git -C "$REPO" worktree add --detach "$NEW_WT" "$NEW_COMMIT"
  cap "build the new binary from the clean worktree" \
    env CARGO_TARGET_DIR="$NEW_TARGET" cargo build \
    --manifest-path "$NEW_WT/Cargo.toml" --locked -p volvisord
  NEW_BIN="$NEW_TARGET/debug/volvisord"
fi
[[ -x "$NEW_BIN" ]] || { printf 'new binary missing: %s\n' "$NEW_BIN" | tee -a "$LOG"; exit 1; }

cap "old binary version stamp" "$OLD_BIN" --version
cap "new binary version stamp" "$NEW_BIN" --version
# Bind each binary to the commit its build tree was checked out at. The version
# stamp is `git describe --tags --always --dirty`, which prints only a tag name
# at a tagged commit (no SHA), so containing the short SHA is not a valid
# identity check. The worktree HEAD is.
assert_eq "$(git -C "$OLD_WT" rev-parse HEAD)" "$(git -C "$REPO" rev-parse "$OLD_COMMIT")" "old build worktree HEAD is OLD_COMMIT"
assert_contains "$("$OLD_BIN" --version 2>&1)" "volvisord" "old binary reports a volvisord version"
if [[ -n "$NEW_WT" ]]; then
  assert_eq "$(git -C "$NEW_WT" rev-parse HEAD)" "$(git -C "$REPO" rev-parse "$NEW_COMMIT")" "new build worktree HEAD is NEW_COMMIT"
  assert_eq "$(git -C "$NEW_WT" status --porcelain | wc -l | tr -d ' ')" "0" "new build worktree is clean"
  NEW_DESCRIBE="$(git -C "$NEW_WT" describe --tags --always HEAD)"
else
  assert_eq "$(git -C "$REPO" rev-parse HEAD)" "$(git -C "$REPO" rev-parse "$NEW_COMMIT")" "clean caller worktree HEAD is NEW_COMMIT"
  NEW_DESCRIBE="$(git -C "$REPO" describe --tags --always HEAD)"
fi
assert_contains "$("$NEW_BIN" --version 2>&1)" "volvisord" "new binary reports a volvisord version"
# The version stamp is derived from the build tree's `git describe`, so pinning
# it to the tree's describe output proves the binary under BIN is the build of
# NEW_COMMIT, not a stale binary left there by an earlier run.
assert_eq "$("$NEW_BIN" --version 2>&1)" "volvisord $NEW_DESCRIBE" "new binary version stamp matches NEW_COMMIT"
# The NEW binary must come from clean source. A `-dirty` stamp proves it was
# built with uncommitted changes.
assert_not_contains "$("$NEW_BIN" --version 2>&1)" "-dirty" "new binary is not a dirty build"
{
  printf 'old_bin=%s\n' "$OLD_BIN"
  printf 'old_version=%s\n' "$("$OLD_BIN" --version 2>&1)"
  printf 'old_sha256=%s\n' "$(sha256sum "$OLD_BIN" | cut -d' ' -f1)"
  printf 'new_bin=%s\n' "$NEW_BIN"
  printf 'new_version=%s\n' "$("$NEW_BIN" --version 2>&1)"
  printf 'new_sha256=%s\n' "$(sha256sum "$NEW_BIN" | cut -d' ' -f1)"
  printf 'old_commit=%s\n' "$(git -C "$REPO" rev-parse "$OLD_COMMIT")"
  printf 'new_commit=%s\n' "$(git -C "$REPO" rev-parse "$NEW_COMMIT")"
  printf 'old_worktree_sha=%s\n' "$(git -C "$OLD_WT" rev-parse HEAD)"
  # Caller-scoped: this is the invoking repository, not the NEW build tree.
  printf 'caller_worktree_dirty=%s\n' "$([[ -n "$(git -C "$REPO" status --porcelain)" ]] && echo yes || echo no)"
  if [[ -n "$NEW_WT" ]]; then
    printf 'new_worktree_dirty_at_build=%s\n' "$(git -C "$NEW_WT" status --porcelain | wc -l)"
    printf 'new_worktree_sha=%s\n' "$(git -C "$NEW_WT" rev-parse HEAD)"
  else
    printf 'new_worktree_dirty_at_build=caller-clean-path\n'
  fi
} >"$RUN/records/$TAG-binaries.txt"
cap "record binary identity" cat "$RUN/records/$TAG-binaries.txt"

# ---------------------------------------------------------------------------
# Lab state and configs accepted by both builds.
# ---------------------------------------------------------------------------
JOURNAL="$RUN/lab-$TAG/journal"
STATE="$JOURNAL/lvm-state.json"
VG="vg-d2live"
mkdir -p "$JOURNAL"
lvm_lab_setup "$TAG" 4G "$STATE" "$VG" || {
  printf 'lab setup failed\n' | tee -a "$LOG"
  exit 1
}

PORT="$(pick_port)"
TOKEN="d2live-disposable-admin-$TAG"
CLAIM_TOKEN="d2live-disposable-claim-$TAG"
write_header_config "$RUN/configs/$TAG.curl" "$TOKEN"
CURL="$RUN/configs/$TAG.curl"
BASE="http://127.0.0.1:$PORT"

# Both commits accept exactly these fields (the old config is
# deny_unknown_fields); keep separate files so the evidence shows which
# binary read which path.
write_config() {
  local path="$1"
  cat >"$path" <<EOF
listen = "127.0.0.1:$PORT"
journal_dir = "$JOURNAL"
provider = "lvm"
lvm_vg_prefix = "$VG"
device_claim_token = "$CLAIM_TOKEN"
lvm_state_path = "$STATE"
admin_token = "$TOKEN"
EOF
}
OLD_CFG="$RUN/configs/$TAG-old.toml"
NEW_CFG="$RUN/configs/$TAG-new.toml"
write_config "$OLD_CFG"
write_config "$NEW_CFG"
printf '%s' "$PORT" >"$RUN/records/$TAG.port"
cap "old binary accepts the shared config shape" "$OLD_BIN" --check-config --config "$OLD_CFG"
cap "new binary accepts the shared config shape" "$NEW_BIN" --check-config --config "$NEW_CFG"

# ---------------------------------------------------------------------------
# Step 1: OLD on the lab-seeded v1 state; its first write stamps v2.
# ---------------------------------------------------------------------------
say "STEP 1 old binary: open the lab v1 state file and create the original volume"
start_volvisord_bin "$OLD_BIN" "$TAG-old-step1" "$OLD_CFG" "$PORT" || exit 1
cat >"$RUN/records/$TAG-create-original.json" <<EOF
{"api_version":"volvisor.volume.v2","operation_id":"op-d2live-original","project_id":"day2","volume_id":"$ORIGINAL_ID","class":"native-local","size_bytes":$ORIGINAL_SIZE}
EOF
api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-original.json" 200 "$RUN/records/$TAG-create-original.resp.json"
api_expect "$CURL" GET "$BASE/v2/volumes/$ORIGINAL_ID" "" 200 "$RUN/records/$TAG-original-old.json"
assert_eq "$(jq -r '.volume_id' "$RUN/records/$TAG-original-old.json")" "$ORIGINAL_ID" "old binary serves the original volume"
assert_eq "$(jq -r '.provisioned_bytes' "$RUN/records/$TAG-original-old.json")" "$ORIGINAL_SIZE" "old binary reports the original size"

cap "old binary's first write stamped schema_version 2" jq '.schema_version' "$STATE"
assert_eq "$(jq -r '.schema_version' "$STATE")" "2" "old build wrote LVM state schema v2"
assert_eq "$(jq -r --arg v "$ORIGINAL_ID" '.volumes[$v].entry.size_bytes' "$STATE")" "$ORIGINAL_SIZE" "original volume is in the v2 state"
ORIGINAL_GENERATION="$(jq -r --arg v "$ORIGINAL_ID" '.volumes[$v].entry.generation' "$STATE")"
cap "record lvs before the upgrade" lvs --noheadings -o vg_name,lv_name,lv_size "$VG"
stop_or_exit "$TAG-old-step1"

# ---------------------------------------------------------------------------
# Step 1b: OLD reopens the v2 file it wrote. This proves the downgrade target
# can still serve before any upgrade happens, without rewriting the schema.
# ---------------------------------------------------------------------------
say "STEP 1b old binary: reopen the v2 state and serve the original volume"
start_volvisord_bin "$OLD_BIN" "$TAG-old-v2" "$OLD_CFG" "$PORT" || exit 1
api_expect "$CURL" GET "$BASE/v2/volumes/$ORIGINAL_ID" "" 200 "$RUN/records/$TAG-original-old-v2.json"
assert_eq "$(jq -r '.volume_id' "$RUN/records/$TAG-original-old-v2.json")" "$ORIGINAL_ID" "old binary reopens v2 and serves the original volume"
assert_eq "$(jq -r '.provisioned_bytes' "$RUN/records/$TAG-original-old-v2.json")" "$ORIGINAL_SIZE" "old binary reports the original size after reopen"
cap "reopening the v2 file with the old binary leaves schema_version 2" jq '.schema_version' "$STATE"
assert_eq "$(jq -r '.schema_version' "$STATE")" "2" "old binary reopens v2 without rewriting its schema"
stop_or_exit "$TAG-old-v2"
assert_no_writer "$JOURNAL" "backup consistency point has no writer"

# ---------------------------------------------------------------------------
# Step 2: stopped whole-directory backup (v2 state + v1 journal).
# ---------------------------------------------------------------------------
BACKUP="$RUN/records/$TAG-pre-upgrade-state.tar"
PRE_STATE_SHA="$(sha256sum "$STATE" | cut -d' ' -f1)"
PRE_JOURNAL_SHA="$(sha256sum "$JOURNAL/journal.log" | cut -d' ' -f1)"
cap "record pre-upgrade state checksum" sha256sum "$STATE"
cap "record pre-upgrade journal checksum" sha256sum "$JOURNAL/journal.log"
cap "back up the whole stopped state directory" tar -C "$JOURNAL" -cf "$BACKUP" .
cap "pre-upgrade backup checksum" sha256sum "$BACKUP"
cp "$STATE" "$RUN/records/$TAG-state-v2.json"
cp "$JOURNAL/journal.log" "$RUN/records/$TAG-journal-v1.log"

# ---------------------------------------------------------------------------
# Step 3: NEW opens v2, serves the original, first new write upgrades to v3.
# ---------------------------------------------------------------------------
say "STEP 3 new binary: open the v2 state and prove loading alone does not upgrade or rewrite it"
PRE_LOAD_STATE_SHA="$(sha256sum "$STATE" | cut -d' ' -f1)"
start_volvisord_bin "$NEW_BIN" "$TAG-new" "$NEW_CFG" "$PORT" || exit 1
api_expect "$CURL" GET "$BASE/v2/volumes/$ORIGINAL_ID" "" 200 "$RUN/records/$TAG-original-new.json"
assert_eq "$(jq -r '.volume_id' "$RUN/records/$TAG-original-new.json")" "$ORIGINAL_ID" "new binary serves the original volume"
cap "loading the v2 file alone leaves schema_version 2" jq '.schema_version' "$STATE"
assert_eq "$(jq -r '.schema_version' "$STATE")" "2" "new binary does not upgrade merely on open"
assert_eq "$(sha256sum "$STATE" | cut -d' ' -f1)" "$PRE_LOAD_STATE_SHA" "new binary does not rewrite the v2 state merely on open"

cat >"$RUN/records/$TAG-create-upgrade.json" <<EOF
{"api_version":"volvisor.volume.v2","operation_id":"op-d2live-upgrade","project_id":"day2","volume_id":"$UPGRADE_ID","class":"native-local","size_bytes":67108864}
EOF
api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-upgrade.json" 200 "$RUN/records/$TAG-create-upgrade.resp.json"
api_expect "$CURL" GET "$BASE/v2/volumes/$ORIGINAL_ID" "" 200 "$RUN/records/$TAG-original-after-upgrade.json"
assert_eq "$(jq -r '.volume_id' "$RUN/records/$TAG-original-after-upgrade.json")" "$ORIGINAL_ID" "original volume survives the upgrade"
stop_or_exit "$TAG-new"
assert_no_writer "$JOURNAL" "post-upgrade state has no writer before reading it"

cap "the new binary's first write stamped schema_version 3" jq '.schema_version' "$STATE"
assert_eq "$(jq -r '.schema_version' "$STATE")" "3" "new build wrote LVM state schema v3"
assert_eq "$(jq -r --arg v "$ORIGINAL_ID" '.volumes[$v].entry.size_bytes' "$STATE")" "$ORIGINAL_SIZE" "original volume size survives the upgrade"
assert_eq "$(jq -r --arg v "$ORIGINAL_ID" '.volumes[$v].entry.generation' "$STATE")" "$ORIGINAL_GENERATION" "original volume generation survives the upgrade"
assert_eq "$(jq -r --arg v "$UPGRADE_ID" '.volumes[$v].entry.size_bytes' "$STATE")" "67108864" "post-upgrade volume is recorded"
cp "$STATE" "$RUN/records/$TAG-state-v3.json"
V3_STATE_SHA="$(sha256sum "$STATE" | cut -d' ' -f1)"
cap "record post-upgrade state checksum" sha256sum "$STATE"
# Retain the post-upgrade journal itself: Step 4a's refusal names its record
# version, and a reviewer must be able to verify that claim from the bundle.
cp "$JOURNAL/journal.log" "$RUN/records/$TAG-journal-post-upgrade.log"
POST_JOURNAL_SHA="$(sha256sum "$RUN/records/$TAG-journal-post-upgrade.log" | cut -d' ' -f1)"
cap "record post-upgrade journal checksum" sha256sum "$RUN/records/$TAG-journal-post-upgrade.log"

# ---------------------------------------------------------------------------
# Step 4a: OLD refuses the post-upgrade directory (typed, no rewrite).
# ---------------------------------------------------------------------------
say "STEP 4a old binary against the real post-upgrade directory (v2 journal + v3 state)"
V3_BEFORE_REFUSAL="$(sha256sum "$STATE" | cut -d' ' -f1)"
# shellcheck disable=SC2016 # expanded by the inner bash, not by this shell
CAP_EXPECT_RC=1 cap "old binary refuses the post-upgrade directory" \
  bash -c 'timeout 10s "$1" --config "$2" >"$3" 2>&1' _ "$OLD_BIN" "$OLD_CFG" "$RUN/raw/$TAG-old-refusal.daemon.log"
REFUSAL_LOG="$RUN/raw/$TAG-old-refusal.daemon.log"
cap "old refusal stderr" cat "$REFUSAL_LOG"
assert_contains "$(cat "$REFUSAL_LOG")" "unsupported durable-state schema version" "typed future-version refusal on the real directory"
assert_contains "$(cat "$REFUSAL_LOG")" "journal record" "refusal names the journal artifact it opened first"
assert_contains "$(cat "$REFUSAL_LOG")" "schema_version 2" "refusal names the v2 journal record"
assert_contains "$(cat "$REFUSAL_LOG")" "supports at most 1" "old build names its v1 journal ceiling"
assert_eq "$(sha256sum "$STATE" | cut -d' ' -f1)" "$V3_BEFORE_REFUSAL" "post-upgrade state untouched by old refusal"
assert_eq "$(sha256sum "$JOURNAL/journal.log" | cut -d' ' -f1)" "$POST_JOURNAL_SHA" "post-upgrade journal untouched by old refusal"

# ---------------------------------------------------------------------------
# Step 4b: isolate the v3 provider refusal from the v2 journal record.
# ---------------------------------------------------------------------------
say "STEP 4b old binary against a fresh journal plus a copy of the v3 LVM state"
REFUSAL_DIR="$RUN/lab-$TAG/refusal"
mkdir -p "$REFUSAL_DIR/journal"
cp "$STATE" "$REFUSAL_DIR/journal/lvm-state.json"
OLD_REFUSAL_CFG="$RUN/configs/$TAG-old-refusal.toml"
cat >"$OLD_REFUSAL_CFG" <<EOF
listen = "127.0.0.1:$(pick_port)"
journal_dir = "$REFUSAL_DIR/journal"
provider = "lvm"
lvm_vg_prefix = "$VG"
device_claim_token = "$CLAIM_TOKEN"
lvm_state_path = "$REFUSAL_DIR/journal/lvm-state.json"
admin_token = "$TOKEN"
EOF
REFUSED_STATE="$REFUSAL_DIR/journal/lvm-state.json"
REFUSED_BEFORE="$(sha256sum "$REFUSED_STATE" | cut -d' ' -f1)"
# shellcheck disable=SC2016 # expanded by the inner bash, not by this shell
CAP_EXPECT_RC=1 cap "old binary refuses the isolated v3 LVM state" \
  bash -c 'timeout 10s "$1" --config "$2" >"$3" 2>&1' _ "$OLD_BIN" "$OLD_REFUSAL_CFG" "$RUN/raw/$TAG-old-v3-refusal.daemon.log"
ISOLATED_LOG="$RUN/raw/$TAG-old-v3-refusal.daemon.log"
cap "isolated v3 refusal stderr" cat "$ISOLATED_LOG"
assert_contains "$(cat "$ISOLATED_LOG")" "unsupported durable-state schema version" "typed v3 refusal"
assert_contains "$(cat "$ISOLATED_LOG")" "schema_version 3" "refusal names the v3 file"
assert_contains "$(cat "$ISOLATED_LOG")" "supports at most 2" "old build names its v2 ceiling"
assert_eq "$(sha256sum "$REFUSED_STATE" | cut -d' ' -f1)" "$REFUSED_BEFORE" "v3 state copy untouched by old refusal"

# ---------------------------------------------------------------------------
# Step 5: restore the stopped v2 backup and serve the original volume.
# ---------------------------------------------------------------------------
say "STEP 5 restore the pre-upgrade v2 state directory and restart the old binary"
[[ "$JOURNAL" == "$RUN/"* ]] || { printf 'refusing to remove journal path outside RUN: %s\n' "$JOURNAL" | tee -a "$LOG"; exit 1; }
rm -rf "$JOURNAL"
mkdir -p "$JOURNAL"
cap "restore the whole pre-upgrade state directory" tar -C "$JOURNAL" -xf "$BACKUP"
cap "restored state checksum" sha256sum "$STATE"
assert_eq "$(jq -r '.schema_version' "$STATE")" "2" "restored state is the pre-upgrade v2 file"
assert_eq "$(sha256sum "$STATE" | cut -d' ' -f1)" "$PRE_STATE_SHA" "restored state is byte-identical to the backup source"
assert_eq "$(sha256sum "$JOURNAL/journal.log" | cut -d' ' -f1)" "$PRE_JOURNAL_SHA" "restored journal is byte-identical to the backup source"

start_volvisord_bin "$OLD_BIN" "$TAG-old" "$OLD_CFG" "$PORT" || exit 1
api_expect "$CURL" GET "$BASE/v2/volumes/$ORIGINAL_ID" "" 200 "$RUN/records/$TAG-original-restored.json"
assert_eq "$(jq -r '.volume_id' "$RUN/records/$TAG-original-restored.json")" "$ORIGINAL_ID" "old binary serves the restored original volume"
api_expect "$CURL" GET "$BASE/v2/volumes/$UPGRADE_ID" "" 404 "$RUN/records/$TAG-upgrade-after-restore.json"
cap "post-backup volume response after restore" cat "$RUN/records/$TAG-upgrade-after-restore.json"
cap "record lvs after restore (the post-backup LV may remain for operator reconciliation)" \
  lvs --noheadings -o vg_name,lv_name,lv_size "$VG"
stop_or_exit "$TAG-old"
assert_no_writer "$JOURNAL" "restored state has no writer before teardown"

{
  printf 'pre_upgrade_state_sha256=%s\n' "$PRE_STATE_SHA"
  printf 'pre_upgrade_journal_sha256=%s\n' "$PRE_JOURNAL_SHA"
  printf 'post_upgrade_state_sha256=%s\n' "$V3_STATE_SHA"
  printf 'post_upgrade_journal_sha256=%s\n' "$POST_JOURNAL_SHA"
  printf 'pre_upgrade_backup_sha256=%s\n' "$(sha256sum "$BACKUP" | cut -d' ' -f1)"
} >"$RUN/records/$TAG-sha256.txt"
cap "record the state/journal digest table" cat "$RUN/records/$TAG-sha256.txt"
cleanup

if end_drill; then
  printf 'D2 LIVE COMPLETE\n' | tee -a "$LOG"
else
  printf 'D2 LIVE FAILED\n' | tee -a "$LOG"
  exit 1
fi
