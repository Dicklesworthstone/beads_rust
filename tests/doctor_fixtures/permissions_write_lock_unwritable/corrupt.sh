#!/usr/bin/env bash
# Fixture: permissions_write_lock_unwritable
# FM: fm-state_files-orphaned-write-lock / permissions.write_lock

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

: > .beads/.write.lock
chmod 0444 .beads/.write.lock

mode=$(stat -c '%a' .beads/.write.lock)
if [ "$mode" != "444" ]; then
  echo "corrupt.sh: expected .write.lock mode 444, got $mode" >&2
  exit 1
fi

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

