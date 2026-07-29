#!/usr/bin/env bash
# Fixture: dirty_flag_divergence

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"

"$tool_bin" init --quiet

# Avoid unrelated fixture noise so this fixture is about sync.metadata only.
grep -qxF ".write.lock" .beads/.gitignore 2>/dev/null || printf "\n.write.lock\n" >> .beads/.gitignore

issue_id="$("$tool_bin" create --title "dirty flag divergence seed" --type task --priority 2 --json | jq -r '.id')"
"$tool_bin" sync --flush-only --json >/dev/null
cp .beads/issues.jsonl .beads/beads.base.jsonl

sqlite3 .beads/beads.db \
  "INSERT OR REPLACE INTO dirty_issues(issue_id, marked_at) VALUES('$issue_id', '2026-01-01T00:00:00Z')"

sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_sha256
printf '%s\n' "BR_DOCTOR_FIXTURE_REPAIR_ARGS=--only fm-state_files-dirty-flag-divergence" > .fixture_env

mkdir -p "$target_dir/.fixture_baseline"
( cd "$target_dir" && tar cf - --exclude=.fixture_baseline --exclude=./.fixture_baseline . ) | \
  ( cd "$target_dir/.fixture_baseline" && tar xf - )
