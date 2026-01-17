#!/bin/bash
# Run code coverage with cargo-tarpaulin
# Usage: ./scripts/coverage.sh [quick|full|html]
#
# Modes:
#   quick - Fast coverage check, stdout only (default)
#   full  - Full coverage with HTML/JSON/Lcov reports
#   html  - Full coverage + open HTML report
#
# Prerequisites:
#   cargo install cargo-tarpaulin

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
COVERAGE_DIR="$PROJECT_ROOT/coverage"

cd "$PROJECT_ROOT"

# Check if tarpaulin is installed
if ! command -v cargo-tarpaulin &> /dev/null; then
    echo "Error: cargo-tarpaulin is not installed"
    echo "Install with: cargo install cargo-tarpaulin"
    exit 1
fi

MODE="${1:-quick}"

case "$MODE" in
    quick)
        echo "Running quick coverage check..."
        cargo tarpaulin --skip-clean --timeout 120 --out Stdout
        ;;

    full)
        echo "Running full coverage analysis..."
        mkdir -p "$COVERAGE_DIR"
        cargo tarpaulin \
            --skip-clean \
            --timeout 600 \
            --out Html \
            --out Lcov \
            --out Json \
            --output-dir "$COVERAGE_DIR"

        echo ""
        echo "Coverage reports generated in: $COVERAGE_DIR"
        echo "  - HTML:  $COVERAGE_DIR/tarpaulin-report.html"
        echo "  - LCOV:  $COVERAGE_DIR/lcov.info"
        echo "  - JSON:  $COVERAGE_DIR/tarpaulin-report.json"
        ;;

    html)
        echo "Running coverage and opening HTML report..."
        mkdir -p "$COVERAGE_DIR"
        cargo tarpaulin \
            --skip-clean \
            --timeout 600 \
            --out Html \
            --output-dir "$COVERAGE_DIR"

        # Try to open the report
        HTML_FILE="$COVERAGE_DIR/tarpaulin-report.html"
        if [[ -f "$HTML_FILE" ]]; then
            if command -v xdg-open &> /dev/null; then
                xdg-open "$HTML_FILE"
            elif command -v open &> /dev/null; then
                open "$HTML_FILE"
            else
                echo "Report generated at: $HTML_FILE"
            fi
        fi
        ;;

    lib)
        echo "Running coverage on library only..."
        cargo tarpaulin --lib --skip-clean --timeout 120 --out Stdout
        ;;

    *)
        echo "Usage: $0 [quick|full|html|lib]"
        echo ""
        echo "Modes:"
        echo "  quick - Fast coverage check, stdout only (default)"
        echo "  full  - Full coverage with HTML/JSON/Lcov reports"
        echo "  html  - Full coverage + open HTML report"
        echo "  lib   - Library code only (excludes tests)"
        exit 1
        ;;
esac
