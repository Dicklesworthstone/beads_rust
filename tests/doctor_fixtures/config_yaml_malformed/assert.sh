#!/usr/bin/env bash
# Fixture assertions: config_yaml_malformed
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "config.yaml")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: config.yaml not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "config.yaml")' >&2
      exit 1
    }
    # parse_error must be present + recommended_fix must mention "Open".
    echo "$out" | jq -e '
      .checks[] | select(.name == "config.yaml")
      | (.details.parse_error // "") | length > 0
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: config.yaml details.parse_error empty" >&2
      echo "$out" | jq '.checks[] | select(.name == "config.yaml") | .details' >&2
      exit 1
    }
    echo "$out" | jq -e '
      .checks[] | select(.name == "config.yaml")
      | .details.recommended_fix | test("Open")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: recommended_fix missing 'Open <path>' advice" >&2
      exit 1
    }
    ;;
  post_repair)
    # Detect-only — the file must still be present and still malformed.
    [ -f .beads/config.yaml ] || {
      echo "ASSERT FAIL[$stage]: config.yaml vanished after --repair (unsafe)" >&2
      exit 1
    }
    # The "invalid_block_mapping_entry" marker must still be there.
    if ! grep -q 'invalid_block_mapping_entry' .beads/config.yaml; then
      echo "ASSERT FAIL[$stage]: doctor silently rewrote config.yaml (the bad marker is gone)" >&2
      cat .beads/config.yaml >&2
      exit 1
    fi
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    [ -f .beads/config.yaml ] || { echo "ASSERT FAIL[$stage]: config.yaml gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
