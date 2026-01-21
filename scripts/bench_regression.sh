#!/bin/bash
set -e

# Benchmark Regression Runner
# Runs benchmarks against a baseline and checks for regressions.

BASELINE_NAME="baseline"
THRESHOLD=${BENCH_REGRESSION_THRESHOLD:-0.10}

echo "Running benchmarks with baseline comparison..."

# Check if baseline exists
if [ ! -d "target/criterion" ]; then
    echo "No existing baseline found. Saving current run as baseline."
    cargo bench --bench benchmarks -- --save-baseline "$BASELINE_NAME" "$@"
else
    echo "Comparing against existing baseline '$BASELINE_NAME'..."
    cargo bench --bench benchmarks -- --baseline "$BASELINE_NAME" "$@"
fi

# Run regression check
python3 scripts/check_regression.py target

echo "Benchmark run complete."
