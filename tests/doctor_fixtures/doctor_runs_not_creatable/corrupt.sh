#!/usr/bin/env bash
# Fixture: doctor_runs_not_creatable
# FM: fm-permissions-doctor-runs-not-creatable (P2)
#
# Initialises a workspace, creates a `.doctor/` directory at repo
# root, then chmods it to 0o555 (owner-write stripped). Doctor's
# `doctor.runs_creatable` check should fire warn; `--repair` must
# NOT silently chmod the directory (operator intent is sacred).

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1
"$tool_bin" create --title "fixture issue 1" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Materialise `.doctor/` and lock it so the detector has something
# to flag. We use a separate `.doctor_seed_marker` file inside so
# the directory has identifiable content the undo stage can verify.
mkdir -p .doctor
echo "fixture-seed" > .doctor/.doctor_seed_marker
chmod 0555 .doctor

# Snapshot the corrupted mode for diagnostics without relying on GNU stat.
python3 - .doctor > .fixture_pre_mode <<'PY'
import os
import sys

print(format(os.stat(sys.argv[1]).st_mode & 0o777, "o"))
PY

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
