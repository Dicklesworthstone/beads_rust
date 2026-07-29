#!/usr/bin/env bash
# Fixture assertions: jsonl_row_count_mismatch
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "counts.db_vs_jsonl")
      | select(.status == "warn" or .status == "error")
      | select((.details.db // 0) != (.details.jsonl // 0))
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: counts.db_vs_jsonl did not surface count mismatch" >&2
      echo "$out" | jq '.checks[] | select(.name == "counts.db_vs_jsonl")' >&2
      exit 1
    }
    ;;
  post_repair)
    # Two acceptable outcomes:
    #   (a) doctor rebuilt the DB to match JSONL — counts now agree.
    #   (b) doctor's freshness gate refused the rebuild — mismatch persists.
    # Either way the JSONL file must NOT have lost data.
    n_jsonl=$(wc -l < .beads/issues.jsonl 2>/dev/null || echo 0)
    if [ "$n_jsonl" -lt 7 ]; then
      echo "ASSERT FAIL[$stage]: JSONL line count dropped to $n_jsonl (expected >= 7; planted records destroyed)" >&2
      exit 1
    fi
    ;;
  post_undo)
    [ -f .beads/issues.jsonl ] || {
      echo "ASSERT FAIL[$stage]: issues.jsonl gone after undo" >&2
      exit 1
    }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
