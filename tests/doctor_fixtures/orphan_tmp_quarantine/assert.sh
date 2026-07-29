#!/usr/bin/env bash
# Fixture assertions: orphan_tmp_quarantine
#
# Pass-5 cycle 16: fm-state_files-orphan-tmp-files graduates from
# detect-only (Tier B from cycle 15) to auto-fixed (Tier A).
# --repair renames each orphan tmp into the per-run quarantine via
# Op::Rename. The fixer re-runs the age predicate at fix time as a
# TOCTOU defense.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "tmp_files_orphan")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: tmp_files_orphan not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "tmp_files_orphan")' >&2
      exit 1
    }
    if [ ! -f .beads/orphan-one.tmp ]; then
      echo "ASSERT FAIL[$stage]: .beads/orphan-one.tmp missing in pre-fix state" >&2
      exit 1
    fi
    if [ ! -f .beads/orphan-two.tmp.42 ]; then
      echo "ASSERT FAIL[$stage]: .beads/orphan-two.tmp.42 missing in pre-fix state" >&2
      exit 1
    fi
    ;;
  post_repair)
    # Both orphans are gone from .beads/.
    if [ -f .beads/orphan-one.tmp ]; then
      echo "ASSERT FAIL[$stage]: .beads/orphan-one.tmp still present after repair" >&2
      exit 1
    fi
    if [ -f .beads/orphan-two.tmp.42 ]; then
      echo "ASSERT FAIL[$stage]: .beads/orphan-two.tmp.42 still present after repair" >&2
      exit 1
    fi
    # Both orphans live under the per-run quarantine. The chokepoint
    # places them at <run-dir>/quarantine/<rel-path>.
    runs_root="$target_dir/.doctor/runs"
    quarantined_one=$(find "$runs_root" -path '*quarantine/.beads/orphan-one.tmp' 2>/dev/null | head -n 1 || true)
    quarantined_two=$(find "$runs_root" -path '*quarantine/.beads/orphan-two.tmp.42' 2>/dev/null | head -n 1 || true)
    if [ -z "$quarantined_one" ] || [ ! -f "$quarantined_one" ]; then
      echo "ASSERT FAIL[$stage]: orphan-one.tmp not found under quarantine" >&2
      find "$runs_root" -type f >&2 2>/dev/null || true
      exit 1
    fi
    if [ -z "$quarantined_two" ] || [ ! -f "$quarantined_two" ]; then
      echo "ASSERT FAIL[$stage]: orphan-two.tmp.42 not found under quarantine" >&2
      find "$runs_root" -type f >&2 2>/dev/null || true
      exit 1
    fi
    # Bytes preserved verbatim under quarantine (RULE 1: no deletion).
    one_q_sha=$(sha256sum "$quarantined_one" | awk '{print $1}')
    one_pre_sha=$(cat .fixture_orphan_one_sha256)
    if [ "$one_q_sha" != "$one_pre_sha" ]; then
      echo "ASSERT FAIL[$stage]: orphan-one bytes drifted across quarantine" >&2
      exit 1
    fi
    two_q_sha=$(sha256sum "$quarantined_two" | awk '{print $1}')
    two_pre_sha=$(cat .fixture_orphan_two_sha256)
    if [ "$two_q_sha" != "$two_pre_sha" ]; then
      echo "ASSERT FAIL[$stage]: orphan-two bytes drifted across quarantine" >&2
      exit 1
    fi
    if [ -f "$target_dir/_diag/repair.json" ]; then
      jq -e '
        .repaired == true
        and (.recovery_audit.applied_actions | index("orphan_tmp_quarantined"))
      ' "$target_dir/_diag/repair.json" >/dev/null || {
        echo "ASSERT FAIL[$stage]: repair output did not report orphan_tmp_quarantined" >&2
        cat "$target_dir/_diag/repair.json" >&2
        exit 1
      }
    fi
    # SACRED INVARIANT: JSONL byte-identical (tmp quarantine must
    # never touch JSONL).
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_pre=$(cat .fixture_jsonl_pre_sha256)
    if [ "$jsonl_now" != "$jsonl_pre" ]; then
      echo "ASSERT FAIL[$stage]: JSONL bytes mutated by tmp quarantine" >&2
      exit 1
    fi
    redetect=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$redetect" | jq -r '.checks[] | select(.name == "tmp_files_orphan") | .status' 2>/dev/null || echo "")
    if [ "$status" != "ok" ] && [ -n "$status" ]; then
      echo "ASSERT FAIL[$stage]: tmp_files_orphan still '$status' after repair" >&2
      exit 1
    fi
    # actions.jsonl records at least 2 rename ops (one per orphan).
    rename_count=0
    while IFS= read -r d; do
      a="$d/actions.jsonl"
      [ -s "$a" ] || continue
      c=$(grep -c '"op":"rename"' "$a" 2>/dev/null || echo 0)
      c="${c//[[:space:]]/}"
      rename_count=$((rename_count + ${c:-0}))
    done < <(find "$runs_root" -maxdepth 1 -mindepth 1 -type d 2>/dev/null)
    if [ "${rename_count:-0}" -lt 2 ]; then
      echo "ASSERT FAIL[$stage]: expected >=2 rename actions, got $rename_count" >&2
      find "$runs_root" -name actions.jsonl -exec cat {} \; >&2 2>/dev/null || true
      exit 1
    fi
    ;;
  post_undo)
    # Both orphans restored to .beads/ byte-deterministically.
    if [ ! -f .beads/orphan-one.tmp ]; then
      echo "ASSERT FAIL[$stage]: undo did not restore .beads/orphan-one.tmp" >&2
      exit 1
    fi
    if [ ! -f .beads/orphan-two.tmp.42 ]; then
      echo "ASSERT FAIL[$stage]: undo did not restore .beads/orphan-two.tmp.42" >&2
      exit 1
    fi
    one_sha=$(sha256sum .beads/orphan-one.tmp | awk '{print $1}')
    one_pre=$(cat .fixture_orphan_one_sha256)
    if [ "$one_sha" != "$one_pre" ]; then
      echo "ASSERT FAIL[$stage]: orphan-one bytes drifted across undo" >&2
      exit 1
    fi
    two_sha=$(sha256sum .beads/orphan-two.tmp.42 | awk '{print $1}')
    two_pre=$(cat .fixture_orphan_two_sha256)
    if [ "$two_sha" != "$two_pre" ]; then
      echo "ASSERT FAIL[$stage]: orphan-two bytes drifted across undo" >&2
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
