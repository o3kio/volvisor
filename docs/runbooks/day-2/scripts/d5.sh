#!/usr/bin/env bash
# D5: ENOSPC on the journal filesystem and on the LVM state filesystem.
#
# A: journal dir on a 256 KiB tmpfs, filled to ENOSPC after a baseline
#    volume. A new create must fail closed (no volume, no partial state),
#    then succeed after space is freed (journal-before-mutate recovery).
# B: LVM provider state on a 256 KiB tmpfs with the journal on ordinary
#    storage, so only the state save hits ENOSPC. The create has already
#    run lvcreate when persist fails: we record exactly what is durable
#    (the old state file) versus what is not (the in-memory record and
#    the orphan LV), then restart and show the divergence honestly.
source "$(dirname "$0")/lib.sh"

TAG="d5"
begin_drill "05-enospc"
trap 'stop_pidfile "$TAG-daemon" TERM >/dev/null 2>&1 || true; stop_pidfile "$TAG-lvm-daemon" TERM >/dev/null 2>&1 || true; umount "$RUN/lab-$TAG/journalfs" 2>/dev/null || true; umount "$RUN/lab-$TAG/statefs" 2>/dev/null || true; lvm_lab_teardown "$TAG-lvm" >/dev/null 2>&1 || true' EXIT

# fill_fs <dir> <name>: write chunks (shrinking from 4 KiB to 1 byte)
# until the filesystem refuses, leaving effectively zero free bytes.
fill_fs() {
  local dir="$1" name="$2"
  python3 - "$dir/$name" <<'PY'
import sys
path = sys.argv[1]
written = 0
chunk = 4096
with open(path, "wb", buffering=0) as f:
    while chunk:
        try:
            f.write(b"\0" * chunk)
        except OSError as err:
            if chunk == 1:
                print(f"filled={written} stop={err.strerror}")
                break
            chunk //= 2
        else:
            written += chunk
PY
}

# ---------------------------------------------------------------- part A
say "A: journal filesystem ENOSPC (fake provider)"
JFS="$RUN/lab-$TAG/journalfs"
JOURNAL="$JFS/journal"
mkdir -p "$JFS" "$JOURNAL"
mount -t tmpfs -o size=256k tmpfs "$JFS"
cp /etc/mtab "$RUN/records/$TAG-journalfs.mtab"
PORT="$(pick_port)"
TOKEN="d5-disposable-journal-admin-$TAG"
write_header_config "$RUN/configs/$TAG-journal.curl" "$TOKEN"
CURL="$RUN/configs/$TAG-journal.curl"
BASE="http://127.0.0.1:$PORT"
CFG="$RUN/configs/$TAG-journal.toml"
cat >"$CFG" <<EOF
listen = "127.0.0.1:$PORT"
journal_dir = "$JOURNAL"
provider = "fake"
admin_token = "$TOKEN"
EOF

cat >"$RUN/records/$TAG-create-baseline.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d5-create-baseline","project_id":"day2","volume_id":"vol-d5-baseline","class":"native-local","size_bytes":67108864}
EOF
cat >"$RUN/records/$TAG-create-full.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d5-create-full","project_id":"day2","volume_id":"vol-d5-full","class":"native-local","size_bytes":67108864}
EOF

start_volvisord "$TAG-daemon" "$CFG" "$PORT" || exit 1
cap "create the baseline volume before filling the journal filesystem" \
  api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-baseline.json" 200 "$RUN/records/$TAG-create-baseline.resp.json"
cap "filesystem usage before the fill" df -k "$JFS"

# tmpfs accounts whole 4 KiB pages: an append can still fit in the slack
# of the journal file's last page after every other page is full. Pad
# the journal with real (journaled) creates until less than one frame's
# worth of slack remains, so the next intent needs a new page.
PAD=0
while :; do
  JSIZE="$(stat -c %s "$JOURNAL/journal.log")"
  SLACK=$((4096 - (JSIZE % 4096)))
  printf 'journal size=%s last-page slack=%s bytes\n' "$JSIZE" "$SLACK" | tee -a "$LOG"
  if [[ "$SLACK" -lt 500 ]]; then break; fi
  PAD=$((PAD + 1))
  if [[ "$PAD" -gt 24 ]]; then
    printf 'could not reach a small last-page slack after %s padding creates\n' "$PAD" | tee -a "$LOG"
    exit 1
  fi
  cat >"$RUN/records/$TAG-pad-$PAD.json" <<EOF
{"api_version":"volvisor.volume.v2","operation_id":"op-d5-pad-$PAD","project_id":"day2","volume_id":"vol-d5-pad-$PAD","class":"native-local","size_bytes":67108864}
EOF
  api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-pad-$PAD.json" 200 "$RUN/records/$TAG-pad-$PAD.resp.json"
done

cap "fill the 256 KiB journal filesystem to ENOSPC" fill_fs "$JFS" filler.bin
cap "filesystem usage at ENOSPC" df -k "$JFS"
JOURNAL_SHA_BEFORE="$(sha256sum "$JOURNAL/journal.log" | cut -d' ' -f1)"
JOURNAL_SIZE_BEFORE="$(stat -c %s "$JOURNAL/journal.log")"
printf 'journal before the failed append: size=%s sha=%s\n' "$JOURNAL_SIZE_BEFORE" "$JOURNAL_SHA_BEFORE" | tee -a "$LOG"

cap "a create whose intent append hits ENOSPC is refused" \
  api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-full.json" 500 "$RUN/records/$TAG-create-full.resp.json"
cat "$RUN/records/$TAG-create-full.resp.json" | tee -a "$LOG"
assert_contains "$(cat "$RUN/records/$TAG-create-full.resp.json")" "No space left on device" "journal ENOSPC detail"
cap "the refused volume is absent (journal-before-mutate held)" \
  api_expect "$CURL" GET "$BASE/v2/volumes/vol-d5-full" "" 404 "$RUN/records/$TAG-full-absent.json"
AFTER_FAIL_SIZE="$(stat -c %s "$JOURNAL/journal.log")"
printf 'journal size immediately after the failed append: %s\n' "$AFTER_FAIL_SIZE" | tee -a "$LOG"

say "stop while the journal filesystem is full, free the filler, restart"
stop_pidfile "$TAG-daemon" TERM
cap "free the filler" rm -f "$JFS/filler.bin"
cap "filesystem usage after freeing the filler" df -k "$JFS"
start_volvisord "$TAG-daemon" "$CFG" "$PORT" || exit 1
assert_eq "$(stat -c %s "$JOURNAL/journal.log")" "$JOURNAL_SIZE_BEFORE" "torn append truncated back to the valid prefix"
assert_eq "$(sha256sum "$JOURNAL/journal.log" | cut -d' ' -f1)" "$JOURNAL_SHA_BEFORE" "valid journal prefix unchanged after ENOSPC recovery"
cap "the retried create succeeds once space is available" \
  api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-full.json" 200 "$RUN/records/$TAG-create-full.retry.json"
assert_eq "$(jq -r '.volume_id' "$RUN/records/$TAG-create-full.retry.json")" "vol-d5-full" "retried volume id"
stop_pidfile "$TAG-daemon" TERM
cap "journal frame map after ENOSPC recovery" python3 - "$JOURNAL/journal.log" <<'PY'
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
PY

# ---------------------------------------------------------------- part B
say "B: LVM provider state filesystem ENOSPC (journal on ordinary storage)"
SFS="$RUN/lab-$TAG/statefs"
mkdir -p "$SFS"
mount -t tmpfs -o size=256k tmpfs "$SFS"
LSTATE="$SFS/lvm-state.json"
LCFG="$RUN/configs/$TAG-lvm.toml"
LJOURNAL="$RUN/lab-$TAG/lvm-journal"
lvm_lab_setup "$TAG-lvm" 2G "$LSTATE" "vg-d5lvm"
LPORT="$(pick_port)"
LTOKEN="d5-disposable-lvm-admin-$TAG"
write_header_config "$RUN/configs/$TAG-lvm.curl" "$LTOKEN"
LCURL="$RUN/configs/$TAG-lvm.curl"
LBASE="http://127.0.0.1:$LPORT"
cat >"$LCFG" <<EOF
listen = "127.0.0.1:$LPORT"
journal_dir = "$LJOURNAL"
provider = "lvm"
lvm_vg_prefix = "vg-d5lvm"
device_claim_token = "d5-disposable-claim-$TAG"
lvm_state_path = "$LSTATE"
admin_token = "$LTOKEN"
EOF

cat >"$RUN/records/$TAG-lvm-baseline.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d5-lvm-baseline","project_id":"day2","volume_id":"vol-d5-lvm-baseline","class":"native-local","size_bytes":67108864}
EOF
cat >"$RUN/records/$TAG-lvm-full.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d5-lvm-full","project_id":"day2","volume_id":"vol-d5-lvm-full","class":"native-local","size_bytes":67108864}
EOF
cat >"$RUN/records/$TAG-lvm-recovered.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d5-lvm-recovered","project_id":"day2","volume_id":"vol-d5-lvm-recovered","class":"native-local","size_bytes":67108864}
EOF

start_volvisord "$TAG-lvm-daemon" "$LCFG" "$LPORT" || exit 1
cap "create the LVM baseline volume" \
  api_expect "$LCURL" POST "$LBASE/v2/volumes" "$RUN/records/$TAG-lvm-baseline.json" 200 "$RUN/records/$TAG-lvm-baseline.resp.json"
cp "$LSTATE" "$RUN/records/$TAG-lvm-state-baseline.json"
STATE_SHA_BEFORE="$(sha256sum "$LSTATE" | cut -d' ' -f1)"
STATE_SIZE_BEFORE="$(stat -c %s "$LSTATE")"
printf 'state before the failed save: size=%s sha=%s\n' "$STATE_SIZE_BEFORE" "$STATE_SHA_BEFORE" | tee -a "$LOG"
cap "LVM baseline LV exists" lvs --noheadings -o lv_name,vg_name,lv_size "vg-d5lvm"
cap "fill the state filesystem to ENOSPC" fill_fs "$SFS" filler.bin
cap "state filesystem usage at ENOSPC" df -k "$SFS"

cap "a create whose provider state save hits ENOSPC returns an error" \
  api_expect "$LCURL" POST "$LBASE/v2/volumes" "$RUN/records/$TAG-lvm-full.json" 500 "$RUN/records/$TAG-lvm-full.resp.json"
cat "$RUN/records/$TAG-lvm-full.resp.json" | tee -a "$LOG"
cap "the state file after the failed save" sha256sum "$LSTATE"
assert_eq "$(sha256sum "$LSTATE" | cut -d' ' -f1)" "$STATE_SHA_BEFORE" "durable state file unchanged by the failed save"
assert_eq "$(stat -c %s "$LSTATE")" "$STATE_SIZE_BEFORE" "durable state file size unchanged by the failed save"
cap "no stale .tmp state file is left behind" ls -la "$SFS"
cap "the LV was nevertheless created by lvcreate (orphan candidate)" lvs --noheadings -o lv_name,vg_name,lv_size "vg-d5lvm"
cap "the same process now reports the volume from in-memory state (not durable)" \
  api_expect "$LCURL" GET "$LBASE/v2/volumes/vol-d5-lvm-full" "" 200 "$RUN/records/$TAG-lvm-full-inmem.json"
jq '{volume_id,state,provisioned_bytes}' "$RUN/records/$TAG-lvm-full-inmem.json" | tee -a "$LOG"

say "stop while the state filesystem is still full, then free space and restart"
stop_pidfile "$TAG-lvm-daemon" TERM
rm -f "$SFS/filler.bin"
cap "state filesystem after freeing the filler" df -k "$SFS"
start_volvisord "$TAG-lvm-daemon" "$LCFG" "$LPORT" || exit 1
cap "after restart the non-durable volume is gone from the API" \
  api_expect "$LCURL" GET "$LBASE/v2/volumes/vol-d5-lvm-full" "" 404 "$RUN/records/$TAG-lvm-full-after.json"
cap "but the orphan LV still exists in LVM" lvs --noheadings -o lv_name,vg_name,lv_size "vg-d5lvm"
cap "the durable baseline volume is still served" \
  api_expect "$LCURL" GET "$LBASE/v2/volumes/vol-d5-lvm-baseline" "" 200 "$RUN/records/$TAG-lvm-baseline-after.json"
jq '{volume_id,state,provisioned_bytes}' "$RUN/records/$TAG-lvm-baseline-after.json" | tee -a "$LOG"
cap "a new create works again after space is restored" \
  api_expect "$LCURL" POST "$LBASE/v2/volumes" "$RUN/records/$TAG-lvm-recovered.json" 200 "$RUN/records/$TAG-lvm-recovered.retry.json"
jq '{volume_id,state}' "$RUN/records/$TAG-lvm-recovered.retry.json" | tee -a "$LOG"
stop_pidfile "$TAG-lvm-daemon" TERM

if end_drill; then
  printf 'D5 COMPLETE\n'
else
  printf 'D5 FAILED\n'
  exit 1
fi
