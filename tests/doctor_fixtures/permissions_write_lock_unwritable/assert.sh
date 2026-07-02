#!/usr/bin/env bash
# Fixture assertions: permissions_write_lock_unwritable

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

assert_lock_preserved() {
  [ -f .beads/.write.lock ] || {
    echo "ASSERT FAIL[$stage]: .beads/.write.lock vanished" >&2
    exit 1
  }
  if [ -L .beads/.write.lock ]; then
    echo "ASSERT FAIL[$stage]: .beads/.write.lock became a symlink" >&2
    exit 1
  fi
  mode=$(stat -c '%a' .beads/.write.lock)
  if [ "$mode" != "444" ]; then
    echo "ASSERT FAIL[$stage]: .beads/.write.lock mode changed to $mode" >&2
    exit 1
  fi
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .ok == false
      and .workspace_health == "degraded"
      and (.checks[] | select(.name == "permissions.write_lock")
        | select(.status == "warn")
        | select(.details.mode_octal == "444")
        | select(.details.finding_id == "fm-state_files-orphaned-write-lock")
        | select(.details.startup_error | contains("Failed to open write lock")))
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: plain doctor did not emit permissions.write_lock diagnostic" >&2
      echo "$out" | jq '.' >&2
      exit 1
    }
    assert_lock_preserved
    ;;

  post_repair)
    if [ ! -f "$target_dir/_diag/repair.json" ]; then
      echo "ASSERT FAIL[$stage]: missing _diag/repair.json" >&2
      exit 1
    fi
    jq -e '
      .ok == false
      and .code == "concurrency_lost"
      and .exit_code == 5
      and (.detail | contains("Failed to open write lock"))
    ' "$target_dir/_diag/repair.json" >/dev/null || {
      echo "ASSERT FAIL[$stage]: --repair did not refuse with concurrency_lost" >&2
      cat "$target_dir/_diag/repair.json" >&2
      exit 1
    }
    assert_lock_preserved
    ;;

  post_undo)
    assert_lock_preserved
    ;;

  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
