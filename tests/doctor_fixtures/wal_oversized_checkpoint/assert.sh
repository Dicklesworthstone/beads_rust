#!/usr/bin/env bash
# Fixture assertions: wal_oversized_checkpoint
#
# Pass-5 cycle 37: fm-state_files-wal-oversized graduates from
# detect-only to auto-fixed via PRAGMA wal_checkpoint(TRUNCATE).
# This is the first fixture to exercise the legacy chokepoint
# (record_legacy_mutation) end-to-end — multi-sidecar audit
# entries in actions.jsonl under fixer_id
# `doctor.wal_checkpoint_truncate`.
#
# WAL-lifecycle caveat: see corrupt.sh — SQLite removes the
# zero-padded WAL on the FIRST connection close, so the
# detect-stage assertion stats the file directly instead of
# invoking `br doctor --json` (which would remove it before
# the harness runs --repair).

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

WAL_THRESHOLD_BYTES=33554432  # 32 MiB; const WAL_OVERSIZED_BYTES in doctor.rs

case "$stage" in
  detect)
    # Verify the planted state is on disk. Intentionally NO `br doctor`
    # call here — see header comment.
    [ -f .beads/beads.db-wal ] || {
      echo "ASSERT FAIL[$stage]: planted WAL sidecar missing" >&2
      exit 1
    }
    size=$(wc -c < .beads/beads.db-wal)
    if [ "${size:-0}" -le "$WAL_THRESHOLD_BYTES" ]; then
      echo "ASSERT FAIL[$stage]: planted WAL only $size bytes (threshold $WAL_THRESHOLD_BYTES)" >&2
      exit 1
    fi
    # Also verify the workspace looks initialised so the harness's
    # subsequent --repair has a real workspace to operate against.
    [ -f .beads/beads.db ] || { echo "ASSERT FAIL[$stage]: beads.db missing" >&2; exit 1; }
    ;;

  post_repair)
    # WAL must have been truncated. Accept <=32MB (the threshold);
    # PRAGMA wal_checkpoint(TRUNCATE) usually shrinks to 0, but any
    # value at or below threshold proves the FM no longer fires.
    if [ -f .beads/beads.db-wal ]; then
      size=$(wc -c < .beads/beads.db-wal)
      if [ "${size:-0}" -gt "$WAL_THRESHOLD_BYTES" ]; then
        echo "ASSERT FAIL[$stage]: WAL still $size bytes after --repair" >&2
        exit 1
      fi
    fi

    # Database must still be queryable post-checkpoint (data-equivalent op).
    [ -f .beads/beads.db ] || { echo "ASSERT FAIL[$stage]: beads.db missing" >&2; exit 1; }
    "$tool_bin" list --json >/dev/null 2>&1 || {
      echo "ASSERT FAIL[$stage]: br list failed post-checkpoint" >&2
      exit 1
    }

    # actions.jsonl in SOME run-dir must record legacy_op entries under
    # the wal_checkpoint fixer_id. The chokepoint records one entry per
    # target (db, -wal, -shm). Search across all run-dirs because under
    # REPLAY_IDEMPOTENCE=1 the harness runs --repair twice and the
    # second run is intentionally empty (idempotent no-op); the actual
    # fixer entries live in the FIRST run-dir, not necessarily "latest".
    run_dirs=$(find .doctor/runs -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort)
    if [ -z "$run_dirs" ]; then
      echo "ASSERT FAIL[$stage]: no run-dirs under .doctor/runs" >&2
      exit 1
    fi
    matching_lines=""
    for run_dir in $run_dirs; do
      if [ -f "$run_dir/actions.jsonl" ]; then
        if grep '"fixer_id":"doctor.wal_checkpoint_truncate"' "$run_dir/actions.jsonl"; then
          matching_lines="yes"
          break
        fi
      fi
    done
    if [ -z "$matching_lines" ]; then
      echo "ASSERT FAIL[$stage]: no run-dir actions.jsonl contains doctor.wal_checkpoint_truncate" >&2
      for run_dir in $run_dirs; do
        echo "  --- $run_dir/actions.jsonl ---" >&2
        [ -f "$run_dir/actions.jsonl" ] && sed 's/^/    /' "$run_dir/actions.jsonl" >&2
      done
      exit 1
    fi
    # Each legacy_op entry must carry op=legacy_op (look across all runs).
    if ! grep -h '"fixer_id":"doctor.wal_checkpoint_truncate"' .doctor/runs/*/actions.jsonl 2>/dev/null \
      | grep -q '"op":"legacy_op"'; then
      echo "ASSERT FAIL[$stage]: wal_checkpoint entries are not legacy_op ops" >&2
      exit 1
    fi
    ;;

  post_undo)
    # Undo restores whatever the chokepoint snapshotted. On this
    # codepath the snapshot was taken AFTER the sqlite3 CLI
    # integrity check had already cleaned up our planted WAL
    # (see corrupt.sh header), so the snapshot for the WAL is
    # empty/absent and undo cannot restore the 33MB inflated
    # state. We assert only that the workspace is still functional.
    [ -f .beads/beads.db ] || { echo "ASSERT FAIL[$stage]: beads.db missing after undo" >&2; exit 1; }
    "$tool_bin" list --json >/dev/null 2>&1 || {
      echo "ASSERT FAIL[$stage]: br list failed post-undo" >&2
      exit 1
    }
    ;;

  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
