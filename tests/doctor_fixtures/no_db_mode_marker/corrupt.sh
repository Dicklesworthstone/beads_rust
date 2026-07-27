#!/usr/bin/env bash
# Fixture: no_db_mode_marker
# FM: fm-state_files-no-db-mode-db-checks-skipped (P4, informational) —
# issue #329's `--no-db` (JSONL-only) mode marker. Nothing is corrupt:
# the marker's contract is that a reduced JSONL-only doctor run says so
# EXPLICITLY (an enumerated `db.no_db_mode` check) rather than silently
# omitting the DB-backed findings.
#
# The "planted state" is simply a healthy workspace; the assert stage
# drives `br doctor --no-db --json` itself and pins the marker's shape.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "healthy issue" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
