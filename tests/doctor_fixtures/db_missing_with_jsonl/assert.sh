#!/usr/bin/env bash
# Fixture assertions: db_missing_with_jsonl

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    # Precondition: doctor's own inspection must not have recreated the
    # file before we look. If some layer resurrects it, the fixture
    # cannot hold its planted state.
    if [ -e .beads/beads.db ]; then
      echo "SKIP[$stage]: .beads/beads.db reappeared before doctor ran; environment cannot hold the missing-db precondition" >&2
      exit 3
    fi
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "db.exists")
      | select(.status == "error")
      | select(.details.finding_id == "fm-state_files-empty-or-truncated-database")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: db.exists did not error on the missing database" >&2
      echo "$out" | jq '.checks[] | select(.name == "db.exists")' >&2
      exit 1
    }
    # JSONL must be untouched by the read-only detect pass.
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_pre=$(cat .fixture_jsonl_pre_sha256)
    if [ "$jsonl_now" != "$jsonl_pre" ]; then
      echo "ASSERT FAIL[$stage]: JSONL bytes drifted during detect" >&2
      exit 1
    fi
    ;;
  post_repair)
    # --repair rebuilds the DB from the surviving JSONL.
    [ -f .beads/beads.db ] || {
      echo "ASSERT FAIL[$stage]: --repair did not recreate .beads/beads.db" >&2
      exit 1
    }
    redetect=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$redetect" | jq -e '
      .checks[] | select(.name == "db.exists" or .name == "db.open")
      | select(.status == "error")
    ' >/dev/null && {
      echo "ASSERT FAIL[$stage]: db.exists/db.open still error after repair" >&2
      echo "$redetect" | jq '.checks[] | select(.name == "db.exists" or .name == "db.open")' >&2
      exit 1
    }
    # Both survivors must be back in the live store.
    count=$("$tool_bin" list --json 2>/dev/null | jq '[.issues[] | select(.title | startswith("survivor"))] | length')
    if [ "$count" != "2" ]; then
      echo "ASSERT FAIL[$stage]: expected 2 survivors after rebuild, got '$count'" >&2
      exit 1
    fi
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    [ -f .beads/issues.jsonl ] || { echo "ASSERT FAIL[$stage]: issues.jsonl gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
