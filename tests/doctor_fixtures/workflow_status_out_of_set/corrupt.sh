#!/usr/bin/env bash
# Fixture: workflow_status_out_of_set
# FM: fm-agent_coordination-workflow-status-out-of-set (P3) — issue
# #311's read-side audit: an issue whose status is outside the strict
# workflow status set (written before the policy existed, or imported
# from an external source that used its own vocabulary).
#
# Plants a strict workflow policy AND a pre-existing offender row whose
# status the policy does not allow. The offender is written directly to
# the DB (bypassing `br update`, which would reject it under the strict
# policy — exactly how real offenders predate their policy).

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "status offender" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" create --title "conforming issue" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

offender_id=$(python3 <<'PY'
import json
with open(".beads/issues.jsonl", "r", encoding="utf-8") as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(rec, dict) and rec.get("title") == "status offender":
            print(rec["id"])
            break
PY
)
if [ -z "$offender_id" ]; then
  echo "corrupt: could not resolve offender id from JSONL" >&2
  exit 1
fi
echo "$offender_id" > .fixture_offender_id

# Strict workflow policy that does NOT include the offender's status.
cat > .beads/policy.yaml <<'YAML'
workflow:
  strict: true
  statuses: [open, in_progress, blocked, deferred, closed]
YAML

# Write the out-of-set status directly (FK/enforcement bypass — the
# offender predates the policy from the enforcement layer's viewpoint).
python3 - "$offender_id" <<'PY'
import sqlite3, sys
offender_id = sys.argv[1]
conn = sqlite3.connect(".beads/beads.db")
cur = conn.cursor()
cur.execute("UPDATE issues SET status = 'limbo' WHERE id = ?", (offender_id,))
assert cur.rowcount == 1, f"expected 1 row updated, got {cur.rowcount}"
conn.commit()
conn.close()
PY

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
