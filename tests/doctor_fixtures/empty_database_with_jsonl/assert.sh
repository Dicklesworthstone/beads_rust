#!/usr/bin/env bash
# Fixture assertions: empty_database_with_jsonl
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null || true) || true
    # schema.tables MUST be error (no tables in zero-byte DB).
    echo "$out" | jq -e '
      .checks[] | select(.name == "schema.tables") | select(.status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: schema.tables not error on truncated DB" >&2
      echo "$out" | jq '.checks[] | select(.name == "schema.tables")' >&2
      exit 1
    }
    # workspace_health should be recoverable/degraded.
    health=$(echo "$out" | jq -r '.workspace_health // "unknown"')
    case "$health" in
      recoverable|degraded|broken) : ;;
      *)
        echo "ASSERT FAIL[$stage]: unexpected workspace_health=$health" >&2
        exit 1
        ;;
    esac
    # DB must be 0 bytes (planted).
    size=$(stat -c%s .beads/beads.db 2>/dev/null || stat -f%z .beads/beads.db)
    if [ "$size" -ne 0 ]; then
      echo "ASSERT FAIL[$stage]: planted DB is not 0 bytes (size=$size)" >&2
      exit 1
    fi
    ;;
  post_repair)
    # DB should now have content rebuilt from JSONL.
    size=$(stat -c%s .beads/beads.db 2>/dev/null || stat -f%z .beads/beads.db)
    if [ "$size" -lt 1024 ]; then
      echo "ASSERT FAIL[$stage]: repaired DB suspiciously small ($size bytes)" >&2
      exit 1
    fi
    # The rebuild quarantines the corrupted artifact, so we expect recovery artifacts.
    # Doctor should no longer report missing schema tables.
    out=$("$tool_bin" doctor --json 2>/dev/null || true) || true
    status=$(echo "$out" | jq -r '.checks[] | select(.name == "schema.tables") | .status')
    if [ "$status" = "error" ]; then
      echo "ASSERT FAIL[$stage]: schema.tables still error post-repair" >&2
      echo "$out" | jq '.checks[] | select(.name == "schema.tables")' >&2
      exit 1
    fi
    # Issues should be queryable.
    n=$("$tool_bin" list --json 2>/dev/null | jq 'length' 2>/dev/null || echo 0)
    if [ -z "$n" ] || [ "$n" -lt 1 ]; then
      echo "ASSERT FAIL[$stage]: rebuilt DB has no issues (seed lost)" >&2
      exit 1
    fi
    ;;
  post_undo)
    # The rebuild path predates the chokepoint; undo may be a no-op.
    # Just verify nothing catastrophic happened.
    [ -f .beads/beads.db ] || {
      echo "ASSERT FAIL[$stage]: beads.db missing after undo" >&2
      exit 1
    }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
