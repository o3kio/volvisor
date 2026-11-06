#!/usr/bin/env bash
# Run the default integration scenarios for durable provider/journal semantics.
#
#   scripts/test-default-scenarios.sh [--list-only]
#       [--per-test-timeout SECONDS] [--budget SECONDS]
#
# The normative test list is docs/testing/default-integration-scenarios.tsv;
# docs/testing/default-integration-scenarios.md describes each scenario. This
# runner verifies that every exact test is present in its cargo test binary,
# that the human catalog mentions it, and then runs it with `--exact`. A
# missing, renamed, ignored, failing or over-budget test fails the gate.
#
# The acceptance budget is 60 s for any single test and 300 s for the whole
# warm execution (after the test binaries are listed/built). Long-running
# work — soaks, real backends, campaign tiers, fault campaigns — is not a
# default scenario and belongs in its own scheduled or evidence lane.
#
# This is a control-plane regression gate, not production-support evidence.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
CATALOG="$ROOT/docs/testing/default-integration-scenarios.tsv"
DOC="$ROOT/docs/testing/default-integration-scenarios.md"

LIST_ONLY=0
PER_TEST_TIMEOUT=60
TOTAL_BUDGET=300
while [[ $# -gt 0 ]]; do
  case "$1" in
    --list-only) LIST_ONLY=1; shift ;;
    --per-test-timeout)
      [[ $# -ge 2 ]] || { echo "missing value for $1" >&2; exit 2; }
      PER_TEST_TIMEOUT="$2"; shift 2 ;;
    --budget)
      [[ $# -ge 2 ]] || { echo "missing value for $1" >&2; exit 2; }
      TOTAL_BUDGET="$2"; shift 2 ;;
    -h|--help) sed -n '1,20p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ "$PER_TEST_TIMEOUT" =~ ^[1-9][0-9]*$ ]] \
  || { echo "--per-test-timeout must be a positive integer" >&2; exit 2; }
[[ "$TOTAL_BUDGET" =~ ^[1-9][0-9]*$ ]] \
  || { echo "--budget must be a positive integer" >&2; exit 2; }

die() {
  echo "FAILED [default-scenarios]: $*" >&2
  exit 1
}

[[ -f "$CATALOG" ]] || die "missing catalog: $CATALOG"
[[ -f "$DOC" ]] || die "missing catalog document: $DOC"

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

declare -A LIST_FILES=()
declare -A SEEN_IDS=()
declare -A SEEN_TESTS=()
ROWS="$TMP/rows.tsv"
: >"$ROWS"
checked=0

# Pass 1: validate the catalog, docs and test presence, and warm the test
# binaries. Compilation and listing are outside the execution budget so the
# budget measures the tests themselves.
while IFS=$'\t' read -r id group pkg target test_name invariant || [[ -n "${id:-}" ]]; do
  [[ -z "$id" || "$id" == \#* || "$id" == scenario_id ]] && continue
  [[ -n "$group" && -n "$pkg" && -n "$target" && -n "$test_name" && -n "$invariant" ]] \
    || die "malformed catalog row: $id"

  [[ -z "${SEEN_IDS[$id]:-}" ]] || die "duplicate scenario id: $id"
  SEEN_IDS[$id]=1

  read -r -a targs <<<"$target"
  key="$pkg ${targs[*]}"
  [[ -z "${SEEN_TESTS[$key $test_name]:-}" ]] \
    || die "duplicate scenario test $pkg $target $test_name"
  SEEN_TESTS["$key $test_name"]=1

  checked=$((checked + 1))

  grep -Fq -- "$id" "$DOC" || die "scenario $id is not documented in $DOC"
  grep -Fq -- "$test_name" "$DOC" \
    || die "test $test_name for $id is not named in $DOC"

  # Cache `cargo test -- --list` per package/argument pair so a cold build is
  # paid once per test binary instead of once per scenario.
  file="${LIST_FILES[$key]:-}"
  if [[ -z "$file" ]]; then
    file="$TMP/list-$checked.txt"
    if ! cargo test -q --locked -p "$pkg" "${targs[@]}" -- --list >"$file" 2>"$TMP/list.err"; then
      cat "$TMP/list.err" >&2
      die "could not list tests for $pkg $target"
    fi
    LIST_FILES[$key]="$file"
  fi

  if ! awk -v want="$test_name" \
    '{ sub(/: (test|bench)$/, ""); if ($0 == want) found = 1 } END { exit !found }' \
    "$file"; then
    die "test not present: $id $pkg $target $test_name"
  fi

  printf '%s\t%s\t%s\t%s\n' "$id" "$pkg" "$target" "$test_name" >>"$ROWS"
  if [[ "$LIST_ONLY" -eq 1 ]]; then
    echo "PRESENT $id $test_name"
  fi
done <"$CATALOG"

[[ "$checked" -gt 0 ]] || die "catalog contains no scenarios"

if [[ "$LIST_ONLY" -eq 1 ]]; then
  echo "default scenarios: $checked present (list-only; per-test limit ${PER_TEST_TIMEOUT}s, execution budget ${TOTAL_BUDGET}s)"
  exit 0
fi

command -v timeout >/dev/null 2>&1 \
  || die "coreutils 'timeout' is required to enforce the acceptance budget"

# Pass 2: run each exact test under the per-test timeout and the total budget.
start_ns=$(date +%s%N)
executed=0
slowest_ms=0
slowest_id="none"

while IFS=$'\t' read -r id pkg target test_name; do
  read -r -a targs <<<"$target"
  test_start_ns=$(date +%s%N)
  timeout --kill-after=5s "${PER_TEST_TIMEOUT}s" \
    cargo test -q --locked -p "$pkg" "${targs[@]}" -- \
    --exact --test-threads=1 "$test_name" >"$TMP/run.out" 2>&1
  rc=$?
  if [[ "$rc" -ne 0 ]]; then
    cat "$TMP/run.out" >&2
    if [[ "$rc" -eq 124 || "$rc" -eq 137 ]]; then
      die "test exceeded the ${PER_TEST_TIMEOUT}s per-test limit: $id $test_name"
    fi
    die "test failed: $id $pkg $target $test_name"
  fi
  test_end_ns=$(date +%s%N)
  elapsed_ms=$(((test_end_ns - test_start_ns) / 1000000))

  # `--exact` with a filter that matches nothing exits 0 with "0 passed".
  # An ignored test exits 0 with "0 passed; ... 1 ignored". Both must fail
  # the gate, so require exactly one passing and zero ignored tests.
  if ! grep -Eq 'test result: ok\. 1 passed; 0 failed; 0 ignored' "$TMP/run.out"; then
    cat "$TMP/run.out" >&2
    die "test did not run exactly once (missing, filtered or ignored): $id $test_name"
  fi

  echo "PASS $id (${elapsed_ms} ms) $test_name"
  executed=$((executed + 1))
  if [[ "$elapsed_ms" -gt "$slowest_ms" ]]; then
    slowest_ms="$elapsed_ms"
    slowest_id="$id"
  fi

  now_ns=$(date +%s%N)
  total_ms=$(((now_ns - start_ns) / 1000000))
  if [[ "$total_ms" -gt $((TOTAL_BUDGET * 1000)) ]]; then
    die "execution budget of ${TOTAL_BUDGET}s exceeded after $id"
  fi
done <"$ROWS"

total_ms=$((($(date +%s%N) - start_ns) / 1000000))
echo "default scenarios: $executed passed in $((total_ms / 1000)).$(printf '%03d' $((total_ms % 1000))) s (slowest $slowest_id ${slowest_ms} ms; per-test limit ${PER_TEST_TIMEOUT}s, execution budget ${TOTAL_BUDGET}s)"
