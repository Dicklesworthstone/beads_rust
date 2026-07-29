#!/usr/bin/env bash
# Fixture: jsonl_row_count_mismatch
# FM: fm-state_files-jsonl-row-count-mismatch (P1)
#
# Appends two synthetic JSONL records to `issues.jsonl` without inserting
# them into the DB. Triggers `counts.db_vs_jsonl` = warn ("DB and JSONL
# counts differ"). Confirms detection. Doctor's auto-fix path here is the
# JSONL-is-authority rebuild, which can be triggered by --repair when the
# error is severe enough; we only require the count mismatch is surfaced.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

for t in one two three four five; do
  "$tool_bin" create --title "$t" --type task --priority 2 --json >/dev/null
done
"$tool_bin" sync --flush-only >/dev/null 2>&1 || true

# Append synthetic records — DB still has 5; JSONL grows to 7.
cat >> .beads/issues.jsonl <<'JSONL'
{"id":"bd-9001","title":"appended-one","status":"open","priority":2,"issue_type":"task","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","assignee":null,"labels":[],"description":"","acceptance_criteria":"","dependencies":[],"epic_id":null,"discovered_by":null,"discovered_from":null,"source_repo":null,"design":null,"notes":null,"closed_at":null,"close_reason":null}
{"id":"bd-9002","title":"appended-two","status":"open","priority":2,"issue_type":"task","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","assignee":null,"labels":[],"description":"","acceptance_criteria":"","dependencies":[],"epic_id":null,"discovered_by":null,"discovered_from":null,"source_repo":null,"design":null,"notes":null,"closed_at":null,"close_reason":null}
JSONL

# Bump JSONL mtime so freshness checks treat it as the newer authority.
# Use Python instead of GNU-only `touch -d` for BSD/macOS fixture hosts.
python3 - .beads/issues.jsonl <<'PY'
import os
import sys
import time

mtime = time.time() + 60 * 60
os.utime(sys.argv[1], (mtime, mtime))
PY

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
