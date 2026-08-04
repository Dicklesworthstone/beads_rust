#!/usr/bin/env bash
# Fixture assertions: orphaned_write_lock
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

assert_lock_identity_preserved() {
  [ -f .fixture_lock_identity ] || {
    echo "ASSERT FAIL[$stage]: missing baseline lock identity" >&2
    exit 1
  }
  expected_identity=$(cat .fixture_lock_identity)
  actual_identity=$(stat -c '%d:%i' .beads/.write.lock)
  if [ "$actual_identity" != "$expected_identity" ]; then
    echo "ASSERT FAIL[$stage]: lock identity changed $expected_identity -> $actual_identity" >&2
    exit 1
  fi
}

case "$stage" in
  detect)
    assert_lock_identity_preserved
    set +e
    out=$("$tool_bin" doctor --json 2>/dev/null)
    doctor_rc=$?
    set -e
    if [ "$doctor_rc" -ne 0 ]; then
      echo "ASSERT FAIL[$stage]: healthy persistent inode made doctor exit $doctor_rc" >&2
      echo "$out" >&2
      exit 1
    fi
    assert_lock_identity_preserved
    echo "$out" | jq -e '
      .checks[] | select(.name == "write_lock")
      | select(.status == "ok")
      | select(.details.reason == "persistent_advisory_inode")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: persistent lock inode was not classified healthy" >&2
      echo "$out" | jq '.checks[] | select(.name == "write_lock")' >&2
      exit 1
    }
    echo "$out" | jq -e '
      .checks[] | select(.name == "write_lock")
      | (.details.recommended_fix == null)
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: healthy inode exposed a destructive recommendation" >&2
      echo "$out" | jq '.checks[] | select(.name == "write_lock") | .details' >&2
      exit 1
    }
    ;;
  post_repair)
    # The inode is not a finding and must remain untouched.
    [ -f .beads/.write.lock ] || {
      echo "ASSERT FAIL[$stage]: .write.lock vanished after --repair (unsafe; could corrupt a live writer)" >&2
      exit 1
    }
    if [ -L .beads/.write.lock ]; then
      echo "ASSERT FAIL[$stage]: .write.lock became a symlink after --repair (unsafe)" >&2
      exit 1
    fi
    assert_lock_identity_preserved
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    [ -f .beads/.write.lock ] || { echo "ASSERT FAIL[$stage]: .write.lock gone after undo" >&2; exit 1; }
    assert_lock_identity_preserved
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
