# E2E Sync Safety Tests

This document explains how to run the sync safety end-to-end tests and interpret their output. These tests verify that `br sync` operations adhere to strict safety invariants.

## Overview

The e2e sync test suite verifies several critical safety properties:

1. **No Git Operations** - `br sync` never executes git commands, creates commits, or modifies `.git/`
2. **Path Confinement** - Sync only touches files within `.beads/` (with a strict allowlist)
3. **Atomic Writes** - Export uses write-to-temp + atomic rename; failures preserve original files
4. **Preflight Validation** - Import validates JSONL before any database changes
5. **No Partial Writes** - Failed operations leave state unchanged

## Test Files

| File | Purpose |
|------|---------|
| `tests/e2e_sync_git_safety.rs` | Verifies sync never creates commits or mutates `.git/` |
| `tests/e2e_sync_artifacts.rs` | Tests with detailed logging and artifact preservation |
| `tests/e2e_sync_fuzz_edge_cases.rs` | Malformed JSONL, path traversal, conflict markers |
| `tests/e2e_sync_failure_injection.rs` | Read-only dirs, permission errors, atomic guarantees |
| `tests/e2e_sync_preflight_integration.rs` | Preflight checks catch safety issues before writes |

## Running the Tests

### Run All Sync E2E Tests

```bash
# Run all sync-related e2e tests
cargo test e2e_sync --release

# Run with verbose output
cargo test e2e_sync --release -- --nocapture

# Run specific test file
cargo test --test e2e_sync_git_safety --release -- --nocapture
```

### Run Individual Test Categories

```bash
# Git safety regression tests
cargo test --test e2e_sync_git_safety --release

# Artifact preservation tests (detailed logging)
cargo test --test e2e_sync_artifacts --release

# Fuzz and edge case tests
cargo test --test e2e_sync_fuzz_edge_cases --release

# Failure injection tests
cargo test --test e2e_sync_failure_injection --release

# Preflight integration tests
cargo test --test e2e_sync_preflight_integration --release
```

### Run a Specific Test

```bash
# Run a specific test by name
cargo test regression_sync_export_does_not_create_commits --release -- --nocapture

# Run tests matching a pattern
cargo test conflict_marker --release -- --nocapture
```

### Debug Mode

For debugging test failures, omit `--release` to get better stack traces:

```bash
cargo test e2e_sync --release -- --nocapture 2>&1 | tee test_output.log
```

## Artifact Locations

Tests produce various artifacts for postmortem analysis:

### Temp Directory Structure

Each test creates a temporary workspace:

```
/tmp/tmp.XXXXX/           # BrWorkspace.root
├── .beads/               # Beads directory
│   ├── beads.db          # SQLite database
│   ├── issues.jsonl      # JSONL export
│   └── .manifest.json    # Optional manifest
├── logs/                 # Test logs (BrWorkspace.log_dir)
│   ├── init.log          # br init output
│   ├── create1.log       # br create output
│   ├── sync_export.log   # br sync --flush-only output
│   └── artifacts/        # Detailed artifact captures
│       ├── *_snapshots.txt
│       ├── *_commands.log
│       └── *.jsonl
└── src/                  # Simulated source files (some tests)
```

### Failure Injection Test Artifacts

Location: `target/test-artifacts/failure-injection/<test_name>/`

```
target/test-artifacts/failure-injection/
├── export_readonly_dir/
│   └── test.log          # Detailed failure logs
├── import_malformed_json/
│   └── test.log
└── ...
```

### Accessing Artifacts

After a test failure:

```bash
# Find temp directories (may already be cleaned up)
ls -la /tmp/tmp.* 2>/dev/null

# Find persisted test artifacts
ls -la target/test-artifacts/failure-injection/
```

## Log Interpretation

### Test Output Format

Each test prints structured output:

```
[TEST 1] Testing sync export...
  Snapshot before export: 15 files
  Snapshot after export: 17 files
  [PASS] Export modified 2 allowed files, 0 violations

[PASS] e2e_sync_export_with_artifacts
  - Artifacts saved to: /tmp/tmpXXX/logs/artifacts
  - JSONL size: 1234 bytes
  - Files in .beads/: 3
```

### Log File Format

Individual command logs contain:

```
label: sync_export
started: SystemTime { ... }
duration: 45.123ms
status: exit status: 0
args: ["sync", "--flush-only"]
cwd: /tmp/tmp.XXXXX

stdout:
Exported 3 issues to .beads/issues.jsonl

stderr:
[DEBUG beads_rust::sync] Starting export...
[INFO beads_rust::sync] Export complete: 3 issues
```

### Understanding Snapshot Diffs

```
=== CREATED FILES (2) ===
  CREATED: .beads/issues.jsonl (size: 1234 bytes, hash: a1b2c3d4...)
  CREATED: .beads/.manifest.json (size: 56 bytes, hash: e5f6g7h8...)

=== SUMMARY ===
Created: 2
Modified: 0
Deleted: 0
Unchanged: 15
```

### Safety Violation Messages

If a test detects a safety violation:

```
SAFETY VIOLATION: sync export modified files outside allowed list!

  MODIFIED: src/main.rs
    Before: a1b2c3d4e5f6...
    After:  f7g8h9i0j1k2...

Detailed log: /tmp/tmpXXX/logs/sync_export_diff.log
```

## Test Categories Explained

### 1. Git Safety Tests (`e2e_sync_git_safety.rs`)

Verifies the core safety invariant: **sync never touches git**.

Tests:
- `regression_sync_export_does_not_create_commits` - Export leaves HEAD unchanged
- `regression_sync_import_does_not_create_commits` - Import leaves HEAD unchanged
- `regression_full_sync_cycle_does_not_touch_git` - Full cycle preserves git state
- `regression_sync_manifest_does_not_touch_git` - Manifest generation is git-safe
- `regression_sync_never_touches_source_files` - Source files are never modified
- `integration_sync_only_touches_allowed_files` - Comprehensive allowlist verification

### 2. Artifact Tests (`e2e_sync_artifacts.rs`)

Tests with detailed logging for debugging:

- `e2e_sync_export_with_artifacts` - Export with full artifact capture
- `e2e_sync_import_with_artifacts` - Import with full artifact capture
- `e2e_sync_full_cycle_with_artifacts` - Complete cycle with artifacts
- `e2e_sync_status_with_artifacts` - Status command logging
- `e2e_sync_error_conflict_markers` - Conflict marker rejection
- `e2e_sync_export_empty_db` - Empty database handling
- `e2e_sync_deterministic_export` - Export ordering consistency

### 3. Fuzz/Edge Case Tests (`e2e_sync_fuzz_edge_cases.rs`)

Tests malformed input handling:

- Partial/truncated JSONL lines
- Invalid JSON syntax
- Conflict markers (various patterns)
- Path traversal attempts
- Symlink escape attempts
- Huge lines (1MB+ titles)
- Invalid UTF-8
- Whitespace-only files
- Empty files
- Deeply nested JSON
- Partial write prevention

### 4. Failure Injection Tests (`e2e_sync_failure_injection.rs`)

Tests atomic operation guarantees:

- Read-only directory exports
- Blocked temp file creation
- Missing file imports
- Malformed JSON imports
- Conflict marker imports
- Prefix mismatch imports
- Multiple sequential failures
- Large JSONL preservation

### 5. Preflight Tests (`e2e_sync_preflight_integration.rs`)

Tests early validation:

- Conflict marker detection
- Path validation (outside .beads, .git paths)
- Path traversal rejection
- Export safety checks
- Actionable error messages

## Troubleshooting

### Test Fails with "SAFETY VIOLATION"

This indicates a genuine safety regression. Steps:

1. Read the full error message for the specific violation
2. Check the log file path provided in the error
3. Review the snapshot diff to see exactly what changed
4. Check if `is_allowed_sync_file()` in `src/sync/path.rs` matches the test's allowlist

### Tests Hang or Timeout

```bash
# Run with timeout
timeout 120 cargo test e2e_sync --release

# Check for lock contention
lsof +D /tmp/tmp.* 2>/dev/null | grep -E '\.db'
```

### "Permission denied" Errors

Some tests (failure injection) require filesystem permission manipulation:

```bash
# Ensure tests have permission to chmod
ls -la /tmp/

# Some CI environments may restrict this - check stderr for details
cargo test e2e_sync_failure_injection -- --nocapture
```

### Flaky Tests

If tests pass/fail intermittently:

1. Check for race conditions in parallel test execution
2. Run with `--test-threads=1`:
   ```bash
   cargo test e2e_sync --release -- --test-threads=1
   ```

### "Command not found: br"

Tests require the `br` binary to be built:

```bash
# Ensure binary is built
cargo build --release

# Verify binary exists
ls -la target/release/br
```

### Git Not Installed

Some tests use git to verify safety invariants:

```bash
# Check git is available
git --version

# Install if missing (Ubuntu/Debian)
sudo apt-get install git
```

### Cleanup Stale Temp Dirs

Tests should clean up, but if space is low:

```bash
# Remove old test temp directories
rm -rf /tmp/tmp.* 2>/dev/null

# Remove test artifacts
rm -rf target/test-artifacts/
```

## CI Integration

For CI pipelines:

```yaml
# GitHub Actions example
- name: Run sync safety tests
  run: |
    cargo test e2e_sync --release -- --nocapture 2>&1 | tee sync_test_output.log

- name: Upload test artifacts on failure
  if: failure()
  uses: actions/upload-artifact@v3
  with:
    name: sync-test-artifacts
    path: |
      target/test-artifacts/
      sync_test_output.log
```

## Related Documentation

- [SYNC_SAFETY.md](SYNC_SAFETY.md) - Sync safety model and design
- `.beads/SYNC_SAFETY_INVARIANTS.md` - Safety invariants specification
- `.beads/SYNC_CLI_FLAG_SEMANTICS.md` - CLI flag behavior
- `.beads/SYNC_THREAT_MODEL.md` - Threat model for sync operations
