#!/usr/bin/env bash
# Run the executable day-2 drills (D1-D7 plus the D2 live cross-binary drill)
# against one RUN directory.
#
# D8 (total host loss / re-admission) is deliberately not executable here;
# it is documented as NOT RUN in docs/runbooks/day-2/host-loss-and-rejoin.md.
# The preflight below is a readiness report, not a drill result, and does not
# turn D3 storage-side fencing or D8 into executed evidence.
#
# Requirements: Linux root, lvm2, loop devices, util-linux, openssl,
# jq, python3, curl, git, cargo. This is a lab harness, not a production tool.
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

echo
echo "===== preflight (report only; does not execute D3 storage-side or D8) ====="
bash "$HERE/preflight.sh" || true

for drill in d1 d2 d2-live d3 d4 d5 d6 d7; do
  echo
  echo "===== $drill ====="
  if ! bash "$HERE/$drill.sh"; then
    echo "DRILL $drill FAILED; stopping (see $RUN)"
    exit 1
  fi
done

echo
echo "all executable drills completed: $RUN"
echo "D8 total host loss remains NOT RUN (no multi-host/BMC/fencing); see host-loss-and-rejoin.md"
ls -l "$RUN/raw"
