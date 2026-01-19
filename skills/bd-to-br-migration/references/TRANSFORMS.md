# Complete Transform Reference

## Table of Contents
- [Section Headers](#section-headers)
- [Command Transforms](#command-transforms)
- [Workflow Transforms](#workflow-transforms)
- [Session Protocol](#session-protocol)
- [Landing the Plane](#landing-the-plane)
- [Agent Mail Integration](#agent-mail-integration)
- [Full File Example](#full-file-example)

---

## Section Headers

| Before | After |
|--------|-------|
| `## Issue Tracking with bd (beads)` | `## Issue Tracking with br (beads_rust)` |
| `## Beads (bd)` | `## Beads (br)` |
| `## Beads (bd) — Dependency-Aware Issue Tracking` | `## Beads (br) — Dependency-Aware Issue Tracking` |
| `[beads_viewer](https://...)` | `[beads_rust](https://github.com/Dicklesworthstone/beads_rust)` |

---

## Command Transforms

### Simple Renames (No Behavioral Change)

```bash
# Before → After (all identical except name)
bd ready              → br ready
bd list               → br list
bd list --status=open → br list --status=open
bd show <id>          → br show <id>
bd create             → br create
bd create --title="..." --type=task --priority=2 → br create --title="..." --type=task --priority=2
bd update <id>        → br update <id>
bd update <id> --status=in_progress → br update <id> --status=in_progress
bd close <id>         → br close <id>
bd close <id> --reason="Done" → br close <id> --reason="Done"
bd dep add            → br dep add
bd stats              → br stats
```

### Sync Transform (BEHAVIORAL CHANGE)

**Before:**
```bash
bd sync               # Commits and pushes
```

**After:**
```bash
br sync --flush-only  # Exports only
git add .beads/       # YOU stage
git commit -m "..."   # YOU commit
```

---

## Workflow Transforms

### Basic Workflow

**Before:**
```markdown
1. **Start**: Run `bd ready` to find actionable work
2. **Claim**: Use `bd update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `bd close <id>`
5. **Sync**: Always run `bd sync` at session end
```

**After:**
```markdown
1. **Start**: Run `br ready` to find actionable work
2. **Claim**: Use `br update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id>`
5. **Sync**: Run `br sync --flush-only` then manually commit `.beads/`
```

### Agent Workflow with Commands

**Before:**
```markdown
### Agent workflow:

1. `bd ready` to find unblocked work.
2. Claim: `bd update <id> --status in_progress`.
3. Implement + test.
4. Close when done.
5. Commit `.beads/` in the same commit as code changes.
```

**After:**
```markdown
### Agent workflow:

1. `br ready` to find unblocked work.
2. Claim: `br update <id> --status in_progress`.
3. Implement + test.
4. Close when done.
5. Sync and commit:
   ```bash
   br sync --flush-only
   git add .beads/
   git commit -m "..."
   ```
```

---

## Session Protocol

### Before
```bash
git status              # Check what changed
git add <files>         # Stage code changes
bd sync                 # Commit beads changes
git commit -m "..."     # Commit code
bd sync                 # Commit any new beads changes
git push                # Push to remote
```

### After
```bash
git status              # Check what changed
git add <files>         # Stage code changes
br sync --flush-only    # Export beads to JSONL (no git ops)
git add .beads/         # Stage beads changes
git commit -m "..."     # Commit everything
git push                # Push to remote
```

---

## Landing the Plane

### Before
```bash
git pull --rebase
bd sync
git push
git status  # MUST show "up to date with origin"
```

### After
```bash
git pull --rebase
br sync --flush-only    # Export beads to JSONL (no git ops)
git add .beads/         # Stage beads changes
git commit -m "sync beads"  # Commit beads
git push
git status  # MUST show "up to date with origin"
```

---

## Agent Mail Integration

### Thread ID Convention

**Before:**
```markdown
- Mail `thread_id`: `bd-###`
- Mail subject: `[bd-###] ...`
- File reservation `reason`: `bd-###`
- Commit messages: Include `bd-###` for traceability
```

**After:**
```markdown
- Mail `thread_id`: `br-###`
- Mail subject: `[br-###] ...`
- File reservation `reason`: `br-###`
- Commit messages: Include `br-###` for traceability
```

### Typical Agent Flow

**Before:**
```markdown
1. **Pick ready work (Beads):**
   ```bash
   bd ready --json
   ```

2. **Reserve edit surface (Mail):**
   ```
   file_reservation_paths(..., reason="bd-123")
   ```

3. **Announce start (Mail):**
   ```
   send_message(..., thread_id="bd-123", subject="[bd-123] Start: <title>")
   ```
```

**After:**
```markdown
1. **Pick ready work (Beads):**
   ```bash
   br ready --json
   ```

2. **Reserve edit surface (Mail):**
   ```
   file_reservation_paths(..., reason="br-123")
   ```

3. **Announce start (Mail):**
   ```
   send_message(..., thread_id="br-123", subject="[br-123] Start: <title>")
   ```
```

---

## Full File Example

### Before (Complete Section)

```markdown
## Issue Tracking with bd (beads)

All issue tracking goes through **bd**. No other TODO systems.

Key invariants:
- `.beads/` is authoritative state and **must always be committed** with code changes.
- Do not edit `.beads/*.jsonl` directly; only via `bd`.

### Basics

Check ready work:
```bash
bd ready --json
```

### Essential Commands

```bash
bd ready              # Show issues ready to work
bd list --status=open # All open issues
bd create --title="..." --type=task --priority=2
bd update <id> --status=in_progress
bd close <id> --reason="Completed"
bd sync               # Commit and push changes
```

### Session End Checklist

```bash
git status
git add <files>
bd sync
git commit -m "..."
git push
```
```

### After (Complete Section)

```markdown
## Issue Tracking with br (beads_rust)

All issue tracking goes through **br** (beads_rust). No other TODO systems.

**Note:** `br` is non-invasive and never executes git commands. After `br sync --flush-only`, you must manually run `git add .beads/ && git commit`.

Key invariants:
- `.beads/` is authoritative state and **must always be committed** with code changes.
- Do not edit `.beads/*.jsonl` directly; only via `br`.

### Basics

Check ready work:
```bash
br ready --json
```

### Essential Commands

```bash
br ready              # Show issues ready to work
br list --status=open # All open issues
br create --title="..." --type=task --priority=2
br update <id> --status=in_progress
br close <id> --reason="Completed"
br sync --flush-only  # Export to JSONL (no git ops)
```

### Session End Checklist

```bash
git status
git add <files>
br sync --flush-only
git add .beads/
git commit -m "..."
git push
```
```

---

## Quick Search

```bash
# Find specific transform patterns
grep -i "session" references/TRANSFORMS.md
grep -i "landing" references/TRANSFORMS.md
grep -i "agent mail" references/TRANSFORMS.md
```
