#!/usr/bin/env bash
# E2E test for rich output integration
# Logs all output to timestamped file for debugging

set -euo pipefail

# Setup logging
LOG_DIR="/tmp/br_e2e_logs"
mkdir -p "$LOG_DIR"
LOG_FILE="$LOG_DIR/rich_output_$(date +%Y%m%d_%H%M%S).log"
exec > >(tee -a "$LOG_FILE") 2>&1

log() { echo "[$(date '+%H:%M:%S')] $*"; }
log_section() { echo ""; log "════════════════════════════════════════"; log "$*"; }
assert_eq() { [[ "$1" == "$2" ]] || { log "FAIL: '$1' != '$2'"; exit 1; }; }
assert_contains() { [[ "$1" == *"$2"* ]] || { log "FAIL: '$1' does not contain '$2'"; exit 1; }; }
assert_no_ansi() {
    if [[ "$1" == *$'\e['* ]]; then
        log "FAIL: Output contains ANSI escape codes"
        exit 1
    fi
}

log_section "E2E TEST: RICH OUTPUT"
log "Log file: $LOG_FILE"
log "br version: $(br version --json 2>/dev/null | jq -r '.version' || br version)"

# Create test environment
TESTDIR=$(mktemp -d)
cd "$TESTDIR"
log "Test directory: $TESTDIR"
trap 'rm -rf "$TESTDIR"' EXIT

# Test 1: Init
log_section "TEST 1: Initialize workspace"
br init --prefix e2e 2>&1
if [[ -f .beads/beads.db ]]; then
    log "✓ Init successful - database created"
else
    log "✗ FAIL: Database not created"
    exit 1
fi

# Test 2: Create issues
log_section "TEST 2: Create test issues"
br create "High priority bug" --type bug --priority 0
br create "Medium task" --type task --priority 2
br create "Low feature" --type feature --priority 3
ISSUE_COUNT=$(br list --json | jq 'length')
assert_eq "$ISSUE_COUNT" "3"
log "✓ Created 3 issues"

# Test 3: List modes
log_section "TEST 3: List output modes"
log "--- JSON mode:"
br list --json | jq -c '.[] | {id, title}' | head -5
log "--- Plain mode (--no-color):"
PLAIN_OUTPUT=$(br list --no-color 2>&1)
assert_no_ansi "$PLAIN_OUTPUT"
echo "$PLAIN_OUTPUT" | head -5
log "✓ All list modes work"

# Test 4: Show command
log_section "TEST 4: Show command"
ID=$(br list --json | jq -r '.[0].id')
br show "$ID" --no-color
log "✓ Show command works for $ID"

# Test 5: JSON structure unchanged
log_section "TEST 5: JSON structure validation"
br list --json | jq -e '.[0] | has("id", "title", "status", "priority")' > /dev/null
log "✓ JSON structure valid - has required fields"

# Test 6: Ready command
log_section "TEST 6: Ready command"
br ready --no-color | head -5
br ready --json | jq -c 'length' > /dev/null
log "✓ Ready command works"

# Test 7: Blocked command
log_section "TEST 7: Blocked command"
# Create dependency to test blocked
ID1=$(br list --json | jq -r '.[0].id')
ID2=$(br list --json | jq -r '.[1].id')
br dep add "$ID2" "$ID1"
br blocked --no-color | head -5
br blocked --json | jq -c 'length' > /dev/null
log "✓ Blocked command works"

# Test 8: Stats command
log_section "TEST 8: Stats command"
br stats --no-color | head -10
br stats --json 2>/dev/null | jq -e '.summary.total_issues' > /dev/null
log "✓ Stats command works"

log_section "E2E TEST COMPLETE"
log "All tests passed"
log "Full log: $LOG_FILE"
