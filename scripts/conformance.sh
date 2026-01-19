#!/usr/bin/env bash
# Conformance test runner - compares br and bd outputs for parity.
#
# Usage:
#   scripts/conformance.sh                  # Run conformance tests
#   scripts/conformance.sh --json           # Output summary as JSON
#   scripts/conformance.sh --verbose        # Show test output
#   scripts/conformance.sh --filter PATTERN # Run only tests matching PATTERN
#   scripts/conformance.sh --check-bd       # Verify bd is available first
#
# Environment:
#   HARNESS_ARTIFACTS=1       Enable artifact logging to target/test-artifacts/
#   HARNESS_PRESERVE_SUCCESS=1  Keep artifacts even on success
#   BR_BINARY=/path/to/br     Override br binary location
#   BD_BINARY=/path/to/bd     Override bd binary location (required)
#   CONFORMANCE_TIMEOUT=180   Per-test timeout in seconds (default: 120)
#   CONFORMANCE_STRICT=1      Fail on any normalization differences
#
# Requirements:
#   - Both br (Rust) and bd (Go) binaries must be available
#   - bd is typically found at /data/projects/beads/.bin/beads
#
# Output:
#   - Exit code 0 on success, 1 on failure, 2 on bd unavailable
#   - Summary JSON written to target/test-artifacts/conformance_summary.json
#   - Artifacts in target/test-artifacts/conformance/<test>/

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ARTIFACTS_DIR="${PROJECT_ROOT}/target/test-artifacts"

# Conformance tests
CONFORMANCE_TESTS=(
    conformance
    conformance_edge_cases
    conformance_labels_comments
    conformance_schema
)

# Configuration
JSON_OUTPUT=0
VERBOSE=0
FILTER=""
CHECK_BD=0
TIMEOUT="${CONFORMANCE_TIMEOUT:-120}"

# Binary paths
BR_BIN="${BR_BINARY:-}"
BD_BIN="${BD_BINARY:-}"

log() {
    if [[ "$JSON_OUTPUT" -eq 0 ]]; then
        echo -e "\033[35m[conformance]\033[0m $*"
    fi
}

error() {
    if [[ "$JSON_OUTPUT" -eq 0 ]]; then
        echo -e "\033[31m[conformance] ERROR:\033[0m $*" >&2
    fi
}

warn() {
    if [[ "$JSON_OUTPUT" -eq 0 ]]; then
        echo -e "\033[33m[conformance] WARN:\033[0m $*" >&2
    fi
}

usage() {
    head -n 25 "$0" | tail -n +2 | sed 's/^# //' | sed 's/^#//'
    exit 0
}

# Find bd binary
find_bd() {
    if [[ -n "$BD_BIN" ]] && [[ -x "$BD_BIN" ]]; then
        echo "$BD_BIN"
        return 0
    fi

    # Common locations for bd
    local candidates=(
        "/data/projects/beads/.bin/beads"
        "$HOME/go/bin/bd"
        "$HOME/.local/bin/bd"
        "$(command -v bd 2>/dev/null || true)"
    )

    for candidate in "${candidates[@]}"; do
        if [[ -n "$candidate" ]] && [[ -x "$candidate" ]]; then
            echo "$candidate"
            return 0
        fi
    done

    return 1
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
        --check-bd)
            CHECK_BD=1
            shift
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

# Check for bd availability
log "Checking for bd (Go beads) binary..."
if ! BD_PATH=$(find_bd); then
    error "bd binary not found!"
    error "Conformance tests require both br (Rust) and bd (Go) binaries."
    error "Set BD_BINARY=/path/to/bd or install bd."
    if [[ "$JSON_OUTPUT" -eq 1 ]]; then
        echo '{"error": "bd_not_found", "message": "bd binary required for conformance tests"}'
    fi
    exit 2
fi

log "Found bd at: $BD_PATH"
BD_VERSION=$("$BD_PATH" version 2>/dev/null | head -1 || echo "unknown")
log "bd version: $BD_VERSION"

if [[ "$CHECK_BD" -eq 1 ]]; then
    log "bd is available. Exiting (--check-bd mode)."
    exit 0
fi

# Ensure br build is up to date
log "Building br binary..."
cd "$PROJECT_ROOT"
cargo build --release --quiet 2>/dev/null || cargo build --release

BR_PATH="${BR_BINARY:-$(cargo metadata --format-version=1 2>/dev/null | jq -r '.target_directory')/release/br}"
if [[ ! -x "$BR_PATH" ]]; then
    BR_PATH="$PROJECT_ROOT/target/release/br"
fi
log "Using br at: $BR_PATH"
BR_VERSION=$("$BR_PATH" version 2>/dev/null | head -1 || echo "unknown")
log "br version: $BR_VERSION"

# Export binary paths for tests
export BR_BINARY="$BR_PATH"
export BD_BINARY="$BD_PATH"

# Create artifacts directory
mkdir -p "$ARTIFACTS_DIR/conformance"

# Run tests
PASSED=0
FAILED=0
SKIPPED=0
START_TIME=$(date +%s)
RESULTS=()

log "Running conformance tests (${#CONFORMANCE_TESTS[@]} test files)..."
log "Artifacts: $ARTIFACTS_DIR/conformance"

for test in "${CONFORMANCE_TESTS[@]}"; do
    # Apply filter if specified
    if [[ -n "$FILTER" ]] && [[ ! "$test" =~ $FILTER ]]; then
        ((SKIPPED++))
        continue
    fi

    log "  Running: $test"

    TEST_START=$(date +%s.%N)

    NOCAPTURE_FLAG=""
    if [[ "$VERBOSE" -eq 1 ]]; then
        NOCAPTURE_FLAG="--nocapture"
    fi

    if timeout "$TIMEOUT" cargo test --test "$test" -- $NOCAPTURE_FLAG 2>&1; then
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
SUMMARY_FILE="$ARTIFACTS_DIR/conformance_summary.json"
RESULTS_JSON=$(printf '%s\n' "${RESULTS[@]}" | paste -sd, -)

cat > "$SUMMARY_FILE" << EOF
{
  "suite": "conformance",
  "generated_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "passed": $PASSED,
  "failed": $FAILED,
  "skipped": $SKIPPED,
  "total": $((PASSED + FAILED)),
  "duration_s": $TOTAL_DURATION,
  "binaries": {
    "br": "$BR_PATH",
    "br_version": "$BR_VERSION",
    "bd": "$BD_PATH",
    "bd_version": "$BD_VERSION"
  },
  "artifacts_dir": "$ARTIFACTS_DIR/conformance",
  "results": [$RESULTS_JSON]
}
EOF

log "Summary written to: $SUMMARY_FILE"

if [[ "$JSON_OUTPUT" -eq 1 ]]; then
    cat "$SUMMARY_FILE"
fi

# Final summary
log "============================================"
log "Conformance Results: $PASSED passed, $FAILED failed, $SKIPPED skipped"
log "Duration: ${TOTAL_DURATION}s"
log "br: $BR_VERSION | bd: $BD_VERSION"
log "Artifacts: $ARTIFACTS_DIR/conformance"
log "============================================"

if [[ "$FAILED" -gt 0 ]]; then
    exit 1
fi

exit 0
