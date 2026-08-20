#!/usr/bin/env bash
# Fixture assertions: write_lock_symlink_node
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

assert_symlink_and_target_untouched() {
  [ -L .beads/.write.lock ] || {
    echo "ASSERT FAIL[$stage]: symlinked .write.lock was removed or replaced (doctor must never touch the node)" >&2
    exit 1
  }
  [ -f .beads/.lock_target ] || {
    echo "ASSERT FAIL[$stage]: symlink target vanished (target traversal / mutation is forbidden)" >&2
    exit 1
  }
  expected_identity=$(cat .fixture_target_identity)
  actual_identity=$(stat -c '%d:%i' .beads/.lock_target 2>/dev/null \
    || stat -f '%d:%i' .beads/.lock_target)
  if [ "$actual_identity" != "$expected_identity" ]; then
    echo "ASSERT FAIL[$stage]: symlink target inode changed $expected_identity -> $actual_identity" >&2
    exit 1
  fi
}

case "$stage" in
  detect)
    assert_symlink_and_target_untouched
    set +e
    out=$("$tool_bin" doctor --json 2>&1)
    doctor_rc=$?
    set -e
    # Fail closed at startup: `blocking_write_lock` refuses a non-regular
    # lock node from lstat alone before any open, so doctor exits
    # non-zero with the typed refusal instead of running its checks.
    if [ "$doctor_rc" -eq 0 ]; then
      echo "ASSERT FAIL[$stage]: symlinked .write.lock did not fail doctor (beads_rust-5sej)" >&2
      echo "$out" >&2
      exit 1
    fi
    echo "$out" | grep -q "Refusing unsafe workspace write lock path" || {
      echo "ASSERT FAIL[$stage]: startup refusal diagnostic missing for symlinked .write.lock" >&2
      echo "$out" >&2
      exit 1
    }
    echo "$out" | grep -q "expected a regular file, not a symlink or special file" || {
      echo "ASSERT FAIL[$stage]: refusal did not name the non-regular shape" >&2
      echo "$out" >&2
      exit 1
    }
    assert_symlink_and_target_untouched
    ;;
  post_repair)
    # Detect-only: --repair must not remove, retarget, or replace the node.
    assert_symlink_and_target_untouched
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    assert_symlink_and_target_untouched
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
