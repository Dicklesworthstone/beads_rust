#!/usr/bin/env bash
# Fixture: schemas_missing_required_table
# FM: fm-schemas-missing-required-table (P0)
#
# Drops the `export_hashes` table — one of the canonical required tables
# enumerated by the `schema.tables` check. The detector surfaces the
# missing table; `--repair` reapplies `apply_schema()` via the JSONL→DB
# rebuild path, which is forward-only additive (CREATE TABLE IF NOT EXISTS),
# restoring the table without touching existing rows.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "missing-table seed 1" --type task --priority 2 --json >/dev/null
"$tool_bin" create --title "missing-table seed 2" --type task --priority 1 --json >/dev/null
"$tool_bin" sync --flush-only >/dev/null 2>&1 || true

# Drop a required table that the doctor's check_schema_tables list watches.
# Test harness DROPs; the fixer never DROPs.
printf '%s\n' "DROP TABLE IF EXISTS export_hashes;" | sqlite3 .beads/beads.db

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
