#!/usr/bin/env bash
# Fixture: sqlite_version_downgrade
# FM: fm-schemas-sqlite-version-downgrade.

set -euo pipefail

target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"

"$tool_bin" init --quiet 2>&1
"$tool_bin" create --title "schema downgrade fixture" --type task --priority 2 --json >/dev/null

python3 - <<'PY'
from pathlib import Path

db_path = Path(".beads/beads.db")
data = bytearray(db_path.read_bytes())
if len(data) < 64 or data[:16] != b"SQLite format 3\0":
    raise SystemExit("fixture setup expected a SQLite database header")
data[60:64] = (99).to_bytes(4, "big")
db_path.write_bytes(data)
PY

mkdir -p .fixture_baseline
sha256sum .beads/beads.db > .fixture_baseline/beads.db.sha256

echo "fixture corrupt.sh: stamped .beads/beads.db user_version=99" >&2
