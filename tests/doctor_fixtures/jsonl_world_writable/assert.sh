#!/usr/bin/env bash
# Fixture assertions: jsonl_world_writable
#
# Pass-5 cycle 25: fm-permissions-jsonl-world-writable graduates
# from detect-only (Tier B from cycle 14) to auto-fixed (Tier A).
# --repair strips the world-write bit via Op::Chmod through the
# chokepoint. This is the FIRST production Op::Chmod fixture.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

cur_mode() {
  python3 - .beads/issues.jsonl <<'PY'
import os
import sys

print(format(os.stat(sys.argv[1]).st_mode & 0o777, "o"))
PY
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "permissions.jsonl_world_writable")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: permissions.jsonl_world_writable not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "permissions.jsonl_world_writable")' >&2
      exit 1
    }
    mode=$(cur_mode)
    if [ "$mode" != "666" ]; then
      echo "ASSERT FAIL[$stage]: expected pre-fix mode 666, got '$mode'" >&2
      exit 1
    fi
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_pre=$(cat .fixture_jsonl_pre_sha256)
    if [ "$jsonl_now" != "$jsonl_pre" ]; then
      echo "ASSERT FAIL[$stage]: JSONL bytes drifted during corrupt stage" >&2
      exit 1
    fi
    ;;
  post_repair)
    mode=$(cur_mode)
    # World-write bit (002) MUST be cleared. Other bits (owner-
    # write, group-write) MUST be preserved — only the world-write
    # bit was the security concern.
    if [ "$(( 8#$mode & 8#002 ))" != "0" ]; then
      echo "ASSERT FAIL[$stage]: world-write bit still set after repair (mode=$mode)" >&2
      exit 1
    fi
    if [ "$mode" != "664" ]; then
      echo "ASSERT FAIL[$stage]: expected post-fix mode 664 (only world-write stripped), got '$mode'" >&2
      exit 1
    fi
    if [ -f "$target_dir/_diag/repair.json" ]; then
      jq -e '
        .repaired == true
        and (.recovery_audit.applied_actions | index("jsonl_world_write_stripped"))
      ' "$target_dir/_diag/repair.json" >/dev/null || {
        echo "ASSERT FAIL[$stage]: repair output did not report jsonl_world_write_stripped" >&2
        cat "$target_dir/_diag/repair.json" >&2
        exit 1
      }
    fi
    # SACRED INVARIANT: JSONL bytes byte-identical. Op::Chmod only
    # touches mode, never content.
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_pre=$(cat .fixture_jsonl_pre_sha256)
    if [ "$jsonl_now" != "$jsonl_pre" ]; then
      echo "ASSERT FAIL[$stage]: JSONL bytes mutated by --repair (Op::Chmod must not touch content)" >&2
      exit 1
    fi
    redetect=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$redetect" | jq -r '.checks[] | select(.name == "permissions.jsonl_world_writable") | .status' 2>/dev/null || echo "")
    if [ "$status" != "ok" ] && [ -n "$status" ]; then
      echo "ASSERT FAIL[$stage]: permissions.jsonl_world_writable still '$status' after repair" >&2
      exit 1
    fi
    # actions.jsonl records at least one chmod op across the run dirs.
    runs_root="$target_dir/.doctor/runs"
    chmod_count=0
    while IFS= read -r d; do
      a="$d/actions.jsonl"
      [ -s "$a" ] || continue
      c=$(grep -c '"op":"chmod"' "$a" 2>/dev/null || echo 0)
      c="${c//[[:space:]]/}"
      chmod_count=$((chmod_count + ${c:-0}))
    done < <(find "$runs_root" -maxdepth 1 -mindepth 1 -type d 2>/dev/null)
    if [ "${chmod_count:-0}" -lt 1 ]; then
      echo "ASSERT FAIL[$stage]: expected >=1 chmod action, got $chmod_count" >&2
      find "$runs_root" -name actions.jsonl -exec cat {} \; >&2 2>/dev/null || true
      exit 1
    fi
    ;;
  post_undo)
    mode=$(cur_mode)
    expected=$(cat .fixture_pre_mode)
    if [ "$mode" != "$expected" ]; then
      echo "ASSERT FAIL[$stage]: undo did not restore mode; got '$mode', expected '$expected'" >&2
      exit 1
    fi
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_pre=$(cat .fixture_jsonl_pre_sha256)
    if [ "$jsonl_now" != "$jsonl_pre" ]; then
      echo "ASSERT FAIL[$stage]: JSONL bytes drifted across undo" >&2
      exit 1
    fi
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
