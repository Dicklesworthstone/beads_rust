#!/usr/bin/env bash
# Fixture assertions: root_gitignore_not_writable
#
# Pass-5 cycle 32: fm-permissions-gitignore-not-writable-blocks-repair
# surfaces a read-only repo-root `.gitignore` before the gitignore_repair
# fixer fails mid-write. DETECT-ONLY by design — operators may have
# intentionally locked the file (compliance, vendored config), so
# `--repair` must NOT silently chmod it.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

EXPECTED_CORRUPT_MODE=444

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "permissions.root_gitignore")
      | select(.status == "warn")
      | select(.details.mode_octal == "444")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: permissions.root_gitignore did not fire warn for mode 444" >&2
      echo "$out" | jq '.checks[] | select(.name == "permissions.root_gitignore")' >&2
      exit 1
    }
    # Detector must surface the operator-facing chmod remediation.
    echo "$out" | jq -e '
      .checks[] | select(.name == "permissions.root_gitignore")
      | .details.remediation | test("chmod u\\+w"; "")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: detector did not surface chmod remediation hint" >&2
      exit 1
    }
    # Mode on disk must still be 0o444 after detect.
    actual_mode=$(stat -c '%a' .gitignore)
    if [ "$actual_mode" != "$EXPECTED_CORRUPT_MODE" ]; then
      echo "ASSERT FAIL[$stage]: planted mode 0o$EXPECTED_CORRUPT_MODE no longer in effect (got $actual_mode)" >&2
      exit 1
    fi
    ;;

  post_repair)
    # SACRED INVARIANT: --repair must NOT chmod the operator-locked file.
    actual_mode=$(stat -c '%a' .gitignore)
    if [ "$actual_mode" != "$EXPECTED_CORRUPT_MODE" ]; then
      echo "ASSERT FAIL[$stage]: doctor silently chmod'd .gitignore (got 0o$actual_mode, expected 0o$EXPECTED_CORRUPT_MODE)" >&2
      exit 1
    fi
    # Content must also be unchanged: no Op::WriteFile slipping past the
    # detect-only contract.
    expected_content=$(cat <<'GITIGNORE'
# Test fixture root .gitignore (no .beads/ shadowing patterns)
*.log
node_modules/
target/
# br doctor per-run artifacts
.doctor/
GITIGNORE
)
    actual_content=$(cat .gitignore)
    if [ "$actual_content" != "$expected_content" ]; then
      echo "ASSERT FAIL[$stage]: .gitignore content was modified by --repair" >&2
      diff <(printf '%s\n' "$expected_content") <(printf '%s\n' "$actual_content") >&2 || true
      exit 1
    fi
    # Doctor should still fire warn (we didn't fix anything).
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$out" | jq -r '.checks[] | select(.name == "permissions.root_gitignore") | .status')
    if [ "$status" != "warn" ]; then
      echo "ASSERT FAIL[$stage]: warn unexpectedly cleared after --repair (status=$status)" >&2
      exit 1
    fi
    ;;

  post_undo)
    # Undo is a no-op for this detect-only FM. Verify the workspace is
    # unchanged (mode and content) since corruption.
    actual_mode=$(stat -c '%a' .gitignore)
    if [ "$actual_mode" != "$EXPECTED_CORRUPT_MODE" ]; then
      echo "ASSERT FAIL[$stage]: mode changed after undo (got 0o$actual_mode)" >&2
      exit 1
    fi
    [ -f .gitignore ] || { echo "ASSERT FAIL[$stage]: .gitignore missing after undo" >&2; exit 1; }
    ;;

  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
