#!/usr/bin/env bash
# D4: torn / corrupt journal tail and corrupt durable state file.
#
# Part A (fake provider, real journal):
#   A1 append garbage after the last frame -> restart truncates the torn
#      tail, health is restored, the journal is byte-identical to the
#      pre-garbage prefix and the idempotency registry replays;
#   A2 cut the last frame in half -> restart drops only that frame, keeps
#      the valid prefix, and appends continue from the recovered sequence;
#   A3 append a complete, CRC-valid frame declaring record_version 2 ->
#      restart refuses (typed unsupported-version), the log is NOT
#      truncated (durable state from a newer build is never discarded).
#
# Part B (loop-device LVM provider): corrupt lvm-state.json -> startup
# refuses with a typed parse error and the file is left byte-identical.
source "$(dirname "$0")/lib.sh"

TAG="d4"
begin_drill "04-torn-journal-and-corrupt-state"
trap 'stop_pidfile "$TAG-daemon" TERM >/dev/null 2>&1 || true; lvm_lab_teardown "$TAG-lvm" >/dev/null 2>&1 || true' EXIT

FRAMES="$RUN/records/$TAG-frames.py"
cat >"$FRAMES" <<'PY'
import json, struct, sys, zlib
path = sys.argv[1]
data = open(path, "rb").read()
off = 0
seq = 1
rows = []
while len(data) - off >= 20:
    magic, frame_seq, length, crc = struct.unpack_from("<IQII", data, off)
    if magic != 0x564A4C31 or frame_seq != seq:
        break
    payload = data[off + 20:off + 20 + length]
    if len(payload) != length or (zlib.crc32(payload) & 0xFFFFFFFF) != crc:
        break
    end = off + 20 + length
    rows.append({"start": off, "end": end, "sequence": frame_seq, "payload_len": length})
    off = end
    seq += 1
print(json.dumps({"frames": rows, "valid_prefix": off, "file_size": len(data)}))
PY
frames() { python3 "$FRAMES" "$1"; }

# ---------------------------------------------------------------- part A
JOURNAL="$RUN/lab-$TAG/journal"
CFG="$RUN/configs/$TAG.toml"
mkdir -p "$JOURNAL"
PORT="$(pick_port)"
TOKEN="d4-disposable-admin-$TAG"
write_header_config "$RUN/configs/$TAG.curl" "$TOKEN"
CURL="$RUN/configs/$TAG.curl"
BASE="http://127.0.0.1:$PORT"
cat >"$CFG" <<EOF
listen = "127.0.0.1:$PORT"
journal_dir = "$JOURNAL"
provider = "fake"
admin_token = "$TOKEN"
EOF

cat >"$RUN/records/$TAG-create-keep.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d4-create-keep","project_id":"day2","volume_id":"vol-d4-keep","class":"native-local","size_bytes":67108864}
EOF
cat >"$RUN/records/$TAG-create-tail.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d4-create-tail","project_id":"day2","volume_id":"vol-d4-tail","class":"native-local","size_bytes":67108864}
EOF
cat >"$RUN/records/$TAG-create-new.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d4-create-new","project_id":"day2","volume_id":"vol-d4-new","class":"native-local","size_bytes":67108864}
EOF

start_volvisord "$TAG-daemon" "$CFG" "$PORT" || exit 1
cap "create a first volume whose intent/outcome must survive" \
  api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-keep.json" 200 "$RUN/records/$TAG-create-keep.resp.json"
cap "create a second volume so the last frame belongs to it" \
  api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-tail.json" 200 "$RUN/records/$TAG-create-tail.resp.json"
stop_pidfile "$TAG-daemon" TERM

LOGJ="$JOURNAL/journal.log"
cp "$LOGJ" "$RUN/records/$TAG-journal-baseline.log"
BASELINE_SIZE="$(stat -c %s "$LOGJ")"
BASELINE_SHA="$(sha256sum "$LOGJ" | cut -d' ' -f1)"
cap "frame map of the clean journal" frames "$LOGJ"
cap "clean journal size/checksum" sha256sum "$LOGJ"
printf 'baseline_size=%s baseline_sha=%s\n' "$BASELINE_SIZE" "$BASELINE_SHA" | tee -a "$LOG"

say "A1: append garbage after the last frame (simulated torn append)"
python3 - "$LOGJ" <<'PY'
import sys
with open(sys.argv[1], "ab") as f:
    f.write(b"\xa5" * 64)
PY
cap "journal size after appending 64 garbage bytes" stat -c '%s %n' "$LOGJ"
start_volvisord "$TAG-daemon" "$CFG" "$PORT" || exit 1
cap "health after garbage-tail restart" curl -fsS "$BASE/healthz"
assert_eq "$(stat -c %s "$LOGJ")" "$BASELINE_SIZE" "torn garbage tail truncated back to the valid prefix"
assert_eq "$(sha256sum "$LOGJ" | cut -d' ' -f1)" "$BASELINE_SHA" "torn garbage tail left the valid prefix byte-identical"
cap "idempotent replay of the first create after garbage-tail recovery" \
  api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-keep.json" 200 "$RUN/records/$TAG-create-keep.replay.json"
assert_eq "$(jq -r '.volume_id' "$RUN/records/$TAG-create-keep.replay.json")" "vol-d4-keep" "replayed volume id"
stop_pidfile "$TAG-daemon" TERM

say "A2: cut the last frame in half (simulated power loss mid-append)"
FRAME_JSON="$(frames "$LOGJ")"
LAST_START="$(printf '%s' "$FRAME_JSON" | jq -r '.frames[-1].start')"
PREFIX_SIZE="$(printf '%s' "$FRAME_JSON" | jq -r '.frames[-1].start')"
printf 'last frame starts at %s; truncating to start+8\n' "$LAST_START" | tee -a "$LOG"
truncate -s "$((LAST_START + 8))" "$LOGJ"
cap "journal size after cutting the last frame" stat -c '%s %n' "$LOGJ"
start_volvisord "$TAG-daemon" "$CFG" "$PORT" || exit 1
assert_eq "$(stat -c %s "$LOGJ")" "$PREFIX_SIZE" "half-written last frame truncated away"
assert_eq "$(sha256sum "$LOGJ" | cut -d' ' -f1)" "$(head -c "$PREFIX_SIZE" "$RUN/records/$TAG-journal-baseline.log" | sha256sum | cut -d' ' -f1)" "recovered prefix matches the baseline prefix"
cap "the recovered journal still replays the earlier operation" \
  api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-keep.json" 200 "$RUN/records/$TAG-create-keep.replay2.json"
cap "a new append succeeds from the recovered sequence" \
  api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-new.json" 200 "$RUN/records/$TAG-create-new.resp.json"
cap "frame map after appending to the recovered journal" frames "$LOGJ"
stop_pidfile "$TAG-daemon" TERM

say "A3: append a complete CRC-valid frame that declares record_version 2"
cp "$LOGJ" "$RUN/records/$TAG-journal-before-future.log"
BEFORE_FUTURE_SHA="$(sha256sum "$LOGJ" | cut -d' ' -f1)"
BEFORE_FUTURE_SIZE="$(stat -c %s "$LOGJ")"
python3 - "$LOGJ" <<'PY'
import json, struct, sys, zlib
path = sys.argv[1]
data = open(path, "rb").read()
off = 0
seq = 1
while len(data) - off >= 20:
    magic, frame_seq, length, crc = struct.unpack_from("<IQII", data, off)
    if magic != 0x564A4C31 or frame_seq != seq:
        break
    payload = data[off + 20:off + 20 + length]
    if len(payload) != length or (zlib.crc32(payload) & 0xFFFFFFFF) != crc:
        break
    off += 20 + length
    seq += 1
payload = json.dumps({
    "record_version": 2,
    "record": {"record_type": "checkpoint", "note": "written by a future build"},
}).encode()
frame = struct.pack("<IQII", 0x564A4C31, seq, len(payload), zlib.crc32(payload) & 0xFFFFFFFF) + payload
with open(path, "ab") as f:
    f.write(frame)
print(f"appended future-version frame sequence={seq} payload_len={len(payload)}")
PY
cap "future-version journal checksum and size before startup" sha256sum "$LOGJ"
FUTURE_SIZE="$(stat -c %s "$LOGJ")"
printf 'future_journal_size=%s\n' "$FUTURE_SIZE" | tee -a "$LOG"
timeout 10s "$BIN/volvisord" --config "$CFG" >"$RUN/raw/$TAG-future-journal.daemon.log" 2>&1
FUTURE_RC=$?
printf 'future-journal startup exit=%s\n' "$FUTURE_RC" | tee -a "$LOG"
cap "future journal refusal stderr" cat "$RUN/raw/$TAG-future-journal.daemon.log"
assert_eq "$FUTURE_RC" "1" "future journal record startup exit"
assert_contains "$(cat "$RUN/raw/$TAG-future-journal.daemon.log")" "unsupported durable-state schema version" "typed future journal refusal"
assert_eq "$(stat -c %s "$LOGJ")" "$FUTURE_SIZE" "future frame was not truncated"
assert_eq "$(head -c "$BEFORE_FUTURE_SIZE" "$LOGJ" | sha256sum | cut -d' ' -f1)" "$BEFORE_FUTURE_SHA" "valid prefix before the future frame is untouched"
cp "$RUN/records/$TAG-journal-before-future.log" "$LOGJ"

# ---------------------------------------------------------------- part B
say "B: corrupt the LVM durable state file and prove startup refuses without rewriting it"
LSTATE="$RUN/lab-$TAG-lvm/journal/lvm-state.json"
LCFG="$RUN/configs/$TAG-lvm.toml"
lvm_lab_setup "$TAG-lvm" 2G "$LSTATE" "vg-d4lvm"
# Make the seeded file syntactically corrupt (truncated JSON, a realistic
# partial write) and prove the daemon refuses rather than guessing.
printf '{"schema_version":1,"volumes":{"vol-partial":' >"$LSTATE"
CORRUPT_SHA="$(sha256sum "$LSTATE" | cut -d' ' -f1)"
LPORT="$(pick_port)"
LTOKEN="d4-disposable-lvm-admin-$TAG"
write_header_config "$RUN/configs/$TAG-lvm.curl" "$LTOKEN"
cat >"$LCFG" <<EOF
listen = "127.0.0.1:$LPORT"
journal_dir = "$RUN/lab-$TAG-lvm/journal"
provider = "lvm"
lvm_vg_prefix = "vg-d4lvm"
device_claim_token = "d4-disposable-claim-$TAG"
lvm_state_path = "$LSTATE"
admin_token = "$LTOKEN"
EOF
cap "corrupt state file contents" cat "$LSTATE"
timeout 10s "$BIN/volvisord" --config "$LCFG" >"$RUN/raw/$TAG-corrupt-state.daemon.log" 2>&1
CORRUPT_RC=$?
printf 'corrupt-state startup exit=%s\n' "$CORRUPT_RC" | tee -a "$LOG"
cap "corrupt state refusal stderr" cat "$RUN/raw/$TAG-corrupt-state.daemon.log"
assert_eq "$CORRUPT_RC" "1" "corrupt state startup exit"
assert_contains "$(cat "$RUN/raw/$TAG-corrupt-state.daemon.log")" "lvm provider construction failed" "typed corrupt-state refusal"
assert_eq "$(sha256sum "$LSTATE" | cut -d' ' -f1)" "$CORRUPT_SHA" "corrupt state file untouched (no silent rewrite)"

if end_drill; then
  printf 'D4 COMPLETE\n'
else
  printf 'D4 FAILED\n'
  exit 1
fi
