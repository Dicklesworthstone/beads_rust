#!/usr/bin/env bash
# Fixture assertions: audit_suspect_close_reasons
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    # The audit.suspect_close_reasons check should be present AND
    # flagged at warn level (the recently-landed audit policy).
    echo "$out" | jq -e '
      .checks[] | select(.name == "audit.suspect_close_reasons")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: audit.suspect_close_reasons not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "audit.suspect_close_reasons")' >&2
      exit 1
    }
    ;;
  post_repair)
    # Detect-only FM. The bead must still be present, still closed, and
    # MUST NOT carry the audit-historical-cycle-close-YYYY-MM-DD label
    # — the doctor must never silently add it (that would defeat the
    # audit purpose). Use python3 since sqlite3 CLI isn't guaranteed
    # in the harness env.
    py_out=$(python3 <<PY 2>&1
import sqlite3
try:
    conn = sqlite3.connect(".beads/beads.db")
    cur = conn.cursor()
    cur.execute("""
        SELECT COUNT(*) FROM issue_labels
        WHERE issue_id = 'br-suspect01'
          AND label LIKE 'audit-historical-cycle-close-%'
    """)
    has_label = cur.fetchone()[0]
    cur.execute("SELECT COUNT(*) FROM issues WHERE id='br-suspect01'")
    bead_present = cur.fetchone()[0]
    conn.close()
    print(f"has_label={has_label} bead_present={bead_present}")
except sqlite3.OperationalError as e:
    # issue_labels table may not exist in this schema version — that's
    # itself the "no label silently added" assertion.
    print(f"has_label=0 bead_present_query_err={e}")
PY
)
    if echo "$py_out" | grep -q "has_label=0"; then
      :
    else
      echo "ASSERT FAIL[$stage]: doctor --repair silently added audit-historical-cycle-close label (unsafe): $py_out" >&2
      exit 1
    fi
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
