#!/usr/bin/env bash
# Run the issue #43 benchmark suite on one development host.
#
# Usage:
#   sudo scripts/bench/run-all.sh [--run-dir DIR] [--soak-minutes 25]
#                                 [--sweep-seconds 30] [--skip-soak]
#                                 [--skip-build] [--keep-lab]
#
# The default is the published run: a release build (CARGO_BUILD_JOBS=4),
# a 1/8/32/128 mixed-concurrency sweep, and a shortened 25-minute soak with
# one restart. The 24-hour soak is explicitly NOT RUN here.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091
source "$HERE/lib.sh"

RUN_DIR=""
SOAK_MINUTES=25
SWEEP_SECONDS=30
SKIP_SOAK=0
SKIP_BUILD=0
KEEP_LAB=0
LAB_READY=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --run-dir) RUN_DIR="$2"; shift 2 ;;
    --soak-minutes) SOAK_MINUTES="$2"; shift 2 ;;
    --sweep-seconds) SWEEP_SECONDS="$2"; shift 2 ;;
    --skip-soak) SKIP_SOAK=1; shift ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --keep-lab) KEEP_LAB=1; shift ;;
    -h|--help) sed -n '1,20p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

if [[ -z "$RUN_DIR" ]]; then
  RUN_DIR="/tmp/opencode/volvisor-bench-$(date -u +%Y%m%dT%H%M%SZ)"
fi
export RUN="$RUN_DIR"
export REPO BIN
make_run_tree
begin_log "00-orchestration"

cleanup() {
  local rc=$?
  stop_pidfile "daemon" TERM >/dev/null 2>&1 || true
  if [[ "$LAB_READY" -eq 1 && "$KEEP_LAB" -eq 0 ]]; then
    lvm_lab_teardown "main" || true
  fi
  end_log
  exit "$rc"
}
trap cleanup EXIT

require_prereqs

say "benchmark run dir: $RUN"
say "source sha: $(git -C "$REPO" rev-parse HEAD)"
if [[ -n "$(git -C "$REPO" status --porcelain)" ]]; then
  say "worktree: DIRTY (results must be reported as dirty)"
else
  say "worktree: clean"
fi

if [[ "$SKIP_BUILD" -eq 0 || ! -x "$BIN/volvisord" ]]; then
  say "building release volvisord (CARGO_BUILD_JOBS=4)"
  (
    cd "$REPO" || exit 1
    CARGO_BUILD_JOBS=4 cargo build --release --locked -p volvisord
  ) >>"$LOG" 2>&1 || die "release build failed; see $LOG"
fi

write_environment "$RUN/ENVIRONMENT.txt"

say "recording pre-existing volume groups"
vgs --noheadings -o vg_name,vg_size,pv_count >"$RUN/records/vgs-before.txt" 2>&1 || true

TOKEN="$(new_token)"
write_token_file "$RUN/configs/token" "$TOKEN"
PORT="$(pick_port)"
VG="vgb-$(date -u +%s)"
printf '%s' "$VG" >"$RUN/records/lab.vg"
JOURNAL_DIR="$RUN/lab/journal"
STATE_PATH="$RUN/lab/lvm-state.json"
mkdir -p "$JOURNAL_DIR"

umask 077
cat >"$RUN/configs/volvisord.toml" <<EOF
listen = "127.0.0.1:$PORT"
journal_dir = "$JOURNAL_DIR"
provider = "lvm"
lvm_vg_prefix = "volvisor-bench"
device_claim_token = "$TOKEN"
lvm_state_path = "$STATE_PATH"
admin_token = "$TOKEN"

[admission]
read_timeout_secs = 30
queue_timeout_secs = 5
max_concurrent_requests = 256
rate_limit_per_second = 0
rate_limit_burst = 0
max_list_limit = 1000
EOF
chmod 600 "$RUN/configs/volvisord.toml"

cat >"$RUN/configs/volvisord-cap32.toml" <<EOF
listen = "127.0.0.1:$PORT"
journal_dir = "$JOURNAL_DIR"
provider = "lvm"
lvm_vg_prefix = "volvisor-bench"
device_claim_token = "$TOKEN"
lvm_state_path = "$STATE_PATH"
admin_token = "$TOKEN"

[admission]
read_timeout_secs = 30
# Diagnostic: a 1s queue timeout makes shedding observable at this cap.
# The default-cap runs above keep the product default of 5s.
queue_timeout_secs = 1
max_concurrent_requests = 32
rate_limit_per_second = 0
rate_limit_burst = 0
max_list_limit = 1000
EOF
chmod 600 "$RUN/configs/volvisord-cap32.toml"

cat >"$RUN/configs/volvisord-rate50.toml" <<EOF
listen = "127.0.0.1:$PORT"
journal_dir = "$JOURNAL_DIR"
provider = "lvm"
lvm_vg_prefix = "volvisor-bench"
device_claim_token = "$TOKEN"
lvm_state_path = "$STATE_PATH"
admin_token = "$TOKEN"

[admission]
read_timeout_secs = 30
queue_timeout_secs = 5
max_concurrent_requests = 256
# Diagnostic: deterministic shedding at 50 requests/s per principal.
rate_limit_per_second = 50
rate_limit_burst = 50
max_list_limit = 1000
EOF
chmod 600 "$RUN/configs/volvisord-rate50.toml"
umask 022

printf 'port=%s vg=%s\n' "$PORT" "$VG" >"$RUN/records/lab.env"
printf '%s\n' "$PORT" >"$RUN/records/port"

say "config check"
cap "check-config" "$BIN/volvisord" --check-config --config "$RUN/configs/volvisord.toml" \
  || die "config check failed"

say "creating disposable loop-backed LVM lab"
lvm_lab_setup "main" 80G "$STATE_PATH" "$VG"
LAB_READY=1

start_daemon "daemon" "$RUN/configs/volvisord.toml" "$PORT" >/dev/null
PID="$(cat "$RUN/records/daemon.pid")"

COMMON_BASE=(
  --host 127.0.0.1
  --port "$PORT"
  --token-file "$RUN/configs/token"
  --out-dir "$RUN"
  --journal "$JOURNAL_DIR/journal.log"
  --state "$STATE_PATH"
  --repo "$REPO"
  --binary "$BIN/volvisord"
  --config "$RUN/configs/volvisord.toml"
  --daemon-log "$RUN/raw/daemon.daemon.log"
  --pidfile "$RUN/records/daemon.pid"
)
COMMON=("${COMMON_BASE[@]}" --daemon-pid "$PID")

say "micro benchmarks: create/delete, attach/report/detach, grow, inspect"
python3 "$HERE/bench.py" micro "${COMMON[@]}" --create-delete-iterations 40 \
  --cycle-iterations 150 --grow-iterations 30 --inspect-iterations 400 \
  --list-iterations 100 --capabilities-iterations 100 >>"$LOG" 2>&1 \
  || die "micro benchmark failed"

say "restart replay after the micro journal"
python3 "$HERE/bench.py" restart "${COMMON[@]}" --restart-iterations 3 \
  --label "pre-soak" >>"$LOG" 2>&1 || die "restart benchmark failed"

PID="$(cat "$RUN/records/daemon.pid")"
COMMON=("${COMMON_BASE[@]}" --daemon-pid "$PID")

for concurrency in 1 8 32 128; do
  say "mixed concurrency sweep: $concurrency in-flight, ${SWEEP_SECONDS}s"
  python3 "$HERE/bench.py" mixed "${COMMON[@]}" --concurrency "$concurrency" \
    --duration "$SWEEP_SECONDS" --read-ratio 0.7 \
    --admission-cap 256 --queue-timeout 5 --label "c$concurrency" \
    >>"$LOG" 2>&1 || die "mixed concurrency $concurrency failed"
done

say "admission shedding probe: cap=32, 1s queue timeout, 128 in-flight, 15s (diagnostic, not the default cap)"
stop_pidfile "daemon" TERM
start_daemon "daemon" "$RUN/configs/volvisord-cap32.toml" "$PORT" >/dev/null
PID="$(cat "$RUN/records/daemon.pid")"
COMMON=("${COMMON_BASE[@]}" --daemon-pid "$PID")
python3 "$HERE/bench.py" mixed "${COMMON[@]}" --concurrency 128 --duration 15 \
  --read-ratio 0.7 --admission-cap 32 --queue-timeout 1 \
  --label "cap32-c128" >>"$LOG" 2>&1 || die "admission cap probe failed"

say "rate-limit shedding probe: 50 rps burst 50, 64 in-flight, 15s (diagnostic)"
stop_pidfile "daemon" TERM
start_daemon "daemon" "$RUN/configs/volvisord-rate50.toml" "$PORT" >/dev/null
PID="$(cat "$RUN/records/daemon.pid")"
COMMON=("${COMMON_BASE[@]}" --daemon-pid "$PID")
python3 "$HERE/bench.py" mixed "${COMMON[@]}" --concurrency 64 --duration 15 \
  --read-ratio 0.7 --admission-cap 256 --queue-timeout 5 --rate-limit 50 \
  --label "rate50-c64" >>"$LOG" 2>&1 || die "rate-limit shedding probe failed"
stop_pidfile "daemon" TERM
start_daemon "daemon" "$RUN/configs/volvisord.toml" "$PORT" >/dev/null
PID="$(cat "$RUN/records/daemon.pid")"
COMMON=("${COMMON_BASE[@]}" --daemon-pid "$PID")

if [[ "$SKIP_SOAK" -eq 0 ]]; then
  RESTART_AFTER=$((SOAK_MINUTES / 2))
  say "shortened soak: ${SOAK_MINUTES} minutes, 4 workers, restart after ${RESTART_AFTER} minutes"
  # The soak driver owns the daemon lifecycle for its restart; the pidfile is
  # the shell's only handle after this point.
  python3 "$HERE/bench.py" soak "${COMMON[@]}" --duration-minutes "$SOAK_MINUTES" \
    --workers 4 --read-ratio 0.6 --restart-after-minutes "$RESTART_AFTER" \
    --max-extra-grows 128 >>"$LOG" 2>&1 \
    || die "soak failed"

  say "restart replay after the shortened soak journal"
  PID="$(cat "$RUN/records/daemon.pid")"
  COMMON=("${COMMON_BASE[@]}" --daemon-pid "$PID")
  python3 "$HERE/bench.py" restart "${COMMON[@]}" --restart-iterations 2 \
    --label "post-soak" >>"$LOG" 2>&1 || die "post-soak restart benchmark failed"
fi

stop_pidfile "daemon" TERM
say "recording post-run volume groups"
vgs --noheadings -o vg_name,vg_size,pv_count >"$RUN/records/vgs-after.txt" 2>&1 || true

say "lab teardown"
lvm_lab_teardown "main"
LAB_READY=0

say "summarizing raw results"
python3 "$HERE/summarize.py" --run-dir "$RUN" >>"$LOG" 2>&1 \
  || die "summary generation failed"

write_checksums "$RUN"
say "done: $RUN"
say "raw JSON, CSV, environment and MANIFEST.sha256 are in the run directory"
