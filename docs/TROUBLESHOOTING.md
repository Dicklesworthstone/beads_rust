# br Troubleshooting Guide

Common issues and solutions when using `br` (beads_rust).

---

## Table of Contents

- [Quick Diagnostics](#quick-diagnostics)
- [Workspace Failure Mode Catalog](#workspace-failure-mode-catalog)
- [Initialization Issues](#initialization-issues)
- [Issue Operations](#issue-operations)
- [Dependency Problems](#dependency-problems)
- [Sync & JSONL Issues](#sync--jsonl-issues)
- [Database Problems](#database-problems)
- [Configuration Issues](#configuration-issues)
- [Error Code Reference](#error-code-reference)
- [Debug Logging](#debug-logging)
- [Performance Issues](#performance-issues)
- [Agent Integration Issues](#agent-integration-issues)
- [Recovery Procedures](#recovery-procedures)

---

## Quick Diagnostics

Run these commands to diagnose common problems:

```bash
# Run the selftest first: it drives this binary through a full issue lifecycle
# in a throwaway directory on your filesystem (never your workspace) and prints
# a br.doctor.selftest.v1 receipt; a platform or filesystem problem shows up here
RUST_LOG=error br doctor --selftest --json

# Check workspace health
br doctor

# Show project statistics
br stats

# Check sync status
br sync --status

# Show configuration
br config list

# Show version
br version
```

---

## Workspace Failure Mode Catalog

This section is the canonical inventory of workspace-level failure states that
`br` is expected to survive, reject, quarantine, or repair. Use it before
improvising a recovery plan.

Not every failure mode below is a defect. Some entries describe deliberate
safety stops, where the correct behavior is to refuse a risky operation and
preserve evidence rather than guessing.

### How to use this catalog

1. Match the observed symptom to the closest failure class.
2. Check the listed observability surface before making changes.
3. Prefer the desired system response over ad hoc manual cleanup.
4. Treat higher-risk classes as evidence-preservation problems first and a
   convenience problem second.

### Database corruption and structural anomalies

| Failure class | Symptom signature | Likely root cause | Observability surface | Data-loss risk | Desired system response |
|---------------|-------------------|-------------------|-----------------------|----------------|-------------------------|
| Missing SQLite family with valid JSONL | `beads.db` is absent but `issues.jsonl` still exists; startup can proceed only after rebuild | Workspace copied without DB, manual deletion, interrupted cleanup, sidecar-only residue | Startup warnings, `br doctor`, `.beads/` directory listing | Low if JSONL is authoritative and recent | Rebuild SQLite from JSONL automatically or via explicit repair path; do not treat as fatal corruption by itself |
| Not-a-database / short-read DB file | Open fails with corruption-style errors such as `NotADatabase` or `ShortRead` | Truncated file, wrong file copied into `beads.db`, interrupted filesystem write | Startup error, `br doctor`, verbose logs, `src/config/mod.rs` recovery path | Medium to high depending on JSONL freshness | Preserve the original DB family in `.beads/.br_recovery/`, rebuild from JSONL, and surface the original open error if recovery also fails |
| Malformed schema / duplicate schema entries / index mismatch | DB opens or probes with messages like `malformed database schema`, `table ... already exists`, `index ... already exists`, or `missing from index` | Corrupt schema pages, failed migration-like writes, damaged catalog/index state | Startup probe, `br doctor`, recovery warnings, integrity checks | Medium | Quarantine the DB family, rebuild from JSONL, and preserve the malformed original for forensic follow-up |
| WAL / sidecar mismatch | Main DB exists but `-wal`, `-shm`, or `-journal` sidecars are stale or corrupted | Interrupted transaction, crash, partial copy of database family | Open failure, recovery warnings, presence of stale sidecars in `.beads/` | Medium | Move the whole database family into recovery together and rebuild atomically, rather than cherry-picking only `beads.db` |
| Partially recoverable row-level corruption | Reads and `doctor` may succeed, but writes against certain rows fail with corruption-like or downstream constraint errors | Localized page/index corruption, inconsistent row/index state | Targeted mutation failures, repro tests such as row-specific update failures, verbose logs | Medium to high if writes are retried blindly | Detect as recoverable corruption, rebuild from JSONL, then retry the mutation once against the repaired DB instead of persisting partial state |

### JSONL integrity and sync drift

| Failure class | Symptom signature | Likely root cause | Observability surface | Data-loss risk | Desired system response |
|---------------|-------------------|-------------------|-----------------------|----------------|-------------------------|
| Merge conflict markers in JSONL | Import fails with conflict marker diagnostics; file contains `<<<<<<<`, `=======`, `>>>>>>>` | Unresolved git merge on `.beads/issues.jsonl` | `br sync --import-only`, `br doctor`, direct file inspection | High if imported blindly | Reject import unconditionally; require manual conflict resolution before any DB mutation |
| Malformed JSONL lines | Import or doctor reports parse errors on one or more lines | Manual edit mistake, truncated write, external tool damage | `br doctor`, `br sync --import-only`, JSON parser errors, line-numbered diagnostics | Medium | Refuse import, preserve the original file, and require line-level repair rather than best-effort partial mutation |
| Stale DB relative to JSONL | Export refuses with stale-database language because JSONL contains issues missing from SQLite | Git pull/import not run yet, external JSONL edit, DB drift | `br sync --status`, export guard errors, doctor metadata checks | High if export proceeds | Refuse destructive export unless the operator explicitly chooses `--force`; preferred path is import-first |
| Empty DB vs non-empty JSONL | Export sees zero DB issues while JSONL already has data | Wrong DB target, accidental DB reset, missing import after workspace copy | Export guard, `br sync --status`, `br stats`, `.beads/` inspection | High if empty export overwrites JSONL | Stop export by default; require import or an explicit `--force` acknowledgement |
| Prefix mismatch / mixed prefixes | Import rejects with prefix mismatch or mixed project IDs | Wrong workspace, copied JSONL from another project, prefix drift after rename | Import preflight, `br doctor`, `br config get id.prefix`, JSONL inspection | Medium | Refuse import by default, surface the expected vs observed prefix, and only allow override when the operator intentionally wants remapping/repair |
| JSONL-only write false negative | A `--no-db` write persists to JSONL and then still returns an error such as a bogus primary-key failure | Write-path bug in JSONL-only/in-memory flow, duplicate post-write validation, race in finalization | Command exit code vs actual JSONL contents, repro tests, follow-up reads | Medium because automation may retry a write that already succeeded | Report success when the write succeeded, keep genuine duplicate/conflict protection, and add regression coverage for create/comment/dependency paths |

### Metadata, routing, and configuration drift

| Failure class | Symptom signature | Likely root cause | Observability surface | Data-loss risk | Desired system response |
|---------------|-------------------|-------------------|-----------------------|----------------|-------------------------|
| Wrong workspace discovered | Commands report `NOT_INITIALIZED` or operate on an unexpected `.beads/` tree | Running from the wrong cwd, stale `BEADS_DIR`, incorrect `--db`, ancestor discovery surprise | `br where`, `br config list -v`, resolved path output, env inspection | Medium | Surface the effective paths before mutation and prefer explicit path/DB selection over silent fallback |
| DB/JSONL target drift | DB and JSONL refer to different workspaces or one target moved independently | External path overrides, copied `.beads/` trees, stale config or metadata | `br sync --status`, doctor metadata checks, config output | Medium to high | Detect and report path disagreement before mutation; require the operator to reconcile the intended authoritative target |
| Missing or stale metadata after recovery | Commands work, but prefix or export metadata is absent/stale after rebuild/import | Rebuild path recreated core tables but not all metadata yet, interrupted export/import | Doctor metadata checks, startup config resolution, sync status | Low to medium | Rehydrate metadata from config/JSONL/project naming rules and report that an external import or recovery is pending |
| Ambient env or legacy config leakage | Behavior changes unexpectedly between shells or hosts | Inherited `BD_DB`, `BD_DATABASE`, `BEADS_JSONL`, legacy config files, user-level config precedence | `br config list -v`, `env`, non-hermetic smoke tests | Medium | Show source-aware config diagnostics and make it obvious which layer won, rather than silently forcing defaults |

### Lifecycle interruption and recovery artifacts

| Failure class | Symptom signature | Likely root cause | Observability surface | Data-loss risk | Desired system response |
|---------------|-------------------|-------------------|-----------------------|----------------|-------------------------|
| Interrupted export/import | Operation exits mid-flight; temp or backup artifacts remain | Crash, kill signal, disk-full, remote fs hiccup | Verbose logs, `.beads/.br_history/`, sync status, temp files | Medium | Use atomic temp-file + rename semantics so the last committed JSONL stays valid; leave artifacts as evidence instead of silently deleting them |
| Failed automatic rebuild from JSONL | Startup attempts recovery but repair also fails | JSONL itself is invalid, prefix mismatch, recovery restore failure, deeper disk corruption | Startup warnings, `.beads/.br_recovery/`, structured error context | High | Preserve both the original DB family and any failed rebuild outputs, then surface the richer recovery error rather than hiding it |
| Partial temp-file or backup cleanup | Recovery/history directories accumulate stale files after failed or interrupted operations | Interrupted rename sequence, manual restoration attempt, repeated failed rebuilds | `.beads/.br_recovery/`, `.beads/.br_history/`, filesystem inspection | Low direct risk, medium operator confusion | Prefer retaining artifacts over deleting them automatically; document how to inspect and prune only after the workspace is healthy |
| Crash during mutating no-db workflow | Command may have updated JSONL but not all follow-up validation/reporting steps completed | In-memory/JSONL-only mutation path interrupted after persistence | Exit code mismatch, JSONL diff, follow-up read commands | Medium | Make post-write finalization idempotent and ensure the user can distinguish “state changed” from “state uncertain” without re-applying the mutation blindly |

### Multi-actor contention and environment interference

| Failure class | Symptom signature | Likely root cause | Observability surface | Data-loss risk | Desired system response |
|---------------|-------------------|-------------------|-----------------------|----------------|-------------------------|
| Database locked / concurrent writer | Mutating command fails or waits on lock acquisition | Multiple agents or shells writing the same workspace simultaneously | Lock timeout errors, verbose logs, active process list | Low to medium | Fail or retry cleanly; never reinterpret a lock as corruption, and keep the operator-visible error distinct from recovery flows |
| Interleaved read/write staleness | One actor reads stale DB state while another updated JSONL or performed import/export | Missing import before read, overlapping sessions, long-lived processes | `br sync --status`, auto-import warnings, surprising ready/list results | Medium | Prefer import-before-read on commands that need freshness and keep stale-export guards enabled |
| Existing-workspace assumptions hidden by hermetic tests | Commands work in fresh tempdirs but fail in long-lived or ambient-env workspaces | Test harness isolates env too aggressively, latent dependency on preexisting files/config | Non-hermetic smoke runs, field repros, ambient-env regressions | Medium | Keep a lightweight smoke profile against existing workspaces and preserve selected ambient env variables in regression coverage |
| Multiple agents sharing one workspace with different local state | Different shells see different config/env resolution and reach different conclusions about safety | Divergent `HOME`, config files, env overrides, manually edited `.beads/` artifacts | `br config list -v`, shell env, agent repro transcripts | Medium | Make path/config provenance explicit in diagnostics so multi-actor sessions converge on the same effective workspace before mutating it |

### Observability cheat sheet

Use these surfaces first, before manual repair:

- `br doctor`: workspace health, schema checks, metadata drift, JSONL parse/conflict checks
- `br sync --status`: stale/empty export guard conditions and import/export pending state
- `br config list -v`: effective configuration plus the source layer that won
- `br where`: resolved workspace/database paths
- Verbose logs (`-v`, `-vv`, `RUST_LOG=debug`): startup recovery, path validation, and sync preflight decisions
- `.beads/.br_recovery/`: quarantined database families preserved during automatic rebuild
- `.beads/.br_history/`: JSONL backup history preserved during export/restore flows

---

## Initialization Issues

### "Beads not initialized: run 'br init' first"

**Error Code:** `NOT_INITIALIZED` (exit code 2)

**Cause:** No beads workspace found in current directory or ancestors.

**Solution:**
```bash
# Initialize new workspace
br init

# Initialize with custom prefix
br init --prefix myproj
```

**Verification:**
```bash
ls -la .beads/
# Should show: beads.db, issues.jsonl, beads.yaml
```

---

### "Already initialized at '...'"

**Error Code:** `ALREADY_INITIALIZED` (exit code 2)

**Cause:** Attempting to initialize in a directory that already has a beads workspace.

**Solution:**
```bash
# Reinitialize (caution: resets database!)
br init --force

# Or work with existing workspace
br list
```

---

### Database created in wrong location

**Cause:** `br init` was run in wrong directory, or `.beads/` was moved.

**Solution:**
```bash
# Check current location
br config path

# Move to correct directory
cd /correct/path
br init
```

---

## Issue Operations

### "Issue not found: bd-xyz"

**Error Code:** `ISSUE_NOT_FOUND` (exit code 3)

**Cause:** Issue ID doesn't exist or was mistyped.

**Solutions:**

```bash
# List all issues to find correct ID
br list

# Use partial ID matching
br show abc  # Matches bd-abc123

# Search by title
br search "keyword"

# Check if deleted (tombstoned)
br list -a --json | jq '.issues[] | select(.status == "tombstone")'
```

**JSON error provides hints:**
```json
{
  "error": {
    "code": "ISSUE_NOT_FOUND",
    "hint": "Did you mean 'bd-abc123'?",
    "context": {
      "searched_id": "bd-abc12",
      "similar_ids": ["bd-abc123", "bd-abc124"]
    }
  }
}
```

---

### "Ambiguous ID 'bd-ab': matches 3 issues"

**Error Code:** `AMBIGUOUS_ID` (exit code 3)

**Cause:** Partial ID matches multiple issues.

**Solution:**
```bash
# Provide more characters
br show bd-abc1  # More specific

# List matches to see full IDs
br list --id bd-ab
```

---

### "Invalid priority: high"

**Error Code:** `INVALID_PRIORITY` (exit code 4)

**Cause:** Priority must be numeric (0-4) or P-notation (P0-P4).

**Solution:**
```bash
# Use numeric priority
br create "Task" -p 1   # High priority

# Or P-notation
br create "Task" -p P2  # Medium priority

# Priority meanings:
# 0 (P0) = critical
# 1 (P1) = high
# 2 (P2) = medium (default)
# 3 (P3) = low
# 4 (P4) = backlog
```

**Common synonym mappings:**
| Input | Maps to |
|-------|---------|
| high, important | 1 |
| medium, normal | 2 |
| low, minor | 3 |
| critical, urgent | 0 |
| backlog, trivial | 4 |

---

### "Invalid status: done"

**Error Code:** `INVALID_STATUS` (exit code 4)

**Cause:** Invalid status value provided.

**Valid statuses:**
- `open` - Ready for work
- `in_progress` - Currently being worked on
- `blocked` - Waiting on dependencies
- `deferred` - Postponed
- `closed` - Completed

**Common synonym mappings:**
| Input | Maps to |
|-------|---------|
| done, complete, finished | closed |
| wip, working, active | in_progress |
| new, todo, pending | open |
| hold, later, postponed | deferred |

**Solution:**
```bash
# Use valid status
br update bd-123 -s in_progress

# Or use close command
br close bd-123  # Instead of --status closed
```

---

### "Invalid issue type: story"

**Error Code:** `INVALID_TYPE` (exit code 4)

**Cause:** Invalid issue type value.

**Valid types:**
- `task` - General work item
- `bug` - Defect to fix
- `feature` - New functionality
- `epic` - Large grouping of related issues
- `chore` - Maintenance work
- `docs` - Documentation
- `question` - Discussion item

**Common synonym mappings:**
| Input | Maps to |
|-------|---------|
| story, enhancement | feature |
| issue, defect | bug |
| ticket, item | task |
| documentation, doc | docs |
| cleanup, refactor | chore |

---

### "Validation failed: title: cannot be empty"

**Error Code:** `VALIDATION_FAILED` (exit code 4)

**Cause:** Required field missing or invalid.

**Solution:**
```bash
# Provide required title
br create "My task title"

# Check what fields are required
br create --help
```

---

## Dependency Problems

### "Cycle detected in dependencies: bd-123 -> bd-456 -> bd-123"

**Error Code:** `CYCLE_DETECTED` (exit code 5)

**Cause:** Adding a dependency would create a circular reference.

**Solutions:**
```bash
# Find existing cycles
br dep cycles

# View dependency tree
br dep tree bd-123

# Remove problematic dependency
br dep remove bd-456 bd-123
```

**Prevention:**
- Use `br dep tree <id>` before adding dependencies
- Consider if relationship should be `related` instead of `blocks`

---

### "Issue cannot depend on itself: bd-123"

**Error Code:** `SELF_DEPENDENCY` (exit code 5)

**Cause:** Attempting to add self-referential dependency.

**Solution:**
```bash
# This is always an error - fix the command
br dep add bd-123 bd-456  # Different IDs
```

---

### "Cannot delete: bd-123 has 3 dependents"

**Error Code:** `HAS_DEPENDENTS` (exit code 5)

**Cause:** Issue has other issues depending on it.

**Solutions:**
```bash
# View what depends on it
br dep list bd-123

# Remove dependencies first
br dep remove bd-dependent bd-123

# Or force delete (cascades to dependents)
br delete bd-123 --force
```

---

### "Dependency target not found: bd-xyz"

**Error Code:** `DEPENDENCY_NOT_FOUND` (exit code 5)

**Cause:** The target issue in a dependency doesn't exist.

**Solution:**
```bash
# Verify issue exists
br show bd-xyz

# List to find correct ID
br list | grep xyz
```

---

### "Dependency already exists: bd-123 -> bd-456"

**Error Code:** `DUPLICATE_DEPENDENCY` (exit code 5)

**Cause:** Dependency between these issues already exists.

**Solution:**
```bash
# Check existing dependencies
br dep list bd-123

# If different type needed, remove and re-add
br dep remove bd-123 bd-456
br dep add bd-123 bd-456 --type related
```

---

## Sync & JSONL Issues

### "JSONL parse error at line 42: invalid JSON"

**Error Code:** `JSONL_PARSE_ERROR` (exit code 6)

**Cause:** Malformed JSON in the JSONL file.

**Diagnosis:**
```bash
# Check the specific line
sed -n '42p' .beads/issues.jsonl

# Validate JSON syntax
jq -c '.' .beads/issues.jsonl 2>&1 | head -20

# Find problematic lines
cat -n .beads/issues.jsonl | while read n line; do
  echo "$line" | jq '.' >/dev/null 2>&1 || echo "Line $n: Invalid"
done
```

**Solutions:**
```bash
# Capture current classification before changing the file
br doctor --json
br sync --status --json

# Manual fix: edit the malformed line(s)
$EDITOR .beads/issues.jsonl

# Or restore a known-good JSONL history entry after reviewing it
br history list
br history restore <backup>

# Validate and import only after the JSONL is parseable
jq -c '.' .beads/issues.jsonl >/dev/null
br sync --import-only --json
```

---

### "Prefix mismatch: expected 'proj', found 'bd'"

**Error Code:** `PREFIX_MISMATCH` (exit code 6)

**Cause:** JSONL contains issues with different prefix than configured.

**Solutions:**
```bash
# Check configured prefix
br config get id.prefix

# Import with force (if intentional)
br sync --import-only --force

# Or update config to match
br config set id.prefix=bd
```

---

### "Import collision: 5 issues have conflicting content"

**Error Code:** `IMPORT_COLLISION` (exit code 6)

**Cause:** Same issue IDs with different content in database and JSONL.

**Solutions:**
```bash
# Check sync status
br sync --status --json

# Inspect both sides before choosing an authority
br --no-auto-import --allow-stale list --json

# If JSONL is authoritative, import it explicitly
br sync --import-only --force

# If SQLite is authoritative, export it explicitly
br sync --flush-only --force
```

---

### "Conflict markers detected in JSONL"

**Error Code:** `CONFLICT_MARKERS` (exit code 6)

**Cause:** Git merge conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) in JSONL.

**Solution:**
```bash
# Find conflict markers
grep -n "^<<<<<<\|^======\|^>>>>>>" .beads/issues.jsonl

# Resolve manually
$EDITOR .beads/issues.jsonl

# Then import
br sync --import-only
```

---

### "Path traversal attempt blocked"

**Error Code:** `PATH_TRAVERSAL` (exit code 6)

**Cause:** JSONL path contains `..` or absolute path outside workspace.

**Solution:**
```bash
# Use default path
br sync --flush-only

# Or explicitly allow external path
br sync --flush-only --allow-external-jsonl
```

---

### Sync status shows "db_newer" but export fails

**Diagnosis:**
```bash
# Check for dirty issues
br list --json | jq '[.issues[] | select(.dirty)] | length'

# Check file permissions
ls -la .beads/issues.jsonl

# Check disk space
df -h .beads/
```

**Solutions:**
```bash
# Check file permissions
chmod 644 .beads/issues.jsonl

# Try with verbose logging
br sync --flush-only -vv
```

---

### "AUTO_FLUSH_FAILED" after a successful mutation

**Cause:** The command updated SQLite successfully, but the automatic JSONL
export that runs after mutating commands failed. The DB now contains newer data
than `.beads/issues.jsonl`, so committing `.beads/` without repairing the export
can commit stale issue state.

**Diagnosis:**
```bash
# Confirm the DB/JSONL relationship
br sync --status --json

# Inspect the export target and its parent directory
br where --json
ls -la .beads .beads/issues.jsonl
df -h .beads/
```

**Recovery:**
```bash
# Fix the reported filesystem/path/config problem first, then export explicitly
br sync --flush-only

# Confirm no export debt remains before committing
br sync --status --json
git add .beads/
```

If the warning came from a JSON or robot-mode command, the diagnostic is printed
to stderr as structured JSON so stdout remains parseable.

---

## Database Problems

### "Database is locked"

**Error Code:** `DATABASE_LOCKED` (exit code 2)

**Cause:** Another process has the database locked.

**Solutions:**
```bash
# Wait and retry with timeout
br list --lock-timeout 10000

# Identify the locking process, then coordinate with its owner
fuser .beads/beads.db

# Re-check workspace health after the writer finishes
br doctor --json
```

**Prevention:**
- Avoid running multiple br commands simultaneously
- Don't leave interactive sessions open
- Use `--lock-timeout` for agent workflows

---

### "Schema version mismatch: expected 5, found 3"

**Error Code:** `SCHEMA_MISMATCH` (exit code 2)

**Cause:** Database was created with older/newer br version.

**Solutions:**
```bash
# Check br version
br version

# Classify the workspace and projected repair
br doctor --json
br doctor --repair --dry-run --json

# Use the supported migration path if available
br upgrade --migrate-db

# Otherwise preserve evidence and escalate with the doctor output
br where --json
br config list -v
```

---

### "Database not found at '.beads/beads.db'"

**Error Code:** `DATABASE_NOT_FOUND` (exit code 2)

**Cause:** Database file doesn't exist at expected location.

**Solutions:**
```bash
# Initialize if new project
br init

# Check if moved
find . -name "beads.db" 2>/dev/null

# Import from JSONL
br sync --import-only
```

---

### Database corruption suspected

**Diagnosis:**
```bash
# Classify corruption and related sidecar/metadata state
br doctor --json

# Preview the repair plan without changing files
br doctor --repair --dry-run --json
```

**Recovery:**
```bash
# Let br preserve the DB family and rebuild only from valid JSONL
br doctor --repair --json

# Verify the repaired workspace before writing
br doctor --json
br sync --status --json
```

---

## Configuration Issues

### "Configuration error: invalid YAML"

**Error Code:** `CONFIG_ERROR` (exit code 7)

**Cause:** Invalid YAML syntax in config file.

**Solutions:**
```bash
# Check syntax
cat .beads/beads.yaml | python3 -c "import yaml,sys; yaml.safe_load(sys.stdin)"

# Find config paths
br config path

# Repair the YAML in place, then verify the effective config
$EDITOR .beads/beads.yaml
br config list -v
```

---

### Config values not taking effect

**Cause:** Config precedence issue (7 layers from defaults to CLI).

**Diagnosis:**
```bash
# Show effective config with sources
br config list -v

# Check specific value
br config get <key>

# Override via CLI
br --db /path/to/db list
```

**Config precedence (highest to lowest):**
1. CLI flags
2. Environment variables
3. Project config (`.beads/beads.yaml`)
4. User config (`~/.config/beads/config.yaml`)
5. Global config (`/etc/beads/config.yaml`)
6. Embedded defaults
7. Compiled defaults

---

## Error Code Reference

Quick reference for all error codes:

| Exit | Code | Category | Description |
|------|------|----------|-------------|
| 1 | `INTERNAL_ERROR` | Internal | Unexpected error |
| 2 | `DATABASE_NOT_FOUND` | Database | DB file missing |
| 2 | `DATABASE_LOCKED` | Database | DB in use |
| 2 | `SCHEMA_MISMATCH` | Database | Version mismatch |
| 2 | `NOT_INITIALIZED` | Database | No workspace |
| 2 | `ALREADY_INITIALIZED` | Database | Already init'd |
| 3 | `ISSUE_NOT_FOUND` | Issue | ID not found |
| 3 | `AMBIGUOUS_ID` | Issue | Partial match multiple |
| 3 | `ID_COLLISION` | Issue | Duplicate ID |
| 3 | `INVALID_ID` | Issue | Bad ID format |
| 4 | `VALIDATION_FAILED` | Validation | Field invalid |
| 4 | `INVALID_STATUS` | Validation | Bad status |
| 4 | `INVALID_TYPE` | Validation | Bad type |
| 4 | `INVALID_PRIORITY` | Validation | Bad priority |
| 5 | `CYCLE_DETECTED` | Dependency | Circular ref |
| 5 | `SELF_DEPENDENCY` | Dependency | Self-reference |
| 5 | `HAS_DEPENDENTS` | Dependency | Can't delete |
| 5 | `DEPENDENCY_NOT_FOUND` | Dependency | Target missing |
| 5 | `DUPLICATE_DEPENDENCY` | Dependency | Already exists |
| 6 | `JSONL_PARSE_ERROR` | Sync | Invalid JSON |
| 6 | `PREFIX_MISMATCH` | Sync | Wrong prefix |
| 6 | `IMPORT_COLLISION` | Sync | Content conflict |
| 6 | `CONFLICT_MARKERS` | Sync | Git conflict |
| 6 | `PATH_TRAVERSAL` | Sync | Bad path |
| 7 | `CONFIG_ERROR` | Config | Config problem |
| 8 | `IO_ERROR` | I/O | File error |

---

## Debug Logging

Enable debug output for detailed diagnostics:

```bash
# Basic verbose
br list -v

# Very verbose
br sync --flush-only -vv

# Full debug logging
RUST_LOG=debug br list 2>debug.log

# Trace level (very detailed)
RUST_LOG=trace br sync --flush-only 2>trace.log

# Module-specific logging
RUST_LOG=beads_rust::storage=debug br list

# Combine with JSON for parsing
RUST_LOG=debug br list --json 2>debug.log 1>issues.json
```

### Test Harness Logging (Conformance/Benchmark)

Conformance and benchmark tests can emit structured logs for CI parsing.

Enable with environment variables:

```bash
# JSONL event log of each br/bd run
CONFORMANCE_JSON_LOGS=1

# Summary report with br/bd timing ratios
CONFORMANCE_SUMMARY=1

# JUnit XML output for CI systems
CONFORMANCE_JUNIT_XML=1

# Failure context dump (stdout/stderr previews + .beads listing)
CONFORMANCE_FAILURE_CONTEXT=1
```

Outputs are written under the test workspace `logs/` directory:

```
conformance_runs.jsonl
conformance_summary.json
conformance_junit.xml
<label>.failure.json  (only on failure)
```

---

## Performance Issues

### Slow list/query operations

**Diagnosis:**
```bash
# Check issue count
br count

# Check database size
du -h .beads/beads.db
```

**Solutions:**
```bash
# Use limit
br list --limit 50

# Use specific filters
br list -s open -t bug

# Check whether derived-state repair is needed
br doctor --json
br doctor --repair --dry-run --json
```

---

### Slow sync operations

**Diagnosis:**
```bash
# Check dirty count
br sync --status --json | jq '.dirty_count'

# Check JSONL size
du -h .beads/issues.jsonl
wc -l .beads/issues.jsonl
```

**Solutions:**
```bash
# Flush only dirty issues (default)
br sync --flush-only

# For large imports, use progress
br sync --import-only -v
```

---

### Memory usage concerns

```bash
# Monitor during operation
/usr/bin/time -v br list --limit 0

# For very large databases
# Use incremental operations
br list --limit 100
br list --limit 100 --offset 100
```

---

## Agent Integration Issues

### JSON parsing errors

**Cause:** Mixing human output with JSON mode.

**Solution:**
```bash
# Always use --json for programmatic access
br list --json

# Suppress stderr if needed
br list --json 2>/dev/null

# Check exit code
br list --json || echo "Failed with code $?"
```

---

### Concurrent access conflicts

**Cause:** Multiple agents accessing database simultaneously.

**Solutions:**
```bash
# Use lock timeout
br update bd-123 --claim --lock-timeout 5000

# Retry on failure
for i in 1 2 3; do
  br list --json && break
  sleep 1
done
```

---

### Actor not being recorded

**Cause:** `BD_ACTOR` not set.

**Solution:**
```bash
# Set actor for audit trail
export BD_ACTOR="claude-agent"

# Or per-command
br --actor "my-agent" update bd-123 --claim
```

---

## Recovery Procedures

Recovery starts with classification and evidence preservation. Do not delete,
rename, or partially overwrite `.beads/` files as a first response. The repair
path should either prove that JSONL is authoritative and rebuild from it, or
stop and preserve the evidence needed for manual repair.

### Capture the incident bundle

One command collects everything below into a redacted archive (add
`--include-db` / `--include-jsonl` to attach the database bytes and
`issues.jsonl`; see `docs/reliability/HEALTH_CONTRACT.md` for the member
list):

```bash
br doctor --bundle /tmp/incident-$(date +%Y%m%d-%H%M%S).tar.gz --json
```

If `br` itself cannot run, capture by hand instead. Run these commands
before attempting repair:

```bash
br doctor --json
br doctor --repair --dry-run --json
br sync --status --json
br where --json
br config list -v
```

Also preserve the failing command, exact stdout/stderr, `.beads/issues.jsonl`,
`.beads/metadata.json`, directory listings for `.beads/`, `.beads/.br_recovery/`,
and `.beads/.br_history/`, plus the presence and hashes of `beads.db`,
`beads.db-wal`, `beads.db-shm`, and `beads.db-journal` when present.

### Recoverable database family with valid JSONL

Use this when `br doctor` classifies the DB family as recoverable and JSONL is
parseable, conflict-free, and pointed at the intended workspace.

```bash
# Preview what repair will do
br doctor --repair --dry-run --json

# Execute the supported repair path
br doctor --repair --json

# Verify health and freshness before mutating issues
br doctor --json
br sync --status --json
br list --json
```

Expected behavior: `br` preserves the original DB family under
`.beads/.br_recovery/`, rebuilds from valid JSONL, and verifies the repaired
workspace instead of asking the operator to remove individual database files.

### Unsafe JSONL or merge-conflict state

Use this when JSONL contains conflict markers, malformed lines, mixed prefixes,
or data from the wrong workspace. Automatic import/rebuild is not safe until the
interchange file is corrected.

```bash
# Find conflict markers or invalid JSONL
grep -n "^<<<<<<\|^======\|^>>>>>>" .beads/issues.jsonl
jq -c '.' .beads/issues.jsonl >/dev/null

# Edit the JSONL to one valid record per line, with the intended prefix
$EDITOR .beads/issues.jsonl

# Re-validate and import after the file is unambiguous
jq -c '.' .beads/issues.jsonl >/dev/null
br sync --import-only --json
br doctor --json
```

Do not use raw `git checkout --ours` or `git checkout --theirs` as a recovery
shortcut unless the operator has already decided which side is authoritative and
has accepted the data-loss consequence. The safer default is line-level conflict
resolution followed by import.

### SQLite/JSONL drift authority decision

Use this when both sides are readable but disagree.

```bash
# Classify freshness and path metadata first
br sync --status --json
br where --json
br config list -v

# If JSONL is authoritative
br sync --import-only --force

# If SQLite is authoritative
br sync --flush-only --force

# Verify after the chosen direction succeeds
br sync --status --json
br doctor --json
```

The important part is the explicit direction choice. Do not run a force flag as
a generic fix; it is an assertion about which side should win.

### JSONL-only emergency read path

`--no-db` is useful when the SQLite family is unavailable and the operator needs
to inspect valid JSONL, but it is not the normal recovery path for writes.

```bash
br --no-db list --json
br --no-db show <issue-id> --json
br --no-db ready --json
```

After inspection, return to the supported repair/import/export flows above.
Avoid repeating JSONL-only writes after a partial failure unless the previous
command result has been reconciled against the JSONL contents.

---

## Getting Help

If you're still stuck:

1. **Check documentation:**
   - [CLI_REFERENCE.md](CLI_REFERENCE.md)
   - [AGENT_INTEGRATION.md](AGENT_INTEGRATION.md)
   - [ARCHITECTURE.md](ARCHITECTURE.md)

2. **Run diagnostics:**
   ```bash
   br doctor
   br version
   br config list
   ```

3. **Enable debug logging:**
   ```bash
   RUST_LOG=debug br <command> 2>debug.log
   ```

4. **Check for updates:**
   ```bash
   br upgrade --check
   ```

---

## See Also

- [CLI_REFERENCE.md](CLI_REFERENCE.md) - Complete command reference
- [AGENT_INTEGRATION.md](AGENT_INTEGRATION.md) - AI agent integration
- [ARCHITECTURE.md](ARCHITECTURE.md) - Technical architecture
- [SYNC_SAFETY.md](SYNC_SAFETY.md) - Sync safety model
