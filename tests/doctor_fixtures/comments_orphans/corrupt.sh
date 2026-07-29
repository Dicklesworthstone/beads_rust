#!/usr/bin/env bash
# Fixture: comments_orphans
# FM: fm-caches_indexes-comments-orphans (P2) — orphan rows in comments
# table (issue_id references an issue that no longer exists).
#
# Creates a workspace with valid issues + comments, then inserts an
# orphan comment row with FK guard off so the FK CASCADE doesn't kick
# in. Doctor's `comments.orphans` check should fire warn; `--repair`
# should surgically delete the orphan via Op::DbExec; `doctor undo`
# should restore the orphan row from the chokepoint snapshot.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Create a couple of valid issues so the workspace has non-empty
# context the orphan can be observed against.
"$tool_bin" create --title "fixture issue 1" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" create --title "fixture issue 2" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Capture the JSONL bytes pre-corruption so we can later assert
# they're unchanged across the entire round-trip. The JSONL is
# derived from the DB; pruning orphan COMMENT rows should never
# affect the JSONL bytes (comments are stored per-issue and an
# orphan row by definition isn't reachable from any issue).
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_pre_sha256

# Inject an orphan comment row referencing a non-existent issue.
# PRAGMA foreign_keys=OFF lets the INSERT succeed where the CASCADE
# would otherwise prevent it. Uses python3 because the sqlite3 CLI
# isn't guaranteed in the harness env.
python3 <<'PY'
import sqlite3
conn = sqlite3.connect(".beads/beads.db")
cur = conn.cursor()
cur.execute("PRAGMA foreign_keys = OFF")
cur.execute(
    "INSERT INTO comments(issue_id, text, created_at, author) "
    "VALUES (?, ?, ?, ?)",
    ("bd-ghost-fixture", "orphan body", "2026-05-16T00:00:00Z", "ghost"),
)
conn.commit()
conn.close()
PY

# Snapshot the orphan row's body so post_undo can verify the
# byte-deterministic restore.
echo "orphan body" > .fixture_orphan_body

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
