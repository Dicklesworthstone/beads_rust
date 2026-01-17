# br sync Safety Maintenance Checklist

> Use this checklist when making changes to sync-related code.

---

## Quick Reference

Before merging any PR that touches sync code, verify all checks pass:

```
[ ] No git operations introduced
[ ] Path allowlist unchanged or documented
[ ] All sync safety tests pass
[ ] Logs reviewed for safety events
[ ] Documentation updated if behavior changed
```

---

## Detailed Checklist

### 1. Verify No Git Operations

**Why**: `br sync` must never execute git commands. This is a non-negotiable safety invariant.

**Checks**:

```bash
# Static check: no git subprocess calls
grep -rn 'Command::new.*git' src/sync/ src/cli/commands/sync.rs

# Should return 0 results. Any git command invocation is a blocker.

# Dependency check: no git libraries
grep -E '^(git2|gitoxide|libgit)' Cargo.toml

# Should return 0 results.
```

**If found**: STOP. Discuss with team before proceeding. The sync module must remain git-free.

---

### 2. Verify Path Allowlist

**Why**: Sync file I/O must be confined to `.beads/` directory.

**Checks**:

Review `src/sync/path.rs`:

```bash
# Check the allowlist hasn't expanded dangerously
grep -A20 'fn is_allowed_sync_file' src/sync/path.rs
```

Verify the allowlist only includes:
- `.beads/*.db` (SQLite database)
- `.beads/*.db-wal`, `.beads/*.db-shm` (SQLite WAL files)
- `.beads/*.jsonl` (JSONL export)
- `.beads/*.jsonl.tmp` (atomic write temp files)
- `.beads/.manifest.json` (optional manifest)
- `.beads/metadata.json` (optional metadata)

**If changed**: Document the reason in the PR and update `SYNC_SAFETY_INVARIANTS.md`.

---

### 3. Run Sync Safety Tests

**Why**: Tests verify safety invariants haven't regressed.

**Commands**:

```bash
# Run all tests (required)
cargo test --release

# Run sync-specific unit tests
cargo test sync:: --release

# Run sync safety e2e tests
cargo test e2e_sync --release

# Run with verbose output for debugging
cargo test e2e_sync --release -- --nocapture
```

**Expected results**:
- All tests pass
- No new `SAFETY VIOLATION` assertions
- No unexpected file modifications logged

**If tests fail**: Do not merge. Fix the issue or revert the change.

---

### 4. Review Logs for Safety Events

**Why**: Logs reveal unexpected safety-critical behavior that tests may miss.

**Process**:

1. Enable verbose logging:
   ```bash
   RUST_LOG=beads_rust=debug cargo test e2e_sync --release -- --nocapture 2>&1 | tee sync_test.log
   ```

2. Search for safety events:
   ```bash
   grep -E '(Safety|guard|VIOLATION|reject|block|refuse)' sync_test.log
   ```

3. Review any matches for unexpected behavior.

**Warning signs**:
- `Safety guard: refusing` - Guard triggered unexpectedly
- `SAFETY VIOLATION` - Test detected a safety regression
- `reject` or `block` for legitimate paths

---

### 5. Review Documentation

**Why**: Safety guarantees must be documented for users and maintainers.

**If behavior changed**, update:

| Document | When to update |
|----------|----------------|
| `docs/SYNC_SAFETY.md` | User-facing safety model changes |
| `.beads/SYNC_SAFETY_INVARIANTS.md` | Technical invariant additions/modifications |
| `.beads/SYNC_CLI_FLAG_SEMANTICS.md` | New flags or flag behavior changes |
| `docs/E2E_SYNC_TESTS.md` | New test files or test patterns |

**Checklist for docs**:
```
[ ] Safety guarantees still accurate?
[ ] New flags documented with safety implications?
[ ] Test coverage section updated?
```

---

## Pre-Merge Verification Summary

Run this final check before approving:

```bash
# 1. Verify no git operations
! grep -rn 'Command::new.*git' src/sync/ src/cli/commands/sync.rs

# 2. Run full test suite
cargo test --release

# 3. Specifically run sync safety tests
cargo test e2e_sync --release

# 4. Check for any test failures
echo $?  # Should be 0
```

All commands should succeed (exit code 0) before merging.

---

## Post-Merge Monitoring

After merging sync changes:

1. **Monitor CI** - Verify nightly/weekly test runs pass
2. **Review issues** - Watch for user reports of unexpected sync behavior
3. **Log audit** - Periodically check production logs for safety events

---

## When to Escalate

Escalate immediately if:

- Any test containing `SAFETY VIOLATION` fails
- `grep` finds git command invocations in sync code
- Path allowlist needs expansion beyond `.beads/`
- User reports data loss or unexpected file modifications

Contact the maintainer team before proceeding with any of these cases.

---

## Related Documentation

- [SYNC_SAFETY.md](SYNC_SAFETY.md) - User-facing safety model
- [E2E_SYNC_TESTS.md](E2E_SYNC_TESTS.md) - Test execution guide
- [.beads/SYNC_SAFETY_INVARIANTS.md](../.beads/SYNC_SAFETY_INVARIANTS.md) - Technical invariants
- [.beads/SYNC_THREAT_MODEL.md](../.beads/SYNC_THREAT_MODEL.md) - Threat analysis

---

*This checklist is part of the br safety hardening initiative.*
*Last updated: 2026-01-16 by SilverValley*
