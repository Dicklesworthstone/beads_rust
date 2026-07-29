#!/usr/bin/env bash
# Fixture assertions: routes_jsonl_corrupt
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    # The new pass-2 detector must surface this at warn level.
    echo "$out" | jq -e '
      .checks[] | select(.name == "routes_jsonl")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: routes_jsonl not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "routes_jsonl")' >&2
      exit 1
    }
    # The malformed_lines payload must name line 2.
    echo "$out" | jq -e '
      .checks[] | select(.name == "routes_jsonl")
      | .details.malformed_lines | map(.line) | index(2)
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: malformed_lines payload missing line=2" >&2
      echo "$out" | jq '.checks[] | select(.name == "routes_jsonl") | .details' >&2
      exit 1
    }
    ;;
  post_repair)
    # Detect-only: routes.jsonl must still be present, byte-identical to
    # the corrupted state. Operator intent is unknowable so doctor never
    # rewrites this file silently.
    [ -f .beads/routes.jsonl ] || {
      echo "ASSERT FAIL[$stage]: routes.jsonl vanished after --repair (unsafe)" >&2
      exit 1
    }
    if ! grep -q '{not json at all}' .beads/routes.jsonl; then
      echo "ASSERT FAIL[$stage]: doctor silently rewrote routes.jsonl (the bad line is gone)" >&2
      cat .beads/routes.jsonl >&2
      exit 1
    fi
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
