# Common Pitfalls & Fixes

## Table of Contents
- [Critical Pitfalls](#critical-pitfalls)
- [Transform Pitfalls](#transform-pitfalls)
- [Verification Pitfalls](#verification-pitfalls)
- [Bulk Migration Pitfalls](#bulk-migration-pitfalls)
- [Quick Diagnostics](#quick-diagnostics)

---

## Critical Pitfalls

### 1. Forgetting Manual Git Steps (THE BIG ONE)

**Symptom:** Work appears lost after session end.

**Cause:** Agent followed migrated docs but docs didn't include git steps after `br sync --flush-only`.

**Detection:**
```bash
# Find files with sync but no git add
for f in /data/projects/*/AGENTS.md; do
  if grep -q 'br sync --flush-only' "$f" && ! grep -q 'git add .beads/' "$f"; then
    echo "MISSING GIT STEPS: $f"
  fi
done
```

**Fix:** After EVERY `br sync --flush-only`, add:
```bash
git add .beads/
git commit -m "sync beads"
```

---

### 2. Incomplete Sync Transform

**Symptom:** `br sync` fails or behaves unexpectedly.

**Cause:** Transformed `bd sync` to `br sync` but forgot `--flush-only` flag.

**Detection:**
```bash
grep -n 'br sync[^-]' file.md
grep -n 'br sync$' file.md
```

**Fix:** `br sync` → `br sync --flush-only`

---

### 3. Mixed Terminology in Same File

**Symptom:** Confusing docs with both `bd-123` and `br-123` references.

**Cause:** Partial migration, missed some issue ID references.

**Detection:**
```bash
# Check for both patterns in same file
if grep -q 'bd-[0-9]' file.md && grep -q 'br-[0-9]' file.md; then
  echo "MIXED IDs in file.md"
fi
```

**Fix:** Search comprehensively:
```bash
grep -n 'bd-[0-9]' file.md
# Transform all to br-###
```

---

## Transform Pitfalls

### 4. Missing Non-Invasive Note

**Symptom:** Readers follow br commands but expect auto-commit behavior.

**Cause:** Forgot to add the critical behavioral note.

**Detection:**
```bash
# Files with br commands but no note
if grep -q '`br ' file.md && ! grep -q 'non-invasive' file.md; then
  echo "MISSING NOTE: $f"
fi
```

**Fix:** Add after section header:
```markdown
**Note:** `br` is non-invasive and never executes git commands. After `br sync --flush-only`, you must manually run `git add .beads/ && git commit`.
```

---

### 5. Daemon/Hook References Left Behind

**Symptom:** Docs mention "daemon" or "hooks" that don't exist in br.

**Cause:** Failed to remove bd-specific content.

**Detection:**
```bash
grep -in 'daemon\|hook\|rpc' file.md
```

**Fix:** Remove these sections entirely (not transform—DELETE).

---

### 6. Incomplete Pattern Search

**Symptom:** Some bd references remain after "complete" migration.

**Cause:** bd references appear in unexpected places:
- Inline code: `` `bd` ``
- Code blocks inside examples
- Mapping tables
- P0 workflow sections
- Agent Mail examples

**Detection (comprehensive):**
```bash
grep -E '(bd ready|bd list|bd show|bd create|bd update|bd close|bd sync|bd dep|bd stats|bd-[0-9]|\`bd )' file.md
```

**Fix:** Search with ALL patterns, not just common ones.

---

## Verification Pitfalls

### 7. False Positive in Verification

**Symptom:** Verification passes but file still has issues.

**Cause:** Verification only checks specific patterns, misses edge cases.

**Example missed patterns:**
```markdown
Use bd for issue tracking    # No backticks, not caught
The bd tool is deprecated    # Prose reference, not caught
```

**Fix:** Add prose check:
```bash
grep -i '\bbd\b' file.md | grep -v '`bd' | grep -v 'br'
```

---

### 8. False Negative in Verification

**Symptom:** Verification fails but file is actually correct.

**Cause:** File legitimately has no beads section (verification expects br patterns).

**Detection:**
```bash
# Check if file actually has beads content
grep -q 'beads\|\.beads\|br ' file.md && echo "Has beads"
```

**Fix:** Skip verification for files without beads sections.

---

## Bulk Migration Pitfalls

### 9. Parallel Agent File Conflicts

**Symptom:** File corrupted or has duplicate content.

**Cause:** Two agents edited same file simultaneously.

**Prevention:**
- Strict batching—no file in multiple batches
- Sequential verification between batches

**Recovery:**
```bash
git checkout -- /path/to/corrupted/file.md
# Re-run migration for this file only
```

---

### 10. Batch Size Too Large

**Symptom:** Agent context overflow, incomplete migrations.

**Cause:** >15 files per batch exceeds practical context.

**Fix:** Max 10 files per subagent batch.

---

### 11. Not Verifying Between Batches

**Symptom:** Later batches build on broken earlier batches.

**Cause:** Proceeded without verification.

**Fix:** ALWAYS verify before next batch:
```bash
./scripts/verify-migration.sh /path/to/batch/*.md
```

---

## Quick Diagnostics

### Comprehensive Health Check

```bash
#!/usr/bin/env bash
file="$1"

echo "=== Checking: $file ==="

# Should be 0
echo -n "bd commands: "
grep -c '`bd ' "$file" 2>/dev/null || echo "0"

echo -n "bd sync: "
grep -c 'bd sync' "$file" 2>/dev/null || echo "0"

echo -n "bd-### IDs: "
grep -c 'bd-[0-9]' "$file" 2>/dev/null || echo "0"

# Should be > 0 if file has beads sections
echo -n "br sync --flush-only: "
grep -c 'br sync --flush-only' "$file" 2>/dev/null || echo "0"

echo -n "git add .beads/: "
grep -c 'git add .beads/' "$file" 2>/dev/null || echo "0"

echo -n "non-invasive note: "
grep -c 'non-invasive' "$file" 2>/dev/null || echo "0"

# Should be 0
echo -n "daemon refs: "
grep -ci 'daemon' "$file" 2>/dev/null || echo "0"

echo -n "hook refs: "
grep -ci '\bhook\b' "$file" 2>/dev/null || echo "0"
```

### One-Liner Status

```bash
# Quick pass/fail for file
grep -q '`bd ' file.md && echo "FAIL: bd refs remain" || echo "PASS: no bd refs"
```
