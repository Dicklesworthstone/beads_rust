#!/usr/bin/env bash
# Fixture assertions: schemas_missing_required_column
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

fixture_kind=column

# Inspect the planted source without opening its live SQLite family.
# _diag is harness output; every file under .beads must still match the
# byte baseline, including the main database, any WAL/journal, and JSONL.
assert_preserved() {
  python3 - "$stage" "$fixture_kind" <<'PY'
from pathlib import Path, PurePosixPath
import sqlite3
import sys
import tarfile
import tempfile

stage, fixture_kind = sys.argv[1:]

def require(condition, message):
    if not condition:
        raise SystemExit(f"ASSERT FAIL[{stage}]: {message}")

with tarfile.open(".fixture_baseline/state.tar") as archive:
    before = {
        PurePosixPath(member.name).as_posix(): archive.extractfile(member).read()
        for member in archive.getmembers()
        if member.isfile() and PurePosixPath(member.name).parts[0] == ".beads"
    }
after = {
    path.as_posix(): path.read_bytes()
    for path in Path(".beads").rglob("*")
    if path.is_file()
}
require(".beads/beads.db" in before, "baseline database is missing")
require(".beads/issues.jsonl" in before, "baseline JSONL is missing")
changed = sorted(
    path for path in before.keys() | after.keys()
    if before.get(path) != after.get(path)
)
require(not changed, f"tracker bytes changed: {changed}")
for directory in (".doctor", ".br_recovery", ".beads/.br_recovery"):
    require(not Path(directory).exists(), f"unexpected repair artifact: {directory}")
require(
    b"fixture-db-only" not in after[".beads/issues.jsonl"],
    "database-only sentinels unexpectedly entered JSONL",
)

# Native SQLite may create or remove coordination sidecars even for reads.
# Query a disposable copy so the preservation assertion cannot mutate its source.
with tempfile.TemporaryDirectory(prefix="br-doctor-fixture-inspect-") as temporary:
    copied_db = Path(temporary) / "beads.db"
    for suffix in ("", "-wal", "-journal", "-shm"):
        source = ".beads/beads.db" + suffix
        if source in after:
            Path(str(copied_db) + suffix).write_bytes(after[source])
    connection = sqlite3.connect(copied_db)
    try:
        require(connection.execute("PRAGMA user_version").fetchone() == (19,),
                "fixture no longer declares current schema 19")
        if fixture_kind == "column":
            columns = [row[1] for row in connection.execute("PRAGMA table_info(comments)")]
            require("text" not in columns, "comments.text was silently recreated")
        else:
            tables = connection.execute(
                "SELECT name FROM sqlite_schema WHERE type='table' AND name='export_hashes'"
            ).fetchall()
            require(not tables, "export_hashes was silently recreated")
        for table, first_rowid in (("config", 70001), ("metadata", 71001)):
            rows = connection.execute(
                f"SELECT rowid, key, value FROM {table} WHERE key=? ORDER BY rowid",
                ("fixture-db-only-" + table,),
            ).fetchall()
            require(rows == [
                (first_rowid, "fixture-db-only-" + table, "older private value"),
                (first_rowid + 8, "fixture-db-only-" + table, "newer private value"),
            ], f"{table} database-only duplicate records or rowids changed")
        history = connection.execute(
            "SELECT id, actor, comment, agent_name, harness, model FROM events WHERE id=72001"
        ).fetchall()
        require(history == [(
            72001, "fixture-db-only-actor", "fixture-db-only-history",
            "fixture-db-only-agent", "fixture-db-only-harness", "fixture-db-only-model",
        )], "database-only attributed history changed")
    finally:
        connection.close()
PY
}

assert_refusal() {
  jq -e '
    .ok == false
    and .exit_code == 4
    and .code == "refused_unsafe"
    and .gate == "schema_current_layout_unsupported"
    and .evidence.gate == "schema_current_layout_unsupported"
    and .evidence.finding == "schema.runtime"
    and .evidence.user_version == 19
    and .evidence.runtime_compatible == false
    and .evidence.layout_classification == "unsupported_current_layout"
    and .evidence.dry_run == false
    and .recovery_audit.outcome == "refused"
    and ((.recovery_audit.applied_actions // []) | length == 0)
    and (.recovery_audit | has("imported") | not)
  ' "$1" >/dev/null || {
    echo "ASSERT FAIL[$stage]: expected structured unsupported-layout refusal in $1" >&2
    cat "$1" >&2
    exit 1
  }
}

case "$stage" in
  detect)
    assert_preserved
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "schema.columns")
      | select(.status == "error")
      | select(.details.tables // [] | tostring | test("text"))
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: schema.columns did not flag comments.text missing" >&2
      echo "$out" | jq '.checks[] | select(.name == "schema.columns")' >&2
      exit 1
    }
    assert_preserved
    ;;
  post_repair)
    # Check bytes before any further inspection: a failed command may already
    # have replaced the source before reporting an error.
    assert_preserved
    assert_refusal _diag/repair.json
    if [ -f _diag/repair_replay.json ]; then
      assert_refusal _diag/repair_replay.json
    fi
    ;;
  post_undo)
    # Refusal created no repair run. The harness's best-effort undo must
    # leave the same unsupported source and its database-only state intact.
    assert_preserved
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac
