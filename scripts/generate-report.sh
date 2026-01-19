#!/usr/bin/env bash
# Generate HTML/Markdown reports from test artifacts
#
# Usage:
#   ./scripts/generate-report.sh [options]
#
# Options:
#   --artifacts-dir DIR   Base directory for artifacts (default: target/test-artifacts)
#   --output-dir DIR      Output directory for reports (default: target/reports)
#   --failures-only       Only include failed tests in report
#   --help                Show this help message
#
# This script runs a Rust test that generates reports using the report_indexer module.
# The generated reports will be in:
#   - target/reports/report.html
#   - target/reports/report.md
#
# Example:
#   # First run tests with artifacts enabled
#   HARNESS_ARTIFACTS=1 cargo test e2e_sync
#
#   # Then generate reports
#   ./scripts/generate-report.sh
#
# Task: beads_rust-x7on

set -euo pipefail

ARTIFACTS_DIR="${ARTIFACTS_DIR:-target/test-artifacts}"
OUTPUT_DIR="${OUTPUT_DIR:-target/reports}"
FAILURES_ONLY="${FAILURES_ONLY:-0}"

usage() {
    grep '^#' "$0" | sed 's/^#//' | head -25
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --artifacts-dir)
            ARTIFACTS_DIR="$2"
            shift 2
            ;;
        --output-dir)
            OUTPUT_DIR="$2"
            shift 2
            ;;
        --failures-only)
            FAILURES_ONLY=1
            shift
            ;;
        --help|-h)
            usage
            ;;
        *)
            echo "Unknown option: $1"
            usage
            ;;
    esac
done

# Check if artifacts exist
if [[ ! -d "$ARTIFACTS_DIR" ]]; then
    echo "Error: Artifacts directory not found: $ARTIFACTS_DIR"
    echo ""
    echo "Run tests with HARNESS_ARTIFACTS=1 first to generate artifacts:"
    echo "  HARNESS_ARTIFACTS=1 cargo test e2e_sync"
    exit 1
fi

# Create output directory
mkdir -p "$OUTPUT_DIR"

# Run the report generation test with appropriate environment
echo "Generating reports from: $ARTIFACTS_DIR"
echo "Output directory: $OUTPUT_DIR"

REPORT_ARTIFACTS_DIR="$ARTIFACTS_DIR" \
REPORT_OUTPUT_DIR="$OUTPUT_DIR" \
REPORT_FAILURES_ONLY="$FAILURES_ONLY" \
cargo test --test e2e_report_generation -- --nocapture generate_and_save_report 2>&1 || {
    # If test doesn't exist yet, use inline Rust to generate
    echo "Fallback: Using inline report generation..."

    # Create a simple Rust script via cargo run
    cat > /tmp/generate_report.rs << 'EOF'
use std::path::PathBuf;

fn main() {
    let artifacts_dir = std::env::var("REPORT_ARTIFACTS_DIR")
        .unwrap_or_else(|_| "target/test-artifacts".to_string());
    let output_dir = std::env::var("REPORT_OUTPUT_DIR")
        .unwrap_or_else(|_| "target/reports".to_string());

    println!("Artifacts: {}", artifacts_dir);
    println!("Output: {}", output_dir);

    // Would use report_indexer here if this were compiled with the test
    println!("Note: Run 'cargo test e2e_report_generation' for full report generation");
}
EOF
    rustc /tmp/generate_report.rs -o /tmp/generate_report && /tmp/generate_report
}

# Show results
if [[ -f "$OUTPUT_DIR/report.html" ]]; then
    echo ""
    echo "Reports generated successfully!"
    echo "  HTML: $OUTPUT_DIR/report.html"
    echo "  Markdown: $OUTPUT_DIR/report.md"
    echo ""
    echo "Open in browser:"
    echo "  open $OUTPUT_DIR/report.html"
else
    echo ""
    echo "Note: Reports not generated. Ensure tests were run with HARNESS_ARTIFACTS=1"
fi
