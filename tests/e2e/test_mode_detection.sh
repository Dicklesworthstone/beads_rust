#!/usr/bin/env bash
# E2E test for output mode detection
# Tests that output modes are correctly detected and applied

set -euo pipefail

# Setup logging
LOG_DIR="/tmp/br_e2e_logs"
mkdir -p "$LOG_DIR"
LOG_FILE="$LOG_DIR/mode_detection_$(date +%Y%m%d_%H%M%S).log"
exec > >(tee -a "$LOG_FILE") 2>&1

log() { echo "[$(date '+%H:%M:%S')] $*"; }
log_section() { echo ""; log "════════════════════════════════════════"; log "$*"; }

has_ansi() {
    [[ "$1" == *$'\e['* ]]
}

log_section "MODE DETECTION TEST"
log "Log file: $LOG_FILE"

# Setup test environment
TESTDIR=$(mktemp -d)
cd "$TESTDIR"
log "Test directory: $TESTDIR"
trap 'rm -rf "$TESTDIR"' EXIT

# Initialize workspace
br init --prefix md
br create "Test issue" --type task --priority 1
log "Created test workspace"

FAILED=0

# Test 1: NO_COLOR environment variable
log_section "TEST 1: NO_COLOR env var"
OUTPUT=$(NO_COLOR=1 br list 2>&1)
if has_ansi "$OUTPUT"; then
    log "✗ FAIL: Output contains ANSI codes despite NO_COLOR"
    FAILED=1
else
    log "✓ NO_COLOR disables ANSI codes"
fi

# Test 2: --no-color flag
log_section "TEST 2: --no-color flag"
OUTPUT=$(br list --no-color 2>&1)
if has_ansi "$OUTPUT"; then
    log "✗ FAIL: Output contains ANSI codes despite --no-color"
    FAILED=1
else
    log "✓ --no-color disables ANSI codes"
fi

# Test 3: Piped output (non-TTY)
log_section "TEST 3: Piped output detection"
OUTPUT=$(br list | cat)
if has_ansi "$OUTPUT"; then
    log "⚠ Warning: Piped output contains ANSI codes (may be intentional)"
else
    log "✓ Piped output has no ANSI codes"
fi
log "Piped output sample:"
echo "$OUTPUT" | head -3

# Test 4: --json flag forces JSON mode
log_section "TEST 4: --json flag"
OUTPUT=$(br list --json 2>&1)
if echo "$OUTPUT" | jq -e '.' > /dev/null 2>&1; then
    log "✓ --json flag produces valid JSON"
else
    log "✗ FAIL: --json flag does not produce valid JSON"
    FAILED=1
fi

# Test 5: --quiet flag
log_section "TEST 5: --quiet flag"
# Most commands should produce no/minimal output with --quiet
OUTPUT=$(br list --quiet 2>&1 || true)
if [ -z "$OUTPUT" ] || [ ${#OUTPUT} -lt 100 ]; then
    log "✓ --quiet flag produces minimal output (${#OUTPUT} chars)"
else
    log "⚠ Warning: --quiet output may be too verbose (${#OUTPUT} chars)"
fi

# Test 6: Multiple commands with --no-color
log_section "TEST 6: Multiple commands with --no-color"
for cmd in "list" "ready" "blocked" "stats"; do
    OUTPUT=$(br $cmd --no-color 2>&1 || true)
    if has_ansi "$OUTPUT"; then
        log "✗ FAIL: $cmd has ANSI codes with --no-color"
        FAILED=1
    else
        log "✓ $cmd respects --no-color"
    fi
done

# Test 7: Environment variable override
log_section "TEST 7: Environment variable combinations"
OUTPUT=$(NO_COLOR=1 br list --json 2>&1)
if echo "$OUTPUT" | jq -e '.' > /dev/null 2>&1; then
    log "✓ --json takes precedence over NO_COLOR"
else
    log "✗ FAIL: --json with NO_COLOR produces invalid output"
    FAILED=1
fi

log_section "MODE DETECTION TEST COMPLETE"

if [ $FAILED -eq 0 ]; then
    log "All tests passed"
    exit 0
else
    log "Some tests failed"
    exit 1
fi
