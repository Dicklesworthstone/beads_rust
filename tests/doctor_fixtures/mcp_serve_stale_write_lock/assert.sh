#!/usr/bin/env bash
# Fixture assertions: mcp_serve_stale_write_lock

set -euo pipefail

target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"

cd "$target_dir"

assert_lock_artifacts_preserved() {
    [ -f .beads/.write.lock ] || {
        echo "ASSERT FAIL[$stage]: .beads/.write.lock was removed or moved" >&2
        exit 1
    }
    [ ! -L .beads/.write.lock ] || {
        echo "ASSERT FAIL[$stage]: .beads/.write.lock became a symlink" >&2
        exit 1
    }
    [ -f .beads/.write.lock.holder.pid ] || {
        echo "ASSERT FAIL[$stage]: .write.lock.holder.pid was removed or moved" >&2
        exit 1
    }
    if [ "$(cat .beads/.write.lock.holder.pid)" != "99999999" ]; then
        echo "ASSERT FAIL[$stage]: holder pid sidecar content changed" >&2
        exit 1
    fi
}

assert_no_repair_actions() {
    [ -d .doctor/runs ] || return 0
    local actions
    while IFS= read -r actions; do
        if grep -q -v '^[[:space:]]*$' "$actions"; then
            echo "ASSERT FAIL[$stage]: detect-only stale lock produced repair actions in $actions" >&2
            sed 's/^/  /' "$actions" >&2
            exit 1
        fi
    done < <(find .doctor/runs -name actions.jsonl -type f | sort)
}

case "$stage" in
    detect)
        assert_lock_artifacts_preserved
        out=$("$tool_bin" doctor --json 2>/dev/null) || true
        echo "$out" | jq -e '
          .checks[]
          | select(.name == "write_lock")
          | select(.status == "warn")
          | select(.details.reason == "stale_mtime")
          | select(.details.finding_id == "fm-concurrency_primitives-orphaned-write-lock")
        ' >/dev/null || {
            echo "ASSERT FAIL[$stage]: stale MCP write lock was not reported by write_lock detector" >&2
            echo "$out" | jq '.checks[] | select(.name == "write_lock")' >&2
            exit 1
        }
        ;;
    post_repair)
        assert_lock_artifacts_preserved
        assert_no_repair_actions
        issue_id=$("$tool_bin" list --json | jq -r 'if type == "array" then .[0].id else .issues[0].id end')
        if [ -z "$issue_id" ] || [ "$issue_id" = "null" ]; then
            echo "ASSERT FAIL[$stage]: could not resolve seed issue id" >&2
            exit 1
        fi
        "$tool_bin" update "$issue_id" --priority 1 --json >/dev/null
        ;;
    post_undo)
        assert_lock_artifacts_preserved
        ;;
    *)
        echo "unknown stage: $stage" >&2
        exit 2
        ;;
esac
