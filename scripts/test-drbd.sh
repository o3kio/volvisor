#!/usr/bin/env bash
# Real-DRBD gate: runs the nearline lifecycle suite against an operator-
# provisioned peer. The host needs the DRBD 9 kernel module; the peer is a
# second host at the configured address with the pre-provisioned backing LV.
#
# This host has no DRBD module and no DRBD peer, so the gate fails loudly
# here by design. The self-hosted DRBD runner (labels:
# self-hosted, linux, x64, volvisor-drbd) supplies the rig.
#
# Usage:
#   sudo -E env "PATH=$PATH" \
#     VOLVISOR_DRBD_VG=volvisor-test VOLVISOR_DRBD_CONFIG_DIR=/etc/drbd.d \
#     VOLVISOR_DRBD_NODE_NAME="$(uname -n)" VOLVISOR_DRBD_LOCAL_ADDRESS=... \
#     VOLVISOR_DRBD_PEER_NAME=... VOLVISOR_DRBD_PEER_ADDRESS=...:7990 \
#     VOLVISOR_DRBD_SECRET_FILE=/root/.config/volvisor/drbd-secret \
#     ./scripts/test-drbd.sh
#   ./scripts/test-drbd.sh --check     # prerequisites only
#
# Secret handling: VOLVISOR_DRBD_SECRET (content) may be supplied instead
# of VOLVISOR_DRBD_SECRET_FILE; the script writes it to a mode-0600 temp
# file and removes it on exit. The secret is never written to evidence.

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

rb_init drbd "$REPO_ROOT"
rb_write_environment
rb_log "== real-DRBD preflight =="

rb_require_root drbd
for tool in cargo rustc drbdadm drbdsetup drbdmeta vgs lvs lvcreate lvremove ip getent timeout; do
    rb_require_tool drbd "$tool"
done
for var in \
    VOLVISOR_DRBD_VG \
    VOLVISOR_DRBD_CONFIG_DIR \
    VOLVISOR_DRBD_NODE_NAME \
    VOLVISOR_DRBD_LOCAL_ADDRESS \
    VOLVISOR_DRBD_PEER_NAME \
    VOLVISOR_DRBD_PEER_ADDRESS; do
    rb_require_env_var drbd "$var"
done

if [[ -n "${VOLVISOR_DRBD_SECRET:-}" && -n "${VOLVISOR_DRBD_SECRET_FILE:-}" ]]; then
    rb_fail drbd.ambiguous-secret \
        "set only one of VOLVISOR_DRBD_SECRET and VOLVISOR_DRBD_SECRET_FILE, not both. Ambiguous credential delivery is refused."
fi
if [[ -n "${VOLVISOR_DRBD_SECRET:-}" ]]; then
    RB_DRBD_SECRET_FILE=$(mktemp "${TMPDIR:-/tmp}/volvisor-drbd-secret.XXXXXX")
    export RB_CLEANUP_SECRET=1
    chmod 600 "$RB_DRBD_SECRET_FILE"
    printf '%s' "$VOLVISOR_DRBD_SECRET" >"$RB_DRBD_SECRET_FILE"
    export VOLVISOR_DRBD_SECRET_FILE="$RB_DRBD_SECRET_FILE"
fi
rb_require_env_var drbd VOLVISOR_DRBD_SECRET_FILE
rb_require_private_file drbd VOLVISOR_DRBD_SECRET_FILE "$VOLVISOR_DRBD_SECRET_FILE"

if ! drbd_version=$(drbdadm --version 2>&1); then
    rb_fail drbd.drbdadm-version \
        "'drbdadm --version' failed. Install the drbd-utils 9.x generation and ensure the DRBD 9 kernel module is available on this runner."
fi
if ! grep -Eq '^DRBD_KERNEL_VERSION=9\.' <<<"$drbd_version"; then
    rb_fail drbd.kernel-module \
        "drbdadm reports no DRBD 9 kernel module (expected DRBD_KERNEL_VERSION=9.x). Load/install the out-of-tree DRBD 9 module (LINBIT/ELRepo on RHEL-family; distribution package otherwise); DRBD 8.4 is not accepted."
fi

if [[ "$VOLVISOR_DRBD_NODE_NAME" != "$(uname -n)" ]]; then
    rb_fail drbd.node-name-mismatch \
        "VOLVISOR_DRBD_NODE_NAME='$VOLVISOR_DRBD_NODE_NAME' does not equal this host's 'uname -n' ('$(uname -n)'). The provider refuses the wrong host; fix the rig inventory."
fi

if [[ ! -d "$VOLVISOR_DRBD_CONFIG_DIR" ]]; then
    rb_fail drbd.config-dir-missing \
        "VOLVISOR_DRBD_CONFIG_DIR='$VOLVISOR_DRBD_CONFIG_DIR' does not exist. Point it at the drbd-utils config directory (usually /etc/drbd.d) where volvisor-*.res files may be written."
fi
if [[ ! -w "$VOLVISOR_DRBD_CONFIG_DIR" ]]; then
    rb_fail drbd.config-dir-not-writable \
        "VOLVISOR_DRBD_CONFIG_DIR='$VOLVISOR_DRBD_CONFIG_DIR' is not writable by root. The provider writes generated volvisor-*.res files there; fix ownership/mode on the dedicated rig."
fi

if ! vg_free=$(vgs --noheadings --units b --nosuffix -o vg_free "$VOLVISOR_DRBD_VG" 2>"$RB_OUT/vgs.stderr"); then
    rb_fail drbd.vg-missing \
        "volume group '$VOLVISOR_DRBD_VG' does not exist or is not readable. Provision the dedicated nearline VG documented in docs/testing/real-backends.md#rig-topology; stderr is in $RB_OUT/vgs.stderr."
fi
vg_free=$(printf '%s' "$vg_free" | tr -d '[:space:]')
if [[ ! "$vg_free" =~ ^[0-9]+$ ]]; then
    rb_fail drbd.vg-free-unreadable \
        "could not parse free space in VG '$VOLVISOR_DRBD_VG' (got '$vg_free'). Check 'vgs -o vg_free' output on the runner."
fi
required_free=${VOLVISOR_DRBD_MIN_FREE_BYTES:-268435456}
if (( vg_free < required_free )); then
    rb_fail drbd.vg-too-small \
        "VG '$VOLVISOR_DRBD_VG' has $vg_free bytes free; the suite needs at least $required_free bytes for create (64 MiB), grow (128 MiB) and LVM metadata. Free space or enlarge the dedicated test VG."
fi

if lvs --noheadings -o lv_name "$VOLVISOR_DRBD_VG" 2>/dev/null | grep -Fq 'vol-itest-drbd-vol-'; then
    rb_fail drbd.stale-test-lv \
        "the dedicated VG already contains a previous 'vol-itest-drbd-vol-*' LV. Remove the stale test LV (the suite refuses to collide with it) before rerunning."
fi
if compgen -G "$VOLVISOR_DRBD_CONFIG_DIR/volvisor-vol-itest-drbd-vol-*.res" >/dev/null; then
    rb_fail drbd.stale-resource-file \
        "the config dir already contains a previous generated volvisor-vol-itest-drbd-vol-*.res file. Remove the stale resource definition before rerunning."
fi

if ! ip -o -4 addr show | awk '{print $4}' | cut -d/ -f1 | grep -Fxq -- "$VOLVISOR_DRBD_LOCAL_ADDRESS"; then
    rb_fail drbd.local-address-not-present \
        "VOLVISOR_DRBD_LOCAL_ADDRESS='$VOLVISOR_DRBD_LOCAL_ADDRESS' is not an IPv4 address on this host. The generated resource binds replication to it; fix the rig inventory or the interface."
fi

if [[ ! "$VOLVISOR_DRBD_PEER_ADDRESS" =~ ^([^:]+):([0-9]+)$ ]]; then
    rb_fail drbd.invalid-peer-address \
        "VOLVISOR_DRBD_PEER_ADDRESS='$VOLVISOR_DRBD_PEER_ADDRESS' is not <ipv4-or-host>:<port>. Examples: 10.0.0.12:7990; IPv6 is not supported by this rig contract."
fi
peer_host="${BASH_REMATCH[1]}"
peer_port="${BASH_REMATCH[2]}"
if (( peer_port < 1 || peer_port > 65535 )); then
    rb_fail drbd.invalid-peer-port \
        "VOLVISOR_DRBD_PEER_ADDRESS port $peer_port is out of range. Use the DRBD replication port allocated on the peer."
fi
if ! getent ahostsv4 "$peer_host" >/dev/null 2>&1; then
    rb_fail drbd.peer-unresolvable \
        "the peer host '$peer_host' does not resolve to an IPv4 address on this runner. Fix /etc/hosts or DNS in the rig; the peer must be reachable for replication."
fi

rb_record "drbd_version=$(printf '%s' "$drbd_version" | tr '\n' ' ')"
rb_record "drbd_vg=$VOLVISOR_DRBD_VG"
rb_record "drbd_vg_free_bytes=$vg_free"
rb_record "drbd_config_dir=$VOLVISOR_DRBD_CONFIG_DIR"
rb_record "drbd_node_name=$VOLVISOR_DRBD_NODE_NAME"
rb_record "drbd_local_address=$VOLVISOR_DRBD_LOCAL_ADDRESS"
rb_record "drbd_peer_name=$VOLVISOR_DRBD_PEER_NAME"
rb_record "drbd_peer_address=$VOLVISOR_DRBD_PEER_ADDRESS"
rb_record "drbd_secret_mode=$(stat -Lc '%a' "$VOLVISOR_DRBD_SECRET_FILE")"
rb_log "DRBD 9 module, dedicated VG, local address and peer resolution present"

if [[ "$CHECK_ONLY" -eq 1 ]]; then
    rb_check_only_finish
fi

export VOLVISOR_TEST_DRBD=1
set +e
rb_run cargo test --locked -p volvisor-drbd --features integration-drbd --test integration_drbd -- --nocapture --test-threads=1
status=$?
set -e
if [[ "$status" -ne 0 ]]; then
    rb_fail drbd.tests-failed \
        "cargo test exited $status for the real-DRBD suite. This is a gate failure (not a skip); inspect $RB_OUT/run.log and $RB_OUT/junit.xml, then the peer's drbdadm status and kernel log."
fi

rb_assert_no_skip_note drbd "skipping real-DRBD integration test"
rb_assert_test_ok drbd drbd_lifecycle_on_a_real_cluster
rb_assert_test_ok drbd drbd_constructor_refuses_the_wrong_host

rb_finish
