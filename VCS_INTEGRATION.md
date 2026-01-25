# VCS Integration (Git Baseline)

br is VCS-agnostic and never runs version-control commands. It only reads/writes
inside `.beads/`. This document covers the **git baseline** workflow plus
best-effort command equivalents for other VCS (hg, jj, Perforce, Sapling).
Verify details with your VCS docs if your environment differs.

## Core Principles

- **br does not run git** — all VCS operations are explicit and manual.
- **Track `.beads/` in version control** — it is the collaboration surface.
- **Local-only artifacts stay local** — `.beads/.gitignore` excludes dbs, locks,
  and machine-specific files.

## Git Baseline Workflow

### Export before commit

```bash
br sync --flush-only
git add .beads/
git commit -m "Update issues"
```

### Pull and import after sync

```bash
git pull --rebase
br sync --import-only
```

## Common Git Commands

| Task | Git command |
|------|-------------|
| Stage `.beads/` changes | `git add .beads/` |
| Check status | `git status .beads/` |
| Inspect JSONL diff | `git diff .beads/issues.jsonl` |
| Resolve JSONL conflicts | `git add .beads/issues.jsonl` (after manual edit), then `br sync --import-only` |

## Conflict Handling (JSONL)

JSONL is line-based, so conflicts are typically straightforward:

```bash
git status .beads/issues.jsonl
vim .beads/issues.jsonl   # resolve per-line conflicts
git add .beads/issues.jsonl
br sync --import-only
```

For more detail, see the README FAQ on JSONL conflicts.

---

## Mercurial (hg)

| Task | Command |
|------|---------|
| Stage `.beads/` changes | `hg add .beads/` |
| Check status | `hg status .beads/` |
| Inspect JSONL diff | `hg diff .beads/issues.jsonl` |
| Resolve JSONL conflicts | edit file → `hg resolve -m .beads/issues.jsonl` |

## Jujutsu (jj)

jj is snapshot-based; there is no explicit “add” step for tracked files.

| Task | Command |
|------|---------|
| Check status | `jj status .beads/` |
| Inspect JSONL diff | `jj diff .beads/issues.jsonl` |
| Record change | `jj commit -m "Update issues"` |

## Perforce (p4)

| Task | Command |
|------|---------|
| Open/add/edit `.beads/` | `p4 reconcile //.../.beads/...` |
| Check status | `p4 status //.../.beads/...` |
| Inspect JSONL diff | `p4 diff //.../.beads/issues.jsonl` |
| Resolve JSONL conflicts | edit file → `p4 resolve //.../.beads/issues.jsonl` |

## Sapling (sl)

Sapling uses hg-like commands via the `sl` CLI.

| Task | Command |
|------|---------|
| Stage `.beads/` changes | `sl add .beads/` |
| Check status | `sl status .beads/` |
| Inspect JSONL diff | `sl diff .beads/issues.jsonl` |
| Resolve JSONL conflicts | edit file → `sl resolve -m .beads/issues.jsonl` |
