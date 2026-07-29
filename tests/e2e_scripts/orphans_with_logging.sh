#!/usr/bin/env bash
# tests/e2e_scripts/orphans_with_logging.sh
#
# beads_rust-750p: e2e harness for `br orphans` with detailed structured
# logging. Sets up a workspace with a JSONL newer than the DB, then runs
# `br orphans --json` with RUST_LOG=info and verifies the auto-import
# happened before the scan.
#
# Exit codes:
#   0   PASS
#   1   FAIL (named expectation didn't hold)
#   2   prerequisite missing

set -euo pipefail

LOG_TS=$(date -u +%Y%m%dT%H%M%SZ)
SUMMARY="/tmp/orphans_with_logging_${LOG_TS}.summary.txt"
log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$SUMMARY"; }
fail() { log "FAIL: $*"; exit 1; }

log "=== orphans_with_logging.sh START ts=${LOG_TS} ==="

if [[ -n "${BR_BIN:-}" ]]; then BR="$BR_BIN"
elif [[ -n "${CARGO_TARGET_DIR:-}" && -x "$CARGO_TARGET_DIR/release/br" ]]; then BR="$CARGO_TARGET_DIR/release/br"
elif command -v br >/dev/null 2>&1; then BR=$(command -v br)
else log "ERROR: br not found"; exit 2
fi
command -v jq >/dev/null 2>&1 || { log "ERROR: jq missing"; exit 2; }
command -v git >/dev/null 2>&1 || { log "ERROR: git missing"; exit 2; }

WORK=$(mktemp -d)
cd "$WORK"
log "  workspace: $WORK"

# Phase 1: git init + br init
log "Phase 1: git init + br init --prefix br"
git init -q
git config user.email "test@example.com"
git config user.name "Test User"
"$BR" init --prefix br >/dev/null 2>&1 || fail "br init"

# Phase 2: create + commit a reference
log "Phase 2: create issue + commit referencing it"
CREATE_OUT=$("$BR" create "Open issue referenced in commit" -t task 2>&1)
# Default prefix from `br init --prefix br` is `br-` so match that
ID=$(echo "$CREATE_OUT" | grep -oE 'br-[a-z0-9-]+' | head -1 || true)
[[ -n "$ID" ]] || fail "extract created ID from: $CREATE_OUT"
log "  created: $ID"

echo "init" > README.md
git add . >/dev/null
git commit -m "initial" -q
echo "commit" >> README.md
git add . >/dev/null
git commit -m "feat: implement $ID open work" -q

# Phase 3: br orphans (open issue should appear)
log "Phase 3: br orphans --json (open ID should appear)"
RUST_LOG=info ORPH=$("$BR" orphans --json 2>&1)
echo "$ORPH" | grep -q "$ID" || fail "open ID $ID not in orphans output: $ORPH"
log "  [OK] open ID $ID found in orphan list"

# Phase 4: rewrite JSONL to mark the issue closed (simulating a git pull)
log "Phase 4: edit JSONL to close the issue (simulate git pull)"
JSONL=".beads/issues.jsonl"
[[ -f "$JSONL" ]] || fail "JSONL missing: $JSONL"
TMPF=$(mktemp)
jq -c "if .id == \"$ID\" then .status = \"closed\" | .closed_at = \"2099-01-01T00:00:00Z\" | .updated_at = \"2099-01-01T00:00:00Z\" | .close_reason = \"Closed via JSONL edit\" else . end" "$JSONL" > "$TMPF"
mv "$TMPF" "$JSONL"
log "  [OK] JSONL rewritten with future timestamp + closed status"

# Phase 5: br orphans (should auto-import then see the issue is now closed → no orphans)
log "Phase 5: br orphans --json (auto-import should kick in; closed ID should NOT appear)"
RUST_LOG=info ORPH2=$("$BR" orphans --json 2>&1)
log "  output: $ORPH2"
COUNT=$(echo "$ORPH2" | jq 'length' 2>/dev/null || echo "parse-error")
[[ "$COUNT" == "0" ]] || fail "expected 0 orphans after JSONL close; got $COUNT (output: $ORPH2)"
log "  [OK] auto-import kicked in; orphan list is empty"

# Phase 6: verify the DB was actually updated
STATUS=$("$BR" show "$ID" --json | jq -r '.[0].status // .status')
[[ "$STATUS" == "closed" ]] || fail "DB not updated by auto-import; status=$STATUS"
log "  [OK] DB now reflects closed status (auto-import imported the JSONL change)"

cd /
rm -rf "$WORK"
log "=== orphans_with_logging.sh PASS ==="
exit 0
