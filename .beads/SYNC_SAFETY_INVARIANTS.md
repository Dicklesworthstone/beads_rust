# br sync Safety Invariants and Non-Goals

> Precise, testable invariants for br sync operations.
> Each invariant is phrased for direct assertion and logging.

---

## 1. Non-Goals (What br sync Will NEVER Do)

These are explicit design exclusions. br sync is intentionally less invasive than its Go predecessor.

| ID | Non-Goal | Rationale |
|----|----------|-----------|
| NG-1 | **Execute Git commands from sync** | Prevents implicit working tree side effects; users explicitly request any VCS diagnostic |
| NG-2 | **Install or invoke git hooks** | Non-invasive design; users add hooks manually if desired |
| NG-3 | **Run as daemon or background process** | Simple CLI only; no persistent state outside .beads/ |
| NG-4 | **Auto-commit changes** | Every Git operation remains user-controlled; `br vcs-status` is read-only |
| NG-5 | **Delete files outside .beads/** | Minimal filesystem footprint; data safety by confinement |
| NG-6 | **Create files outside .beads/** (without explicit opt-in) | Path confinement by default |
| NG-7 | **Modify files outside .beads/** | All mutations confined to project metadata |
| NG-8 | **Connect to external services** | Offline-first; no network calls during sync |

---

## 2. Safety Invariants (Testable Assertions)

**Risk Levels**: CRITICAL (data loss/corruption), HIGH (security/integrity), MEDIUM (reliability), LOW (usability)

### 2.1 Path Confinement Invariants

| ID | Risk | Invariant | Test Strategy |
|----|------|-----------|---------------|
| PC-1 | CRITICAL | All file writes occur within `.beads/` directory OR an explicitly user-specified JSONL path | Unit test: mock filesystem, assert no writes outside allowlist |
| PC-2 | HIGH | If `BEADS_JSONL` env var is set, it MUST be validated and logged before use | Unit test: verify validation function is called; log assertion |
| PC-3 | HIGH | All paths are canonicalized before I/O operations | Unit test: verify symlink resolution doesn't escape .beads/ |
| PC-4 | MEDIUM | Temp files are created in the same directory as target file | Unit test: verify temp path parent matches target parent |
| PC-RECOVERY | MEDIUM | Sync invocations may *transitively* cause storage recovery flows in `src/config/mod.rs::backup_database_family_for_recovery` to write under `.beads/.br_recovery/` with filenames `<original-name>.<stamp>.<suffix>` where `<suffix>` ∈ {`bak`, `rebuild-failed`, `truncated-wal`}. Recovery flow has its own path validation (separate from sync's `validate_sync_path`); the two are deliberately decoupled because recovery is a storage concern, not a sync concern. Tests that take a workspace-wide snapshot during sync MUST recognize these as legitimate side-effect writes. | Unit + e2e tests in `tests/e2e_sync_git_safety.rs::is_allowed_sync_file` recognize the three suffixes; arbitrary other contents in `.beads/.br_recovery/` are still rejected. |

### 2.2 Atomic Write Invariants

| ID | Risk | Invariant | Test Strategy |
|----|------|-----------|---------------|
| AW-1 | HIGH | Export uses temp file → rename pattern (never in-place modification) | Code inspection + unit test for temp file existence |
| AW-2 | HIGH | Temp file is flushed and synced before rename | Code inspection: verify `flush()` and `sync_all()` calls |
| AW-3 | MEDIUM | On any error during export, temp file is cleaned up | Unit test: inject error, verify no temp file remains |
| AW-4 | CRITICAL | Partial writes never corrupt the target JSONL | Unit test: crash simulation, verify original file intact |
| AW-5 | CRITICAL | Missing-database recovery installs a pre-locked candidate with an atomic no-replace operation, and database authority compares stable OS file IDs (Unix device/inode; Windows volume serial/file index), never timestamps | Unit tests: an existing destination remains byte-identical; a Windows replacement with equal creation time is rejected |

### 2.3 Data Loss Prevention Invariants

| ID | Risk | Invariant | Test Strategy |
|----|------|-----------|---------------|
| DL-1 | CRITICAL | Export of empty DB over non-empty JSONL requires `--force` | Integration test: attempt without --force, verify rejection |
| DL-2 | CRITICAL | Export that would lose JSONL issues requires `--force` | Integration test: DB missing issues from JSONL, verify rejection |
| DL-3 | HIGH | Import never resurrects tombstoned issues | Unit test: tombstoned issue in JSONL, verify skip |
| DL-4 | CRITICAL | Import conflict marker scan runs BEFORE any database modifications | Code inspection + unit test: markers detected → no DB changes |
| DL-5 | MEDIUM | Any operation that could discard/override data logs a warning at INFO level | Log assertion test |

### 2.4 Input Validation Invariants

| ID | Risk | Invariant | Test Strategy |
|----|------|-----------|---------------|
| IV-1 | HIGH | Import rejects files containing git merge conflict markers | Unit test: file with `<<<<<<<`, verify error |
| IV-2 | MEDIUM | Import validates JSON schema before inserting | Unit test: malformed JSON, verify rejection |
| IV-3 | LOW | Import validates issue ID prefix matches project prefix | Unit test: wrong prefix, verify collision handling |
| IV-4 | MEDIUM | Import uses 4-phase collision detection: external_ref → content_hash → id → new | Unit test: each phase |
| IV-5 | HIGH | Import never refuses a positive comment ID claimed by more than one issue (per-database rowids collide across clones); it reassigns the local rowid and verifies comments by payload `(issue_id, created_at, author, text)` | Unit test: payload-equal and payload-different cross-issue duplicates in either line order import, re-import is a no-op; skipped local owner keeps its rowid |

### 2.5 No Git Operations Invariants

| ID | Risk | Invariant | Test Strategy |
|----|------|-----------|---------------|
| NGI-1 | CRITICAL | Neither sync source boundary has subprocess, Git-library, or VCS-adapter authority | Fail-closed recursive source validation over `src/sync/**/*.rs` and `src/cli/commands/sync.rs`; reject missing/unreadable/non-UTF-8/symlinked/special entries, direct/aliased process construction, and indirect VCS delegation |
| NGI-2 | CRITICAL | br sync has no embedded runtime Git-library authority | Parsed manifest audit rejects direct normal/target-runtime git2/libgit2, gitoxide/gix, legacy git-repository family edges and aliases; strict `cargo tree -e normal` separately proves the resolved transitive runtime closure while allowing build/dev tooling such as vergen-gix |
| NGI-3 | CRITICAL | Every exercised sync branch leaves the complete `.git/` tree unchanged | E2E: fake `git` first on PATH detects ordinary executable-name dispatch, plus before/after equality of every path, file byte, symlink target, and Unix mode, including root/index/logs/refs/objects/config/HEAD with zero exclusions; static source/runtime-dependency guards cover absolute-path, shell, linked, and adapter authority surfaces |
| NGI-4 | HIGH | `sync --status` never probes VCS and emits a stable compatibility pointer | E2E inside/outside a Git repository: exact `{available:false, reason:"not_probed", diagnostic_command:"br vcs-status --json"}` object |
| NGI-5 | HIGH | VCS observation is available only through the explicit budgeted `br vcs-status` command | E2E tracked/dirty/missing-Git/non-repo semantics, deadline-aware secure capture/hash, direct-child termination/reap before return, anonymous-file output bounds, inherited-descriptor return, non-UTF-8 path, and external-path redaction; an individual filesystem read and cleanup may extend the probe budget, the selected executable remains trusted, and arbitrary descendants are not claimed sandboxed |

### 2.6 Additive Reconciliation Invariants

| ID | Risk | Invariant | Test Strategy |
|----|------|-----------|---------------|
| AR-1 | CRITICAL | Planning is read-only and emits a deterministic v2 plan token | Snapshot the complete DB family and JSONL bytes before/after repeated plans |
| AR-2 | CRITICAL | Apply requires the exact token from an identically configured reviewed plan | Reject missing, malformed, mismatched, stale-source, stale-DB, and resolution-set-drift tokens |
| AR-3 | CRITICAL | No authoritative issue, relation, child-evidence, config, event, JSONL, base, or merge-note row is deleted; only token-bound export/dirty/metadata bookkeeping and independently projected derived caches may be replaced | Seed every table; compare complete typed-row SHA-256 witnesses before/after and require exact projected bookkeeping/cache state |
| AR-4 | CRITICAL | Shared scalar drift fails closed except a field-bounded monotonic closure or exact-ID source resolution | Unit-test newer/equal/older drift, closure field boundaries, redundant/unknown resolution IDs |
| AR-5 | CRITICAL | Relation drift, tombstone resurrection, live-to-tombstone, or new blocking cycles cannot be overridden | Adversarial plan/apply rollback tests |
| AR-6 | HIGH | Comment surrogate collisions are deterministically remapped without changing logical comment payload | Five-collision fixture, remap witness digest, apply, and true second no-op |
| AR-7 | HIGH | Derived blocked cache and child counters equal an independent issue-graph projection | Seed stale/corrupt caches; plan, rebuild, compare exact maps and digests |
| AR-8 | HIGH | Strict source parsing rejects unknown or silently normalized fields | Unknown-field, enum-alias, blank optional, ignored content-hash, and omitted-option round-trip tests |
| AR-9 | HIGH | Receipt JSON Schema and command envelope are discoverable through `br schema` | Schema catalog and enum-domain tests |

---

## 3. Logging Requirements for Safety-Critical Decisions

### 3.1 Required INFO-Level Logs

These events MUST be logged at INFO level for visibility:

| Event | Log Message Template |
|-------|---------------------|
| Empty DB guard activated | `"Safety guard: refusing empty DB export over {n} existing issues"` |
| Stale DB guard activated | `"Safety guard: refusing export that would lose {n} issues: {ids}"` |
| Force override used | `"Force override: bypassing safety guard for {guard_name}"` |
| External JSONL path used | `"Using external JSONL path: {path}"` |
| Conflict markers detected | `"Conflict markers detected at {path}:{lines}"` |

### 3.2 Required DEBUG-Level Logs

These events should be logged at DEBUG level for forensic analysis:

| Event | Log Message Template |
|-------|---------------------|
| File read | `"Reading {path} ({bytes} bytes)"` |
| File write (temp) | `"Writing temp file {path}"` |
| File rename (atomic) | `"Atomic rename: {temp} -> {target}"` |
| Temp file cleanup | `"Cleaning up temp file {path}"` |
| Issue export | `"Exporting issue {id} (hash: {content_hash})"` |
| Issue import (new) | `"Importing new issue {id}"` |
| Issue import (update) | `"Updating existing issue {id} (old_hash -> new_hash)"` |
| Issue skip (tombstone) | `"Skipping tombstoned issue {id}"` |
| Collision detected | `"Collision detected for {id}: {collision_type}"` |
| Additive plan | `"Planned additive reconciliation"` with token prefix, counts, and status |
| Additive apply | `"Applying reviewed additive reconciliation transaction"` |
| Additive rollback | `"Rolled back additive reconciliation transaction"` with reason |

### 3.3 Structured Logging Format

All safety-critical logs MUST include:
- `operation`: The sync operation type (export/import)
- `path`: The file path involved
- `result`: success/failure/skipped
- `reason`: Human-readable explanation (on failure/skip)

---

## 4. Invariant-to-Guard Mapping

| Invariant | Implementation Guard | Location |
|-----------|---------------------|----------|
| PC-1 | `validate_sync_path()` | `sync/path.rs:130-220` |
| PC-2 | `require_valid_sync_path()` | `sync/path.rs:280-290` |
| PC-3 | Path canonicalization in `validate_sync_path()` | `sync/path.rs:150-175` |
| PC-4 | Same-directory temp file validation | `sync/path.rs` |
| DL-1 | `count_issues_in_jsonl()` check | `sync/mod.rs:480-490` |
| DL-2 | `get_issue_ids_from_jsonl()` diff check | `sync/mod.rs:493-526` |
| IV-1 | `ensure_no_conflict_markers()` | `sync/mod.rs:341-366` |
| AW-1 | Temp file pattern | `sync/mod.rs:590-646` |
| AW-2 | `flush()` + `sync_all()` calls | `sync/mod.rs:638-643` |
| DL-3 | Tombstone check in import | `sync/mod.rs` import logic |

---

## 5. Test Coverage Matrix

| Invariant Category | Unit Tests | Integration Tests | Fuzz Tests |
|-------------------|------------|-------------------|------------|
| Path Confinement | ✓ Required | ✓ Required | ○ Recommended |
| Atomic Write | ✓ Required | ○ Recommended | ○ Optional |
| Data Loss Prevention | ✓ Required | ✓ Required | ○ Recommended |
| Input Validation | ✓ Required | ✓ Required | ✓ Required |
| No Git Operations | ✓ Required (fail-closed) | ✓ Required (all modes) | N/A |
| Logging | ✓ Required | ○ Optional | N/A |

---

## 6. Explicit Opt-In Requirements

The following dangerous operations require explicit user intent:

| Operation | Required Flag | Additional Requirement |
|-----------|---------------|----------------------|
| Export empty DB over JSONL | `--force` | None |
| Export stale DB (loses issues) | `--force` | None |
| Use external JSONL path | `BEADS_JSONL` env var | Future: `--allow-external-jsonl` flag |

---

## 7. Future Hardening Recommendations

1. **Path Allowlist Validation** (Priority 1)
   - Validate `BEADS_JSONL` must be within `.beads/` or require explicit flag
   - Reject symlinks pointing outside `.beads/`
   - Canonicalize all paths before comparison

2. **Audit Logging** (Priority 1)
   - Implement structured logging for all safety-critical decisions
   - Add `--dry-run` mode that logs what would happen without doing it

3. **Test Hardening** (Priority 1)
   - Add regression tests for each invariant
   - Fuzz test import path handling
   - Add crash-recovery tests for atomic writes

---

## 8. Summary: Invariants by Risk Priority

### CRITICAL (Must-Fix Immediately)
| ID | Category | Invariant |
|----|----------|-----------|
| PC-1 | Path Confinement | All file writes within `.beads/` or explicit JSONL |
| AW-4 | Atomic Write | Partial writes never corrupt target JSONL |
| DL-1 | Data Loss | Empty DB export requires `--force` |
| DL-2 | Data Loss | Stale DB export requires `--force` |
| DL-4 | Data Loss | Conflict marker scan before any DB modifications |
| NGI-1 | No Git | Never execute git commands |
| NGI-2 | No Git | Never use git libraries |
| NGI-3 | No Git | Never modify .git/ |

### HIGH (Address Promptly)
| ID | Category | Invariant |
|----|----------|-----------|
| PC-2 | Path Confinement | BEADS_JSONL env var validated and logged |
| PC-3 | Path Confinement | Paths canonicalized before I/O |
| AW-1 | Atomic Write | Temp file → rename pattern |
| AW-2 | Atomic Write | Flush and sync before rename |
| DL-3 | Data Loss | Never resurrect tombstones |
| IV-1 | Input Validation | Reject conflict markers |
| IV-5 | Input Validation | Tolerate cross-issue duplicate comment IDs by reassigning the local rowid; identity is the comment payload |

### MEDIUM (Schedule Appropriately)
| ID | Category | Invariant |
|----|----------|-----------|
| PC-4 | Path Confinement | Temp files in same directory as target |
| AW-3 | Atomic Write | Cleanup temp files on error |
| DL-5 | Data Loss | Log warnings for data-affecting ops |
| IV-2 | Input Validation | Validate JSON schema |
| IV-4 | Input Validation | 4-phase collision detection |

### LOW (Best Effort)
| ID | Category | Invariant |
|----|----------|-----------|
| IV-3 | Input Validation | Validate issue ID prefix |

---

*Document created by PurpleFox (claude-opus-4-5-20251101) on 2026-01-16*
*Updated by BrightMesa (claude-opus-4-5-20251101) on 2026-01-16: Added risk prioritization*
*Reference: beads_rust-0v1.1.2*
