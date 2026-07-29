#!/usr/bin/env bash
# Fixture assertions: base_jsonl_stale_regen
#
# Pass-5 cycle 6: fm-state_files-base-jsonl-missing-or-stale (STALE
# subset) graduates from detect-only to auto-fixed. The fixer rewrites
# .beads/beads.base.jsonl with the current .beads/issues.jsonl bytes
# via Op::WriteFile. doctor undo restores the pre-fix anchor
# byte-deterministically from the chokepoint snapshot.

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
      | select(.details.kind == "stale")
      | select(.details.path | endswith(".beads/beads.base.jsonl"))
      | select(.details.live_jsonl | endswith(".beads/issues.jsonl"))
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: base_jsonl did not fire warn with kind=stale" >&2
      echo "$out" | jq '.checks[] | select(.name == "base_jsonl")' >&2
      exit 1
    }

    # Stale anchor must still be on disk (regular file, not symlink),
    # and its content must match the planted stale placeholder.
    [ -f .beads/beads.base.jsonl ] || {
      echo "ASSERT FAIL[$stage]: anchor missing after detect" >&2
      exit 1
    }
    [ -L .beads/beads.base.jsonl ] && {
      echo "ASSERT FAIL[$stage]: anchor became a symlink during detect" >&2
      exit 1
    }
    if ! cmp -s .beads/beads.base.jsonl .fixture_baseline_stale; then
      echo "ASSERT FAIL[$stage]: planted stale anchor content drifted during detect" >&2
      exit 1
    fi
    ;;

  post_repair)
    # Anchor still exists at its canonical path.
    [ -f .beads/beads.base.jsonl ] || {
      echo "ASSERT FAIL[$stage]: anchor missing after --repair" >&2
      exit 1
    }
    # Anchor content is byte-equal to the live JSONL (the fixer reads
    # issues.jsonl bytes and Op::WriteFile-writes them to the anchor).
    if ! cmp -s .beads/beads.base.jsonl .beads/issues.jsonl; then
      echo "ASSERT FAIL[$stage]: regenerated anchor does not match issues.jsonl" >&2
      diff .beads/beads.base.jsonl .beads/issues.jsonl | head -10 >&2 || true
      exit 1
    fi
    # The planted stale content must be GONE.
    if cmp -s .beads/beads.base.jsonl .fixture_baseline_stale; then
      echo "ASSERT FAIL[$stage]: anchor still has the planted stale content" >&2
      exit 1
    fi

    # actions.jsonl must record the write_file under the right fixer_id.
    found=""
    for run_dir in $(find .doctor/runs -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort); do
      if [ -f "$run_dir/actions.jsonl" ] && \
         grep '"fixer_id":"doctor.base_jsonl_regen"' "$run_dir/actions.jsonl" \
           | grep -q '"op":"write_file"'; then
        found="$run_dir/actions.jsonl"
        break
      fi
    done
    if [ -z "$found" ]; then
      echo "ASSERT FAIL[$stage]: actions.jsonl missing write_file op under doctor.base_jsonl_regen" >&2
      for run_dir in $(find .doctor/runs -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort); do
        echo "  --- $run_dir/actions.jsonl ---" >&2
        [ -f "$run_dir/actions.jsonl" ] && sed 's/^/    /' "$run_dir/actions.jsonl" >&2
      done
      exit 1
    fi

    # Doctor should no longer flag base_jsonl=stale. After regeneration
    # both files are byte-equal and the anchor mtime is now >= live.
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    if echo "$out" | jq -e '
      .checks[] | select(.name == "base_jsonl")
      | select(.status == "warn")
      | select(.details.kind == "stale")
    ' >/dev/null; then
      echo "ASSERT FAIL[$stage]: stale finding still fires post-repair" >&2
      exit 1
    fi
    ;;

  post_undo)
    # Undo should restore the pre-fix STALE anchor bytes from the
    # chokepoint backup. Unlike the symlink cycle (54), this fixture
    # has no follow-the-link complication — the source was always a
    # regular file, so backup+restore round-trips byte-deterministically.
    [ -f .beads/beads.base.jsonl ] || {
      echo "ASSERT FAIL[$stage]: anchor missing after undo" >&2
      exit 1
    }
    if ! cmp -s .beads/beads.base.jsonl .fixture_baseline_stale; then
      echo "ASSERT FAIL[$stage]: undo did not restore planted stale anchor bytes" >&2
      diff .beads/beads.base.jsonl .fixture_baseline_stale | head -10 >&2 || true
      exit 1
    fi
    ;;

  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
