#!/usr/bin/env bash
# Volvisor three-physical-host Rook-cell POC preflight (issue #38).
#
# READ-ONLY. Validates an operator inventory and the three physical hosts,
# their Kubernetes cell Nodes, Rook tooling/operator readiness, VFIO/NVMe
# isolation and the evidence collector. It never applies a manifest, creates
# an OSD, formats a disk, claims a device or writes an evidence bundle.
#
# Usage:
#   scripts/preflight-rook.sh /etc/volvisor/rook-cell-inventory.env
#   scripts/preflight-rook.sh --require-ceph /etc/volvisor/rook-cell-inventory.env
#
# --require-ceph additionally requires the CephCluster named by the inventory
# to exist and be reachable; use it for the post-bring-up preflight, not the
# first one.
#
# The inventory is a documented KEY=VALUE file (see
# scripts/rook-cell-inventory.example.env), parsed and NOT sourced. Keep the
# real file outside the repository (mode 0600); never commit real addresses,
# kubeconfigs, tokens or screenshots.
#
# Exit codes: 0 = preflight passed, 1 = preflight failed, 2 = usage/inventory
# error (including a missing or malformed inventory; nothing is contacted).
#
# This script does not require root for the failure path: inventory and local
# tool validation happen before any ssh/kubectl/helm call. Success-path host
# checks use read-only commands.

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
Usage: $SCRIPT_NAME [--require-ceph] <inventory-file>

The inventory file is a KEY=VALUE file. Start from:
  $REPO_ROOT/scripts/rook-cell-inventory.example.env

It must describe three physical hosts in distinct failure domains, three
dedicated Kubernetes cell Nodes (one per host), Rook tooling and a writable
evidence root OUTSIDE the repository. No real addresses, kubeconfigs or
secrets are committed to this repository.
EOF
}

REQUIRE_CEPH=0
if [[ $# -gt 0 && "$1" == "--require-ceph" ]]; then
  REQUIRE_CEPH=1
  shift
fi
[[ $# -eq 1 ]] || { usage; exit 2; }
[[ "$1" != "--help" && "$1" != "-h" ]] || { usage; exit 0; }

INVENTORY="$1"
[[ -n "$INVENTORY" ]] || die "empty inventory path"
[[ -f "$INVENTORY" ]] || die "inventory file not found: $INVENTORY
Create it from scripts/rook-cell-inventory.example.env; do not commit real addresses, kubeconfigs or secrets."
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
    value="${value#"${value%%[![:space:]]*}"}"
    value="${value%"${value##*[![:space:]]}"}"
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
See scripts/rook-cell-inventory.example.env for the documented shape."
  done
}

load_inventory "$INVENTORY"

require_keys \
  CAMPAIGN_ID \
  HOST_A_SSH HOST_B_SSH HOST_C_SSH \
  HOST_A_FAILURE_DOMAIN HOST_B_FAILURE_DOMAIN HOST_C_FAILURE_DOMAIN \
  CELL_NODE_A CELL_NODE_B CELL_NODE_C \
  CELL_ROLE_LABEL CELL_ROLE_VALUE CELL_FD_LABEL \
  CELL_TAINT_KEY CELL_TAINT_VALUE CELL_TAINT_EFFECT \
  ALLOWED_CELL_NAMESPACES \
  KUBE_CONTEXT ROOK_NAMESPACE ROOK_OPERATOR_NAMESPACE \
  CEPHCLUSTER_NAME CEPHBLOCKPOOL_NAME \
  HOST_A_VFIO_BDF HOST_B_VFIO_BDF HOST_C_VFIO_BDF \
  EVIDENCE_ROOT \
  VOLVISOR_COLLECT_EVIDENCE

# Refuse the committed template before any ssh/kubectl/helm call. A copied-
# but-not-edited template is not a real inventory.
for key in "${!INV[@]}"; do
  case "${INV[$key]}" in
    *REPLACE_*|*.invalid)
      die "inventory key $key still holds a template placeholder: ${INV[$key]}
Edit a copy outside the repository; the committed example is never runnable."
      ;;
  esac
done

# Materialize every value used below. set -u turns an unassigned key into a
# loud failure instead of an empty selector or namespace matching everything.
KUBE_CONTEXT="$(get KUBE_CONTEXT)"
ROOK_NAMESPACE="$(get ROOK_NAMESPACE)"
ROOK_OPERATOR_NAMESPACE="$(get ROOK_OPERATOR_NAMESPACE)"
CEPHCLUSTER_NAME="$(get CEPHCLUSTER_NAME)"
CEPHBLOCKPOOL_NAME="$(get CEPHBLOCKPOOL_NAME)"
CELL_ROLE_LABEL="$(get CELL_ROLE_LABEL)"
CELL_ROLE_VALUE="$(get CELL_ROLE_VALUE)"
CELL_FD_LABEL="$(get CELL_FD_LABEL)"
CELL_TAINT_KEY="$(get CELL_TAINT_KEY)"
CELL_TAINT_VALUE="$(get CELL_TAINT_VALUE)"
CELL_TAINT_EFFECT="$(get CELL_TAINT_EFFECT)"
ALLOWED_CELL_NAMESPACES="$(get ALLOWED_CELL_NAMESPACES)"

MIN_FREE_GIB="$(get MIN_FREE_GIB)"; MIN_FREE_GIB="${MIN_FREE_GIB:-50}"
SSH_CONNECT_TIMEOUT="$(get SSH_CONNECT_TIMEOUT)"; SSH_CONNECT_TIMEOUT="${SSH_CONNECT_TIMEOUT:-5}"
[[ "$MIN_FREE_GIB" =~ ^[0-9]+$ ]] || die "MIN_FREE_GIB must be an integer, got: $MIN_FREE_GIB"

SSH_A="$(get HOST_A_SSH)"
SSH_B="$(get HOST_B_SSH)"
SSH_C="$(get HOST_C_SSH)"
FD_A="$(get HOST_A_FAILURE_DOMAIN)"
FD_B="$(get HOST_B_FAILURE_DOMAIN)"
FD_C="$(get HOST_C_FAILURE_DOMAIN)"
CELL_A="$(get CELL_NODE_A)"
CELL_B="$(get CELL_NODE_B)"
CELL_C="$(get CELL_NODE_C)"

[[ "$SSH_A" != "$SSH_B" && "$SSH_A" != "$SSH_C" && "$SSH_B" != "$SSH_C" ]] \
  || die "HOST_A_SSH/HOST_B_SSH/HOST_C_SSH must be three distinct targets"
[[ "$FD_A" != "$FD_B" && "$FD_A" != "$FD_C" && "$FD_B" != "$FD_C" ]] \
  || die "HOST_A/B/C_FAILURE_DOMAIN must be three distinct failure domains"
[[ "$CELL_A" != "$CELL_B" && "$CELL_A" != "$CELL_C" && "$CELL_B" != "$CELL_C" ]] \
  || die "CELL_NODE_A/B/C must be three distinct Kubernetes node names"

# Evidence root must be outside the repository: raw evidence may contain
# address, credential-adjacent and topology data that must be curated before
# any committed bundle is created.
EVIDENCE_ABS="$(realpath -m -- "$(get EVIDENCE_ROOT)")"
case "$EVIDENCE_ABS" in
  "$REPO_ROOT"|"$REPO_ROOT"/*)
    die "EVIDENCE_ROOT must be outside the repository ($REPO_ROOT): $EVIDENCE_ABS
Collect raw evidence outside, curate it, and only then add a reviewed bundle under docs/evidence/."
    ;;
esac

# ---------------------------------------------------------------------------
# Results and helpers.
# ---------------------------------------------------------------------------

FAILURES=0
WARNINGS=0

ok()   { printf 'PASS  %s\n' "$*"; }
bad()  { printf 'FAIL  %s\n' "$*" >&2; FAILURES=$((FAILURES + 1)); }
warn() { printf 'WARN  %s\n' "$*" >&2; WARNINGS=$((WARNINGS + 1)); }

SSH_BASE=(ssh -o BatchMode=yes -o ConnectTimeout="$SSH_CONNECT_TIMEOUT" -o StrictHostKeyChecking=accept-new)
if [[ -n "$(get SSH_IDENTITY_FILE)" ]]; then
  SSH_BASE+=(-i "$(get SSH_IDENTITY_FILE)")
fi
remote_sh() {
  local target="$1" script="$2"
  "${SSH_BASE[@]}" "$target" bash -c "$script"
}

KUBECTL=(kubectl --request-timeout=15s)
INV_KUBECONFIG="$(get KUBECONFIG)"
if [[ -n "$INV_KUBECONFIG" ]]; then
  export KUBECONFIG="$INV_KUBECONFIG"
fi
if [[ -n "$KUBE_CONTEXT" ]]; then
  KUBECTL+=(--context "$KUBE_CONTEXT")
fi
k() { "${KUBECTL[@]}" "$@"; }

# ---------------------------------------------------------------------------
# Local tool validation (before any cluster/host contact).
# ---------------------------------------------------------------------------

for tool in kubectl helm jq ssh realpath; do
  if command -v "$tool" >/dev/null 2>&1; then
    ok "local tool present: $tool"
  else
    bad "local tool missing: $tool (install the POC toolchain before the run)"
  fi
done

if command -v kubectl >/dev/null 2>&1; then
  if kubectl version --client -o json >/dev/null 2>&1; then
    ok "kubectl client answers"
  else
    bad "kubectl client is not runnable"
  fi
fi
if command -v helm >/dev/null 2>&1; then
  if helm version --short >/dev/null 2>&1; then
    ok "helm answers ($(helm version --short 2>/dev/null || true))"
  else
    bad "helm is not runnable"
  fi
fi

((FAILURES == 0)) || {
  printf '\nPREFLIGHT FAIL: fix the inventory/local tools above. No host or cluster was contacted and no evidence was produced.\n' >&2
  exit 1
}

# ---------------------------------------------------------------------------
# Kubernetes control plane / Rook tooling.
# ---------------------------------------------------------------------------

if k get --raw=/readyz >/dev/null 2>&1; then
  ok "Kubernetes API reachable (context: ${KUBE_CONTEXT:-current})"
else
  bad "Kubernetes API unreachable for context '${KUBE_CONTEXT:-current}'; check KUBECONFIG/context/network"
fi

for crd in cephclusters.ceph.rook.io cephblockpools.ceph.rook.io; do
  if k get crd "$crd" >/dev/null 2>&1; then
    ok "Rook CRD installed: $crd"
  else
    bad "Rook CRD missing: $crd (install the pinned upstream Rook release on the independent management plane)"
  fi
done

if k -n "$ROOK_OPERATOR_NAMESPACE" get deployment -l app=rook-ceph-operator -o name 2>/dev/null | grep -q .; then
  ok "Rook operator deployment found in namespace $ROOK_OPERATOR_NAMESPACE"
else
  bad "Rook operator deployment not found in namespace $ROOK_OPERATOR_NAMESPACE (install/pin it before the POC)"
fi

# ---------------------------------------------------------------------------
# Dedicated cell Nodes.
# ---------------------------------------------------------------------------

mapfile -t LABELLED_CELLS < <(k get nodes -l "${CELL_ROLE_LABEL}=${CELL_ROLE_VALUE}" -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null || true)
if [[ ${#LABELLED_CELLS[@]} -ne 3 ]]; then
  bad "expected exactly 3 Nodes labelled ${CELL_ROLE_LABEL}=${CELL_ROLE_VALUE}; found ${#LABELLED_CELLS[@]}"
else
  ok "exactly 3 dedicated cell Nodes labelled ${CELL_ROLE_LABEL}=${CELL_ROLE_VALUE}"
fi

for expected in "$CELL_A" "$CELL_B" "$CELL_C"; do
  found=0
  for actual in "${LABELLED_CELLS[@]}"; do
    [[ "$actual" == "$expected" ]] && found=1
  done
  if ((found == 1)); then
    ok "cell Node present with the dedicated label: $expected"
  else
    bad "cell Node missing or unlabelled: $expected"
  fi
done

check_cell_node() {
  local node="$1" expected_fd="$2"
  local ready role fd taints unschedulable control_plane
  if ! k get node "$node" >/dev/null 2>&1; then
    bad "Kubernetes Node not found: $node"
    return
  fi
  ready="$(k get node "$node" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null || true)"
  if [[ "$ready" == "True" ]]; then
    ok "cell Node $node is Ready"
  else
    bad "cell Node $node is not Ready (got: ${ready:-<none>})"
  fi
  role="$(k get node "$node" -o json | jq -r --arg key "$CELL_ROLE_LABEL" '.metadata.labels[$key] // ""')"
  if [[ "$role" == "$CELL_ROLE_VALUE" ]]; then
    ok "cell Node $node carries the trusted role label"
  else
    bad "cell Node $node role label is '$role', expected '$CELL_ROLE_VALUE' (labels are trusted management input, never tenant input)"
  fi
  fd="$(k get node "$node" -o json | jq -r --arg key "$CELL_FD_LABEL" '.metadata.labels[$key] // ""')"
  if [[ -z "$fd" ]]; then
    bad "cell Node $node is missing failure-domain label '$CELL_FD_LABEL'"
  elif [[ "$fd" != "$expected_fd" ]]; then
    bad "cell Node $node failure domain '$fd' does not match inventory '$expected_fd'"
  else
    ok "cell Node $node failure domain is $fd"
  fi
  taints="$(k get node "$node" -o json | jq -r '.spec.taints // [] | .[] | "\(.key)=\(.value):\(.effect)"')"
  if grep -Fxq -- "${CELL_TAINT_KEY}=${CELL_TAINT_VALUE}:${CELL_TAINT_EFFECT}" <<<"$taints"; then
    ok "cell Node $node carries the storage-only taint"
  else
    bad "cell Node $node is missing taint ${CELL_TAINT_KEY}=${CELL_TAINT_VALUE}:${CELL_TAINT_EFFECT} (compute/tenant workloads must not schedule here by taint or policy)"
  fi
  unschedulable="$(k get node "$node" -o jsonpath='{.spec.unschedulable}' 2>/dev/null || true)"
  if [[ "$unschedulable" == "true" ]]; then
    warn "cell Node $node is cordoned (spec.unschedulable=true); uncordon it deliberately before the POC"
  fi
  control_plane="$(k get node "$node" -o json | jq -r '.metadata.labels["node-role.kubernetes.io/control-plane"] // ""')"
  if [[ -n "$control_plane" ]]; then
    bad "cell Node $node is also a control-plane node; the POC requires an independent Kubernetes control plane"
  fi

  # Dedicated: only explicitly allowed namespaces may have Pods on a cell.
  local offenders
  offenders="$(k get pods -A --field-selector "spec.nodeName=$node" -o json | jq -r '.items[] | "\(.metadata.namespace)/\(.metadata.name)"' | while IFS= read -r pod; do
    [[ -z "$pod" ]] && continue
    ns="${pod%%/*}"
    allowed=0
    IFS=',' read -ra allowed_ns <<<"$ALLOWED_CELL_NAMESPACES"
    for a in "${allowed_ns[@]}"; do
      [[ "$ns" == "$a" ]] && allowed=1
    done
    ((allowed == 0)) && printf '%s\n' "$pod"
  done)"
  if [[ -n "$offenders" ]]; then
    bad "cell Node $node runs Pods outside ALLOWED_CELL_NAMESPACES ($ALLOWED_CELL_NAMESPACES): $offenders"
  else
    ok "cell Node $node runs only allowlisted system/Rook Pods"
  fi
}

check_cell_node "$CELL_A" "$FD_A"
check_cell_node "$CELL_B" "$FD_B"
check_cell_node "$CELL_C" "$FD_C"

# ---------------------------------------------------------------------------
# Physical hosts: identity, VFIO/IOMMU, NVMe inventory, free space.
# ---------------------------------------------------------------------------

declare -A SEEN_HOSTNAME=()
declare -A SEEN_MACHINE_ID=()

check_host_identity() {
  local role="$1" target="$2"
  local out host machine_id
  # shellcheck disable=SC2016 # The single-quoted script must expand on the remote host.
  out="$(remote_sh "$target" 'printf "%s\t%s\n" "$(uname -n)" "$(cat /etc/machine-id 2>/dev/null || echo missing)"')" || {
    bad "host $role: ssh/identity query failed ($target)"
    return 1
  }
  IFS=$'\t' read -r host machine_id <<<"$out"
  [[ -n "$host" ]] || { bad "host $role: empty uname -n"; return 1; }
  if [[ -n "${SEEN_HOSTNAME[$host]:-}" ]]; then
    bad "host $role: duplicate physical host identity '$host' (also host ${SEEN_HOSTNAME[$host]})"
  else
    SEEN_HOSTNAME["$host"]="$role"
    ok "host $role: physical identity $host"
  fi
  if [[ "$machine_id" != "missing" && -n "$machine_id" ]]; then
    if [[ -n "${SEEN_MACHINE_ID[$machine_id]:-}" ]]; then
      bad "host $role: duplicate machine-id (also host ${SEEN_MACHINE_ID[$machine_id]})"
    else
      SEEN_MACHINE_ID["$machine_id"]="$role"
    fi
  else
    warn "host $role: /etc/machine-id is not readable; distinct chassis identity rests on hostname plus the operator inventory"
  fi
  return 0
}

check_vfio_host() {
  local role="$1" target="$2" bdf="$3"
  local driver group group_devices other_devices lspci_line
  if ! remote_sh "$target" 'command -v lspci >/dev/null 2>&1 && command -v lsblk >/dev/null 2>&1 && command -v findmnt >/dev/null 2>&1 && test -e /dev/vfio/vfio' >/dev/null 2>&1; then
    bad "host $role: VFIO/NVMe tooling or /dev/vfio/vfio missing (need lspci, lsblk, findmnt and the vfio module)"
    return
  fi
  if ! remote_sh "$target" "test -d /sys/bus/pci/devices/$bdf" >/dev/null 2>&1; then
    bad "host $role: VFIO BDF $bdf does not exist in sysfs"
    return
  fi
  driver="$(remote_sh "$target" "readlink -f /sys/bus/pci/devices/$bdf/driver 2>/dev/null | xargs -r basename")" || driver=""
  if [[ "$driver" == "vfio-pci" ]]; then
    ok "host $role: $bdf is bound to vfio-pci"
  else
    bad "host $role: $bdf driver is '${driver:-<none>}', expected vfio-pci (the exclusive cell controller must be bound before the POC)"
  fi
  lspci_line="$(remote_sh "$target" "lspci -s '$bdf' -nnk 2>/dev/null | tr '\n' ' '")" || lspci_line=""
  if [[ "$lspci_line" == *"Non-Volatile memory controller"* ]]; then
    ok "host $role: $bdf is an NVMe controller"
  else
    bad "host $role: $bdf does not look like an NVMe controller: ${lspci_line:-<no lspci output>}"
  fi
  if remote_sh "$target" "test -d /sys/bus/pci/devices/$bdf/iommu_group" >/dev/null 2>&1; then
    group="$(remote_sh "$target" "basename \"\$(readlink -f /sys/bus/pci/devices/$bdf/iommu_group)\"")" || group=""
    group_devices="$(remote_sh "$target" "ls -1 /sys/bus/pci/devices/$bdf/iommu_group/devices 2>/dev/null | sort")" || group_devices=""
    other_devices="$(printf '%s\n' "$group_devices" | awk -v slot="${bdf%.*}" 'NF && $0 !~ "^"slot"\\." {print}' | paste -sd, -)"
    if [[ -n "$other_devices" ]]; then
      bad "host $role: IOMMU group $group for $bdf also contains $other_devices; VFIO isolation is not dedicated"
    else
      ok "host $role: $bdf has an IOMMU group with no unrelated devices (group ${group:-?})"
    fi
  else
    bad "host $role: $bdf has no IOMMU group (enable VT-d/AMD-Vi and verify the kernel IOMMU configuration)"
  fi
  # While vfio-bound, the kernel nvme driver may have no sysfs controller;
  # the stable serial must then be corroborated against the discovery record.
  if remote_sh "$target" "test -d /sys/bus/pci/devices/$bdf/nvme" >/dev/null 2>&1; then
    warn "host $role: $bdf still exposes an nvme sysfs controller while vfio-bound; verify guest/host exclusivity manually"
  else
    ok "host $role: $bdf is not exposing a host nvme block controller (expected while VFIO-bound)"
  fi
}

check_host_disk() {
  local role="$1" target="$2" avail_kb min_kb
  if ! remote_sh "$target" "test -d '$(get EVIDENCE_ROOT)'" >/dev/null 2>&1; then
    bad "host $role: EVIDENCE_ROOT does not exist: $(get EVIDENCE_ROOT) (create it on the campaign controller before the run)"
    return
  fi
  if ! remote_sh "$target" "test -w '$(get EVIDENCE_ROOT)'" >/dev/null 2>&1; then
    bad "host $role: EVIDENCE_ROOT is not writable: $(get EVIDENCE_ROOT)"
    return
  fi
  avail_kb="$(remote_sh "$target" "df -Pk -- '$(get EVIDENCE_ROOT)' 2>/dev/null | awk 'NR==2 {print \$4}'")" || avail_kb=""
  [[ "$avail_kb" =~ ^[0-9]+$ ]] || { bad "host $role: df returned no numeric free space for EVIDENCE_ROOT"; return; }
  min_kb=$((MIN_FREE_GIB * 1024 * 1024))
  if ((avail_kb < min_kb)); then
    bad "host $role: EVIDENCE_ROOT has $((avail_kb / 1024 / 1024)) GiB free, below MIN_FREE_GIB=$MIN_FREE_GIB"
  else
    ok "host $role: EVIDENCE_ROOT has $((avail_kb / 1024 / 1024)) GiB free"
  fi
}

while IFS='|' read -r role target bdf; do
  if check_host_identity "$role" "$target"; then
    check_vfio_host "$role" "$target" "$bdf"
    if [[ "$role" == "C" ]]; then
      # Raw evidence is collected on the independent controller; A/B need
      # no evidence tree.
      check_host_disk "$role" "$target"
    fi
  fi
done <<EOF
A|$SSH_A|$(get HOST_A_VFIO_BDF)
B|$SSH_B|$(get HOST_B_VFIO_BDF)
C|$SSH_C|$(get HOST_C_VFIO_BDF)
EOF

# ---------------------------------------------------------------------------
# Rook/Ceph cluster (optional pre-bring-up; required post-bring-up).
# ---------------------------------------------------------------------------

if ((REQUIRE_CEPH == 1)); then
  if k -n "$ROOK_NAMESPACE" get "cephcluster/$CEPHCLUSTER_NAME" >/dev/null 2>&1; then
    ok "CephCluster $ROOK_NAMESPACE/$CEPHCLUSTER_NAME exists"
    if k -n "$ROOK_NAMESPACE" get "cephblockpool/$CEPHBLOCKPOOL_NAME" >/dev/null 2>&1; then
      ok "CephBlockPool $ROOK_NAMESPACE/$CEPHBLOCKPOOL_NAME exists"
    else
      bad "CephBlockPool $ROOK_NAMESPACE/$CEPHBLOCKPOOL_NAME missing (apply the reviewed pool manifest after cluster health)"
    fi
  else
    bad "CephCluster $ROOK_NAMESPACE/$CEPHCLUSTER_NAME missing (--require-ceph is the post-bring-up check)"
  fi
fi

# ---------------------------------------------------------------------------
# Evidence collector safety.
# ---------------------------------------------------------------------------

collector="$(get VOLVISOR_COLLECT_EVIDENCE)"
case "$collector" in
  /*) ;;
  *) collector="$REPO_ROOT/$collector" ;;
esac
if [[ -f "$collector" ]]; then
  if bash -n "$collector" >/dev/null 2>&1; then
    ok "evidence collector parses: $collector"
  else
    bad "evidence collector has a shell syntax error: $collector"
  fi
  if [[ -x "$collector" ]]; then
    ok "evidence collector is executable"
  else
    warn "evidence collector is not executable (invoke it as: bash $collector ...)"
  fi
  if KUBECONFIG="${KUBECONFIG:-}" bash "$collector" --check >/dev/null 2>&1; then
    ok "evidence collector --check passed (read-only)"
  else
    bad "evidence collector --check failed; run 'bash $collector --check' and read its message"
  fi
else
  bad "evidence collector not found: $collector"
fi

printf '\n'
if ((FAILURES > 0)); then
  printf 'PREFLIGHT FAIL: %d check(s) failed, %d warning(s).\n' "$FAILURES" "$WARNINGS" >&2
  printf 'Fix every FAIL before executing the #38 POC. No host was mutated and no evidence was produced.\n' >&2
  exit 1
fi
printf 'PREFLIGHT PASS: %d warning(s). The environment shape is eligible for the #38 POC.\n' "$WARNINGS"
printf 'This preflight produced NO POC evidence. Running the POC is a separate, operator-led act.\n'
