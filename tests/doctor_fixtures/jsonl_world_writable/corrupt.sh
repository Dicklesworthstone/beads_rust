#!/usr/bin/env bash
# Fixture: jsonl_world_writable
# FM: fm-permissions-jsonl-world-writable (P2) — security concern
# when .beads/issues.jsonl is world-writable.
#
# Initialises a workspace, sync flushes JSONL, then chmods the file
# to 0o666 to set the world-write bit. Doctor's
# `permissions.jsonl_world_writable` check should fire warn;
# `--repair` should strip the world-write bit via Op::Chmod;
# `doctor undo` should restore the original 0o666 mode.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "fixture issue 1" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" create --title "fixture issue 2" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Capture JSONL bytes pre-corruption so we can assert they're
# untouched throughout (Op::Chmod only changes mode, not bytes).
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_pre_sha256

# Set world-write bit.
chmod 0666 .beads/issues.jsonl

# Snapshot the corrupted mode so post_undo can verify byte-
# deterministic restore.
python3 - .beads/issues.jsonl > .fixture_pre_mode <<'PY'
import os
import sys

print(format(os.stat(sys.argv[1]).st_mode & 0o777, "o"))
PY

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
