#!/usr/bin/env bash
# Run the default integration scenarios for durable provider/journal semantics.
#
#   scripts/test-default-scenarios.sh [--list-only]
#
# The normative test list is docs/testing/default-integration-scenarios.tsv;
# docs/testing/default-integration-scenarios.md describes each scenario. This
# runner verifies that every exact test is present in its cargo test binary,
# that the human catalog mentions it, and then runs it with `--exact`. A
# missing, renamed, ignored or failing test fails the gate.
#
# This is a control-plane regression gate, not production-support evidence.
# It does not run real backends, a long soak, a real VMM or a multi-host rig.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
CATALOG="$ROOT/docs/testing/default-integration-scenarios.tsv"
DOC="$ROOT/docs/testing/default-integration-scenarios.md"

LIST_ONLY=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --list-only) LIST_ONLY=1; shift ;;
    -h|--help) sed -n '1,16p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

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
checked=0
executed=0

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

  if [[ "$LIST_ONLY" -eq 1 ]]; then
    echo "PRESENT $id $test_name"
    continue
  fi

  if ! cargo test -q --locked -p "$pkg" "${targs[@]}" -- \
    --exact --test-threads=1 "$test_name" >"$TMP/run.out" 2>&1; then
    cat "$TMP/run.out" >&2
    die "test failed: $id $pkg $target $test_name"
  fi
  # `--exact` with a filter that matches nothing exits 0 with "0 passed".
  # An ignored test exits 0 with "0 passed; ... 1 ignored". Both must fail
  # the gate, so require exactly one passing and zero ignored tests.
  if ! grep -Eq 'test result: ok\. 1 passed; 0 failed; 0 ignored' "$TMP/run.out"; then
    cat "$TMP/run.out" >&2
    die "test did not run exactly once (missing, filtered or ignored): $id $test_name"
  fi
  echo "PASS $id $test_name"
  executed=$((executed + 1))
done <"$CATALOG"

[[ "$checked" -gt 0 ]] || die "catalog contains no scenarios"

if [[ "$LIST_ONLY" -eq 1 ]]; then
  echo "default scenarios: $checked present (list-only)"
else
  echo "default scenarios: $executed passed"
fi
