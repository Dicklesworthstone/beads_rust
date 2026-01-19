# Artifact Log Schema

> Machine-parseable JSONL schema for E2E test artifacts.
> Task: beads_rust-r23m

## Overview

The E2E test harness emits structured logs in JSONL format for:
1. Command execution events
2. File tree snapshots
3. Test run summaries

All artifacts are written to `target/test-artifacts/<suite>/<test>/`.

## Schema Definitions

### 1. RunEvent (events.jsonl)

Each line in `events.jsonl` is a JSON object matching this schema:

```json
{
  "timestamp": "2026-01-17T12:34:56.789Z",  // RFC3339, required
  "event_type": "command",                   // "command" | "snapshot", required
  "label": "init",                           // human-readable step name, required
  "binary": "br",                            // binary executed, required for command
  "args": ["init", "--prefix", "bd"],        // array of strings, required
  "cwd": "/tmp/test123",                     // working directory, required
  "exit_code": 0,                            // integer, required for command
  "success": true,                           // boolean, required
  "duration_ms": 42,                         // integer (milliseconds), required for command
  "stdout_len": 1024,                        // byte count, required
  "stderr_len": 0,                           // byte count, required
  "stdout_path": "0001_init.stdout",         // optional, relative path
  "stderr_path": null,                       // optional, relative path
  "snapshot_path": null                      // optional, for event_type=snapshot
}
```

#### Field Definitions

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `timestamp` | string | Yes | RFC3339 timestamp (UTC) |
| `event_type` | string | Yes | `"command"` or `"snapshot"` |
| `label` | string | Yes | Human-readable step identifier |
| `binary` | string | Yes* | Binary name (`"br"`, `"bd"`, `"git"`) |
| `args` | string[] | Yes | Command arguments (excluding binary) |
| `cwd` | string | Yes | Absolute path to working directory |
| `exit_code` | integer | Yes* | Process exit code (0 = success) |
| `success` | boolean | Yes | True if exit_code == 0 |
| `duration_ms` | integer | Yes* | Execution time in milliseconds |
| `stdout_len` | integer | Yes | Byte count of stdout |
| `stderr_len` | integer | Yes | Byte count of stderr |
| `stdout_path` | string? | No | Relative path to stdout capture file |
| `stderr_path` | string? | No | Relative path to stderr capture file |
| `snapshot_path` | string? | No | Relative path to snapshot JSON |

*Required when `event_type` is `"command"`.

### 2. FileEntry (*.snapshot.json)

Snapshot files contain an array of file entries:

```json
[
  {"path": ".beads", "size": 0, "is_dir": true},
  {"path": ".beads/beads.db", "size": 12288, "is_dir": false},
  {"path": ".beads/issues.jsonl", "size": 456, "is_dir": false}
]
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `path` | string | Yes | Path relative to workspace root |
| `size` | integer | Yes | File size in bytes (0 for dirs) |
| `is_dir` | boolean | Yes | True if directory |

### 3. Summary (summary.json)

Written at test completion:

```json
{
  "suite": "e2e_basic",
  "test": "test_create_issue",
  "passed": true,
  "run_count": 5,
  "timestamp": "2026-01-17T12:35:00.000Z"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `suite` | string | Yes | Test suite name |
| `test` | string | Yes | Test function name |
| `passed` | boolean | Yes | Overall test result |
| `run_count` | integer | Yes | Number of commands executed |
| `timestamp` | string | Yes | RFC3339 completion timestamp |

## Directory Structure

```
target/test-artifacts/
└── <suite>/
    └── <test>/
        ├── events.jsonl          # Command and snapshot events
        ├── summary.json          # Test result summary
        ├── 0001_init.stdout      # Captured stdout
        ├── 0001_init.stderr      # Captured stderr (if non-empty)
        ├── before.snapshot.json  # File tree snapshot
        └── after.snapshot.json   # File tree snapshot
```

## Validation Rules

### Required Invariants

1. **Timestamp format**: Must be valid RFC3339 with timezone
2. **Event type**: Must be exactly `"command"` or `"snapshot"`
3. **Args array**: Must be array of strings, never null
4. **Path safety**: All paths must be relative (no `..` traversal)
5. **Exit codes**: Must be integer in range [-128, 255]
6. **Size values**: Must be non-negative integers

### Cross-Platform Normalization

1. **Line endings**: All text outputs normalized to `\n`
2. **Paths**: Forward slashes `/` on all platforms
3. **Timestamps**: Always UTC with `Z` suffix or offset

## Programmatic Validation

Use the `ArtifactValidator` in tests:

```rust
use beads_rust::test_utils::ArtifactValidator;

let validator = ArtifactValidator::new();
validator.validate_events_file("target/test-artifacts/e2e/test/events.jsonl")?;
validator.validate_snapshot_file("target/test-artifacts/e2e/test/before.snapshot.json")?;
validator.validate_summary_file("target/test-artifacts/e2e/test/summary.json")?;
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `HARNESS_ARTIFACTS` | `0` | Set to `1` to enable artifact logging |
| `HARNESS_PRESERVE_SUCCESS` | `0` | Set to `1` to keep all artifacts on success |

## Example: Valid events.jsonl

```jsonl
{"timestamp":"2026-01-17T12:34:56.000Z","event_type":"command","label":"init","binary":"br","args":["init"],"cwd":"/tmp/test","exit_code":0,"success":true,"duration_ms":42,"stdout_len":64,"stderr_len":0,"stdout_path":"0001_init.stdout","stderr_path":null,"snapshot_path":null}
{"timestamp":"2026-01-17T12:34:56.100Z","event_type":"snapshot","label":"after_init","binary":"","args":[],"cwd":"/tmp/test","exit_code":0,"success":true,"duration_ms":0,"stdout_len":0,"stderr_len":0,"stdout_path":null,"stderr_path":null,"snapshot_path":"after_init.snapshot.json"}
{"timestamp":"2026-01-17T12:34:56.200Z","event_type":"command","label":"create","binary":"br","args":["create","--title","Test issue"],"cwd":"/tmp/test","exit_code":0,"success":true,"duration_ms":15,"stdout_len":32,"stderr_len":0,"stdout_path":"0002_create.stdout","stderr_path":null,"snapshot_path":null}
```

## Report Generation

### Generating Human-Friendly Reports

The artifact report indexer generates HTML and Markdown reports from test artifacts
for faster triage of test failures.

**Task: beads_rust-x7on**

#### Quick Start

```bash
# 1. Run tests with artifacts enabled
HARNESS_ARTIFACTS=1 cargo test e2e_sync

# 2. Generate reports
./scripts/generate-report.sh

# 3. Open the report
open target/reports/report.html
```

#### Manual Report Generation

```bash
REPORT_ARTIFACTS_DIR=target/test-artifacts \
REPORT_OUTPUT_DIR=target/reports \
cargo test --test e2e_report_generation -- --nocapture --ignored generate_and_save_report
```

#### Programmatic Usage

```rust
use common::report_indexer::{ArtifactIndexer, write_reports};

// Create indexer
let indexer = ArtifactIndexer::new("target/test-artifacts");

// Generate report
let report = indexer.generate_report()?;

// Access report data
println!("Total tests: {}", report.total_tests);
println!("Pass rate: {:.1}%", report.pass_rate());

// Write HTML and Markdown files
let (md_path, html_path) = write_reports(&report, "target/reports")?;
```

#### Report Contents

The generated reports include:

- **Summary statistics**: Total tests, pass/fail counts, duration
- **Suite breakdown**: Per-suite results with test tables
- **Failed test details**: Failure reasons, failed commands, artifact links
- **Slowest tests**: Top 10 slowest tests for performance analysis

#### Configuration Options

```rust
use common::report_indexer::{ArtifactIndexer, IndexerConfig};

let config = IndexerConfig {
    artifact_root: PathBuf::from("target/test-artifacts"),
    failures_only: true,   // Only include failed tests
    max_tests: 100,        // Limit tests per suite (0 = unlimited)
    include_commands: true, // Include command details
    include_snapshots: true, // Include snapshot metrics
};

let indexer = ArtifactIndexer::with_config(config);
```

## References

- [E2E_COVERAGE_MATRIX.md](E2E_COVERAGE_MATRIX.md) - Test coverage mapping
- [TROUBLESHOOTING.md](TROUBLESHOOTING.md) - Error codes and JSON shapes
- [tests/common/harness.rs](../tests/common/harness.rs) - Harness implementation
- [tests/common/report_indexer.rs](../tests/common/report_indexer.rs) - Report indexer implementation

---

*Updated: 2026-01-18*
*Tasks: beads_rust-r23m, beads_rust-x7on*
*Agents: SilentFalcon (opus-4.5), Opus-C (opus-4.5)*
