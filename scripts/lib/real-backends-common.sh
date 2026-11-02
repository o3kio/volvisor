# shellcheck shell=bash
# Shared helpers for the real-backend one-command scripts.
#
# Every script in this directory is a GATE: missing prerequisites are a
# hard failure with a typed code, never a skip that looks like success.
# The release workflow runs these scripts for every provider declared
# `supported` in release-scope.toml.
#
# The helper writes an evidence directory per run:
#
#   $VOLVISOR_TEST_OUTPUT_DIR   (default: target/real-backends/<suite>)
#     preflight.log  errors.txt  environment.txt  command.txt
#     run.log        junit.xml   status.txt
#
# Do not put secrets in this file or in the evidence directory. Ceph
# keyring material and the DRBD shared secret are materialized into
# mode-0600 temporary files by the scripts/workflow and deleted on exit.

set -Eeuo pipefail

# Evidence must be readable by the GitHub artifact uploader even when the
# suite itself runs as root. Secrets are explicitly chmod 600 after mktemp.
umask 022

RB_SUITE=""
RB_REPO_ROOT=""
RB_OUT=""
RB_START_UTC=""
RB_CLEANUP_KEYRING=0
RB_CLEANUP_SECRET=0
RB_KEYRING_FILE=""
RB_DRBD_SECRET_FILE=""

rb_now() {
    date -u +"%Y-%m-%dT%H:%M:%SZ"
}

rb_init() {
    local suite="$1"
    local repo_root="$2"
    RB_SUITE="$suite"
    RB_REPO_ROOT="$repo_root"
    RB_OUT="${VOLVISOR_TEST_OUTPUT_DIR:-$RB_REPO_ROOT/target/real-backends/$suite}"
    RB_START_UTC="$(rb_now)"
    mkdir -p "$RB_OUT"
    : >"$RB_OUT/preflight.log"
    : >"$RB_OUT/errors.txt"
    : >"$RB_OUT/environment.txt"
    trap rb_on_exit EXIT
}

rb_on_exit() {
    local status=$?
    trap - EXIT
    set +e
    if [[ -n "$RB_OUT" ]]; then
        {
            echo "suite=$RB_SUITE"
            echo "started_utc=$RB_START_UTC"
            echo "finished_utc=$(rb_now)"
            echo "exit_code=$status"
            if [[ "$status" -eq 0 ]]; then
                echo "result=passed"
            else
                echo "result=failed"
            fi
        } >"$RB_OUT/status.txt"
        python3 "$RB_REPO_ROOT/scripts/lib/libtest-junit.py" \
            --suite "$RB_SUITE" \
            --log "$RB_OUT/run.log" \
            --errors "$RB_OUT/errors.txt" \
            --out "$RB_OUT/junit.xml" \
            --exit-code "$status" \
            --timestamp "$RB_START_UTC" >/dev/null 2>&1 || true
    fi
    if [[ "$RB_CLEANUP_KEYRING" -eq 1 && -n "$RB_KEYRING_FILE" ]]; then
        rm -f -- "$RB_KEYRING_FILE"
    fi
    if [[ "$RB_CLEANUP_SECRET" -eq 1 && -n "$RB_DRBD_SECRET_FILE" ]]; then
        rm -f -- "$RB_DRBD_SECRET_FILE"
    fi
    exit "$status"
}

# Fail loudly with a typed, grep-able code. Exit 2 distinguishes an
# unmet prerequisite from a test failure (exit 1 from cargo is turned
# into a typed failure too, still exit 2, because the gate did not pass).
rb_fail() {
    local code="$1"
    shift
    local message="${*:-unspecified prerequisite failure}"
    {
        echo
        echo "FAILED [$code]: $message"
        echo "  This is a typed gate failure, not a skip."
        echo "  The real-backend suite did not run and the release gate must fail."
        echo "  Triage path: docs/testing/real-backends.md#failure-triage"
    } >&2
    if [[ -n "$RB_OUT" ]]; then
        printf '%s\t%s\n' "$code" "$message" >>"$RB_OUT/errors.txt"
        printf 'FAILED [%s]: %s\n' "$code" "$message" >>"$RB_OUT/preflight.log"
    fi
    exit 2
}

rb_warn() {
    local code="$1"
    shift
    local message="${*:-warning}"
    printf 'WARNING [%s]: %s\n' "$code" "$message" >&2
    if [[ -n "$RB_OUT" ]]; then
        printf '%s\t%s\n' "$code" "$message" >>"$RB_OUT/warnings.txt"
    fi
}

rb_log() {
    printf '%s\n' "$*" | tee -a "$RB_OUT/preflight.log"
}

rb_record() {
    printf '%s\n' "$*" >>"$RB_OUT/environment.txt"
}

rb_write_environment() {
    {
        echo "suite=$RB_SUITE"
        echo "started_utc=$RB_START_UTC"
        echo "repo_root=$RB_REPO_ROOT"
        echo "git_commit=$(git -C "$RB_REPO_ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
        echo "git_describe=$(git -C "$RB_REPO_ROOT" describe --tags --always --dirty 2>/dev/null || echo unknown)"
        echo "uname=$(uname -a)"
        echo "identity=$(id)"
        echo "cargo=$(cargo --version 2>/dev/null || echo missing)"
        echo "rustc=$(rustc --version 2>/dev/null || echo missing)"
    } >>"$RB_OUT/environment.txt"
}

rb_require_root() {
    local prefix="$1"
    if [[ "$(id -u)" -ne 0 ]]; then
        rb_fail "$prefix.not-root" \
            "the real-backend suite needs root (rbd map / device-mapper / DRBD device nodes). Re-run as root or through sudo, e.g. 'sudo -E env \"PATH=\$PATH\" $0'."
    fi
}

rb_require_tool() {
    local prefix="$1"
    local tool="$2"
    if ! command -v "$tool" >/dev/null 2>&1; then
        rb_fail "$prefix.missing-tool.$tool" \
            "required command '$tool' was not found in PATH. Install the package documented in docs/testing/real-backends.md#packages; this gate cannot run without it."
    fi
}

rb_require_env_var() {
    local prefix="$1"
    local var="$2"
    if [[ -z "${!var:-}" ]]; then
        rb_fail "$prefix.missing-env.$var" \
            "required environment variable $var is unset or empty. Export the rig configuration documented in docs/testing/real-backends.md#configuration-repository-variables-and-secrets; the suite refuses to guess a cluster or peer."
    fi
}

rb_require_regular_file() {
    local prefix="$1"
    local var="$2"
    local path="$3"
    if [[ ! -e "$path" ]]; then
        rb_fail "$prefix.missing-file.$var" \
            "$var points at '$path', which does not exist. Provision the file as documented in docs/testing/real-backends.md."
    fi
    if [[ ! -f "$path" ]]; then
        rb_fail "$prefix.not-a-file.$var" \
            "$var points at '$path', which is not a regular file (follow symlinks only to a root-owned secret file)."
    fi
}

# Refuse a secret file readable by group/other. GNU stat is used because
# the runners are Linux; the release workflow runs on Linux only.
rb_require_private_file() {
    local prefix="$1"
    local var="$2"
    local path="$3"
    rb_require_regular_file "$prefix" "$var" "$path"
    local mode
    mode="$(stat -Lc '%a' "$path")"
    if (( (8#$mode & 63) != 0 )); then
        rb_fail "$prefix.insecure-secret-mode.$var" \
            "$var points at '$path' with mode $mode; group/other permission bits are set. chmod 600 (or 400) the file; least-privilege handling is mandatory."
    fi
    if [[ ! -s "$path" ]]; then
        rb_fail "$prefix.empty-secret.$var" \
            "$var points at '$path', which is empty. The shared secret/keyring cannot be empty."
    fi
}

rb_run() {
    printf '%s\n' "$*" >"$RB_OUT/command.txt"
    set +e
    "$@" 2>&1 | tee "$RB_OUT/run.log"
    local status=${PIPESTATUS[0]}
    set -e
    return "$status"
}

rb_assert_no_skip_note() {
    local prefix="$1"
    local pattern="$2"
    if grep -Fq -- "$pattern" "$RB_OUT/run.log"; then
        rb_fail "$prefix.suite-skipped" \
            "the test binary printed its skip note ('$pattern') and exited successfully; a skipped real-backend suite is a FAILED release gate. Check the environment variables and rerun through this script."
    fi
}

rb_assert_test_ok() {
    local prefix="$1"
    local name="$2"
    if ! grep -Fq -- "test $name ... ok" "$RB_OUT/run.log"; then
        rb_fail "$prefix.test-not-run.$name" \
            "expected test '$name' did not report '... ok'. The suite is not proven by a compile-only or partially filtered run; see $RB_OUT/run.log."
    fi
}

rb_finish() {
    echo "PASS: real-$RB_SUITE gate satisfied; evidence: $RB_OUT"
    exit 0
}

rb_check_only_finish() {
    echo "PASS: real-$RB_SUITE prerequisites are satisfied (--check only; tests were not run)"
    exit 0
}
