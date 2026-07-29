#!/usr/bin/env bash
# Fixture: routes_jsonl_corrupt
# FM: fm-routes_external-routes-jsonl-corrupt (P1) — detect-only.
#
# Plants a `.beads/routes.jsonl` with two well-formed lines and one
# malformed line. The new `routes_jsonl` detector (pass-2) must flag
# it at warn level reporting the bad line number. `--repair` must not
# silently rewrite the file — operator intent is unknowable.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Plant routes.jsonl with mixed valid/invalid content. Line 1 is
# well-formed, line 2 is invalid JSON, line 3 is well-formed.
cat > .beads/routes.jsonl <<'JSONL'
{"prefix":"api-","path":"../api"}
{not json at all}
{"prefix":"ops-","path":"/srv/projects/ops/.beads"}
JSONL

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
