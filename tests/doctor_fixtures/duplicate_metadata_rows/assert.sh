#!/usr/bin/env bash
# Fixture assertions: duplicate_metadata_rows
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "db.recoverable_anomalies")
      | select(.status == "warn" or .status == "error")
      | select((.details.findings // [] | tostring)
               | test("metadata contains duplicate rows"; "i"))
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: db.recoverable_anomalies did not flag duplicate metadata rows" >&2
      echo "$out" | jq '.checks[] | select(.name == "db.recoverable_anomalies")' >&2
      exit 1
    }
    ;;
  post_repair)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    msg=$(echo "$out" | jq -r '.checks[] | select(.name == "db.recoverable_anomalies") | .message // ""')
    case "$msg" in
      *metadata\ contains\ duplicate\ rows*)
        echo "ASSERT FAIL[$stage]: duplicate metadata rows persist post-repair: $msg" >&2
        exit 1
        ;;
    esac
    # Verify only one row for the duplicated key (rebuild collapses duplicates).
    nrows=$(printf '%s\n' "SELECT COUNT(*) FROM metadata WHERE key='jsonl_content_hash';" \
              | sqlite3 .beads/beads.db 2>/dev/null || echo 0)
    if [ "$nrows" -gt 1 ]; then
      echo "ASSERT FAIL[$stage]: duplicate metadata row count still $nrows (expected 0 or 1)" >&2
      exit 1
    fi
    ;;
  post_undo)
    [ -f .beads/beads.db ] || {
      echo "ASSERT FAIL[$stage]: beads.db gone after undo" >&2
      exit 1
    }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
