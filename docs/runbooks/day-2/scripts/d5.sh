#!/usr/bin/env bash
# D5: ENOSPC on the journal filesystem and on the LVM state filesystem.
#
# A: journal dir on a 256 KiB tmpfs, filled to ENOSPC after a baseline
#    volume. A new create must fail closed (no volume, no partial state),
#    then succeed after space is freed (journal-before-mutate recovery).
# B: LVM provider state on a 256 KiB tmpfs with the journal on ordinary
#    storage, so only the state save hits ENOSPC after lvcreate. Post-#58
#    the failure is compensated: 500 INTERNAL carrying the original save
#    error (state .tmp path and "No space left on device"), the old state
#    file byte-identical, the just-created LV removed, the in-memory entry
#    rolled back, and no .tmp residue. The same process reports the volume
#    404; after a restart it is still 404, a replay of the same
#    operation_id stays the recorded 500 (terminal failure), a fresh
#    operation_id succeeds exactly once, and state/LVM agree throughout.
# shellcheck disable=SC1091 # resolved from the script directory at runtime
source "$(dirname "$0")/lib.sh"

TAG="d5"
begin_drill "05-enospc"
D5_CLEANED=0
cleanup() {
  [[ "$D5_CLEANED" -eq 1 ]] && return 0
  D5_CLEANED=1
  stop_pidfile "$TAG-daemon" TERM >/dev/null 2>&1 || {
    stop_pidfile "$TAG-daemon" KILL >/dev/null 2>&1 || true
  }
  stop_pidfile "$TAG-lvm-daemon" TERM >/dev/null 2>&1 || {
    stop_pidfile "$TAG-lvm-daemon" KILL >/dev/null 2>&1 || true
  }
  local fs
  for fs in "$RUN/lab-$TAG/journalfs" "$RUN/lab-$TAG/statefs"; do
    umount "$fs" 2>/dev/null || umount -l "$fs" 2>/dev/null || true
    if mountpoint -q "$fs"; then
      printf 'ASSERT FAIL: %s is still mounted after teardown\n' "$fs" | tee -a "$LOG"
      ASSERT_FAILS=$((ASSERT_FAILS + 1))
    fi
  done
  if ! lvm_lab_teardown "$TAG-lvm"; then
    printf 'ASSERT FAIL: lab teardown incomplete (loop/VG may still exist)\n' | tee -a "$LOG"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
  fi
}
trap cleanup EXIT

# fill_fs <dir> <name>: write chunks (shrinking from 4 KiB to 1 byte)
# until the filesystem refuses, leaving effectively zero free bytes. The
# byte cap is a safety rail: the tmpfs mount is verified first, so this
# cannot trigger on a 256 KiB filesystem, but if that check is ever
# bypassed the helper fails instead of filling the host filesystem.
fill_fs() {
  local dir="$1" name="$2" limit=524288
  python3 - "$dir/$name" "$limit" <<'PY'
import sys
path = sys.argv[1]
limit = int(sys.argv[2])
written = 0
chunk = 4096
with open(path, "wb", buffering=0) as f:
    while chunk:
        if written + chunk > limit:
            print(f"fill cap {limit} reached at {written} bytes; refusing to continue")
            sys.exit(1)
        try:
            n = f.write(b"\0" * chunk)
        except OSError as err:
            if chunk == 1:
                print(f"filled={written} stop={err.strerror}")
                break
            chunk //= 2
        else:
            written += n
PY
}

# mount_tmpfs_256k <dir>: mount a size-capped tmpfs and prove the mount
# actually took effect. Without the fstype check a failed mount would make
# fill_fs write into the host filesystem instead of a disposable tmpfs.
mount_tmpfs_256k() {
  local dir="$1" fstype
  mkdir -p "$dir"
  if ! mount -t tmpfs -o size=256k tmpfs "$dir" >>"$LOG" 2>&1; then
    printf 'FAIL: could not mount a 256 KiB tmpfs on %s; refusing to fill it\n' "$dir" | tee -a "$LOG"
    return 1
  fi
  fstype="$(stat -f -c %T "$dir")"
  if [[ "$fstype" != "tmpfs" ]]; then
    printf 'FAIL: %s is %s, not tmpfs; refusing to fill it\n' "$dir" "$fstype" | tee -a "$LOG"
    umount "$dir" 2>/dev/null || true
    return 1
  fi
  printf 'mounted tmpfs size=256k on %s\n' "$dir" | tee -a "$LOG"
}

# stop_daemon <name>: stop a drill daemon and fail the drill if TERM does
# not take effect. A still-live daemon would make the pre-restart snapshot
# and the restart evidence meaningless. Escalate to KILL so a TERM-ignoring
# daemon cannot keep the tmpfs busy and leak the lab on the failure path.
stop_daemon() {
  local name="$1"
  if ! stop_pidfile "$name" TERM; then
    printf 'WARNING: %s ignored TERM; escalating to KILL to release the lab\n' "$name" | tee -a "$LOG"
    stop_pidfile "$name" KILL || true
    printf 'FAIL: %s did not stop on TERM\n' "$name" | tee -a "$LOG"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
    cleanup
    end_drill || true
    exit 1
  fi
}

# fail_drill <message>: record a setup failure that is not a `cap` command,
# close the drill with the assertion summary and exit nonzero.
fail_drill() {
  printf 'FAIL: %s\n' "$1" | tee -a "$LOG"
  ASSERT_FAILS=$((ASSERT_FAILS + 1))
  # Teardown can add failures; run it before the summary so the printed count
  # includes them. The EXIT trap then re-enters cleanup, which is idempotent.
  cleanup
  end_drill || true
  exit 1
}

# free_kib <dir>: available KiB on the filesystem holding <dir>.
free_kib() {
  df -k "$1" | awk 'NR==2 {print $4}'
}

# assert_not_contains <haystack> <needle> <what>: inverse of assert_contains,
# with the same PASS/FAIL accounting.
assert_not_contains() {
  local haystack="$1" needle="$2" what="$3"
  if [[ "$haystack" != *"$needle"* ]]; then
    printf 'ASSERT PASS: %s does not contain %q\n' "$what" "$needle" | tee -a "$LOG"
  else
    printf 'ASSERT FAIL: %s contains %q\n' "$what" "$needle" | tee -a "$LOG"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
    return 1
  fi
}

# lv_name_for <volume-id>: mirror the provider's
# vol-<sanitized-id>-<8 hex chars of sha256(full id)> naming, so the drill
# derives the LV name instead of hardcoding a hash.
lv_name_for() {
  local id="$1" sanitized hash
  sanitized="${id//./-}"
  sanitized="${sanitized//:/-}"
  sanitized="${sanitized:0:100}"
  hash="$(printf '%s' "$id" | sha256sum | cut -c1-8)"
  printf 'vol-%s-%s' "$sanitized" "$hash"
}

# lv_names <vg>: trimmed LV names, one per line.
lv_names() {
  lvs --noheadings -o lv_name "$1" |
    sed '/^[[:space:]]*$/d; s/^[[:space:]]*//; s/[[:space:]]*$//'
}

# lv_count <vg>: number of LVs, with the count trimmed.
lv_count() {
  lv_names "$1" | wc -l | tr -d '[:space:]'
}

# ---------------------------------------------------------------- part A
say "A: journal filesystem ENOSPC (fake provider)"
JFS="$RUN/lab-$TAG/journalfs"
JOURNAL="$JFS/journal"
mkdir -p "$JFS" "$JOURNAL"
mount_tmpfs_256k "$JFS" || fail_drill "journal tmpfs is not usable"
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

start_volvisord "$TAG-daemon" "$CFG" "$PORT" || fail_drill "$TAG-daemon did not become healthy"
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
    fail_drill "could not reach a small last-page slack after $PAD padding creates"
  fi
  cat >"$RUN/records/$TAG-pad-$PAD.json" <<EOF
{"api_version":"volvisor.volume.v2","operation_id":"op-d5-pad-$PAD","project_id":"day2","volume_id":"vol-d5-pad-$PAD","class":"native-local","size_bytes":67108864}
EOF
  api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-pad-$PAD.json" 200 "$RUN/records/$TAG-pad-$PAD.resp.json"
done

cap "fill the 256 KiB journal filesystem to ENOSPC" fill_fs "$JFS" filler.bin
cap "filesystem usage at ENOSPC" df -k "$JFS"
assert_eq "$(free_kib "$JFS")" "0" "journal filesystem has zero KiB available at ENOSPC"
JOURNAL_SHA_BEFORE="$(sha256sum "$JOURNAL/journal.log" | cut -d' ' -f1)"
JOURNAL_SIZE_BEFORE="$(stat -c %s "$JOURNAL/journal.log")"
printf 'journal before the failed append: size=%s sha=%s\n' "$JOURNAL_SIZE_BEFORE" "$JOURNAL_SHA_BEFORE" | tee -a "$LOG"

cap "a create whose intent append hits ENOSPC is refused" \
  api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-full.json" 500 "$RUN/records/$TAG-create-full.resp.json"
tee -a "$LOG" < "$RUN/records/$TAG-create-full.resp.json"
printf '\n' | tee -a "$LOG"
assert_eq "$(jq -r '.code' "$RUN/records/$TAG-create-full.resp.json")" "INTERNAL" "journal ENOSPC code"
assert_contains "$(cat "$RUN/records/$TAG-create-full.resp.json")" "No space left on device" "journal ENOSPC detail"
cap "the refused volume is absent (journal-before-mutate held)" \
  api_expect "$CURL" GET "$BASE/v2/volumes/vol-d5-full" "" 404 "$RUN/records/$TAG-full-absent.json"
AFTER_FAIL_SIZE="$(stat -c %s "$JOURNAL/journal.log")"
printf 'journal size immediately after the failed append: %s\n' "$AFTER_FAIL_SIZE" | tee -a "$LOG"
# Prove the failed append actually reached the filesystem before ENOSPC.
# Without this, the later truncate-back check would pass vacuously if the
# journal layer ever refused before writing any bytes.
if [[ -n "$JOURNAL_SIZE_BEFORE" ]] && (( AFTER_FAIL_SIZE > JOURNAL_SIZE_BEFORE )); then
  printf 'ASSERT PASS: the failed append grew the journal past the valid prefix before ENOSPC\n' | tee -a "$LOG"
else
  printf 'ASSERT FAIL: the failed append did not grow the journal (before=%s after=%s)\n' "$JOURNAL_SIZE_BEFORE" "$AFTER_FAIL_SIZE" | tee -a "$LOG"
  ASSERT_FAILS=$((ASSERT_FAILS + 1))
fi

say "stop while the journal filesystem is full, free the filler, restart"
stop_daemon "$TAG-daemon"
# The harness opens each daemon log with `>`; snapshot this incarnation's
# log before the restart truncates it.
cap "snapshot the first daemon log before restart" cp "$RUN/raw/$TAG-daemon.daemon.log" "$RUN/raw/$TAG-daemon.pre-restart.log"
cap "free the filler" rm -f "$JFS/filler.bin"
cap "filesystem usage after freeing the filler" df -k "$JFS"
start_volvisord "$TAG-daemon" "$CFG" "$PORT" || fail_drill "$TAG-daemon did not become healthy"
assert_eq "$(stat -c %s "$JOURNAL/journal.log")" "$JOURNAL_SIZE_BEFORE" "torn append truncated back to the valid prefix"
assert_eq "$(sha256sum "$JOURNAL/journal.log" | cut -d' ' -f1)" "$JOURNAL_SHA_BEFORE" "valid journal prefix unchanged after ENOSPC recovery"
cap "the retried create succeeds once space is available" \
  api_expect "$CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-create-full.json" 200 "$RUN/records/$TAG-create-full.retry.json"
assert_eq "$(jq -r '.volume_id' "$RUN/records/$TAG-create-full.retry.json")" "vol-d5-full" "retried volume id"
stop_daemon "$TAG-daemon"
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
if off != len(data):
    print(f"ERROR: {len(data) - off} trailing byte(s) after the valid prefix", file=sys.stderr)
    sys.exit(1)
PY

# ---------------------------------------------------------------- part B
say "B: LVM provider state filesystem ENOSPC (journal on ordinary storage)"
SFS="$RUN/lab-$TAG/statefs"
mkdir -p "$SFS"
mount_tmpfs_256k "$SFS" || fail_drill "state tmpfs is not usable"
LSTATE="$SFS/lvm-state.json"
LCFG="$RUN/configs/$TAG-lvm.toml"
LJOURNAL="$RUN/lab-$TAG/lvm-journal"
lvm_lab_setup "$TAG-lvm" 2G "$LSTATE" "vg-d5lvm" || fail_drill "lvm lab setup failed"
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
cat >"$RUN/records/$TAG-lvm-full-newop.json" <<'EOF'
{"api_version":"volvisor.volume.v2","operation_id":"op-d5-lvm-full-newop","project_id":"day2","volume_id":"vol-d5-lvm-full","class":"native-local","size_bytes":67108864}
EOF

start_volvisord "$TAG-lvm-daemon" "$LCFG" "$LPORT" || fail_drill "$TAG-lvm-daemon did not become healthy"
cap "create the LVM baseline volume" \
  api_expect "$LCURL" POST "$LBASE/v2/volumes" "$RUN/records/$TAG-lvm-baseline.json" 200 "$RUN/records/$TAG-lvm-baseline.resp.json"
cap "snapshot the durable state before the failure" cp "$LSTATE" "$RUN/records/$TAG-lvm-state-baseline.json"
STATE_SHA_BEFORE="$(sha256sum "$LSTATE" | cut -d' ' -f1)"
STATE_SIZE_BEFORE="$(stat -c %s "$LSTATE")"
printf 'state before the failed save: size=%s sha=%s\n' "$STATE_SIZE_BEFORE" "$STATE_SHA_BEFORE" | tee -a "$LOG"
BASELINE_LV="$(jq -r '.volumes["vol-d5-lvm-baseline"].entry.lv_name' "$LSTATE")"
FAILED_LV="$(lv_name_for "vol-d5-lvm-full")"
printf 'baseline LV=%s failed-volume LV=%s\n' "$BASELINE_LV" "$FAILED_LV" | tee -a "$LOG"
assert_eq "$BASELINE_LV" "$(lv_name_for "vol-d5-lvm-baseline")" "LV name derivation matches the durable baseline record"
cap "LVM baseline LV exists" lvs --noheadings -o lv_name,vg_name,lv_size "vg-d5lvm"
cap "state filesystem usage before the fill" df -k "$SFS"
cap "fill the state filesystem to ENOSPC" fill_fs "$SFS" filler.bin
cap "state filesystem usage at ENOSPC" df -k "$SFS"
assert_eq "$(free_kib "$SFS")" "0" "state filesystem has zero KiB available at ENOSPC"

FAIL_RESP="$RUN/records/$TAG-lvm-full.resp.json"
cap "a create whose provider state save hits ENOSPC returns 500 INTERNAL" \
  api_expect "$LCURL" POST "$LBASE/v2/volumes" "$RUN/records/$TAG-lvm-full.json" 500 "$FAIL_RESP"
cap "the failed create's error body" cat "$FAIL_RESP"
FAIL_CODE="$(jq -r '.code' "$FAIL_RESP")"
FAIL_MSG="$(jq -r '.message' "$FAIL_RESP")"
assert_eq "$FAIL_CODE" "INTERNAL" "state save ENOSPC error code"
assert_contains "$FAIL_MSG" "No space left on device" "state save ENOSPC message names the OS error"
assert_contains "$FAIL_MSG" "$LSTATE.tmp" "state save ENOSPC message names the exact state tmp path"
assert_not_contains "$FAIL_MSG" "manual lvremove" "compensating lvremove succeeded (no manual remediation named)"
assert_not_contains "$FAIL_MSG" "were kept" "failure is the uncommitted-save compensation, not the post-rename carve-out"

cap "the state file after the failed save" sha256sum "$LSTATE"
assert_eq "$(sha256sum "$LSTATE" | cut -d' ' -f1)" "$STATE_SHA_BEFORE" "durable state file unchanged by the failed save"
assert_eq "$(stat -c %s "$LSTATE")" "$STATE_SIZE_BEFORE" "durable state file size unchanged by the failed save"
assert_eq "$(jq -r '.volumes | has("vol-d5-lvm-full")' "$LSTATE")" "false" "durable state has no failed-volume record"
assert_not_contains "$(cat "$LSTATE")" "vol-d5-lvm-full" "durable state text does not contain the failed volume id"

cap "state filesystem contents after the failed save" ls -la "$SFS"
TMP_AFTER_FAIL="$(find "$SFS" -maxdepth 1 -name '*.tmp' -print)"
printf '.tmp residue after the failed save: %s\n' "${TMP_AFTER_FAIL:-<none>}" | tee -a "$LOG"
assert_eq "$TMP_AFTER_FAIL" "" "no .tmp residue after the failed state save"

cap "LVs after the compensated create (only the baseline remains)" lvs --noheadings -o lv_name,vg_name,lv_size "vg-d5lvm"
assert_not_contains "$(lv_names "vg-d5lvm")" "$FAILED_LV" "no LV for the failed volume"
assert_eq "$(lv_count "vg-d5lvm")" "1" "exactly one LV (the baseline) remains"
assert_contains "$(lv_names "vg-d5lvm")" "$BASELINE_LV" "baseline LV still present"

cap "the same process reports the failed volume as absent" \
  api_expect "$LCURL" GET "$LBASE/v2/volumes/vol-d5-lvm-full" "" 404 "$RUN/records/$TAG-lvm-full-now.json"
cap "the same-process volume list contains only the baseline" \
  api_expect "$LCURL" GET "$LBASE/v2/volumes" "" 200 "$RUN/records/$TAG-lvm-list-now.json"
assert_eq "$(jq -r '.volumes | length' "$RUN/records/$TAG-lvm-list-now.json")" "1" "one volume in the same-process list"
assert_eq "$(jq -r '[.volumes[].volume_id] | join(",")' "$RUN/records/$TAG-lvm-list-now.json")" "vol-d5-lvm-baseline" "same-process list volume ids"

say "stop the daemon while the state filesystem is still full, free the filler, restart"
stop_daemon "$TAG-lvm-daemon"
# Preserve the first incarnation's log (failed save + the 500/404 audit
# trail; the compensation itself is shown by the `lvs` checks above) before
# start_volvisord truncates it.
cap "snapshot the first LVM daemon log before restart" cp "$RUN/raw/$TAG-lvm-daemon.daemon.log" "$RUN/raw/$TAG-lvm-daemon.pre-restart.log"
cap "free the filler" rm -f "$SFS/filler.bin"
cap "state filesystem usage after freeing the filler" df -k "$SFS"
start_volvisord "$TAG-lvm-daemon" "$LCFG" "$LPORT" || fail_drill "$TAG-lvm-daemon did not become healthy"
cap "after restart the failed volume is still 404" \
  api_expect "$LCURL" GET "$LBASE/v2/volumes/vol-d5-lvm-full" "" 404 "$RUN/records/$TAG-lvm-full-after.json"
cap "after restart the durable baseline volume is still served" \
  api_expect "$LCURL" GET "$LBASE/v2/volumes/vol-d5-lvm-baseline" "" 200 "$RUN/records/$TAG-lvm-baseline-after.json"
jq '{volume_id,state,provisioned_bytes}' "$RUN/records/$TAG-lvm-baseline-after.json" | tee -a "$LOG"
assert_eq "$(jq -r '.volume_id' "$RUN/records/$TAG-lvm-baseline-after.json")" "vol-d5-lvm-baseline" "baseline volume id after restart"
assert_eq "$(jq -r '.state' "$RUN/records/$TAG-lvm-baseline-after.json")" "Ready" "baseline volume state after restart"
cap "LVs after restart (only the baseline remains)" lvs --noheadings -o lv_name,vg_name,lv_size "vg-d5lvm"
assert_contains "$(lv_names "vg-d5lvm")" "$BASELINE_LV" "baseline LV still present after restart"
assert_eq "$(lv_count "vg-d5lvm")" "1" "only the baseline LV present after restart"

cap "durable state checksum after restart" sha256sum "$LSTATE"
assert_eq "$(sha256sum "$LSTATE" | cut -d' ' -f1)" "$STATE_SHA_BEFORE" "durable state still matches the pre-failure snapshot after restart"
assert_eq "$(stat -c %s "$LSTATE")" "$STATE_SIZE_BEFORE" "durable state size still matches the pre-failure snapshot after restart"
assert_eq "$(jq -r '.volumes | has("vol-d5-lvm-full")' "$LSTATE")" "false" "durable state still has no failed-volume record after restart"
assert_not_contains "$(cat "$LSTATE")" "vol-d5-lvm-full" "durable state text still lacks the failed volume id after restart"

cap "state filesystem contents after restart" ls -la "$SFS"
TMP_AFTER_RESTART="$(find "$SFS" -maxdepth 1 -name '*.tmp' -print)"
printf '.tmp residue after restart: %s\n' "${TMP_AFTER_RESTART:-<none>}" | tee -a "$LOG"
assert_eq "$TMP_AFTER_RESTART" "" "no .tmp residue after restart"

REPLAY_RESP="$RUN/records/$TAG-lvm-full.replay.json"
cap "replay the same operation_id after space is free (terminal failure replay)" \
  api_expect "$LCURL" POST "$LBASE/v2/volumes" "$RUN/records/$TAG-lvm-full.json" 500 "$REPLAY_RESP"
cap "the same-operation_id replay error body" cat "$REPLAY_RESP"
assert_eq "$(jq -r '.code' "$REPLAY_RESP")" "INTERNAL" "same operation_id terminal replay keeps INTERNAL"
assert_eq "$(jq -r '.message' "$REPLAY_RESP")" "$FAIL_MSG" "same operation_id terminal replay serves the recorded message"
assert_eq "$(cmp -s "$REPLAY_RESP" "$FAIL_RESP" && printf identical || printf different)" "identical" "same operation_id terminal replay body is byte-identical (cmp)"
assert_contains "$(cat "$RUN/raw/$TAG-lvm-daemon.daemon.log")" "replaying recorded outcome without executing" "daemon log records the replay path without execution"
assert_eq "$(jq -r '.volumes | length' "$LSTATE")" "1" "terminal replay added no durable volume"
assert_eq "$(jq -r '.volumes | has("vol-d5-lvm-full")' "$LSTATE")" "false" "terminal replay added no failed-volume record"
cap "LVs after the terminal replay (only the baseline remains)" lvs --noheadings -o lv_name,vg_name,lv_size "vg-d5lvm"
assert_eq "$(lv_count "vg-d5lvm")" "1" "terminal replay left only the baseline LV"

NEWOP_RESP="$RUN/records/$TAG-lvm-full-newop.resp.json"
cap "a new operation_id with the same payload succeeds" \
  api_expect "$LCURL" POST "$LBASE/v2/volumes" "$RUN/records/$TAG-lvm-full-newop.json" 200 "$NEWOP_RESP"
jq '{volume_id,state,provisioned_bytes}' "$NEWOP_RESP" | tee -a "$LOG"
assert_eq "$(jq -r '.volume_id' "$NEWOP_RESP")" "vol-d5-lvm-full" "new-operation volume id"
assert_eq "$(jq -r '.state' "$NEWOP_RESP")" "Ready" "new-operation volume state"
assert_eq "$(jq -r '.provisioned_bytes' "$NEWOP_RESP")" "67108864" "new-operation provisioned bytes"

cap "snapshot the durable state after the successful create" cp "$LSTATE" "$RUN/records/$TAG-lvm-state-final.json"
cap "durable state checksum after the successful create" sha256sum "$LSTATE"
DURABLE_LV="$(jq -r '.volumes["vol-d5-lvm-full"].entry.lv_name' "$LSTATE")"
printf 'durable failed-volume LV name=%s\n' "$DURABLE_LV" | tee -a "$LOG"
assert_eq "$DURABLE_LV" "$FAILED_LV" "durable record uses the derived failed-volume LV name"
cap "LVs after the successful create (baseline plus new volume)" lvs --noheadings -o lv_name,vg_name,lv_size "vg-d5lvm"
NEW_VOLUME_LV_COUNT="$(lv_names "vg-d5lvm" | awk -v wanted="$FAILED_LV" '$0 == wanted { n++ } END { print n + 0 }')"
assert_eq "$(lv_count "vg-d5lvm")" "2" "baseline plus new-volume LV (total 2)"
assert_eq "$NEW_VOLUME_LV_COUNT" "1" "the new volume's derived LV name appears exactly once"
assert_contains "$(lv_names "vg-d5lvm")" "$BASELINE_LV" "baseline LV still present after the new create"
assert_eq "$(jq -r '.volumes | length' "$LSTATE")" "2" "durable state holds exactly two volume records"
assert_eq "$(jq -r '.volumes["vol-d5-lvm-full"] != null' "$LSTATE")" "true" "durable state holds the new volume record"
assert_eq "$(jq -r '.volumes["vol-d5-lvm-baseline"] != null' "$LSTATE")" "true" "durable state retains the baseline record"

cap "state filesystem contents after the successful create" ls -la "$SFS"
TMP_AFTER_SUCCESS="$(find "$SFS" -maxdepth 1 -name '*.tmp' -print)"
printf '.tmp residue after the successful create: %s\n' "${TMP_AFTER_SUCCESS:-<none>}" | tee -a "$LOG"
assert_eq "$TMP_AFTER_SUCCESS" "" "no .tmp residue after the successful create"
stop_daemon "$TAG-lvm-daemon"
cleanup

if end_drill; then
  printf 'D5 COMPLETE\n'
else
  printf 'D5 FAILED\n'
  exit 1
fi
