#!/usr/bin/env bash
# Fixture: jsonl_trailing_newline
# FM: fm-state_files-jsonl-missing-trailing-newline (P2) — JSONL
# export has no trailing `\n` (last record gets silently skipped by
# many line-oriented tools).
#
# Initialises a workspace, sync flushes JSONL, then strips the
# trailing `\n`. Doctor's `jsonl_eof_newline` check should fire
# warn; `--repair` should append `\n` via Op::AppendFile;
# `doctor undo` should restore the no-trailing-newline state.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "fixture issue 1" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" create --title "fixture issue 2" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Capture the trailing-newline baseline — post_repair must match this.
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_post_repair_sha256

# Strip the trailing newline if present. Python is used because POSIX
# `truncate -s -1` is GNU-specific and not guaranteed in CI.
python3 - <<'PY'
import os
path = ".beads/issues.jsonl"
with open(path, "rb") as f:
    data = f.read()
if data.endswith(b"\n"):
    with open(path, "wb") as f:
        f.write(data[:-1])
PY

# Snapshot the corrupted bytes — post_undo must match this.
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_pre_sha256

# Sanity check: last byte must NOT be \n right now.
last_byte=$(tail -c 1 .beads/issues.jsonl | xxd -p)
if [ "$last_byte" = "0a" ]; then
  echo "corrupt: failed to strip trailing newline (last byte still 0x0a)" >&2
  exit 1
fi

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
