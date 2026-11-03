#!/usr/bin/env bash
# D2: schema policy drills.
#   - run the repository's populated-directory compatibility test;
#   - live upgrade-on-write from a pre-versioning state file;
#   - live refusal of a newer schema without rewriting the file.
# This is NOT a live old-binary -> new-binary upgrade; no second binary
# version exists here. See the report.
source "$(dirname "$0")/lib.sh"

TAG="d2"
begin_drill "02-schema-upgrade"
trap 'stop_pidfile "$TAG-daemon" TERM >/dev/null 2>&1 || true; lvm_lab_teardown "$TAG" >/dev/null 2>&1 || true' EXIT

say "repository schema compatibility test (populated directory, all covered stores)"
cap "cargo test schema_upgrade" cargo test --manifest-path "$REPO/Cargo.toml" --locked -p volvisord --test schema_upgrade -- --nocapture

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
   '.moves = {} | .devices = {"dev-d2b01": {"stable_identity":"day2-lab-d2","path":$loop,"vg_name":"vg-d2lab","role":"native_pool","owner_generation":1}} | .volumes += $tomb[0].volumes | del(.schema_version)' \
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
stop_pidfile "$TAG-daemon" TERM

cap "after the first successful write the file carries schema_version 1" jq '.schema_version' "$STATE"
cap "the attachment state/revision survived the upgrade-on-write" jq '.volumes["vol-upgrade-n1"].runtime.attachment | {state,state_revision,evidence}' "$STATE"
cap "the tombstones survived the upgrade-on-write" jq '.volumes["vol-upgrade-tombstone"].runtime.retired_attachment_ids' "$STATE"
assert_eq "$(jq -r '.schema_version' "$STATE")" "1" "upgraded schema_version"
assert_eq "$(jq -r '.volumes["vol-upgrade-n1"].runtime.attachment.state' "$STATE")" "active" "upgraded attachment state"
assert_eq "$(jq -r '.volumes["vol-upgrade-n1"].runtime.attachment.state_revision' "$STATE")" "3" "upgraded attachment state_revision"
assert_eq "$(jq -r '.volumes["vol-upgrade-tombstone"].runtime.retired_attachment_ids | join(",")' "$STATE")" "att-retired-oldest,att-retired-newer" "upgraded tombstones"

say "live refusal of a newer schema: set schema_version=2 and start the daemon"
cp "$STATE" "$RUN/records/$TAG-state-v1.json"
jq '.schema_version = 2' "$STATE" >"$RUN/records/$TAG-state-v2.tmp" && mv "$RUN/records/$TAG-state-v2.tmp" "$STATE"
cp "$STATE" "$RUN/records/$TAG-state-v2.json"
cap "future schema file checksum before startup" sha256sum "$STATE"
set +e
timeout 10s "$BIN/volvisord" --config "$CFG" >"$RUN/raw/$TAG-future.daemon.log" 2>&1
FUTURE_RC=$?
set -e
printf 'future-schema startup exit=%s\n' "$FUTURE_RC" | tee -a "$LOG"
cap "future-schema daemon stderr" cat "$RUN/raw/$TAG-future.daemon.log"
assert_eq "$FUTURE_RC" "1" "future schema startup exit status"
assert_contains "$(cat "$RUN/raw/$TAG-future.daemon.log")" "unsupported durable-state schema version" "typed future-schema refusal"
assert_eq "$(sha256sum "$STATE" | cut -d' ' -f1)" "$(sha256sum "$RUN/records/$TAG-state-v2.json" | cut -d' ' -f1)" "future schema file untouched"

say "restore the supported schema and prove the same binary starts"
cp "$RUN/records/$TAG-state-v1.json" "$STATE"
start_volvisord "$TAG-daemon" "$CFG" "$PORT" || exit 1
api_expect "$CURL" GET "$BASE/v2/volumes/vol-upgrade-n1" "" 200 "$RUN/records/$TAG-inspect-restored.json"
cat "$RUN/records/$TAG-inspect-restored.json" | tee -a "$LOG"
stop_pidfile "$TAG-daemon" TERM

if end_drill; then
  printf 'D2 COMPLETE\n'
else
  printf 'D2 FAILED\n'
  exit 1
fi
