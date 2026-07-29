#!/usr/bin/env bash
# Fixture assertions: base_jsonl_symlink_quarantine
#
# Pass-5 cycle 5: fm-state_files-base-jsonl-missing-or-stale (SYMLINK
# subset) graduates from detect-only to auto-fixed. The fixer renames
# the symlinked anchor into <run-dir>/quarantine/.beads/beads.base.jsonl
# via Op::Rename. fs::rename operates on the link bytes (not its
# target), so the operation is byte-deterministic and reversible by
# `doctor undo`.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "base_jsonl")
      | select(.status == "warn")
      | select(.details.kind == "symlink")
      | select(.details.path | endswith(".beads/beads.base.jsonl"))
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: base_jsonl did not fire warn with kind=symlink" >&2
      echo "$out" | jq '.checks[] | select(.name == "base_jsonl")' >&2
      exit 1
    }
    # Symlink must still be present on disk after detect.
    [ -L .beads/beads.base.jsonl ] || {
      echo "ASSERT FAIL[$stage]: planted symlink missing after detect" >&2
      exit 1
    }
    ;;

  post_repair)
    # Symlink must be GONE from .beads/ (quarantined under run-dir).
    if [ -e .beads/beads.base.jsonl ] || [ -L .beads/beads.base.jsonl ]; then
      echo "ASSERT FAIL[$stage]: symlink still present at .beads/beads.base.jsonl after --repair" >&2
      exit 1
    fi

    # Quarantine destination must contain the (renamed) symlink.
    quarantined=""
    for run_dir in $(find .doctor/runs -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort); do
      q="$run_dir/quarantine/.beads/beads.base.jsonl"
      if [ -L "$q" ] || [ -e "$q" ]; then
        quarantined="$q"
        break
      fi
    done
    if [ -z "$quarantined" ]; then
      echo "ASSERT FAIL[$stage]: quarantined anchor not found under any .doctor/runs/*/quarantine/.beads/" >&2
      find .doctor/runs -type d -maxdepth 4 2>/dev/null | sed 's/^/  /' >&2
      exit 1
    fi
    # Renaming a symlink preserves the link bytes — the quarantined
    # entry MUST itself be a symlink, not a copy of the target.
    if [ ! -L "$quarantined" ]; then
      echo "ASSERT FAIL[$stage]: quarantined entry $quarantined is not a symlink (fs::rename should preserve link bytes)" >&2
      exit 1
    fi

    # actions.jsonl must record the rename under the right fixer_id.
    found=""
    for run_dir in $(find .doctor/runs -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort); do
      if [ -f "$run_dir/actions.jsonl" ] && \
         grep '"fixer_id":"doctor.base_jsonl_symlink_quarantine"' "$run_dir/actions.jsonl" \
           | grep -q '"op":"rename"'; then
        found="$run_dir/actions.jsonl"
        break
      fi
    done
    if [ -z "$found" ]; then
      echo "ASSERT FAIL[$stage]: actions.jsonl missing rename op under doctor.base_jsonl_symlink_quarantine" >&2
      for run_dir in $(find .doctor/runs -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort); do
        echo "  --- $run_dir/actions.jsonl ---" >&2
        [ -f "$run_dir/actions.jsonl" ] && sed 's/^/    /' "$run_dir/actions.jsonl" >&2
      done
      exit 1
    fi

    # Doctor should no longer flag base_jsonl as a symlink (status may
    # transition to ok or to a different kind like stale; we only
    # forbid kind=symlink reappearing).
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    if echo "$out" | jq -e '
      .checks[] | select(.name == "base_jsonl")
      | select(.status == "warn")
      | select(.details.kind == "symlink")
    ' >/dev/null; then
      echo "ASSERT FAIL[$stage]: symlink finding still fires post-repair" >&2
      exit 1
    fi
    ;;

  post_undo)
    # Undo restores SOMETHING at the original path, but for symlink
    # sources the restored entry is a REGULAR FILE containing the
    # link's target content, NOT the symlink itself. This is a
    # documented chokepoint deviation: `copy_verbatim_with_perms`
    # uses `fs::copy` (which follows symlinks) for the backup, and
    # the undo's `from.exists()` check follows the renamed
    # symlink's (now-broken because the relative target doesn't
    # resolve under the run-dir) and finds it missing, so undo
    # falls back to the byte backup and writes it via Op::WriteFile.
    # See README for the full architectural finding.
    [ -e .beads/beads.base.jsonl ] || {
      echo "ASSERT FAIL[$stage]: nothing restored at .beads/beads.base.jsonl after undo" >&2
      exit 1
    }
    # If the implementation is ever upgraded to preserve symlinks
    # through backup+undo, accept that too — the fixture should not
    # block the improvement.
    if [ -L .beads/beads.base.jsonl ]; then
      target=$(readlink .beads/beads.base.jsonl)
      if [ "$target" != "issues.jsonl" ]; then
        echo "ASSERT FAIL[$stage]: symlink target changed (got '$target', expected 'issues.jsonl')" >&2
        exit 1
      fi
    else
      # Regular-file fallback path — verify content matches the link's
      # original target content (issues.jsonl).
      if ! cmp -s .beads/beads.base.jsonl .beads/issues.jsonl; then
        echo "ASSERT FAIL[$stage]: restored regular-file content does not match issues.jsonl" >&2
        diff .beads/beads.base.jsonl .beads/issues.jsonl | head -5 >&2 || true
        exit 1
      fi
    fi
    ;;

  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
