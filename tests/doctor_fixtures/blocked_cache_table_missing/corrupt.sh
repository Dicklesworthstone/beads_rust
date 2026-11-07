#!/usr/bin/env bash
# Fixture: blocked_cache_table_missing
# FM: fm-caches_indexes-blocked-cache-table-missing (P0)
#
# Drops the `blocked_issues_cache` table and its index — the structural twin
# of the FTS-missing false-negative (br-#152). The doctor's `schema.tables`
# check goes to `error` because `blocked_issues_cache` is in the required-
# tables list. For an otherwise admitted current layout, `--repair` uses
# dedicated transactional maintenance to recreate the cache and its index in
# place and mark it stale. That invocation ends after reporting remaining
# findings; it does not proceed to JSONL reconstruction or other fixers.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "alpha" --type task --priority 2 --json >/dev/null
"$tool_bin" create --title "beta"  --type task --priority 2 --json >/dev/null
ids=$("$tool_bin" list --json 2>/dev/null | jq -r '.issues[].id' | sort)
a_id=$(echo "$ids" | sed -n 1p)
b_id=$(echo "$ids" | sed -n 2p)
[ -n "$a_id" ] && [ -n "$b_id" ] || {
  echo "corrupt: failed to enumerate issue IDs" >&2
  exit 1
}
"$tool_bin" dep add "$a_id" "$b_id" --type blocks >/dev/null 2>&1 || true
"$tool_bin" sync --flush-only >/dev/null 2>&1 || true

# The test harness removes the derived cache and its index. The narrow
# maintenance fixer recreates both in place.
printf '%s\n' \
  "DROP INDEX IF EXISTS idx_blocked_cache_blocked_at;" \
  "DROP TABLE IF EXISTS blocked_issues_cache;" \
  | sqlite3 .beads/beads.db

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
