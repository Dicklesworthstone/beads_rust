#!/usr/bin/env bash
# Fixture: jsonl_crlf_to_lf
# FM: fm-state_files-jsonl-crlf-line-endings (P2)
#
# Initialises a workspace, sync flushes JSONL, then rewrites the
# file with CRLF line endings. Doctor's `jsonl_crlf` check should
# fire warn; `--repair` should rewrite LF-only via Op::WriteFile;
# `doctor undo` should restore the CRLF state.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "fixture issue 1" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" create --title "fixture issue 2" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Capture the LF baseline — post_repair must match this.
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_post_repair_sha256

# Rewrite with CRLF line endings. Python keeps the byte-level
# semantics explicit and avoids locale issues with sed/awk.
python3 - <<'PY'
path = ".beads/issues.jsonl"
with open(path, "rb") as f:
    data = f.read()
# Replace every \n with \r\n (idempotent: \r\n -> \r\r\n would be
# wrong, but the source data is purely LF so a naive replace works).
crlf = data.replace(b"\n", b"\r\n")
with open(path, "wb") as f:
    f.write(crlf)
PY

# Snapshot the CRLF-corrupted bytes — post_undo must match this.
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_pre_sha256

# Sanity check: file MUST contain at least one \r\n now.
if ! grep -q $'\r' .beads/issues.jsonl; then
  echo "corrupt: failed to inject CRLF" >&2
  exit 1
fi

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
