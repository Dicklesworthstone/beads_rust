#!/usr/bin/env bash
# Fixture assertions: orphan_shm_sidecar
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "db.sidecars")
      | select(.status == "error" or .status == "warn")
      | select(.message | test("SHM sidecar"; "i"))
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: db.sidecars did not flag orphan SHM" >&2
      echo "$out" | jq '.checks[] | select(.name == "db.sidecars")' >&2
      exit 1
    }
    # NOTE: opening the DB for the inspect call may itself sweep the orphan
    # SHM out of the way (frankensqlite manages SHM in-process), so we do
    # NOT assert the SHM file is still present here. The detection message
    # above is the load-bearing evidence the planted state was observed.
    ;;
  post_repair)
    # The orphan SHM should be gone from .beads/ (either deleted or quarantined
    # under .br_recovery/). DB itself must still exist and be queryable.
    [ ! -f .beads/beads.db-shm ] || {
      # Acceptable if it now has a paired WAL.
      [ -f .beads/beads.db-wal ] || {
        echo "ASSERT FAIL[$stage]: stale shm sidecar still bare in .beads/" >&2
        exit 1
      }
    }
    [ -f .beads/beads.db ] || { echo "ASSERT FAIL[$stage]: beads.db missing after --repair" >&2; exit 1; }
    # Doctor should no longer flag db.sidecars as error.
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$out" | jq -r '.checks[] | select(.name == "db.sidecars") | .status')
    if [ "$status" = "error" ]; then
      echo "ASSERT FAIL[$stage]: db.sidecars still error post-repair" >&2
      echo "$out" | jq '.checks[] | select(.name == "db.sidecars")' >&2
      exit 1
    fi
    ;;
  post_undo)
    # The DB-rebuild path predates the chokepoint, so undo may be a no-op.
    # We accept either restored>=0 (no failures). The harness verifies the
    # exit status separately; here we only assert the workspace is still
    # usable.
    [ -f .beads/beads.db ] || { echo "ASSERT FAIL[$stage]: beads.db gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
