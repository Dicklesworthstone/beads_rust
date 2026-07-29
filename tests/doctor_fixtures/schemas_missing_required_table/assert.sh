#!/usr/bin/env bash
# Fixture assertions: schemas_missing_required_table
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "schema.tables")
      | select(.status == "error")
      | select(.details.missing // [] | tostring | test("export_hashes"))
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: schema.tables did not flag export_hashes missing" >&2
      echo "$out" | jq '.checks[] | select(.name == "schema.tables")' >&2
      exit 1
    }
    ;;
  post_repair)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$out" | jq -r '.checks[] | select(.name == "schema.tables") | .status')
    if [ "$status" = "error" ]; then
      echo "ASSERT FAIL[$stage]: schema.tables still error post-repair" >&2
      echo "$out" | jq '.checks[] | select(.name == "schema.tables")' >&2
      exit 1
    fi
    have=$(printf '%s\n' "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='export_hashes';" \
            | sqlite3 .beads/beads.db 2>/dev/null || echo 0)
    if [ "$have" != "1" ]; then
      echo "ASSERT FAIL[$stage]: export_hashes table not recreated (count=$have)" >&2
      exit 1
    fi
    # Issues row count must be preserved.
    nissues=$(printf '%s\n' "SELECT COUNT(*) FROM issues;" \
              | sqlite3 .beads/beads.db 2>/dev/null || echo 0)
    if [ "$nissues" -lt 2 ]; then
      echo "ASSERT FAIL[$stage]: issues row count dropped (got $nissues, expected >= 2)" >&2
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
