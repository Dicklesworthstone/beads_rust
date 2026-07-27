#!/usr/bin/env bash
# Fixture assertions: dep_dead_closed_blocking_edges
#
# Both #350 graph-audit checks are DETECT-ONLY: remediation (removing
# or updating the stale edge) is an operator decision, so --repair must
# leave the planted state untouched and the warnings truthfully present.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

blocker_id="$(sed -n '1p' .fixture_ids)"
blocked_id="$(sed -n '2p' .fixture_ids)"

assert_both_checks_warn() {
  local out="$1"
  echo "$out" | jq -e --arg blocked "$blocked_id" --arg blocker "$blocker_id" '
    (.checks[] | select(.name == "dep.dead_closed_blocking_edges")
      | select(.status == "warn")
      | select(.details.finding_id == "fm-dependencies-dead-closed-blocking-edges")
      | select(.details.issues[] | select(.id == $blocked)
          | .dead_blockers | index($blocker)))
  ' >/dev/null || {
    echo "ASSERT FAIL[$stage]: dep.dead_closed_blocking_edges did not warn on $blocked_id -> $blocker_id" >&2
    echo "$out" | jq '.checks[] | select(.name == "dep.dead_closed_blocking_edges")' >&2
    return 1
  }
  echo "$out" | jq -e --arg blocked "$blocked_id" '
    (.checks[] | select(.name == "dep.fully_unblocked_open")
      | select(.status == "warn")
      | select(.details.finding_id == "fm-dependencies-fully-unblocked-open-issues")
      | select(.details.issues | index($blocked)))
  ' >/dev/null || {
    echo "ASSERT FAIL[$stage]: dep.fully_unblocked_open did not warn on $blocked_id" >&2
    echo "$out" | jq '.checks[] | select(.name == "dep.fully_unblocked_open")' >&2
    return 1
  }
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    assert_both_checks_warn "$out" || exit 1
    ;;
  post_repair)
    # Detect-only contract: the stale edge is still there, the warnings
    # are still truthfully reported, and no fixer touched the graph.
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    assert_both_checks_warn "$out" || exit 1
    "$tool_bin" show "$blocked_id" --json >/dev/null 2>&1 || {
      echo "ASSERT FAIL[$stage]: blocked issue $blocked_id vanished across --repair" >&2
      exit 1
    }
    status=$("$tool_bin" show "$blocker_id" --json 2>/dev/null | jq -r '.[0].status')
    if [ "$status" != "closed" ]; then
      echo "ASSERT FAIL[$stage]: blocker status drifted to '$status' across --repair" >&2
      exit 1
    fi
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    [ -f .beads/issues.jsonl ] || { echo "ASSERT FAIL[$stage]: issues.jsonl gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
