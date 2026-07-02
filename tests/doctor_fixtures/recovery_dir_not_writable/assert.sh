#!/usr/bin/env bash
# Fixture assertions: recovery_dir_not_writable

set -euo pipefail

target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"

cd "$target_dir"

recovery_dir=".beads/.br_recovery"

cur_mode() {
    python3 - "$recovery_dir" <<'PY'
import os
import sys

print(format(os.stat(sys.argv[1]).st_mode & 0o777, "o"))
PY
}

assert_recovery_dir_locked() {
    [ -d "$recovery_dir" ] || {
        echo "ASSERT FAIL[$stage]: $recovery_dir is missing" >&2
        exit 1
    }
    mode=$(cur_mode)
    if [ "$mode" != "555" ]; then
        echo "ASSERT FAIL[$stage]: expected recovery dir mode 555, got '$mode'" >&2
        exit 1
    fi
    contents=$(cat "$recovery_dir/sentinel.txt")
    if [ "$contents" != "fixture-seed" ]; then
        echo "ASSERT FAIL[$stage]: sentinel content drifted: '$contents'" >&2
        exit 1
    fi
}

assert_no_repair_actions() {
    [ -d .doctor/runs ] || return 0
    local actions
    while IFS= read -r actions; do
        if grep -q -v '^[[:space:]]*$' "$actions"; then
            echo "ASSERT FAIL[$stage]: detect-only recovery-dir warning produced repair actions in $actions" >&2
            sed 's/^/  /' "$actions" >&2
            exit 1
        fi
    done < <(find .doctor/runs -name actions.jsonl -type f | sort)
}

case "$stage" in
    detect)
        assert_recovery_dir_locked
        out=$("$tool_bin" doctor --json 2>/dev/null) || true
        echo "$out" | jq -e '
          .checks[]
          | select(.name == "permissions.recovery_dir")
          | select(.status == "warn")
          | select(.details.mode_octal == "555")
          | select(.details.finding_id == "fm-permissions-recovery-dir-not-writable")
        ' >/dev/null || {
            echo "ASSERT FAIL[$stage]: permissions.recovery_dir warning missing or malformed" >&2
            echo "$out" | jq '.checks[] | select(.name == "permissions.recovery_dir")' >&2
            exit 1
        }
        ;;
    post_repair)
        assert_recovery_dir_locked
        assert_no_repair_actions
        chmod 0755 "$recovery_dir"
        ;;
    post_undo)
        [ -d "$recovery_dir" ] || {
            echo "ASSERT FAIL[$stage]: $recovery_dir missing after undo" >&2
            exit 1
        }
        contents=$(cat "$recovery_dir/sentinel.txt")
        if [ "$contents" != "fixture-seed" ]; then
            echo "ASSERT FAIL[$stage]: sentinel content drifted after undo: '$contents'" >&2
            exit 1
        fi
        ;;
    *)
        echo "unknown stage: $stage" >&2
        exit 2
        ;;
esac
