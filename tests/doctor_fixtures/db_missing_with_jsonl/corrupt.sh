#!/usr/bin/env bash
# Fixture: db_missing_with_jsonl
# FM: fm-state_files-empty-or-truncated-database (P1) — the database
# file is gone entirely while the JSONL export survives (crash mid-
# recovery, overzealous cleanup, partial clone). `db.exists` reports
# error; `--repair` rebuilds the DB from the surviving JSONL.
#
# Distinct from the sibling `empty_database_with_jsonl` fixture, which
# plants a present-but-schema-empty DB: this one removes the file
# family outright, exercising the `db.exists` error branch.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "survivor one" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" create --title "survivor two" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_pre_sha256

# Remove the whole DB family; the JSONL is the only surviving store.
rm -f .beads/beads.db .beads/beads.db-wal .beads/beads.db-shm

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
