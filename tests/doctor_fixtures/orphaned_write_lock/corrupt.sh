#!/usr/bin/env bash
# Fixture: orphaned_write_lock
# FM: fm-concurrency_primitives-orphaned-write-lock (P1) — detect-only.
#
# Plants an old `.beads/.write.lock` regular file. Production keeps this inode
# across successful commands; its mtime is deliberately irrelevant to the
# advisory lock held by an open file description.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

: > .beads/.write.lock
touch -d '2024-01-01T00:00:00Z' .beads/.write.lock
stat -c '%d:%i' .beads/.write.lock > .fixture_lock_identity

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
