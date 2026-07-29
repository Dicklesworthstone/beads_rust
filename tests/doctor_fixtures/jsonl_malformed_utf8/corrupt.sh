#!/usr/bin/env bash
# Fixture: jsonl_malformed_utf8
# FM: fm-state_files-jsonl-malformed-utf8 (P1)
#
# Appends raw invalid-UTF-8 bytes to the JSONL stream. Triggers
# `jsonl.parse` = error with "stream did not contain valid UTF-8".
#
# Safety gate: `--repair` must NOT silently rewrite the JSONL (the bytes may
# be operator data that needs hand review). The detector continues to flag
# the failure; doctor refuses to auto-fix.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1
"$tool_bin" create --title "utf8 seed" --type task --priority 2 --json >/dev/null
"$tool_bin" sync --flush-only >/dev/null 2>&1 || true

# Append a deterministic invalid-UTF-8 sequence.
printf '\xff\xfe\xfdInvalid bytes follow\n' >> .beads/issues.jsonl

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
