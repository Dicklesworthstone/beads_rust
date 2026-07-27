#!/usr/bin/env bash
# Fixture assertions: jsonl_oversized
#
# DETECT-ONLY: compaction (closing stale issues, archiving comments,
# splitting the workspace) is operator-decided. --repair must never
# truncate or rewrite the oversized JSONL to silence the warning.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

threshold=$((100 * 1024 * 1024))

assert_size_warn() {
  local out="$1"
  echo "$out" | jq -e --argjson threshold "$threshold" '
    .checks[] | select(.name == "jsonl_size")
    | select(.status == "warn")
    | select(.details.finding_id == "fm-state_files-jsonl-oversized")
    | select(.details.size_bytes > $threshold)
    | select(.details.threshold_bytes == $threshold)
  ' >/dev/null || {
    echo "ASSERT FAIL[$stage]: jsonl_size did not warn above threshold" >&2
    echo "$out" | jq '.checks[] | select(.name == "jsonl_size")' >&2
    return 1
  }
  # The padding is whitespace-only, so the parse check must stay clean —
  # this fixture isolates the SIZE finding from content corruption.
  echo "$out" | jq -e '
    .checks[] | select(.name == "jsonl.parse") | select(.status == "ok")
  ' >/dev/null || {
    echo "ASSERT FAIL[$stage]: jsonl.parse not ok — padding leaked into content validity" >&2
    echo "$out" | jq '.checks[] | select(.name == "jsonl.parse")' >&2
    return 1
  }
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    assert_size_warn "$out" || exit 1
    ;;
  post_repair)
    # Detect-only: the file must not have been truncated to silence the
    # warning, and the warning must still be truthfully present.
    planted=$(cat .fixture_planted_size)
    size_now=$(stat -c '%s' .beads/issues.jsonl)
    if [ "$size_now" -lt "$planted" ]; then
      echo "ASSERT FAIL[$stage]: --repair shrank the JSONL ($planted -> $size_now); size remediation is operator-only" >&2
      exit 1
    fi
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    assert_size_warn "$out" || exit 1
    ;;
  post_undo)
    [ -f .beads/issues.jsonl ] || { echo "ASSERT FAIL[$stage]: issues.jsonl gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
