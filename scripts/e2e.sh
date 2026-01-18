#!/usr/bin/env bash
# Quick E2E test runner - runs a subset of tests for fast feedback.
#
# Usage:
#   scripts/e2e.sh                  # Run quick E2E tests
#   scripts/e2e.sh --json           # Output summary as JSON
#   scripts/e2e.sh --verbose        # Show test output
#   scripts/e2e.sh --filter PATTERN # Run only tests matching PATTERN
#   HARNESS_ARTIFACTS=1 scripts/e2e.sh  # Enable artifact logging
#
# Environment:
#   HARNESS_ARTIFACTS=1       Enable artifact logging to target/test-artifacts/
#   HARNESS_PRESERVE_SUCCESS=1  Keep artifacts even on success
#   BR_BINARY=/path/to/br     Override br binary location
#   E2E_TIMEOUT=300           Per-test timeout in seconds (default: 60)
#
# Output:
#   - Exit code 0 on success, 1 on failure
#   - Summary JSON written to target/test-artifacts/e2e_quick_summary.json
#   - Artifacts in target/test-artifacts/<suite>/<test>/

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ARTIFACTS_DIR="${PROJECT_ROOT}/target/test-artifacts"

# Quick subset of E2E tests (critical path, fast-running)
QUICK_TESTS=(
    e2e_basic_lifecycle
    e2e_ready
    e2e_create_output
    e2e_list_priority
    e2e_errors
    e2e_harness_demo
)

# Configuration
JSON_OUTPUT=0
VERBOSE=0
FILTER=""
TIMEOUT="${E2E_TIMEOUT:-60}"

log() {
    if [[ "$JSON_OUTPUT" -eq 0 ]]; then
        echo -e "\033[32m[e2e]\033[0m $*"
    fi
}

error() {
    if [[ "$JSON_OUTPUT" -eq 0 ]]; then
        echo -e "\033[31m[e2e] ERROR:\033[0m $*" >&2
    fi
}

warn() {
    if [[ "$JSON_OUTPUT" -eq 0 ]]; then
        echo -e "\033[33m[e2e] WARN:\033[0m $*" >&2
    fi
}

usage() {
    head -n 20 "$0" | tail -n +2 | sed 's/^# //' | sed 's/^#//'
    exit 0
}

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --json)
            JSON_OUTPUT=1
            shift
            ;;
        --verbose|-v)
            VERBOSE=1
            shift
            ;;
        --filter)
            FILTER="$2"
            shift 2
            ;;
        --help|-h)
            usage
            ;;
        *)
            error "Unknown argument: $1"
            usage
            ;;
    esac
done

# Ensure build is up to date
log "Building br binary..."
cd "$PROJECT_ROOT"
cargo build --release --quiet 2>/dev/null || cargo build --release

# Create artifacts directory
mkdir -p "$ARTIFACTS_DIR"

# Run tests
PASSED=0
FAILED=0
SKIPPED=0
START_TIME=$(date +%s)
RESULTS=()

log "Running quick E2E tests (${#QUICK_TESTS[@]} tests)..."
log "Artifacts: $ARTIFACTS_DIR"

for test in "${QUICK_TESTS[@]}"; do
    # Apply filter if specified
    if [[ -n "$FILTER" ]] && [[ ! "$test" =~ $FILTER ]]; then
        ((SKIPPED++))
        continue
    fi

    log "  Running: $test"

    TEST_START=$(date +%s.%N)

    if [[ "$VERBOSE" -eq 1 ]]; then
        if timeout "$TIMEOUT" cargo test --test "$test" -- --nocapture 2>&1; then
            RESULT="pass"
            ((PASSED++))
        else
            RESULT="fail"
            ((FAILED++))
        fi
    else
        if timeout "$TIMEOUT" cargo test --test "$test" -- --nocapture >/dev/null 2>&1; then
            RESULT="pass"
            ((PASSED++))
        else
            RESULT="fail"
            ((FAILED++))
        fi
    fi

    TEST_END=$(date +%s.%N)
    TEST_DURATION=$(echo "$TEST_END - $TEST_START" | bc 2>/dev/null || echo "0")

    RESULTS+=("{\"test\":\"$test\",\"result\":\"$RESULT\",\"duration_s\":$TEST_DURATION}")

    if [[ "$RESULT" == "pass" ]]; then
        log "    PASS (${TEST_DURATION}s)"
    else
        error "    FAIL (${TEST_DURATION}s)"
    fi
done

END_TIME=$(date +%s)
TOTAL_DURATION=$((END_TIME - START_TIME))

# Generate summary
SUMMARY_FILE="$ARTIFACTS_DIR/e2e_quick_summary.json"
RESULTS_JSON=$(printf '%s\n' "${RESULTS[@]}" | paste -sd, -)

cat > "$SUMMARY_FILE" << EOF
{
  "suite": "e2e_quick",
  "generated_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "passed": $PASSED,
  "failed": $FAILED,
  "skipped": $SKIPPED,
  "total": $((PASSED + FAILED)),
  "duration_s": $TOTAL_DURATION,
  "artifacts_dir": "$ARTIFACTS_DIR",
  "results": [$RESULTS_JSON]
}
EOF

log "Summary written to: $SUMMARY_FILE"

if [[ "$JSON_OUTPUT" -eq 1 ]]; then
    cat "$SUMMARY_FILE"
fi

# Final summary
log "============================================"
log "Quick E2E Results: $PASSED passed, $FAILED failed, $SKIPPED skipped"
log "Duration: ${TOTAL_DURATION}s"
log "Artifacts: $ARTIFACTS_DIR"
log "============================================"

if [[ "$FAILED" -gt 0 ]]; then
    exit 1
fi

exit 0
