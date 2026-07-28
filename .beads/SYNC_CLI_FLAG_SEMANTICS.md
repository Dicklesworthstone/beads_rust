# br sync CLI Flag Semantics and User-Intent Gating

> Safe defaults and explicit opt-in requirements for br sync operations.

---

## 1. Design Principles

### 1.1 Safety-First Defaults

| Principle | Description |
|-----------|-------------|
| **Explicit over implicit** | Every significant action requires explicit user intent |
| **Read-only by default** | Operations that could lose data require explicit flags |
| **Local-only by default** | Sync makes no network or Git calls and uses no external services |
| **Confined by default** | All I/O within `.beads/` unless explicitly overridden |

### 1.2 User Intent Hierarchy

1. **Implicit** (no flags needed): Safe, non-destructive queries
2. **Explicit** (flag required): Operations with side effects
3. **Forced** (`--force` required): Operations that could lose data
4. **Forbidden to sync**: Operations `br sync` will never perform (for example, Git commands)

---

## 2. Current Flag Matrix

### 2.1 Sync Command Flags

| Flag | Required? | Default | Behavior | Safety Level |
|------|-----------|---------|----------|--------------|
| `--flush-only` | Yes* | N/A | Export DB → JSONL | Explicit |
| `--import-only` | Yes* | N/A | Import JSONL → DB | Explicit |
| `--merge` | Yes* | N/A | Three-way merge base + DB + JSONL | Explicit |
| `--reconcile` | Yes* | N/A | Additive JSONL → DB reconciliation (no deletes, no JSONL writes) | Explicit |
| `--reconcile-additive` | Yes* | N/A | Read-only exact-ID additive recovery plan | Implicit until apply |
| `--apply` | No | `false` | Apply an additive plan | Explicit, token-gated |
| `--expect-plan-sha256` | With additive apply | N/A | Bind apply to the exact reviewed plan | Required |
| `--resolve-source-id` | No, repeatable | Empty | Resolve one reviewed non-lifecycle scalar conflict in favor of non-older JSONL | Explicit per ID; complete requested set is token-bound |
| `--status` | No | N/A | Show DB/JSONL sync status without probing VCS; machine output points to `br vcs-status --json` | Implicit |
| `--force` / `-f` | No | `false` | Override safety guards | Forced |
| `--force-db` | No | `false` | Resolve `--merge` conflicts by keeping local SQLite rows | Forced |
| `--force-jsonl` | No | `false` | Resolve `--merge` conflicts by keeping JSONL rows | Forced |
| `--dry-run` | No | `false` | With `--reconcile`: read-only plan preview, zero mutation | Implicit |
| `--manifest` | No | `false` | Write export manifest | Explicit |
| `--error-policy` | No | `strict` | Error handling mode | Explicit |
| `--orphans` | No | `strict` | Orphan handling mode | Explicit |
| `--robot` / `--json` | No | `false` | Machine-readable output | Implicit |

*One of `--flush-only`, `--import-only`, `--merge`, `--reconcile`,
`--reconcile-additive`, `--status`, or `--witness` is required.

`--reconcile` deliberately has NO force tier: it rejects `--force`,
`--rename-prefix`, and `--orphans`. Its guards (conflict markers, duplicate
ids, plan/apply witness verification, event-table immutability) cannot be
bypassed; the destructive alternatives remain `--import-only --force` and
`--import-only --rebuild`.

### 2.2 Flag Dependencies

```
br sync                        → ERROR: Must specify mode
br sync --status              → OK: Read-only query
br sync --flush-only          → OK: Export with safety guards
br sync --flush-only --force  → OK: Export bypassing guards
br sync --import-only         → OK: Import with validation
br sync --import-only --force → OK: Import bypassing staleness check
br sync --merge               → OK: Three-way merge; reports unresolved conflicts
br sync --merge --force       → OK: Resolve merge conflicts by newer timestamp
br sync --merge --force-db    → OK: Resolve merge conflicts by keeping SQLite
br sync --merge --force-jsonl → OK: Resolve merge conflicts by keeping JSONL
br sync --reconcile --dry-run → OK: Read-only additive plan preview
br sync --reconcile           → OK: Additive apply (creates + newer updates only)
br sync --reconcile --force   → ERROR: reconcile has no force tier
br sync --dry-run             → ERROR: --dry-run requires --reconcile
br sync --reconcile-additive  → OK: Read-only plan; emits plan_sha256
br sync --reconcile-additive --apply
                              → ERROR: exact --expect-plan-sha256 is required
br sync --reconcile-additive --apply --expect-plan-sha256 <reviewed-sha>
                              → OK only if the fresh plan is byte-for-byte identical
br sync --reconcile-additive --resolve-source-id bd-x
                              → Re-plan one exact non-lifecycle scalar conflict; rejects older JSONL and never permits relation/tombstone drift
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

### 3.3 Merge Safety Guards

| Guard | Trigger Condition | User Message | Bypass |
|-------|-------------------|--------------|--------|
| Both Modified | Base, SQLite, and JSONL all contain divergent versions | "Merge conflicts detected..." | `--force`, `--force-db`, `--force-jsonl` |
| Delete vs Modify | One side deletes an issue the other side modified | "Merge conflicts detected..." | `--force`, `--force-db`, `--force-jsonl` |
| Convergent Creation | SQLite and JSONL independently create the same ID with different content | "Merge conflicts detected..." | `--force`, `--force-db`, `--force-jsonl` |

### 3.4 Non-Bypassable Guards

These guards can NEVER be bypassed, even with `--force`:

| Guard | Rationale |
|-------|-----------|
| Conflict Marker Scan | Importing unresolved merge conflicts corrupts data |
| Schema Validation | Invalid JSON would crash or corrupt |
| Path Confinement | Writing outside `.beads/` is a design non-goal |
| Git Operations | `br sync` will never execute Git commands |
| Additive relation or tombstone drift | `--resolve-source-id` is scalar-only |
| Additive plan-token mismatch | No force flag exists; re-plan and review |
| Additive source/DB health failure | No force flag exists |

### 3.5 Explicit VCS Diagnostic

`br vcs-status` is not a sync mode. It is a separately requested, bounded
diagnostic that preserves tracked/worktree/index/HEAD/worktree-hash visibility
without granting process authority to either sync source boundary.

| Flag | Default | Behavior |
|------|---------|----------|
| `--jsonl PATH` | Configured JSONL | Select the JSONL to inspect |
| `--allow-external-jsonl` | `false` | Authorize a selected JSONL outside `.beads/`; output remains path-redacted |
| `--timeout-ms N` | `2000` | Shared execution budget starting before secure source capture; mandatory direct-child cleanup may extend it |
| `--json`, `--robot` | Human | Emit `br.vcs-export-status.v2` |

Unavailable Git, non-repositories, timeouts, and bounded-probe failures are
successful diagnostic records with a stable `reason`; unsafe path selection is
an execution error. Each stream is redirected to an anonymous temporary file,
polled against a fixed byte limit, and read back with a hard cap after the
direct child exits. On failure the direct child is terminated and reaped before
return; cleanup may extend past the probe budget. Secure capture checks the
deadline between bounded reads, though an individual filesystem read cannot be
preempted. Inherited
descendant descriptors therefore do not create a pipe-EOF wait. The selected
Git executable remains trusted. Search/attribute probes neutralize ambient
execution features, while fixed-key config probes intentionally observe
effective system/global/common/worktree settings. The command is not a sandbox
for arbitrary daemonized descendants. No sync flag delegates to this command.

---

## 4. External JSONL Path Handling

### 4.1 Current Behavior (Environment Variable)

The `BEADS_JSONL` environment variable allows specifying an alternative JSONL path:

```bash
BEADS_JSONL=/custom/path/issues.jsonl br sync --flush-only
```

External paths are rejected unless the same invocation includes
`--allow-external-jsonl`.

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

# Force merge resolution (could discard one side of a conflict)
br sync --merge --force-db
br sync --merge --force-jsonl

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

# No external paths without opt-in
BEADS_JSONL=/external/path/issues.jsonl br sync --flush-only  # ERROR
```

---

## 7. CLI Help Messages

### 7.1 Command Help

```
br sync - Synchronize SQLite database with JSONL file

USAGE:
    br sync --flush-only    Export database to JSONL
    br sync --import-only   Import JSONL to database
    br sync --merge         Three-way merge base + database + JSONL
    br sync --status        Show sync status
    br sync --witness       Emit deterministic JSONL witness
    br sync --reconcile-additive
                            Plan exact-ID additive reconciliation

SAFETY:
    br sync performs NO git operations. Run `br vcs-status` explicitly for
    bounded, read-only Git visibility.
    Safety guards prevent accidental data loss. Use --force to override.

FLAGS:
    --flush-only        Export database to JSONL (required unless --import-only or --status)
    --import-only       Import JSONL to database (required unless --flush-only or --status)
    --status            Show DB/JSONL sync status without probing VCS
    --force, -f         Override safety guards (use with caution)
    --force-db          Resolve --merge conflicts by keeping SQLite
    --force-jsonl       Resolve --merge conflicts by keeping JSONL
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
