# THREAT MODEL: br sync Safety Analysis

## 1. Incident Class Summary

**Incident**: The Go-based `bd sync` command produced a commit that deleted all repository source files, requiring version control recovery.

**Impact Classification**: CATASTROPHIC - Complete working tree deletion is unrecoverable without version control.

**Recovery Path**: Use git to restore files from previous commit

---

## 2. Root Cause Analysis

### 2.1 Plausible Root Causes in bd (Go)

The incident could have occurred through several mechanisms:

#### A. Auto-Git Operations (MOST LIKELY)
- `bd sync` may have executed `git add .` followed by `git commit`
- An overbroad add pattern could stage deletions from a bad state
- Auto-commit hooks could trigger commits at inopportune times

#### B. Path Misconfiguration
- If JSONL export path was misconfigured to repo root or a working tree path
- Relative path resolution could escape the `.beads/` directory
- Environment variable `BEADS_JSONL` pointing to unsafe location

#### C. Merge Driver Bugs
- Git merge drivers for `.jsonl` files could produce incorrect resolutions
- Merge conflicts could result in empty or corrupted JSONL
- Auto-resolution might delete content

#### D. Cleanup/Housekeeping Gone Wrong
- A cleanup routine that removed "stale" files with too broad a pattern
- Temporary file cleanup that matched real files
- Orphan detection that deleted too much

#### E. Hook Chain Reactions
- Pre-commit or post-commit hooks interacting badly
- Hooks that modify the working tree before/after sync
- Cascading failures from hook errors

---

## 3. Threat Model for br

### 3.1 Threat Actors

| Actor | Description | Capability |
|-------|-------------|------------|
| **User Error** | Misconfiguration, wrong flags | Likely, recoverable |
| **Path Injection** | Malicious JSONL paths | Medium risk |
| **Corrupted JSONL** | Invalid/crafted input | Medium risk |
| **Tool Bug** | Logic error in br | High impact |
| **Environment** | Bad env vars, symlinks | Medium risk |

### 3.2 Attack Vectors & Failure Scenarios

#### Scenario 1: Path Traversal via BEADS_JSONL
- **Vector**: User sets `BEADS_JSONL=/important/file.jsonl`
- **Risk**: Overwrites critical system or project files
- **Likelihood**: LOW (requires explicit user action)
- **Impact**: HIGH (data loss)

#### Scenario 2: Empty DB Export Over Full JSONL
- **Vector**: DB corruption/reset leads to 0 issues, export overwrites
- **Risk**: Loss of all JSONL-tracked issues
- **Likelihood**: MEDIUM (requires DB issue + no --force)
- **Impact**: HIGH (data loss)
- **Current Mitigation**: Safety guard blocks without --force ✓

#### Scenario 3: Stale DB Export Loses Issues
- **Vector**: DB has subset of JSONL issues, export truncates
- **Risk**: Issues in JSONL but not DB are lost
- **Likelihood**: MEDIUM
- **Impact**: HIGH (data loss)
- **Current Mitigation**: Safety guard blocks without --force ✓

#### Scenario 4: Git Operations Side Effects
- **Vector**: br invokes git commands that modify working tree
- **Risk**: Unintended file deletion or modification
- **Likelihood**: N/A - br performs NO git operations ✓
- **Impact**: N/A
- **Current Mitigation**: Complete prevention by design ✓

#### Scenario 5: Atomic Write Failure
- **Vector**: System crash during temp file → rename
- **Risk**: Partial file or corrupt state
- **Likelihood**: LOW
- **Impact**: MEDIUM
- **Current Mitigation**: Atomic rename pattern ✓

#### Scenario 6: Conflict Marker Injection
- **Vector**: Import file contains git merge markers
- **Risk**: Corrupt data import, cascading failures
- **Likelihood**: MEDIUM
- **Impact**: LOW (rejected at import)
- **Current Mitigation**: Conflict marker scan before import ✓

---

## 4. Current br Safety Architecture (Verified)

### 4.1 What br DOES (Safe Operations)

| Operation | Location | Safety |
|-----------|----------|--------|
| Read SQLite DB | `.beads/*.db` | SAFE - read only |
| Write SQLite DB | `.beads/*.db` | SAFE - confined |
| Export JSONL | `.beads/*.jsonl` | SAFE - atomic write |
| Import JSONL | `.beads/*.jsonl` | SAFE - validates first |
| Write manifest | `.beads/.manifest.json` | SAFE - confined |
| Update metadata | DB metadata table | SAFE - confined |

### 4.2 What br NEVER Does (Critical Non-Goals)

| Forbidden Operation | Rationale |
|--------------------|-----------|
| Execute `git` commands | Prevents working tree side effects |
| Write outside `.beads/` | Prevents file system damage |
| Auto-commit changes | Prevents unintended commits |
| Install git hooks | Non-invasive by design |
| Run as daemon | No background processes |
| Delete files (outside DB) | Minimal file system footprint |

### 4.3 Existing Safety Guards

1. **Empty DB Guard**: Refuses export of 0 issues over non-empty JSONL without `--force`
2. **Stale DB Guard**: Refuses export that would lose JSONL issues without `--force`
3. **Conflict Marker Scan**: Aborts import if merge markers detected
4. **Atomic Writes**: Temp file → rename pattern prevents partial writes
5. **Tombstone Protection**: Never resurrects deleted issues during import
6. **Path Confinement**: All I/O within `.beads/` directory (with env override escape hatch)

---

## 5. Mitigation Mapping

| Failure Scenario | Mitigation | Status |
|-----------------|------------|--------|
| Export deletes JSONL issues | Stale DB guard | ✅ Implemented |
| Empty DB overwrites JSONL | Empty DB guard | ✅ Implemented |
| Corrupt JSONL import | Conflict marker scan, JSON validation | ✅ Implemented |
| Git side effects | No git operations | ✅ By design |
| Path traversal | Confined to .beads/ (except env var) | ⚠️ ENV can escape |
| Partial writes | Atomic rename | ✅ Implemented |
| Tombstone resurrection | Tombstone protection | ✅ Implemented |

---

## 6. Recommendations for Hardening

### 6.1 Path Validation (Priority 1)
- Validate `BEADS_JSONL` if set: must be within `.beads/` or require `--allow-external-jsonl`
- Canonicalize all paths before I/O
- Reject symlinks pointing outside `.beads/`

### 6.2 Logging for Safety-Critical Decisions (Priority 1)
- Log at INFO level when safety guards activate
- Log at DEBUG level all file I/O operations with paths
- Structured logging for forensic analysis

### 6.3 Test Coverage (Priority 1)
- Unit tests proving no files touched outside `.beads/`
- Regression test for "export deletes issues" scenario
- Fuzz testing for JSONL import paths

### 6.4 Documentation (Priority 2)
- Document the safety model in README/docs
- Explain what br will NEVER do
- Migration guide from bd with safety notes

---

## 7. Conclusion

The current br implementation is fundamentally safe because:
1. **It performs NO git operations** - eliminating the primary attack vector
2. **All file I/O is confined** - to `.beads/` directory by default
3. **Safety guards exist** - for common data loss scenarios
4. **Atomic operations** - prevent partial/corrupt states

The remaining risk is the `BEADS_JSONL` environment variable escape hatch, which should be hardened with additional validation or require explicit opt-in.

---

*Analysis performed by PurpleFox (claude-opus-4-5-20251101) on 2026-01-16*
