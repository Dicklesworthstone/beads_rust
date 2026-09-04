#!/usr/bin/env bash
# scripts/br-stress.sh — multi-process mixed read/write stress for a br binary
# against a throwaway copy of a real `.beads/` family.
#
# This is the release gate that caught GitHub #457: single-process checks were
# green while ordinary multi-agent use malformed databases within hours. Always
# run it against a REAL migrated family (history, comments, overflow-sized
# bodies), not just a freshly seeded workspace.
#
# Usage:
#   scripts/br-stress.sh <br-binary> <src-.beads-dir> [workers=8] [seconds=60]
#
# The source family is only read. A pass requires, on the copy after the run:
#   * `PRAGMA integrity_check` == ok (stock sqlite3 CLI or python3 sqlite3
#     when available; otherwise `br doctor --json` integrity checks)
#   * DB issue rows == JSONL records, and every JSONL line parses
#   * no `.br_recovery/` artifacts created after the warm-up rebuild
#   * no `br doctor` ERROR findings
#   * no unexpected error signatures in worker stderr (claim conflicts and
#     lock-timeout retries are expected under contention and are reported
#     but do not fail the run)
set -u

BR="${1:?usage: br-stress.sh <br-binary> <src-.beads-dir> [workers] [seconds]}"
SRC="${2:?usage: br-stress.sh <br-binary> <src-.beads-dir> [workers] [seconds]}"
WORKERS="${3:-8}"
SECS="${4:-60}"

if [[ ! -x "$BR" ]]; then
    echo "br binary not executable: $BR" >&2
    exit 2
fi
if [[ ! -f "$SRC/issues.jsonl" ]]; then
    echo "source family has no issues.jsonl: $SRC" >&2
    exit 2
fi
# Both arguments are used after `cd "$WORK"`, so relative paths (the CI
# steps pass `target/debug/br` and `.beads`) must be made absolute first.
BR="$(cd "$(dirname "$BR")" && pwd -P)/$(basename "$BR")"
SRC="$(cd "$SRC" && pwd -P)"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/br-stress-XXXXXX")"
cd "$WORK" || exit 2
git init -q .
git config user.email stress@example.invalid
git config user.name stress
mkdir .beads
for f in beads.db beads.db-wal issues.jsonl beads.base.jsonl config.yaml metadata.json .gitignore; do
    [[ -e "$SRC/$f" ]] && cp "$SRC/$f" ".beads/$f"
done
git add -A >/dev/null 2>&1
git commit -qm seed >/dev/null 2>&1

echo "[stress] workspace=$WORK br=$("$BR" --version 2>&1 | head -1) family=$SRC"

integrity() {
    # Prints the first integrity_check line for .beads/beads.db (+WAL).
    cp .beads/beads.db probe.db
    if [[ -f .beads/beads.db-wal ]]; then
        cp .beads/beads.db-wal probe.db-wal
    else
        rm -f probe.db-wal
    fi
    if command -v sqlite3 >/dev/null 2>&1; then
        sqlite3 probe.db 'PRAGMA integrity_check' 2>&1 | head -1
    elif command -v python3 >/dev/null 2>&1; then
        python3 - <<'PY' 2>&1 | head -1
import sqlite3
print(sqlite3.connect("probe.db").execute("PRAGMA integrity_check").fetchone()[0])
PY
    else
        echo "unavailable"
    fi
}

db_rows() {
    if command -v sqlite3 >/dev/null 2>&1; then
        sqlite3 probe.db 'SELECT count(*) FROM issues' 2>&1 | head -1
    elif command -v python3 >/dev/null 2>&1; then
        python3 -c 'import sqlite3; print(sqlite3.connect("probe.db").execute("SELECT count(*) FROM issues").fetchone()[0])' 2>&1
    else
        echo "unavailable"
    fi
}

# Warm the family (rebuilds the DB from JSONL when the copy has none).
"$BR" ready --json >/dev/null 2>pre-ready.err
echo "[stress] pre-run br ready rc=$? $(head -c 200 pre-ready.err)"
echo "[stress] pre-run integrity: $(integrity)"

"$BR" list --status all --limit 0 --json 2>/dev/null \
    | python3 -c 'import json,sys
d=json.load(sys.stdin)
issues=d.get("issues",d) if isinstance(d,dict) else d
print("\n".join(i["id"] for i in issues if isinstance(i,dict) and i.get("status") not in ("tombstone",)))' > ids.txt
NIDS="$(wc -l < ids.txt | tr -d ' ')"
echo "[stress] $NIDS listable issues"
if [[ "$NIDS" -eq 0 ]]; then
    echo "[stress] FAIL: no issues listable from the family copy"
    exit 1
fi
# Artifacts written while warming the copy (for example a rebuild's own
# pre-compaction backup) are not stress findings; only new ones count.
BASELINE_REC="$(find .beads/.br_recovery -type f 2>/dev/null | sort)"
echo "[stress] pre-run recovery artifacts: $(printf '%s' "$BASELINE_REC" | grep -c . || true)"

worker() {
    local n=$1 end=$((START + SECS)) ok=0 fail=0 rc id
    while [[ "$(date +%s)" -lt "$end" ]]; do
        id="$(sed -n "$(( (RANDOM % NIDS) + 1 ))p" ids.txt)"
        case $((RANDOM % 7)) in
            0) "$BR" update "$id" --priority $((RANDOM % 4)) >/dev/null 2>>"w$n.err"; rc=$? ;;
            1) "$BR" comments add "$id" "worker $n at $(date +%s) $(head -c 3000 /dev/zero | tr '\0' 'x')" >/dev/null 2>>"w$n.err"; rc=$? ;;
            2) "$BR" update "$id" --claim --actor "w$n" >/dev/null 2>>"w$n.err"; rc=$? ;;
            3) "$BR" list --status open --limit 20 >/dev/null 2>>"w$n.err"; rc=$? ;;
            4) "$BR" create --title "w$n new $(date +%s)$RANDOM" --priority 3 --description "$(head -c 4500 /dev/zero | tr '\0' 'd')" >/dev/null 2>>"w$n.err"; rc=$? ;;
            5) "$BR" update "$id" --notes "note from w$n $(date +%s)" >/dev/null 2>>"w$n.err"; rc=$? ;;
            6) "$BR" ready --json >/dev/null 2>>"w$n.err"; rc=$? ;;
        esac
        if [[ "$rc" -eq 0 ]]; then ok=$((ok + 1)); else fail=$((fail + 1)); fi
    done
    echo "$ok $fail" > "w$n.count"
}

START="$(date +%s)"
for n in $(seq 1 "$WORKERS"); do worker "$n" & done
wait

OK=0; FAIL=0
for n in $(seq 1 "$WORKERS"); do
    read -r o f < "w$n.count"
    OK=$((OK + o)); FAIL=$((FAIL + f))
done
echo "[stress] $WORKERS workers x ${SECS}s: ok=$OK fail=$FAIL"
echo "[stress] distinct error lines (claim conflicts / lock waits are expected):"
cat w*.err 2>/dev/null | sed -E 's/[0-9]{5,}/N/g' | sort | uniq -c | sort -rn | head -8

UNEXPECTED="$(cat w*.err 2>/dev/null | grep -icE 'malformed|corrupt|snapshot conflict|unable to open database|not found after insert|more than one row|export failed' || true)"

IC="$(integrity)"
DB="$(db_rows)"
JL="$(wc -l < .beads/issues.jsonl | tr -d ' ')"
BADJSON="$(python3 -c 'import json,sys
bad=0
for line in open(".beads/issues.jsonl"):
    line=line.strip()
    if not line: continue
    try: json.loads(line)
    except Exception: bad+=1
print(bad)' 2>/dev/null || echo "unavailable")"
DOCTOR_ERR="$("$BR" doctor --json 2>/dev/null | python3 -c 'import json,sys
d=json.load(sys.stdin)
checks=d.get("checks") or d.get("results") or []
print(sum(1 for c in checks if isinstance(c,dict) and str(c.get("status","")).lower()=="error"))' 2>/dev/null || echo "unavailable")"
NEW_REC="$(comm -13 <(printf '%s\n' "$BASELINE_REC" | grep .) <(find .beads/.br_recovery -type f 2>/dev/null | sort))"
REC="$(printf '%s' "$NEW_REC" | grep -c . || true)"
if [[ "$REC" -gt 0 ]]; then
    echo "[stress] new recovery artifacts:"
    printf '%s\n' "$NEW_REC"
fi

echo "[stress] integrity=$IC db_rows=$DB jsonl_records=$JL bad_jsonl_lines=$BADJSON doctor_errors=$DOCTOR_ERR recovery_artifacts=$REC unexpected_error_lines=$UNEXPECTED"

if [[ "$IC" == "ok" && "$DB" == "$JL" && "$BADJSON" == "0" && "$DOCTOR_ERR" == "0" && "$REC" -eq 0 && "$UNEXPECTED" -eq 0 ]]; then
    echo "[stress] PASS ($WORK)"
    exit 0
fi
echo "[stress] FAIL ($WORK)"
exit 1
