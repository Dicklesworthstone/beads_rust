#!/usr/bin/env bash
# Fixture: metadata_json_drift
# FM: fm-configs-metadata-json-stale (P1) — detect-only.
#
# Plant a `.beads/metadata.json` that declares a `jsonl_export`
# pointing at a file that doesn't exist on disk. The new pass-2
# detector `check_metadata_json` must surface the drift with the
# field name + the expected path. `--repair` must NOT silently
# rewrite the file.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Overwrite metadata.json to declare a jsonl_export that points at a
# non-existent file. Keep `database` honest with an absolute path so
# ambient BEADS_CACHE_DIR cannot redirect the partial-drift assertion.
printf '{\n  "database": "%s/.beads/beads.db",\n  "jsonl_export": "renamed-by-operator.jsonl"\n}\n' \
  "$target_dir" > .beads/metadata.json

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
