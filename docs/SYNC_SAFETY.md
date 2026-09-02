# br sync Safety Model

> How `br sync` keeps your repository safe.

---

## Overview

`br` (beads_rust) is a local-first issue tracker. This document covers the
safety model for the `br sync` command, which synchronizes your SQLite database
with a JSONL file for git-based collaboration.

**Key safety principle**: with the default `.beads/` paths, `br sync` will never
modify your source code or execute git commands. External JSONL paths require
explicit opt-in and remain subject to extension, symlink, traversal, and `.git/`
guards.

---

## What br sync Does

| Operation | Description |
|-----------|-------------|
| **Export** (`--flush-only`) | Writes issues from SQLite to `.beads/issues.jsonl` |
| **Import** (`--import-only`) | Reads issues from JSONL into SQLite |
| **Salvage import** (`--import-only --skip-invalid-records`) | Explicit additive recovery: removes invalid records after preserving an exact protected source backup, then imports the validated survivor generation |
| **Merge** (`--merge`) | Three-way merge of base snapshot, SQLite, and JSONL |
| **Additive reconciliation** (`--reconcile-additive`) | Plans exact-ID recovery of JSONL-only rows while preserving SQLite-only rows and events |
| **Additive apply** (`--reconcile-additive --apply`) | Applies a conflict-free, hash-bound additive plan transactionally |
| **Source-path migration** (`--migrate-source-repo-path`) | Reconciles DB/JSONL rows and plans normalization of every `source_repo_path` to the canonical current workspace while preserving portable `source_repo` names |
| **Rebuild** (`--import-only --rebuild`) | Treats JSONL as authoritative and rebuilds SQLite from it |
| **Status** (`--status`) | Shows database/JSONL sync state without probing VCS |

All file I/O is confined to the `.beads/` directory by default.

---

## What br sync Will NEVER Do

These are explicit design non-goals for the sync command. `br sync` will never:

1. **Execute git commands** - No commits, no pushes, no staging
2. **Modify files outside its sync allowlist** - Default writes stay in `.beads/`; external JSONL paths require explicit opt-in
3. **Install or invoke git hooks** - Fully manual hook setup if desired
4. **Run as a daemon** - Simple CLI only, no background processes
5. **Auto-commit changes** - Every git operation requires explicit user action
6. **Connect to external services** - Offline-first, no network calls

### Writing your own pre-commit hook

If you add a hook that flushes and stages `.beads/issues.jsonl`, make it
respect path-limited commits. `git commit --only <path>` and
`git commit <path>` build the commit from a temporary index
(`$GIT_INDEX_FILE` is `.git/next-index-<pid>.lock`), and a `git add` inside
the hook widens that commit beyond the paths the caller named. Skip the
staging step in that mode; the flush itself is still safe:

```sh
#!/bin/sh
br sync --flush-only --quiet || exit 1
case "$(basename "${GIT_INDEX_FILE:-index}")" in
  next-index-*) exit 0 ;;   # path-limited commit: do not widen it
esac
git add .beads/issues.jsonl
```

Other explicitly requested br commands have their own scope: for example,
`br changelog`, `br orphans`, and commit-activity `br stats` inspect git
history, while `br agents`, `br doctor --repair`, `br config`, and `br
completions -o` can write the user-requested files they manage. Those commands
do not weaken the `br sync` invariants described here.

### Explicit VCS diagnostics are separate

`br sync --status --json` retains a compatibility `git_export` object, but it
always reports:

```json
{
  "available": false,
  "reason": "not_probed",
  "diagnostic_command": "br vcs-status --json"
}
```

Run `br vcs-status` only when you explicitly want Git visibility for the JSONL
export. That command is isolated outside both sync source boundaries. Its probe
budget starts before secure source capture; capture, every Git subprocess, and
each in-process blob-hash chunk check the shared deadline between bounded
operations. An individual filesystem read cannot itself be preempted. Subprocess output goes
to anonymous temporary files with hard retained-output caps. On timeout or a
runner failure, br terminates and reaps the direct child before returning;
cleanup can extend past the probe budget. The v2 result compares exact
HEAD/index identities and computes raw Git/SHA-256 hashes in-process from one
immutable no-follow JSONL snapshot. Effective system/global/common/worktree
configuration and repository-local attributes are inspected before any
worktree comparison; configured filters or text conversions make that
comparison explicitly unavailable rather than being executed. The observations
are sequential, not a transactional Git snapshot.

The selected Git executable remains part of the trust boundary. Search and
attribute probes neutralize hooks, filters, prompts, paging, lazy object
fetches, fsmonitor, untracked-cache writes, and inherited Git redirections.
Fixed-key effective-config probes intentionally observe Git's normal
system/global/common/worktree precedence without printing configured paths.
The command is not a process sandbox and does not claim to terminate
arbitrary daemonized descendants. No sync mode calls or delegates to it.

---

## Safety Guards

### Export Guards

| Guard | What it prevents | Override |
|-------|-----------------|----------|
| **Empty DB guard** | Exporting 0 issues over a JSONL with N issues | `--force` |
| **Stale DB guard** | Exporting when DB is missing issues from JSONL | `--force` |

### Import Guards

| Guard | What it prevents | Override |
|-------|-----------------|----------|
| **Conflict marker scan** | Importing unresolved merge conflicts | **None** - must resolve conflicts |
| **Schema validation** | Importing malformed JSON | **None** - must fix JSONL |
| **Payload comment identity** | Treating a database-local comment rowid as global identity: two clones both mint comment id `1`, so a merged JSONL routinely carries one id for two issues; import reassigns the local rowid and verifies comments by `(issue, created_at, author, text)` instead of refusing the file | n/a - never refuses; same-issue duplicate payloads are still rejected |
| **Tombstone protection** | Resurrecting deleted issues | **None** - by design |

### Merge Guards

| Guard | What it prevents | Override |
|-------|-----------------|----------|
| **Both modified conflict** | Silently choosing between divergent SQLite and JSONL edits | `--force`, `--force-db`, `--force-jsonl` |
| **Delete vs modify conflict** | Silently deleting one side's edit | `--force`, `--force-db`, `--force-jsonl` |
| **Convergent creation conflict** | Silently choosing between independently created same-ID issues | `--force`, `--force-db`, `--force-jsonl` |

### Additive Reconciliation Guards

| Guard | What it prevents | Override |
|-------|------------------|----------|
| **Read-only default** | Accidental mutation while inspecting recovery scope | `--apply` after receipt review |
| **Exact-ID identity** | Merging unrelated rows that happen to share a content hash | **None** |
| **Database-only preservation** | JSONL set difference deleting local rows | **None** |
| **Event witness** | Recovery truncating or rewriting the audit log | **None** |
| **Timestamp conflict** | Unreviewed scalar drift or older JSONL overwriting newer SQLite | Exact `--resolve-source-id` only for the non-lifecycle scalar whitelist when JSONL is not older |
| **Tombstone protection** | Recovery resurrecting a deleted issue | **None** |
| **Relation identity** | Orphan/self dependencies, logical comment-payload changes, invalid metadata, or relation-owner mismatch | **None**; storage-local incoming comment surrogates are deterministically reallocated and witnessed |
| **Projected-cycle check** | Newly introducing a blocking or parent-child dependency cycle | **None** |
| **Source/database witness recheck** | Applying a plan after either side changed | **None** - regenerate the plan |

### Source Repository Path Migration Guards

| Guard | What it prevents | Override |
|-------|------------------|----------|
| **Read-only default** | Reconciliation or path rewriting before review | `--apply` with the exact reviewed plan token |
| **Canonical current target** | Retaining a stale home, temporary, or foreign checkout path | **None** |
| **Portable-name preservation** | Replacing `source_repo` with a machine-specific path | **None** |
| **Timestamp/tombstone rules** | Older JSONL clobbering newer SQLite, equal-timestamp drift, or resurrection | **None** |
| **Complete-generation witness** | Applying after DB or JSONL changed | **None** - regenerate the plan |
| **Commit-both recovery receipt** | An interruption leaving an unwitnessed DB/JSONL generation | **None** - the next migration/merge invocation resumes the pending receipt |

The migration deliberately does not probe Git or infer staged/unstaged state;
all `br sync` modes retain zero Git authority. Run `br vcs-status --json`
separately before applying when VCS state is part of the operator's review.

---

## Using --force Safely

The `--force` flag bypasses export safety guards. Use it only when you understand the consequences:

```bash
# Safe: Export after intentionally clearing the database
br sync --flush-only --force

# Safe: Import after confirming JSONL is authoritative
br sync --import-only --force

# Safe: Merge after confirming the newer timestamp should win
br sync --merge --force
```

### Recovering a historical malformed record

Ordinary import rejects any invalid issue record. If a pre-existing tracked
JSONL is already malformed, use the explicit salvage flag:

```bash
br sync --import-only --skip-invalid-records --json
```

Salvage does not weaken the default parser. It retains exact original bytes in
a protected `.beads/.br_history/*pre-salvage*.jsonl` backup excluded from
automatic age/count rotation, reports every rejected line in robot output,
refuses when no valid record would remain, and still hard-fails on
merge-conflict markers. Explicit history pruning can still remove the backup.
The cleaned file is conditionally published under the same JSONL-family
authority used by normal sync and the import consumes that exact immutable
generation.

The recovery is additive and rejects `--force`, `--rebuild`, and
`--rename-prefix`. If the database preserves valid records absent from the
cleaned JSONL, the receipt reports `database_records_requiring_export`, sets
`needs_flush`, and a normal `br sync --flush-only` restores full JSONL coverage.

**When to use --force:**
- After a deliberate database reset
- When JSONL is known to be authoritative
- During recovery from corruption
- During `--merge`, when timestamp-based conflict resolution is intentional

**When NOT to use --force:**
- Routinely (defeats the purpose of guards)
- Without understanding why a guard triggered
- When the error message is unclear

Use `--force-db` or `--force-jsonl` instead of `--force` when you want a specific
side of a merge conflict to win regardless of timestamps:

```bash
# Keep local SQLite changes for merge conflicts
br sync --merge --force-db

# Keep JSONL changes for merge conflicts
br sync --merge --force-jsonl
```

The merge base is `.beads/beads.base.jsonl`. A successful export or merge updates
that snapshot so future `--merge` runs can distinguish local SQLite edits from
JSONL edits.

`--force-db`, `--force-jsonl`, and `--force` are mutually exclusive during
`--merge`. They only resolve semantic merge conflicts; they do not bypass JSONL
syntax validation or unresolved git conflict markers.

---

## Rebuilding From JSONL

Use `--rebuild` only when JSONL is the source of truth and the SQLite database
should be made to match it:

```bash
br sync --import-only --rebuild
```

`--rebuild` is import-only. It is rejected with every non-import mode,
including `--flush-only`, `--merge`, `--status`, and `--witness`.
After importing JSONL, br removes database entries absent from JSONL and
preserves deletion tombstones when they are still needed for sync safety.

When rebuild is part of corruption recovery, br preserves the original database
family under `.beads/.br_recovery/` before creating the repaired database. These
artifacts are evidence for diagnosis; inspect them before pruning anything.

If `--rename-prefix` is combined with rebuild, imported IDs may be rewritten to
the configured prefix. In that mode, br skips set-difference orphan cleanup
because the original JSONL IDs no longer match the rewritten database IDs. If
open-time recovery already rebuilt the database before `--rename-prefix` could
apply, br reports a rerun command with the needed flags.

## Lossless Additive Recovery

Use additive reconciliation when valid JSONL contains rows missing from SQLite
but SQLite also contains rows or audit events that must not be discarded:

```bash
# Read-only plan with complete source/database witnesses
br sync --reconcile-additive --json

# Apply only the exact conflict-free plan that was reviewed
plan="$(br sync --reconcile-additive --robot)"
plan_sha256="$(printf '%s\n' "$plan" | jq -r .plan_sha256)"
br sync --reconcile-additive --apply \
  --expect-plan-sha256 "$plan_sha256" --robot
```

The dry-run opens the current-schema database read-only and does not take the
writer lock. Its v2 receipt includes an exact `plan_sha256`. The apply path
requires that token, takes `.beads/.write.lock`, rebuilds the identically
configured plan, and rejects a mismatch before mutation. It rechecks exact JSONL
bytes plus canonical content, size, mtime, issue payload, all relation rows,
events, close metadata, gate-result tables, config, dirty/export state, metadata,
SQLite schema/AUTOINCREMENT state, and independently projected derived caches.
Raw SQLite storage classes and semantic issue projections are separately
witnessed. Source and database health are rechecked inside the transaction
before commit.

The operation is deliberately additive: SQLite-only issues are retained and no
physical delete is available. Shared scalar drift is fail-closed except for a
narrowly defined monotonic closure. A reviewed operator may use
`--resolve-source-id` only for the documented non-lifecycle scalar whitelist,
and never when JSONL is older than SQLite. Relation drift and lifecycle or
tombstone transitions remain non-bypassable. The complete requested resolution
set, including inapplicable IDs, is part of the plan token. The operation never
writes the JSONL source, `.beads/beads.base.jsonl`, or a merge note. Preserved
SQLite-only state sets `needs_flush=true` rather than hiding divergence.

Use authoritative rebuild only when deleting SQLite-only state is intentional.
Additive reconciliation is the safer first recovery tool when both sides may
contain valuable evidence.

## Portable Source Repository Path Migration

Use the migration when valid SQLite and JSONL generations may each contain
valuable rows, but `source_repo_path` values came from another machine or
checkout:

```bash
# Read-only reconciliation and path-normalization plan
plan="$(br sync --migrate-source-repo-path --robot)"
plan_sha256="$(printf '%s\n' "$plan" | jq -r .plan_sha256)"

# Commit only the exact reviewed DB/JSONL generation
br sync --migrate-source-repo-path --apply \
  --expect-plan-sha256 "$plan_sha256" --robot
```

The plan imports JSONL-only rows, takes a strictly newer shared payload from
the newer side, preserves SQLite tombstones, and rejects equal-timestamp
semantic drift. Every surviving row receives the canonical directory that
contains the active `.beads/` folder in `source_repo_path`; the portable
`source_repo` display name is not derived from or replaced by that path.

Apply uses the same durable publication saga as three-way merge: the database
transaction records a hash-bound pending receipt, JSONL is conditionally
published from that exact database post-state, export bookkeeping and the base
snapshot are witnessed, and the receipt is cleared only after command-level
adoption. An interruption is therefore recoverable on the next migration or
merge invocation. This is a crash-recoverable commit-both protocol, not a claim
that SQLite and filesystem rename share one physical transaction.

---

## External JSONL Paths

By default, sync operates on `.beads/issues.jsonl`. To use a different path:

```bash
# Set via environment variable
export BEADS_JSONL=/path/to/issues.jsonl
br sync --flush-only --allow-external-jsonl
```

Paths outside `.beads/` require the explicit `--allow-external-jsonl` opt-in.

**Backups:** When exporting to a JSONL file that lives inside `.beads/` (including custom
`BEADS_JSONL` paths that still target `.beads/`), br creates timestamped backups in
`.beads/.br_history/` before overwriting.

**Safety notes:**
- External paths bypass the default confinement
- Symlinks pointing outside `.beads/` are rejected
- If import preflight rejects a path, it stops before opening or parsing that path
- Automatic flush validates the JSONL target before inspecting an existing file
- Startup auto-import and no-db prefix inference validate existing JSONL targets before hashing or reading them
- `br sync --allow-external-jsonl` carries that path policy through startup recovery, config loading, and no-db startup imports
- Paths are canonicalized before use

---

## Typical Workflow

### Starting a session
```bash
br sync --status           # Check if import is needed
br sync --import-only      # Import any JSONL changes
```

### Ending a session
```bash
br sync --flush-only       # Export DB changes to JSONL
git add .beads/            # Stage for commit (manual!)
git commit -m "Update issues"
```

### After pulling changes
```bash
git pull
br sync --import-only      # Import collaborators' changes
```

---

## Error Messages and What They Mean

### "Refusing to export empty database..."

**Cause**: Your database has 0 issues, but the JSONL file has existing issues.

**Fix**:
- Run `br sync --import-only` first to populate the database
- Or use `--force` if you intentionally want an empty export

### "Refusing to export stale database..."

**Cause**: The JSONL file contains issues that don't exist in your database.

**Fix**:
- Run `br sync --import-only` first to import the missing issues
- Or use `--force` if you intentionally want to lose those issues

### "Merge conflict markers detected..."

**Cause**: The JSONL file contains unresolved git merge conflicts.

**Fix**:
- Open the JSONL file and resolve the conflicts manually
- Look for `<<<<<<<`, `=======`, and `>>>>>>>` markers
- `--force` will NOT bypass this check

---

## Why These Guardrails Exist

### The Incident That Shaped br

The Go predecessor (`bd`) suffered a catastrophic failure where `bd sync` **deleted all repository source files**. This wasn't a theoretical risk—it actually happened, destroying irreplaceable work. The root cause was a sync operation that had too much authority: it could execute git commands, modify arbitrary files, and make irreversible changes without explicit confirmation.

This incident motivated every design decision in `br`'s safety model.

### Defense in Depth

`br` employs multiple layers of protection:

| Layer | Protection | Failure Mode Blocked |
|-------|------------|---------------------|
| **No sync git operations** | `br sync` has no runtime git subprocess path | Eliminates the primary attack vector from the original incident |
| **Sync write allowlist** | Default writes stay in `.beads/`; external JSONL writes require opt-in | Prevents accidental modification of source code, configs, or system files |
| **Path validation** | Rejects `.git`, traversal (`../`), symlink escapes, and disallowed extensions | Blocks path injection attacks and symlink-based escapes |
| **Checked publication and transactions** | JSONL/base/manifest publication uses checked temporary replacement; database mutations use transactions and operation-specific rollback | Prevents partial publication and partial database mutation |
| **Safety guards** | Empty DB and stale DB guards require `--force` to override | Makes destructive operations explicit and intentional |

### How Tests Enforce Safety

The safety model is backed by an extensive test suite that ensures these guarantees cannot regress:

- **Path guard unit tests** (`sync::path::tests`): 22 tests verify that traversal attempts, external paths, and disallowed file types are rejected
- **File tree snapshot tests** (`e2e_sync_git_safety.rs`): Integration tests take complete snapshots of the directory tree before and after sync, verifying that only `.beads/issues.jsonl` and related files are touched
- **Authority sentinel matrix** (`e2e_every_sync_mode_has_zero_git_authority_and_zero_git_mutation`): supported sync operations and distinct option branches run with a fake `git` first on `PATH`, detecting ordinary executable-name dispatch; every invocation compares the complete `.git` tree byte-for-byte with no exceptions. The source and runtime-dependency guards cover absolute-path, shell, linked-library, and sibling-adapter authority surfaces separately
- **Fail-closed source validation** (`SyncSafetyValidator::validate_no_git_authority_in_sync_sources`): recursively scans both `src/sync/**/*.rs` and `src/cli/commands/sync.rs`, rejecting subprocess construction, inclusion escape hatches, Git libraries, process-capable CLI delegation, missing/unreadable/non-UTF-8 paths, symlinks, and unsupported special filesystem entries; a parsed Cargo-manifest check rejects direct normal and target-runtime Git-library edges, while `cargo tree -e normal` is the separate transitive runtime-closure gate
- **Atomic write tests** (`e2e_sync_failure_injection.rs`): Tests inject failures mid-export to verify the original file is preserved
- **Conflict marker tests**: Import preflight tests verify that merge conflicts are detected and rejected

### How Logging Aids Diagnosis

When sync operations occur, structured logging records safety-critical decisions:

```bash
# Enable verbose logging to see safety checks
br sync --flush-only -v
br sync --flush-only -vv  # Even more detail
```

Key logged events:
- Path validation results (allowed/rejected with reason)
- Conflict marker scan results
- Export guard trigger events (empty DB, stale DB)
- Atomic write operations (temp file creation, rename)

If a safety guard triggers unexpectedly, the verbose log will show exactly why.

### The Core Guarantee

With default `.beads/` paths, sync is deliberately constrained to its storage
allowlist and has no intended process, Git-library, or CLI-adapter authority.
External JSONL access requires explicit opt-in.

This is defense-in-depth regression evidence, not a proof against arbitrary
future code outside the inspected boundary or a compromised absolute-path
executable. The maintained evidence consists of:

1. Path validation that rejects `.git` and confines default writes to the sync
   allowlist.
2. A fail-closed static scan of the complete declared sync source boundary and
   direct normal/target runtime dependency declarations, plus a strict
   `cargo tree -e normal` review for the resolved transitive runtime closure.
3. A Unix runtime PATH sentinel plus byte-exact `.git` snapshot matrix over
   supported sync branches.

---

## Further Reading

For technical details, see:
- `.beads/SYNC_THREAT_MODEL.md` - Incident analysis and failure scenarios
- `.beads/SYNC_SAFETY_INVARIANTS.md` - Testable safety invariants
- `.beads/SYNC_CLI_FLAG_SEMANTICS.md` - Flag matrix and opt-in rules

---

*This document is part of the br safety hardening initiative.*
