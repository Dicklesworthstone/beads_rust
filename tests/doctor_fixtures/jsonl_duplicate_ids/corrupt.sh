#!/usr/bin/env bash
# Fixture: jsonl_duplicate_ids
# FM: fm-state_files-jsonl-duplicate-ids (P2)
#
# Initialises a workspace, then overwrites .beads/issues.jsonl
# with a hand-crafted file that contains two records with the same
# id. Doctor's `jsonl.duplicate_ids` check should fire warn;
# `--repair` must NOT mutate the file (operator decides canonical).

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Create a baseline issue + sync so the JSONL has the project's
# expected leading metadata structure (some workspaces require the
# first line to be a project-state record).
"$tool_bin" create --title "baseline" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Read the existing JSONL header (everything before the first
# normal issue record) so we preserve the project's metadata shape
# while injecting our own duplicates. The detector cares about the
# top-level `id` substring, which is what each issue record carries.
header_path=.fixture_header.jsonl
issue_id_pattern='"id":'

python3 - .beads/issues.jsonl "$header_path" <<'PY'
import sys
src = sys.argv[1]
dst = sys.argv[2]
with open(src, "r", encoding="utf-8") as f, open(dst, "w", encoding="utf-8") as out:
    for line in f:
        # Preserve only the leading non-issue lines (merge_state,
        # metadata snapshots, blank). The first line carrying a
        # top-level id is where we stop.
        if "\"id\":" in line:
            break
        out.write(line)
PY

# Now build the corrupted JSONL: header + 3 records, two of which
# share `bd-aaa`.
cat > .beads/issues.jsonl <<JSONL
{"id":"bd-aaa","title":"first","status":"open"}
{"id":"bd-bbb","title":"unique","status":"open"}
{"id":"bd-aaa","title":"merge-conflict-side","status":"open"}
JSONL
# Prepend any preserved header lines back atop the duplicates.
if [ -s "$header_path" ]; then
  cat "$header_path" .beads/issues.jsonl > .beads/issues.jsonl.tmp
  mv .beads/issues.jsonl.tmp .beads/issues.jsonl
fi

# Snapshot the corrupted bytes — post_repair AND post_undo must
# match this exactly (no doctor mutation allowed).
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_pre_sha256

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
