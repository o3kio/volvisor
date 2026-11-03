#!/usr/bin/env bash
# Shared helpers for the #43 benchmark harness.
#
# This is a single-host lab harness, not a production tool. It creates a
# disposable loop-backed LVM volume group under $RUN, seeds the LVM
# provider's operator-provisioned device claim, and starts volvisord as a
# foreground child. It never inspects or mutates an existing VG: teardown
# names only the VG this run created.
set -uo pipefail

BENCH_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="${REPO:-$(cd "$BENCH_SCRIPT_DIR/../.." && pwd)}"
BIN="${BIN:-$REPO/target/release}"
RUN="${RUN:-/tmp/opencode/volvisor-bench-$(date -u +%Y%m%dT%H%M%SZ)}"

LOG=""

now() { date -u +%FT%TZ; }

say() { printf '\n### %s %s\n' "$(now)" "$*" | tee -a "$LOG"; }

die() {
  printf 'bench: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  local name="$1"
  command -v "$name" >/dev/null 2>&1 || die "required command not found: $name"
}

require_root() {
  [[ "$(id -u)" -eq 0 ]] || die "this harness needs root (loop devices + LVM)"
}

require_prereqs() {
  require_root
  local cmd
  for cmd in curl jq lvm losetup pvcreate pvremove vgcreate vgremove lvcreate lvremove \
    python3 sha256sum; do
    require_cmd "$cmd"
  done
  [[ -x "$BIN/volvisord" ]] || die "release binary missing: $BIN/volvisord (build it first)"
}

make_run_tree() {
  mkdir -p "$RUN"/{raw,records,csv,json,configs,lab,summary}
}

begin_log() {
  local name="$1"
  LOG="$RUN/raw/$name.log"
  {
    printf '# benchmark: %s\n' "$name"
    printf '# start: %s\n' "$(now)"
    printf '# host: %s\n' "$(uname -n)"
    printf '# kernel: %s\n' "$(uname -r)"
    printf '# repo: %s\n' "$REPO"
    printf '# source_sha: %s\n' "$(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unknown)"
    if [[ -n "$(git -C "$REPO" status --porcelain 2>/dev/null)" ]]; then
      printf '# worktree_dirty: yes\n'
    else
      printf '# worktree_dirty: no\n'
    fi
    printf '# binary: %s\n' "$BIN/volvisord"
    printf '# binary_sha256: %s\n' "$(sha256sum "$BIN/volvisord" | awk '{print $1}')"
    printf '# runner: uid=%s gid=%s\n' "$(id -u)" "$(id -g)"
    printf '\n'
  } >>"$LOG"
}

end_log() {
  printf '\n# end: %s\n' "$(now)" >>"$LOG"
}

# cap <tag> <command...>: record exact argv, stdout/stderr and exit status.
cap() {
  local tag="$1"
  shift
  say "$tag"
  local rc
  {
    printf '$'
    printf ' %q' "$@"
    printf '\n'
    "$@"
    rc=$?
    printf 'exit=%s\n' "$rc"
    # The group is the left side of a pipeline (a subshell), so `rc` does
    # not survive in this shell. Exit the subshell with the command's status;
    # PIPESTATUS[0] then carries it back.
    exit "$rc"
  } 2>&1 | tee -a "$LOG"
  return "${PIPESTATUS[0]}"
}

# restamp_binary <path>: replace the pre-build binary hash in the log header
# with the hash of the binary the run actually executed.
restamp_binary() {
  local log="$1"
  local hash
  hash="$(sha256sum "$BIN/volvisord" | awk '{print $1}')"
  sed -i "s|^# binary_sha256: .*|# binary_sha256: $hash|" "$log"
}

new_token() {
  python3 -c 'import secrets; print(secrets.token_urlsafe(32))'
}

# Pick a currently free loopback port.
#
# The bind is inherently racy: the daemon binds the port later. If another
# process wins the race the daemon fails to bind and the run dies at startup;
# there is no silent fallback to a different port, so no evidence is recorded
# against an unintended listener.
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
  local base="$1" timeout="${2:-30}" i
  for ((i = 0; i < timeout * 20; i++)); do
    if curl -fsS --max-time 1 "$base/healthz" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.05
  done
  return 1
}

write_token_file() {
  local path="$1" token="$2"
  umask 077
  printf '%s\n' "$token" >"$path"
  chmod 600 "$path"
}

# start_daemon <name> <config> <port>: echo the pid; logs under raw/<name>.daemon.log
start_daemon() {
  local name="$1" cfg="$2" port="$3"
  local log="$RUN/raw/$name.daemon.log"
  "$BIN/volvisord" --config "$cfg" >>"$log" 2>&1 &
  local pid=$!
  printf '%s\n' "$pid" >"$RUN/records/$name.pid"
  if ! wait_health "http://127.0.0.1:$port" 30; then
    tail -60 "$log" | tee -a "$LOG"
    die "daemon $name (pid $pid) did not become healthy"
  fi
  printf 'daemon %s healthy pid=%s port=%s\n' "$name" "$pid" "$port" | tee -a "$LOG"
  printf '%s\n' "$pid"
}

# pid_is_volvisord <pid>: true only when /proc/<pid>/cmdline is this run's
# volvisord binary. Guards against signalling an unrelated process after PID
# reuse.
pid_is_volvisord() {
  local pid="$1" cmd
  [[ -r "/proc/$pid/cmdline" ]] || return 1
  cmd="$(tr '\0' '\n' <"/proc/$pid/cmdline" | head -n 1)"
  # Exact path only: a basename match would signal another run's daemon after
  # PID reuse.
  [[ "$cmd" == "$BIN/volvisord" ]]
}

stop_pidfile() {
  local name="$1" sig="${2:-TERM}" pidfile="$RUN/records/$1.pid"
  [[ -f "$pidfile" ]] || return 0
  local pid
  pid="$(cat "$pidfile")"
  if ! pid_is_volvisord "$pid"; then
    printf 'WARNING: %s pid %s is not this run'"'"'s volvisord (stale pidfile); not signalling\n' \
      "$name" "$pid" | tee -a "$LOG"
    rm -f "$pidfile"
    return 0
  fi
  if kill -0 "$pid" 2>/dev/null; then
    kill "-$sig" "$pid" 2>/dev/null || true
  fi
  local i
  for ((i = 0; i < 200; i++)); do
    if ! kill -0 "$pid" 2>/dev/null; then
      rm -f "$pidfile"
      printf 'stopped %s pid=%s sig=%s\n' "$name" "$pid" "$sig" | tee -a "$LOG"
      return 0
    fi
    sleep 0.05
  done
  printf 'WARNING: %s pid %s still alive after %s\n' "$name" "$pid" "$sig" | tee -a "$LOG"
  return 1
}

# lvm_lab_setup <tag> <size> <state_path> <vg_name>
# Creates a loop-backed VG and the provider state that claims it. Loop
# devices are deliberately skipped by provider discovery, so the harness
# seeds a lab-only operator claim instead of using /v2/admin.
lvm_lab_setup() {
  local tag="$1" size="$2" state_path="$3" vg="$4"
  local dir="$RUN/lab/$tag"
  mkdir -p "$dir" "$(dirname "$state_path")"

  if vgs "$vg" >/dev/null 2>&1; then
    die "refusing to touch an existing VG named $vg"
  fi

  truncate -s "$size" "$dir/disk.img"
  local loop
  loop="$(losetup -f --show "$dir/disk.img")" || die "losetup failed"
  printf '%s' "$loop" >"$RUN/records/$tag.loop"
  printf '%s' "$vg" >"$RUN/records/$tag.vg"

  {
    printf '$ pvcreate --yes %s\n' "$loop"
    pvcreate --yes "$loop"
    printf '$ vgcreate --yes %s %s\n' "$vg" "$loop"
    vgcreate --yes "$vg" "$loop"
  } >>"$LOG" 2>&1 || {
    # The loop device exists now even though setup failed. Remove any VG/PV
    # residue and detach it before dying so the EXIT trap (which only knows
    # LAB_READY) cannot leak it.
    if vgs "$vg" >/dev/null 2>&1; then
      vgremove --yes --force "$vg" >>"$LOG" 2>&1 || true
    fi
    pvremove --yes "$loop" >>"$LOG" 2>&1 || true
    losetup -d "$loop" >>"$LOG" 2>&1 || \
      printf 'bench: WARNING: failed to detach %s after setup failure\n' "$loop" >&2
    rm -f "$RUN/records/$tag.loop" "$RUN/records/$tag.vg"
    die "loop-backed LVM setup failed; see $LOG"
  }

  cat >"$state_path" <<EOF
{
  "schema_version": 1,
  "volumes": {},
  "devices": {
    "dev-bench0001": {
      "stable_identity": "bench-lab-$tag",
      "path": "$loop",
      "vg_name": "$vg",
      "role": "native_pool",
      "owner_generation": 1
    }
  },
  "moves": {}
}
EOF
  chmod 600 "$state_path"
  printf 'lab setup tag=%s loop=%s vg=%s state=%s\n' "$tag" "$loop" "$vg" "$state_path" | tee -a "$LOG"
}

# lvm_lab_teardown <tag>: remove only the VG/loop recorded by setup.
# Returns non-zero and leaves the directory in place if any removal step
# fails, so a live loop/VG is never hidden by `rm -rf`.
lvm_lab_teardown() {
  local tag="$1"
  local dir="$RUN/lab/$tag"
  local loop="" vg="" failed=0
  [[ -f "$RUN/records/$tag.loop" ]] && loop="$(cat "$RUN/records/$tag.loop")"
  [[ -f "$RUN/records/$tag.vg" ]] && vg="$(cat "$RUN/records/$tag.vg")"
  if [[ -n "$vg" ]] && vgs "$vg" >/dev/null 2>&1; then
    if ! vgremove --yes "$vg" >>"$LOG" 2>&1; then
      printf 'bench: ERROR: vgremove %s failed; leaving lab state\n' "$vg" | tee -a "$LOG"
      failed=1
    fi
  fi
  if [[ -n "$loop" ]] && losetup "$loop" >/dev/null 2>&1; then
    if ! pvremove --yes "$loop" >>"$LOG" 2>&1; then
      printf 'bench: ERROR: pvremove %s failed; leaving loop attached\n' "$loop" | tee -a "$LOG"
      failed=1
    fi
    if ! losetup -d "$loop" >>"$LOG" 2>&1; then
      printf 'bench: ERROR: losetup -d %s failed; leaving loop attached\n' "$loop" | tee -a "$LOG"
      failed=1
    fi
  fi
  if [[ "$failed" -ne 0 ]]; then
    printf 'bench: lab teardown incomplete tag=%s loop=%s vg=%s dir=%s\n' \
      "$tag" "$loop" "$vg" "$dir" | tee -a "$LOG"
    return 1
  fi
  rm -rf "$dir"
  printf 'lab teardown tag=%s loop=%s vg=%s\n' "$tag" "$loop" "$vg" | tee -a "$LOG"
}

# write_environment <path>: record the exact host/toolchain identity.
write_environment() {
  local path="$1"
  local source_sha dirty
  source_sha="$(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unknown)"
  if [[ -n "$(git -C "$REPO" status --porcelain 2>/dev/null)" ]]; then
    dirty=yes
  else
    dirty=no
  fi
  {
    printf 'Volvisor benchmark environment\n'
    printf '==============================\n'
    printf 'Run window start: %s\n' "$(now)"
    printf 'Host: %s (single development host; not a multi-host lab)\n' "$(uname -n)"
    printf 'Runner: uid=%s gid=%s\n' "$(id -u)" "$(id -g)"
    printf '\nRun-time source identity\n------------------------\n'
    printf 'source_sha:      %s\n' "$source_sha"
    printf 'worktree_dirty:  %s\n' "$dirty"
    printf 'binary:          %s\n' "$BIN/volvisord"
    printf 'binary_sha256:   %s\n' "$(sha256sum "$BIN/volvisord" | awk '{print $1}')"
    printf 'binary_version:  %s\n' "$("$BIN/volvisord" --version 2>&1 | head -1)"
    printf 'build_profile:   release (cargo build --release --locked -p volvisord)\n'
    printf '\nOperating system and toolchain\n------------------------------\n'
    printf 'uname:   %s\n' "$(uname -a)"
    printf 'os:      %s\n' "$(head -2 /etc/os-release | tr '\n' ' ')"
    printf 'rustc:   %s\n' "$(rustc --version 2>&1 || echo absent)"
    printf 'cargo:   %s\n' "$(cargo --version 2>&1 || echo absent)"
    printf 'lvm:     %s\n' "$(lvm version 2>&1 | tr '\n' ' ')"
    printf 'jq:      %s\n' "$(jq --version 2>&1 || echo absent)"
    printf 'python3: %s\n' "$(python3 --version 2>&1 || echo absent)"
    printf 'curl:    %s\n' "$(curl --version 2>&1 | head -1 || echo absent)"
    printf 'losetup: %s\n' "$(losetup --version 2>&1 || echo absent)"
    printf 'CPUs:    %s\n' "$(nproc)"
    printf 'CPU model: %s\n' "$(awk -F: '/model name/ {gsub(/^ +/, "", $2); print $2; exit}' /proc/cpuinfo)"
    printf 'Memory:  %s\n' "$(awk '/MemTotal/ {printf "%.1f GiB\n", $2/1048576}' /proc/meminfo)"
    printf '\nDisk layout (read-only observation)\n-----------------------------------\n'
    lsblk -o NAME,TYPE,SIZE,SERIAL,WWN,MODEL 2>&1
    printf '\nPre-existing volume groups (read-only observation at start)\n-----------------------------------------------------------\n'
    vgs --noheadings -o vg_name,vg_size,pv_count 2>&1 || true
    printf '\nAbsent capabilities (blockers, not silent skips)\n------------------------------------------------\n'
    for cmd in drbdadm rbd ceph fio; do
      if command -v "$cmd" >/dev/null 2>&1; then
        printf '%s: present\n' "$cmd"
      else
        printf '%s: not found\n' "$cmd"
      fi
    done
    printf 'Second physical host: none. Every client and daemon runs on 127.0.0.1.\n'
    printf 'Real Ceph/DRBD/physical-disk LVM: not available on this host.\n'
    printf '\nLab construction\n----------------\n'
    printf -- '- Loop-backed LVM VG created only for this run; the VG name is\n'
    printf '  recorded in records/lab.vg and teardown names only that VG.\n'
    printf -- '- The LVM provider skips loop devices in discovery, so lvm-state.json\n'
    printf '  is pre-seeded with a lab-only operator device claim pointing at the\n'
    printf '  loop device. This is a lab scaffold, not a production claim path.\n'
    printf -- '- Journal and provider state live under the run directory on the same\n'
    printf '  ext4 filesystem as the loop image; every acknowledged append uses\n'
    printf '  the daemon implementation (fsync), never a mock.\n'
  } >"$path"
}

# write_checksums <dir>: MANIFEST.sha256 over every regular file.
write_checksums() {
  local dir="$1"
  (
    cd "$dir" || exit 1
    find . -type f ! -name MANIFEST.sha256 -print0 | sort -z | xargs -0 sha256sum
  ) >"$dir/MANIFEST.sha256"
}
