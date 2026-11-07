#!/usr/bin/env bash
# Fixture: schemas_missing_required_table
# FM: fm-schemas-missing-required-table (P0)
#
# Drop export_hashes from a readable current-version database. This is outside
# the reviewed upgrade and derived-cache maintenance profiles. Generic repair
# must preserve the unsupported source and refuse JSONL reconstruction.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "missing-table seed 1" --type task --priority 2 --json >/dev/null
"$tool_bin" create --title "missing-table seed 2" --type task --priority 1 --json >/dev/null
"$tool_bin" sync --flush-only >/dev/null 2>&1

# These records are deliberately absent from the already-exported JSONL.
# Duplicate KV rows, explicit rowids, and audit attribution must survive a
# refusal; reconstructing only issues from JSONL would lose this authority.
sqlite3 .beads/beads.db <<'SQL'
BEGIN IMMEDIATE;
INSERT INTO config (rowid, key, value) VALUES (70001, 'fixture-db-only-config', 'older private value'), (70009, 'fixture-db-only-config', 'newer private value');
INSERT INTO metadata (rowid, key, value) VALUES (71001, 'fixture-db-only-metadata', 'older private value'), (71009, 'fixture-db-only-metadata', 'newer private value');
INSERT INTO events (id, issue_id, event_type, actor, comment, agent_name, harness, model)
SELECT 72001, id, 'created', 'fixture-db-only-actor', 'fixture-db-only-history', 'fixture-db-only-agent', 'fixture-db-only-harness', 'fixture-db-only-model' FROM issues ORDER BY id LIMIT 1;
DROP TABLE export_hashes;
COMMIT;
SQL

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
