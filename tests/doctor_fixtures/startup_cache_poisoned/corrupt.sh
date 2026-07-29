#!/usr/bin/env bash
# Fixture: startup_cache_poisoned
# FM: fm-configs-startup-cache-poisoned (P2)
#
# Plant a poisoned `startup-*.json` file under a fixture-scoped cache
# directory. The doctor's `startup_cache.health` check should flag it;
# `--repair` should quarantine it into <run-dir>/quarantine/startup-cache/.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

# The harness pins HOME=$target_dir and strips BR_STARTUP_CACHE so the
# binary resolves the cache dir via the HOME fallback at
# $HOME/.cache/beads/startup/. We plant the poisoned files there so
# `br doctor` finds them without any extra env setup at invocation time.
CACHE_DIR="$target_dir/.cache/beads/startup"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1
mkdir -p "$CACHE_DIR"

# Prime the real startup-cache entry for this workspace, then poison that exact
# file. The production startup path reads only startup-<current-key>.json, not
# every startup-*.json in the directory.
BR_STARTUP_CACHE=1 "$tool_bin" info --json >/dev/null 2>&1
current_cache_file="$(find "$CACHE_DIR" -maxdepth 1 -type f -name 'startup-*.json' | sort | head -n 1)"
if [ -z "${current_cache_file:-}" ]; then
  echo "failed to prime startup cache under $CACHE_DIR" >&2
  exit 1
fi
printf '%s\n' "$current_cache_file" > .fixture_cache_file
printf 'not-json-at-all\n' > "$current_cache_file"

# Plant an unrelated corrupt startup cache entry. The detector/fixer must ignore
# it because it is not the current workspace's key.
printf '{"hello":"world"}\n' > "$CACHE_DIR/startup-deadbeef.json"

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
