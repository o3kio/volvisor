#!/usr/bin/env bash
# Shared helpers for the #46 day-2 drills (single-host lab).
# Not a production script: it starts daemons directly (no systemd) and
# uses disposable loop-device LVM. Evidence is written under $RUN.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="${REPO:-$(cd "$SCRIPT_DIR/../../../.." && pwd)}"
BIN="${BIN:-$REPO/target/debug}"
RUN="${RUN:-/tmp/opencode/volvisor-day2-$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$RUN/raw" "$RUN/records" "$RUN/configs" "$RUN/certs"

LOG=""
START_UTC=""
ASSERT_FAILS=0
# Per-drill registry of binary paths this drill may signal. A path registered
# by one drill must never authorize signalling another drill's daemon, so the
# filename includes the drill name (set by begin_drill).
DAEMON_PATHS_FILE=""

now() { date -u +%FT%TZ; }

begin_drill() {
  local name="$1"
  LOG="$RUN/raw/$name.log"
  START_UTC="$(now)"
  DAEMON_PATHS_FILE="$RUN/records/daemon-paths-$name.txt"
  # Bind the retained log to the exact script bytes that produced it. The
  # harness and shared library are not part of the product commit, so the
  # digest is the only run-time proof of which logic ran.
  local harness="${BASH_SOURCE[1]:-$0}" lib="${BASH_SOURCE[0]}"
  {
    printf '# drill: %s\n' "$name"
    printf '# start: %s\n' "$START_UTC"
    printf '# invocation: %s\n' "$harness"
    printf '# harness_sha256: %s\n' "$(sha256sum "$harness" | cut -d' ' -f1)"
    printf '# lib_sha256: %s\n' "$(sha256sum "$lib" | cut -d' ' -f1)"
    printf '# host: %s\n' "$(uname -n)"
    printf '# kernel: %s\n' "$(uname -r)"
    printf '# source_sha: %s\n' "$(git -C "$REPO" rev-parse HEAD)"
    printf '# worktree_dirty: %s\n' "$(test -n "$(git -C "$REPO" status --porcelain)" && echo yes || echo no)"
    printf '# runner: uid=%s\n' "$(id -u)"
    printf '\n'
  } >>"$LOG"
}

end_drill() {
  printf '\n# end: %s\n' "$(now)" >>"$LOG"
  if [[ "$ASSERT_FAILS" -ne 0 ]]; then
    printf 'ASSERT SUMMARY: %s failure(s)\n' "$ASSERT_FAILS" | tee -a "$LOG"
    return 1
  fi
  printf 'ASSERT SUMMARY: 0 failures\n' | tee -a "$LOG"
}

say() { printf '\n### %s %s host=%s\n' "$(now)" "$1" "$(uname -n)" | tee -a "$LOG"; }

# cap <tag> <command...>: run, record exact argv/stdout/stderr/exit status.
# CAP_EXPECT_RC (default 0) is the status the command is expected to return;
# any other status is an assertion failure. Set it for commands whose nonzero
# status is itself the observation (D3's transport-failure capture).
cap() {
  local tag="$1"; shift
  local want_rc="${CAP_EXPECT_RC:-0}"
  say "$tag"
  local rc
  {
    printf '$'
    printf ' %q' "$@"
    printf '\n'
    "$@"
    rc=$?
    printf 'exit=%s\n' "$rc"
    # Left side of a pipeline runs in a subshell; exit it with the command's
    # status so PIPESTATUS[0] actually carries the failure.
    exit "$rc"
  } 2>&1 | tee -a "$LOG"
  rc="${PIPESTATUS[0]}"
  # The command ran in the pipeline's subshell, so any path that increments
  # ASSERT_FAILS there (api_expect on a status mismatch, a local assert) is
  # lost to the parent. Count an unexpected status here instead: every `cap`
  # command is expected to succeed unless CAP_EXPECT_RC says otherwise.
  if [[ "$rc" -ne "$want_rc" ]]; then
    printf 'ASSERT FAIL: cap %q exited %s (expected %s)\n' "$tag" "$rc" "$want_rc" | tee -a "$LOG"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
  fi
  return "$rc"
}

# cap_stdin <tag> <file> <command...>: same, but feed <file> on stdin.
cap_stdin() {
  local tag="$1" input="$2"; shift 2
  local want_rc="${CAP_EXPECT_RC:-0}"
  say "$tag"
  local rc
  {
    printf '$'
    printf ' %q' "$@"
    printf ' < %q\n' "$input"
    "$@" <"$input"
    rc=$?
    printf 'exit=%s\n' "$rc"
    exit "$rc"
  } 2>&1 | tee -a "$LOG"
  rc="${PIPESTATUS[0]}"
  if [[ "$rc" -ne "$want_rc" ]]; then
    printf 'ASSERT FAIL: cap_stdin %q exited %s (expected %s)\n' "$tag" "$rc" "$want_rc" | tee -a "$LOG"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
  fi
  return "$rc"
}

assert_eq() {
  local got="$1" want="$2" what="$3"
  if [[ "$got" == "$want" ]]; then
    printf 'ASSERT PASS: %s == %q\n' "$what" "$want" | tee -a "$LOG"
  else
    printf 'ASSERT FAIL: %s got %q want %q\n' "$what" "$got" "$want" | tee -a "$LOG"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
    return 1
  fi
}

assert_contains() {
  local haystack="$1" needle="$2" what="$3"
  if [[ "$haystack" == *"$needle"* ]]; then
    printf 'ASSERT PASS: %s contains %q\n' "$what" "$needle" | tee -a "$LOG"
  else
    printf 'ASSERT FAIL: %s does not contain %q\n' "$what" "$needle" | tee -a "$LOG"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
    return 1
  fi
}

assert_not_contains() {
  local haystack="$1" needle="$2" what="$3"
  if [[ "$haystack" != *"$needle"* ]]; then
    printf 'ASSERT PASS: %s does not contain %q\n' "$what" "$needle" | tee -a "$LOG"
  else
    printf 'ASSERT FAIL: %s unexpectedly contains %q\n' "$what" "$needle" | tee -a "$LOG"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
    return 1
  fi
}

# Pick a currently free loopback port.
#
# The bind is inherently racy: the daemon binds the port later. If another
# process wins the race the daemon fails to bind and the drill fails at
# startup; there is no silent fallback to a different port.
pick_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

wait_health() {
  local base="$1" timeout="${2:-20}" i
  for ((i = 0; i < timeout * 10; i++)); do
    if curl -fsS --max-time 1 "$base/healthz" >/dev/null 2>&1; then return 0; fi
    sleep 0.1
  done
  return 1
}

# start_volvisord <name> <config> <port>: start directly, wait for health.
start_volvisord() {
  start_volvisord_bin "$BIN/volvisord" "$@"
}

# Register a binary path this drill is allowed to signal. start_volvisord_bin
# does this automatically; a cross-binary drill can register an extra path.
register_daemon_path() {
  local path="$1"
  mkdir -p "$(dirname "$DAEMON_PATHS_FILE")"
  touch "$DAEMON_PATHS_FILE"
  grep -Fxq "$path" "$DAEMON_PATHS_FILE" 2>/dev/null || printf '%s\n' "$path" >>"$DAEMON_PATHS_FILE"
}

# start_volvisord_bin <binary> <name> <config> <port>: start an explicitly
# named volvisord build (D2 live starts both an old and a new binary), wait
# for health.
start_volvisord_bin() {
  local bin="$1" name="$2" cfg="$3" port="$4"
  local log="$RUN/raw/$name.daemon.log"
  register_daemon_path "$bin"
  "$bin" --config "$cfg" >"$log" 2>&1 &
  echo "$!" >"$RUN/records/$name.pid"
  if ! wait_health "http://127.0.0.1:$port" 30; then
    printf 'DAEMON FAILED TO BECOME HEALTHY: %s (pid %s)\n' "$name" "$(cat "$RUN/records/$name.pid")" | tee -a "$LOG"
    tail -40 "$log" | tee -a "$LOG"
    return 1
  fi
  printf 'daemon %s healthy pid=%s port=%s bin=%s sha256=%s\n' "$name" \
    "$(cat "$RUN/records/$name.pid")" "$port" "$bin" \
    "$(sha256sum "$bin" | cut -d' ' -f1)" | tee -a "$LOG"
}

pid_is_drill_daemon() {
  local pid="$1" cmd
  [[ -r "/proc/$pid/cmdline" ]] || return 1
  cmd="$(tr '\0' '\n' <"/proc/$pid/cmdline" | head -n 1)"
  # Exact path only: a basename match would signal another drill's daemon
  # after PID reuse. The default binaries and every binary registered by *this
  # drill* are accepted; a process executing any other path is not.
  [[ "$cmd" == "$BIN/volvisord" || "$cmd" == "$BIN/volvisor-witnessd" ]] && return 0
  [[ -f "$DAEMON_PATHS_FILE" ]] && grep -Fxq "$cmd" "$DAEMON_PATHS_FILE"
}

stop_pidfile() {
  local name="$1" sig="${2:-TERM}" pidfile="$RUN/records/$1.pid"
  [[ -f "$pidfile" ]] || return 0
  local pid; pid="$(cat "$pidfile")"
  if ! pid_is_drill_daemon "$pid"; then
    printf 'WARNING: %s pid %s is not this drill'"'"'s daemon (stale pidfile); not signalling\n' \
      "$name" "$pid" | tee -a "$LOG"
    rm -f "$pidfile"
    return 0
  fi
  if kill -0 "$pid" 2>/dev/null; then
    kill "-$sig" "$pid" 2>/dev/null || true
  fi
  local i
  for ((i = 0; i < 100; i++)); do
    kill -0 "$pid" 2>/dev/null || { rm -f "$pidfile"; printf 'stopped %s pid=%s sig=%s\n' "$name" "$pid" "$sig" | tee -a "$LOG"; return 0; }
    sleep 0.1
  done
  printf 'ASSERT FAIL: %s pid %s still alive after %s\n' "$name" "$pid" "$sig" | tee -a "$LOG"
  ASSERT_FAILS=$((ASSERT_FAILS + 1))
  return 1
}

# stop_or_exit <name>: stop a daemon and refuse to continue if it is still
# alive. A backup or restore taken while a writer survives is not a
# consistency point, so this is a hard stop rather than an accumulated assert.
stop_or_exit() {
  local name="$1"
  if ! stop_pidfile "$name" TERM; then
    printf 'REFUSING to continue: %s did not stop\n' "$name" | tee -a "$LOG"
    exit 1
  fi
}

# assert_no_writer <journal-dir> <what>: prove no process holds the journal
# lock before treating the directory as a stopped-state consistency point.
assert_no_writer() {
  local journal="$1" what="$2"
  if flock -n "$journal/journal.lock" -c true 2>/dev/null; then
    printf 'ASSERT PASS: %s (journal lock is free)\n' "$what" | tee -a "$LOG"
  else
    printf 'ASSERT FAIL: %s (a writer still holds %s/journal.lock)\n' "$what" "$journal" | tee -a "$LOG"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
    return 1
  fi
}

# api <curl-config> <method> <url> [body-file] -> stdout, status in $RUN/records/last.status
api() {
  local cfg="$1" method="$2" url="$3" body="${4:-}"
  local out status
  # curl's --show-error diagnostics go straight to the drill log: the
  # returned body only carries stdout + status, so a transport failure would
  # otherwise leave the retained evidence with just status 000.
  if [[ -n "$body" ]]; then
    out="$(curl --silent --show-error --config "$cfg" --request "$method" \
      --header 'content-type: application/json' --data-binary @"$body" \
      --write-out $'\n%{http_code}' "$url" 2>>"$LOG")"
  else
    out="$(curl --silent --show-error --config "$cfg" --request "$method" \
      --write-out $'\n%{http_code}' "$url" 2>>"$LOG")"
  fi
  status="${out##*$'\n'}"
  out="${out%$'\n'*}"
  printf '%s' "$status" >"$RUN/records/last.status"
  printf '%s' "$out"
}

# api_expect <curl-config> <method> <url> <body-file> <expected-status> <out-file>
api_expect() {
  local cfg="$1" method="$2" url="$3" body="$4" want="$5" outfile="$6"
  local body_out status
  body_out="$(api "$cfg" "$method" "$url" "$body")"
  status="$(cat "$RUN/records/last.status")"
  printf '%s' "$body_out" >"$outfile"
  printf 'HTTP %s (want %s) %s %s\n' "$status" "$want" "$method" "$url" | tee -a "$LOG"
  if [[ "$status" != "$want" ]]; then
    printf 'response: %s\n' "$body_out" | tee -a "$LOG"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
    return 1
  fi
}

# write_header_config <path> <token>: owner-only curl config, never printed.
write_header_config() {
  local path="$1" token="$2"
  umask 077
  printf 'header = "Authorization: Bearer %s"\n' "$token" >"$path"
  chmod 600 "$path"
}

# json_get <file> <jq expr>
json_get() { jq -r "$2" "$1"; }

# checksum <path> -> sha256 line
checksum() { sha256sum "$1"; }

# lvm_lab_setup <tag> <size> <seed-state-path> [vg-name]
# Creates a loop-backed VG and a provider state file claiming it. Loop
# devices are deliberately skipped by discovery, so the lab seeds the
# operator-provisioned claim record instead of using /v2/admin claim.
lvm_lab_setup() {
  local tag="$1" size="$2" state_path="$3" vg="${4:-$1}" dir="$RUN/lab-$1"
  if vgs "$vg" >/dev/null 2>&1; then
    printf 'REFUSING to reuse existing VG %s; choose a lab-unique name\n' "$vg" >&2
    return 1
  fi
  mkdir -p "$dir"
  truncate -s "$size" "$dir/disk.img"
  local loop; loop="$(losetup -f --show "$dir/disk.img")" || {
    printf 'losetup failed for %s\n' "$dir/disk.img" >&2
    return 1
  }
  printf '%s' "$loop" >"$RUN/records/$tag.loop"
  printf '%s' "$vg" >"$RUN/records/$tag.vg"
  pvcreate --yes "$loop" >>"$LOG" 2>&1 || return 1
  vgcreate --yes "$vg" "$loop" >>"$LOG" 2>&1 || return 1
  mkdir -p "$(dirname "$state_path")"
  cat >"$state_path" <<EOF
{
  "schema_version": 1,
  "volumes": {},
  "devices": {
    "dev-d2${tag}01": {
      "stable_identity": "day2-lab-$tag",
      "path": "$loop",
      "vg_name": "$vg",
      "role": "native_pool",
      "owner_generation": 1
    }
  },
  "moves": {}
}
EOF
  printf 'lab setup tag=%s loop=%s vg=%s state=%s\n' "$tag" "$loop" "$vg" "$state_path" | tee -a "$LOG"
}

lvm_lab_teardown() {
  local tag="$1"
  local dir="$RUN/lab-$tag"
  local loop="" vg="" failed=0 owners=""
  [[ -f "$RUN/records/$tag.loop" ]] && loop="$(cat "$RUN/records/$tag.loop")"
  [[ -f "$RUN/records/$tag.vg" ]] && vg="$(cat "$RUN/records/$tag.vg")"
  if [[ -n "$vg" ]] && vgs "$vg" >/dev/null 2>&1; then
    # `pvs` positional arguments are *PV* names, not VG names: `pvs "$vg"`
    # prints nothing and the ownership guard would refuse every teardown.
    # Ask the VG which PVs back it instead.
    owners="$(vgs --noheadings -o pv_name "$vg" 2>/dev/null | xargs)"
    if [[ -z "$loop" || "$owners" != "$loop" ]]; then
      printf 'REFUSING to remove VG %s: its PVs (%s) are not this drill'"'"'s loop (%s)\n' \
        "$vg" "${owners:-unknown}" "${loop:-unknown}" | tee -a "$LOG"
      failed=1
    elif ! vgremove --yes "$vg" >>"$LOG" 2>&1; then
      printf 'vgremove %s failed\n' "$vg" | tee -a "$LOG"
      failed=1
    fi
  fi
  if [[ -n "$loop" ]] && losetup "$loop" >/dev/null 2>&1; then
    if ! pvremove --yes "$loop" >>"$LOG" 2>&1; then
      printf 'pvremove %s failed\n' "$loop" | tee -a "$LOG"
      failed=1
    fi
    if ! losetup -d "$loop" >>"$LOG" 2>&1; then
      printf 'losetup -d %s failed\n' "$loop" | tee -a "$LOG"
      failed=1
    fi
  fi
  if [[ "$failed" -ne 0 ]]; then
    printf 'lab teardown INCOMPLETE tag=%s loop=%s vg=%s dir=%s\n' \
      "$tag" "$loop" "$vg" "$dir" | tee -a "$LOG"
    return 1
  fi
  rm -rf "$dir"
  printf 'lab teardown tag=%s loop=%s vg=%s\n' "$tag" "$loop" "$vg" | tee -a "$LOG"
}
