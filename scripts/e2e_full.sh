#!/usr/bin/env bash
# Full E2E test runner - runs all E2E tests with comprehensive logging.
#
# Usage:
#   scripts/e2e_full.sh                  # Run all E2E tests
#   scripts/e2e_full.sh --json           # Output summary as JSON
#   scripts/e2e_full.sh --verbose        # Show test output
#   scripts/e2e_full.sh --filter PATTERN # Run only tests matching PATTERN
#   scripts/e2e_full.sh --parallel       # Run tests in parallel (faster, less isolation)
#   scripts/e2e_full.sh --dataset beads_rust  # Use specific dataset
#
# Environment:
#   HARNESS_ARTIFACTS=1       Enable artifact logging to target/test-artifacts/
#   HARNESS_PRESERVE_SUCCESS=1  Keep artifacts even on success
#   BR_BINARY=/path/to/br     Override br binary location
#   E2E_TIMEOUT=300           Per-test timeout in seconds (default: 120)
#   E2E_PARALLEL=1            Enable parallel execution
#   E2E_DATASET=beads_rust    Dataset to use for tests
#
# Output:
#   - Exit code 0 on success, 1 on failure
#   - Summary JSON written to target/test-artifacts/e2e_full_summary.json
#   - Artifacts in target/test-artifacts/<suite>/<test>/
#
# WARNING: Full suite may take several minutes. Use scripts/e2e.sh for quick feedback.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ARTIFACTS_DIR="${PROJECT_ROOT}/target/test-artifacts"

# Configuration
JSON_OUTPUT=0
VERBOSE=0
FILTER=""
PARALLEL=0
DATASET="${E2E_DATASET:-}"
TIMEOUT="${E2E_TIMEOUT:-120}"

log() {
    if [[ "$JSON_OUTPUT" -eq 0 ]]; then
        echo -e "\033[32m[e2e_full]\033[0m $*"
    fi
}

error() {
    if [[ "$JSON_OUTPUT" -eq 0 ]]; then
        echo -e "\033[31m[e2e_full] ERROR:\033[0m $*" >&2
    fi
}

warn() {
    if [[ "$JSON_OUTPUT" -eq 0 ]]; then
        echo -e "\033[33m[e2e_full] WARN:\033[0m $*" >&2
    fi
}

usage() {
    head -n 25 "$0" | tail -n +2 | sed 's/^# //' | sed 's/^#//'
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
        --parallel)
            PARALLEL=1
            shift
            ;;
        --dataset)
            DATASET="$2"
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

# Environment guard for long-running tests
if [[ -z "${E2E_FULL_CONFIRM:-}" ]] && [[ "$JSON_OUTPUT" -eq 0 ]]; then
    warn "Full E2E suite runs all tests and may take several minutes."
    warn "Set E2E_FULL_CONFIRM=1 or use scripts/e2e.sh for quick feedback."
    read -p "Continue? [y/N] " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        log "Aborted."
        exit 0
    fi
fi

# Ensure build is up to date
log "Building br binary..."
cd "$PROJECT_ROOT"
cargo build --release --quiet 2>/dev/null || cargo build --release

# Discover all E2E tests
log "Discovering E2E tests..."
ALL_TESTS=()
for test_file in tests/e2e_*.rs; do
    test_name=$(basename "$test_file" .rs)
    # Apply filter if specified
    if [[ -n "$FILTER" ]] && [[ ! "$test_name" =~ $FILTER ]]; then
        continue
    fi
    ALL_TESTS+=("$test_name")
done

log "Found ${#ALL_TESTS[@]} E2E tests"

# Create artifacts directory
mkdir -p "$ARTIFACTS_DIR"

# Export dataset if specified
if [[ -n "$DATASET" ]]; then
    export E2E_DATASET="$DATASET"
    log "Using dataset: $DATASET"
fi

# Run tests
PASSED=0
FAILED=0
SKIPPED=0
START_TIME=$(date +%s)
RESULTS=()

if [[ "$PARALLEL" -eq 1 ]]; then
    log "Running tests in parallel..."
    CARGO_ARGS="--jobs $(nproc 2>/dev/null || echo 4)"
else
    log "Running tests serially..."
    CARGO_ARGS="--jobs 1"
fi

for test in "${ALL_TESTS[@]}"; do
    log "  Running: $test"

    TEST_START=$(date +%s.%N)

    NOCAPTURE_FLAG=""
    if [[ "$VERBOSE" -eq 1 ]]; then
        NOCAPTURE_FLAG="--nocapture"
    fi

    if timeout "$TIMEOUT" cargo test --test "$test" $CARGO_ARGS -- $NOCAPTURE_FLAG >/dev/null 2>&1; then
        RESULT="pass"
        ((PASSED++))
    else
        RESULT="fail"
        ((FAILED++))
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
SUMMARY_FILE="$ARTIFACTS_DIR/e2e_full_summary.json"
RESULTS_JSON=$(printf '%s\n' "${RESULTS[@]}" | paste -sd, -)

cat > "$SUMMARY_FILE" << EOF
{
  "suite": "e2e_full",
  "generated_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "passed": $PASSED,
  "failed": $FAILED,
  "skipped": $SKIPPED,
  "total": $((PASSED + FAILED)),
  "duration_s": $TOTAL_DURATION,
  "parallel": $([[ "$PARALLEL" -eq 1 ]] && echo "true" || echo "false"),
  "dataset": "${DATASET:-null}",
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
log "Full E2E Results: $PASSED passed, $FAILED failed, $SKIPPED skipped"
log "Duration: ${TOTAL_DURATION}s"
log "Artifacts: $ARTIFACTS_DIR"
log "============================================"

if [[ "$FAILED" -gt 0 ]]; then
    exit 1
fi

exit 0
