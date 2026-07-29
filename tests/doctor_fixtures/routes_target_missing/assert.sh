#!/usr/bin/env bash
# Fixture assertions: routes_target_missing
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    # routes_jsonl shape must still be OK (the lines are valid JSON).
    echo "$out" | jq -e '
      .checks[] | select(.name == "routes_jsonl") | .status == "ok"
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: routes_jsonl unexpectedly not OK" >&2
      echo "$out" | jq '.checks[] | select(.name == "routes_jsonl")' >&2
      exit 1
    }
    # The companion routes.targets resolution check must surface warn.
    echo "$out" | jq -e '
      .checks[] | select(.name == "routes.targets")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: routes.targets not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "routes.targets")' >&2
      exit 1
    }
    # Unresolved list must include the ghost- prefix.
    echo "$out" | jq -e '
      .checks[] | select(.name == "routes.targets")
      | .details.unresolved_routes
      | map(.prefix) | index("ghost-")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: unresolved_routes missing ghost- prefix" >&2
      echo "$out" | jq '.checks[] | select(.name == "routes.targets") | .details' >&2
      exit 1
    }
    # Resolved count must be 1 (the self- route resolves cleanly).
    rc=$(echo "$out" | jq '.checks[] | select(.name == "routes.targets") | .details.resolved_count')
    if [ "$rc" != "1" ]; then
      echo "ASSERT FAIL[$stage]: expected resolved_count=1, got $rc" >&2
      exit 1
    fi
    ;;
  post_repair)
    # Detect-only: routes.jsonl must still be byte-identical (doctor
    # must NOT silently rewrite the operator's routing decisions).
    [ -f .beads/routes.jsonl ] || {
      echo "ASSERT FAIL[$stage]: routes.jsonl vanished after --repair (unsafe)" >&2
      exit 1
    }
    if ! grep -q '"ghost-"' .beads/routes.jsonl; then
      echo "ASSERT FAIL[$stage]: doctor silently rewrote routes.jsonl (ghost- prefix gone)" >&2
      cat .beads/routes.jsonl >&2
      exit 1
    fi
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    [ -f .beads/routes.jsonl ] || { echo "ASSERT FAIL[$stage]: routes.jsonl gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
