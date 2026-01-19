---
name: bd-to-br-migration
description: >-
  Migrate docs from bd to br (beads_rust). Use when "bd sync" → "br sync
  --flush-only", updating AGENTS.md, or beads migration.
---

# bd → br Migration

> `br` never touches git. That's the only change.

## Quick Start

```bash
# Before
bd sync              # Auto-commits

# After
br sync --flush-only # You must: git add .beads/ && git commit
```

All other commands: `s/bd/br/g`

---

## THE EXACT PROMPT

```
Migrate this file from bd to br. Apply IN ORDER:

1. Headers: "bd (beads)" → "br (beads_rust)"
2. Add note: "**Note:** `br` never executes git. After `br sync --flush-only`, run `git add .beads/ && git commit`."
3. Commands: `bd X` → `br X` (ready/list/show/create/update/close/dep/stats)
4. Sync: `bd sync` → `br sync --flush-only` + git add .beads/ + git commit
5. IDs: bd-### → br-###
6. Remove: daemon refs, auto-commit assumptions, hooks, RPC

VERIFY:
grep -c '`bd ' file.md  # Must be 0
```

---

## Decision Tree

```
File count?
├─ 1-5   → Apply prompt sequentially
├─ 6-15  → 2 subagents, ~5-7 files each
├─ 16-50 → 5 subagents, ~10 files each
└─ 50+   → See BULK.md
```

---

## Validation

```bash
./scripts/verify-migration.sh file.md
```

| Check | Must be |
|-------|---------|
| `grep -c '\`bd ' file` | 0 |
| `grep -c 'bd sync' file` | 0 |

---

## Degrees of Freedom: LOW

Deterministic transformation. One correct output per input. No creative interpretation.

---

## References

| Need | File |
|------|------|
| Full before/after examples | [TRANSFORMS.md](references/TRANSFORMS.md) |
| Command map & patterns | [TRANSFORMS.md](references/TRANSFORMS.md) |
| Bulk migration (10+ files) | [BULK.md](references/BULK.md) |
| Common mistakes | [PITFALLS.md](references/PITFALLS.md) |

---

## Scripts

| Script | Purpose |
|--------|---------|
| `./scripts/find-bd-refs.sh /path` | Find files needing migration |
| `./scripts/verify-migration.sh file` | Verify complete |
