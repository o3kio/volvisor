#!/usr/bin/env bash
# D1: stopped-state backup, destructive drift, restore; prove attachment
# state/revision/generation fence survive and post-backup operations are lost.
# shellcheck disable=SC1091 # resolved from the script directory at runtime
source "$(dirname "$0")/lib.sh"

TAG="d1"
begin_drill "01-backup-restore"
# Every daemon session gets its own tag so lib.sh does not truncate an earlier
# session's daemon log. The trap reuses the same cleanup, so a failed early
# exit still releases the lab.
D1_CREATE="$TAG-daemon-create"
D1_DRIFT="$TAG-daemon-drift"
D1_RESTORE="$TAG-daemon-restore"
D1_CLEANED=0
cleanup() {
  [[ "$D1_CLEANED" -eq 1 ]] && return 0
  D1_CLEANED=1
  stop_pidfile "$D1_CREATE" TERM >/dev/null 2>&1 || true
  stop_pidfile "$D1_DRIFT" TERM >/dev/null 2>&1 || true
  stop_pidfile "$D1_RESTORE" TERM >/dev/null 2>&1 || true
  if ! lvm_lab_teardown "$TAG"; then
    printf 'ASSERT FAIL: lab teardown incomplete (loop/VG may still exist)\n' | tee -a "$LOG"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
  fi
}
trap cleanup EXIT

# Bind the run to the exact caller build. D1 uses the same binary as D2, but
# the digest is captured here rather than borrowed from the later D2 run.
D1_BIN="$BIN/volvisord"
D1_BIN_VERSION="$("$D1_BIN" --version 2>&1 || true)"
D1_BIN_SHA="$(sha256sum "$D1_BIN" | cut -d' ' -f1)"
{
  printf 'binary_path=%s\n' "$D1_BIN"
  printf 'binary_version=%s\n' "$D1_BIN_VERSION"
  printf 'binary_sha256=%s\n' "$D1_BIN_SHA"
  printf 'caller_worktree_dirty=%s\n' "$([[ -n "$(git -C "$REPO" status --porcelain)" ]] && echo yes || echo no)"
  printf 'source_sha=%s\n' "$(git -C "$REPO" rev-parse HEAD)"
} >"$RUN/records/$TAG-binaries.txt"
cap "record the D1 drill binary identity (version, sha256, dirty flag)" cat "$RUN/records/$TAG-binaries.txt"

STATE="$RUN/lab-$TAG/journal/lvm-state.json"
CFG="$RUN/configs/$TAG.toml"
JOURNAL="$RUN/lab-$TAG/journal"
BACKUP="$RUN/lab-$TAG/backup"
mkdir -p "$JOURNAL" "$BACKUP"
lvm_lab_setup "$TAG" 768M "$STATE" "vg-d246a" || {
  printf 'lab setup failed\n' | tee -a "$LOG"
  exit 1
}

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

cap "config check (lvm lab)" "$D1_BIN" --config "$CFG" --check-config
start_volvisord_bin "$D1_BIN" "$D1_CREATE" "$CFG" "$PORT" || exit 1
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
tee -a "$LOG" < "$RUN/records/$TAG-inspect-a-before.json"
say "inspect B before backup (retired-generation source)"
curl --fail-with-body --silent --show-error --config "$CURL" "$BASE/v2/volumes/vol-d1-b" >"$RUN/records/$TAG-inspect-b-before.json"
tee -a "$LOG" < "$RUN/records/$TAG-inspect-b-before.json"

A_STATE_BEFORE="$(json_get "$RUN/records/$TAG-inspect-a-before.json" '.attachments[0].state')"
A_REV_BEFORE="$(json_get "$RUN/records/$TAG-inspect-a-before.json" '.attachments[0].state_revision')"
assert_eq "$A_STATE_BEFORE" "active" "pre-backup attachment state"
assert_eq "$A_REV_BEFORE" "2" "pre-backup attachment state_revision"

say "stop daemon for the backup (consistency point)"
stop_or_exit "$D1_CREATE"
assert_no_writer "$JOURNAL" "backup consistency point has no writer"
cap "record state checksums before backup" sha256sum "$JOURNAL/journal.log" "$STATE"
PRE_JOURNAL_SHA="$(sha256sum "$JOURNAL/journal.log" | cut -d' ' -f1)"
PRE_STATE_SHA="$(sha256sum "$STATE" | cut -d' ' -f1)"
cap "record attachment and retired-generation facts in the durable store" jq '{a: .volumes["vol-d1-a"].runtime.attachment, b_generation: .volumes["vol-d1-b"].entry.generation, b_filter: (.volumes["vol-d1-b"].runtime.retired_attachment_filter // "absent")}' "$STATE"

BACKUP_T0="$(date +%s.%N)"
cap "backup the whole stopped state directory" tar --xattrs --acls -C "$RUN/lab-$TAG" -cpf "$BACKUP/state.tar" journal
cap "backup archive checksum" sha256sum "$BACKUP/state.tar"
BACKUP_T1="$(date +%s.%N)"
awk -v a="$BACKUP_T0" -v b="$BACKUP_T1" 'BEGIN { printf "backup_duration_secs=%.3f\n", b-a }' | tee -a "$LOG"

say "start after backup and create a post-backup operation (vol-d1-c)"
start_volvisord_bin "$D1_BIN" "$D1_DRIFT" "$CFG" "$PORT" || exit 1
api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-c.json" 200 "$RUN/records/$TAG-create-c.resp.json"
cap "post-backup inspect C" curl --fail-with-body --silent --show-error --config "$CURL" "$BASE/v2/volumes/vol-d1-c"
stop_or_exit "$D1_DRIFT"
cap "prove post-backup drift is in the live state" jq -r '.volumes | keys[]' "$STATE"
cap "live backend LVs before restore (C exists only here)" lvs --noheadings -o lv_name,vg_name,lv_size "vg-d246a"

RESTORE_T0="$(date +%s.%N)"
say "restore the stopped-state backup"
cap "move the drifted live directory aside (never deleted)" mv "$JOURNAL" "$RUN/lab-$TAG/journal.drifted"
cap "extract the backup" tar --xattrs --acls -C "$RUN/lab-$TAG" -xpf "$BACKUP/state.tar"
cap "restored state checksum (must equal the pre-backup checksum)" sha256sum "$JOURNAL/journal.log" "$STATE"
assert_eq "$(sha256sum "$JOURNAL/journal.log" | cut -d' ' -f1)" "$PRE_JOURNAL_SHA" "restored journal is byte-identical to the backup source"
assert_eq "$(sha256sum "$STATE" | cut -d' ' -f1)" "$PRE_STATE_SHA" "restored state is byte-identical to the backup source"
assert_no_writer "$JOURNAL" "restored journal has no writer before restart"
cap "prove restored durable facts" jq '{a: .volumes["vol-d1-a"].runtime.attachment, b_generation: .volumes["vol-d1-b"].entry.generation, b_filter: (.volumes["vol-d1-b"].runtime.retired_attachment_filter // "absent")}' "$STATE"
start_volvisord_bin "$D1_BIN" "$D1_RESTORE" "$CFG" "$PORT" || exit 1
RESTORE_T1="$(date +%s.%N)"
awk -v a="$RESTORE_T0" -v b="$RESTORE_T1" 'BEGIN { printf "restore_to_health_duration_secs=%.3f\n", b-a }' | tee -a "$LOG"

say "post-restore assertions"
curl --fail-with-body --silent --show-error --config "$CURL" "$BASE/v2/volumes/vol-d1-a" >"$RUN/records/$TAG-inspect-a-after.json"
curl --fail-with-body --silent --show-error --config "$CURL" "$BASE/v2/volumes/vol-d1-b" >"$RUN/records/$TAG-inspect-b-after.json"
tee -a "$LOG" < "$RUN/records/$TAG-inspect-a-after.json"
tee -a "$LOG" < "$RUN/records/$TAG-inspect-b-after.json"
A_STATE_AFTER="$(json_get "$RUN/records/$TAG-inspect-a-after.json" '.attachments[0].state')"
A_REV_AFTER="$(json_get "$RUN/records/$TAG-inspect-a-after.json" '.attachments[0].state_revision')"
A_EVIDENCE_AFTER="$(json_get "$RUN/records/$TAG-inspect-a-after.json" '.attachments[0].evidence')"
assert_eq "$A_STATE_AFTER" "active" "post-restore attachment state"
assert_eq "$A_REV_AFTER" "2" "post-restore attachment state_revision"
assert_eq "$A_EVIDENCE_AFTER" "guest_confirmed" "post-restore attachment evidence"
B_GENERATION_AFTER="$(json_get "$STATE" '.volumes["vol-d1-b"].entry.generation')"
assert_eq "$B_GENERATION_AFTER" "3" "post-restore retired volume generation"
B_FILTER_AFTER="$(json_get "$STATE" '.volumes["vol-d1-b"].runtime.retired_attachment_filter // "absent"')"
assert_eq "$B_FILTER_AFTER" "absent" "new writes do not populate the legacy tombstone filter"

say "prove the generation fence: reusing att-d1-b is a new instance"
api_expect "$CURL" POST "$BASE/v2/volumes/vol-d1-b/attach" "$RUN/records/$TAG-reattach-b.json" 200 "$RUN/records/$TAG-reattach-b-after.json"
B_REATTACH_GEN="$(json_get "$RUN/records/$TAG-reattach-b-after.json" '.attachment_generation')"
printf 'reattached att-d1-b generation=%s\n' "$B_REATTACH_GEN" | tee -a "$LOG"
assert_eq "$B_REATTACH_GEN" "3" "re-attach records the consumed volume generation"

cat >"$RUN/records/$TAG-delayed-report-b.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d1-delayed-report-b","attachment_id":"att-d1-b","expected_attachment_generation":1,"state":"active","evidence":"guest_confirmed","reporter":"day2-drill-guest"}
EOF
say "prove a delayed report from the retired instance is fenced"
api_expect "$CURL" POST "$BASE/v2/volumes/vol-d1-b/attachment-state" "$RUN/records/$TAG-delayed-report-b.json" 409 "$RUN/records/$TAG-delayed-report-b.resp.json"
assert_contains "$(cat "$RUN/records/$TAG-delayed-report-b.resp.json")" "STALE_GENERATION" "delayed retired-generation report code"

say "prove the post-backup volume C is not in restored state (expected RPO loss)"
api "$CURL" GET "$BASE/v2/volumes/vol-d1-c" >"$RUN/records/$TAG-inspect-c-after.json"
C_STATUS="$(cat "$RUN/records/last.status")"
tee -a "$LOG" < "$RUN/records/$TAG-inspect-c-after.json"
printf 'HTTP %s for post-backup volume after restore\n' "$C_STATUS" | tee -a "$LOG"
assert_eq "$C_STATUS" "404" "post-backup volume absent after restore"

say "show the orphaned backend LV left by the discarded post-backup operation"
cap "backend LVs after restore (operator reconciliation required for C)" lvs --noheadings -o lv_name,vg_name,lv_size "vg-d246a"

stop_or_exit "$D1_RESTORE"
cleanup
if end_drill; then
  printf 'D1 COMPLETE\n' | tee -a "$LOG"
else
  printf 'D1 FAILED\n' | tee -a "$LOG"
  exit 1
fi
