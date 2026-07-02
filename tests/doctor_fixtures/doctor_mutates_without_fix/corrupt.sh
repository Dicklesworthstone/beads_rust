#!/usr/bin/env bash
# Fixture: doctor_mutates_without_fix
# FM: fm-state_files-doctor-mutates-without-fix.

set -euo pipefail

target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"

"$tool_bin" init --quiet 2>&1
"$tool_bin" create --title "alpha" --type task --priority 2 --json >/dev/null
"$tool_bin" create --title "beta" --type task --priority 2 --json >/dev/null
"$tool_bin" create --title "gamma" --type task --priority 2 --json >/dev/null
"$tool_bin" sync --flush-only --json >/dev/null

printf 'this is not a SQLite' > .beads/beads.db

if [ ! -f .beads/beads.db-wal ]; then
    echo "fixture corrupt.sh: expected a WAL sidecar to exercise the SHM creation regression" >&2
    exit 1
fi
if [ -e .beads/beads.db-shm ]; then
    echo "fixture corrupt.sh: expected frankensqlite-style WAL without SHM before doctor" >&2
    exit 1
fi

mkdir -p .fixture_baseline
( cd .beads && find . -type f -printf '%P\n' | sort ) > .fixture_baseline/beads.files
( cd .beads && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -d ' ' -f 1 ) \
    > .fixture_baseline/beads.sha256

echo "fixture corrupt.sh: planted malformed beads.db with WAL/no-SHM sidecar" >&2
