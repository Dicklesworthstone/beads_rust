#!/usr/bin/env bash
# Fixture assertions: doctor_runs_not_creatable
#
# Pass-5 cycle 28: fm-permissions-doctor-runs-not-creatable detects
# a read-only `.doctor/` parent dir. DETECT-ONLY by design —
# operators may have intentionally locked the directory, so
# `--repair` must NOT silently chmod it.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

cur_mode() {
  python3 - .doctor <<'PY'
import os
import sys

print(format(os.stat(sys.argv[1]).st_mode & 0o777, "o"))
PY
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "doctor.runs_creatable")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: doctor.runs_creatable not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "doctor.runs_creatable")' >&2
      exit 1
    }
    mode=$(cur_mode)
    if [ "$mode" != "555" ]; then
      echo "ASSERT FAIL[$stage]: expected pre-fix mode 555, got '$mode'" >&2
      exit 1
    fi
    # Pre-fix details report the mode.
    echo "$out" | jq -e '
      .checks[] | select(.name == "doctor.runs_creatable")
      | .details.mode_octal == "555"
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: doctor.runs_creatable details missing mode_octal=\"555\"" >&2
      echo "$out" | jq '.checks[] | select(.name == "doctor.runs_creatable") | .details' >&2
      exit 1
    }
    ;;
  post_repair)
    # SACRED INVARIANT: doctor must NOT silently chmod .doctor/.
    # Operators may have intentionally locked it. The check
    # surfaces the problem; the operator handles the chmod.
    mode=$(cur_mode)
    if [ "$mode" != "555" ]; then
      echo "ASSERT FAIL[$stage]: doctor silently chmod'd .doctor/ from 0555 to 0$mode (operator intent stomped)" >&2
      exit 1
    fi
    # Restore writability so undo can proceed and TempDir cleanup
    # works. The fixture wraps the sacred-invariant check; once the
    # check passes, the test harness is free to undo the corruption.
    chmod 0755 .doctor
    ;;
  post_undo)
    # `.doctor/` still exists and the seed marker is intact.
    [ -d .doctor ] || { echo "ASSERT FAIL[$stage]: .doctor gone after undo" >&2; exit 1; }
    if [ ! -f .doctor/.doctor_seed_marker ]; then
      echo "ASSERT FAIL[$stage]: seed marker missing after undo" >&2
      exit 1
    fi
    contents=$(cat .doctor/.doctor_seed_marker)
    if [ "$contents" != "fixture-seed" ]; then
      echo "ASSERT FAIL[$stage]: seed marker content drifted: '$contents'" >&2
      exit 1
    fi
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
