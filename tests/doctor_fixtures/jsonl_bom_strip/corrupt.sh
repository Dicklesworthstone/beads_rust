#!/usr/bin/env bash
# Fixture: jsonl_bom_strip
# FM: fm-state_files-jsonl-utf8-bom-prefix (P2) — UTF-8 BOM at the
# head of .beads/issues.jsonl.
#
# Initialises a workspace, sync flushes JSONL, then prepends the
# 3-byte UTF-8 BOM. Doctor's `jsonl_bom` check should fire warn;
# `--repair` should rewrite without the BOM via Op::WriteFile;
# `doctor undo` should restore the BOM-prefixed bytes.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "fixture issue 1" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" create --title "fixture issue 2" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Capture the BOM-free baseline bytes — post_repair must match this.
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_post_repair_sha256

# Prepend BOM bytes 0xEF 0xBB 0xBF. The shell printf trick keeps the
# bytes exact across locales.
printf '\xEF\xBB\xBF' > .fixture_bom_prefix
cat .fixture_bom_prefix .beads/issues.jsonl > .beads/issues.jsonl.tmp
mv .beads/issues.jsonl.tmp .beads/issues.jsonl

# Snapshot the BOM-prefixed bytes — post_undo must match this.
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_pre_sha256

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
