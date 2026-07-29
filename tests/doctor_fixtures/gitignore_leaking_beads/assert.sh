#!/usr/bin/env bash
# Fixture assertions: gitignore_leaking_beads
# Usage: assert.sh <target_dir> <stage>
# Stages:
#   detect       - planted state must surface the check
#   post_repair  - --repair must clear the check, preserve other patterns
#   post_undo    - undo latest must restore .gitignore byte-identically

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    # `br doctor` exits non-zero whenever it surfaces ANY finding (warn or
    # degraded health), which is exactly what this fixture plants — so we
    # capture output regardless of exit code and assert on the specific
    # check, matching the suite-wide pattern (see gitignore_bare_pattern).
    # Requiring exit-zero here was a latent bug: an unrelated degraded-
    # health anomaly (e.g. `jsonl_newer` from init/flush mtime ordering)
    # would fail the detect stage before the target finding was even
    # inspected.
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    # gitignore.beads_inner must be warn/error
    echo "$out" | jq -e '
      .checks[] | select(.name == "gitignore.beads_inner")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: gitignore.beads_inner check not in warn/error state" >&2
      echo "$out" | jq '.checks[] | select(.name == "gitignore.beads_inner")' >&2
      exit 1
    }
    ;;
  post_repair)
    # .beads/ pattern must be gone from .gitignore
    if grep -qE '^\.beads/?$' .gitignore; then
      echo "ASSERT FAIL[$stage]: .beads/ pattern still in .gitignore after --repair" >&2
      exit 1
    fi
    # Other patterns preserved
    grep -q '^node_modules/$' .gitignore || {
      echo "ASSERT FAIL[$stage]: node_modules/ removed by --repair (should be preserved)" >&2
      cat .gitignore >&2
      exit 1
    }
    grep -q '^\*\.tmp$' .gitignore || {
      echo "ASSERT FAIL[$stage]: *.tmp removed by --repair (should be preserved)" >&2
      exit 1
    }
    # Re-running detect should now report ok
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '.checks[] | select(.name == "gitignore.beads_inner") | select(.status == "ok")' \
      >/dev/null || {
      echo "ASSERT FAIL[$stage]: gitignore.beads_inner still warn/error after --repair" >&2
      echo "$out" | jq '.checks[] | select(.name == "gitignore.beads_inner")' >&2
      exit 1
    }
    ;;
  post_undo)
    # After undo: .gitignore is back to planted state (.beads/ restored).
    if ! grep -qE '^\.beads/$' .gitignore; then
      echo "ASSERT FAIL[$stage]: undo did not restore .beads/ pattern" >&2
      cat .gitignore >&2
      exit 1
    fi
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
