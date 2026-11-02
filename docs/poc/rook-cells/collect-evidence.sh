#!/usr/bin/env bash
# Volvisor rook-cell POC read-only evidence collector. NO DISK MUTATIONS.
#
# Usage:
#   bash collect-evidence.sh [--check] [output-dir]
#
#   --check       Validate kubectl, the selected context and the destination
#                 path; write nothing.
#   --help        Show this text.
#
# The collector only runs read-only `kubectl get/describe/version` commands.
# It never applies, deletes, patches, execs into a Pod, drains a node, touches
# a disk, or changes a Ceph/DRBD/Kubernetes object. It refuses to write into a
# non-empty destination so a later run can never silently overwrite an earlier
# evidence snapshot; choose a fresh run directory per collection.
#
# Raw output can contain node addresses, service-account names, Pod
# environment and Kubernetes object metadata. Keep it private, collect it
# outside the repository, and curate/scrub before committing any bundle under
# docs/evidence/ (see docs/evidence/README.md).
#
# The environment (all optional; defaults shown):
#   KUBECONFIG, KUBE_CONTEXT
#   ROOK_NAMESPACE=rook-ceph
#   CEPHCLUSTER_NAME=rook-ceph
#   CEPHBLOCKPOOL_NAME=volvisor-poc-rbd
#   NODE_ROLE_LABEL=volvisor.io/node-role
#   NODE_ROLE_VALUE=storage-cell
#   CELL_NODES=volvisor-cell-a,volvisor-cell-b,volvisor-cell-c

set -euo pipefail
IFS=$'\n\t'
umask 077

readonly SCRIPT_NAME="${0##*/}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
readonly DEFAULT_OUT="rook-cell-evidence"

usage() {
  cat >&2 <<EOF
Usage: $SCRIPT_NAME [--check] [output-dir]

Read-only Kubernetes/Rook snapshot. Default output dir: $DEFAULT_OUT
  --check   validate tools/context/destination and write nothing
  --help    this text
EOF
}

CHECK_ONLY=0
OUT=""
for arg in "$@"; do
  case "$arg" in
    --check) CHECK_ONLY=1 ;;
    --help|-h) usage; exit 0 ;;
    --*) printf 'ERROR: unknown option: %s\n' "$arg" >&2; usage; exit 2 ;;
    *)
      [[ -z "$OUT" ]] || { printf 'ERROR: at most one output dir may be given\n' >&2; exit 2; }
      OUT="$arg"
      ;;
  esac
done
OUT="${OUT:-$DEFAULT_OUT}"

ROOK_NAMESPACE="${ROOK_NAMESPACE:-rook-ceph}"
CEPHCLUSTER_NAME="${CEPHCLUSTER_NAME:-rook-ceph}"
CEPHBLOCKPOOL_NAME="${CEPHBLOCKPOOL_NAME:-volvisor-poc-rbd}"
NODE_ROLE_LABEL="${NODE_ROLE_LABEL:-volvisor.io/node-role}"
NODE_ROLE_VALUE="${NODE_ROLE_VALUE:-storage-cell}"
CELL_NODES="${CELL_NODES:-volvisor-cell-a,volvisor-cell-b,volvisor-cell-c}"

KUBECTL=(kubectl --request-timeout=15s)
if [[ -n "${KUBE_CONTEXT:-}" ]]; then
  KUBECTL+=(--context "$KUBE_CONTEXT")
fi
k() { "${KUBECTL[@]}" "$@"; }

command -v kubectl >/dev/null 2>&1 || { printf 'ERROR: kubectl missing; install it before collecting\n' >&2; exit 2; }
k version --client >/dev/null 2>&1 || { printf 'ERROR: kubectl client is not runnable\n' >&2; exit 2; }
k get --raw=/readyz >/dev/null 2>&1 || {
  printf 'ERROR: Kubernetes API is unreachable for context %s; check KUBECONFIG/KUBE_CONTEXT/network\n' "${KUBE_CONTEXT:-<current>}" >&2
  exit 2
}

# Destination safety: never a symlink, never the filesystem root, and never a
# non-empty directory (a new run gets a new directory).
if [[ -L "$OUT" ]]; then
  printf 'ERROR: refusing symlink output dir: %s\n' "$OUT" >&2
  exit 2
fi
OUT_ABS="$(realpath -m -- "$OUT")"
[[ "$OUT_ABS" != "/" ]] || { printf 'ERROR: refusing to use / as the evidence directory\n' >&2; exit 2; }
if [[ -e "$OUT" ]]; then
  [[ -d "$OUT" ]] || { printf 'ERROR: output path exists and is not a directory: %s\n' "$OUT" >&2; exit 2; }
  if [[ "$CHECK_ONLY" == 0 ]] && [[ -n "$(find "$OUT" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)" ]]; then
    printf 'ERROR: output dir is not empty: %s\nChoose a fresh run directory; this collector never overwrites an earlier snapshot.\n' "$OUT" >&2
    exit 2
  fi
else
  parent="$(dirname -- "$OUT_ABS")"
  [[ -d "$parent" ]] || { printf 'ERROR: parent directory does not exist: %s\nCreate it first (outside the repository) and re-run.\n' "$parent" >&2; exit 2; }
  [[ -w "$parent" ]] || { printf 'ERROR: parent directory is not writable: %s\n' "$parent" >&2; exit 2; }
fi

if [[ "$CHECK_ONLY" == 1 ]]; then
  printf 'CHECK OK: kubectl %s, context %s, destination %s is safe for a new snapshot.\n' \
    "$(k version --client -o jsonpath='{.clientVersion.gitVersion}' 2>/dev/null || echo '?')" \
    "${KUBE_CONTEXT:-<current>}" "$OUT_ABS"
  printf 'No files were written.\n'
  exit 0
fi

if [[ ! -d "$OUT" ]]; then
  mkdir -m 700 -- "$OUT"
fi
chmod 700 -- "$OUT"

printf 'Read-only Kubernetes snapshot -> %s\n' "$OUT_ABS"
printf 'WARNING: raw output may contain addresses, identities and Pod environment. Keep it private; scrub before sharing.\n' >&2

date -u +"%Y-%m-%dT%H:%M:%SZ" > "$OUT/collected-at-utc.txt"
{
  printf 'collector_sha256=%s\n' "$(sha256sum -- "$SCRIPT_DIR/collect-evidence.sh" | awk '{print $1}')"
  printf 'kube_context=%s\n' "${KUBE_CONTEXT:-<current>}"
  printf 'hostname=%s\n' "$(uname -n)"
  printf 'kernel=%s\n' "$(uname -r)"
} > "$OUT/collection-context.txt"

k config current-context > "$OUT/kube-context.txt" 2>&1 || true
k version -o yaml > "$OUT/kubectl-version.yaml" 2>&1 || true
k get nodes -o wide > "$OUT/nodes-wide.txt"
k get nodes -o yaml > "$OUT/nodes.yaml"
k get pods -A -o wide > "$OUT/pods-wide.txt"
k get pods -A -o yaml > "$OUT/pods.yaml"
k -n "$ROOK_NAMESPACE" get "cephcluster/$CEPHCLUSTER_NAME" -o yaml > "$OUT/cephcluster.yaml" 2>&1 \
  || echo "MISSING CephCluster $ROOK_NAMESPACE/$CEPHCLUSTER_NAME (pre-bring-up snapshot?)" >&2
k -n "$ROOK_NAMESPACE" get "cephblockpool/$CEPHBLOCKPOOL_NAME" -o yaml > "$OUT/cephblockpool.yaml" 2>&1 \
  || echo "MISSING CephBlockPool $ROOK_NAMESPACE/$CEPHBLOCKPOOL_NAME (pre-bring-up snapshot?)" >&2
k -n "$ROOK_NAMESPACE" get pods -o wide > "$OUT/rook-pods-wide.txt" 2>&1 \
  || echo "MISSING namespace $ROOK_NAMESPACE (operator not installed yet?)" >&2
k -n "$ROOK_NAMESPACE" get events --sort-by=.lastTimestamp > "$OUT/rook-events.txt" 2>&1 || true

fail=0
IFS=',' read -ra CELL_NODE_LIST <<<"$CELL_NODES"
for node in "${CELL_NODE_LIST[@]}"; do
  [[ -n "$node" ]] || continue
  if ! k get node "$node" >/dev/null 2>&1; then
    echo "MISSING Kubernetes Node: $node" >&2
    fail=1
    continue
  fi
  ready="$(k get node "$node" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')"
  role="$(k get node "$node" -o jsonpath="{.metadata.labels.${NODE_ROLE_LABEL//./\\.}}" 2>/dev/null || true)"
  echo "$node Ready=$ready role=$role"
  if [[ "$ready" != "True" || "$role" != "$NODE_ROLE_VALUE" ]]; then
    echo "Node not Ready or missing trusted role label: $node" >&2
    fail=1
  fi
  k describe node "$node" > "$OUT/$node-describe.txt"
  k get pods -A --field-selector "spec.nodeName=$node" -o wide > "$OUT/$node-pods.txt"
done

cat > "$OUT/README.txt" <<'INFO'
READ-ONLY snapshot. This is NOT an automated GO verdict.
Raw output may contain node addresses, service account names, Pod environment
and Kubernetes object metadata. Keep it private and scrub before sharing.
Still required manually on actual physical hosts:
- physical PCI/IOMMU/VFIO and guest NVMe stable identity attestation
- Ceph command output: ceph -s; ceph osd tree; ceph osd dump; ceph quorum_status
- three DISTINCT physical-host CRUSH failure domains
- RBD checksum/fio; matched bare-metal benchmark; power/disk/VM failure injection
- verified tenant Pod admission denial, RAM/CPU ceiling and independence cycle
- exact source SHA, component versions and digests for every host/guest
Never accept PASS just because the Kubernetes API reports healthy nodes.
INFO

{
  cd -- "$OUT"
  find . -type f ! -name 'MANIFEST.sha256' -print0 | sort -z | xargs -0 sha256sum > MANIFEST.sha256
}
printf 'Snapshot files listed in %s/MANIFEST.sha256\n' "$OUT_ABS"

if [[ "$fail" != 0 ]]; then
  echo "PRE-FLIGHT FAIL. NOT a POC GO. Snapshot is incomplete but was still hashed for audit." >&2
  exit 1
fi
echo "PRE-FLIGHT Kubernetes Node checks passed. Real-host POC gates NOT evaluated."
