# Test Harness Documentation

This document explains how to run the comprehensive E2E, conformance, and benchmark test suites for `br` (beads Rust).

## Overview

The test harness provides:

1. **E2E Tests** - End-to-end tests verifying CLI behavior and output parity
2. **Conformance Tests** - Cross-implementation parity tests (br vs bd)
3. **Benchmarks** - Performance measurements and regression detection
4. **Artifact Logging** - Detailed logs and snapshots for debugging

## Quick Start

```bash
# Fast feedback loop (recommended during development)
scripts/e2e.sh                    # Quick E2E subset (~6 tests)

# Full test suite
E2E_FULL_CONFIRM=1 scripts/e2e_full.sh   # All E2E tests

# Conformance (requires bd binary)
scripts/conformance.sh            # br vs bd parity checks

# Benchmarks
scripts/bench.sh --quick          # Quick performance comparison
```

## Script Reference

| Script | Purpose | Duration | When to Use |
|--------|---------|----------|-------------|
| `scripts/e2e.sh` | Quick E2E subset | ~30s | PR feedback, local iteration |
| `scripts/e2e_full.sh` | All E2E tests | 2-5min | Pre-merge validation |
| `scripts/conformance.sh` | br↔bd parity | 1-3min | Implementation changes |
| `scripts/bench.sh` | Benchmarks | 3-10min | Performance work |
| `scripts/ci-local.sh` | Full CI simulation | 2-5min | Before pushing |

## E2E Tests

### Quick E2E (`scripts/e2e.sh`)

Runs a curated subset of tests for fast feedback:

```bash
scripts/e2e.sh                    # Run quick subset
scripts/e2e.sh --verbose          # Show test output
scripts/e2e.sh --json             # JSON summary
scripts/e2e.sh --filter lifecycle # Run matching tests
```

**Tests included:**
- `e2e_basic_lifecycle` - Create/update/close/delete workflow
- `e2e_ready` - Ready command behavior
- `e2e_create_output` - Create command output format
- `e2e_list_priority` - List with priority filtering
- `e2e_errors` - Error handling and exit codes
- `e2e_harness_demo` - Harness infrastructure validation

### Full E2E (`scripts/e2e_full.sh`)

Runs all `tests/e2e_*.rs` files:

```bash
E2E_FULL_CONFIRM=1 scripts/e2e_full.sh     # All tests
scripts/e2e_full.sh --parallel             # Parallel execution
scripts/e2e_full.sh --filter sync          # Only sync-related
scripts/e2e_full.sh --dataset beads_rust   # Specific dataset
```

**Environment variables:**
- `E2E_FULL_CONFIRM=1` - Skip confirmation prompt
- `E2E_TIMEOUT=300` - Per-test timeout (default: 120s)
- `E2E_PARALLEL=1` - Enable parallel execution
- `E2E_DATASET=beads_rust` - Dataset to use

### Individual E2E Test Files

```bash
# Run specific test file
cargo test --test e2e_sync_git_safety --release -- --nocapture

# Run specific test by name
cargo test regression_sync_export_does_not_create_commits --release -- --nocapture
```

## Conformance Tests

Conformance tests verify br produces identical outputs to bd (Go implementation).

### Requirements

- Both `br` (Rust) and `bd` (Go) binaries must be available
- bd is typically at `/data/projects/beads/.bin/beads`

### Running Conformance

```bash
scripts/conformance.sh                    # Run all conformance
scripts/conformance.sh --check-bd         # Verify bd is available
scripts/conformance.sh --verbose          # Show test output
scripts/conformance.sh --json             # JSON summary
scripts/conformance.sh --filter schema    # Only schema tests
```

**Environment variables:**
- `BD_BINARY=/path/to/bd` - Override bd location
- `BR_BINARY=/path/to/br` - Override br location
- `CONFORMANCE_TIMEOUT=180` - Per-test timeout
- `CONFORMANCE_STRICT=1` - Fail on any differences

### Conformance Test Files

| File | Purpose |
|------|---------|
| `tests/conformance.rs` | Core command parity |
| `tests/conformance_edge_cases.rs` | Edge case handling |
| `tests/conformance_labels_comments.rs` | Labels/comments parity |
| `tests/conformance_schema.rs` | DB schema compatibility |

### Conformance Output

Output written to `target/test-artifacts/conformance/`:
- `conformance_summary.json` - Overall results
- `<test>/` - Per-test artifacts and logs

## Benchmarks

### Quick Benchmark

```bash
scripts/bench.sh --quick            # Quick comparison only
BENCH_CONFIRM=1 scripts/bench.sh    # Full benchmarks
```

### Criterion Benchmarks

```bash
scripts/bench.sh --criterion                     # Run criterion
scripts/bench.sh --save baseline-v1              # Save baseline
scripts/bench.sh --baseline baseline-v1          # Compare to baseline
```

### br vs bd Comparison

```bash
scripts/bench.sh --compare          # Compare br and bd
```

**Environment variables:**
- `BENCH_CONFIRM=1` - Skip confirmation
- `BENCH_TIMEOUT=600` - Per-benchmark timeout
- `BENCH_DATASET=beads_rust` - Dataset to benchmark

### Benchmark Suites (Cold/Warm, Synthetic Scale, Real Datasets)

In addition to Criterion, the repo includes benchmark suites under `tests/` that
produce structured JSON in `target/benchmark-results/`. These are opt-in and
use isolated workspaces so source datasets are never mutated.

**Cold/Warm start**
```bash
cargo test --test bench_cold_warm_start -- --nocapture --ignored
HARNESS_ARTIFACTS=1 cargo test --test bench_cold_warm_start -- --nocapture --ignored
```
Outputs: `target/benchmark-results/cold_warm_*_latest.json`,
`target/benchmark-results/cold_warm_all_<timestamp>.json`

**Synthetic scale (10k–250k issues)**
```bash
BR_E2E_STRESS=1 cargo test --test bench_synthetic_scale -- --nocapture --ignored
```
Outputs: `target/benchmark-results/synthetic_*_latest.json`,
`target/benchmark-results/synthetic_all_<timestamp>.json`

**Real datasets**
```bash
cargo test --test bench_real_datasets -- --nocapture --ignored
HARNESS_ARTIFACTS=1 cargo test --test bench_real_datasets -- --nocapture --ignored
```
Outputs: `target/benchmark-results/real_datasets_latest.json`,
`target/benchmark-results/real_datasets_<timestamp>.json`

### Benchmark Output

- `target/test-artifacts/benchmark_summary.json` - Summary
- `target/test-artifacts/benchmark/` - Detailed logs
- `target/criterion/` - Criterion reports with HTML

## Artifact Logging

Enable detailed artifact logging for debugging:

```bash
HARNESS_ARTIFACTS=1 scripts/e2e.sh
HARNESS_PRESERVE_SUCCESS=1 scripts/e2e.sh   # Keep artifacts on success
```

### Artifact Locations

```
target/test-artifacts/
├── e2e_quick_summary.json         # Quick E2E summary
├── e2e_full_summary.json          # Full E2E summary
├── conformance_summary.json       # Conformance summary
├── benchmark_summary.json         # Benchmark summary
├── conformance/                   # Conformance artifacts
│   └── <test_name>/
│       ├── br_output.json
│       ├── bd_output.json
│       └── diff.txt
├── benchmark/                     # Benchmark artifacts
│   ├── quick_comparison.log
│   ├── criterion.log
│   └── bd_comparison.json
└── failure-injection/            # Failure injection test logs
    └── <test_name>/
        └── test.log
```

### Summary JSON Format

All summary files follow this structure:

```json
{
  "suite": "e2e_quick",
  "generated_at": "2026-01-18T00:00:00Z",
  "passed": 5,
  "failed": 0,
  "skipped": 1,
  "total": 5,
  "duration_s": 45,
  "artifacts_dir": "target/test-artifacts",
  "results": [
    {"test": "e2e_basic_lifecycle", "result": "pass", "duration_s": 12.5},
    {"test": "e2e_errors", "result": "pass", "duration_s": 8.2}
  ]
}
```

## CI Integration

### GitHub Actions Workflow

The existing `.github/workflows/ci.yml` runs:

1. **check** - Formatting, clippy, cargo check
2. **security** - cargo-audit
3. **test** - Full test suite (`cargo test`)
4. **coverage** - llvm-cov with Codecov upload
5. **build** - Multi-platform binaries
6. **bench** - Criterion benchmarks with regression detection

### Adding E2E to PR Checks

Add to `.github/workflows/ci.yml`:

```yaml
  e2e-quick:
    name: Quick E2E
    runs-on: ubuntu-latest
    timeout-minutes: 10
    needs: check
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@master
        with:
          toolchain: nightly

      - name: Cache cargo
        uses: Swatinem/rust-cache@v2

      - name: Run quick E2E tests
        run: scripts/e2e.sh --json
        env:
          NO_COLOR: 1

      - name: Upload E2E summary
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: e2e-quick-summary
          path: target/test-artifacts/e2e_quick_summary.json
```

### Full Conformance (On-Demand)

Create `.github/workflows/conformance.yml`:

```yaml
name: Conformance

on:
  workflow_dispatch:
    inputs:
      bd_version:
        description: 'bd version or path'
        default: 'latest'
  schedule:
    - cron: '0 6 * * 1'  # Weekly Monday 6am

jobs:
  conformance:
    runs-on: ubuntu-latest
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@master
        with:
          toolchain: nightly

      - name: Cache cargo
        uses: Swatinem/rust-cache@v2

      - name: Install bd (Go beads)
        run: |
          go install github.com/example/beads/cmd/bd@latest
          echo "$HOME/go/bin" >> $GITHUB_PATH

      - name: Run conformance tests
        run: scripts/conformance.sh --json
        env:
          BD_BINARY: ${{ github.workspace }}/../bd
          CONFORMANCE_TIMEOUT: 300

      - name: Upload conformance results
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: conformance-results
          path: |
            target/test-artifacts/conformance_summary.json
            target/test-artifacts/conformance/
```

### Benchmark Regression Detection

The existing `ci.yml` already includes benchmark regression detection:

```yaml
  - name: Check benchmark regressions (10% threshold)
    run: |
      # Python script compares criterion baselines
      # Fails if any benchmark is >10% slower
```

## Local Development Workflow

### Before Pushing

```bash
# Run local CI checks
scripts/ci-local.sh

# Or step-by-step:
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

### Quick Iteration

```bash
# Fast feedback on specific changes
cargo test --test e2e_basic_lifecycle -- --nocapture

# With artifacts
HARNESS_ARTIFACTS=1 cargo test --test e2e_sync_git_safety -- --nocapture
```

### Debugging Test Failures

1. Enable artifacts: `HARNESS_ARTIFACTS=1`
2. Preserve on success: `HARNESS_PRESERVE_SUCCESS=1`
3. Run with `--nocapture` for live output
4. Check `target/test-artifacts/<suite>/<test>/`

## Test Categories

### E2E Test Types

| Pattern | Tests |
|---------|-------|
| `e2e_basic_*` | Core lifecycle operations |
| `e2e_sync_*` | Sync safety and atomicity |
| `e2e_list_*` | List command variations |
| `e2e_search_*` | Search functionality |
| `e2e_errors_*` | Error handling |
| `e2e_*_scenarios` | Multi-step scenarios |

### Conformance Test Types

| File | Scope |
|------|-------|
| `conformance.rs` | Core command parity |
| `conformance_edge_cases.rs` | Unusual inputs/states |
| `conformance_labels_comments.rs` | Metadata handling |
| `conformance_schema.rs` | Database schema |

## Environment Variables Reference

| Variable | Default | Description |
|----------|---------|-------------|
| `HARNESS_ARTIFACTS` | 0 | Enable artifact logging |
| `HARNESS_PRESERVE_SUCCESS` | 0 | Keep artifacts on success |
| `BR_BINARY` | auto | Path to br binary |
| `BD_BINARY` | auto | Path to bd binary |
| `E2E_TIMEOUT` | 120 | E2E per-test timeout (seconds) |
| `E2E_FULL_CONFIRM` | 0 | Skip full E2E confirmation |
| `E2E_DATASET` | auto | Dataset to use |
| `CONFORMANCE_TIMEOUT` | 120 | Conformance per-test timeout |
| `CONFORMANCE_STRICT` | 0 | Fail on any differences |
| `BENCH_TIMEOUT` | 300 | Benchmark timeout |
| `BENCH_CONFIRM` | 0 | Skip benchmark confirmation |
| `BENCH_DATASET` | beads_rust | Benchmark dataset |
| `NO_COLOR` | 0 | Disable colored output |
| `RUST_LOG` | - | Enable debug logging |

## Troubleshooting

### Tests Hang or Timeout

```bash
# Run with explicit timeout
timeout 120 cargo test e2e_sync --release

# Check for lock contention
lsof +D /tmp/tmp.* 2>/dev/null | grep -E '\.db'
```

### "Command not found: br"

```bash
# Ensure binary is built
cargo build --release

# Verify binary exists
ls -la target/release/br
```

### Conformance "bd not found"

```bash
# Check bd availability
scripts/conformance.sh --check-bd

# Set bd path explicitly
BD_BINARY=/path/to/bd scripts/conformance.sh
```

### Flaky Tests

```bash
# Run serially to avoid race conditions
cargo test e2e_sync --release -- --test-threads=1
```

### Cleanup Stale State

```bash
# Remove temp directories
rm -rf /tmp/tmp.* 2>/dev/null

# Remove test artifacts
rm -rf target/test-artifacts/
```

## Related Documentation

- [SYNC_SAFETY.md](SYNC_SAFETY.md) - Sync safety model
- [E2E_SYNC_TESTS.md](E2E_SYNC_TESTS.md) - Sync-specific test details
- [ARTIFACT_LOG_SCHEMA.md](ARTIFACT_LOG_SCHEMA.md) - Artifact format specification
