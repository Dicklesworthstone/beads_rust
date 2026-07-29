#!/usr/bin/env bash
# Fixture: base_jsonl_missing_post_flush
# FM: fm-state_files-base-jsonl-missing-or-stale (missing-post-flush subset)
#
# Older skeletons removed .beads/beads.base.jsonl after a sync flush. This
# fixture avoids deletion: a fresh `br init` workspace already has no anchor,
# so we synthesize the post-flush evidence by setting metadata.last_export_time
# directly.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Avoid unrelated inner-gitignore repair noise; this fixture is testing the
# detect-only missing-post-flush branch.
for pattern in ".write.lock" "*.tmp"; do
  if ! grep -Fxq "$pattern" .beads/.gitignore 2>/dev/null; then
    printf '\n%s\n' "$pattern" >> .beads/.gitignore
  fi
done

sqlite3 .beads/beads.db \
  "UPDATE metadata SET value='2026-05-01T00:00:00Z' WHERE key='last_export_time';"

if [ -e .beads/beads.base.jsonl ]; then
  echo "corrupt.sh: fresh workspace unexpectedly has .beads/beads.base.jsonl" >&2
  exit 1
fi

sqlite3 .beads/beads.db \
  "SELECT value FROM metadata WHERE key='last_export_time' ORDER BY rowid DESC LIMIT 1;" \
  > .fixture_last_export_time

if [ "$(cat .fixture_last_export_time)" != "2026-05-01T00:00:00Z" ]; then
  echo "corrupt.sh: failed to plant metadata.last_export_time" >&2
  exit 1
fi

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

