#!/usr/bin/env bash
# Fixture: metadata_json_malformed
# FM: fm-state_files-metadata-json-stale-or-malformed (P1) — ParseError variant
#
# Overwrites .beads/metadata.json with non-JSON bytes. Triggers the
# `metadata` check = error with "Failed to read metadata.json: JSON error".
# Safety: doctor's repair path is detect-only here; we assert the malformed
# bytes are preserved (operator review required).

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1
"$tool_bin" create --title "meta-malformed seed" --type task --priority 2 --json >/dev/null

# Plant invalid JSON. Capture the planted bytes so post-repair assertions can
# verify they survive.
printf '%s' '{not valid json {{' > .beads/metadata.json

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
