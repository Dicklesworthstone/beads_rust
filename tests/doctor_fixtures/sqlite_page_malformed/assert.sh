#!/usr/bin/env bash
# Fixture assertions: sqlite_page_malformed
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null || true) || true
    # sqlite.integrity_check OR sqlite3.integrity_check MUST be error.
    flagged=$(echo "$out" | jq -e '
      any(.checks[]; (.name == "sqlite.integrity_check" or .name == "sqlite3.integrity_check") and .status == "error")
    ')
    if [ "$flagged" != "true" ]; then
      echo "ASSERT FAIL[$stage]: neither integrity check flagged corruption" >&2
      echo "$out" | jq '.checks[] | select(.name | test("integrity"))' >&2
      exit 1
    fi
    ;;
  post_repair)
    out=$("$tool_bin" doctor --json 2>/dev/null || true) || true
    # Integrity must be ok after rebuild (DB was reconstructed from JSONL).
    status=$(echo "$out" | jq -r '.checks[] | select(.name == "sqlite.integrity_check") | .status')
    if [ "$status" = "error" ]; then
      echo "ASSERT FAIL[$stage]: sqlite.integrity_check still error post-repair" >&2
      echo "$out" | jq '.checks[] | select(.name == "sqlite.integrity_check")' >&2
      exit 1
    fi
    # Seed bead should survive rebuild.
    n=$("$tool_bin" list --json 2>/dev/null | jq 'length' 2>/dev/null || echo 0)
    if [ -z "$n" ] || [ "$n" -lt 1 ]; then
      echo "ASSERT FAIL[$stage]: rebuilt DB lost all issues" >&2
      exit 1
    fi
    ;;
  post_undo)
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
