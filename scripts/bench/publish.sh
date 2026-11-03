#!/usr/bin/env bash
# Copy a benchmark run into a committed docs bundle with secrets redacted.
#
#   scripts/bench/publish.sh --run-dir DIR --dest docs/benchmarks/<date>-devhost
#
# REPORT.md is authored separately; this script only copies raw evidence and
# sanitized configuration. It refuses to publish if the run's token appears in
# any copied file.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091
source "$HERE/lib.sh"

SOURCE=""
DEST=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --run-dir) SOURCE="$2"; shift 2 ;;
    --dest) DEST="$2"; shift 2 ;;
    -h|--help) sed -n '1,12p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ -n "$SOURCE" && -d "$SOURCE" ]] || die "--run-dir must name an existing run directory"
[[ -n "$DEST" ]] || die "--dest is required"
[[ -f "$SOURCE/configs/token" ]] || die "run has no configs/token; refusing an unverifiable bundle"

TOKEN="$(cat "$SOURCE/configs/token")"
[[ -n "$TOKEN" ]] || die "empty token file; refusing"

mkdir -p "$DEST"/{raw,csv,json,records,summary,configs}

copy_dir() {
  local name="$1"
  [[ -d "$SOURCE/$name" ]] || return 0
  while IFS= read -r -d '' path; do
    local rel="${path#"$SOURCE/"}"
    mkdir -p "$DEST/$(dirname "$rel")"
    cp -p "$path" "$DEST/$rel"
  done < <(find "$SOURCE/$name" -type f -print0)
}

copy_dir raw
copy_dir csv
copy_dir json
copy_dir summary

for rel in ENVIRONMENT.txt records/vgs-before.txt records/vgs-after.txt \
  records/lab.vg records/lab.env records/main.loop records/main.vg; do
  if [[ -f "$SOURCE/$rel" ]]; then
    mkdir -p "$DEST/$(dirname "$rel")"
    cp -p "$SOURCE/$rel" "$DEST/$rel"
  fi
done

# Sanitized configurations: keep every benchmark-relevant knob, replace the
# bearer/claim token with a literal marker.
for config in volvisord volvisord-cap32 volvisord-rate50; do
  if [[ -f "$SOURCE/configs/$config.toml" ]]; then
    sed "s|$TOKEN|<redacted>|g" "$SOURCE/configs/$config.toml" >"$DEST/configs/$config.toml"
    chmod 644 "$DEST/configs/$config.toml"
  fi
done

if grep -rIl -- "$TOKEN" "$DEST" >/dev/null 2>&1; then
  grep -rIl -- "$TOKEN" "$DEST" >&2
  die "run token found in the published bundle; refusing to publish secrets"
fi

# Forge hosting rejects individual files above 100 MiB. Raw daemon logs are
# the only unbounded artifact (the 25-minute soak produced a 606 MiB one), so
# compress oversized raw logs in place after the plaintext token check.
# `gzip -n` omits the timestamp so the committed bytes are reproducible.
while IFS= read -r -d '' path; do
  gzip -9 -n -f "$path"
done < <(find "$DEST/raw" -type f -size +50M -print0)

# An authored report must exist before the manifest is sealed.
if [[ ! -f "$DEST/REPORT.md" ]]; then
  printf 'WARNING: %s/REPORT.md is missing; the bundle is evidence, not a report\n' "$DEST" >&2
fi

write_checksums "$DEST"
printf 'published benchmark bundle: %s\n' "$DEST"
