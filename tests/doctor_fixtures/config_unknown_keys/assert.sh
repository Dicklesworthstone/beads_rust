#!/usr/bin/env bash
# Fixture assertions: config_unknown_keys
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"

cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "config.unknown_keys")
      | select(.status == "warn")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: config.unknown_keys not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "config.unknown_keys")' >&2
      exit 1
    }
    # The unknown key must be named so the operator can find the typo.
    echo "$out" | jq -e '
      .checks[] | select(.name == "config.unknown_keys")
      | .details.unknown_keys | map(tostring) | join(",") | test("defualt_priority")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: details.unknown_keys does not name 'defualt_priority'" >&2
      echo "$out" | jq '.checks[] | select(.name == "config.unknown_keys") | .details' >&2
      exit 1
    }
    # Valid YAML: the parse check itself must stay ok.
    echo "$out" | jq -e '
      .checks[] | select(.name == "config.yaml") | select(.status == "ok")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: config.yaml parse check is not ok for a valid file" >&2
      echo "$out" | jq '.checks[] | select(.name == "config.yaml")' >&2
      exit 1
    }
    ;;
  post_repair)
    # Detect-only — the operator's file must be untouched, typo included.
    [ -f .beads/config.yaml ] || {
      echo "ASSERT FAIL[$stage]: config.yaml vanished after --repair (unsafe)" >&2
      exit 1
    }
    if ! grep -q '^defualt_priority:' .beads/config.yaml; then
      echo "ASSERT FAIL[$stage]: doctor rewrote config.yaml (the unknown key is gone)" >&2
      cat .beads/config.yaml >&2
      exit 1
    fi
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    [ -f .beads/config.yaml ] || {
      echo "ASSERT FAIL[$stage]: config.yaml gone after undo" >&2
      exit 1
    }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
