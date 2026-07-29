#!/usr/bin/env bash
# Fixture assertions: orphaned_write_lock
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

# Force the stale-mtime branch by overriding the staleness threshold to 0.
# Any non-future mtime is then "older than threshold" — which, since
# GitHub #395, only selects the file for a non-blocking flock PROBE: the
# planted lock is free, so the probe acquires it and the check must
# classify Ok. Lock acquisition never updates mtime, so file age alone
# is not evidence of an orphan.
export BR_DOCTOR_STALE_LOCK_THRESHOLD_SECS=0

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    # A free lock — however old the file — must be Ok via the probe.
    echo "$out" | jq -e '
      .checks[] | select(.name == "write_lock")
      | select(.status == "ok")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: free stale-mtime lock must be ok (GH #395)" >&2
      echo "$out" | jq '.checks[] | select(.name == "write_lock")' >&2
      exit 1
    }
    # The classification must come from the probe, not the mtime heuristic.
    echo "$out" | jq -e '
      .checks[] | select(.name == "write_lock")
      | (.details.reason == "probe_acquired_free" or .details.reason == "probe_would_block_live_holder")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: details.reason != probe_acquired_free" >&2
      echo "$out" | jq '.checks[] | select(.name == "write_lock") | .details' >&2
      exit 1
    }
    # Pin the declared FM id to the check (coverage manifest contract).
    # The warn path (stale_unprobed) is unreachable on a workspace whose
    # startup succeeds — an unopenable lock degrades doctor startup
    # before this check runs — so the probe fixture is where the
    # `fm-concurrency_primitives-orphaned-write-lock` id is pinned.
    echo "$out" | jq -e '
      .checks[] | select(.name == "write_lock")
      | select(.details.finding_id == "fm-concurrency_primitives-orphaned-write-lock")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: write_lock finding_id drifted from fm-concurrency_primitives-orphaned-write-lock" >&2
      echo "$out" | jq '.checks[] | select(.name == "write_lock") | .details' >&2
      exit 1
    }
    # The old move-aside advice was the inode-split hazard; it must be gone.
    echo "$out" | jq -e '
      .checks[] | select(.name == "write_lock")
      | ((.details.recommended_fix // "") | test("\\.stale-") | not)
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: move-aside advice must not be suggested" >&2
      exit 1
    }
    ;;
  post_repair)
    # Detect-only: .write.lock MUST still be present byte-identically
    # (doctor must NEVER auto-remove). The fact that --repair completed
    # without mutating it is the safety guarantee.
    [ -f .beads/.write.lock ] || {
      echo "ASSERT FAIL[$stage]: .write.lock vanished after --repair (unsafe; could corrupt a live writer)" >&2
      exit 1
    }
    if [ -L .beads/.write.lock ]; then
      echo "ASSERT FAIL[$stage]: .write.lock became a symlink after --repair (unsafe)" >&2
      exit 1
    fi
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    [ -f .beads/.write.lock ] || { echo "ASSERT FAIL[$stage]: .write.lock gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
