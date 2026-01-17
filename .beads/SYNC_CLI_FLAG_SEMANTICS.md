# br sync CLI Flag Semantics and User-Intent Gating

> Safe defaults and explicit opt-in requirements for br sync operations.

---

## 1. Design Principles

### 1.1 Safety-First Defaults

| Principle | Description |
|-----------|-------------|
| **Explicit over implicit** | Every significant action requires explicit user intent |
| **Read-only by default** | Operations that could lose data require explicit flags |
| **Local-only by default** | No network calls, no git operations, no external services |
| **Confined by default** | All I/O within `.beads/` unless explicitly overridden |

### 1.2 User Intent Hierarchy

1. **Implicit** (no flags needed): Safe, non-destructive queries
2. **Explicit** (flag required): Operations with side effects
3. **Forced** (`--force` required): Operations that could lose data
4. **Forbidden**: Operations br will never perform (e.g., git commands)

---

## 2. Current Flag Matrix

### 2.1 Sync Command Flags

| Flag | Required? | Default | Behavior | Safety Level |
|------|-----------|---------|----------|--------------|
| `--flush-only` | Yes* | N/A | Export DB → JSONL | Explicit |
| `--import-only` | Yes* | N/A | Import JSONL → DB | Explicit |
| `--status` | No | N/A | Show sync status (read-only) | Implicit |
| `--force` / `-f` | No | `false` | Override safety guards | Forced |
| `--manifest` | No | `false` | Write export manifest | Explicit |
| `--error-policy` | No | `strict` | Error handling mode | Explicit |
| `--orphans` | No | `strict` | Orphan handling mode | Explicit |
| `--robot` / `--json` | No | `false` | Machine-readable output | Implicit |

*One of `--flush-only` or `--import-only` is required.

### 2.2 Flag Dependencies

```
br sync                        → ERROR: Must specify mode
br sync --status              → OK: Read-only query
br sync --flush-only          → OK: Export with safety guards
br sync --flush-only --force  → OK: Export bypassing guards
br sync --import-only         → OK: Import with validation
br sync --import-only --force → OK: Import bypassing staleness check
```

---

## 3. Safety Guards and Their Bypass Flags

### 3.1 Export Safety Guards

| Guard | Trigger Condition | User Message | Bypass |
|-------|-------------------|--------------|--------|
| Empty DB Guard | DB has 0 issues, JSONL has N > 0 | "Refusing to export empty database..." | `--force` |
| Stale DB Guard | DB missing issues that exist in JSONL | "Refusing to export stale database..." | `--force` |
| No Dirty Issues | No changes since last export | "Nothing to export" | N/A (not an error) |

### 3.2 Import Safety Guards

| Guard | Trigger Condition | User Message | Bypass |
|-------|-------------------|--------------|--------|
| Conflict Markers | JSONL contains `<<<<<<<`, `=======`, `>>>>>>>` | "Merge conflict markers detected..." | **NONE** |
| JSONL Not Found | JSONL file doesn't exist | "No JSONL file found..." | N/A (informational) |
| Hash Unchanged | JSONL hash matches last import | "JSONL is current..." | `--force` |
| Schema Invalid | Malformed JSON in JSONL | "Invalid JSON at line N..." | **NONE** |

### 3.3 Non-Bypassable Guards

These guards can NEVER be bypassed, even with `--force`:

| Guard | Rationale |
|-------|-----------|
| Conflict Marker Scan | Importing unresolved merge conflicts corrupts data |
| Schema Validation | Invalid JSON would crash or corrupt |
| Path Confinement | Writing outside `.beads/` is a design non-goal |
| Git Operations | br sync will never execute git commands |

---

## 4. External JSONL Path Handling

### 4.1 Current Behavior (Environment Variable)

The `BEADS_JSONL` environment variable allows specifying an alternative JSONL path:

```bash
BEADS_JSONL=/custom/path/issues.jsonl br sync --flush-only
```

**Current Risk**: This allows escaping the `.beads/` directory without explicit CLI intent.

### 4.2 Recommended Hardening

Add explicit CLI opt-in for external paths:

| Scenario | Current | Recommended |
|----------|---------|-------------|
| `BEADS_JSONL` set, no flag | Silent use | Warning + require `--allow-external-jsonl` |
| `BEADS_JSONL` set, with flag | N/A | Allowed |
| Path outside `.beads/` via config | Allowed | Require `--allow-external-jsonl` |

**Proposed Flag**: `--allow-external-jsonl`
- Only needed when JSONL path is outside `.beads/`
- Logged at INFO level when activated
- Must be paired with `BEADS_JSONL` or `--jsonl-path`

### 4.3 Path Validation Rules

| Path | Action | Flag Required |
|------|--------|---------------|
| `.beads/issues.jsonl` | Allow | None |
| `.beads/custom.jsonl` | Allow | None |
| `../issues.jsonl` | Reject unless | `--allow-external-jsonl` |
| `/absolute/path.jsonl` | Reject unless | `--allow-external-jsonl` |
| Symlink → outside `.beads/` | Reject always | **Not allowed** |

---

## 5. Error Policy Semantics

### 5.1 Export Error Policies

| Policy | Behavior | Use Case |
|--------|----------|----------|
| `strict` (default) | Abort on any error | Production safety |
| `best-effort` | Skip errors, export what works | Recovery/debug |
| `partial` | Export valid, report failures | Partial recovery |
| `required-core` | Export issues, tolerate non-core errors | Data preservation |

### 5.2 Orphan Handling Modes

| Mode | Behavior | Risk Level |
|------|----------|------------|
| `strict` (default) | Fail on orphan dependencies | Safe |
| `skip` | Skip issues with orphan deps | Safe |
| `allow` | Import anyway, leave deps broken | Medium |
| `resurrect` | Import and create placeholder deps | Medium |

---

## 6. Safe vs Unsafe Invocations

### 6.1 Safe Invocations (Recommended)

```bash
# Check status before any operation
br sync --status

# Standard export (with safety guards)
br sync --flush-only

# Standard import (with validation)
br sync --import-only

# Export with manifest for audit
br sync --flush-only --manifest
```

### 6.2 Potentially Unsafe Invocations (Require Understanding)

```bash
# Force export (could lose JSONL-only issues)
br sync --flush-only --force

# Force import (could import stale data)
br sync --import-only --force

# Best-effort export (could silently skip issues)
br sync --flush-only --error-policy=best-effort
```

### 6.3 Forbidden Invocations (Will Never Work)

```bash
# No bidirectional sync (explicit modes only)
br sync                          # ERROR

# No auto-commit (br doesn't touch git)
br sync --auto-commit           # NOT IMPLEMENTED

# No hooks (non-invasive by design)
br sync --install-hooks         # NOT IMPLEMENTED

# No external paths without opt-in (RECOMMENDED)
BEADS_JSONL=/external/path br sync --flush-only  # FUTURE: ERROR without --allow-external-jsonl
```

---

## 7. CLI Help Messages

### 7.1 Command Help

```
br sync - Synchronize SQLite database with JSONL file

USAGE:
    br sync --flush-only    Export database to JSONL
    br sync --import-only   Import JSONL to database
    br sync --status        Show sync status

SAFETY:
    br sync performs NO git operations. Use git manually.
    Safety guards prevent accidental data loss. Use --force to override.

FLAGS:
    --flush-only        Export database to JSONL (required unless --import-only or --status)
    --import-only       Import JSONL to database (required unless --flush-only or --status)
    --status            Show sync status without modifying anything
    --force, -f         Override safety guards (use with caution)
    --manifest          Write manifest file with export summary
    --error-policy      Error handling: strict|best-effort|partial|required-core
    --orphans           Orphan handling: strict|skip|allow|resurrect
    --json, --robot     Machine-readable JSON output
```

### 7.2 Error Message Templates

| Scenario | Message |
|----------|---------|
| Empty DB guard | `Refusing to export empty database over non-empty JSONL. Use --force to override.` |
| Stale DB guard | `Refusing to export stale database. Run --import-only first, or use --force.` |
| Conflict markers | `Merge conflict markers detected. Resolve conflicts before importing.` |
| No mode specified | `Must specify exactly one of --flush-only or --import-only.` |

---

## 8. Future Considerations

### 8.1 Potential New Flags

| Flag | Purpose | Priority |
|------|---------|----------|
| `--allow-external-jsonl` | Explicit opt-in for external paths | High |
| `--dry-run` | Show what would happen without doing it | Medium |
| `--verbose` | Detailed logging during sync | Low |
| `--backup` | Create backup before destructive operations | Medium |

### 8.2 Deprecation Candidates

None currently. All flags serve specific purposes.

---

*Document authored by PurpleFox (claude-opus-4-5-20251101) on 2026-01-16*
*Reference: beads_rust-0v1.1.4*
