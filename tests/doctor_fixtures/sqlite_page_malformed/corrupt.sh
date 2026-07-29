#!/usr/bin/env bash
# Fixture: sqlite_page_malformed
# FM: fm-state_files-sqlite-page-malformed (P0)
#
# Plants a workspace with a single bead, flushes WAL into the DB, then
# corrupts a non-header page in beads.db. `sqlite.integrity_check` /
# `sqlite3.integrity_check` go to error; `--repair` rebuilds from JSONL.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1
"$tool_bin" create --title "page corrupt seed" --description "rebuild me" \
  --priority 2 --no-auto-flush >/dev/null 2>&1 || true
"$tool_bin" sync --flush-only >/dev/null 2>&1 || true

# Force checkpoint so WAL contents land in the main DB, otherwise the corrupted
# pages get masked.
rm -f .beads/beads.db-wal .beads/beads.db-shm

# Overwrite a B-tree page beyond the SQLite header (header is 100 bytes,
# page-1 is the schema page, page-2+ are user data). Writing junk at offset
# 4096 (page-2 boundary for the default 4 KiB page size) reliably reproduces
# a "btreeInitPage()" failure under `PRAGMA integrity_check`.
python3 - <<'PY'
import os
p = ".beads/beads.db"
sz = os.path.getsize(p)
assert sz >= 8192, f"DB too small to corrupt safely: {sz} bytes"
fd = os.open(p, os.O_RDWR)
try:
    os.lseek(fd, 4096, 0)
    os.write(fd, b"\xff\x00" * 100)
finally:
    os.close(fd)
PY

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
