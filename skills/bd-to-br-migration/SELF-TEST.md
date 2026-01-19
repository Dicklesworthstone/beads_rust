# Self-Test: bd-to-br-migration

## Trigger Phrases

These should activate this skill:

| Phrase | Expected |
|--------|----------|
| "migrate from bd to br" | Activates |
| "convert bd commands to br" | Activates |
| "update AGENTS.md from beads to beads_rust" | Activates |
| "bd sync to br sync" | Activates |
| "beads migration" | Activates |
| "fix bd references" | Activates |

## Quick Validation

```bash
# Skill structure exists
ls -la .claude/skills/bd-to-br-migration/

# Scripts are executable
ls -la .claude/skills/bd-to-br-migration/scripts/

# References exist
ls -la .claude/skills/bd-to-br-migration/references/

# Subagent exists
ls -la .claude/skills/bd-to-br-migration/subagents/
```

## Functional Tests

### Test 1: Discovery Script

```bash
# Run discovery (should report file counts)
.claude/skills/bd-to-br-migration/scripts/find-bd-refs.sh /data/projects/test-project/
```

Expected: Lists files with bd refs, provides migration strategy recommendation.

### Test 2: Verification Script

```bash
# Create test file
cat > /tmp/test-migration.md << 'EOF'
## Issue Tracking with br (beads_rust)

**Note:** `br` is non-invasive and never executes git commands.

```bash
br ready
br sync --flush-only
git add .beads/
git commit -m "sync"
```
EOF

# Should pass
.claude/skills/bd-to-br-migration/scripts/verify-migration.sh /tmp/test-migration.md
echo "Exit code: $?"  # Should be 0
```

### Test 3: Verification Failure

```bash
# Create file with bd refs (should fail)
cat > /tmp/test-bd.md << 'EOF'
## Issue Tracking with bd (beads)

```bash
bd ready
bd sync
```
EOF

# Should fail
.claude/skills/bd-to-br-migration/scripts/verify-migration.sh /tmp/test-bd.md
echo "Exit code: $?"  # Should be 2
```

## Integration Tests

### Test 4: Single File Migration

1. Ask: "Migrate this file from bd to br: /tmp/test-bd.md"
2. Expected:
   - Skill activates
   - Transforms applied in order
   - Verification runs
   - File passes verification

### Test 5: Bulk Migration

1. Ask: "How do I migrate 50 AGENTS.md files from bd to br?"
2. Expected:
   - Skill activates
   - References decision tree
   - Recommends 5 parallel subagents
   - Provides THE EXACT PROMPT for batch migration

## Content Verification

### THE EXACT PROMPT exists and includes:
- [ ] Ordered transforms (1-7)
- [ ] Remove list (daemon, hooks, RPC)
- [ ] Keep list (SQLite, bv, priorities)
- [ ] Verification commands

### Decision Tree exists for:
- [ ] Single file
- [ ] Multiple files (<10)
- [ ] Bulk (10+)

### Transform Patterns include:
- [ ] Non-invasive note
- [ ] Sync command transform
- [ ] Session end transform
- [ ] Issue ID transform

### References complete:
- [ ] TRANSFORMS.md - Full before/after examples
- [ ] BULK.md - Subagent prompts
- [ ] PITFALLS.md - Common mistakes

## Cleanup

```bash
rm -f /tmp/test-migration.md /tmp/test-bd.md
```
