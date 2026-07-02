#!/usr/bin/env bash
# Fixture: mcp_serve_stale_write_lock
# FM: fm-agent_coordination-mcp-serve-stale-write-lock.
#
# Simulates a killed `br serve` owner by planting a stale regular write lock
# and an orphan holder-pid sidecar. The live doctor detector intentionally
# handles this through the general write_lock stale-mtime check.

set -euo pipefail

target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"

"$tool_bin" init --quiet 2>&1
"$tool_bin" create --title "mcp stale lock seed" --type task --priority 2 --json >/dev/null

{
    printf '.write.lock\n'
    printf '*.tmp\n'
} > .beads/.gitignore

: > .beads/.write.lock
printf '99999999\n' > .beads/.write.lock.holder.pid
touch -d '2024-01-01T00:00:00Z' .beads/.write.lock
touch -d '2024-01-01T00:00:00Z' .beads/.write.lock.holder.pid

echo "fixture corrupt.sh: planted stale .write.lock and orphan holder pid sidecar" >&2
