# Bulk Migration Strategy

## Table of Contents
- [Discovery Phase](#discovery-phase)
- [Batch Strategy](#batch-strategy)
- [Subagent Prompt](#subagent-prompt)
- [Verification Phase](#verification-phase)
- [Commit Strategy](#commit-strategy)
- [Rollback](#rollback)

---

## Discovery Phase

### Find All Files Needing Migration

```bash
# Find files with bd command references
grep -l '`bd ' /data/projects/*/AGENTS.md 2>/dev/null

# Count total files
grep -l '`bd ' /data/projects/*/AGENTS.md 2>/dev/null | wc -l

# Find files with bd sync specifically
grep -l 'bd sync' /data/projects/*/AGENTS.md 2>/dev/null

# Find files with bd-### issue IDs
grep -l 'bd-[0-9]' /data/projects/*/AGENTS.md 2>/dev/null
```

### Use the Discovery Script

```bash
./scripts/find-bd-refs.sh /data/projects/
```

Output:
```
=== Files with bd references ===
/data/projects/foo/AGENTS.md
/data/projects/bar/AGENTS.md
...

=== Count summary ===
Files with bd commands: 74
Files with bd sync: 68
Files with bd-### IDs: 45
```

---

## Batch Strategy

### Decision Matrix

| File Count | Strategy | Rationale |
|------------|----------|-----------|
| 1-5 | Sequential | Overhead not worth parallelization |
| 6-15 | 2 subagents | Balance speed and coordination |
| 16-50 | 5 subagents (~10 each) | Efficient parallel processing |
| 50+ | 8 subagents | Max practical parallelization |

### Batching Rules

1. **~10 files per subagent** — Sweet spot for context and verification
2. **Group by project type** — Similar files migrate similarly
3. **Verify each batch** — Don't proceed to next batch until current passes
4. **One agent per batch** — No file touched by multiple agents

---

## Subagent Prompt

### THE EXACT PROMPT — Batch Migration

```
Migrate these files from bd (beads) to br (beads_rust):

FILES:
- /data/projects/project1/AGENTS.md
- /data/projects/project2/AGENTS.md
- /data/projects/project3/AGENTS.md
[... list all files in batch ...]

Apply transforms IN THIS ORDER for each file:
1. Section headers: "bd (beads)" → "br (beads_rust)"
2. Add non-invasive note after beads section header:
   **Note:** `br` is non-invasive and never executes git commands. After `br sync --flush-only`, you must manually run `git add .beads/ && git commit`.
3. Commands: `bd X` → `br X` for ready/list/show/create/update/close/dep/stats
4. Sync command: `bd sync` → `br sync --flush-only`
5. Add git steps after EVERY sync command:
   git add .beads/
   git commit -m "sync beads"
6. Issue IDs: bd-### → br-### in thread_ids, subjects, reasons
7. Links: beads_viewer → beads_rust (if present)

Remove completely:
- Daemon references
- Auto-commit assumptions
- Hook installation mentions

VERIFY each file after editing:
grep -c '`bd ' FILE.md     # Must be 0
grep -c 'bd sync' FILE.md  # Must be 0

Report format:
✓ /path/file.md - migrated
✗ /path/file.md - FAILED: [reason]

Do NOT proceed to next file if current file fails verification.
```

---

## Verification Phase

### Post-Batch Verification

```bash
# Verify no bd refs remain (should return empty)
grep -l '`bd ' /data/projects/*/AGENTS.md 2>/dev/null | grep -v beads_rust

# Verify new patterns exist
grep -l 'br sync --flush-only' /data/projects/*/AGENTS.md | wc -l
grep -l 'git add .beads/' /data/projects/*/AGENTS.md | wc -l
```

### Full Verification Script

```bash
# Run on all files
for f in /data/projects/*/AGENTS.md; do
  ./scripts/verify-migration.sh "$f" || echo "FAILED: $f"
done
```

### Expected Results

| Check | Expected |
|-------|----------|
| Files with `bd ` | 0 (except beads_rust repo) |
| Files with `br sync --flush-only` | = Files with beads sections |
| Files with `git add .beads/` | = Files with sync commands |

---

## Commit Strategy

### Option 1: Single Commit (Recommended)

```bash
# Stage all AGENTS.md changes
git add /data/projects/*/AGENTS.md

# Single descriptive commit
git commit -m "$(cat <<'EOF'
docs: migrate AGENTS.md from bd to br (beads_rust)

- bd → br command references
- bd sync → br sync --flush-only + manual git steps
- bd-### → br-### issue ID convention
- Added non-invasive note explaining git requirements
- Removed daemon/RPC references (not applicable to br)

Affected: ~74 AGENTS.md files

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

### Option 2: Per-Project Commits

```bash
# For each project
for project in /data/projects/*/; do
  if [[ -f "$project/AGENTS.md" ]]; then
    git add "$project/AGENTS.md"
    git commit -m "docs($project): migrate AGENTS.md bd → br"
  fi
done
```

---

## Rollback

### If Migration Introduced Errors

```bash
# Check git diff for specific file
git diff /data/projects/PROJECT/AGENTS.md

# Restore single file
git checkout -- /data/projects/PROJECT/AGENTS.md

# Restore all AGENTS.md files (CAREFUL)
git checkout -- /data/projects/*/AGENTS.md
```

### Partial Rollback

```bash
# Find files that failed verification
for f in /data/projects/*/AGENTS.md; do
  if ! ./scripts/verify-migration.sh "$f" >/dev/null 2>&1; then
    echo "Restoring: $f"
    git checkout -- "$f"
  fi
done
```

---

## Progress Tracking

For large migrations, track progress:

```markdown
# bd → br Migration Progress

## Completed Batches
- [x] Batch 1: projects a-g (10 files) - verified
- [x] Batch 2: projects h-n (10 files) - verified
- [ ] Batch 3: projects o-t (10 files) - in progress

## Issues Encountered
- project_x: No beads section (skipped)
- project_y: Custom bd wrapper (manual review needed)

## Verification Status
Total files: 74
Migrated: 45
Remaining: 29
```
