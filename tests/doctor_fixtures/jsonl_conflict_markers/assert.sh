#!/usr/bin/env bash
# Fixture assertions: jsonl_conflict_markers
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null || true) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "sync_conflict_markers")
      | select(.status == "error" or .status == "warn")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: sync_conflict_markers not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "sync_conflict_markers")' >&2
      exit 1
    }
    grep -q '<<<<<<< ' .beads/issues.jsonl || {
      echo "ASSERT FAIL[$stage]: planted conflict markers missing" >&2
      exit 1
    }
    ;;
  post_repair)
    # Currently auto-fixed: no. The conflict markers must NOT have been
    # auto-removed (that would risk destroying operator data without consent).
    # Either: the JSONL still contains the markers, OR repair refused with
    # exit 4 and left the file alone.
    if [ -f .beads/issues.jsonl ]; then
      # If markers are gone, that's unexpected destructive behavior — fail.
      if ! grep -q '<<<<<<< ' .beads/issues.jsonl; then
        # Allow the case where rebuild from JSONL preserved-by-failing kept the file
        # but rewrote without markers? We treat this as a regression — flag it.
        echo "ASSERT FAIL[$stage]: conflict markers silently removed by --repair" >&2
        echo "(safety contract: doctor must refuse to auto-resolve merge conflicts)" >&2
        exit 1
      fi
    fi
    ;;
  post_undo)
    # No-op: this fixture's repair is expected to refuse, so undo has nothing
    # to restore. Just check the workspace wasn't trashed.
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
