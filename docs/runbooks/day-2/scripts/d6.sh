#!/usr/bin/env bash
# D6: daemon crash loop (SIGKILL) and single-writer lock contention.
#
# A: an LVM-backed daemon is killed with SIGKILL five times, once while a
#    create request is in flight. After every restart the durable volume
#    and its attachment must still be served and the journal must open
#    with no trailing garbage. In-flight requests may fail (that is what
#    a crash does); the drill records the observed curl result rather
#    than claiming exactly-once completion.
# B: a second daemon pointed at the same journal directory must refuse to
#    start (single-writer flock), and the first daemon must keep serving.
#    The same lock rule is shown for two witness daemons on one state dir.
# shellcheck disable=SC1091 # resolved from the script directory at runtime
source "$(dirname "$0")/lib.sh"

TAG="d6"
begin_drill "06-crash-loop-and-lock"
D6_CLEANED=0
cleanup() {
  [[ "$D6_CLEANED" -eq 1 ]] && return 0
  D6_CLEANED=1
  stop_pidfile "$TAG-daemon" TERM >/dev/null 2>&1 || true
  stop_pidfile "$TAG-witness" TERM >/dev/null 2>&1 || true
  if ! lvm_lab_teardown "$TAG-lvm"; then
    printf 'ASSERT FAIL: lab teardown incomplete (loop/VG may still exist)\n' | tee -a "$LOG"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
  fi
}
trap cleanup EXIT

frames_ok() {
  python3 - "$1" <<'PY'
import struct, sys, zlib
data = open(sys.argv[1], "rb").read()
off = 0
seq = 1
frames = 0
while len(data) - off >= 20:
    magic, frame_seq, length, crc = struct.unpack_from("<IQII", data, off)
    payload = data[off + 20:off + 20 + length]
    if magic != 0x564A4C31 or frame_seq != seq or len(payload) != length:
        break
    if (zlib.crc32(payload) & 0xFFFFFFFF) != crc:
        break
    off += 20 + length
    seq += 1
    frames += 1
print(f"valid_frames={frames} valid_prefix={off} file_size={len(data)}")
sys.exit(0 if off == len(data) else 1)
PY
}

# ---------------------------------------------------------------- part A
STATE="$RUN/lab-$TAG-lvm/journal/lvm-state.json"
JOURNAL="$RUN/lab-$TAG-lvm/journal"
CFG="$RUN/configs/$TAG.toml"
lvm_lab_setup "$TAG-lvm" 2G "$STATE" "vg-d6lvm" || {
  printf 'lab setup failed\n' | tee -a "$LOG"
  exit 1
}
PORT="$(pick_port)"
TOKEN="d6-disposable-admin-$TAG"
write_header_config "$RUN/configs/$TAG.curl" "$TOKEN"
CURL="$RUN/configs/$TAG.curl"
BASE="http://127.0.0.1:$PORT"
cat >"$CFG" <<EOF
listen = "127.0.0.1:$PORT"
journal_dir = "$JOURNAL"
provider = "lvm"
lvm_vg_prefix = "vg-d6lvm"
device_claim_token = "d6-disposable-claim-$TAG"
lvm_state_path = "$STATE"
admin_token = "$TOKEN"
EOF

cat >"$RUN/records/$TAG-create-base.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d6-create-base","project_id":"day2","volume_id":"vol-d6-base","class":"native-local","size_bytes":67108864}
EOF
cat >"$RUN/records/$TAG-attach-base.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d6-attach-base","vm_id":"vm-d6-base","host_id":"host-d6","attachment_id":"att-d6-base","expected_volume_generation":1,"access_mode":"single_writer","requested_frontend":"virtio-blk","vmm_disk_id":"disk0"}
EOF
cat >"$RUN/records/$TAG-report-base.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d6-report-base","attachment_id":"att-d6-base","expected_attachment_generation":1,"state":"active","evidence":"guest_confirmed","reporter":"day2-drill-guest","detail":"drill observation"}
EOF

start_volvisord "$TAG-daemon" "$CFG" "$PORT" || exit 1
cap "create the durable baseline volume" \
  api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-base.json" 200 "$RUN/records/$TAG-create-base.resp.json"
cap "attach it to a VM" \
  api_expect "$CURL" POST "$BASE/v2/volumes/vol-d6-base/attach" "$RUN/records/$TAG-attach-base.json" 200 "$RUN/records/$TAG-attach-base.resp.json"
cap "report the attachment active (durable attachment evidence)" \
  api_expect "$CURL" POST "$BASE/v2/volumes/vol-d6-base/attachment-state" "$RUN/records/$TAG-report-base.json" 200 "$RUN/records/$TAG-report-base.resp.json"
cap "durable state before the crash loop" \
  jq '{state: .volumes["vol-d6-base"].runtime.state, attachment: .volumes["vol-d6-base"].runtime.attachment | {id,state,state_revision,evidence}}' "$STATE"

for i in 1 2 3 4 5; do
  say "crash iteration $i: SIGKILL a daemon with an in-flight create"
  cat >"$RUN/records/$TAG-crash-$i.json" <<EOF
{"api_version":"volvisor.volume.v2","operation_id":"op-d6-crash-$i","project_id":"day2","volume_id":"vol-d6-crash-$i","class":"native-local","size_bytes":67108864}
EOF
  curl --silent --show-error --config "$CURL" --request POST \
    --header 'content-type: application/json' --data-binary @"$RUN/records/$TAG-crash-$i.json" \
    --write-out '%{http_code}' --output "$RUN/records/$TAG-crash-$i.resp.json" \
    "$BASE/v2/volumes" >"$RUN/records/$TAG-crash-$i.status" 2>"$RUN/records/$TAG-crash-$i.err" &
  CURL_PID=$!
  sleep 0.02
  DPID="$(cat "$RUN/records/$TAG-daemon.pid")"
  kill -9 "$DPID" 2>/dev/null || true
  wait "$CURL_PID"
  CURL_RC=$?
  sleep 0.2
  printf 'iteration=%s killed_pid=%s in_flight_curl_exit=%s status=%s stderr=%s\n' \
    "$i" "$DPID" "$CURL_RC" "$(cat "$RUN/records/$TAG-crash-$i.status" 2>/dev/null || echo none)" \
    "$(tr '\n' ' ' <"$RUN/records/$TAG-crash-$i.err" 2>/dev/null)" | tee -a "$LOG"
  rm -f "$RUN/records/$TAG-daemon.pid"

  start_volvisord "$TAG-daemon" "$CFG" "$PORT" || exit 1
  cap "restart $i: journal opens with no trailing garbage" frames_ok "$JOURNAL/journal.log"
  cap "restart $i: durable volume is still served" \
    api_expect "$CURL" GET "$BASE/v2/volumes/vol-d6-base" "" 200 "$RUN/records/$TAG-base-after-$i.json"
  assert_eq "$(jq -r '.attachments[0].state' "$RUN/records/$TAG-base-after-$i.json")" "active" "attachment state after crash $i"
  assert_eq "$(jq -r '.attachments[0].state_revision' "$RUN/records/$TAG-base-after-$i.json")" "2" "attachment state_revision after crash $i"
  assert_eq "$(jq -r '.attachments[0].evidence' "$RUN/records/$TAG-base-after-$i.json")" "guest_confirmed" "attachment evidence after crash $i"
done

cap "the durable attachment record after five SIGKILLs" \
  jq '.volumes["vol-d6-base"].runtime.attachment | {id,state,state_revision,evidence}' "$STATE"

# ---------------------------------------------------------------- part B
say "B1: a second storage daemon on the same journal directory must refuse"
SPORT="$(pick_port)"
SCFG="$RUN/configs/$TAG-second.toml"
cat >"$SCFG" <<EOF
listen = "127.0.0.1:$SPORT"
journal_dir = "$JOURNAL"
provider = "lvm"
lvm_vg_prefix = "vg-d6lvm"
device_claim_token = "d6-disposable-claim-$TAG"
lvm_state_path = "$STATE"
admin_token = "$TOKEN"
EOF
timeout 10s "$BIN/volvisord" --config "$SCFG" >"$RUN/raw/$TAG-second.daemon.log" 2>&1
SECOND_RC=$?
printf 'second-daemon startup exit=%s\n' "$SECOND_RC" | tee -a "$LOG"
cap "second daemon stderr" cat "$RUN/raw/$TAG-second.daemon.log"
assert_eq "$SECOND_RC" "1" "second daemon startup exit"
assert_contains "$(cat "$RUN/raw/$TAG-second.daemon.log")" "already locked by another writer" "typed journal lock refusal"
cap "the first daemon still serves after the refused second start" \
  api_expect "$CURL" GET "$BASE/v2/volumes/vol-d6-base" "" 200 "$RUN/records/$TAG-base-after-lock.json"

say "B2: two witness daemons on one state directory obey the same lock"
WSTATE="$RUN/lab-$TAG/witness-state"
WCFG="$RUN/configs/$TAG-witness.toml"
WCFG2="$RUN/configs/$TAG-witness-second.toml"
mkdir -p "$WSTATE"
WPORT="$(pick_port)"
WPORT2="$(pick_port)"
cat >"$WCFG" <<EOF
listen = "127.0.0.1:$WPORT"
state_dir = "$WSTATE"
auth_token = "d6-disposable-witness-admin-$TAG"
[host_tokens]
host-a = "d6-disposable-witness-a-$TAG"
host-b = "d6-disposable-witness-b-$TAG"
EOF
sed "s/^listen = .*/listen = \"127.0.0.1:$WPORT2\"/" "$WCFG" >"$WCFG2"
"$BIN/volvisor-witnessd" --config "$WCFG" >"$RUN/raw/$TAG-witness.daemon.log" 2>&1 &
echo "$!" >"$RUN/records/$TAG-witness.pid"
for ((i = 0; i < 300; i++)); do
  curl -fsS --max-time 1 "http://127.0.0.1:$WPORT/healthz" >/dev/null 2>&1 && break
  sleep 0.1
done
cap "the first witness is healthy" curl -fsS "http://127.0.0.1:$WPORT/healthz"
timeout 10s "$BIN/volvisor-witnessd" --config "$WCFG2" >"$RUN/raw/$TAG-witness-second.log" 2>&1
WSECOND_RC=$?
printf 'second-witness startup exit=%s\n' "$WSECOND_RC" | tee -a "$LOG"
cap "second witness stderr" cat "$RUN/raw/$TAG-witness-second.log"
assert_eq "$WSECOND_RC" "1" "second witness startup exit"
assert_contains "$(cat "$RUN/raw/$TAG-witness-second.log")" "already locked by another writer" "typed witness lock refusal"
cap "the first witness is still healthy after the refused second start" curl -fsS "http://127.0.0.1:$WPORT/healthz"

stop_pidfile "$TAG-witness" TERM
stop_pidfile "$TAG-daemon" TERM
cleanup

if end_drill; then
  printf 'D6 COMPLETE\n'
else
  printf 'D6 FAILED\n'
  exit 1
fi
