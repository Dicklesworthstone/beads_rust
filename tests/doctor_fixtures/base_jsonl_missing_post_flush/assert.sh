#!/usr/bin/env bash
# Fixture assertions: base_jsonl_missing_post_flush

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

assert_missing_warning() {
  local out="$1"
  echo "$out" | jq -e '
    .checks[] | select(.name == "base_jsonl.missing_post_flush")
    | select(.status == "warn")
    | select(.details.kind == "missing_post_flush")
    | select(.details.last_export_time == "2026-05-01T00:00:00Z")
    | select(.details.finding_id == "fm-state_files-base-jsonl-missing-or-stale")
  ' >/dev/null || {
    echo "ASSERT FAIL[$stage]: base_jsonl.missing_post_flush did not warn with expected details" >&2
    echo "$out" | jq '.checks[] | select(.name == "base_jsonl" or .name == "base_jsonl.missing_post_flush")' >&2
    exit 1
  }
}

assert_anchor_absent() {
  if [ -e .beads/beads.base.jsonl ]; then
    echo "ASSERT FAIL[$stage]: .beads/beads.base.jsonl unexpectedly exists" >&2
    ls -l .beads/beads.base.jsonl >&2
    exit 1
  fi
}

assert_metadata_preserved() {
  local value
  value=$(sqlite3 .beads/beads.db \
    "SELECT value FROM metadata WHERE key='last_export_time' ORDER BY rowid DESC LIMIT 1;")
  if [ "$value" != "2026-05-01T00:00:00Z" ]; then
    echo "ASSERT FAIL[$stage]: metadata.last_export_time changed to '$value'" >&2
    exit 1
  fi
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    assert_missing_warning "$out"
    echo "$out" | jq -e '
      .checks[] | select(.name == "base_jsonl")
      | select(.status == "ok")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: base_jsonl file-level check should stay ok for a missing anchor" >&2
      echo "$out" | jq '.checks[] | select(.name == "base_jsonl")' >&2
      exit 1
    }
    assert_anchor_absent
    assert_metadata_preserved
    ;;

  post_repair)
    if [ -f "$target_dir/_diag/repair.json" ]; then
      jq -e '
        .repaired == false
        and .recovery_audit.outcome == "nothing_to_repair"
      ' "$target_dir/_diag/repair.json" >/dev/null || {
        echo "ASSERT FAIL[$stage]: repair should be a no-op for missing-post-flush only" >&2
        cat "$target_dir/_diag/repair.json" >&2
        exit 1
      }
    fi
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    assert_missing_warning "$out"
    assert_anchor_absent
    assert_metadata_preserved
    ;;

  post_undo)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    assert_missing_warning "$out"
    assert_anchor_absent
    assert_metadata_preserved
    ;;

  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac

