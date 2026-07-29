#!/usr/bin/env bash
# Fixture assertions: config_yaml_secret_mode
#
# Pass-5 cycle 26: fm-permissions-config-yaml-mode-leaks-secrets
# graduates from detect-only (Tier B from cycle 11) to auto-fixed
# (Tier A). --repair strips world-read AND world-write bits via
# Op::Chmod (mask 0o006), preserving owner+group bits.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

cur_mode() {
  python3 - .beads/config.yaml <<'PY'
import os
import sys

print(format(os.stat(sys.argv[1]).st_mode & 0o777, "o"))
PY
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "permissions.config_yaml_secrets")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: permissions.config_yaml_secrets not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "permissions.config_yaml_secrets")' >&2
      exit 1
    }
    mode=$(cur_mode)
    if [ "$mode" != "666" ]; then
      echo "ASSERT FAIL[$stage]: expected pre-fix mode 666, got '$mode'" >&2
      exit 1
    fi
    ;;
  post_repair)
    mode=$(cur_mode)
    # Both world bits (006 = world-read | world-write) MUST be
    # cleared. Owner and group bits (660 in the original 666) MUST
    # be preserved.
    if [ "$(( 8#$mode & 8#006 ))" != "0" ]; then
      echo "ASSERT FAIL[$stage]: world bits still set after repair (mode=$mode)" >&2
      exit 1
    fi
    if [ "$mode" != "660" ]; then
      echo "ASSERT FAIL[$stage]: expected post-fix mode 660 (only world bits stripped), got '$mode'" >&2
      exit 1
    fi
    if [ -f "$target_dir/_diag/repair.json" ]; then
      jq -e '
        .repaired == true
        and (.recovery_audit.applied_actions | index("config_yaml_secret_mode_chmod"))
      ' "$target_dir/_diag/repair.json" >/dev/null || {
        echo "ASSERT FAIL[$stage]: repair output did not report config_yaml_secret_mode_chmod" >&2
        cat "$target_dir/_diag/repair.json" >&2
        exit 1
      }
    fi
    # SACRED INVARIANT: config.yaml bytes byte-identical. Op::Chmod
    # only touches mode, never content.
    cfg_now=$(sha256sum .beads/config.yaml | awk '{print $1}')
    cfg_pre=$(cat .fixture_config_pre_sha256)
    if [ "$cfg_now" != "$cfg_pre" ]; then
      echo "ASSERT FAIL[$stage]: config.yaml bytes mutated by --repair (Op::Chmod must not touch content)" >&2
      exit 1
    fi
    redetect=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$redetect" | jq -r '.checks[] | select(.name == "permissions.config_yaml_secrets") | .status' 2>/dev/null || echo "")
    if [ "$status" != "ok" ] && [ -n "$status" ]; then
      echo "ASSERT FAIL[$stage]: permissions.config_yaml_secrets still '$status' after repair" >&2
      exit 1
    fi
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
    cfg_now=$(sha256sum .beads/config.yaml | awk '{print $1}')
    cfg_pre=$(cat .fixture_config_pre_sha256)
    if [ "$cfg_now" != "$cfg_pre" ]; then
      echo "ASSERT FAIL[$stage]: config.yaml bytes drifted across undo" >&2
      exit 1
    fi
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
