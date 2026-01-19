#!/usr/bin/env bash
# Verify bd → br migration is complete for a file
# Usage: ./verify-migration.sh <file.md>
#
# Exit codes:
#   0 - Migration verified complete
#   1 - File not found or argument error
#   2 - Migration incomplete (blocked)

set -euo pipefail

file="${1:?Usage: verify-migration.sh <file.md>}"

if [[ ! -f "$file" ]]; then
    echo "ERROR: File not found: $file"
    exit 1
fi

echo "=== Migration Verification: $file ==="
echo ""

errors=0
warnings=0

# === MUST BE 0 ===

echo "Checking for remaining bd references (must be 0)..."

# Check for bd command references
bd_refs=$(grep -c '`bd ' "$file" 2>/dev/null || echo "0")
if [[ "$bd_refs" -gt 0 ]]; then
    echo "  ✗ FAIL: Found $bd_refs \`bd\` command references"
    grep -n '`bd ' "$file" | head -3 | sed 's/^/    /'
    errors=$((errors + 1))
else
    echo "  ✓ PASS: No \`bd\` command references"
fi

# Check for bd sync specifically
bd_sync=$(grep -c 'bd sync' "$file" 2>/dev/null || echo "0")
if [[ "$bd_sync" -gt 0 ]]; then
    echo "  ✗ FAIL: Found $bd_sync 'bd sync' references"
    grep -n 'bd sync' "$file" | head -3 | sed 's/^/    /'
    errors=$((errors + 1))
else
    echo "  ✓ PASS: No 'bd sync' references"
fi

# Check for bd-### issue IDs
bd_ids=$(grep -c 'bd-[0-9]' "$file" 2>/dev/null || echo "0")
if [[ "$bd_ids" -gt 0 ]]; then
    echo "  ✗ FAIL: Found $bd_ids 'bd-###' issue ID references"
    grep -n 'bd-[0-9]' "$file" | head -3 | sed 's/^/    /'
    errors=$((errors + 1))
else
    echo "  ✓ PASS: No 'bd-###' issue IDs"
fi

# Check for daemon references (should be removed)
daemon_refs=$(grep -ci 'daemon' "$file" 2>/dev/null || echo "0")
if [[ "$daemon_refs" -gt 0 ]]; then
    echo "  ⚠ WARN: Found $daemon_refs daemon references (br has no daemon)"
    warnings=$((warnings + 1))
fi

echo ""

# === Check if file has beads content ===

has_beads=$(grep -c 'beads\|\.beads\|br ready\|br sync' "$file" 2>/dev/null || echo "0")

if [[ "$has_beads" -gt 0 ]]; then
    echo "Checking for required br patterns (file has beads content)..."

    # Check for br sync --flush-only
    br_sync=$(grep -c 'br sync --flush-only' "$file" 2>/dev/null || echo "0")
    if [[ "$br_sync" -eq 0 ]]; then
        echo "  ⚠ WARN: No 'br sync --flush-only' found (expected if file has sync sections)"
        warnings=$((warnings + 1))
    else
        echo "  ✓ PASS: Found $br_sync 'br sync --flush-only' references"
    fi

    # Check for git add .beads/
    git_add=$(grep -c 'git add .beads/' "$file" 2>/dev/null || echo "0")
    if [[ "$git_add" -eq 0 && "$br_sync" -gt 0 ]]; then
        echo "  ⚠ WARN: No 'git add .beads/' found (required after br sync)"
        warnings=$((warnings + 1))
    elif [[ "$git_add" -gt 0 ]]; then
        echo "  ✓ PASS: Found $git_add 'git add .beads/' references"
    fi

    # Check for non-invasive note
    note=$(grep -c 'non-invasive' "$file" 2>/dev/null || echo "0")
    if [[ "$note" -eq 0 ]]; then
        echo "  ⚠ WARN: No non-invasive note found"
        warnings=$((warnings + 1))
    else
        echo "  ✓ PASS: Non-invasive note present"
    fi
else
    echo "Note: File does not appear to have beads content (skipping br pattern checks)"
fi

# === Summary ===

echo ""
echo "=== Summary ==="
if [[ "$errors" -eq 0 ]]; then
    if [[ "$warnings" -gt 0 ]]; then
        echo "PASS with $warnings warning(s)"
        echo "Review warnings above - may need attention"
        exit 0
    else
        echo "✓ PASS: Migration verified complete"
        exit 0
    fi
else
    echo "✗ FAIL: $errors error(s), $warnings warning(s)"
    echo ""
    echo "Fix the errors above and re-run verification"
    exit 2
fi
