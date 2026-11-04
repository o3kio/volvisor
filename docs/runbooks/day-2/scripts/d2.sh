#!/usr/bin/env bash
# D2: same-binary schema policy drill (current binary, current v3 schema).
#   - run the repository's populated-directory compatibility test;
#   - live upgrade-on-write from a pre-versioning state file to v3;
#   - live refusal of a newer schema without rewriting the file.
# This is NOT a live old-binary -> new-binary upgrade and NOT a rollback
# drill. Those are performed by d2-live.sh with a real v2 binary built from
# OLD_COMMIT and the current v3 build. See upgrade-and-rollback.md and the
# evidence bundles.
# shellcheck disable=SC1091 # resolved from the script directory at runtime
source "$(dirname "$0")/lib.sh"

TAG="d2"
begin_drill "02-schema-upgrade"
D2_CLEANED=0
cleanup() {
  [[ "$D2_CLEANED" -eq 1 ]] && return 0
  D2_CLEANED=1
  stop_pidfile "$TAG-daemon" TERM >/dev/null 2>&1 || true
  if ! lvm_lab_teardown "$TAG"; then
    printf 'ASSERT FAIL: lab teardown incomplete (loop/VG may still exist)\n' | tee -a "$LOG"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
  fi
}
trap cleanup EXIT

say "repository schema compatibility test (populated directory, all covered stores)"
cap "cargo test schema_upgrade" cargo test --manifest-path "$REPO/Cargo.toml" --locked -p volvisord --test schema_upgrade -- --nocapture

# Capture the exact binary identity after the test build and immediately
# before the daemon starts: `cargo test -p volvisord` can rebuild
# target/debug/volvisord, so a digest taken earlier could name a binary the
# drill did not run.
D2_BIN="$BIN/volvisord"
D2_BIN_VERSION="$("$D2_BIN" --version 2>&1 || true)"
D2_BIN_SHA="$(sha256sum "$D2_BIN" | cut -d' ' -f1)"
{
  printf 'binary_path=%s\n' "$D2_BIN"
  printf 'binary_version=%s\n' "$D2_BIN_VERSION"
  printf 'binary_sha256=%s\n' "$D2_BIN_SHA"
  printf 'caller_worktree_dirty=%s\n' "$([[ -n "$(git -C "$REPO" status --porcelain)" ]] && echo yes || echo no)"
  printf 'source_sha=%s\n' "$(git -C "$REPO" rev-parse HEAD)"
} >"$RUN/records/$TAG-binaries.txt"
cap "record the v3 drill binary identity (version, sha256, dirty flag)" cat "$RUN/records/$TAG-binaries.txt"

STATE="$RUN/lab-$TAG/journal/lvm-state.json"
JOURNAL="$RUN/lab-$TAG/journal"
CFG="$RUN/configs/$TAG.toml"
VG="vg-d2lab"
mkdir -p "$JOURNAL"
lvm_lab_setup "$TAG" 4G "$STATE" "$VG" || {
  printf 'lab setup failed\n' | tee -a "$LOG"
  exit 1
}

# Replace the lab-seeded state with a real pre-versioning shape assembled
# from the golden fixtures: the N-1 attachment record plus the tombstone
# volume. The device claim points at this lab's loop VG. No schema_version
# field (baseline), exactly like a file written before versioning.
N1="$REPO/tests/fixtures/schema/lvm-state-v1-n1.json"
TOMB="$REPO/tests/fixtures/schema/lvm-state-v1-tombstones.json"
LOOP="$(cat "$RUN/records/$TAG.loop")"
jq --arg loop "$LOOP" \
   --slurpfile tomb "$TOMB" \
   '.moves = {} | .devices = {"dev-d2b01": {"stable_identity":"day2-lab-d2","path":$loop,"vg_name":"vg-d2lab","role":"native_pool","owner_generation":1}} | .volumes += $tomb[0].volumes | .volumes |= with_entries(.value.entry.vg_name = "vg-d2lab") | del(.schema_version)' \
   "$N1" >"$STATE"
cap "record the pre-versioning state (no schema_version; attachment + tombstones)" cat "$STATE"
cap "create the fixture LVs so the pre-versioning records reconcile" lvcreate --yes -n vol-vol-upgrade-n1-abcd1234 -L 2G vg-d2lab
cap "create the fixture LV for the tombstone volume" lvcreate --yes -n vol-vol-upgrade-tombstone-abcd1234 -L 1G vg-d2lab
cap "state has no schema_version before startup" jq 'has("schema_version")' "$STATE"
cap "pre-upgrade state checksum" sha256sum "$STATE"

PORT="$(pick_port)"
TOKEN="d2-disposable-admin-$TAG"
write_header_config "$RUN/configs/$TAG.curl" "$TOKEN"
cat >"$CFG" <<EOF
listen = "127.0.0.1:$PORT"
journal_dir = "$JOURNAL"
provider = "lvm"
lvm_vg_prefix = "vg-d2lab"
device_claim_token = "d2-disposable-claim-$TAG"
lvm_state_path = "$STATE"
admin_token = "$TOKEN"
EOF
printf '%s' "$PORT" >"$RUN/records/$TAG.port"

start_volvisord "$TAG-daemon" "$CFG" "$PORT" || exit 1
BASE="http://127.0.0.1:$PORT"
CURL="$RUN/configs/$TAG.curl"

cat >"$RUN/records/$TAG-create-new.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d2-create-new","project_id":"day2","volume_id":"vol-d2-new","class":"native-local","size_bytes":67108864}
EOF
api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-new.json" 200 "$RUN/records/$TAG-create-new.resp.json"
stop_or_exit "$TAG-daemon"
assert_no_writer "$JOURNAL" "stopped state has no writer before reading the upgraded file"

cap "after the first successful write the file carries schema_version 3" jq '.schema_version' "$STATE"
cap "the attachment state/revision survived the upgrade-on-write" jq '.volumes["vol-upgrade-n1"].runtime.attachment | {state,state_revision,evidence}' "$STATE"
cap "the v2 tombstone vector was imported into the legacy v3 filter (decode only)" jq '.volumes["vol-upgrade-tombstone"].runtime | {legacy_vector: (.retired_attachment_ids // "absent"), filter_inserted: .retired_attachment_filter.inserted}' "$STATE"
assert_eq "$(jq -r '.schema_version' "$STATE")" "3" "upgraded schema_version"
assert_eq "$(jq -r '.volumes["vol-upgrade-n1"].runtime.attachment.state' "$STATE")" "active" "upgraded attachment state"
assert_eq "$(jq -r '.volumes["vol-upgrade-n1"].runtime.attachment.state_revision' "$STATE")" "3" "upgraded attachment state_revision"
assert_eq "$(jq -r '.volumes["vol-upgrade-tombstone"].runtime.retired_attachment_ids // "absent"' "$STATE")" "absent" "legacy tombstone vector is not rewritten"
assert_eq "$(jq -r '.volumes["vol-upgrade-tombstone"].runtime.retired_attachment_filter.inserted' "$STATE")" "2" "tombstones imported into the v3 filter"

say "live refusal of a newer schema: construct a hand-edited synthetic v4 copy and start the daemon"
cp "$STATE" "$RUN/records/$TAG-state-supported.json"
# This copy is deliberately hand-edited test data, not a file any Volvisor
# version wrote. Record the construction command and the resulting digest.
printf '$ jq %q %q > %q\n' '.schema_version = 4' \
  "$RUN/records/$TAG-state-supported.json" "$RUN/records/$TAG-state-v4.json" | tee -a "$LOG"
jq '.schema_version = 4' "$RUN/records/$TAG-state-supported.json" >"$RUN/records/$TAG-state-v4.json"
cp "$RUN/records/$TAG-state-v4.json" "$STATE"
cap "future schema file checksum before startup" sha256sum "$STATE"
# The harness runs without `set -e`, so capture the expected refusal status
# explicitly instead of toggling a shell option that was never enabled.
timeout 10s "$BIN/volvisord" --config "$CFG" >"$RUN/raw/$TAG-future.daemon.log" 2>&1
FUTURE_RC=$?
printf 'future-schema startup exit=%s\n' "$FUTURE_RC" | tee -a "$LOG"
cap "future-schema daemon stderr" cat "$RUN/raw/$TAG-future.daemon.log"
assert_eq "$FUTURE_RC" "1" "future schema startup exit status"
assert_contains "$(cat "$RUN/raw/$TAG-future.daemon.log")" "unsupported durable-state schema version" "typed future-schema refusal"
assert_contains "$(cat "$RUN/raw/$TAG-future.daemon.log")" "supports at most 3" "future-schema refusal names the v3 ceiling"
assert_eq "$(sha256sum "$STATE" | cut -d' ' -f1)" "$(sha256sum "$RUN/records/$TAG-state-v4.json" | cut -d' ' -f1)" "future schema file untouched"

say "restore the supported schema and prove the same binary starts"
cp "$RUN/records/$TAG-state-supported.json" "$STATE"
start_volvisord "$TAG-daemon" "$CFG" "$PORT" || exit 1
api_expect "$CURL" GET "$BASE/v2/volumes/vol-upgrade-n1" "" 200 "$RUN/records/$TAG-inspect-restored.json"
assert_eq "$(jq -r '.state' "$RUN/records/$TAG-inspect-restored.json")" "Attached" "restored volume is served as Attached, not Failed"
tee -a "$LOG" <"$RUN/records/$TAG-inspect-restored.json"
stop_or_exit "$TAG-daemon"
assert_no_writer "$JOURNAL" "stopped state has no writer before teardown"
cleanup

if end_drill; then
  printf 'D2 COMPLETE\n' | tee -a "$LOG"
else
  printf 'D2 FAILED\n' | tee -a "$LOG"
  exit 1
fi
