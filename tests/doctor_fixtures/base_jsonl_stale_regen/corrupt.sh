#!/usr/bin/env bash
# Fixture: base_jsonl_stale_regen
# FM: fm-state_files-base-jsonl-missing-or-stale (P2, STALE subset)
#
# Initialises a workspace with a seed issue, writes a hand-crafted
# stale `.beads/beads.base.jsonl` whose mtime predates the live
# `.beads/issues.jsonl`, then backdates the anchor's mtime so the
# detector's `base_mtime < live_mtime` predicate fires.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1
"$tool_bin" create "seed-base-jsonl-stale" --type task --priority 2 \
    >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Plant a hand-crafted "stale" anchor with content that's recognisably
# different from the live JSONL so the post_repair byte-equality check
# is meaningful. Content is also stored at .fixture_baseline_stale so
# the post_undo check can verify the chokepoint backup round-trips
# byte-for-byte.
cat > .beads/beads.base.jsonl <<'STALE'
{"id":"br-stale-fixture-snapshot","title":"stale anchor placeholder","status":"open","priority":2,"issue_type":"task"}
STALE
cp .beads/beads.base.jsonl .fixture_baseline_stale

# Backdate the anchor mtime so it predates issues.jsonl. Use a year-old
# epoch to leave wide margin even on filesystems with second-only
# resolution.
touch -m -d '2025-01-01 00:00:00' .beads/beads.base.jsonl

# Sanity: live JSONL is newer than the anchor.
base_mtime=$(stat -c '%Y' .beads/beads.base.jsonl)
live_mtime=$(stat -c '%Y' .beads/issues.jsonl)
if [ "$base_mtime" -ge "$live_mtime" ]; then
    echo "corrupt.sh: anchor mtime ($base_mtime) is not older than live ($live_mtime)" >&2
    exit 1
fi

if [ -e .fixture_baseline ]; then
    echo "fixture baseline already exists; expected a fresh workspace" >&2
    exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
