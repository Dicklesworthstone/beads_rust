#!/usr/bin/env bash
# Fixture: audit_suspect_close_reasons
# FM: fm-agent_coordination-suspect-close-reason (P2) — detect-only.
#
# Plants a closed bead whose close_reason matches one of the audit
# patterns ("forced close due to cycle") without the dated
# audit-historical-cycle-close-YYYY-MM-DD escape-hatch label. The doctor's
# audit.suspect_close_reasons check must flag the bead at warn level so
# operators can review it. Repair must NOT auto-add the label.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Plant the suspect-closed bead directly via SQL. We don't go through
# `br create` + `br close` because the close-policy path may reject the
# forbidden reason — the audit check exists precisely to catch beads
# that bypassed that policy (e.g., via JSONL import from another tool).
sqlite_db=".beads/beads.db"
python3 <<PY
import sqlite3
conn = sqlite3.connect("$sqlite_db")
cur = conn.cursor()
cur.execute("""
    INSERT INTO issues (
        id, title, description, status, priority, issue_type,
        created_at, updated_at, closed_at, close_reason,
        source_repo, compaction_level,
        original_size, content_hash, ephemeral, pinned, is_template
    ) VALUES (
        'br-suspect01',
        'Suspect closed bead',
        'Closed under cycle pressure',
        'closed',
        2,
        'task',
        '2026-04-01T00:00:00Z',
        '2026-04-01T00:00:00Z',
        '2026-04-01T00:00:00Z',
        'Forced close due to cycle.',
        '.',
        0, 0,
        'deadbeef',
        0, 0, 0
    )
""")
# close_reason lives on the issues row itself (already inserted above).
# No events row needed for the audit check — it scans issues.close_reason
# pattern-matched against the audit forbidden-substring list.
conn.commit()
conn.close()
PY

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
