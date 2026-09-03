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

## Running under RCH time caps

Agent sessions offload cargo through RCH, which caps `cargo clippy
--all-targets` at about 5 minutes and `cargo test`/`cargo build` at about 30
minutes and kills the job (exit 137, reported as "likely resource exhaustion")
when the cap is hit. This crate has 162 integration test binaries under
`tests/`; a cold `cargo test --all-features` does not fit in the cap, and the
automatic retry lands on a cold worker and fails the same way.

Run one of these instead:

| Goal | Command | Cold / warm |
|---|---|---|
| Unit suite (2,894 tests) | `rch exec -- cargo test --lib` | ~20 min / ~1 min |
| One unit test or module | `rch exec -- cargo test --lib <filter>` | ~20 min / seconds |
| One integration binary | `rch exec -- cargo test --test <name>` | 5-15 min / ~1 min |
| Clippy when all-targets is killed | `rch exec -- cargo clippy --lib --bins -- -D warnings` then `--tests` | — |

`scripts/test-shard.sh <shard>` partitions the suite into cap-sized groups by
file name (no manifest to maintain): `lib`, `e2e-a-l`, `e2e-m-z`, `storage`
(storage/proptest/repro/workflow binaries), `misc` (conformance, snapshots,
goldens, docs, manifests), and `bench`. `scripts/test-shard.sh list` and
`show <shard>` print the membership; `.github/workflows/ci.yml` runs the first
five as a matrix. Under RCH, `rch exec -- scripts/test-shard.sh e2e-a-l` fits
the 30-minute cap on a warm worker; on a cold worker run binaries individually
or in small groups (`rch queue` shows the worker a job landed on).

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

**On an RCH worker (no local bd):** the workers have Go, so build the
workflow's pinned bd on one and point the tests at it. The job lane and the
compile lane choose workers independently; build on the worker the compile
lane keeps selecting (check `Selected worker:` in the rch output):

```bash
rch exec --job -- bash -c 'git clone -q --depth 1 --branch v0.46.0 \
  https://github.com/steveyegge/beads.git /tmp/beads-go && \
  cd /tmp/beads-go && go build -o /tmp/bd ./cmd/bd && /tmp/bd version'
rch exec -- env BD_BINARY=/tmp/bd NO_COLOR=1 cargo test --test conformance_schema
```

A worker without `/tmp/bd` skips the bd-dependent tests with one actionable
message rather than failing.

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

### Known divergences from bd 0.46.0

The Conformance workflow pins bd `v0.46.0` (commit `812f4e52`). Running the
six conformance targets against that build on 2026-09-03 left these
differences, which are deliberate on br's side or belong to bd's newer
schema; each is encoded in the tests as an allowlist entry or an `#[ignore]`
with a reason pointing here, never as a weakened comparison:

- **Plain-text output is br's own format.** `list`, `ready`, `show`, and
  `stats` render status glyphs (`○`), priority markers (`● P2`), an
  owner/type line in `show`, "All work complete" for an empty `ready`, and
  name the binary as `br` in hints. Parity is checked on `--json`; the nine
  `conformance_text_output.rs` tests that diff plain text are ignored with
  this reason.
- **`dep cycles` agrees with bd since #391.** Only blocking edge types are
  cycle-checked, so a loop of `related` edges is not a cycle for either tool
  and both exit 0. The older expectation (br exits 5 on any-type loops) was
  from #368 and is gone.
- **br paginates list-shaped JSON.** `list --json` and `blocked --json`
  return `{"issues": [...], "total", "limit", "offset", "has_more"}`; bd
  prints a bare array with the same per-issue fields. Count and shape tests
  unwrap `issues` (`common::cli::extract_issues_array`) before comparing.
- **Schema.** br-only tables: `capacity_exemption_history`,
  `capacity_exemptions`, `capacity_occupancy`, `close_metadata`,
  `gate_result_history`, `gate_results`. bd-only tables: `compaction_snapshots`,
  `issue_snapshots`, `repo_mtimes`. br-only columns: `issues.owner`,
  `issues.agent_context`, `issues.source_system`, `events.agent_name`,
  `events.harness`, `events.model`, `blocked_issues_cache.blocked_at`.
  bd-only column: `dirty_issues.content_hash`. Constraint differences:
  `config.key` (bd: primary key, nullable), `dependencies.type` (bd: part of
  the primary key), `dependencies.created_at` and
  `blocked_issues_cache.issue_id` (NOT NULL differs). bd 0.46 dropped
  `issues.owner`, so it is no longer in the core column set.
- **br's JSONL dependency records carry `thread_id` and `metadata`**
  (per-edge provenance, #484–#486); bd's do not. The workflow JSONL
  comparison ignores those two br-only leaf fields.
- **`bd` prints a background-sync note** ("No git repository initialized")
  on stderr in every scratch workspace; tests that assert on bd's stderr
  must tolerate it.
- **`bd show <unknown id>` is silent**: exit 0, empty stdout, only the note
  above on stderr. br exits 3 with an ISSUE_NOT_FOUND diagnostic; the
  not-found text test asserts br and only records what bd did.

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
cargo test --test bench_cold_warm_start startup_matrix_smoke_bundle -- --nocapture
cargo test --test bench_cold_warm_start perf_evidence_smoke_bundle -- --nocapture
```
Outputs: `target/benchmark-results/cold_warm_*_latest.json`,
`target/benchmark-results/cold_warm_all_<timestamp>.json`. The startup matrix
smoke test also writes a validated bundle under
`target/perf-artifacts/startup-matrix-smoke-*/` with command logs, timing,
syscall, RSS, and raw stdout/stderr slots for clean, stale, routed, no-db,
read-only-fast-open, sync-status, and recovery-anomaly startup states. The perf
evidence smoke test writes `target/perf-artifacts/perf-evidence-smoke-*/` with
the `perf-evidence-manifest.json`, timing samples, placeholder syscall/IO/RSS
slots, golden stdout/stderr checksums, an isomorphism note, and an enforcing
self-baseline comparison.

**Synthetic scale (CI profile, 10k–250k issues, and manual million-agent profile)**
```bash
 cargo test --test bench_synthetic_scale synthetic_ci_profile -- --nocapture
BR_E2E_STRESS=1 cargo test --test bench_synthetic_scale -- --nocapture --ignored
BR_E2E_STRESS=1 BR_SYNTHETIC_MILLION=1 BR_SYNTHETIC_SEED=42 \
  cargo test --test bench_synthetic_scale stress_synthetic_million -- --nocapture --ignored
```
Outputs: `target/benchmark-results/synthetic_*_latest.json`,
`target/benchmark-results/synthetic_all_<timestamp>.json`. The generator streams
deterministic JSONL directly, then validates the corpus through real
`br sync --import-only`, `br doctor --json`, and `br sync --status --json`
surfaces. Each generated workspace writes `synthetic-corpus-manifest.json` with
the seed, issue count, dependency density, label/comment distributions, simulated
agent count, claim density, skewed-DAG factor, JSONL hash, file-size report, and
health results. The manual million profile targets 1,000,000 issues and 10,000
simulated agents when the host has enough memory and CPU. The benchmark operation
set includes `graph_hot_hub` for wide reverse-dependent graph reads and
`dep_tree_hot_leaf` for bounded deep dependency-tree reads on the skewed DAG.
`graph_all_components` is included only for corpora up to 10,000 issues so the
small profile records full connected-component graph evidence without forcing
million-issue profiles to materialize a full graph JSON document.

**Contention replay lab (CI smoke and manual 64-worker profile)**
```bash
cargo test --test bench_contention_replay -- --nocapture
BR_CONTENTION_64=1 cargo test --test bench_contention_replay \
  manual_64_worker_contention_profile_records_replayable_trace -- --ignored --nocapture
```
Outputs: `target/test-artifacts/contention-replay/<profile>-seed-*/`.
The trace schema records worker id, command, start/end timing, estimated write
lock wait, auto-import/auto-flush classification, exit code, stdout/stderr
hashes, and replay seed. Replay creates a fresh workspace from only the trace
plan and reports the first divergent worker/event if exit codes or created
issue effects differ.

**NUMA/high-core read-command profile (manual 64+ core evidence)**
```bash
export BR_NUMA_PROFILE_DIR=tests/artifacts/perf/beads-perf-<timestamp>-numa-read-command-profile
export BR_NUMA_PROFILE_WORKSPACE=/data/tmp/br-large-read-profile
export BR_NUMA_PROFILE_BINARY=/data/tmp/br-release/release/br

mkdir -p "$BR_NUMA_PROFILE_DIR"/{env,commands,golden,timing,syscalls,raw}
lscpu > "$BR_NUMA_PROFILE_DIR/env/lscpu.txt"
lscpu --json > "$BR_NUMA_PROFILE_DIR/env/lscpu.json"
numactl --hardware > "$BR_NUMA_PROFILE_DIR/env/numactl-hardware.txt"
free -b > "$BR_NUMA_PROFILE_DIR/env/free-bytes.txt"

hyperfine --warmup 2 --runs 10 \
  --export-json "$BR_NUMA_PROFILE_DIR/timing/hyperfine-default.json" \
  --command-name list_json_limit100 \
    "NO_COLOR=1 $BR_NUMA_PROFILE_BINARY --no-auto-import --no-auto-flush list --json --limit 100" \
  --command-name ready_json_limit100 \
    "NO_COLOR=1 $BR_NUMA_PROFILE_BINARY --no-auto-import --no-auto-flush ready --json --limit 100" \
  --command-name scheduler_json_candidate100 \
    "NO_COLOR=1 $BR_NUMA_PROFILE_BINARY --no-auto-import --no-auto-flush scheduler --json --candidate-limit 100" \
  --command-name search_agent_json_limit100 \
    "NO_COLOR=1 $BR_NUMA_PROFILE_BINARY --no-auto-import --no-auto-flush search agent --json --limit 100" \
  --command-name stats_no_activity_json \
    "NO_COLOR=1 $BR_NUMA_PROFILE_BINARY --no-auto-import --no-auto-flush stats --no-activity --json" \
  --command-name label_list_all_json \
    "NO_COLOR=1 $BR_NUMA_PROFILE_BINARY --no-auto-import --no-auto-flush label list-all --json"
```
Run the same matrix pinned to one logical CPU:

```bash
hyperfine --warmup 2 --runs 10 \
  --export-json "$BR_NUMA_PROFILE_DIR/timing/hyperfine-pinned-cpu0.json" \
  --command-name list_json_limit100_cpu0 \
    "taskset -c 0 env NO_COLOR=1 $BR_NUMA_PROFILE_BINARY --no-auto-import --no-auto-flush list --json --limit 100" \
  --command-name ready_json_limit100_cpu0 \
    "taskset -c 0 env NO_COLOR=1 $BR_NUMA_PROFILE_BINARY --no-auto-import --no-auto-flush ready --json --limit 100" \
  --command-name scheduler_json_candidate100_cpu0 \
    "taskset -c 0 env NO_COLOR=1 $BR_NUMA_PROFILE_BINARY --no-auto-import --no-auto-flush scheduler --json --candidate-limit 100" \
  --command-name search_agent_json_limit100_cpu0 \
    "taskset -c 0 env NO_COLOR=1 $BR_NUMA_PROFILE_BINARY --no-auto-import --no-auto-flush search agent --json --limit 100" \
  --command-name stats_no_activity_json_cpu0 \
    "taskset -c 0 env NO_COLOR=1 $BR_NUMA_PROFILE_BINARY --no-auto-import --no-auto-flush stats --no-activity --json" \
  --command-name label_list_all_json_cpu0 \
    "taskset -c 0 env NO_COLOR=1 $BR_NUMA_PROFILE_BINARY --no-auto-import --no-auto-flush label list-all --json"
```
On hosts where `numactl --hardware` reports at least two nodes, also run a
cross-node matrix such as `numactl --cpunodebind=0 --membind=1 ...` and the
reverse binding. On single-node hosts, keep a `cross_numa` entry in
`manifest.json` with `status: "unavailable_on_pilot_host"` and preserve the raw
`numactl` output.

For each command, capture one `strace -qq -c -o ...` summary and one golden
stdout/stderr pair. The profile bundle must include `env.json`, `manifest.json`,
command stdout/stderr files, `golden/command-output-sha256.txt`, raw hyperfine
samples, p50/p95/p99 summaries, syscall summaries, and `notes.md` with the tail
decomposition across queueing/lock, service CPU, IO/page reads, and
serialization/output. See `docs/ARTIFACT_LOG_SCHEMA.md` for
`br.numa-read-command-profile.v1`.

**Swarm capacity-planning report (manual operator artifact)**
```bash
export BR_CAPACITY_REPORT_DIR=tests/artifacts/perf/beads-perf-<timestamp>-swarm-capacity-planning
export BR_CAPACITY_WORKSPACE=/data/tmp/br-large-read-profile
export BR_CAPACITY_BINARY=/data/tmp/br-release/release/br
export BR_NUMA_PROFILE_DIR=tests/artifacts/perf/beads-perf-<timestamp>-numa-read-command-profile

mkdir -p "$BR_CAPACITY_REPORT_DIR"/{inputs,golden}
cp "$BR_NUMA_PROFILE_DIR/env.json" "$BR_CAPACITY_REPORT_DIR/inputs/numa-env.json"
cp "$BR_NUMA_PROFILE_DIR/timing/default-summary.json" \
  "$BR_CAPACITY_REPORT_DIR/inputs/read-default-summary.json"
cp "$BR_NUMA_PROFILE_DIR/timing/pinned-cpu0-summary.json" \
  "$BR_CAPACITY_REPORT_DIR/inputs/read-pinned-cpu0-summary.json"

"$BR_CAPACITY_BINARY" --no-auto-import --no-auto-flush count --json \
  > "$BR_CAPACITY_REPORT_DIR/inputs/count.json"
"$BR_CAPACITY_BINARY" --no-auto-import --no-auto-flush sync --status --json \
  > "$BR_CAPACITY_REPORT_DIR/inputs/sync-status.json"
"$BR_CAPACITY_BINARY" --no-auto-import --no-auto-flush doctor --json \
  > "$BR_CAPACITY_REPORT_DIR/inputs/doctor.json"
```
The report should emit both `report.json` and `report.md`. The JSON report uses
`br.swarm-capacity-report.v1` and must include source evidence paths, issue
count, dirty/export state, doctor status, host profile, weighted read p95,
assumed command cadence, green/yellow/red agent bands, laptop/small-VM fallback
guidance, and invalidation rules. The Markdown report is the operator-facing
view. Always include `golden/report-sha256.txt` for snapshot-style checks.

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
        uses: dtolnay/rust-toolchain@nightly
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
        uses: dtolnay/rust-toolchain@nightly
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
timeout 120 cargo test --release --test e2e_sync_git_safety --test e2e_sync_status_health --test e2e_vcs_status

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
cargo test --release --test e2e_sync_git_safety --test e2e_sync_status_health --test e2e_vcs_status -- --test-threads=1
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
