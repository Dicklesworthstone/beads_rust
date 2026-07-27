#!/usr/bin/env bash
# Fixture assertions: no_db_mode_marker

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    # JSONL-only mode must announce the reduced scope via the marker
    # check, with the FM id inline and the skipped checks enumerated.
    out=$("$tool_bin" doctor --no-db --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "db.no_db_mode")
      | select(.status == "ok")
      | select(.details.finding_id == "fm-state_files-no-db-mode-db-checks-skipped")
      | select((.details.skipped_checks | length) > 0)
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: --no-db run did not emit the db.no_db_mode marker" >&2
      echo "$out" | jq '.checks[] | select(.name == "db.no_db_mode")' >&2
      exit 1
    }
    # A FULL run must NOT carry the marker — it only exists to make the
    # reduced run self-describing.
    full=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$full" | jq -e '
      [.checks[] | select(.name == "db.no_db_mode")] | length == 0
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: full doctor run unexpectedly carries db.no_db_mode" >&2
      exit 1
    }
    ;;
  post_repair)
    # Healthy workspace: --repair must have nothing to do and the
    # workspace must remain intact.
    [ -f .beads/beads.db ] || { echo "ASSERT FAIL[$stage]: beads.db gone after repair" >&2; exit 1; }
    [ -f .beads/issues.jsonl ] || { echo "ASSERT FAIL[$stage]: issues.jsonl gone after repair" >&2; exit 1; }
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
