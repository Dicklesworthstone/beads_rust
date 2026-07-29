#!/usr/bin/env bash
# Fixture assertions: rust_log_noisy_breaks_json
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

# Override the harness's RUST_LOG=error → debug to trigger the
# rust_log detector. Use a sub-shell so other fixtures' assert.sh
# invocations aren't affected.
case "$stage" in
  detect)
    out=$(RUST_LOG=debug "$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "rust_log")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: rust_log not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "rust_log")' >&2
      exit 1
    }
    # The recommended_fix string must name the canonical `export RUST_LOG=error`.
    echo "$out" | jq -e '
      .checks[] | select(.name == "rust_log")
      | .details.recommended_fix
      | test("export RUST_LOG=error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: recommended_fix payload missing canonical export" >&2
      echo "$out" | jq '.checks[] | select(.name == "rust_log") | .details' >&2
      exit 1
    }
    ;;
  post_repair)
    # Detect-only — doctor cannot mutate the parent env. The marker
    # file should still be present (we never wrote to it).
    [ -f .beads/.fixture_rust_log_level ] || {
      echo "ASSERT FAIL[$stage]: marker file vanished after --repair (unexpected mutation)" >&2
      exit 1
    }
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
