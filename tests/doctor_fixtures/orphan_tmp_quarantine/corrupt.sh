#!/usr/bin/env bash
# Fixture: orphan_tmp_quarantine
# FM: fm-state_files-orphan-tmp-files (P2)
#
# Initialises a workspace, plants two .tmp-shaped files in .beads/
# and backdates their mtimes past the 1-hour threshold. Doctor's
# `tmp_files_orphan` check should fire warn; `--repair` should
# rename each into the per-run quarantine via Op::Rename; `doctor
# undo` should restore them byte-deterministically.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "fixture issue 1" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# JSONL byte-checksum — orphan tmp quarantine must NEVER touch JSONL.
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_pre_sha256

# Plant two orphans: one plain *.tmp, one *.tmp.<digits> (the
# numbered shape some atomic writers use).
echo 'orphan one' > .beads/orphan-one.tmp
echo 'orphan two' > .beads/orphan-two.tmp.42

# Snapshot pre-fix bytes so post_undo can byte-verify the restore.
sha256sum .beads/orphan-one.tmp | awk '{print $1}' > .fixture_orphan_one_sha256
sha256sum .beads/orphan-two.tmp.42 | awk '{print $1}' > .fixture_orphan_two_sha256

# Backdate mtime past the 1-hour threshold. Use Python instead of
# GNU-only `touch -d` so the fixture also runs on BSD/macOS hosts.
python3 - .beads/orphan-one.tmp .beads/orphan-two.tmp.42 <<'PY'
import os
import sys
import time

mtime = time.time() - 2 * 60 * 60
for path in sys.argv[1:]:
    os.utime(path, (mtime, mtime))
PY

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
