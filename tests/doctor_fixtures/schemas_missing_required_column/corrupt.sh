#!/usr/bin/env bash
# Fixture: schemas_missing_required_column
# FM: fm-schemas-missing-required-column (P1)
#
# Drops the required `comments.text` column (one of the columns the
# `schema.columns` check actively watches). `--repair` reapplies the
# canonical schema via the JSONL→DB rebuild path, which re-creates the
# comments table with the full column set.
#
# Note: the rebuild path is destructive at the table level (full rebuild),
# which is acceptable here because the lost column would have been NULL on
# every existing row anyway. Test harness uses raw DROP COLUMN; the fixer
# never DROPs.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "missing-column seed" --type task --priority 2 --json >/dev/null
"$tool_bin" sync --flush-only >/dev/null 2>&1 || true

# Drop indexes that reference the column first (SQLite's ALTER TABLE DROP
# COLUMN refuses to drop columns referenced by indexes). Then drop the
# column. Test harness only.
printf '%s\n' \
  "DROP INDEX IF EXISTS idx_comments_issue_id;" \
  "DROP INDEX IF EXISTS idx_comments_created_at;" \
  "ALTER TABLE comments DROP COLUMN text;" \
  | sqlite3 .beads/beads.db

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
