#!/usr/bin/env bash
# Fixture assertions: merge_artifact_stuck
#
# Pass-4 cycle 1: fm-state_files-merge-artifact-stuck graduated from
# detect-only (Tier B) to auto-fixed (Tier A). The `--repair` flow now
# quarantines the stuck artifacts under <run-dir>/quarantine/.beads/
# via the mutate() chokepoint, so `doctor undo latest` reverses the
# move byte-for-byte.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

ARTIFACTS=(
  ".beads/issues.base.jsonl"
  ".beads/issues.left.jsonl"
  ".beads/issues.right.jsonl"
)

# Find the run-dir that captured the quarantined artifacts. Under
# REPLAY_IDEMPOTENCE=1 the second --repair creates a newer (empty)
# run-dir, so we search every .doctor/runs/<id>/ for one that has
# the quarantine populated. Returns the absolute path or fails.
runs_root="$target_dir/.doctor/runs"

find_quarantine_run_dir() {
  [ -d "$runs_root" ] || return 1
  local dir
  while IFS= read -r dir; do
    if [ -f "$dir/quarantine/.beads/issues.base.jsonl" ]; then
      echo "$dir"
      return 0
    fi
  done < <(find "$runs_root" -maxdepth 1 -mindepth 1 -type d | sort -r)
  return 1
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "jsonl.merge_artifacts")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: jsonl.merge_artifacts not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "jsonl.merge_artifacts")' >&2
      exit 1
    }
    for f in "${ARTIFACTS[@]}"; do
      [ -f "$f" ] || { echo "ASSERT FAIL[$stage]: pre-repair $f missing" >&2; exit 1; }
    done
    ;;
  post_repair)
    # Pass-4: artifacts must be MOVED, not deleted. Source paths cleared,
    # quarantine destinations populated.
    for f in "${ARTIFACTS[@]}"; do
      if [ -e "$f" ]; then
        echo "ASSERT FAIL[$stage]: $f should have been quarantined" >&2
        exit 1
      fi
    done

    run_dir="$(find_quarantine_run_dir || true)"
    if [ -z "${run_dir:-}" ]; then
      echo "ASSERT FAIL[$stage]: no .doctor/runs/<id>/ with populated quarantine" >&2
      ls -la "$runs_root" 2>&1 >&2 || true
      exit 1
    fi
    quarantine="$run_dir/quarantine/.beads"
    for f in "${ARTIFACTS[@]}"; do
      name="$(basename "$f")"
      if [ ! -f "$quarantine/$name" ]; then
        echo "ASSERT FAIL[$stage]: $name missing from quarantine $quarantine" >&2
        ls -la "$quarantine" 2>&1 >&2 || true
        exit 1
      fi
    done

    # Re-detect: warning must clear.
    redetect=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$redetect" | jq -r '.checks[] | select(.name == "jsonl.merge_artifacts") | .status' 2>/dev/null || echo "")
    if [ "$status" != "ok" ] && [ -n "$status" ]; then
      echo "ASSERT FAIL[$stage]: jsonl.merge_artifacts still '$status' after repair" >&2
      exit 1
    fi

    # actions.jsonl must record a rename per artifact (3 lines minimum)
    # somewhere across the run dirs we created.
    rename_count=0
    while IFS= read -r d; do
      a="$d/actions.jsonl"
      [ -s "$a" ] || continue
      c=$(grep -c '"op":"rename"' "$a" 2>/dev/null || echo 0)
      c="${c//[[:space:]]/}"
      rename_count=$((rename_count + ${c:-0}))
    done < <(find "$runs_root" -maxdepth 1 -mindepth 1 -type d 2>/dev/null)
    if [ "${rename_count:-0}" -lt 3 ]; then
      echo "ASSERT FAIL[$stage]: expected >=3 rename actions across runs, got $rename_count" >&2
      find "$runs_root" -name actions.jsonl -exec cat {} \; >&2 2>/dev/null || true
      exit 1
    fi
    ;;
  post_undo)
    # `doctor undo latest` must byte-reverse the quarantine: artifacts
    # restored to .beads/.
    for f in "${ARTIFACTS[@]}"; do
      if [ ! -f "$f" ]; then
        echo "ASSERT FAIL[$stage]: $f not restored by undo" >&2
        exit 1
      fi
    done
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
