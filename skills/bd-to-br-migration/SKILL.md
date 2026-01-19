---
name: bd-to-br-migration
description: >-
  Migrate docs from bd (beads) to br (beads_rust). Use when updating AGENTS.md,
  converting bd commands, "bd sync" → "br sync --flush-only", or beads migration.
---

<!-- TOC: Philosophy | THE EXACT PROMPT | Decision Tree | Command Map | Transform Patterns | Validation Loop | Risk Tiers | References -->

# bd → br Migration

> **Core Philosophy:** One behavioral change, mechanical transforms. The ONLY difference is git handling—everything else is find-replace.

## Why This Matters

Incomplete migrations leave broken docs. Agents follow stale `bd sync` instructions, expect auto-commit, and lose work. This skill ensures **complete, verified migrations**.

---

## THE EXACT PROMPT — Single File Migration

```
Migrate this file from bd (beads) to br (beads_rust).

Apply transforms IN THIS ORDER (order matters):
1. Section headers: "bd (beads)" → "br (beads_rust)"
2. Add non-invasive note after beads section header
3. Commands: `bd X` → `br X` for ready/list/show/create/update/close/dep/stats
4. Sync command: `bd sync` → `br sync --flush-only`
5. Add git steps after EVERY sync:
   git add .beads/
   git commit -m "sync beads"
6. Issue IDs: bd-### → br-### in thread_ids, subjects, reasons, commits
7. Links: beads_viewer → beads_rust (if present)

Remove completely:
- Daemon references
- Auto-commit assumptions
- Hook installation mentions
- RPC mode

Keep unchanged:
- SQLite/WAL cautions
- bv integration
- Priority system (P0-P4)

VERIFY after editing:
grep -c '`bd ' file.md     # Must be 0
grep -c 'bd sync' file.md  # Must be 0
grep -c 'br sync --flush-only' file.md  # Must be > 0
```

### Why This Prompt Works

- **Ordered transforms**: Dependencies exist (sync must change before adding git steps)
- **Explicit removals**: Daemon/RPC don't exist in br—leaving them confuses agents
- **Keep list**: Prevents accidental removal of still-valid patterns
- **Built-in verification**: Grep commands catch missed transforms
- **No degrees of freedom**: This is a LOW freedom task—exact transforms required

---

## Decision Tree: What Are You Migrating?

```
What are you migrating?
│
├─ Single file (AGENTS.md)
│  │
│  └─ Follow THE EXACT PROMPT above
│     Use: ./scripts/verify-migration.sh file.md
│
├─ Multiple files (batch)
│  │
│  ├─ <10 files → Sequential: apply prompt to each
│  │
│  └─ 10+ files → Parallel subagents
│     Batch ~10 files per agent
│     See: [BULK.md](references/BULK.md)
│
└─ Verify existing migration
   │
   └─ Run: ./scripts/find-bd-refs.sh /path
      Any output = incomplete migration
```

---

## The One Behavioral Difference

```
┌─────────────────────────────────────────────────────────────────┐
│                    bd (Go)              br (Rust)               │
├─────────────────────────────────────────────────────────────────┤
│  bd sync                     →    br sync --flush-only          │
│  (auto-commits to git)            (exports JSONL only)          │
│                                                                 │
│                              +    git add .beads/               │
│                              +    git commit -m "..."           │
└─────────────────────────────────────────────────────────────────┘

Everything else is literally s/bd/br/g
```

---

## Command Map

| bd | br | Change Type |
|----|-----|-------------|
| `bd ready` | `br ready` | Name only |
| `bd list` | `br list` | Name only |
| `bd show <id>` | `br show <id>` | Name only |
| `bd create` | `br create` | Name only |
| `bd update` | `br update` | Name only |
| `bd close` | `br close` | Name only |
| `bd dep add` | `br dep add` | Name only |
| `bd stats` | `br stats` | Name only |
| `bd sync` | `br sync --flush-only` + git | **BEHAVIORAL** |

---

## Transform Patterns

### Pattern 1: The Non-Invasive Note

**Add immediately after any beads section header:**

```markdown
**Note:** `br` is non-invasive and never executes git commands. After `br sync --flush-only`, you must manually run `git add .beads/ && git commit`.
```

### Pattern 2: Sync Command Transform

**Before:**
```bash
bd sync
```

**After:**
```bash
br sync --flush-only
git add .beads/
git commit -m "sync beads"
```

### Pattern 3: Session End Transform

**Before:**
```bash
git add <files>
bd sync
git push
```

**After:**
```bash
git add <files>
br sync --flush-only
git add .beads/
git commit -m "..."
git push
```

### Pattern 4: Issue ID Transform

**Before:**
```markdown
thread_id: bd-123
subject: [bd-123] Feature implementation
reason: bd-123
```

**After:**
```markdown
thread_id: br-123
subject: [br-123] Feature implementation
reason: br-123
```

---

## Validation Loop

```
┌─────────────────────────────────────────────────────────────────┐
│                     VALIDATION IS MANDATORY                     │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  1. Apply transforms                                            │
│                    ↓                                            │
│  2. Run verification:                                           │
│     ./scripts/verify-migration.sh file.md                       │
│                    ↓                                            │
│  3. If FAIL → read error → fix specific issue → goto 2          │
│                    ↓                                            │
│  4. Only proceed when PASS                                      │
│                                                                 │
│  ⚠️ Never skip verification. Incomplete migrations break agents.│
└─────────────────────────────────────────────────────────────────┘
```

### Quick Verification Commands

```bash
# MUST return 0:
grep -c '`bd ' file.md
grep -c 'bd sync' file.md
grep -c 'bd ready' file.md

# MUST return > 0 (if file has sync sections):
grep -c 'br sync --flush-only' file.md
grep -c 'git add .beads/' file.md
```

---

## Risk Tiers

| Operation | Risk | Freedom |
|-----------|------|---------|
| Command renames (`bd` → `br`) | Low | Mechanical—no judgment |
| Sync transform + git steps | Medium | MUST add git steps |
| Removing daemon refs | Medium | Verify not removing valid content |
| Bulk migration (10+ files) | High | Use subagents with verification |

### Degrees of Freedom: LOW

This is a **deterministic transformation**. There is ONE correct output for each input.

- No creative interpretation
- No optional improvements
- No stylistic choices
- Apply transforms EXACTLY as specified

---

## What Gets Removed

| Pattern | Why Remove | Verify Absent |
|---------|------------|---------------|
| "bd daemon" | br has no daemon | `grep -i daemon` |
| "auto-commits" | br never commits | `grep -i "auto.*commit"` |
| "git hooks" | br installs none | `grep -i "hook"` |
| "RPC mode" | br has no RPC | `grep -i "rpc"` |

---

## What Stays Unchanged

| Pattern | Why Keep |
|---------|----------|
| SQLite/WAL cautions | br still uses WAL |
| bv integration | Works with both |
| Priority P0-P4 | Same system |
| Issue types | Same system |
| Dependency tracking | Same system |
| `.beads/` as source of truth | Same system |

---

## Before/After Example

### Before (bd)
```markdown
## Issue Tracking with bd (beads)

Key invariants:
- `.beads/` is authoritative

### Agent workflow:
1. `bd ready` to find work
2. `bd update <id> --status in_progress`
3. Implement
4. `bd close <id>`
5. `bd sync` commits changes
```

### After (br)
```markdown
## Issue Tracking with br (beads_rust)

**Note:** `br` is non-invasive and never executes git commands. After `br sync --flush-only`, you must manually run `git add .beads/ && git commit`.

Key invariants:
- `.beads/` is authoritative

### Agent workflow:
1. `br ready` to find work
2. `br update <id> --status in_progress`
3. Implement
4. `br close <id>`
5. Sync and commit:
   ```bash
   br sync --flush-only
   git add .beads/
   git commit -m "sync beads"
   ```
```

---

## References

| Need | Reference |
|------|-----------|
| Complete before/after examples | [TRANSFORMS.md](references/TRANSFORMS.md) |
| Bulk migration strategy | [BULK.md](references/BULK.md) |
| Common mistakes & fixes | [PITFALLS.md](references/PITFALLS.md) |

---

## Scripts

| Script | Purpose |
|--------|---------|
| `./scripts/find-bd-refs.sh /path` | Find files needing migration |
| `./scripts/verify-migration.sh file.md` | Verify migration complete |

---

## Validation

```bash
# Full verification
./scripts/verify-migration.sh /path/to/AGENTS.md

# Quick check (should return nothing)
grep '`bd ' /path/to/AGENTS.md
```

If any bd references remain → migration incomplete → re-apply transforms.
