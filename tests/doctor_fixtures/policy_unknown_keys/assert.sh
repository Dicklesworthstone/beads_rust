#!/usr/bin/env bash
# Fixture assertions: policy_unknown_keys
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"

cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "policy.unknown_keys")
      | select(.status == "warn")
      | select(.details.finding_id == "fm-configs-policy-unknown-keys")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: policy.unknown_keys not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "policy.unknown_keys")' >&2
      exit 1
    }
    # The unknown key must be named, with its dotted scope, so the operator
    # can find the typo.
    echo "$out" | jq -e '
      .checks[] | select(.name == "policy.unknown_keys")
      | .details.unknown_keys | index("close_policy.require_close_reasn")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: details.unknown_keys does not name 'close_policy.require_close_reasn'" >&2
      echo "$out" | jq '.checks[] | select(.name == "policy.unknown_keys") | .details' >&2
      exit 1
    }
    # GH #515: the notice must also reach stderr at default verbosity for
    # ordinary commands, not only under -v.
    err=$("$tool_bin" ready --json 2>&1 >/dev/null) || true
    case "$err" in
      *"close_policy.require_close_reasn"*) ;;
      *)
        echo "ASSERT FAIL[$stage]: br ready did not warn about the unknown policy key on stderr" >&2
        echo "$err" >&2
        exit 1
        ;;
    esac
    ;;
  post_repair)
    # Detect-only: the operator's file must be untouched, typo included.
    [ -f .beads/policy.yaml ] || {
      echo "ASSERT FAIL[$stage]: policy.yaml vanished after --repair (unsafe)" >&2
      exit 1
    }
    if ! grep -q 'require_close_reasn:' .beads/policy.yaml; then
      echo "ASSERT FAIL[$stage]: doctor rewrote policy.yaml (the unknown key is gone)" >&2
      cat .beads/policy.yaml >&2
      exit 1
    fi
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    [ -f .beads/policy.yaml ] || {
      echo "ASSERT FAIL[$stage]: policy.yaml gone after undo" >&2
      exit 1
    }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
