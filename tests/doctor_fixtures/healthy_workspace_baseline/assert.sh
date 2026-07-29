#!/usr/bin/env bash
# Fixture assertions: healthy_workspace_baseline (control)
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$(RUST_LOG=error "$tool_bin" doctor --json 2>/dev/null) || {
      echo "ASSERT FAIL[$stage]: doctor rejected a clean workspace" >&2
      exit 1
    }
    n_non_ok=$(echo "$out" | jq '[.checks[] | select(.status != "ok")] | length')
    if [ "$n_non_ok" -ne 0 ]; then
      echo "ASSERT FAIL[$stage]: clean workspace has non-OK checks" >&2
      echo "$out" | jq '.checks[] | select(.status != "ok")' >&2
      exit 1
    fi
    ;;
  post_repair)
    # Idempotence: repair on a healthy workspace must NOT introduce error checks.
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    n_err=$(echo "$out" | jq '[.checks[] | select(.status == "error")] | length')
    if [ "$n_err" -ne 0 ]; then
      echo "ASSERT FAIL[$stage]: --repair introduced new errors on healthy workspace" >&2
      echo "$out" | jq '.checks[] | select(.status == "error")' >&2
      exit 1
    fi
    # Workspace state must still be queryable.
    "$tool_bin" list --json >/dev/null 2>&1 || {
      echo "ASSERT FAIL[$stage]: br list failed after no-op repair" >&2
      exit 1
    }
    ;;
  post_undo)
    [ -f .beads/beads.db ] || { echo "ASSERT FAIL[$stage]: beads.db gone" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
