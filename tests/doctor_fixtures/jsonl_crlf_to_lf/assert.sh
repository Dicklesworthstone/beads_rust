#!/usr/bin/env bash
# Fixture assertions: jsonl_crlf_to_lf
#
# Pass-5 cycle 24: fm-state_files-jsonl-crlf-line-endings graduates
# from detect-only (Tier B from cycle 20) to auto-fixed (Tier A).
# --repair collapses every CRLF to LF via Op::WriteFile through the
# chokepoint, preserving standalone \r bytes.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

has_crlf() {
  python3 - <<'PY'
with open(".beads/issues.jsonl", "rb") as f:
    print("1" if b"\r\n" in f.read() else "0")
PY
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "jsonl_crlf")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: jsonl_crlf not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "jsonl_crlf")' >&2
      exit 1
    }
    if [ "$(has_crlf)" != "1" ]; then
      echo "ASSERT FAIL[$stage]: expected CRLF in pre-fix state" >&2
      exit 1
    fi
    ;;
  post_repair)
    if [ "$(has_crlf)" != "0" ]; then
      echo "ASSERT FAIL[$stage]: CRLF still present after repair" >&2
      exit 1
    fi
    if [ -f "$target_dir/_diag/repair.json" ]; then
      jq -e '
        .repaired == true
        and (.recovery_audit.applied_actions | index("jsonl_crlf_converted"))
      ' "$target_dir/_diag/repair.json" >/dev/null || {
        echo "ASSERT FAIL[$stage]: repair output did not report jsonl_crlf_converted" >&2
        cat "$target_dir/_diag/repair.json" >&2
        exit 1
      }
    fi
    # The post-repair JSONL bytes must match the pre-corruption
    # LF baseline exactly (CRLF was the only difference).
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_baseline=$(cat .fixture_jsonl_post_repair_sha256)
    if [ "$jsonl_now" != "$jsonl_baseline" ]; then
      echo "ASSERT FAIL[$stage]: post-repair JSONL doesn't match baseline" >&2
      echo "  baseline: $jsonl_baseline" >&2
      echo "  now:      $jsonl_now" >&2
      exit 1
    fi
    redetect=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$redetect" | jq -r '.checks[] | select(.name == "jsonl_crlf") | .status' 2>/dev/null || echo "")
    if [ "$status" != "ok" ] && [ -n "$status" ]; then
      echo "ASSERT FAIL[$stage]: jsonl_crlf still '$status' after repair" >&2
      exit 1
    fi
    # actions.jsonl records at least one write_file op across the run dirs.
    runs_root="$target_dir/.doctor/runs"
    write_count=0
    while IFS= read -r d; do
      a="$d/actions.jsonl"
      [ -s "$a" ] || continue
      c=$(grep -c '"op":"write_file"' "$a" 2>/dev/null || echo 0)
      c="${c//[[:space:]]/}"
      write_count=$((write_count + ${c:-0}))
    done < <(find "$runs_root" -maxdepth 1 -mindepth 1 -type d 2>/dev/null)
    if [ "${write_count:-0}" -lt 1 ]; then
      echo "ASSERT FAIL[$stage]: expected >=1 write_file action, got $write_count" >&2
      find "$runs_root" -name actions.jsonl -exec cat {} \; >&2 2>/dev/null || true
      exit 1
    fi
    ;;
  post_undo)
    if [ "$(has_crlf)" != "1" ]; then
      echo "ASSERT FAIL[$stage]: undo did not restore CRLF" >&2
      exit 1
    fi
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_pre=$(cat .fixture_jsonl_pre_sha256)
    if [ "$jsonl_now" != "$jsonl_pre" ]; then
      echo "ASSERT FAIL[$stage]: undo didn't byte-restore CRLF JSONL" >&2
      echo "  pre: $jsonl_pre" >&2
      echo "  now: $jsonl_now" >&2
      exit 1
    fi
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
