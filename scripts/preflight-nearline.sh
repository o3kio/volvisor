#!/usr/bin/env bash
# Volvisor nearline multi-host campaign preflight (issue #37).
#
# READ-ONLY. This script validates an operator inventory and the three
# physical data/oracle hosts plus the fourth-failure-domain witness. It
# never starts a campaign, creates an evidence bundle, claims a writer
# lease, promotes a DRBD resource, or writes to a volume.
#
# Usage:
#   scripts/preflight-nearline.sh /etc/volvisor/nearline-inventory.env
#
# The inventory is a documented KEY=VALUE file (see
# scripts/nearline-inventory.example.env). It is parsed, NOT sourced, so a
# malformed line or a value containing shell syntax is a loud refusal rather
# than code execution. Keep the real file outside the repository and mode 0600.
#
# Exit codes: 0 = preflight passed, 1 = preflight failed, 2 = usage/inventory
# error (including a missing or malformed inventory; nothing is contacted).
#
# This script does not require root for the failure path: inventory and local
# tool validation happen before any ssh/curl call. Success-path checks use
# read-only remote commands; DRBD/LVM configuration checks may require root
# on the target host.

set -uo pipefail
IFS=$'\n\t'

readonly SCRIPT_NAME="${0##*/}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
readonly REPO_ROOT

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 2
}

usage() {
  cat >&2 <<EOF
Usage: $SCRIPT_NAME <inventory-file>

The inventory file is a KEY=VALUE file. Start from:
  $REPO_ROOT/scripts/nearline-inventory.example.env

It must describe three physical hosts (A/B data, C independent campaign
oracle) in distinct failure domains and a witness on a fourth failure
domain. No real addresses or secrets are committed to this repository.
EOF
}

[[ $# -eq 1 ]] || { usage; exit 2; }
[[ "$1" != "--help" && "$1" != "-h" ]] || { usage; exit 0; }

INVENTORY="$1"
[[ -n "$INVENTORY" ]] || die "empty inventory path"
[[ -f "$INVENTORY" ]] || die "inventory file not found: $INVENTORY
Create it from scripts/nearline-inventory.example.env; do not commit real addresses or secrets."
[[ -r "$INVENTORY" ]] || die "inventory file is not readable: $INVENTORY"

declare -A INV=()
INV_LINE=0

load_inventory() {
  local file="$1" line key value
  while IFS= read -r line || [[ -n "$line" ]]; do
    INV_LINE=$((INV_LINE + 1))
    line="${line%$'\r'}"
    [[ "$line" =~ ^[[:space:]]*$ ]] && continue
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    if [[ ! "$line" =~ ^[[:space:]]*([A-Z][A-Z0-9_]*)[[:space:]]*=(.*)$ ]]; then
      die "malformed inventory line $INV_LINE in $file: expected KEY=VALUE (got: ${line:0:80})"
    fi
    key="${BASH_REMATCH[1]}"
    value="${BASH_REMATCH[2]}"
    # Trim leading/trailing whitespace.
    value="${value#"${value%%[![:space:]]*}"}"
    value="${value%"${value##*[![:space:]]}"}"
    # Allow one layer of matching quotes; values are never evaluated.
    if [[ "$value" =~ ^\"(.*)\"$ ]]; then
      value="${BASH_REMATCH[1]}"
    elif [[ "$value" =~ ^\'(.*)\'$ ]]; then
      value="${BASH_REMATCH[1]}"
    fi
    INV["$key"]="$value"
  done <"$file"
}

get() { printf '%s' "${INV[$1]:-}"; }

require_keys() {
  local key
  for key in "$@"; do
    [[ -n "${INV[$key]:-}" ]] || die "missing required inventory key: $key
See scripts/nearline-inventory.example.env for the documented shape."
  done
}

load_inventory "$INVENTORY"

require_keys \
  CAMPAIGN_ID \
  HOST_A_SSH HOST_B_SSH HOST_C_SSH WITNESS_SSH \
  HOST_A_FAILURE_DOMAIN HOST_B_FAILURE_DOMAIN HOST_C_FAILURE_DOMAIN \
  WITNESS_FAILURE_DOMAIN \
  HOST_A_DRBD_NODE HOST_B_DRBD_NODE HOST_C_DRBD_NODE \
  HOST_A_REPL_ADDRESS HOST_B_REPL_ADDRESS DRBD_PORT \
  WITNESS_DRBD_NODE WITNESS_ADDRESS WITNESS_PORT \
  WITNESS_URL WITNESS_STATE_DIR \
  HOST_A_CONFIG HOST_B_CONFIG WITNESS_CONFIG \
  HOST_C_EVIDENCE_DIR JOURNAL_DIR SNAPSHOT_DIR DRBD_VG \
  VOLVISORD_BIN WITNESSD_BIN

# Refuse the committed template before any ssh/curl call. A copied-but-not-
# edited template is not a real inventory.
for key in "${!INV[@]}"; do
  case "${INV[$key]}" in
    *REPLACE_*|*.invalid)
      die "inventory key $key still holds a template placeholder: ${INV[$key]}
Edit a copy outside the repository; the committed example is never runnable."
      ;;
  esac
done

# Materialize every value used below. set -u turns an unassigned key into a
# loud failure instead of an empty path silently testing the wrong thing.
DRBD_PORT="$(get DRBD_PORT)"
DRBD_VG="$(get DRBD_VG)"
JOURNAL_DIR="$(get JOURNAL_DIR)"
SNAPSHOT_DIR="$(get SNAPSHOT_DIR)"
HOST_C_EVIDENCE_DIR="$(get HOST_C_EVIDENCE_DIR)"
WITNESS_URL="$(get WITNESS_URL)"
WITNESS_ADDRESS="$(get WITNESS_ADDRESS)"
WITNESS_PORT="$(get WITNESS_PORT)"
WITNESS_STATE_DIR="$(get WITNESS_STATE_DIR)"

MIN_FREE_GIB="$(get MIN_FREE_GIB)"; MIN_FREE_GIB="${MIN_FREE_GIB:-50}"
MAX_CLOCK_SKEW_SECS="$(get MAX_CLOCK_SKEW_SECS)"; MAX_CLOCK_SKEW_SECS="${MAX_CLOCK_SKEW_SECS:-5}"
SSH_CONNECT_TIMEOUT="$(get SSH_CONNECT_TIMEOUT)"; SSH_CONNECT_TIMEOUT="${SSH_CONNECT_TIMEOUT:-5}"

[[ "$MIN_FREE_GIB" =~ ^[0-9]+$ ]] || die "MIN_FREE_GIB must be an integer, got: $MIN_FREE_GIB"
[[ "$MAX_CLOCK_SKEW_SECS" =~ ^[0-9]+$ ]] || die "MAX_CLOCK_SKEW_SECS must be an integer, got: $MAX_CLOCK_SKEW_SECS"
[[ "$DRBD_PORT" =~ ^[0-9]+$ ]] || die "DRBD_PORT must be an integer, got: $DRBD_PORT"

# ---------------------------------------------------------------------------
# Topology/failure-domain validation. These are refusals, not warnings: the
# campaign's claim depends on distinct failure domains.
# ---------------------------------------------------------------------------

WITNESS_URL="${WITNESS_URL%/}"
case "$WITNESS_URL" in
  http://*|https://*) ;;
  *) die "WITNESS_URL must start with http:// or https:// (got: $WITNESS_URL)" ;;
esac

check_distinct() {
  local label="$1" a="$2" b="$3" c="$4"
  if [[ "$a" == "$b" || "$a" == "$c" || "$b" == "$c" ]]; then
    bad "failure domains for $label are not distinct: '$a', '$b', '$c'"
  else
    ok "failure domains for $label are distinct: $a / $b / $c"
  fi
}

check_witness_distinct() {
  local w="$1" fd
  for fd in "$2" "$3" "$4"; do
    if [[ "$w" == "$fd" ]]; then
      bad "witness failure domain '$w' equals a data/oracle host failure domain"
      return
    fi
  done
  ok "witness failure domain '$w' is distinct from A/B/C"
}

FD_A="$(get HOST_A_FAILURE_DOMAIN)"
FD_B="$(get HOST_B_FAILURE_DOMAIN)"
FD_C="$(get HOST_C_FAILURE_DOMAIN)"
FD_W="$(get WITNESS_FAILURE_DOMAIN)"
SSH_A="$(get HOST_A_SSH)"
SSH_B="$(get HOST_B_SSH)"
SSH_C="$(get HOST_C_SSH)"
SSH_W="$(get WITNESS_SSH)"
NODE_A="$(get HOST_A_DRBD_NODE)"
NODE_B="$(get HOST_B_DRBD_NODE)"
NODE_C="$(get HOST_C_DRBD_NODE)"

[[ "$SSH_A" != "$SSH_B" && "$SSH_A" != "$SSH_C" && "$SSH_B" != "$SSH_C" ]] \
  || die "HOST_A_SSH/HOST_B_SSH/HOST_C_SSH must be three distinct targets"
[[ "$SSH_W" != "$SSH_A" && "$SSH_W" != "$SSH_B" && "$SSH_W" != "$SSH_C" ]] \
  || die "WITNESS_SSH must not be one of the data/oracle hosts (fourth failure domain)"
[[ "$NODE_A" != "$NODE_B" && "$NODE_A" != "$NODE_C" && "$NODE_B" != "$NODE_C" ]] \
  || die "HOST_A/B/C_DRBD_NODE must be three distinct node names"
NODE_W="$(get WITNESS_DRBD_NODE)"
[[ "$NODE_W" != "$NODE_A" && "$NODE_W" != "$NODE_B" && "$NODE_W" != "$NODE_C" ]] \
  || die "WITNESS_DRBD_NODE must not be one of the data/oracle node names"

# ---------------------------------------------------------------------------
# Results and helpers.
# ---------------------------------------------------------------------------

FAILURES=0
WARNINGS=0

ok()   { printf 'PASS  %s\n' "$*"; }
bad()  { printf 'FAIL  %s\n' "$*" >&2; FAILURES=$((FAILURES + 1)); }
warn() { printf 'WARN  %s\n' "$*" >&2; WARNINGS=$((WARNINGS + 1)); }
info() { printf 'INFO  %s\n' "$*"; }

SSH_BASE=(ssh -o BatchMode=yes -o ConnectTimeout="$SSH_CONNECT_TIMEOUT" -o StrictHostKeyChecking=accept-new)
if [[ -n "$(get SSH_IDENTITY_FILE)" ]]; then
  SSH_BASE+=(-i "$(get SSH_IDENTITY_FILE)")
fi

remote_sh() {
  local target="$1" script="$2"
  "${SSH_BASE[@]}" "$target" bash -c "$script"
}

abs_diff() {
  local a="$1" b="$2"
  if ((a >= b)); then printf '%s' "$((a - b))"; else printf '%s' "$((b - a))"; fi
}

check_distinct "hosts A/B/C" "$FD_A" "$FD_B" "$FD_C"
check_witness_distinct "$FD_W" "$FD_A" "$FD_B" "$FD_C"

# ---------------------------------------------------------------------------
# Local tool validation (still before any host contact).
# ---------------------------------------------------------------------------

for tool in ssh curl awk date df; do
  if command -v "$tool" >/dev/null 2>&1; then
    ok "local tool present: $tool"
  else
    bad "local tool missing: $tool (install it before running the campaign)"
  fi
done

((FAILURES == 0)) || {
  printf '\nPREFLIGHT FAIL: fix the inventory/local tools above. No host was contacted and no evidence was produced.\n' >&2
  exit 1
}

# ---------------------------------------------------------------------------
# Reachability.
# ---------------------------------------------------------------------------

declare -A REACHABLE=()
for role in A B C; do
  target_var="SSH_$role"
  target="${!target_var}"
  if remote_sh "$target" 'true' >/dev/null 2>&1; then
    REACHABLE["$role"]=1
    ok "ssh reachable: host $role ($target)"
  else
    REACHABLE["$role"]=0
    bad "ssh unreachable: host $role ($target). Check network, keys, BatchMode, and SSH_IDENTITY_FILE."
  fi
done
if remote_sh "$SSH_W" 'true' >/dev/null 2>&1; then
  REACHABLE[W]=1
  ok "ssh reachable: witness ($SSH_W)"
else
  REACHABLE[W]=0
  bad "ssh unreachable: witness ($SSH_W). The witness must be a fourth failure domain, not a data host."
fi

# ---------------------------------------------------------------------------
# Host identity, clocks, DRBD/tooling, disk, and configured paths.
# ---------------------------------------------------------------------------

declare -A SEEN_MACHINE_ID=()
declare -A SEEN_PRODUCT_UUID=()
declare -A SEEN_HOSTNAME=()

check_identity() {
  local role="$1" target="$2" expected_node="$3"
  local out host machine_id product_uuid
  # shellcheck disable=SC2016 # The single-quoted script must expand on the remote host.
  out="$(remote_sh "$target" 'printf "%s\t%s\t%s\n" "$(uname -n)" "$(cat /etc/machine-id 2>/dev/null || echo missing)" "$(cat /sys/class/dmi/id/product_uuid 2>/dev/null || echo missing)"')" || {
    bad "host $role: identity query failed"
    return
  }
  IFS=$'\t' read -r host machine_id product_uuid <<<"$out"
  [[ -n "$host" ]] || { bad "host $role: empty uname -n"; return; }
  if [[ "$host" != "$expected_node" ]]; then
    bad "host $role: uname -n '$host' does not match inventory node name '$expected_node'"
  else
    ok "host $role: uname -n matches DRBD node name ($host)"
  fi
  if [[ -n "${SEEN_HOSTNAME[$host]:-}" ]]; then
    bad "host $role: duplicate host identity '$host' (also host ${SEEN_HOSTNAME[$host]})"
  else
    SEEN_HOSTNAME["$host"]="$role"
  fi
  if [[ "$machine_id" != "missing" && -n "$machine_id" ]]; then
    if [[ -n "${SEEN_MACHINE_ID[$machine_id]:-}" ]]; then
      bad "host $role: duplicate machine-id (also host ${SEEN_MACHINE_ID[$machine_id]})"
    else
      SEEN_MACHINE_ID["$machine_id"]="$role"
    fi
  fi
  if [[ "$product_uuid" != "missing" && -n "$product_uuid" ]]; then
    if [[ -n "${SEEN_PRODUCT_UUID[$product_uuid]:-}" ]]; then
      bad "host $role: duplicate DMI product_uuid (also host ${SEEN_PRODUCT_UUID[$product_uuid]})"
    else
      SEEN_PRODUCT_UUID["$product_uuid"]="$role"
    fi
  fi
  if [[ ( "$machine_id" == "missing" || -z "$machine_id" ) && ( "$product_uuid" == "missing" || -z "$product_uuid" ) ]]; then
    warn "host $role: neither /etc/machine-id nor DMI product_uuid is readable; distinct identity rests on hostname alone"
  fi
}

check_clock() {
  local role="$1" target="$2" remote_epoch local_epoch skew
  remote_epoch="$(remote_sh "$target" 'date +%s')" || { bad "$role: clock query failed"; return; }
  local_epoch="$(date +%s)"
  [[ "$remote_epoch" =~ ^[0-9]+$ ]] || { bad "$role: non-numeric remote clock: $remote_epoch"; return; }
  skew="$(abs_diff "$remote_epoch" "$local_epoch")"
  if ((skew > MAX_CLOCK_SKEW_SECS)); then
    bad "$role: clock skew ${skew}s exceeds MAX_CLOCK_SKEW_SECS=${MAX_CLOCK_SKEW_SECS}s (fix NTP/chrony before the campaign)"
  else
    ok "$role: clock skew ${skew}s within ${MAX_CLOCK_SKEW_SECS}s"
  fi
}

check_disk_path() {
  local role="$1" target="$2" path="$3" kind="$4" avail_kb min_kb
  if ! remote_sh "$target" "test -d '$path'" >/dev/null 2>&1; then
    bad "$role: $kind path does not exist: $path (create it before the campaign; do not point at production data)"
    return
  fi
  if ! remote_sh "$target" "test -w '$path'" >/dev/null 2>&1; then
    bad "$role: $kind path is not writable by the preflight account: $path (run as root, or grant write access)"
    return
  fi
  avail_kb="$(remote_sh "$target" "df -Pk -- '$path' 2>/dev/null | awk 'NR==2 {print \$4}'")" || {
    bad "$role: df failed for $path"; return
  }
  [[ "$avail_kb" =~ ^[0-9]+$ ]] || { bad "$role: df returned no numeric free space for $path"; return; }
  min_kb=$((MIN_FREE_GIB * 1024 * 1024))
  if ((avail_kb < min_kb)); then
    bad "$role: $kind path $path has $((avail_kb / 1024 / 1024)) GiB free, below MIN_FREE_GIB=$MIN_FREE_GIB"
  else
    ok "$role: $kind path $path has $((avail_kb / 1024 / 1024)) GiB free"
  fi
}

check_drbd_host() {
  local role="$1" target="$2"
  if remote_sh "$target" 'command -v drbdadm >/dev/null 2>&1 && command -v drbdsetup >/dev/null 2>&1 && test -r /proc/drbd' >/dev/null 2>&1; then
    ok "host $role: drbdadm/drbdsetup present and /proc/drbd readable"
  else
    bad "host $role: DRBD 9 tooling/module missing (need drbdadm, drbdsetup, and a loaded drbd module exposing /proc/drbd)"
    return
  fi
  local version
  version="$(remote_sh "$target" 'drbdadm --version 2>&1 | head -n1')" || {
    bad "host $role: drbdadm --version failed"; return
  }
  if [[ "$version" == *9* ]]; then
    ok "host $role: DRBD 9 reported ($version)"
  else
    bad "host $role: DRBD 9 is required; drbdadm --version answered: $version"
  fi
  if ! remote_sh "$target" 'command -v lvs >/dev/null 2>&1 && command -v vgs >/dev/null 2>&1' >/dev/null 2>&1; then
    bad "host $role: LVM tooling missing (lvs/vgs are required for the nearline backing volume group)"
    return
  fi
  local vg_free
  vg_free="$(remote_sh "$target" "vgs --noheadings --units g --nosuffix -o vg_free '$DRBD_VG' 2>/dev/null | awk 'NF {print \$1; exit}'")" || true
  if [[ ! "$vg_free" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
    bad "host $role: volume group '$DRBD_VG' is not readable by the preflight account (run as root or grant LVM read access)"
  elif ! awk -v free="$vg_free" -v min="$MIN_FREE_GIB" 'BEGIN { exit !(free + 0 >= min + 0) }'; then
    bad "host $role: volume group '$DRBD_VG' has ${vg_free} GiB free, below MIN_FREE_GIB=$MIN_FREE_GIB"
  else
    ok "host $role: volume group '$DRBD_VG' has ${vg_free} GiB free"
  fi
}

check_tcp() {
  local from_role="$1" from="$2" to_role="$3" to="$4" port="$5"
  if remote_sh "$from" "timeout 5 bash -c '</dev/tcp/$to/$port'" >/dev/null 2>&1; then
    ok "network: $from_role -> $to_role replication address $to:$port reachable"
  else
    bad "network: $from_role -> $to_role replication address $to:$port unreachable (check firewall/routes; DRBD cannot replicate otherwise)"
  fi
}

check_config() {
  local role="$1" target="$2" bin="$3" config="$4"
  if remote_sh "$target" "command -v '$bin' >/dev/null 2>&1 && '$bin' --check-config '$config' >/dev/null 2>&1" >/dev/null 2>&1; then
    ok "$role: $bin --check-config passed for $config"
  else
    bad "$role: $bin --check-config failed for $config (run it as root on the host and read the typed error)"
  fi
}

for role in A B; do
  if [[ "${REACHABLE[$role]:-0}" == 1 ]]; then
    case "$role" in
      A) target="$SSH_A"; node="$NODE_A" ;;
      B) target="$SSH_B"; node="$NODE_B" ;;
    esac
    check_identity "$role" "$target" "$node"
    check_clock "$role" "$target"
    check_drbd_host "$role" "$target"
    check_disk_path "$role" "$target" "$JOURNAL_DIR" "journal"
    check_disk_path "$role" "$target" "$SNAPSHOT_DIR" "shared snapshot"
    check_config "$role" "$target" "$(get VOLVISORD_BIN)" "$(get HOST_${role}_CONFIG)"
    # Campaign/VMM tooling used by the runbook (not by the daemon itself).
    if remote_sh "$target" 'command -v fio >/dev/null 2>&1 && command -v jq >/dev/null 2>&1 && command -v curl >/dev/null 2>&1 && command -v cloud-hypervisor >/dev/null 2>&1 && command -v ch-remote >/dev/null 2>&1' >/dev/null 2>&1; then
      ok "host $role: campaign/VMM tooling present (fio, jq, curl, cloud-hypervisor, ch-remote)"
    else
      bad "host $role: campaign/VMM tooling missing (need fio, jq, curl, cloud-hypervisor, ch-remote)"
    fi
  fi
done

if [[ "${REACHABLE[C]:-0}" == 1 ]]; then
  check_identity C "$SSH_C" "$NODE_C"
  check_clock C "$SSH_C"
  check_disk_path C "$SSH_C" "$HOST_C_EVIDENCE_DIR" "campaign evidence/run"
  if remote_sh "$SSH_C" 'command -v fio >/dev/null 2>&1 && command -v jq >/dev/null 2>&1 && command -v curl >/dev/null 2>&1' >/dev/null 2>&1; then
    ok "host C: independent campaign/oracle tooling present (fio, jq, curl)"
  else
    bad "host C: campaign/oracle tooling missing (need fio, jq, curl). Host C is the independent oracle, not a data replica."
  fi
fi

if [[ "${REACHABLE[W]:-0}" == 1 ]]; then
  check_identity W "$SSH_W" "$NODE_W"
  check_clock W "$SSH_W"
  check_disk_path W "$SSH_W" "$WITNESS_STATE_DIR" "witness state"
  check_config W "$SSH_W" "$(get WITNESSD_BIN)" "$(get WITNESS_CONFIG)"
fi

if command -v curl >/dev/null 2>&1; then
  health="$(curl -fsS --max-time 5 "$WITNESS_URL/healthz" 2>/dev/null || true)"
  if [[ "$health" == "ok" ]]; then
    ok "witness health: $WITNESS_URL/healthz answered ok"
  else
    bad "witness health: $WITNESS_URL/healthz did not answer 'ok' (start volvisor-witnessd and check listen/token/firewall)"
  fi
fi

if [[ "${REACHABLE[A]:-0}" == 1 && "${REACHABLE[B]:-0}" == 1 ]]; then
  check_tcp A "$SSH_A" B "$(get HOST_B_REPL_ADDRESS)" "$DRBD_PORT"
  check_tcp B "$SSH_B" A "$(get HOST_A_REPL_ADDRESS)" "$DRBD_PORT"
fi
if [[ "${REACHABLE[A]:-0}" == 1 && "${REACHABLE[W]:-0}" == 1 ]]; then
  check_tcp A "$SSH_A" W "$WITNESS_ADDRESS" "$WITNESS_PORT"
fi
if [[ "${REACHABLE[B]:-0}" == 1 && "${REACHABLE[W]:-0}" == 1 ]]; then
  check_tcp B "$SSH_B" W "$WITNESS_ADDRESS" "$WITNESS_PORT"
fi

printf '\n'
if ((FAILURES > 0)); then
  printf 'PREFLIGHT FAIL: %d check(s) failed, %d warning(s).\n' "$FAILURES" "$WARNINGS" >&2
  printf 'Fix every FAIL before running the #37 campaign. No host was mutated and no evidence was produced.\n' >&2
  exit 1
fi
printf 'PREFLIGHT PASS: %d warning(s). The environment shape is eligible for the #37 campaign.\n' "$WARNINGS"
printf 'This preflight produced NO campaign evidence. Running the campaign is a separate, operator-led act.\n'
