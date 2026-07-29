#!/usr/bin/env bash
# Fixture: labels_orphans
# FM: fm-caches_indexes-labels-orphans (P2) — orphan rows in labels
# table (issue_id references an issue that no longer exists).
#
# Creates a workspace with valid issues, then inserts an orphan label
# row with FK guard off so the CASCADE doesn't kick in. Doctor's
# `labels.orphans` check should fire warn; `--repair` should
# surgically delete the orphan via Op::DbExec; `doctor undo` should
# restore the orphan row from the chokepoint snapshot.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "fixture issue 1" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" create --title "fixture issue 2" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# JSONL byte-checksum before corruption — pruning unreachable
# orphan label rows must NEVER touch JSONL.
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_pre_sha256

python3 <<'PY'
import sqlite3
conn = sqlite3.connect(".beads/beads.db")
cur = conn.cursor()
cur.execute("PRAGMA foreign_keys = OFF")
cur.execute(
    "INSERT INTO labels(issue_id, label) VALUES (?, ?)",
    ("bd-ghost-fixture-l", "ghost-label"),
)
conn.commit()
conn.close()
PY

# Snapshot the orphan row identity so post_undo can verify the
# byte-deterministic restore.
echo "bd-ghost-fixture-l|ghost-label" > .fixture_orphan_key

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
