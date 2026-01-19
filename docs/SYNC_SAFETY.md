# br sync Safety Model

> How `br sync` keeps your repository safe.

---

## Overview

`br` (beads_rust) is a **non-invasive** issue tracker. The `br sync` command synchronizes your SQLite database with a JSONL file for git-based collaboration.

**Key safety principle**: `br sync` will never modify your source code or execute git commands.

---

## What br sync Does

| Operation | Description |
|-----------|-------------|
| **Export** (`--flush-only`) | Writes issues from SQLite to `.beads/issues.jsonl` |
| **Import** (`--import-only`) | Reads issues from JSONL into SQLite |
| **Status** (`--status`) | Shows sync state without modifying anything |

All file I/O is confined to the `.beads/` directory by default.

---

## What br sync Will NEVER Do

These are explicit design non-goals. `br` will never:

1. **Execute git commands** - No commits, no pushes, no staging
2. **Modify files outside `.beads/`** - Your source code is never touched
3. **Install or invoke git hooks** - Fully manual hook setup if desired
4. **Run as a daemon** - Simple CLI only, no background processes
5. **Auto-commit changes** - Every git operation requires explicit user action
6. **Connect to external services** - Offline-first, no network calls

---

## Safety Guards

### Export Guards

| Guard | What it prevents | Override |
|-------|-----------------|----------|
| **Empty DB guard** | Exporting 0 issues over a JSONL with N issues | `--force` |
| **Stale DB guard** | Exporting when DB is missing issues from JSONL | `--force` |

### Import Guards

| Guard | What it prevents | Override |
|-------|-----------------|----------|
| **Conflict marker scan** | Importing unresolved merge conflicts | **None** - must resolve conflicts |
| **Schema validation** | Importing malformed JSON | **None** - must fix JSONL |
| **Tombstone protection** | Resurrecting deleted issues | **None** - by design |

---

## Using --force Safely

The `--force` flag bypasses export safety guards. Use it only when you understand the consequences:

```bash
# Safe: Export after intentionally clearing the database
br sync --flush-only --force

# Safe: Import after confirming JSONL is authoritative
br sync --import-only --force
```

**When to use --force:**
- After a deliberate database reset
- When JSONL is known to be authoritative
- During recovery from corruption

**When NOT to use --force:**
- Routinely (defeats the purpose of guards)
- Without understanding why a guard triggered
- When the error message is unclear

---

## External JSONL Paths

By default, sync operates on `.beads/issues.jsonl`. To use a different path:

```bash
# Set via environment variable
export BEADS_JSONL=/path/to/issues.jsonl
br sync --flush-only
```

**Future enhancement**: An `--allow-external-jsonl` flag will be required for paths outside `.beads/`.

**Safety notes:**
- External paths bypass the default confinement
- Symlinks pointing outside `.beads/` are rejected
- Paths are canonicalized before use

---

## Typical Workflow

### Starting a session
```bash
br sync --status           # Check if import is needed
br sync --import-only      # Import any JSONL changes
```

### Ending a session
```bash
br sync --flush-only       # Export DB changes to JSONL
git add .beads/            # Stage for commit (manual!)
git commit -m "Update issues"
```

### After pulling changes
```bash
git pull
br sync --import-only      # Import collaborators' changes
```

---

## Error Messages and What They Mean

### "Refusing to export empty database..."

**Cause**: Your database has 0 issues, but the JSONL file has existing issues.

**Fix**:
- Run `br sync --import-only` first to populate the database
- Or use `--force` if you intentionally want an empty export

### "Refusing to export stale database..."

**Cause**: The JSONL file contains issues that don't exist in your database.

**Fix**:
- Run `br sync --import-only` first to import the missing issues
- Or use `--force` if you intentionally want to lose those issues

### "Merge conflict markers detected..."

**Cause**: The JSONL file contains unresolved git merge conflicts.

**Fix**:
- Open the JSONL file and resolve the conflicts manually
- Look for `<<<<<<<`, `=======`, and `>>>>>>>` markers
- `--force` will NOT bypass this check

---

## Why These Guardrails Exist

### The Incident That Shaped br

The Go predecessor (`bd`) suffered a catastrophic failure where `bd sync` **deleted all repository source files**. This wasn't a theoretical risk—it actually happened, destroying irreplaceable work. The root cause was a sync operation that had too much authority: it could execute git commands, modify arbitrary files, and make irreversible changes without explicit confirmation.

This incident motivated every design decision in `br`'s safety model.

### Defense in Depth

`br` employs multiple layers of protection:

| Layer | Protection | Failure Mode Blocked |
|-------|------------|---------------------|
| **No git operations** | Cannot execute `git rm`, `git clean`, or any git command | Eliminates the primary attack vector from the original incident |
| **Path confinement** | All writes strictly confined to `.beads/` directory | Prevents accidental modification of source code, configs, or system files |
| **Path validation** | Rejects traversal (`../`), symlink escapes, and disallowed extensions | Blocks path injection attacks and symlink-based escapes |
| **Atomic writes** | Uses temp file + rename; partial failures don't corrupt | Prevents data loss from interrupted operations |
| **Safety guards** | Empty DB and stale DB guards require `--force` to override | Makes destructive operations explicit and intentional |

### How Tests Enforce Safety

The safety model is backed by an extensive test suite (**635+ tests**) that ensures these guarantees cannot regress:

- **Path guard unit tests** (`sync::path::tests`): 22 tests verify that traversal attempts, external paths, and disallowed file types are rejected
- **File tree snapshot tests** (`e2e_sync_git_safety.rs`): Integration tests take complete snapshots of the directory tree before and after sync, verifying that only `.beads/issues.jsonl` and related files are touched
- **Git mutation tests**: Regression tests verify that no commits, staged changes, or `.git/` modifications occur during sync
- **Atomic write tests** (`e2e_sync_failure_injection.rs`): Tests inject failures mid-export to verify the original file is preserved
- **Conflict marker tests**: Import preflight tests verify that merge conflicts are detected and rejected

### How Logging Aids Diagnosis

When sync operations occur, structured logging records safety-critical decisions:

```bash
# Enable verbose logging to see safety checks
br sync --flush-only -v
br sync --flush-only -vv  # Even more detail
```

Key logged events:
- Path validation results (allowed/rejected with reason)
- Conflict marker scan results
- Export guard trigger events (empty DB, stale DB)
- Atomic write operations (temp file creation, rename)

If a safety guard triggers unexpectedly, the verbose log will show exactly why.

### The Core Guarantee

**Even if `br sync` has a bug, it cannot delete your source code.**

This is not a best-effort promise—it's an architectural constraint enforced by:
1. Code that literally cannot call git (no git library, no shell-out to git)
2. Path validation that rejects anything outside `.beads/`
3. Tests that would fail if these constraints were violated

---

## Further Reading

For technical details, see:
- `.beads/SYNC_THREAT_MODEL.md` - Incident analysis and failure scenarios
- `.beads/SYNC_SAFETY_INVARIANTS.md` - Testable safety invariants
- `.beads/SYNC_CLI_FLAG_SEMANTICS.md` - Flag matrix and opt-in rules

---

*This document is part of the br safety hardening initiative.*
