#!/usr/bin/env bash
# Fixture: blocked_cache_stale
# FM: fm-caches_indexes-blocked-cache-stale (P0)
#
# Plants a workspace with three issues (A blocks on B; B blocks on C), then
# flips `metadata.blocked_cache_state` to 'stale' and replaces the cache rows
# with a ghost row referencing non-existent issue IDs. Triggers
# `db.recoverable_anomalies` = warn with the BLOCKED_CACHE_STALE_FINDING
# message. `--repair` rebuilds the blocked cache via SqliteStorage's
# rebuild path, which now routes through WP4 chokepointed ops.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "alpha" --type task --priority 2 --json >/dev/null
"$tool_bin" create --title "beta"  --type task --priority 2 --json >/dev/null
"$tool_bin" create --title "gamma" --type task --priority 2 --json >/dev/null

# Discover IDs from list output (.issues array, sorted for determinism).
ids=$("$tool_bin" list --json 2>/dev/null | jq -r '.issues[].id' | sort)
a_id=$(echo "$ids" | sed -n 1p)
b_id=$(echo "$ids" | sed -n 2p)
c_id=$(echo "$ids" | sed -n 3p)
[ -n "$a_id" ] && [ -n "$b_id" ] && [ -n "$c_id" ] || {
  echo "corrupt: failed to enumerate three issue IDs" >&2
  exit 1
}

"$tool_bin" dep add "$a_id" "$b_id" --type blocks >/dev/null 2>&1 || true
"$tool_bin" dep add "$b_id" "$c_id" --type blocks >/dev/null 2>&1 || true
"$tool_bin" sync --flush-only >/dev/null 2>&1 || true

# Corrupt: flip marker to 'stale', wipe live rows, plant a ghost row referencing
# non-existent issues. Use stdin to keep dcg happy (raw SQL via heredoc).
printf '%s\n' \
  "UPDATE metadata SET value='stale' WHERE key='blocked_cache_state';" \
  "DELETE FROM blocked_issues_cache;" \
  "INSERT INTO blocked_issues_cache(issue_id, blocked_by, blocked_at) VALUES ('br-9999', '[\"br-9998\"]', '2020-01-01T00:00:00Z');" \
  | sqlite3 .beads/beads.db

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
