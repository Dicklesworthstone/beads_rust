#!/usr/bin/env bash
# Fixture assertions: recovery_artifacts_orphaned
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "db.recovery_artifacts")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: db.recovery_artifacts not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "db.recovery_artifacts")' >&2
      exit 1
    }
    [ -d .beads/.br_recovery ] || {
      echo "ASSERT FAIL[$stage]: .br_recovery missing" >&2; exit 1
    }
    ;;
  post_repair)
    # P3: no auto-prune. Artifacts must still exist (doctor must not silently
    # delete user-visible recovery backups).
    [ -d .beads/.br_recovery ] || {
      echo "ASSERT FAIL[$stage]: .br_recovery vanished after --repair" >&2
      exit 1
    }
    # At least one of the planted files should still be there.
    n=$(find .beads/.br_recovery -type f | wc -l)
    if [ "$n" -lt 1 ]; then
      echo "ASSERT FAIL[$stage]: recovery artifacts auto-deleted (unsafe)" >&2
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
