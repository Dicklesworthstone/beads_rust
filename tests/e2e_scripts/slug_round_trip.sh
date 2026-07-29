#!/usr/bin/env bash
# tests/e2e_scripts/slug_round_trip.sh
#
# beads_rust-l6xl: full lifecycle round-trip for the `--slug` feature
# (commit 5c0af3d4 / PR #283).
#
# Walks: create with slug → show → update → close → orphans (with a
# git-commit reference) → verify references resolve. Logs every step
# with named expectations.
#
# Exit codes:
#   0   PASS: full lifecycle works; orphans finds slugged ID in commits
#   1   FAIL: some named expectation didn't hold
#   2   prerequisite missing (br binary, git)

set -euo pipefail

LOG_TS=$(date -u +%Y%m%dT%H%M%SZ)
SUMMARY_LOG="/tmp/slug_round_trip_${LOG_TS}.summary.txt"
log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$SUMMARY_LOG"; }
fail() { log "FAIL: $*"; exit 1; }

log "=== slug_round_trip.sh START ts=${LOG_TS} ==="

# Locate br
if [[ -n "${BR_BIN:-}" ]]; then
    BR="$BR_BIN"
elif command -v br >/dev/null 2>&1; then
    BR=$(command -v br)
elif [[ -n "${CARGO_TARGET_DIR:-}" && -x "$CARGO_TARGET_DIR/release/br" ]]; then
    BR="$CARGO_TARGET_DIR/release/br"
else
    log "ERROR: br binary not found (set BR_BIN= or build with cargo build --release)"
    exit 2
fi
command -v git >/dev/null 2>&1 || { log "ERROR: git not in PATH"; exit 2; }
command -v jq >/dev/null 2>&1 || { log "ERROR: jq not in PATH"; exit 2; }

WORK=$(mktemp -d)
cd "$WORK"
log "  workspace: $WORK"
log "  br: $BR"

# Phase 1: br init with custom prefix
log "Phase 1: br init --prefix myproj"
"$BR" init --prefix myproj >/dev/null 2>&1 || fail "br init"

# Phase 2: br create --slug
log "Phase 2: br create --slug 'Fix Login Flow' (mixed case + spaces)"
CREATE_OUT=$("$BR" create "Fix login regression" --slug "Fix Login Flow" -t bug -p 1 2>&1)
echo "$CREATE_OUT" | tee -a "$SUMMARY_LOG"
SLUG_ID=$(echo "$CREATE_OUT" | grep -oE 'myproj-[a-z0-9-]+' | head -1)
[[ -n "$SLUG_ID" ]] || fail "could not extract slugged ID from create output"
log "  slugged ID: $SLUG_ID"

# Expectation: ID should normalize "Fix Login Flow" → "fix-login-flow"
echo "$SLUG_ID" | grep -q '^myproj-fix-login-flow-' || \
    fail "slug normalization wrong: expected 'myproj-fix-login-flow-*', got $SLUG_ID"
log "  [OK] slug normalized: $SLUG_ID starts with 'myproj-fix-login-flow-'"

# Phase 3: br show --json round-trip
log "Phase 3: br show $SLUG_ID --json"
SHOW_OUT=$("$BR" show "$SLUG_ID" --json 2>&1) || fail "br show failed"
RETURNED_ID=$(echo "$SHOW_OUT" | jq -r '.[0].id // .id // empty')
[[ "$RETURNED_ID" == "$SLUG_ID" ]] || \
    fail "show returned different ID: expected $SLUG_ID, got $RETURNED_ID"
log "  [OK] show round-trip preserves slugged ID"

# Phase 4: br update
log "Phase 4: br update $SLUG_ID --priority 0"
"$BR" update "$SLUG_ID" --priority 0 >/dev/null 2>&1 || fail "br update failed"
NEW_PRIO=$("$BR" show "$SLUG_ID" --json | jq -r '.[0].priority // .priority')
[[ "$NEW_PRIO" == "0" ]] || fail "update priority did not stick: $NEW_PRIO"
log "  [OK] update --priority 0 sticks"

# Phase 5: orphans command finds slugged ID in commit message
log "Phase 5: git init + commit referencing the slugged ID"
git init -q
git config user.email "test@example.com"
git config user.name "Test User"
echo "initial" > README.md
git add . >/dev/null
git commit -m "initial" -q
echo "fix" >> README.md
git add . >/dev/null
git commit -m "feat: implement $SLUG_ID login fix" -q

log "Phase 5b: br orphans --json"
ORPHANS_OUT=$("$BR" orphans --json 2>&1) || fail "br orphans failed"
echo "$ORPHANS_OUT" | grep -q "$SLUG_ID" || \
    fail "orphans output does not contain slugged ID $SLUG_ID; got: $ORPHANS_OUT"
log "  [OK] orphans finds slugged ID in commit message"

# Phase 6: br close
log "Phase 6: br close $SLUG_ID --reason 'PASS lifecycle test'"
"$BR" close "$SLUG_ID" --reason "PASS lifecycle test" >/dev/null 2>&1 || fail "br close failed"
STATUS=$("$BR" show "$SLUG_ID" --json | jq -r '.[0].status // .status')
[[ "$STATUS" == "closed" ]] || fail "close did not stick: $STATUS"
log "  [OK] close transitions status to closed"

# Phase 7: orphans should NOT include closed slugged ID anymore
log "Phase 7: br orphans --json (should not include closed bead)"
ORPHANS_OUT2=$("$BR" orphans --json 2>&1)
if echo "$ORPHANS_OUT2" | jq -e --arg id "$SLUG_ID" '.[] | select(.id == $id and .status == "open")' >/dev/null 2>&1; then
    fail "orphans still flags closed slugged ID $SLUG_ID as open"
fi
log "  [OK] closed slugged ID no longer in orphans (open) list"

cd /
rm -rf "$WORK"
log "=== slug_round_trip.sh PASS ==="
log "  summary: $SUMMARY_LOG"
exit 0
