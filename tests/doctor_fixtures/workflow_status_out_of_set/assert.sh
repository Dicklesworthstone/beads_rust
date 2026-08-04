#!/usr/bin/env bash
# Fixture assertions: workflow_status_out_of_set
#
# The workflow-status audit is DETECT-ONLY: whether the offender should
# be migrated into the set or the policy widened is an operator call, so
# --repair must not rewrite issue statuses.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

offender_id="$(cat .fixture_offender_id)"

assert_offender_flagged() {
  local out="$1"
  echo "$out" | jq -e --arg id "$offender_id" '
    .checks[] | select(.name == "policy.workflow_statuses")
    | select(.status == "warn")
    | select(.details.finding_id == "fm-agent_coordination-workflow-status-out-of-set")
    | select([.. | strings] | index($id))
  ' >/dev/null || {
    echo "ASSERT FAIL[$stage]: policy.workflow_statuses did not flag $offender_id" >&2
    echo "$out" | jq '.checks[] | select(.name == "policy.workflow_statuses")' >&2
    return 1
  }
}

offender_status() {
  python3 - "$offender_id" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(".beads/beads.db")
row = conn.execute("SELECT status FROM issues WHERE id = ?", (sys.argv[1],)).fetchone()
print(row[0] if row else "MISSING")
PY
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    assert_offender_flagged "$out" || exit 1
    ;;
  post_repair)
    # Detect-only: the offender's status must be untouched and the
    # warning still truthfully present.
    st=$(offender_status)
    if [ "$st" != "limbo" ]; then
      echo "ASSERT FAIL[$stage]: --repair rewrote the offender status to '$st'" >&2
      exit 1
    fi
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    assert_offender_flagged "$out" || exit 1
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    [ -f .beads/policy.yaml ] || { echo "ASSERT FAIL[$stage]: policy.yaml gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
