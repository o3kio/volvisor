#!/usr/bin/env bash
# D1: stopped-state backup, destructive drift, restore; prove attachment
# state/revision/tombstones survive and post-backup operations are lost.
source "$(dirname "$0")/lib.sh"

TAG="d1"
begin_drill "01-backup-restore"
trap 'stop_pidfile "$TAG-daemon" TERM >/dev/null 2>&1 || true; lvm_lab_teardown "$TAG" >/dev/null 2>&1 || true' EXIT

STATE="$RUN/lab-$TAG/journal/lvm-state.json"
CFG="$RUN/configs/$TAG.toml"
JOURNAL="$RUN/lab-$TAG/journal"
BACKUP="$RUN/lab-$TAG/backup"
mkdir -p "$JOURNAL" "$BACKUP"
lvm_lab_setup "$TAG" 768M "$STATE" "vg-d246a"

PORT="$(pick_port)"
TOKEN="d2-disposable-admin-$TAG"
write_header_config "$RUN/configs/$TAG.curl" "$TOKEN"
cat >"$CFG" <<EOF
listen = "127.0.0.1:$PORT"
journal_dir = "$JOURNAL"
provider = "lvm"
lvm_vg_prefix = "vg-d246a"
device_claim_token = "d2-disposable-claim-$TAG"
lvm_state_path = "$STATE"
admin_token = "$TOKEN"
EOF
printf '%s' "$PORT" >"$RUN/records/$TAG.port"

cap "config check (lvm lab)" "$BIN/volvisord" --config "$CFG" --check-config
start_volvisord "$TAG-daemon" "$CFG" "$PORT" || exit 1
BASE="http://127.0.0.1:$PORT"
CURL="$RUN/configs/$TAG.curl"

cat >"$RUN/records/$TAG-create-a.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d1-create-a","project_id":"day2","volume_id":"vol-d1-a","class":"native-local","size_bytes":67108864}
EOF
cat >"$RUN/records/$TAG-create-b.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d1-create-b","project_id":"day2","volume_id":"vol-d1-b","class":"native-local","size_bytes":67108864}
EOF
cat >"$RUN/records/$TAG-create-c.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d1-create-c","project_id":"day2","volume_id":"vol-d1-c","class":"native-local","size_bytes":67108864}
EOF
cat >"$RUN/records/$TAG-attach-a.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d1-attach-a","vm_id":"vm-d1-a","host_id":"host-d1","attachment_id":"att-d1-a","expected_volume_generation":1,"access_mode":"single_writer","requested_frontend":"virtio-blk","vmm_disk_id":"disk0"}
EOF
cat >"$RUN/records/$TAG-attach-b.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d1-attach-b","vm_id":"vm-d1-b","host_id":"host-d1","attachment_id":"att-d1-b","expected_volume_generation":1,"access_mode":"single_writer","requested_frontend":"virtio-blk","vmm_disk_id":"disk1"}
EOF
cat >"$RUN/records/$TAG-report-a.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d1-report-a","attachment_id":"att-d1-a","expected_attachment_generation":1,"state":"active","evidence":"guest_confirmed","reporter":"day2-drill-guest","detail":"drill observation"}
EOF
cat >"$RUN/records/$TAG-detach-b.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d1-detach-b","attachment_id":"att-d1-b","expected_attachment_generation":1,"vm_stopped_or_io_drained_proof":"vm_stopped"}
EOF
cat >"$RUN/records/$TAG-reattach-b.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d1-reattach-b","vm_id":"vm-d1-b","host_id":"host-d1","attachment_id":"att-d1-b","expected_volume_generation":3,"access_mode":"single_writer","requested_frontend":"virtio-blk","vmm_disk_id":"disk1"}
EOF

api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-a.json" 200 "$RUN/records/$TAG-create-a.resp.json"
api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-b.json" 200 "$RUN/records/$TAG-create-b.resp.json"
api_expect "$CURL" POST "$BASE/v2/volumes/vol-d1-a/attach" "$RUN/records/$TAG-attach-a.json" 200 "$RUN/records/$TAG-attach-a.resp.json"
api_expect "$CURL" POST "$BASE/v2/volumes/vol-d1-b/attach" "$RUN/records/$TAG-attach-b.json" 200 "$RUN/records/$TAG-attach-b.resp.json"
api_expect "$CURL" POST "$BASE/v2/volumes/vol-d1-a/attachment-state" "$RUN/records/$TAG-report-a.json" 200 "$RUN/records/$TAG-report-a.resp.json"
api_expect "$CURL" POST "$BASE/v2/volumes/vol-d1-b/detach" "$RUN/records/$TAG-detach-b.json" 200 "$RUN/records/$TAG-detach-b.resp.json"

say "inspect A before backup"
curl --fail-with-body --silent --show-error --config "$CURL" "$BASE/v2/volumes/vol-d1-a" >"$RUN/records/$TAG-inspect-a-before.json"
cat "$RUN/records/$TAG-inspect-a-before.json" | tee -a "$LOG"
say "inspect B before backup (tombstone source)"
curl --fail-with-body --silent --show-error --config "$CURL" "$BASE/v2/volumes/vol-d1-b" >"$RUN/records/$TAG-inspect-b-before.json"
cat "$RUN/records/$TAG-inspect-b-before.json" | tee -a "$LOG"

A_STATE_BEFORE="$(json_get "$RUN/records/$TAG-inspect-a-before.json" '.attachments[0].state')"
A_REV_BEFORE="$(json_get "$RUN/records/$TAG-inspect-a-before.json" '.attachments[0].state_revision')"
assert_eq "$A_STATE_BEFORE" "active" "pre-backup attachment state"
assert_eq "$A_REV_BEFORE" "2" "pre-backup attachment state_revision"

say "stop daemon for the backup (consistency point)"
stop_pidfile "$TAG-daemon" TERM
cap "prove no writer holds the journal lock" flock -n "$JOURNAL/journal.lock" -c true
cap "record state checksums before backup" sha256sum "$JOURNAL/journal.log" "$STATE"
cap "record attachment and tombstone facts in the durable store" jq '{a: .volumes["vol-d1-a"].runtime.attachment, b_retired: .volumes["vol-d1-b"].runtime.retired_attachment_ids}' "$STATE"

BACKUP_T0="$(date +%s.%N)"
cap "backup the whole stopped state directory" tar --xattrs --acls -C "$RUN/lab-$TAG" -cpf "$BACKUP/state.tar" journal
cap "backup archive checksum" sha256sum "$BACKUP/state.tar"
BACKUP_T1="$(date +%s.%N)"
awk -v a="$BACKUP_T0" -v b="$BACKUP_T1" 'BEGIN { printf "backup_duration_secs=%.3f\n", b-a }' | tee -a "$LOG"

say "start after backup and create a post-backup operation (vol-d1-c)"
start_volvisord "$TAG-daemon" "$CFG" "$PORT" || exit 1
api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-c.json" 200 "$RUN/records/$TAG-create-c.resp.json"
cap "post-backup inspect C" curl --fail-with-body --silent --show-error --config "$CURL" "$BASE/v2/volumes/vol-d1-c"
stop_pidfile "$TAG-daemon" TERM
cap "prove post-backup drift is in the live state" jq -r '.volumes | keys[]' "$STATE"
cap "live backend LVs before restore (C exists only here)" lvs --noheadings -o lv_name,vg_name,lv_size "vg-d246a"

RESTORE_T0="$(date +%s.%N)"
say "restore the stopped-state backup"
cap "move the drifted live directory aside (never deleted)" mv "$JOURNAL" "$RUN/lab-$TAG/journal.drifted"
cap "extract the backup" tar --xattrs --acls -C "$RUN/lab-$TAG" -xpf "$BACKUP/state.tar"
cap "restored state checksum (must equal the pre-backup checksum)" sha256sum "$JOURNAL/journal.log" "$STATE"
cap "prove restored durable facts" jq '{a: .volumes["vol-d1-a"].runtime.attachment, b_retired: .volumes["vol-d1-b"].runtime.retired_attachment_ids}' "$STATE"
start_volvisord "$TAG-daemon" "$CFG" "$PORT" || exit 1
RESTORE_T1="$(date +%s.%N)"
awk -v a="$RESTORE_T0" -v b="$RESTORE_T1" 'BEGIN { printf "restore_to_health_duration_secs=%.3f\n", b-a }' | tee -a "$LOG"

say "post-restore assertions"
curl --fail-with-body --silent --show-error --config "$CURL" "$BASE/v2/volumes/vol-d1-a" >"$RUN/records/$TAG-inspect-a-after.json"
curl --fail-with-body --silent --show-error --config "$CURL" "$BASE/v2/volumes/vol-d1-b" >"$RUN/records/$TAG-inspect-b-after.json"
cat "$RUN/records/$TAG-inspect-a-after.json" | tee -a "$LOG"
cat "$RUN/records/$TAG-inspect-b-after.json" | tee -a "$LOG"
A_STATE_AFTER="$(json_get "$RUN/records/$TAG-inspect-a-after.json" '.attachments[0].state')"
A_REV_AFTER="$(json_get "$RUN/records/$TAG-inspect-a-after.json" '.attachments[0].state_revision')"
A_EVIDENCE_AFTER="$(json_get "$RUN/records/$TAG-inspect-a-after.json" '.attachments[0].evidence')"
assert_eq "$A_STATE_AFTER" "active" "post-restore attachment state"
assert_eq "$A_REV_AFTER" "2" "post-restore attachment state_revision"
assert_eq "$A_EVIDENCE_AFTER" "guest_confirmed" "post-restore attachment evidence"
TOMBSTONE_AFTER="$(json_get "$STATE" '.volumes["vol-d1-b"].runtime.retired_attachment_ids[0]')"
assert_eq "$TOMBSTONE_AFTER" "att-d1-b" "post-restore tombstone"

say "prove the tombstone is enforced: reusing att-d1-b must be refused"
api "$CURL" POST "$BASE/v2/volumes/vol-d1-b/attach" "$RUN/records/$TAG-reattach-b.json" >"$RUN/records/$TAG-reattach-b-after.json"
REATTACH_STATUS="$(cat "$RUN/records/last.status")"
cat "$RUN/records/$TAG-reattach-b-after.json" | tee -a "$LOG"
printf 'HTTP %s for retired-attachment reattach\n' "$REATTACH_STATUS" | tee -a "$LOG"
assert_eq "$REATTACH_STATUS" "409" "retired attachment id refusal status"
assert_contains "$(cat "$RUN/records/$TAG-reattach-b-after.json")" "IDEMPOTENCY_CONFLICT" "retired attachment refusal code"

say "prove the post-backup volume C is not in restored state (expected RPO loss)"
api "$CURL" GET "$BASE/v2/volumes/vol-d1-c" >"$RUN/records/$TAG-inspect-c-after.json"
C_STATUS="$(cat "$RUN/records/last.status")"
cat "$RUN/records/$TAG-inspect-c-after.json" | tee -a "$LOG"
printf 'HTTP %s for post-backup volume after restore\n' "$C_STATUS" | tee -a "$LOG"
assert_eq "$C_STATUS" "404" "post-backup volume absent after restore"

say "show the orphaned backend LV left by the discarded post-backup operation"
cap "backend LVs after restore (operator reconciliation required for C)" lvs --noheadings -o lv_name,vg_name,lv_size "vg-d246a"

stop_pidfile "$TAG-daemon" TERM
if end_drill; then
  printf 'D1 COMPLETE\n'
else
  printf 'D1 FAILED\n'
  exit 1
fi
