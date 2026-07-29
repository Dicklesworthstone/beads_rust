#!/usr/bin/env bash
# Fixture assertions: startup_cache_poisoned
#
# Pass-4 cycle 2: fm-configs-startup-cache-poisoned graduates from
# undetected (Tier D) to auto-fixed (Tier A). --repair must quarantine
# every poisoned startup-*.json under <run-dir>/quarantine/startup-cache/
# without touching the .beads/ workspace.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"

# The harness pins HOME=$target_dir, so the binary resolves the cache
# dir via the HOME fallback at $HOME/.cache/beads/startup/.
CACHE_DIR="$target_dir/.cache/beads/startup"

cd "$target_dir"

POISONED_FILES=(
  "$(cat .fixture_cache_file)"
)
IGNORED_CACHE_FILE="$CACHE_DIR/startup-deadbeef.json"

runs_root="$target_dir/.doctor/runs"

find_quarantine_run_dir() {
  [ -d "$runs_root" ] || return 1
  local dir
  while IFS= read -r dir; do
    if [ -f "$dir/quarantine/startup-cache/$(basename "${POISONED_FILES[0]}")" ]; then
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
      .checks[] | select(.name == "startup_cache.health")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: startup_cache.health not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "startup_cache.health")' >&2
      exit 1
    }
    for f in "${POISONED_FILES[@]}"; do
      [ -f "$f" ] || { echo "ASSERT FAIL[$stage]: pre-repair $f missing" >&2; exit 1; }
    done
    [ -f "$IGNORED_CACHE_FILE" ] || {
      echo "ASSERT FAIL[$stage]: unrelated cache sentinel missing" >&2
      exit 1
    }
    flagged_count=$(echo "$out" | jq '[.checks[] | select(.name == "startup_cache.health") | .details.poisoned[]?] | length')
    if [ "${flagged_count:-0}" -ne 1 ]; then
      echo "ASSERT FAIL[$stage]: expected exactly one current-key cache finding, got $flagged_count" >&2
      echo "$out" | jq '.checks[] | select(.name == "startup_cache.health")' >&2
      exit 1
    fi
    ;;
  post_repair)
    # The current-key cache file is quarantined. Unrelated cache entries stay
    # untouched because this workspace would never read them.
    for f in "${POISONED_FILES[@]}"; do
      if [ -e "$f" ]; then
        echo "ASSERT FAIL[$stage]: $f should have been quarantined" >&2
        exit 1
      fi
    done
    [ -f "$IGNORED_CACHE_FILE" ] || {
      echo "ASSERT FAIL[$stage]: unrelated cache file should not be quarantined" >&2
      exit 1
    }

    run_dir="$(find_quarantine_run_dir || true)"
    if [ -z "${run_dir:-}" ]; then
      echo "ASSERT FAIL[$stage]: no .doctor/runs/<id>/ with populated quarantine" >&2
      ls -la "$runs_root" 2>&1 >&2 || true
      exit 1
    fi
    q="$run_dir/quarantine/startup-cache"
    for f in "${POISONED_FILES[@]}"; do
      name="$(basename "$f")"
      if [ ! -f "$q/$name" ]; then
        echo "ASSERT FAIL[$stage]: $name missing from quarantine $q" >&2
        ls -la "$q" 2>&1 >&2 || true
        exit 1
      fi
    done

    # Re-detect: status returns to ok.
    redetect=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$redetect" | jq -r '.checks[] | select(.name == "startup_cache.health") | .status' 2>/dev/null || echo "")
    if [ "$status" != "ok" ] && [ -n "$status" ]; then
      echo "ASSERT FAIL[$stage]: startup_cache.health still '$status' after repair" >&2
      exit 1
    fi

    # At least 1 rename op total across run-dirs.
    rename_count=0
    while IFS= read -r d; do
      a="$d/actions.jsonl"
      [ -s "$a" ] || continue
      c=$(grep -c '"op":"rename"' "$a" 2>/dev/null || echo 0)
      c="${c//[[:space:]]/}"
      rename_count=$((rename_count + ${c:-0}))
    done < <(find "$runs_root" -maxdepth 1 -mindepth 1 -type d 2>/dev/null)
    if [ "${rename_count:-0}" -lt 1 ]; then
      echo "ASSERT FAIL[$stage]: expected >=1 rename action, got $rename_count" >&2
      find "$runs_root" -name actions.jsonl -exec cat {} \; >&2 2>/dev/null || true
      exit 1
    fi

    # .beads/ workspace must be untouched.
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads vanished" >&2; exit 1; }
    ;;
  post_undo)
    for f in "${POISONED_FILES[@]}"; do
      if [ ! -f "$f" ]; then
        echo "ASSERT FAIL[$stage]: $f not restored by undo" >&2
        exit 1
      fi
    done
    [ -f "$IGNORED_CACHE_FILE" ] || {
      echo "ASSERT FAIL[$stage]: unrelated cache file missing after undo" >&2
      exit 1
    }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
