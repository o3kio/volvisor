#!/usr/bin/env bash
# Real-LVM gate: runs the loop-device integration suite on this host.
#
# This is locally runnable (root + lvm2 + losetup + device-mapper +
# tmpfs-mount capability for the ENOSPC regression test). It is also the
# `lvm` suite in release-scope.toml, so a release that claims
# native-local runs exactly this script and refuses to publish if it does
# not pass.
#
# Usage:
#   sudo ./scripts/test-lvm.sh
#   sudo ./scripts/test-lvm.sh --check     # prerequisites only
#
# Env:
#   VOLVISOR_TEST_OUTPUT_DIR   evidence dir (default target/real-backends/lvm)
#   CARGO_BUILD_JOBS           forwarded to cargo (default: host default)
#
# The suite is destructive only to a private loop device + temp VG it
# creates and removes; it never touches an operator VG.

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

rb_init lvm "$REPO_ROOT"
rb_write_environment
rb_log "== real-LVM preflight =="

rb_require_root lvm
for tool in cargo rustc losetup lvm pvcreate vgcreate lvcreate lvremove vgremove pvremove lvs vgs pvs dmsetup mount umount; do
    rb_require_tool lvm "$tool"
done

if [[ ! -e /dev/loop-control ]]; then
    # Best effort: loop may be a module that is not auto-loaded yet.
    modprobe loop >/dev/null 2>&1 || true
fi
if [[ ! -e /dev/loop-control ]]; then
    rb_fail lvm.no-loop-control \
        "/dev/loop-control is absent even after 'modprobe loop'. The LVM suite attaches a 256 MiB backing file to a loop device; enable CONFIG_BLK_DEV_LOOP (or load the loop module) on this host."
fi
if ! losetup -f >/dev/null 2>&1; then
    rb_fail lvm.no-free-loop \
        "'losetup -f' cannot find a free loop device. Detach stale loop devices ('losetup -a') or raise max_loop; the suite cannot run without one."
fi
if ! dmsetup version >/dev/null 2>&1; then
    rb_fail lvm.no-device-mapper \
        "'dmsetup version' failed. The device-mapper driver (dm_mod) must be available; the provider creates and grows real dm-linear LVs."
fi
# The ENOSPC regression test mounts a 256 KiB tmpfs over the provider
# state directory. Probe the capability, not merely the binary, so a
# root-but-capability-less host fails with a typed prerequisite error
# instead of a mid-suite test failure.
mount_probe="$RB_OUT/mount-probe"
mkdir -p "$mount_probe"
if ! mount -t tmpfs -o size=4k tmpfs "$mount_probe" >/dev/null 2>&1; then
    rmdir "$mount_probe" 2>/dev/null || true
    rb_fail lvm.no-tmpfs-mount \
        "mounting a tmpfs at $mount_probe failed. The ENOSPC regression test needs root with CAP_SYS_ADMIN (or an equivalent container capability) to mount a 256 KiB tmpfs; enable it or run the suite on a host/VM that permits tmpfs mounts."
fi
if ! umount "$mount_probe" >/dev/null 2>&1; then
    umount -l "$mount_probe" >/dev/null 2>&1 || true
    rmdir "$mount_probe" 2>/dev/null || true
    rb_fail lvm.no-tmpfs-umount \
        "unmounting the tmpfs probe at $mount_probe failed. Remove the stale mount before running the suite."
fi
rmdir "$mount_probe" 2>/dev/null || true
rb_record "tmpfs_mount=ok"

available_kb=$(df -Pk "$RB_OUT" | awk 'NR==2 {print $4}')
if [[ -z "$available_kb" || "$available_kb" -lt 1048576 ]]; then
    rb_fail lvm.low-disk \
        "less than 1 GiB free under $RB_OUT (found ${available_kb:-unknown} KiB). The suite writes a 256 MiB backing file plus LVM metadata/archives; free space or set VOLVISOR_TEST_OUTPUT_DIR to a larger filesystem."
fi

rb_record "lvm_version=$(lvm version 2>/dev/null | head -n 1)"
rb_record "losetup=$(losetup --version 2>&1 | head -n 1)"
rb_record "free_loop=$(losetup -f 2>/dev/null || echo none)"
rb_record "output_dir=$RB_OUT"
rb_log "root, lvm2, loop, device-mapper and tmpfs-mount prerequisites present"

if [[ "$CHECK_ONLY" -eq 1 ]]; then
    rb_check_only_finish
fi

export VOLVISOR_TEST_LVM=1
set +e
rb_run cargo test --locked -p volvisor-lvm --features integration-lvm
status=$?
set -e
if [[ "$status" -ne 0 ]]; then
    rb_fail lvm.tests-failed \
        "cargo test exited $status for the real-LVM suite. This is a gate failure (not a skip); inspect $RB_OUT/run.log and $RB_OUT/junit.xml."
fi

rb_assert_no_skip_note lvm "skipping real-LVM integration test"
for test_name in \
    lvm_lifecycle_on_loop_device \
    lvm_non_aligned_create_rounds_up_and_absent_lv_delete_succeeds \
    lvm_create_intent_save_enospc_fails_closed_and_converges_across_restart \
    lvm_move_evacuates_under_concurrent_io \
    lvm_move_restart_mid_move_re_attaches_and_completes \
    lvm_move_aborted_out_of_band_while_down_parks_in_doubt \
    lvm_move_aborted_mid_supervision_parks_without_freeing; do
    rb_assert_test_ok lvm "$test_name"
done

rb_finish
