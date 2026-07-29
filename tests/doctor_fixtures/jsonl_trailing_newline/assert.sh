#!/usr/bin/env bash
# Fixture assertions: jsonl_trailing_newline
#
# Pass-5 cycle 19: fm-state_files-jsonl-missing-trailing-newline
# graduates from undetected (Tier D) to auto-fixed (Tier A).
# --repair appends a single `\n` byte via Op::AppendFile through the
# chokepoint. This is the canonical end-to-end exercise of
# Op::AppendFile in Phase 9.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

last_byte_hex() {
  tail -c 1 .beads/issues.jsonl | xxd -p
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "jsonl_eof_newline")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: jsonl_eof_newline not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "jsonl_eof_newline")' >&2
      exit 1
    }
    last=$(last_byte_hex)
    if [ "$last" = "0a" ]; then
      echo "ASSERT FAIL[$stage]: expected missing-trailing-newline state; last byte is 0x0a" >&2
      exit 1
    fi
    ;;
  post_repair)
    last=$(last_byte_hex)
    if [ "$last" != "0a" ]; then
      echo "ASSERT FAIL[$stage]: trailing newline not appended (last byte='$last')" >&2
      exit 1
    fi
    if [ -f "$target_dir/_diag/repair.json" ]; then
      jq -e '
        .repaired == true
        and (.recovery_audit.applied_actions | index("jsonl_trailing_newline_appended"))
      ' "$target_dir/_diag/repair.json" >/dev/null || {
        echo "ASSERT FAIL[$stage]: repair output did not report jsonl_trailing_newline_appended" >&2
        cat "$target_dir/_diag/repair.json" >&2
        exit 1
      }
    fi
    # The post-repair JSONL bytes must match the pre-corruption
    # baseline exactly (the newline was the only difference).
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_baseline=$(cat .fixture_jsonl_post_repair_sha256)
    if [ "$jsonl_now" != "$jsonl_baseline" ]; then
      echo "ASSERT FAIL[$stage]: post-repair JSONL doesn't match baseline" >&2
      echo "  baseline: $jsonl_baseline" >&2
      echo "  now:      $jsonl_now" >&2
      exit 1
    fi
    redetect=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$redetect" | jq -r '.checks[] | select(.name == "jsonl_eof_newline") | .status' 2>/dev/null || echo "")
    if [ "$status" != "ok" ] && [ -n "$status" ]; then
      echo "ASSERT FAIL[$stage]: jsonl_eof_newline still '$status' after repair" >&2
      exit 1
    fi
    # actions.jsonl records at least one append_file op across the run dirs.
    runs_root="$target_dir/.doctor/runs"
    append_count=0
    while IFS= read -r d; do
      a="$d/actions.jsonl"
      [ -s "$a" ] || continue
      c=$(grep -c '"op":"append_file"' "$a" 2>/dev/null || echo 0)
      c="${c//[[:space:]]/}"
      append_count=$((append_count + ${c:-0}))
    done < <(find "$runs_root" -maxdepth 1 -mindepth 1 -type d 2>/dev/null)
    if [ "${append_count:-0}" -lt 1 ]; then
      echo "ASSERT FAIL[$stage]: expected >=1 append_file action, got $append_count" >&2
      find "$runs_root" -name actions.jsonl -exec cat {} \; >&2 2>/dev/null || true
      exit 1
    fi
    ;;
  post_undo)
    last=$(last_byte_hex)
    if [ "$last" = "0a" ]; then
      echo "ASSERT FAIL[$stage]: undo did not strip the appended newline (last byte still 0x0a)" >&2
      exit 1
    fi
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_pre=$(cat .fixture_jsonl_pre_sha256)
    if [ "$jsonl_now" != "$jsonl_pre" ]; then
      echo "ASSERT FAIL[$stage]: undo didn't byte-restore corrupted JSONL" >&2
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
