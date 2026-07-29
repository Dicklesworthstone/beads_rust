#!/usr/bin/env bash
# Fixture: jsonl_oversized
# FM: fm-state_files-jsonl-oversized (P3) — `.beads/issues.jsonl`
# exceeds the 100MB `jsonl_size` threshold; flushes become slow and the
# full-file parse pressures low-RAM hosts.
#
# Plant: pad the JSONL past the threshold with whitespace-only lines.
# Every JSONL scanner in br skips blank/whitespace lines, so the file
# stays PARSE-VALID (jsonl.parse ok) while its size trips the
# detect-only size check — isolating fm-state_files-jsonl-oversized
# from the malformed-content findings. 104 lines of 1MB spaces keeps
# line-iteration counts trivial (no 100M-line pathology).

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "small real issue" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Record the pre-padding size so post_undo can shrink the retained
# workspace back down after a fully-passed run (the padding is APPENDED,
# so truncating to this size restores the original bytes exactly).
stat -c '%s' .beads/issues.jsonl > .fixture_prepad_size

python3 <<'PY'
line = b" " * (1024 * 1024) + b"\n"
with open(".beads/issues.jsonl", "ab") as f:
    for _ in range(104):
        f.write(line)
PY

# Sanity: the padded file must exceed the 100MB threshold.
size=$(stat -c '%s' .beads/issues.jsonl)
if [ "$size" -le $((100 * 1024 * 1024)) ]; then
  echo "corrupt: padded JSONL is only $size bytes (<=100MB threshold)" >&2
  exit 1
fi
echo "$size" > .fixture_planted_size

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
# The ~105MB padded JSONL would double the fixture's disk cost inside the
# baseline tar, and run_all.sh retains every workspace even on pass.
# Nothing consumes state.tar for this fixture, so exclude the padded file
# and record its identity (size above, sha256 here) for forensics instead.
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_baseline/issues.jsonl.sha256
tar --exclude=.fixture_baseline --exclude='*/issues.jsonl' -cf .fixture_baseline/state.tar .
# Guard the exclusion: a tar that silently captured the padding anyway
# would reintroduce the double-cost.
tar_size=$(stat -c '%s' .fixture_baseline/state.tar)
if [ "$tar_size" -gt $((10 * 1024 * 1024)) ]; then
  echo "corrupt: baseline tar is $tar_size bytes; issues.jsonl exclusion failed" >&2
  exit 1
fi
