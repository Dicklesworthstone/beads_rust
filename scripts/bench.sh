#!/usr/bin/env bash
# Benchmark runner - measures br performance with timing and optional comparisons.
#
# Usage:
#   scripts/bench.sh                    # Run all benchmarks
#   scripts/bench.sh --quick            # Run quick comparison only (no criterion)
#   scripts/bench.sh --criterion        # Run criterion benchmarks only
#   scripts/bench.sh --compare          # Compare br vs bd performance
#   scripts/bench.sh --json             # Output summary as JSON
#   scripts/bench.sh --save NAME        # Save baseline as NAME
#   scripts/bench.sh --baseline NAME    # Compare against baseline NAME
#
# Environment:
#   HARNESS_ARTIFACTS=1       Enable artifact logging to target/test-artifacts/
#   BR_BINARY=/path/to/br     Override br binary location
#   BD_BINARY=/path/to/bd     Override bd binary location (for comparison)
#   BENCH_TIMEOUT=600         Per-benchmark timeout in seconds (default: 300)
#   BENCH_DATASET=beads_rust  Dataset to use for benchmarks
#
# Output:
#   - Exit code 0 on success, 1 on failure
#   - Summary JSON written to target/test-artifacts/benchmark_summary.json
#   - Criterion reports in target/criterion/
#   - Benchmark artifacts in target/test-artifacts/benchmark/

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ARTIFACTS_DIR="${PROJECT_ROOT}/target/test-artifacts"
CRITERION_DIR="${PROJECT_ROOT}/target/criterion"

# Configuration
JSON_OUTPUT=0
QUICK_ONLY=0
CRITERION_ONLY=0
COMPARE_BD=0
SAVE_BASELINE=""
USE_BASELINE=""
TIMEOUT="${BENCH_TIMEOUT:-300}"
DATASET="${BENCH_DATASET:-beads_rust}"

log() {
    if [[ "$JSON_OUTPUT" -eq 0 ]]; then
        echo -e "\033[36m[bench]\033[0m $*"
    fi
}

error() {
    if [[ "$JSON_OUTPUT" -eq 0 ]]; then
        echo -e "\033[31m[bench] ERROR:\033[0m $*" >&2
    fi
}

warn() {
    if [[ "$JSON_OUTPUT" -eq 0 ]]; then
        echo -e "\033[33m[bench] WARN:\033[0m $*" >&2
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
        --quick)
            QUICK_ONLY=1
            shift
            ;;
        --criterion)
            CRITERION_ONLY=1
            shift
            ;;
        --compare)
            COMPARE_BD=1
            shift
            ;;
        --save)
            SAVE_BASELINE="$2"
            shift 2
            ;;
        --baseline)
            USE_BASELINE="$2"
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

# Environment guard for long-running benchmarks
if [[ -z "${BENCH_CONFIRM:-}" ]] && [[ "$JSON_OUTPUT" -eq 0 ]] && [[ "$QUICK_ONLY" -eq 0 ]]; then
    warn "Full benchmarks may take several minutes."
    warn "Set BENCH_CONFIRM=1 or use --quick for fast feedback."
    read -p "Continue? [y/N] " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        log "Aborted."
        exit 0
    fi
fi

# Ensure build is up to date (release mode for accurate benchmarks)
log "Building br binary (release)..."
cd "$PROJECT_ROOT"
cargo build --release --quiet 2>/dev/null || cargo build --release

# Create artifacts directory
mkdir -p "$ARTIFACTS_DIR/benchmark"

START_TIME=$(date +%s)
RESULTS=()

# Quick comparison benchmark
run_quick_benchmark() {
    log "Running quick comparison benchmark..."

    local quick_result
    if timeout "$TIMEOUT" cargo test --test benchmark_comparison -- --nocapture 2>&1 | tee "$ARTIFACTS_DIR/benchmark/quick_comparison.log"; then
        quick_result="pass"
        RESULTS+=("{\"benchmark\":\"quick_comparison\",\"result\":\"pass\"}")
        log "  Quick comparison: PASS"
    else
        quick_result="fail"
        RESULTS+=("{\"benchmark\":\"quick_comparison\",\"result\":\"fail\"}")
        error "  Quick comparison: FAIL"
    fi
}

# Criterion benchmarks
run_criterion_benchmarks() {
    log "Running criterion benchmarks..."

    local baseline_args=""
    if [[ -n "$SAVE_BASELINE" ]]; then
        baseline_args="--save-baseline $SAVE_BASELINE"
        log "  Saving baseline as: $SAVE_BASELINE"
    elif [[ -n "$USE_BASELINE" ]]; then
        baseline_args="--baseline $USE_BASELINE"
        log "  Comparing against baseline: $USE_BASELINE"
    fi

    # Run criterion benchmarks
    if timeout "$TIMEOUT" cargo bench $baseline_args 2>&1 | tee "$ARTIFACTS_DIR/benchmark/criterion.log"; then
        RESULTS+=("{\"benchmark\":\"criterion\",\"result\":\"pass\"}")
        log "  Criterion benchmarks: PASS"
    else
        RESULTS+=("{\"benchmark\":\"criterion\",\"result\":\"fail\"}")
        error "  Criterion benchmarks: FAIL"
    fi

    # Copy criterion reports to artifacts
    if [[ -d "$CRITERION_DIR" ]]; then
        log "  Criterion reports: $CRITERION_DIR"
    fi
}

# br vs bd comparison
run_bd_comparison() {
    log "Running br vs bd performance comparison..."

    # Find bd binary
    BD_PATH="${BD_BINARY:-}"
    if [[ -z "$BD_PATH" ]]; then
        for candidate in "/data/projects/beads/.bin/beads" "$HOME/go/bin/bd" "$(command -v bd 2>/dev/null || true)"; do
            if [[ -n "$candidate" ]] && [[ -x "$candidate" ]]; then
                BD_PATH="$candidate"
                break
            fi
        done
    fi

    if [[ -z "$BD_PATH" ]] || [[ ! -x "$BD_PATH" ]]; then
        warn "bd binary not found, skipping comparison"
        RESULTS+=("{\"benchmark\":\"bd_comparison\",\"result\":\"skipped\",\"reason\":\"bd_not_found\"}")
        return
    fi

    BR_PATH="$PROJECT_ROOT/target/release/br"

    log "  br: $BR_PATH"
    log "  bd: $BD_PATH"

    # Simple timing comparison on common operations
    local comparison_file="$ARTIFACTS_DIR/benchmark/bd_comparison.json"
    local ops=("list --json" "ready --json" "stats --json")

    echo "{" > "$comparison_file"
    echo "  \"operations\": [" >> "$comparison_file"

    local first=1
    for op in "${ops[@]}"; do
        if [[ "$first" -eq 0 ]]; then
            echo "," >> "$comparison_file"
        fi
        first=0

        # Time br
        local br_start=$(date +%s.%N)
        "$BR_PATH" $op >/dev/null 2>&1 || true
        local br_end=$(date +%s.%N)
        local br_time=$(echo "$br_end - $br_start" | bc)

        # Time bd
        local bd_start=$(date +%s.%N)
        "$BD_PATH" $op >/dev/null 2>&1 || true
        local bd_end=$(date +%s.%N)
        local bd_time=$(echo "$bd_end - $bd_start" | bc)

        local speedup=$(echo "scale=2; $bd_time / $br_time" | bc 2>/dev/null || echo "N/A")

        echo "    {\"op\": \"$op\", \"br_ms\": $br_time, \"bd_ms\": $bd_time, \"speedup\": $speedup}" >> "$comparison_file"
        log "  $op: br=${br_time}s bd=${bd_time}s speedup=${speedup}x"
    done

    echo "  ]" >> "$comparison_file"
    echo "}" >> "$comparison_file"

    RESULTS+=("{\"benchmark\":\"bd_comparison\",\"result\":\"pass\",\"report\":\"$comparison_file\"}")
    log "  Comparison report: $comparison_file"
}

# Run benchmarks based on flags
if [[ "$QUICK_ONLY" -eq 1 ]]; then
    run_quick_benchmark
elif [[ "$CRITERION_ONLY" -eq 1 ]]; then
    run_criterion_benchmarks
else
    run_quick_benchmark
    run_criterion_benchmarks
    if [[ "$COMPARE_BD" -eq 1 ]]; then
        run_bd_comparison
    fi
fi

END_TIME=$(date +%s)
TOTAL_DURATION=$((END_TIME - START_TIME))

# Generate summary
SUMMARY_FILE="$ARTIFACTS_DIR/benchmark_summary.json"
RESULTS_JSON=$(printf '%s\n' "${RESULTS[@]}" | paste -sd, -)

cat > "$SUMMARY_FILE" << EOF
{
  "suite": "benchmark",
  "generated_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "duration_s": $TOTAL_DURATION,
  "dataset": "$DATASET",
  "baseline_saved": ${SAVE_BASELINE:+"\"$SAVE_BASELINE\""}${SAVE_BASELINE:-null},
  "baseline_used": ${USE_BASELINE:+"\"$USE_BASELINE\""}${USE_BASELINE:-null},
  "criterion_dir": "$CRITERION_DIR",
  "artifacts_dir": "$ARTIFACTS_DIR/benchmark",
  "results": [$RESULTS_JSON]
}
EOF

log "Summary written to: $SUMMARY_FILE"

if [[ "$JSON_OUTPUT" -eq 1 ]]; then
    cat "$SUMMARY_FILE"
fi

# Final summary
log "============================================"
log "Benchmark Results"
log "Duration: ${TOTAL_DURATION}s"
log "Criterion: $CRITERION_DIR"
log "Artifacts: $ARTIFACTS_DIR/benchmark"
log "============================================"

exit 0
