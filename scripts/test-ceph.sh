#!/usr/bin/env bash
# Real-Ceph gate: runs the RBD lifecycle suite against an external cluster.
#
# This host has no Ceph cluster and no ceph/rbd CLIs, so the gate fails
# loudly here by design. The self-hosted Ceph runner (labels:
# self-hosted, linux, x64, volvisor-ceph) supplies the cluster.
#
# Usage:
#   sudo -E env "PATH=$PATH" \
#     VOLVISOR_CEPH_FSID=... VOLVISOR_CEPH_MONS=... \
#     VOLVISOR_CEPH_POOL=... VOLVISOR_CEPH_USER=client.volvisor \
#     ./scripts/test-ceph.sh
#   ./scripts/test-ceph.sh --check     # prerequisites only
#
# Secrets:
#   Prefer a runner-provisioned keyring at the conventional Ceph path.
#   For CI, VOLVISOR_CEPH_KEYRING_B64 (base64 of the keyring file) or
#   VOLVISOR_CEPH_KEYRING_FILE (mode 0600) is supported; the script sets
#   CEPH_ARGS=--keyring <file> and deletes its own temporary copy on exit.
#   No key material is ever written to the evidence directory.

set -Eeuo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
# shellcheck source=scripts/lib/real-backends-common.sh
source "$SCRIPT_DIR/lib/real-backends-common.sh"

CHECK_ONLY=0
case "${1:-}" in
    --check) CHECK_ONLY=1 ;;
    "") ;;
    *)
        echo "usage: $0 [--check]" >&2
        exit 64
        ;;
esac

rb_init ceph "$REPO_ROOT"
rb_write_environment
rb_log "== real-Ceph preflight =="

rb_require_root ceph
for tool in cargo rustc ceph rbd timeout; do
    rb_require_tool ceph "$tool"
done
for var in VOLVISOR_CEPH_FSID VOLVISOR_CEPH_MONS VOLVISOR_CEPH_POOL VOLVISOR_CEPH_USER; do
    rb_require_env_var ceph "$var"
done

if [[ ! "$VOLVISOR_CEPH_FSID" =~ ^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$ ]]; then
    rb_fail ceph.invalid-fsid \
        "VOLVISOR_CEPH_FSID is not a UUID-shaped cluster FSID. Copy the exact 'ceph fsid' value from the rig; the provider refuses a mis-pointed cluster."
fi
if [[ ! "$VOLVISOR_CEPH_POOL" =~ ^[A-Za-z0-9_.-]+$ ]]; then
    rb_fail ceph.invalid-pool \
        "VOLVISOR_CEPH_POOL contains characters Ceph pool names do not allow. Use the dedicated test pool name (letters, digits, '_', '-', '.')."
fi
if [[ "$VOLVISOR_CEPH_USER" != client.* ]]; then
    rb_fail ceph.invalid-user \
        "VOLVISOR_CEPH_USER must be the FULL Ceph entity name (for example 'client.volvisor'), not a short id. The provider passes it through --name."
fi
IFS=',' read -r -a mon_entries <<<"$VOLVISOR_CEPH_MONS"
if [[ "${#mon_entries[@]}" -eq 0 ]]; then
    rb_fail ceph.invalid-mons \
        "VOLVISOR_CEPH_MONS is empty. Provide the comma-separated monitor addresses the provider should pass through -m."
fi
for mon in "${mon_entries[@]}"; do
    if [[ -z "${mon//[[:space:]]/}" ]]; then
        rb_fail ceph.invalid-mons \
            "VOLVISOR_CEPH_MONS contains an empty entry. Use 'mon-a:6789,mon-b:6789' with no blank entry."
    fi
done

if [[ -n "${VOLVISOR_CEPH_KEYRING_B64:-}" && -n "${VOLVISOR_CEPH_KEYRING_FILE:-}" ]]; then
    rb_fail ceph.ambiguous-keyring \
        "set only one of VOLVISOR_CEPH_KEYRING_B64 and VOLVISOR_CEPH_KEYRING_FILE, not both. Ambiguous credential delivery is refused."
fi
if [[ -n "${VOLVISOR_CEPH_KEYRING_B64:-}" ]]; then
    rb_require_tool ceph base64
    RB_KEYRING_FILE=$(mktemp "${TMPDIR:-/tmp}/volvisor-ceph-keyring.XXXXXX")
    export RB_CLEANUP_KEYRING=1
    chmod 600 "$RB_KEYRING_FILE"
    if ! printf '%s' "$VOLVISOR_CEPH_KEYRING_B64" | base64 -d >"$RB_KEYRING_FILE" 2>/dev/null; then
        rb_fail ceph.invalid-keyring-b64 \
            "VOLVISOR_CEPH_KEYRING_B64 is not valid base64. Store the keyring file as base64 (for example: base64 -w0 keyring)."
    fi
    if [[ ! -s "$RB_KEYRING_FILE" ]]; then
        rb_fail ceph.empty-keyring \
            "VOLVISOR_CEPH_KEYRING_B64 decoded to an empty file. The keyring cannot be empty."
    fi
    export CEPH_ARGS="${CEPH_ARGS:-} --keyring $RB_KEYRING_FILE"
elif [[ -n "${VOLVISOR_CEPH_KEYRING_FILE:-}" ]]; then
    rb_require_private_file ceph VOLVISOR_CEPH_KEYRING_FILE "$VOLVISOR_CEPH_KEYRING_FILE"
    export CEPH_ARGS="${CEPH_ARGS:-} --keyring $VOLVISOR_CEPH_KEYRING_FILE"
fi

if [[ ! -d /sys/bus/rbd ]]; then
    modprobe rbd >/dev/null 2>&1 || true
fi
if [[ ! -d /sys/bus/rbd ]]; then
    rb_fail ceph.rbd-kernel-module \
        "/sys/bus/rbd is absent, so 'rbd map' cannot create a block device. Load the Linux rbd kernel module (modprobe rbd) or use a kernel built with CONFIG_BLK_DEV_RBD."
fi

ceph_cmd=(ceph -m "$VOLVISOR_CEPH_MONS" --name "$VOLVISOR_CEPH_USER")
rbd_cmd=(rbd -m "$VOLVISOR_CEPH_MONS" --name "$VOLVISOR_CEPH_USER")

if ! fsid_out=$(timeout 60 "${ceph_cmd[@]}" fsid 2>"$RB_OUT/ceph-fsid.stderr"); then
    rb_fail ceph.unreachable \
        "'ceph fsid' failed against the configured monitors. Check mon reachability, the client keyring/capabilities and VOLVISOR_CEPH_MONS; stderr is in $RB_OUT/ceph-fsid.stderr."
fi
fsid_out=$(printf '%s' "$fsid_out" | tr -d '[:space:]')
if [[ "$fsid_out" != "$VOLVISOR_CEPH_FSID" ]]; then
    rb_fail ceph.fsid-mismatch \
        "the reachable cluster reports FSID '$fsid_out', not VOLVISOR_CEPH_FSID='$VOLVISOR_CEPH_FSID'. Refusing to run against the wrong cluster."
fi

if ! timeout 60 "${ceph_cmd[@]}" osd pool get "$VOLVISOR_CEPH_POOL" size --format json >"$RB_OUT/ceph-pool.json" 2>"$RB_OUT/ceph-pool.stderr"; then
    rb_fail ceph.pool-unavailable \
        "the dedicated test pool '$VOLVISOR_CEPH_POOL' is not readable by '$VOLVISOR_CEPH_USER' (or does not exist). Provision it and grant the least-privilege client caps documented in docs/testing/real-backends.md#least-privilege-credentials."
fi
if ! timeout 60 "${rbd_cmd[@]}" ls --pool "$VOLVISOR_CEPH_POOL" --format json >"$RB_OUT/ceph-rbd-ls.json" 2>"$RB_OUT/ceph-rbd-ls.stderr"; then
    rb_fail ceph.rbd-cli-unusable \
        "'rbd ls --pool $VOLVISOR_CEPH_POOL' failed. The client needs the rbd CLI, a usable keyring and read access to the pool; stderr is in $RB_OUT/ceph-rbd-ls.stderr."
fi
if grep -Fq 'vol-itest-ceph-vol-' "$RB_OUT/ceph-rbd-ls.json"; then
    rb_fail ceph.stale-test-image \
        "the dedicated pool already contains a previous 'vol-itest-ceph-vol-*' image. Remove it (or purge the RBD trash) before rerunning; the suite refuses to collide with stale test state."
fi

health_out=$(timeout 60 "${ceph_cmd[@]}" health 2>/dev/null || true)
rb_record "ceph_fsid=$VOLVISOR_CEPH_FSID"
rb_record "ceph_mons=$VOLVISOR_CEPH_MONS"
rb_record "ceph_pool=$VOLVISOR_CEPH_POOL"
rb_record "ceph_user=$VOLVISOR_CEPH_USER"
rb_record "ceph_cli=$(ceph --version 2>&1 | head -n 1)"
rb_record "rbd_cli=$(rbd --version 2>&1 | head -n 1)"
rb_record "ceph_health=${health_out:-unknown}"
if [[ "$health_out" != HEALTH_OK* ]]; then
    rb_warn ceph.health-not-ok \
        "cluster health is '${health_out:-unknown}', not HEALTH_OK. The lifecycle may still pass, but this warning is retained with the release evidence and must be triaged by the storage owner."
fi
rb_log "cluster reachable, FSID matches, pool '$VOLVISOR_CEPH_POOL' readable"

if [[ "$CHECK_ONLY" -eq 1 ]]; then
    rb_check_only_finish
fi

export VOLVISOR_TEST_CEPH=1
set +e
rb_run cargo test --locked -p volvisor-ceph --features integration-ceph --test integration_ceph -- --nocapture --test-threads=1
status=$?
set -e
if [[ "$status" -ne 0 ]]; then
    rb_fail ceph.tests-failed \
        "cargo test exited $status for the real-Ceph suite. This is a gate failure (not a skip); inspect $RB_OUT/run.log and $RB_OUT/junit.xml."
fi

rb_assert_no_skip_note ceph "skipping real-Ceph integration test"
rb_assert_test_ok ceph ceph_lifecycle_on_a_real_cluster
rb_assert_test_ok ceph ceph_constructor_refuses_a_mis_pointed_cluster

rb_finish
