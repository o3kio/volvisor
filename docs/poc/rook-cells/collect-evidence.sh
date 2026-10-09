#!/usr/bin/env bash
# Volvisor rook-cell POC read-only evidence collector. NO DISK MUTATIONS.
# Usage: ./collect-evidence.sh [output-dir]
set -euo pipefail

out="${1:-rook-cell-evidence}"
command -v kubectl >/dev/null || { echo "kubectl missing" >&2; exit 2; }
mkdir -p "$out"
chmod 700 "$out"
echo "Read-only Kubernetes snapshot -> $out"

date -u +"%Y-%m-%dT%H:%M:%SZ" > "$out/collected-at-utc.txt"
kubectl version -o yaml > "$out/kubectl-version.yaml" 2>&1 || true
kubectl get nodes -o wide > "$out/nodes-wide.txt"
kubectl get nodes -o yaml > "$out/nodes.yaml"
kubectl get pods -A -o wide > "$out/pods-wide.txt"
kubectl get pods -A -o yaml > "$out/pods.yaml"
kubectl -n rook-ceph get cephcluster rook-ceph -o yaml > "$out/cephcluster.yaml" 2>&1 \
  || echo "MISSING CephCluster rook-ceph (pre-bring-up snapshot?)" >&2
kubectl -n rook-ceph get cephblockpool volvisor-poc-rbd -o yaml > "$out/cephblockpool.yaml" 2>&1 \
  || echo "MISSING CephBlockPool volvisor-poc-rbd (pre-bring-up snapshot?)" >&2
kubectl -n rook-ceph get pods -o wide > "$out/rook-pods-wide.txt" 2>&1 || echo "MISSING namespace rook-ceph (operator not installed yet?)" >&2
kubectl -n rook-ceph get events --sort-by=.lastTimestamp > "$out/rook-events.txt" 2>&1 || true
fail=0
for node in volvisor-cell-a volvisor-cell-b volvisor-cell-c; do
  if ! kubectl get node "$node" >/dev/null 2>&1; then
    echo "MISSING Kubernetes Node: $node" >&2; fail=1; continue
  fi
  ready="$(kubectl get node "$node" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')"
  role="$(kubectl get node "$node" -o jsonpath='{.metadata.labels.volvisor\.io/node-role}')"
  echo "$node Ready=$ready role=$role"
  if [[ "$ready" != "True" || "$role" != "storage-cell" ]]; then
    echo "Node not Ready or missing trusted role label: $node" >&2
    fail=1
  fi
  kubectl describe node "$node" > "$out/$node-describe.txt"
  kubectl get pods -A --field-selector "spec.nodeName=$node" -o wide > "$out/$node-pods.txt"
done

cat > "$out/README.txt" <<'INFO'
READ-ONLY snapshot. This is NOT an automated GO verdict.
Evidence may contain node addresses, service account names, Pod environment
and Kubernetes object metadata. Keep it private and scrub before sharing.
Still required manually on actual physical hosts:
- physical PCI/IOMMU/VFIO and guest NVMe stable identity attestation
- Ceph command output: ceph -s; ceph osd tree; ceph osd dump; ceph quorum_status
- three DISTINCT physical-host CRUSH failure domains
- RBD checksum/fio; matched bare-metal benchmark; power/disk/VM failure injection
- verified tenant Pod admission denial, RAM/CPU ceiling and independence cycle
Never accept PASS just because the Kubernetes API reports healthy nodes.
INFO
if [[ "$fail" != 0 ]]; then
  echo "PRE-FLIGHT FAIL. NOT a POC GO." >&2
  exit 1
fi
echo "PRE-FLIGHT Kubernetes Node checks passed. Real-host POC gates NOT evaluated."
