#!/usr/bin/env bash
# Fixture assertions: dep_dead_closed_blocking_edges
#
# #432 contract: a present-but-closed blocker is a SATISFIED dependency —
# the benign steady state of completed work — so both #350 graph-audit
# checks must report status `ok` while still carrying their FM id and a
# populated, discriminating details payload. Warn is reserved for
# dangling edges / stale `blocked` status (unit-tested in doctor.rs).
# Both checks stay DETECT-ONLY: --repair must leave the graph untouched.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

blocker_id="$(sed -n '1p' .fixture_ids)"
blocked_id="$(sed -n '2p' .fixture_ids)"

assert_both_checks_ok_with_details() {
  local out="$1"
  echo "$out" | jq -e --arg blocked "$blocked_id" --arg blocker "$blocker_id" '
    (.checks[] | select(.name == "dep.dead_closed_blocking_edges")
      | select(.status == "ok")
      | select(.details.finding_id == "fm-dependencies-dead-closed-blocking-edges")
      | select(.details.dangling_count == 0)
      | select(.details.remediation == null)
      | select(.details.issues[] | select(.id == $blocked)
          | (.satisfied_blockers | index($blocker)) and (.dangling_blockers == [])))
  ' >/dev/null || {
    echo "ASSERT FAIL[$stage]: dep.dead_closed_blocking_edges is not ok-with-satisfied-details for $blocked_id -> $blocker_id" >&2
    echo "$out" | jq '.checks[] | select(.name == "dep.dead_closed_blocking_edges")' >&2
    return 1
  }
  echo "$out" | jq -e --arg blocked "$blocked_id" '
    (.checks[] | select(.name == "dep.fully_unblocked_open")
      | select(.status == "ok")
      | select(.details.finding_id == "fm-dependencies-fully-unblocked-open-issues")
      | select(.details.ready | index($blocked))
      | select(.details.stale_blocked == []))
  ' >/dev/null || {
    echo "ASSERT FAIL[$stage]: dep.fully_unblocked_open is not ok-with-ready-details for $blocked_id" >&2
    echo "$out" | jq '.checks[] | select(.name == "dep.fully_unblocked_open")' >&2
    return 1
  }
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    assert_both_checks_ok_with_details "$out" || exit 1
    ;;
  post_repair)
    # Detect-only contract: the satisfied edge is still there, both
    # informational payloads are still truthfully reported, and no
    # fixer touched the graph.
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    assert_both_checks_ok_with_details "$out" || exit 1
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
