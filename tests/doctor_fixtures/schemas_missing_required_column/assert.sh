#!/usr/bin/env bash
# Fixture assertions: schemas_missing_required_column
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "schema.columns")
      | select(.status == "error")
      | select(.details.tables // [] | tostring | test("text"))
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: schema.columns did not flag comments.text missing" >&2
      echo "$out" | jq '.checks[] | select(.name == "schema.columns")' >&2
      exit 1
    }
    ;;
  post_repair)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$out" | jq -r '.checks[] | select(.name == "schema.columns") | .status')
    if [ "$status" = "error" ]; then
      echo "ASSERT FAIL[$stage]: schema.columns still error post-repair" >&2
      echo "$out" | jq '.checks[] | select(.name == "schema.columns")' >&2
      exit 1
    fi
    # Verify text column exists.
    has_text=$(printf '%s\n' "SELECT COUNT(*) FROM pragma_table_info('comments') WHERE name='text';" \
              | sqlite3 .beads/beads.db 2>/dev/null || echo 0)
    if [ "$has_text" != "1" ]; then
      echo "ASSERT FAIL[$stage]: comments.text column not recreated (count=$has_text)" >&2
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
