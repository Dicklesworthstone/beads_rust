#!/usr/bin/env bash
# Fixture assertions: blocked_cache_content_mismatch
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
               | test("content differs from direct dependency graph"; "i"))
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: db.recoverable_anomalies did not surface content-mismatch finding" >&2
      echo "$out" | jq '.checks[] | select(.name == "db.recoverable_anomalies")' >&2
      exit 1
    }
    ;;
  post_repair)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    msg=$(echo "$out" | jq -r '.checks[] | select(.name == "db.recoverable_anomalies") | .message // ""')
    case "$msg" in
      *content\ differs*)
        echo "ASSERT FAIL[$stage]: blocked-cache content mismatch persists post-repair: $msg" >&2
        exit 1
        ;;
    esac
    ghost=$(printf '%s\n' "SELECT COUNT(*) FROM blocked_issues_cache WHERE issue_id='br-9999';" \
              | sqlite3 .beads/beads.db 2>/dev/null || echo "")
    if [ "$ghost" != "0" ]; then
      echo "ASSERT FAIL[$stage]: ghost cache row 'br-9999' still present (count=$ghost)" >&2
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
