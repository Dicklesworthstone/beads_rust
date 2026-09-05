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
#   * every row from Python sqlite3 `PRAGMA integrity_check` == the single `ok`
#   * DB issue rows == JSONL records, and every JSONL line parses
#   * no `.br_recovery/` artifacts created after the warm-up rebuild
#   * no `br doctor` ERROR findings
#   * no unexpected error signatures in worker stderr (claim conflicts and
#     lock-timeout retries are expected under contention and are reported
#     but do not fail the run)
# Each command retains argv, timestamps, exit status, stdout and stderr. A
# nonzero exit has an unknown commit outcome; this integrity stress does not
# replace the separate linearizability checker. JSONL-only source copies are
# identified explicitly and do not prove retained-database migration safety.
set -u -o pipefail

BR="${1:?usage: br-stress.sh <br-binary> <src-.beads-dir> [workers] [seconds]}"
SRC="${2:?usage: br-stress.sh <br-binary> <src-.beads-dir> [workers] [seconds]}"
WORKERS="${3:-8}"
SECS="${4:-60}"
COMMAND_TIMEOUT="${BR_STRESS_COMMAND_TIMEOUT_SECONDS:-60}"

for value in "$WORKERS" "$SECS" "$COMMAND_TIMEOUT"; do
    if [[ ! "$value" =~ ^[1-9][0-9]*$ ]]; then
        echo "workers, seconds and command timeout must be positive integers" >&2
        exit 2
    fi
done

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
mkdir .beads commands || exit 2

# A live br workspace has a persistent advisory write lock. Open it read-only
# and hold a shared lock while copying; never create or modify source files.
# A checked-out JSONL-only corpus may lack that lock: the before/after content
# and inode witness below still has to match, and the receipt names that case.
if [[ -f "$SRC/.write.lock" && ! -L "$SRC/.write.lock" ]]; then
    exec {SOURCE_LOCK_FD}<"$SRC/.write.lock" || exit 2
    flock -s -w 30 "$SOURCE_LOCK_FD" || exit 2
fi
python3 - "$SRC" "$WORK" <<'PY' || exit 2
import hashlib, json, pathlib, shutil, sys
source, work = map(pathlib.Path, sys.argv[1:])
names = {"issues.jsonl", "beads.base.jsonl", "config.yaml", "metadata.json", "policy.yaml", ".gitignore", ".br_history", ".br_recovery"}
def files(root):
    for entry in sorted(root.iterdir()):
        if entry.name.startswith("beads.db") or entry.name in names:
            if entry.is_symlink():
                raise RuntimeError(f"refusing symlink in source family: {entry}")
            if entry.is_dir():
                yield from sorted(p for p in entry.rglob("*") if p.is_symlink() or not p.is_dir())
            else:
                yield entry
def inventory(root):
    result = {}
    for path in files(root):
        if path.is_symlink() or not path.is_file():
            raise RuntimeError(f"not a regular family file: {path}")
        stat = path.stat()
        result[str(path.relative_to(root))] = dict(size=stat.st_size, sha256=hashlib.sha256(path.read_bytes()).hexdigest(), mode=stat.st_mode, device=stat.st_dev, inode=stat.st_ino, mtime_ns=stat.st_mtime_ns, ctime_ns=stat.st_ctime_ns)
    return result
before = inventory(source)
(work / "source-before.json").write_text(json.dumps(before, indent=2) + "\n")
for path in files(source):
    destination = work / ".beads" / path.relative_to(source)
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(path, destination)
after = inventory(source)
copied = inventory(work / ".beads")
(work / "source-after.json").write_text(json.dumps(after, indent=2) + "\n")
(work / "copied-family.json").write_text(json.dumps(copied, indent=2) + "\n")
assert before == after, "source changed during copy; retained copy is not coherent evidence"
assert before.keys() == copied.keys(), "copy lost a family artifact"
for name, record in before.items():
    assert all(record[key] == copied[name][key] for key in ("size", "sha256", "mode")), name
print("[stress] source_kind=" + ("retained_database_family" if "beads.db" in before else "JSONL_rebuild_only"))
PY
if [[ -n "${SOURCE_LOCK_FD:-}" ]]; then
    flock -u "$SOURCE_LOCK_FD" || exit 2
    exec {SOURCE_LOCK_FD}<&-
fi
export BEADS_DIR="$WORK/.beads"
export RUST_LOG="${RUST_LOG:-error}"

"$BR" --version > binary-version.txt 2>binary-version.err || exit 2
sha256sum "$BR" > binary-sha256.txt || exit 2
"$BR" capabilities --format json >binary-capabilities.json 2>binary-capabilities.err || exit 2
echo "[stress] workspace=$WORK br=$(cat binary-version.txt) family=$SRC"

family_inventory() {
    python3 - <<'PY'
import hashlib, json, pathlib, sys
records = {}
for path in sorted(pathlib.Path('.beads').rglob('*')):
    if path.is_file():
        records[str(path)] = dict(size=path.stat().st_size, sha256=hashlib.sha256(path.read_bytes()).hexdigest())
json.dump(records, sys.stdout, indent=2)
PY
}

integrity() {
    # Use a fresh preserved probe each time, so a prior probe WAL can never
    # contaminate a later check and no probe artifact needs to be unlinked.
    local probe=$1
    mkdir "$probe" || return 2
    cp .beads/beads.db "$probe/beads.db" || return 2
    if [[ -f .beads/beads.db-wal ]]; then
        cp .beads/beads.db-wal "$probe/beads.db-wal" || return 2
    fi
    python3 - "$probe/beads.db" <<'PY'
import sqlite3
import sys
with sqlite3.connect(sys.argv[1]) as connection:
    for row in connection.execute("PRAGMA integrity_check"):
        print(row[0])
PY
}

db_rows() {
    python3 - "$1/beads.db" <<'PY'
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as connection:
    print(connection.execute("SELECT count(*) FROM issues").fetchone()[0])
PY
}

# Warm the family (rebuilds the DB from JSONL when the copy has none).
"$BR" ready --json >pre-ready.json 2>pre-ready.err || { cat pre-ready.err >&2; exit 1; }
integrity "$WORK/probe-pre" >pre-integrity.txt 2>pre-integrity.err || { cat pre-integrity.err >&2; exit 1; }
echo "[stress] pre-run integrity: $(cat pre-integrity.txt)"
[[ "$(cat pre-integrity.txt)" == "ok" ]] || exit 1

"$BR" list --status all --limit 0 --json >pre-list.json 2>pre-list.err || { cat pre-list.err >&2; exit 1; }
python3 -c 'import json,sys
d=json.load(sys.stdin)
issues=d.get("issues",d) if isinstance(d,dict) else d
print("\n".join(i["id"] for i in issues if isinstance(i,dict) and i.get("status") not in ("tombstone",)))' <pre-list.json >ids.txt || exit 1
NIDS="$(wc -l < ids.txt | tr -d ' ')"
echo "[stress] $NIDS listable issues"
if [[ "$NIDS" -eq 0 ]]; then
    echo "[stress] FAIL: no issues listable from the family copy"
    exit 1
fi
# Artifacts written while warming the copy (for example a rebuild's own
# pre-compaction backup) are not stress findings; only new ones count.
recovery_files() { if [[ -d .beads/.br_recovery ]]; then find .beads/.br_recovery -type f | sort; fi; }
BASELINE_REC="$(recovery_files)"
echo "[stress] pre-run recovery artifacts: $(printf '%s' "$BASELINE_REC" | grep -c . || true)"
family_inventory >pre-family.json || exit 1

worker() {
    local n=$1 end=$((START + SECS)) ok=0 fail=0 rc id seq=0 invoked returned
    local -a args
    while [[ "$(date +%s)" -lt "$end" ]]; do
        seq=$((seq + 1))
        id="$(sed -n "$(( (RANDOM % NIDS) + 1 ))p" ids.txt)"
        case $((RANDOM % 7)) in
            0) args=(update "$id" --priority $((RANDOM % 4))) ;;
            1) args=(comments add "$id" "worker $n at $(date +%s) $(head -c 3000 /dev/zero | tr '\0' 'x')") ;;
            2) args=(update "$id" --claim --actor "w$n") ;;
            3) args=(list --status open --limit 20) ;;
            4) args=(create --title "w$n new $(date +%s)$RANDOM" --priority 3 --description "$(head -c 4500 /dev/zero | tr '\0' 'd')") ;;
            5) args=(update "$id" --notes "note from w$n $(date +%s)") ;;
            6) args=(ready) ;;
        esac
        printf '%q ' "$BR" "${args[@]}" --json >"commands/w$n-$seq.argv"
        printf '\n' >>"commands/w$n-$seq.argv"
        invoked="$(date +%s%N)"
        timeout --signal=TERM --kill-after=5 "$COMMAND_TIMEOUT" "$BR" "${args[@]}" --json \
            >"commands/w$n-$seq.stdout" 2>"commands/w$n-$seq.stderr"
        rc=$?
        returned="$(date +%s%N)"
        printf '%s\t%s\t%s\t%s\t%s\n' "$seq" "$invoked" "$returned" "$rc" "$id" >>"w$n.history.tsv"
        cat "commands/w$n-$seq.stderr" >>"w$n.err"
        if [[ "$rc" -eq 0 ]]; then ok=$((ok + 1)); else fail=$((fail + 1)); fi
    done
    echo "$ok $fail" > "w$n.count"
}

START="$(date +%s)"
for n in $(seq 1 "$WORKERS"); do worker "$n" & done
wait

OK=0; FAIL=0
for n in $(seq 1 "$WORKERS"); do
    read -r o f < "w$n.count" || exit 1
    [[ "$((o + f))" -gt 0 ]] || { echo "[stress] FAIL: worker $n did not run" >&2; exit 1; }
    OK=$((OK + o)); FAIL=$((FAIL + f))
done
echo "[stress] $WORKERS workers x ${SECS}s: acknowledged=$OK nonzero_or_unknown=$FAIL"
[[ "$OK" -gt 0 && "$((OK + FAIL))" -ge "$WORKERS" ]] || { echo "[stress] FAIL: workload did not run" >&2; exit 1; }
echo "[stress] distinct error lines (claim conflicts / lock waits are expected):"
cat w*.err | sed -E 's/[0-9]{5,}/N/g' | sort | uniq -c | sort -rn | head -8

# JSON mode carries errors on stdout; examine BOTH streams, retaining each
# exact invocation and response. Nonzero is not proof of rollback.
python3 - <<'PY' >command-outcomes.json || exit 1
import json, pathlib, re
records = []
for history in sorted(pathlib.Path('.').glob('w*.history.tsv')):
    worker = history.name.split('.')[0]
    for line in history.read_text().splitlines():
        sequence, invoked, returned, code, issue = line.split('\t')
        stem = pathlib.Path('commands') / f'{worker}-{sequence}'
        text = stem.with_suffix('.stdout').read_text()
        code = int(code)
        try:
            payload = json.loads(text)
        except json.JSONDecodeError:
            if code == 0:
                raise RuntimeError(f'acknowledged command emitted invalid JSON: {stem}')
            payload = None
        records.append(dict(worker=worker, sequence=int(sequence), invoke_ns=int(invoked), return_ns=int(returned), exit_code=code, outcome='acknowledged' if code == 0 else 'nonzero_commit_unknown', selected_issue=issue, command=stem.with_suffix('.argv').read_text(), stdout=text, stderr=stem.with_suffix('.stderr').read_text(), payload=payload))
json.dump(records, __import__('sys').stdout, indent=2)
# Successful list/show payloads can legitimately contain historical issue
# descriptions about corruption; only diagnostics and failed outputs are errors.
pattern = re.compile(r'malformed|corrupt|snapshot conflict|unable to open database|not found after insert|more than one row|export failed', re.I)
unexpected = sum(bool(pattern.search(record['stderr'] + (record['stdout'] if record['exit_code'] else ''))) for record in records)
pathlib.Path('unexpected-errors.count').write_text(str(unexpected) + '\n')
PY
UNEXPECTED="$(cat unexpected-errors.count)"

# Publish any pending successful writes before reconciling the database and
# JSONL. This is a new observed command, never a blanket retry of a mutation.
"$BR" sync --flush-only --json >post-flush.json 2>post-flush.err
FLUSH_RC=$?
printf '%s\n' "$FLUSH_RC" >post-flush.exit
integrity "$WORK/probe-post" >post-integrity.txt 2>post-integrity.err
INTEGRITY_RC=$?
IC="$(cat post-integrity.txt)"
DB="$(db_rows "$WORK/probe-post" 2>post-count.err)"

JL="$(wc -l < .beads/issues.jsonl | tr -d ' ')"
BADJSON="$(python3 -c 'import json,sys
bad=0
for line in open(".beads/issues.jsonl"):
    line=line.strip()
    if not line: continue
    try: json.loads(line)
    except Exception: bad+=1
print(bad)' 2>post-jsonl.err || echo "unavailable")"
"$BR" doctor --json >post-doctor.json 2>post-doctor.err
DOCTOR_RC=$?
printf '%s\n' "$DOCTOR_RC" >post-doctor.exit
DOCTOR_ERR="$(python3 -c 'import json,sys
d=json.load(sys.stdin)
checks=d.get("checks") or d.get("results") or []
print(sum(1 for c in checks if isinstance(c,dict) and str(c.get("status","")).lower()=="error"))' <post-doctor.json 2>post-doctor-parse.err || echo "unavailable")"
NEW_REC="$(comm -13 <(printf '%s\n' "$BASELINE_REC" | grep .) <(recovery_files))"
REC="$(printf '%s' "$NEW_REC" | grep -c . || true)"
if [[ "$REC" -gt 0 ]]; then
    echo "[stress] new recovery artifacts:"
    printf '%s\n' "$NEW_REC"
fi

echo "[stress] integrity=$IC db_rows=$DB jsonl_records=$JL bad_jsonl_lines=$BADJSON doctor_errors=$DOCTOR_ERR recovery_artifacts=$REC unexpected_error_lines=$UNEXPECTED"

family_inventory >post-family.json || exit 1

# Doctor exit 1 also represents warnings, including preserved recovery
# artifacts deliberately copied above. Keep the original gate: no ERROR
# findings and no new recovery files; reject other command failure exits.
if [[ "$FLUSH_RC" -eq 0 && "$INTEGRITY_RC" -eq 0 && ( "$DOCTOR_RC" -eq 0 || "$DOCTOR_RC" -eq 1 ) && "$IC" == "ok" && "$DB" == "$JL" && "$BADJSON" == "0" && "$DOCTOR_ERR" == "0" && "$REC" -eq 0 && "$UNEXPECTED" -eq 0 ]]; then
    echo "[stress] PASS ($WORK)"
    exit 0
fi
echo "[stress] FAIL ($WORK)"
exit 1
