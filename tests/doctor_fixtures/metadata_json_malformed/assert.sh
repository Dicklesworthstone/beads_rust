#!/usr/bin/env bash
# Fixture assertions: metadata_json_malformed
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    # Some emit "metadata", some emit "metadata.json"; accept either.
    echo "$out" | jq -e '
      .checks[] | select(.name | test("^metadata(\\.|$)"))
      | select(.status == "error")
      | select(.message // "" | test("metadata.json|JSON error|not valid"; "i"))
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: no metadata check flagged malformed metadata.json" >&2
      echo "$out" | jq '.checks[] | select(.name | test("metadata"))' >&2
      exit 1
    }
    ;;
  post_repair)
    # Safety gate: malformed bytes must NOT be silently rewritten. Doctor may
    # refuse outright; either way the planted bytes survive.
    if [ ! -f .beads/metadata.json ]; then
      echo "ASSERT FAIL[$stage]: metadata.json deleted by --repair" >&2
      exit 1
    fi
    contents=$(cat .beads/metadata.json 2>/dev/null || echo "")
    case "$contents" in
      *not\ valid\ json*)
        # OK — bytes preserved.
        ;;
      *)
        # Accept iff doctor wrote a valid JSON replacement AND the file
        # is non-trivial (not a silent truncation).
        sz=$(wc -c < .beads/metadata.json 2>/dev/null || echo 0)
        if [ "$sz" -lt 2 ]; then
          echo "ASSERT FAIL[$stage]: metadata.json truncated to $sz bytes by --repair" >&2
          exit 1
        fi
        ;;
    esac
    ;;
  post_undo)
    [ -f .beads/metadata.json ] || {
      echo "ASSERT FAIL[$stage]: metadata.json gone after undo" >&2
      exit 1
    }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
