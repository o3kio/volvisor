#!/usr/bin/env bash
# Run the executable day-2 drills (D1-D7) against one RUN directory.
# D8 (total host loss / re-admission) is deliberately not executable here;
# it is documented as NOT RUN in docs/runbooks/day-2/host-loss-and-rejoin.md.
#
# Requirements: Linux root, lvm2, loop devices, util-linux, openssl,
# jq, python3, curl. This is a lab harness, not a production tool.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="${REPO:-$(cd "$HERE/../../../.." && pwd)}"
export REPO
export RUN="${RUN:-/tmp/opencode/volvisor-day2-$(date -u +%Y%m%dT%H%M%SZ)}"
export BIN="${BIN:-$REPO/target/debug}"
mkdir -p "$RUN"

echo "repo=$REPO"
echo "bin=$BIN"
echo "run=$RUN"
echo "source_sha=$(git -C "$REPO" rev-parse HEAD)"
echo "worktree_dirty=$(test -n "$(git -C "$REPO" status --porcelain)" && echo yes || echo no)"

for drill in d1 d2 d3 d4 d5 d6 d7; do
  echo
  echo "===== $drill ====="
  if ! bash "$HERE/$drill.sh"; then
    echo "DRILL $drill FAILED; stopping (see $RUN)"
    exit 1
  fi
done

echo
echo "all executable drills completed: $RUN"
ls -l "$RUN/raw"
