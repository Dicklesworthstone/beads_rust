# br sync Safety Maintenance Checklist

> Use this checklist when making changes to sync-related code.

---

## Quick Reference

Before merging any PR that touches sync code, verify all checks pass:

```
[ ] Fail-closed sync authority scan passes
[ ] Every sync mode passes the PATH-sentinel and exact .git snapshot matrix
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
# Structural check over both complete sync source boundaries. This fails on
# missing/unreadable/non-UTF-8/symlinked/special source entries as well as direct
# process authority, Git libraries, and delegation to the VCS adapter.
cargo test --lib 'validation::tests::sync_safety_' -- --nocapture

# Runtime check: every sync mode gets a fake `git` first on PATH and a
# byte-exact, zero-exclusion .git tree comparison around the invocation.
cargo test --test e2e_sync_git_safety \
  e2e_every_sync_mode_has_zero_git_authority_and_zero_git_mutation

# Parsed direct-runtime dependency check (normal/target declarations, aliases,
# malformed manifests, and non-table forms fail closed)
cargo test --lib sync_safety_no_direct_runtime_git_library_dependencies

# Resolved transitive runtime closure (build/dev tooling is excluded)
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 \
  rch --no-self-healing exec -- cargo tree -e normal --prefix none
```

Fail closed if the runtime tree contains `git2-*`, `libgit2-*`, `gix-*`,
`gitoxide-*`, or legacy `git-repository-*` authority packages. A clean direct
manifest check alone does not prove this transitive closure.

The structural scan deliberately treats matching text in comments and strings
as a failure. This conservative false-positive boundary is acceptable
maintenance friction: documentation can be reworded or moved outside the sync
authority boundary, while skipping source regions would create an evasion
surface. Whitespace is normalized so split-token formatting cannot bypass the
check.

**If either test fails**: STOP. The sync module must remain process- and
VCS-authority-free.

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
- `.beads/*.db-wal`, `.beads/*.db-shm`, `.beads/*.db-journal` (SQLite sidecar files)
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
cargo test --test e2e_sync_git_safety

# Run with verbose output for debugging
cargo test --test e2e_sync_git_safety -- --nocapture
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
   RUST_LOG=beads_rust=debug cargo test --release \
     --test e2e_sync_git_safety \
     --test e2e_sync_status_health \
     --test e2e_vcs_status \
     -- --nocapture 2>&1 | tee sync_test.log
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
# 1. Verify no process/VCS authority in either sync source boundary
cargo test --lib 'validation::tests::sync_safety_' -- --nocapture

# 2. Verify every sync mode under PATH sentinel + exact .git snapshot
cargo test --test e2e_sync_git_safety \
  e2e_every_sync_mode_has_zero_git_authority_and_zero_git_mutation

# 3. Run full test suite
cargo test --release

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
- The structural authority scan finds a forbidden construct or cannot inspect
  the complete source boundary
- The PATH sentinel is invoked or any `.git` byte/path changes
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
