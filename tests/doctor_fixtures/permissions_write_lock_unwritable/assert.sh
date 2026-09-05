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
    # Precondition re-check (beads_rust-ypwu): the fixture plants a regular
    # 0444 `.write.lock` and needs the OS to actually refuse this uid write
    # access to it. Environments where permission bits do not bind — root,
    # CAP_DAC_OVERRIDE container sandboxes (some CI/remote build workers),
    # or filesystems that drop mode bits — cannot hold the precondition, so
    # the detector legitimately never fires there. Exit 3 is the suite's
    # skip protocol; a precondition the environment cannot hold is not a
    # product failure and must not read as one.
    if [ ! -f .beads/.write.lock ] || [ -L .beads/.write.lock ]; then
      echo "SKIP[$stage]: planted .beads/.write.lock is missing or not a regular file before doctor ran; the harness environment did not preserve the fixture state" >&2
      exit 3
    fi
    mode=$(stat -c '%a' .beads/.write.lock)
    if [ "$mode" != "444" ]; then
      echo "SKIP[$stage]: planted .write.lock mode is $mode (expected 444) before doctor ran; the filesystem did not preserve the fixture's permission bits" >&2
      exit 3
    fi
    if [ -w .beads/.write.lock ]; then
      echo "SKIP[$stage]: uid $(id -u) can write a mode-444 file (root or CAP_DAC_OVERRIDE); this environment cannot hold the unwritable-lock precondition" >&2
      exit 3
    fi

    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    # The startup error names the lock as "write lock" (pre-#412) or
    # "workspace write lock" (post-#412 family-authority split); accept both.
    echo "$out" | jq -e '
      .ok == false
      and .workspace_health == "degraded"
      and (.checks[] | select(.name == "permissions.write_lock")
        | select(.status == "warn")
        | select(.details.mode_octal == "444")
        | select(.details.finding_id == "fm-state_files-orphaned-write-lock")
        | select(.details.remediation
            | contains("chmod u+w")
              and contains("preserve the existing lock file")
              and (test("remove|unlink"; "i") | not))
        | select(.details.startup_error
            | (contains("Failed to open") and contains("write lock"))))
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
      and (.detail | (contains("Failed to open") and contains("write lock")))
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
