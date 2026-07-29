#!/usr/bin/env bash
# Fixture: blocked_cache_content_mismatch
# FM: fm-caches_indexes-blocked-cache-stale (P0) — variant: content mismatch
#   without the stale marker.
#
# Seeds A blocks B (real dependency), then replaces the cache rows with a
# ghost row referencing non-existent issues but leaves the
# `metadata.blocked_cache_state` marker at 'current'. The detector compares
# the cache projection against the direct dependency graph and surfaces a
# BLOCKED_CACHE_CONTENT_MISMATCH_FINDING (along with the ready-projection
# mismatch since it shares the same graph). Confirms detection works even
# when the stale marker is not flipped.

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

# Wipe live rows; plant a single ghost row. Marker is left at 'current' so
# only the projection-health probe flags it.
printf '%s\n' \
  "DELETE FROM blocked_issues_cache;" \
  "INSERT INTO blocked_issues_cache(issue_id, blocked_by, blocked_at) VALUES ('br-9999', '[\"br-9998\"]', '2020-01-01T00:00:00Z');" \
  | sqlite3 .beads/beads.db

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
