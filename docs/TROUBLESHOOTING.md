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
| Older database schema | Read commands warn that they are using a JSONL view, or refuse a storage open; writes report an unsupported or refused migration | Workspace last used with an earlier br release | `br info --json --no-auto-import --no-auto-flush`, `br doctor migrate-schema heal --dry-run --json`, migration plan | High if JSONL is mistaken for a complete backup | Keep the original database active. Apply only an eligible reviewed migration; missing paths and failed preservation checks refuse without reconstructing from JSONL |
| v0.7.6 schema-19 layout regression (#596) | Reads refuse an incompatible runtime layout even though the release and main both report schema 19 | Main added indexed JSONL receipts and nullable close attribution without changing `user_version`; the former heal probe checked only that version | `br doctor --json`, `br doctor migrate-schema plan --json`, `br doctor migrate-schema heal --dry-run --json` | High if JSONL replay is used as a substitute for preserving database-only state | Require the explicit reviewed layout upgrade; preserve raw records and the complete original family, and use guarded undo for a compatible rollback |
| Missing SQLite family with valid JSONL | `beads.db` is absent but `issues.jsonl` still exists | Workspace copied without DB, manual deletion, interrupted cleanup, sidecar-only residue | Startup refusal, `br doctor`, `.beads/` directory listing | Depends on whether surviving sidecars or other evidence contain additional committed state | Reads must not create a database. Use `--no-db` for an intentional JSONL-only view; inspect surviving evidence before any authorized recovery |
| Not-a-database / short-read DB file | Open fails with corruption-style errors such as `NotADatabase` or `ShortRead` | Truncated file, wrong file copied into `beads.db`, interrupted filesystem write | Startup error, `br doctor`, verbose logs, `src/config/mod.rs` recovery path | High when database-only state cannot be inspected | Reads refuse and leave the family unchanged. Preserve the whole family before explicit recovery; valid JSONL alone does not prove that events or configuration can be discarded |
| Malformed schema / duplicate schema entries / index mismatch | DB opens or probes with messages like `malformed database schema`, `table ... already exists`, `index ... already exists`, or `missing from index` | Corrupt schema pages, failed migration-like writes, damaged catalog/index state | Startup probe, `br doctor`, recovery warnings, integrity checks | Medium to high | Refuse writable fallback from an ordinary read. Diagnose the original family and require a repair that retains authoritative records in the active database |
| WAL / sidecar mismatch | Main DB exists but `-wal`, `-shm`, or `-journal` sidecars are stale or corrupted | Interrupted transaction, crash, partial copy of database family | Open failure, recovery warnings, presence of stale sidecars in `.beads/` | High if a sidecar contains committed records absent from the main file | Keep the database and sidecars together. Use the explicit guarded recovery path when indicated; do not delete sidecars or rebuild from JSONL to make a read succeed |
| Partially recoverable row-level corruption | Reads and `doctor` may succeed, but writes against certain rows fail with corruption-like or downstream constraint errors | Localized page/index corruption, inconsistent row/index state | Targeted mutation failures, repro tests such as row-specific update failures, verbose logs | Medium to high if writes are retried blindly | Preserve evidence and require a verified repair before retry. A readable JSONL export and a recovery backup do not establish that all database-only state survived |

An older schema is not permission to replace the database. Issue JSONL does not
fully contain event history, configuration, closure attribution, workflow and
capacity records, allocation reservations, or operator extensions. Keeping those
records only in `.br_recovery` removes them from normal operations and is a
preservation failure ([#585](https://github.com/Dicklesworthstone/beads_rust/issues/585)).

Start with `br doctor migrate-schema heal --dry-run --json` and
`br doctor migrate-schema plan --json`. If the plan is eligible, review it before
running `br doctor migrate-schema apply --plan-token <TOKEN>`. An ineligible plan
requires a supporting migration or operator-led recovery that preserves the
original; neither `heal` nor `--discard-db-only` bypasses that requirement.
Warnings about a private JSONL view mean only the records represented in JSONL
are visible in that command. The original database has not been upgraded.

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
| Missing or stale metadata after recovery | Commands work, but prefix or export metadata is absent/stale after rebuild/import | Rebuild path recreated core tables but not all metadata yet, interrupted export/import | Doctor metadata checks, startup config resolution, sync status | Low to medium | Report missing metadata observationally; restore it only through an authorized write or verified reconciliation, preserving existing non-default configuration |
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
| Interleaved read/write staleness | A read reports JSONL changes awaiting import, or an explicitly stale view omits peer changes | Missing import before read, overlapping sessions, long-lived processes | `br sync --status`, `SYNC_CONFLICT`, explicit stale-view flags | Medium | Reads remain non-mutating. Use `br sync --import-only` to import, or `--allow-stale` / `--no-auto-import` to inspect the existing database; keep stale-export guards enabled |
| Existing-workspace assumptions hidden by hermetic tests | Commands work in fresh tempdirs but fail in long-lived or ambient-env workspaces | Test harness isolates env too aggressively, latent dependency on preexisting files/config | Non-hermetic smoke runs, field repros, ambient-env regressions | Medium | Keep a lightweight smoke profile against existing workspaces and preserve selected ambient env variables in regression coverage |
| Multiple agents sharing one workspace with different local state | Different shells see different config/env resolution and reach different conclusions about safety | Divergent `HOME`, config files, env overrides, manually edited `.beads/` artifacts | `br config list -v`, shell env, agent repro transcripts | Medium | Make path/config provenance explicit in diagnostics so multi-actor sessions converge on the same effective workspace before mutating it |

### Observability cheat sheet

Use these surfaces first, before manual repair:

- `br doctor`: workspace health, schema checks, metadata drift, JSONL parse/conflict checks
- `br sync --status`: stale/empty export guard conditions and import/export pending state
- `br config list -v`: effective configuration plus the source layer that won
- `br where`: resolved workspace/database paths
- Verbose logs (`-v`, `-vv`, `RUST_LOG=debug`): startup recovery, path validation, and sync preflight decisions
- `.beads/.br_recovery/`: retained database families and receipts from recovery or reviewed migrations; inspect these as evidence, not as proof that the active database contains every original record
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
- Use `--lock-timeout` to bound writer admission waits in agent workflows.
- Investigate the owning process before retrying a persistently contended write.
- Preserve live lock files: removing `.write.lock` can let two processes hold
  different lock inodes and invalidate writer exclusion.

### A read command is holding `.write.lock` (#586)

Workspace writer authority must cover writes and their teardown, but must end
before a read command starts its query or renders output. A slow search or a
blocked stdout pipe must not extend that critical section.

Ordinary reads open a coherent private snapshot of the current database. If an
enabled freshness probe finds changed JSONL, the command returns `SYNC_CONFLICT`
with `br sync --import-only` guidance. It does not join the writer queue or
promote the read into an import. `--allow-stale` or `--no-auto-import` selects
the existing database view. Receipt-format maintenance alone does not require
synchronization for a read, including after a release-layout upgrade.

The command captures the database family and rechecks original identities,
metadata and contents. It verifies the private copy against captured digests
before namespace admission or engine access. Private WAL recovery, integrity
checks and runtime-schema checks must succeed before query execution. The
result describes that captured snapshot; a later writer's changes appear in a
later command. A failed freshness or snapshot probe never authorizes a writable
retry against the original family.

The same read intent applies to `br delete --dry-run`, including IDs routed to
another workspace. A preview does not acquire the destination's write lock or
run automatic import/export there. A slow or blocked preview output must not
hold up destination writers. Actual routed deletions still acquire authority
before planning and retain it through their writes and teardown.

`br create --dry-run` and Markdown-import previews also leave pending exports
untouched. They do not flush dirty database records, create a missing JSONL
snapshot, publish history, or update the last-touched ID. The pending changes
remain available for a later real mutation or explicit synchronization.

If startup discovers a pending merge, automatic synchronization is disabled
immediately. Its warning is emitted after read startup, so a full stderr pipe
cannot make that warning hold up other writers.

Tracing and warnings about unknown policy keys to stderr are buffered during
read startup, including diagnostics from connection teardown. Policy warnings
remain visible even with logging disabled; warning delivery cannot grant writer
authority. An error or interruption releases the retained
storage and lock owners before the buffer is drained. A full stderr pipe can
therefore leave the read waiting to deliver diagnostics after startup, while
another writer proceeds. The buffer retains
the first and most recent 128 KiB, up to 256 KiB total; if it fills, a notice
reports omitted middle bytes while preserving the startup and completion details.
Timestamps refer to when events occurred, so the delayed output can be used to
reconstruct startup without treating delivery time as lock-hold time.
An interruption received while those diagnostics drain is checked before query
dispatch. A cancelled read does not start import or produce query output.

Mutating startup's automatic import refuses a torn WAL or a WAL index that needs live recovery.
A readable private snapshot is not permission to repair the original family.
Inspect the reported condition with `br doctor --json` and use the applicable
reviewed recovery procedure. Healthy WAL-backed databases remain eligible for
automatic import.
This refusal also applies on native Unix platforms where Beads cannot perform
its verified quarantine procedure; unavailable recovery is not evidence that
the live index is safe for a writable open.

These boundaries keep normal writers serialized, preserve sole-opener checkpoint
admission, and do not let database replacement change an in-flight query.
Incomplete recovery, changed database routing or identity, and an unavailable
safe read snapshot cause an error before query execution. There is no fallback
that executes the read while retaining writer authority. Explicit sync and
automatic import during a mutation retain authority through their writes and
writable teardown.

An unfinished namespace transition also remains a refusal if its companion
gate file is missing. A read validates surviving transition evidence inside its
private copy before querying it; it does not discard the evidence because the
sidecar pair is incomplete. Preserve the original database family and inspect
it with `br doctor` before retrying. Healthy partial namespace layouts remain
readable; missing one sidecar alone is not diagnosed as unfinished recovery.

`--lock-timeout` bounds acquisition waits, not the duration of an import,
checkpoint, filesystem I/O, query, or output operation. A mutating command's
startup write can still occupy the lock while it completes safely. Increasing the
timeout does not fix an excessively long holder. The protected import recheck
also remains under authority because its decision permits a subsequent write;
the final snapshot's private hashing, recovery, and validation do not. To read the
current database even when JSONL has changes awaiting import, use:

```bash
br --allow-stale search QUERY --all --json
```

Stale-view flags do not make JSONL equivalent to the current database. `--no-db`
reads JSONL, which may omit database-only records. Older-schema issue queries
can use that view only with the existing explicit warning; diagnostic commands
inspect the original through their legacy-schema readers. Read commands never
authorize migration or repair, even with `BR_DISABLE_READ_ONLY_FAST_OPEN=1`.
Use the reviewed migration or explicit recovery commands when required.

**Interpreting the reported incident.** Issue #586 tested 0.5.7 at `c807e36`.
Healthy fast-open searches did not acquire `.write.lock`; the controlled case
disabled fast-open on the reader and demonstrated a writer exhausting its
three-second admission budget. It established lock ownership during startup or
execution, not that the search query had already begun. The separate reported
ten-minute production hold did not identify its startup trigger.

At that revision, plausible routes into writable startup included the debugging
override, an older or incomplete runtime schema, a read-only engine-open miss
(including namespace permission repair), startup recovery, and a positive or
failed automatic-import probe. These are source-derived candidates, not a
diagnosis of the production incident. Current read-intent protection refuses
repair, migration and automatic-import escalation. The defensive handoff from
#586 also ends any retained authority before opening the private query copy.

When investigating a new incident, capture the binary revision, flags and
fast-open override, verbose startup logs, the holder's PID and lock descriptors,
and a bounded scratch-only reproduction. Separate time waiting for authority
from time holding it, and distinguish startup from query/output. Total command
wall time alone does not show a workspace-lock wait. Keep the database, WAL,
sidecars, JSONL, and recovery evidence intact.

---

### "Schema version mismatch" or incompatible runtime layout

**Error Code:** `SCHEMA_MISMATCH` (exit code 2), or `CONFIG_ERROR` (exit code 7)
for an incompatible layout.

**Cause:** The executable does not support the database's version or actual
table layout. Matching version stamps do not prove compatibility.

**Solutions:**

```bash
# Check br version
br version

# Inspect without changing the original tracker
br doctor --json
br doctor migrate-schema plan --json > migration-plan.json
br doctor migrate-schema heal --dry-run --json

# Review the eligible plan and apply its exact token
br doctor migrate-schema apply \
  --plan-token "$(jq -r .plan_token migration-plan.json)" \
  --json > migration-applied.json

# Otherwise preserve evidence and escalate with the doctor output
br where --json
br config list -v
```

**v0.7.6 and schema 19 (#596).** Official v0.7.6 creates a schema-19 database
without `jsonl_row_receipts` or `close_metadata.closed_by`. Main added these
structures in
[`6b8fd6c9`](https://github.com/Dicklesworthstone/beads_rust/commit/6b8fd6c90d1ac2cc746f2dc07e14aa2a11ae8d03)
and
[`dc46e68b`](https://github.com/Dicklesworthstone/beads_rust/commit/dc46e68bf77d0582cb3fcd8f0908319a98a9bc16)
without changing the version. Earlier healing incorrectly treated version
equality as a no-op. The reviewed transition identifies those layout changes
independently and keeps `user_version` at 19.

Plan and heal preview now inspect the same database generation and report the
same migration eligibility and token. Ordinary reads never perform the
upgrade, and ordinary writes, including `sync --import-only`, refuse to install
these additions. Use the explicit plan/apply lifecycle above, or
`br doctor migrate-schema heal --json` after reviewing the preview. A failed
supplementary JSONL audit does not invalidate an eligible raw-record migration.

The migration preserves database-only event history, configuration, metadata,
workflow and capacity records, relations, dirty/export bookkeeping, and
operator tables. It checks raw values and storage classes, row multiplicities,
application/operator table hidden row IDs, sequence names/high-water values,
and admitted schema. New plans bind these checks in
`forecast.schema19_preservation`, using profile
`schema19-layout-preservation-v3` and `source_sha256`. A change confined to
application row IDs, otherwise indistinguishable non-UTF-8 TEXT bytes, or SQL
identifier quoting invalidates the token. Duplicate effective `sqlite_sequence`
names refuse before planning because their row order can change allocation;
the migrator does not discard or merge reservations. Existing closure attribution and
receipts remain exact; absent structures are added with unknown closers as
NULL and no invented receipts. JSONL and configuration files stay unchanged.

The complete original DB, WAL, SHM and rollback journal remain in the reported
recovery bundle. Pending merge metadata, even empty or malformed, refuses the
layout transition until the merge is completed or diagnosed against its
original schema. Do not delete the metadata to make migration eligible.
Unrecognized layouts and failed preservation checks require diagnosis.

**Ordinary maintenance refuses other current-version layout differences.**
Even when both schema-19 additions are present, ordinary writes cannot rebuild a
core or workflow table with extra columns, changed constraints, missing
authoritative tables, or unrecognized indexes. A missing current-version
`issues` table is not a new empty tracker. Keep the original database family
and use the diagnostic commands above; a reviewed decision must account for
the unsupported state before any replacement.

Generic `br doctor --repair` also refuses an unsupported current-version
layout when its schema metadata can be read. The refusal precedes fixer
dispatch and repair-session creation, including dry runs, fixer filters and
`--unsafe-auto-fix`. A failed storage open cannot turn unknown columns or
workflow records into permission to reconstruct the tracker from JSONL.
Preserve the database family and diagnose the reported layout difference;
unrelated row corruption does not authorize discarding recognized extra state.
For missing owned indexes and the narrowly admitted cache layouts below,
doctor uses dedicated transactional maintenance and reports the remaining
findings. That invocation does not continue into JSONL reconstruction, even
if another diagnostic still reports an error. If maintenance cannot complete,
the command stops; failure does not authorize replacement either. A dry run
identifies the same narrow maintenance path without applying it.

An otherwise admitted layout may restore missing br-owned indexes in place,
with admission, repair and the final layout check in one write transaction.
This preserves existing configuration and metadata rows, their row IDs, and
duplicate-key precedence. A required index name occupied on another table
refuses instead of altering that operator's object. Current-version index
restoration does not normalize stored issue values or remove legacy-named
operator indexes. The known nullable blocked-cache layout can still be
repaired, but an unfamiliar cache column or constraint is not permission to
discard it. A foreign key from another table into the cache prevents its
replacement. When the cache is recreated, its derived freshness marker becomes
stale across duplicate marker rows, so reads compute blocking from the retained
dependencies instead of treating the new empty cache as complete. Marker row
IDs and unrelated metadata remain intact. An intentional data reset likewise
refuses foreign keys from retained tables into the reset set before dropping
any table.

After success, a fresh plan reports no work for the healthy canonical layout;
a consumed token refuses. The same token can resume an interrupted run after
the recovery protocol verifies its live and retained generations. New plans
carry the stronger source proof; earlier installed receipts retain their
original semantics for guarded recovery and undo.

If an upgrade or undo is interrupted, finish the explicit recovery command
reported by the retained receipts before ordinary database operations. A new
`apply` cannot supersede an unfinished undo, even if the original main file has
already been restored. `info` remains available for paths and engine details;
its `database_inspection_error` explains why database-derived details are
omitted. `config get` and `config list` refuse while that database generation is
incomplete, so a missing WAL cannot silently replace database configuration
with defaults. These checks preserve the live family and retained recovery
files.

**Replacing only the executable is not a safe downgrade.** The upgraded
layout is incompatible with v0.7.6 despite the unchanged version. Preview and
run the receipt's guarded undo before subsequent changes:

```bash
br doctor migrate-schema undo \
  "$(jq -r .run_id migration-applied.json)" --dry-run --json
br doctor migrate-schema undo \
  "$(jq -r .run_id migration-applied.json)" --json
```

Undo refuses to discard later work and restores the actual original layout.
A source already modified by an earlier main build may itself be incompatible
with v0.7.6; undo does not promise otherwise. Choose the executable for that
restored layout. An older executable may create or change database sidecars
during startup even when its command then refuses the layout. Probe compatibility
only on a disposable copy of the complete family, never on the live upgraded
tracker; return to the older binary after the guarded restore. See the
[migration contract](CLI_REFERENCE.md#reviewed-schema-migration)
for preservation and recovery details.

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
