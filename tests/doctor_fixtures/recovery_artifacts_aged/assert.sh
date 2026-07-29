#!/usr/bin/env bash
# Fixture assertions: recovery_artifacts_aged
#
# Pass-4 cycle 3: fm-state_files-recovery-artifacts-orphaned (TTL variant)
# graduates from undetected (Tier D) to auto-fixed (Tier A). --repair
# quarantines artifacts older than RECOVERY_AGED_TTL_DAYS (30) but
# PRESERVES recent artifacts in place.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

# Load paths recorded by corrupt.sh.
. ./.fixture_paths

runs_root="$target_dir/.doctor/runs"

find_quarantine_run_dir() {
  [ -d "$runs_root" ] || return 1
  local dir
  while IFS= read -r dir; do
    if [ -f "$dir/quarantine/.beads/.br_recovery/$(basename "$AGED")" ]; then
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
      .checks[] | select(.name == "db.recovery_artifacts.aged")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: db.recovery_artifacts.aged not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "db.recovery_artifacts.aged")' >&2
      exit 1
    }
    [ -f "$AGED" ] || { echo "ASSERT FAIL[$stage]: pre-repair aged $AGED missing" >&2; exit 1; }
    [ -f "$RECENT" ] || { echo "ASSERT FAIL[$stage]: pre-repair recent $RECENT missing" >&2; exit 1; }
    [ -f "$BAD_SIBLING" ] || { echo "ASSERT FAIL[$stage]: pre-repair $BAD_SIBLING missing" >&2; exit 1; }
    # Details should list 2 aged artifacts (the .br_recovery one and the .bad_<TS>).
    aged_count=$(echo "$out" | jq '[.checks[] | select(.name == "db.recovery_artifacts.aged") | .details.artifacts[]?] | length')
    if [ "${aged_count:-0}" -ne 2 ]; then
      echo "ASSERT FAIL[$stage]: expected 2 aged artifacts in details, got $aged_count" >&2
      echo "$out" | jq '.checks[] | select(.name == "db.recovery_artifacts.aged")' >&2
      exit 1
    fi
    ;;
  post_repair)
    # Aged files quarantined; recent file preserved in place.
    if [ -e "$AGED" ]; then
      echo "ASSERT FAIL[$stage]: aged $AGED should have been quarantined" >&2
      exit 1
    fi
    if [ -e "$BAD_SIBLING" ]; then
      echo "ASSERT FAIL[$stage]: aged $BAD_SIBLING should have been quarantined" >&2
      exit 1
    fi
    if [ ! -f "$RECENT" ]; then
      echo "ASSERT FAIL[$stage]: recent $RECENT must be preserved (TTL-bound contract)" >&2
      exit 1
    fi

    run_dir="$(find_quarantine_run_dir || true)"
    if [ -z "${run_dir:-}" ]; then
      echo "ASSERT FAIL[$stage]: no .doctor/runs/<id>/ with populated quarantine" >&2
      ls -la "$runs_root" 2>&1 >&2 || true
      exit 1
    fi
    q="$run_dir/quarantine/.beads/.br_recovery"
    [ -f "$q/$(basename "$AGED")" ] || {
      echo "ASSERT FAIL[$stage]: aged $(basename "$AGED") missing from quarantine $q" >&2
      ls -la "$q" 2>&1 >&2 || true
      exit 1
    }
    [ -f "$q/$(basename "$BAD_SIBLING")" ] || {
      echo "ASSERT FAIL[$stage]: aged $(basename "$BAD_SIBLING") missing from quarantine $q" >&2
      exit 1
    }

    # Re-detect: aged-check returns to ok (info-only db.recovery_artifacts
    # may still warn because of the preserved recent artifact — that's
    # the intended additive design).
    redetect=$("$tool_bin" doctor --json 2>/dev/null) || true
    aged_status=$(echo "$redetect" | jq -r '.checks[] | select(.name == "db.recovery_artifacts.aged") | .status' 2>/dev/null || echo "")
    if [ "$aged_status" != "ok" ] && [ -n "$aged_status" ]; then
      echo "ASSERT FAIL[$stage]: db.recovery_artifacts.aged still '$aged_status' after repair" >&2
      exit 1
    fi

    # At least 2 rename ops across run-dirs.
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

    # .beads/ workspace must be intact (db, jsonl unchanged).
    [ -f .beads/beads.db ] || { echo "ASSERT FAIL[$stage]: beads.db vanished" >&2; exit 1; }
    ;;
  post_undo)
    # Aged artifacts restored at their original paths.
    [ -f "$AGED" ] || { echo "ASSERT FAIL[$stage]: aged $AGED not restored by undo" >&2; exit 1; }
    [ -f "$BAD_SIBLING" ] || { echo "ASSERT FAIL[$stage]: $BAD_SIBLING not restored by undo" >&2; exit 1; }
    # Recent file remained untouched throughout.
    [ -f "$RECENT" ] || { echo "ASSERT FAIL[$stage]: $RECENT missing after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
