#!/usr/bin/env bash
# Day-2 readiness preflight for the issue #46 residuals.
#
# READ-ONLY by default: it inspects local tools, the git object store, kernel
# modules and (only when the corresponding env vars are set) SSH/HTTP
# reachability. It never creates a volume, starts a daemon, loads a kernel
# module, powers off a host, or writes an evidence bundle.
#
# Areas and env overrides:
#   D2 live cross-binary drill
#     OLD_COMMIT (default 8c2fc1a), REPO, plus local cargo/git/lvm/loop tools.
#     The old build itself is done by d2-live.sh; pass --check-old-build to
#     build it here in a disposable worktree.
#   D3 storage-side fencing
#     D3_PEER_HOST (or NEARLINE_PEER_HOST), D3_WITNESS_URL (or WITNESS_URL),
#     D3_DRBD_RESOURCE, D3_LOCAL_FAILURE_DOMAIN, D3_PEER_FAILURE_DOMAIN.
#     Requires a loaded DRBD 9 module and drbdadm locally. No peer means
#     NOT RUN, not a product failure.
#   D8 total host loss / re-admission
#     D8_HOSTS="a,b,c" (>=3 distinct), D8_WITNESS_URL, D8_BMC_TOOL
#     (default ipmitool), D8_BMC_HOST, D8_STORAGE_BACKEND=drbd|ceph,
#     D8_DRBD_VG or D8_CEPH_POOL, D8_ORACLE_HOST.
#     D8 remains non-executable from run-all.sh even when this says READY.
#
# Usage:
#   preflight.sh [--strict] [--only d2|d3|d8] [--check-old-build]
#
# Exit status: 0 by default even when a drill is NOT RUN or a check FAILs
# (this is a report; missing hardware is an environment fact, and the operator
# sees the FAIL line). With --strict, exit 1 unless every selected area is
# READY; for D2 that requires --check-old-build because an unverified old build
# is not READY. Usage errors exit 2.

set -uo pipefail
ARGS=("$@")

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="${REPO:-$(cd "$HERE/../../../.." && pwd)}"
OLD_COMMIT="${OLD_COMMIT:-8c2fc1a}"
SSH_CONNECT_TIMEOUT="${SSH_CONNECT_TIMEOUT:-5}"
CURL_TIMEOUT="${CURL_TIMEOUT:-3}"

STRICT=0
ONLY=""
CHECK_OLD_BUILD=0
RUN_WAS_SET=0
if [[ -n "${RUN:-}" ]]; then RUN_WAS_SET=1; else RUN="/tmp/opencode/volvisor-preflight-$(date -u +%Y%m%dT%H%M%SZ)"; fi
BUILD_WT=""
mkdir -p "$RUN"

usage() {
  cat >&2 <<EOF
Usage: $(basename "$0") [--strict] [--only d2|d3|d8] [--check-old-build]

Default: report PASS / NOT RUN / FAIL for all three areas and exit 0
         (including when an area reports FAIL; this is a report, not a gate).
--strict: exit 1 unless every selected area is READY (D2 also needs
          --check-old-build; an unverified old build is not READY).
--only: restrict checks to one area (default: all three).
--check-old-build: build OLD_COMMIT's volvisord in a disposable worktree
                    to verify the D2 live prerequisite now.

See the script header for the documented environment overrides.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --strict) STRICT=1 ;;
    --only)
      [[ $# -ge 2 ]] || { usage; exit 2; }
      ONLY="$2"
      shift
      ;;
    --only=*) ONLY="${1#*=}" ;;
    --check-old-build) CHECK_OLD_BUILD=1 ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage; exit 2 ;;
  esac
  shift
done

case "$ONLY" in
  ""|d2|d3|d8) ;;
  *) printf 'invalid --only value: %s\n' "$ONLY" >&2; usage; exit 2 ;;
esac

printf '# preflight start: %s\n' "$(date -u +%FT%TZ)"
printf '# invoke: %s' "$0"
if [[ "${#ARGS[@]}" -gt 0 ]]; then
  printf ' %q' "${ARGS[@]}"
fi
printf '\n'
printf '# repo: %s at %s (dirty=%s)\n' "$REPO" \
  "$(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo unknown)" \
  "$([[ -n "$(git -C "$REPO" status --porcelain 2>/dev/null)" ]] && echo yes || echo no)"
printf '# runner: uid=%s host=%s\n' "$(id -u)" "$(uname -n)"

# shellcheck disable=SC2317,SC2329 # invoked indirectly through `trap ... EXIT`
cleanup() {
  if [[ -n "$BUILD_WT" && -d "$BUILD_WT" ]]; then
    git -C "$REPO" worktree remove --force "$BUILD_WT" >/dev/null 2>&1 || true
    # Only prune when this run actually created a worktree; a read-only
    # preflight must not mutate repository metadata.
    git -C "$REPO" worktree prune >/dev/null 2>&1 || true
  fi
  if [[ "$RUN_WAS_SET" -eq 0 ]]; then
    rm -rf "$RUN"
  fi
}
trap cleanup EXIT

declare -A AREA_MISSING=([d2]=0 [d3]=0 [d8]=0)
declare -A AREA_FAIL=([d2]=0 [d3]=0 [d8]=0)
declare -A AREA_CHECKS=([d2]=0 [d3]=0 [d8]=0)
MISSING=0
FAILED=0

pass() {
  local area="$1" item="$2" detail="$3"
  AREA_CHECKS[$area]=$((AREA_CHECKS[$area] + 1))
  printf 'PASS      [%s] %s: %s\n' "$area" "$item" "$detail"
}

notrun() {
  local area="$1" item="$2" detail="$3"
  AREA_CHECKS[$area]=$((AREA_CHECKS[$area] + 1))
  AREA_MISSING[$area]=$((AREA_MISSING[$area] + 1))
  MISSING=$((MISSING + 1))
  printf 'NOT RUN   [%s] %s: %s\n' "$area" "$item" "$detail"
}

fail() {
  local area="$1" item="$2" detail="$3"
  AREA_CHECKS[$area]=$((AREA_CHECKS[$area] + 1))
  AREA_FAIL[$area]=$((AREA_FAIL[$area] + 1))
  MISSING=$((MISSING + 1))
  FAILED=$((FAILED + 1))
  printf 'FAIL      [%s] %s: %s\n' "$area" "$item" "$detail"
}

# NOT RUN but not counted as a missing prerequisite (optional verification).
note() {
  local area="$1" item="$2" detail="$3"
  printf 'NOT RUN   [%s] %s: %s\n' "$area" "$item" "$detail"
}

have() { command -v "$1" >/dev/null 2>&1; }

check_tool() {
  local area="$1" item="$2" cmd="$3"
  if have "$cmd"; then
    pass "$area" "$item" "$(command -v "$cmd")"
  else
    notrun "$area" "$item" "command not found: $cmd"
  fi
}

drbd_kernel_present() {
  [[ -e /sys/module/drbd || -e /proc/drbd ]]
}

check_drbd_kernel() {
  local area="$1"
  if drbd_kernel_present; then
    pass "$area" "DRBD 9 kernel module" "present (sysfs/procfs)"
  else
    notrun "$area" "DRBD 9 kernel module" "not loaded/present (no /sys/module/drbd or /proc/drbd)"
  fi
}

check_ssh() {
  local area="$1" label="$2" host="$3"
  if [[ -z "$host" ]]; then
    notrun "$area" "$label" "host is unset"
    return
  fi
  if ssh -o BatchMode=yes -o ConnectTimeout="$SSH_CONNECT_TIMEOUT" "$host" true >/dev/null 2>&1; then
    pass "$area" "$label" "SSH reachable: $host"
  else
    fail "$area" "$label" "configured host is not SSH-reachable: $host"
  fi
}

check_http_health() {
  local area="$1" label="$2" url="$3"
  if [[ -z "$url" ]]; then
    notrun "$area" "$label" "URL is unset"
    return
  fi
  case "$url" in
    http://*|https://*) ;;
    *) fail "$area" "$label" "URL must start with http:// or https://: $url"; return ;;
  esac
  if curl -fsS --max-time "$CURL_TIMEOUT" "${url%/}/healthz" >/dev/null 2>&1; then
    pass "$area" "$label" "healthz OK: ${url%/}"
  else
    fail "$area" "$label" "configured URL did not answer healthz: ${url%/}"
  fi
}

# ---------------------------------------------------------------------------
# D2 live cross-binary drill prerequisites.
# ---------------------------------------------------------------------------
check_d2() {
  printf '\n== D2 live old-binary -> new-binary upgrade/rollback ==\n'
  check_tool d2 "git" git
  check_tool d2 "cargo" cargo
  check_tool d2 "jq" jq
  check_tool d2 "curl" curl
  check_tool d2 "python3 (free-port helper)" python3
  check_tool d2 "sha256sum" sha256sum
  check_tool d2 "tar" tar
  check_tool d2 "losetup" losetup
  local tool
  for tool in vgs pvcreate vgcreate lvcreate lvs vgremove pvremove; do
    check_tool d2 "lvm2 $tool" "$tool"
  done

  if [[ "$(id -u)" -eq 0 ]]; then
    pass d2 "privilege" "uid 0 (loop/LVM lab can run)"
  else
    notrun d2 "privilege" "uid $(id -u); loop-backed LVM lab needs root"
  fi

  if git -C "$REPO" rev-parse --git-dir >/dev/null 2>&1; then
    pass d2 "repository" "$REPO at $(git -C "$REPO" rev-parse --short HEAD)"
  else
    notrun d2 "repository" "not a git worktree: $REPO"
  fi

  if git -C "$REPO" cat-file -e "${OLD_COMMIT}^{commit}" 2>/dev/null; then
    pass d2 "old commit resolves" "$OLD_COMMIT ($(git -C "$REPO" rev-parse --short=7 "$OLD_COMMIT"))"
    if git -C "$REPO" cat-file -e "${OLD_COMMIT}:Cargo.toml" 2>/dev/null \
      && git -C "$REPO" cat-file -e "${OLD_COMMIT}:Cargo.lock" 2>/dev/null; then
      pass d2 "old tree has Cargo.toml/Cargo.lock" "build inputs present"
    else
      fail d2 "old tree build inputs" "Cargo.toml or Cargo.lock missing at $OLD_COMMIT"
    fi
  else
    notrun d2 "old commit resolves" "git object $OLD_COMMIT is absent"
  fi

  if [[ -e /dev/loop-control ]] && losetup -f >/dev/null 2>&1; then
    pass d2 "loop device available" "losetup -f returned $(losetup -f)"
  else
    notrun d2 "loop device available" "no /dev/loop-control or no free loop device"
  fi

  if [[ "$CHECK_OLD_BUILD" -eq 1 ]]; then
    BUILD_WT="$RUN/old-build"
    if git -C "$REPO" worktree add --detach "$BUILD_WT" "$OLD_COMMIT" >/dev/null 2>&1; then
      if (cd "$BUILD_WT" && CARGO_TARGET_DIR="$RUN/target-old" cargo build --locked -p volvisord >/dev/null 2>&1); then
        pass d2 "old commit buildable" "cargo build -p volvisord succeeded at $OLD_COMMIT"
      else
        fail d2 "old commit buildable" "cargo build -p volvisord failed at $OLD_COMMIT"
      fi
      git -C "$REPO" worktree remove --force "$BUILD_WT" >/dev/null 2>&1 || true
      BUILD_WT=""
    else
      fail d2 "old commit buildable" "could not create a disposable worktree at $OLD_COMMIT"
    fi
  else
    if [[ "$STRICT" -eq 1 ]]; then
      # --strict promises READY, and an unverified old build is not READY.
      notrun d2 "old commit buildable" "not verified; rerun with --check-old-build or drop --strict"
    else
      note d2 "old commit buildable" "not built by preflight; d2-live.sh builds it (use --check-old-build to verify now)"
    fi
  fi
}

# ---------------------------------------------------------------------------
# D3 storage-side fencing prerequisites.
# ---------------------------------------------------------------------------
check_d3() {
  printf '\n== D3 storage-side lost-witness fencing ==\n'
  check_drbd_kernel d3
  check_tool d3 "drbdadm" drbdadm

  local peer="${D3_PEER_HOST:-${NEARLINE_PEER_HOST:-}}"
  local witness="${D3_WITNESS_URL:-${WITNESS_URL:-}}"
  local resource="${D3_DRBD_RESOURCE:-}"
  local local_fd="${D3_LOCAL_FAILURE_DOMAIN:-}"
  local peer_fd="${D3_PEER_FAILURE_DOMAIN:-}"

  check_ssh d3 "peer host" "$peer"
  check_http_health d3 "witness service" "$witness"

  if [[ -n "$resource" ]]; then
    pass d3 "DRBD resource name" "$resource"
  else
    notrun d3 "DRBD resource name" "D3_DRBD_RESOURCE is unset"
  fi

  if [[ -n "$local_fd" && -n "$peer_fd" ]]; then
    if [[ "$local_fd" == "$peer_fd" ]]; then
      fail d3 "distinct failure domains" "local and peer both report '$local_fd'"
    else
      pass d3 "distinct failure domains" "$local_fd / $peer_fd"
    fi
  else
    notrun d3 "distinct failure domains" "D3_LOCAL_FAILURE_DOMAIN/D3_PEER_FAILURE_DOMAIN are unset"
  fi
}

# ---------------------------------------------------------------------------
# D8 total host loss / re-admission prerequisites (never executed here).
# ---------------------------------------------------------------------------
check_d8() {
  printf '\n== D8 total host loss and re-admission ==\n'
  local hosts="${D8_HOSTS:-}"
  if [[ -z "$hosts" ]]; then
    notrun d8 "three or more data hosts" "D8_HOSTS is unset (expected e.g. a,b,c)"
  else
    IFS=',' read -r -a host_list <<<"$hosts"
    local distinct
    distinct="$(printf '%s\n' "${host_list[@]}" | sed '/^$/d' | sort -u | wc -l | tr -d ' ')"
    if [[ "$distinct" -ge 3 ]]; then
      pass d8 "three or more data hosts" "$distinct distinct hosts configured"
    else
      notrun d8 "three or more data hosts" "$distinct distinct host(s); need at least 3"
    fi
    local host
    for host in "${host_list[@]}"; do
      [[ -n "$host" ]] || continue
      check_ssh d8 "data host $host" "$host"
    done
  fi

  check_http_health d8 "witness service" "${D8_WITNESS_URL:-}"
  check_ssh d8 "independent oracle host" "${D8_ORACLE_HOST:-}"

  local bmc_tool="${D8_BMC_TOOL:-ipmitool}"
  local bmc_host="${D8_BMC_HOST:-}"
  if [[ -n "$bmc_host" ]] && have "$bmc_tool"; then
    # Configuration alone is not a verified fencing prerequisite: preflight
    # is read-only and does not probe or authenticate to the BMC.
    notrun d8 "BMC/power control" "$bmc_tool configured for $bmc_host (configuration only; not probed)"
  elif [[ -z "$bmc_host" ]]; then
    notrun d8 "BMC/power control" "D8_BMC_HOST is unset (D8_BMC_TOOL=$bmc_tool)"
  else
    notrun d8 "BMC/power control" "D8_BMC_TOOL not found: $bmc_tool"
  fi

  local backend="${D8_STORAGE_BACKEND:-}"
  case "$backend" in
    drbd)
      check_drbd_kernel d8
      if [[ -n "${D8_DRBD_VG:-}" ]]; then
        pass d8 "shared storage assumption" "DRBD volume group: $D8_DRBD_VG"
      else
        notrun d8 "shared storage assumption" "D8_DRBD_VG is unset"
      fi
      ;;
    ceph)
      check_tool d8 "ceph CLI" ceph
      check_tool d8 "rbd CLI" rbd
      if [[ -n "${D8_CEPH_POOL:-}" ]]; then
        pass d8 "shared storage assumption" "Ceph pool: $D8_CEPH_POOL"
      else
        notrun d8 "shared storage assumption" "D8_CEPH_POOL is unset"
      fi
      ;;
    *)
      notrun d8 "storage backend" "D8_STORAGE_BACKEND must be drbd or ceph (got: ${backend:-unset})"
      check_drbd_kernel d8
      ;;
  esac
}

case "$ONLY" in
  d2) check_d2 ;;
  d3) check_d3 ;;
  d8) check_d8 ;;
  "") check_d2; check_d3; check_d8 ;;
esac

area_verdict() {
  local area="$1"
  if [[ "${AREA_FAIL[$area]}" -gt 0 ]]; then
    printf 'FAIL'
  elif [[ "${AREA_MISSING[$area]}" -gt 0 ]]; then
    printf 'NOT RUN'
  else
    printf 'READY'
  fi
}

selected=()
case "$ONLY" in
  d2) selected=(d2) ;;
  d3) selected=(d3) ;;
  d8) selected=(d8) ;;
  "") selected=(d2 d3 d8) ;;
esac

overall="READY"
summary=()
for area in "${selected[@]}"; do
  verdict="$(area_verdict "$area")"
  case "$area" in
    d2) label="D2 live" ;;
    d3) label="D3 storage-side" ;;
    d8) label="D8" ;;
  esac
  summary+=("$label: $verdict")
  if [[ "$verdict" == "FAIL" ]]; then
    overall="FAIL"
  elif [[ "$verdict" == "NOT RUN" && "$overall" != "FAIL" ]]; then
    overall="NOT RUN"
  fi
done

printf '\n== summary ==\n'
printf '%s\n' "${summary[@]}"
printf 'checks: %s selected; %s missing prerequisite(s); %s failed check(s)\n' \
  "$((AREA_CHECKS[d2] + AREA_CHECKS[d3] + AREA_CHECKS[d8]))" "$MISSING" "$FAILED"

joined="$(printf '%s; ' "${summary[@]}")"
joined="${joined%; }"
if [[ "$overall" == "READY" ]]; then
  printf 'VERDICT: READY (%s)\n' "$joined"
elif [[ "$overall" == "FAIL" ]]; then
  printf 'VERDICT: FAIL (%s)\n' "$joined"
else
  printf 'VERDICT: NOT RUN (%s)\n' "$joined"
fi
printf '%s\n' "A preflight result is not drill evidence. D2 live must still run d2-live.sh; D3/D8 remain NOT RUN without the listed hardware."

if [[ "$STRICT" -eq 1 && "$overall" != "READY" ]]; then
  exit 1
fi
exit 0
