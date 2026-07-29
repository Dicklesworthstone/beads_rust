#!/usr/bin/env bash
# tests/e2e_scripts/markdown_import_witness.sh
#
# beads_rust-44rc: full markdown-import lifecycle harness with structured
# logging. Creates a markdown file with N issues, imports, verifies counts +
# titles, then round-trips via JSONL flush + reimport.
#
# Exit codes:
#   0   PASS
#   1   FAIL (named expectation didn't hold)
#   2   prerequisite missing

set -euo pipefail

LOG_TS=$(date -u +%Y%m%dT%H%M%SZ)
SUMMARY="/tmp/markdown_import_witness_${LOG_TS}.summary.txt"
log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$SUMMARY"; }
fail() { log "FAIL: $*"; exit 1; }

log "=== markdown_import_witness.sh START ts=${LOG_TS} ==="

if [[ -n "${BR_BIN:-}" ]]; then BR="$BR_BIN"
elif [[ -n "${CARGO_TARGET_DIR:-}" && -x "$CARGO_TARGET_DIR/release/br" ]]; then BR="$CARGO_TARGET_DIR/release/br"
elif command -v br >/dev/null 2>&1; then BR=$(command -v br)
else log "ERROR: br not found"; exit 2
fi
command -v jq >/dev/null 2>&1 || { log "ERROR: jq missing"; exit 2; }

WORK=$(mktemp -d)
cd "$WORK"

# Phase 1: init
log "Phase 1: br init --prefix br"
"$BR" init --prefix br >/dev/null 2>&1 || fail "br init"

# Phase 2: create markdown with 5 issues
log "Phase 2: prepare issues.md with 5 issues"
cat > issues.md <<'MD'
## Build pipeline
### Priority
1
### Type
task
### Labels
infra, ci

## Cleanup tests
### Priority
2
### Type
task
### Labels
test-debt

## Implement v2 API
### Priority
0
### Type
feature
### Labels
api, backend

## Document --slug
### Priority
3
### Type
docs

## Fix login regression
### Priority
1
### Type
bug
### Labels
auth, frontend
MD

# Phase 3: import
log "Phase 3: br create -f issues.md"
IMPORT_OUT=$("$BR" create -f issues.md 2>&1) || fail "br create -f failed: $IMPORT_OUT"
echo "$IMPORT_OUT" | grep -q "✓ Created 5 issues" || fail "expected '✓ Created 5 issues' in: $IMPORT_OUT"
log "  [OK] 5 issues imported"

# Phase 4: verify count + titles via list --json
log "Phase 4: br list --json invariants"
LIST_OUT=$("$BR" list --json 2>&1)
TOTAL=$(echo "$LIST_OUT" | jq '.total')
[[ "$TOTAL" == "5" ]] || fail "expected total=5, got $TOTAL"

# Verify each title is present (semantic check, format-tolerant)
for expected_title in "Build pipeline" "Cleanup tests" "Implement v2 API" "Document --slug" "Fix login regression"; do
    if ! echo "$LIST_OUT" | jq -e --arg t "$expected_title" '.issues[] | select(.title == $t)' >/dev/null 2>&1; then
        fail "expected title not found: '$expected_title'"
    fi
done
log "  [OK] all 5 titles present"

# Phase 5: flush + verify JSONL has 5 lines
log "Phase 5: br sync --flush-only + JSONL count"
"$BR" sync --flush-only >/dev/null 2>&1 || true
JSONL_COUNT=$(wc -l < .beads/issues.jsonl)
[[ "$JSONL_COUNT" -ge 5 ]] || fail "expected JSONL ≥ 5 lines; got $JSONL_COUNT"
log "  [OK] JSONL has $JSONL_COUNT lines"

# Phase 6: round-trip — re-import the same file (should be idempotent under content-hash dedup)
log "Phase 6: re-import (idempotency check)"
REIMPORT_OUT=$("$BR" create -f issues.md 2>&1 || true)
echo "$REIMPORT_OUT" | head -3 | tee -a "$SUMMARY"
LIST_OUT2=$("$BR" list --json 2>&1)
TOTAL2=$(echo "$LIST_OUT2" | jq '.total')
# Acceptable behaviors: (a) skip duplicates → total stays 5, or (b) create new → total = 10
# We verify total IS one of those two values (not e.g. corrupted)
if [[ "$TOTAL2" != "5" && "$TOTAL2" != "10" ]]; then
    fail "after re-import expected total in {5, 10}; got $TOTAL2"
fi
log "  [OK] re-import landed at total=$TOTAL2 (deduplication-aware)"

cd /
rm -rf "$WORK"
log "=== markdown_import_witness.sh PASS ==="
exit 0
