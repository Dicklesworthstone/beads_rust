# br Troubleshooting Guide

Common issues and solutions when using `br` (beads_rust).

---

## Table of Contents

- [Quick Diagnostics](#quick-diagnostics)
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
# Check workspace health
br doctor

# Show project statistics
br stats

# Check sync status
br sync --status

# Show configuration
br config --list

# Show version
br version
```

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
br config --path

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
br list -a --json | jq '.[] | select(.status == "tombstone")'
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
# Manual fix: edit the file
$EDITOR .beads/issues.jsonl

# Or restore from backup
br history list
br history restore <backup>

# Skip bad lines (lossy)
br sync --import-only --error-policy best-effort
```

---

### "Prefix mismatch: expected 'proj', found 'bd'"

**Error Code:** `PREFIX_MISMATCH` (exit code 6)

**Cause:** JSONL contains issues with different prefix than configured.

**Solutions:**
```bash
# Check configured prefix
br config --get id.prefix

# Import with force (if intentional)
br sync --import-only --force

# Or update config to match
br config --set id.prefix=bd
```

---

### "Import collision: 5 issues have conflicting content"

**Error Code:** `IMPORT_COLLISION` (exit code 6)

**Cause:** Same issue IDs with different content in database and JSONL.

**Solutions:**
```bash
# Check sync status
br sync --status --json

# Export current state first (backup)
br sync --flush-only

# Force import (overwrites local)
br sync --import-only --force
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
br list --json | jq '[.[] | select(.dirty)] | length'

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

## Database Problems

### "Database is locked"

**Error Code:** `DATABASE_LOCKED` (exit code 2)

**Cause:** Another process has the database locked.

**Solutions:**
```bash
# Wait and retry with timeout
br list --lock-timeout 10000

# Find locking process
fuser .beads/beads.db

# Kill if stuck (careful!)
# fuser -k .beads/beads.db
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

# Try automatic migration
br doctor

# Manual migration (if supported)
br upgrade --migrate-db

# Last resort: reinitialize
mv .beads/beads.db .beads/beads.db.backup
br sync --import-only
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
# Check integrity
sqlite3 .beads/beads.db "PRAGMA integrity_check;"

# Check for missing tables
sqlite3 .beads/beads.db ".tables"
```

**Recovery:**
```bash
# Backup current state
cp .beads/beads.db .beads/beads.db.corrupt

# Try repair
sqlite3 .beads/beads.db "REINDEX;"
sqlite3 .beads/beads.db "VACUUM;"

# Or rebuild from JSONL
rm .beads/beads.db
br sync --import-only
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
br config --path

# Reset to defaults
rm .beads/beads.yaml
br init
```

---

### Config values not taking effect

**Cause:** Config precedence issue (7 layers from defaults to CLI).

**Diagnosis:**
```bash
# Show effective config with sources
br config --list -v

# Check specific value
br config --get <key>

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

# Vacuum database
sqlite3 .beads/beads.db "VACUUM;"
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

### Complete workspace recovery from JSONL

```bash
# Backup current state
mv .beads .beads.backup

# Reinitialize
br init --prefix <your-prefix>

# Import from JSONL
cp .beads.backup/issues.jsonl .beads/
br sync --import-only

# Verify
br stats
br doctor
```

---

### Recovery from corrupted database

```bash
# 1. Backup everything
cp -r .beads .beads.backup.$(date +%Y%m%d)

# 2. Export what we can
br sync --flush-only --error-policy best-effort || true

# 3. Check JSONL integrity
jq -c '.' .beads/issues.jsonl >/dev/null && echo "JSONL OK"

# 4. Rebuild database
rm .beads/beads.db
br sync --import-only

# 5. Verify
br doctor
br stats
```

---

### Recovery from git merge conflicts

```bash
# 1. Identify conflicts
grep -l "<<<<<<" .beads/*.jsonl

# 2. Resolve manually or use ours/theirs
git checkout --ours .beads/issues.jsonl
# OR
git checkout --theirs .beads/issues.jsonl

# 3. Import resolved file
br sync --import-only --force

# 4. Mark resolved
git add .beads/issues.jsonl
```

---

### Emergency database reset

**Warning:** This loses any changes not in JSONL.

```bash
# Nuclear option
rm .beads/beads.db
br sync --import-only

# Verify nothing lost
br stats
br list --limit 0 | wc -l
```

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
   br config --list
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
