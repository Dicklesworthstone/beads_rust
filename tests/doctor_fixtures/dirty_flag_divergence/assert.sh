#!/usr/bin/env bash
# Fixture assertions: dirty_flag_divergence

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

dirty_count() {
  sqlite3 .beads/beads.db "SELECT COUNT(*) FROM dirty_issues"
}

assert_sync_metadata_warns() {
  local out="$1"
  echo "$out" | jq -e '
    .checks[] | select(.name == "sync.metadata")
    | select(.status == "warn")
    | select(.details.finding_id == "fm-state_files-dirty-flag-divergence")
    | select(.details.dirty_issues == 1)
    | select(.details.pending_export == true)
  ' >/dev/null || {
    echo "ASSERT FAIL[$stage]: sync.metadata did not surface dirty-flag divergence" >&2
    echo "$out" | jq '.checks[] | select(.name == "sync.metadata")' >&2
    exit 1
  }
}

assert_dirty_row_preserved() {
  local count
  count="$(dirty_count)"
  if [ "$count" != "1" ]; then
    echo "ASSERT FAIL[$stage]: expected exactly one dirty_issues row, got '$count'" >&2
    exit 1
  fi
}

assert_jsonl_preserved() {
  local current expected
  current="$(sha256sum .beads/issues.jsonl | awk '{print $1}')"
  expected="$(cat .fixture_jsonl_sha256)"
  if [ "$current" != "$expected" ]; then
    echo "ASSERT FAIL[$stage]: issues.jsonl changed" >&2
    echo "  expected: $expected" >&2
    echo "  current : $current" >&2
    exit 1
  fi
}

case "$stage" in
  detect)
    out="$("$tool_bin" doctor --json 2>/dev/null || true)"
    assert_sync_metadata_warns "$out"
    assert_dirty_row_preserved
    assert_jsonl_preserved
    ;;

  post_repair)
    if [ ! -s "$target_dir/_diag/repair.json" ]; then
      echo "ASSERT FAIL[$stage]: missing repair transcript" >&2
      exit 1
    fi
    jq -e '
      .repaired == false
      and .verified == false
      and .recovery_audit.outcome == "nothing_to_repair"
      and (
        .post_repair.checks[]
        | select(.name == "sync.metadata")
        | select(.status == "warn")
        | select(.details.finding_id == "fm-state_files-dirty-flag-divergence")
      )
    ' "$target_dir/_diag/repair.json" >/dev/null || {
      echo "ASSERT FAIL[$stage]: repair output did not truthfully report residual selected warning" >&2
      cat "$target_dir/_diag/repair.json" >&2
      exit 1
    }
    out="$("$tool_bin" doctor --json 2>/dev/null || true)"
    assert_sync_metadata_warns "$out"
    assert_dirty_row_preserved
    assert_jsonl_preserved
    ;;

  post_undo)
    out="$("$tool_bin" doctor --json 2>/dev/null || true)"
    assert_sync_metadata_warns "$out"
    assert_dirty_row_preserved
    assert_jsonl_preserved
    ;;

  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
