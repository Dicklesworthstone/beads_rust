#!/usr/bin/env bash
# Fixture assertions: blocked_cache_stale
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
      | select((.message // "") + (.details.findings // [] | tostring)
               | test("blocked_issues_cache is marked stale|content differs from direct dependency graph"; "i"))
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: db.recoverable_anomalies did not surface blocked-cache stale finding" >&2
      echo "$out" | jq '.checks[] | select(.name == "db.recoverable_anomalies")' >&2
      exit 1
    }
    ;;
  post_repair)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$out" | jq -r '.checks[] | select(.name == "db.recoverable_anomalies") | .status')
    if [ "$status" = "error" ] || [ "$status" = "warn" ]; then
      # Inspect message — anything other than blocked-cache message is OK.
      msg=$(echo "$out" | jq -r '.checks[] | select(.name == "db.recoverable_anomalies") | .message // ""')
      case "$msg" in
        *blocked_issues_cache*)
          echo "ASSERT FAIL[$stage]: blocked-cache anomaly persists post-repair: $msg" >&2
          exit 1
          ;;
      esac
    fi
    # Marker must be cleared.
    marker=$(printf '%s\n' "SELECT value FROM metadata WHERE key='blocked_cache_state';" \
              | sqlite3 .beads/beads.db 2>/dev/null || echo "")
    if [ "$marker" = "stale" ]; then
      echo "ASSERT FAIL[$stage]: blocked_cache_state marker still 'stale'" >&2
      exit 1
    fi
    # Ghost row gone.
    ghost=$(printf '%s\n' "SELECT COUNT(*) FROM blocked_issues_cache WHERE issue_id='br-9999';" \
              | sqlite3 .beads/beads.db 2>/dev/null || echo "")
    if [ "$ghost" != "0" ]; then
      echo "ASSERT FAIL[$stage]: ghost cache row 'br-9999' still present (count=$ghost)" >&2
      exit 1
    fi
    ;;
  post_undo)
    # Cache rebuild path is partially chokepointed — undo may report restored=0.
    # We only require the workspace is still queryable.
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
