#!/usr/bin/env bash
# Fixture assertions: permissions_beads_dir_readonly
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "permissions.beads_dir")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: permissions.beads_dir not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "permissions.beads_dir")' >&2
      exit 1
    }
    # The fix payload must contain `chmod u+w` for the file.
    echo "$out" | jq -e '
      .checks[] | select(.name == "permissions.beads_dir")
      | .details.readonly_paths[]
      | select(.path | endswith("/issues.jsonl"))
      | .fix | test("chmod u\\+w ")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: readonly_paths missing chmod u+w fix for issues.jsonl" >&2
      echo "$out" | jq '.checks[] | select(.name == "permissions.beads_dir") | .details' >&2
      exit 1
    }
    ;;
  post_repair)
    # Detect-only: the file MUST still be mode 0444 (doctor did not
    # silently chmod it). Restore writability afterward so TempDir
    # cleanup works.
    mode=$(stat -c %a .beads/issues.jsonl 2>/dev/null || stat -f %A .beads/issues.jsonl 2>/dev/null)
    if [ "$mode" != "444" ]; then
      echo "ASSERT FAIL[$stage]: doctor silently chmod'd issues.jsonl from 0444 to 0$mode (unsafe)" >&2
      exit 1
    fi
    # Restore for cleanup.
    chmod 0644 .beads/issues.jsonl
    ;;
  post_undo)
    # If the file is still owned + readable, undo handled cleanup.
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    # The fixture restored mode 0644 in post_repair; just verify the
    # file is still present.
    [ -f .beads/issues.jsonl ] || { echo "ASSERT FAIL[$stage]: issues.jsonl gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
