#!/usr/bin/env bash
# Fixture: recovery_dir_not_writable
# FM: fm-permissions-recovery-dir-not-writable.

set -euo pipefail

target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"

"$tool_bin" init --quiet 2>&1

mkdir -p .beads/.br_recovery
printf 'fixture-seed\n' > .beads/.br_recovery/sentinel.txt
chmod 0555 .beads/.br_recovery

echo "fixture corrupt.sh: locked .beads/.br_recovery at mode 0555" >&2
