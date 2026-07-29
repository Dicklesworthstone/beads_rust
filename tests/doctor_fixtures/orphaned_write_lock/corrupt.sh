#!/usr/bin/env bash
# Fixture: orphaned_write_lock
# FM: fm-concurrency_primitives-orphaned-write-lock (P1) — detect-only.
#
# Plants a fresh `.beads/.write.lock` regular file. The detector
# normally treats <300s as "still in flight"; we set
# BR_DOCTOR_STALE_LOCK_THRESHOLD_SECS=0 in assert.sh so any
# non-future mtime trips the warn branch.
#
# This is the only fixture that intentionally plants the lock
# itself; production code holds it via flock and never leaves
# a regular file unless the writer crashed.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Plant a regular-file write.lock. fs::write would also work but
# touch keeps the file empty + sets mtime to "now" which is what
# we want.
: > .beads/.write.lock

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
