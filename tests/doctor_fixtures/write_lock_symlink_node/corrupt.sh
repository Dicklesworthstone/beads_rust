#!/usr/bin/env bash
# Fixture: write_lock_symlink_node
# FM: fm-concurrency_primitives-orphaned-write-lock (beads_rust-5sej) — detect-only.
#
# Replaces `.beads/.write.lock` with a symlink to a sibling regular file.
# Startup `OpenOptions` follows the symlink, so the workspace still opens —
# but mutual exclusion has silently moved to the target inode. Doctor must
# fail closed: classify the non-regular node as an error from lstat alone,
# never traverse or replace it.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

rm -f .beads/.write.lock
: > .beads/.lock_target
ln -s .lock_target .beads/.write.lock
# GNU stat first, BSD stat fallback (macOS workers).
{ stat -c '%d:%i' .beads/.lock_target 2>/dev/null \
    || stat -f '%d:%i' .beads/.lock_target; } > .fixture_target_identity

echo "fixture corrupt.sh: planted symlinked .write.lock -> .lock_target" >&2
