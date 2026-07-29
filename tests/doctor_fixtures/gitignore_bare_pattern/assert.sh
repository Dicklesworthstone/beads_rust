#!/usr/bin/env bash
# Fixture assertions: gitignore_bare_pattern
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "gitignore.beads_inner")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: gitignore.beads_inner not warn/error for bare .beads pattern" >&2
      echo "$out" | jq '.checks[] | select(.name == "gitignore.beads_inner")' >&2
      exit 1
    }
    ;;
  post_repair)
    if grep -qE '^\.beads/?$' .gitignore; then
      echo "ASSERT FAIL[$stage]: bare .beads pattern still in .gitignore" >&2
      cat .gitignore >&2
      exit 1
    fi
    grep -q '^\*\.log$' .gitignore || {
      echo "ASSERT FAIL[$stage]: *.log removed" >&2; exit 1
    }
    grep -q '^build/$' .gitignore || {
      echo "ASSERT FAIL[$stage]: build/ removed" >&2; exit 1
    }
    ;;
  post_undo)
    grep -qE '^\.beads$' .gitignore || {
      echo "ASSERT FAIL[$stage]: undo did not restore bare .beads pattern" >&2
      cat .gitignore >&2
      exit 1
    }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
