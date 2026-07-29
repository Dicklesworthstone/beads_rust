#!/usr/bin/env bash
# Fixture assertions: metadata_json_drift
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "metadata.json")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: metadata.json not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "metadata.json")' >&2
      exit 1
    }
    # The drift payload must specifically name the jsonl_export field
    # since `database` does exist (partial-drift case).
    echo "$out" | jq -e '
      .checks[] | select(.name == "metadata.json")
      | .details.drift | map(.field) | index("jsonl_export")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: drift payload missing jsonl_export entry" >&2
      echo "$out" | jq '.checks[] | select(.name == "metadata.json") | .details' >&2
      exit 1
    }
    # `database` was honest → must NOT be in drift.
    if echo "$out" | jq -e '
      .checks[] | select(.name == "metadata.json")
      | .details.drift | map(.field) | index("database")
    ' >/dev/null 2>&1; then
      echo "ASSERT FAIL[$stage]: drift payload wrongly flagged 'database' (the file exists)" >&2
      exit 1
    fi
    ;;
  post_repair)
    # Detect-only: metadata.json must still declare the bad jsonl_export
    # byte-identically — doctor must not silently rewrite it.
    [ -f .beads/metadata.json ] || {
      echo "ASSERT FAIL[$stage]: metadata.json vanished after --repair (unsafe)" >&2
      exit 1
    }
    if ! grep -q '"renamed-by-operator.jsonl"' .beads/metadata.json; then
      echo "ASSERT FAIL[$stage]: doctor silently rewrote metadata.json (the bad value is gone)" >&2
      cat .beads/metadata.json >&2
      exit 1
    fi
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    [ -f .beads/metadata.json ] || { echo "ASSERT FAIL[$stage]: metadata.json gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
