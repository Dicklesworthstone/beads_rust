# Workspace Health Contract

Canonical reference for beads_rust workspace health classification.
Executable implementation: `src/health.rs`.

## Health Levels

| Level | Meaning | Operations Allowed |
|-------|---------|-------------------|
| **Healthy** | All invariants hold | All |
| **Degraded** | Derived state stale or minor drift | All (with advisory) |
| **Recoverable** | Primary data intact, DB corrupted | Read-only until recovery |
| **Unsafe** | Interchange data corrupted beyond repair | None until manual fix |

## Failure Taxonomy

### Primary Data (SQLite)

| Anomaly | Severity | Detection | Recovery |
|---------|----------|-----------|----------|
| `DatabaseMissing` | Recoverable | File stat | Rebuild from JSONL |
| `DatabaseNotSqlite` | Recoverable | Header check (first 16 bytes) | Rebuild from JSONL |
| `DatabaseCorrupt` | Recoverable | fsqlite open / integrity_check | Rebuild from JSONL |
| `DuplicateSchemaRows` | Recoverable | sqlite_master GROUP BY HAVING | Rebuild from JSONL |
| `DuplicateConfigKeys` | Recoverable | Config table duplicate probe | DELETE+INSERT dedup |
| `DuplicateMetadataKeys` | Recoverable | Metadata table duplicate probe | Harmonize rows on metadata write; doctor rebuild collapses duplicates |
| `NullInNotNullColumn` | Degraded | Schema-aware NULL scan | Backfill or rebuild |
| `WriteProbeFailed` | Recoverable | Rollback-only doctor write probe | Rebuild from JSONL before writes continue |

### Interchange Data (JSONL)

| Anomaly | Severity | Detection | Recovery |
|---------|----------|-----------|----------|
| `JsonlParseError` | Unsafe | Line-by-line parse attempt | Manual edit required |
| `JsonlConflictMarkers` | Unsafe | Scan for `<<<<<<<`/`=======`/`>>>>>>>` | Manual merge resolution |
| `DbJsonlCountMismatch` | Degraded | Compare issue counts | Re-export from DB |
| `DbJsonlIdSetMismatch` | Degraded | Compare the issue id sets on both sides | Additive reconcile or re-export, never delete |
| `JsonlNewer` | Degraded | Timestamp/hash comparison | Re-import to DB |
| `DbNewer` | Degraded | Timestamp/hash comparison | Re-export to JSONL |
| `ExportHashMismatch` | Degraded | Compare stored hash vs computed | Re-export |

### Sidecars (WAL/SHM/Journal)

| Anomaly | Severity | Detection | Recovery |
|---------|----------|-----------|----------|
| `WalCorrupt` | Recoverable | WAL header validation | Preserve the whole database family; validate JSONL authority before a receipt-bound rebuild |
| `SidecarMismatch` | Degraded | SHM exists without WAL, a sidecar is not a regular file, or sidecars exist without a database | Inspect the family and active openers; quarantine only through an applicable repair under write authority |
| `TruncatedWal` | Recoverable | WAL file is 1–31 bytes (zero bytes is a normal checkpoint result) | Preserve the family; use the existing quarantine/rebuild path after validating authority |
| `JournalSidecarPresent` | Degraded | File existence check | Preserve the journal with its database; inspect before choosing recovery |
| `StaleRecoveryArtifacts` | Degraded | Recovery temp files present | Retain incident evidence; review aged artifacts through the doctor repair plan |
| `OrphanedLockFile` | Degraded | `.beads.lock` file stat | Verify liveness and inspect the doctor repair plan; file presence alone does not prove the lock is abandoned |

A WAL or journal can contain committed or recoverable data absent from the main
database. Do not delete it to clear a diagnostic. Capture `br doctor --bundle`
with `--include-db` when database bytes are needed, review
`br doctor --repair --dry-run --json`, and follow the
[engine operating model](ENGINE_OPERATING_MODEL.md#4-database-family-and-sidecar-inventory).
Keep the original family and recovery artifacts until the incident is resolved;
any eventual removal is an operator decision. These recovery choices also apply
to explicitly configured database families outside `.beads/`.

### Derived State

| Anomaly | Severity | Detection | Recovery |
|---------|----------|-----------|----------|
| `BlockedCacheStale` | Degraded | Metadata key check | Lazy rebuild on next read |
| `BlockedCacheContentMismatch` | Degraded | Recompute the blocked set from the dependency graph and compare with `blocked_issues_cache` | Rebuild the cache |
| `ReadyProjectionContentMismatch` | Degraded | Recompute the ready projection and compare with the cached projection | Rebuild the projection |
| `ChildCountDrift` | Degraded | Compare stored vs actual dep count | Recompute |
| `DirtyFlagMismatch` | Degraded | Compare flag vs actual dirty state | Reset flag |

## Invariant Matrix

Each row is a workspace component; columns indicate which subsystem owns and validates it.

| Component | Owner | Startup Check | Write-Path Check | Sync Check | Doctor Check |
|-----------|-------|---------------|------------------|------------|--------------|
| SQLite header | storage | `open()` | - | - | integrity_check |
| Schema version | storage | `apply_migrations()` | - | - | schema version match |
| Issue rows | storage | - | `update_issue` | export | count / sample |
| Issue writeability | storage | - | mutation transaction | - | rollback-only write probe |
| Config KV | storage | `get_config` | `set_config` | - | duplicate probe |
| Metadata KV | storage | `get_metadata` | `set_metadata` | - | duplicate probe |
| WAL sidecar | fsqlite | implicit | implicit | - | existence + size |
| SHM sidecar | fsqlite | implicit | implicit | - | existence |
| Journal sidecar | fsqlite | - | - | - | existence |
| JSONL file | sync | - | - | parse + export | conflict markers |
| Export hash | sync | - | - | compare | compare |
| Dirty flag (needs_flush) | sync | staleness probe | set on write | clear on flush | compare |
| Blocked cache | storage | lazy rebuild | refresh after mutation | - | stale marker |
| Child counters | storage | - | update on dep add/remove | - | count vs query |
| Dependencies table | storage | - | add/remove_dependency | - | FK integrity |

## Observability Contract

`br doctor --json` includes machine-readable reliability records:

- `report.reliability_audit`: workspace classification evidence derived from `AnomalyClass`.
- `recovery_audit`: repair action, outcome, applied local actions, quarantine artifacts, and JSONL rebuild counts.

`br sync --status --json` carries the same write-gate fields (beads_rust#334):

- `workspace_health`: the same `healthy`/`degraded`/`recoverable`/`unsafe`
  vocabulary doctor emits, computed from the cheap signals available in
  sync-status context only — the shared file-state probes
  (`classify_file_state`: DB header, WAL/SHM/journal sidecars, JSONL
  conflict markers, orphaned locks) plus the DB↔JSONL drift booleans
  (`jsonl_newer` → `jsonl_newer`, `db_newer` → `db_newer`). It does NOT
  run the full doctor checklist, so doctor-only anomaly codes (count
  mismatches, integrity-check corruption, write-probe failures, …) never
  appear here; absence of a code means "not evaluated", not "passed".
- `reliability_audit`: the matching anomaly evidence record
  (`source: "sync.status"`, `anomalies[].code/severity/message`), in the
  same shape as `report.reliability_audit` from doctor.
- `git_export`: a compatibility slot, not a health probe. Sync always emits
  `{available:false, reason:"not_probed", diagnostic_command:"br vcs-status --json"}`
  and omits the former optional observation fields. Consumers that need
  tracked/worktree/index/hash visibility must explicitly run the isolated,
  bounded `br vcs-status --json` diagnostic.

The same records are emitted through `tracing` with target `br::reliability` so field logs can be correlated with doctor JSON, quarantined artifacts, and replay fixtures.

## Reliability Gate Contract

CI and release verification must run these suites before a change can ship:

- `cargo test --test workspace_failure_replay -- --nocapture`
- `cargo test --test e2e_sync_failure_injection -- --nocapture`
- `BR_LONG_STRESS_ITERATIONS=8 cargo test --test e2e_workspace_scenarios scenario_long_lived_single_workspace_stress_suite -- --nocapture`
- `cargo test --test e2e_concurrency e2e_interleaved_command_families_preserve_workspace_integrity -- --nocapture`

These gates cover the failure-corpus replay, crash-injection matrix, long-lived
workspace stress, concurrent command-family stress, and doctor/recovery
postconditions. Release builds depend on the gate job by default; the manual
release workflow exposes an emergency override that requires a written reason
before artifacts can be built without the gates.

## Evidence Bundle (Incident Capture)

When a field failure occurs, the following artifacts should be collected for diagnosis:

1. **`beads.db`** - Full database file (or SHA-256 if too large)
2. **`beads.db-wal`** - WAL sidecar if present
3. **`beads.db-shm`** - SHM sidecar if present
4. **`issues.jsonl`** - Full JSONL interchange file
5. **`br doctor --json`** - Structured diagnostic output
6. **`br doctor --repair --dry-run --json`** - Projected repair actions
7. **Environment**: OS, fsqlite version, beads_rust version
8. **Timeline**: last successful operation, operation that failed, error message
9. **`.br_history/`** - Recent JSONL backups (last 3)
10. **`metadata` table dump** - All key-value pairs
11. **`sqlite_master` dump** - Schema state

### Capture Command

```sh
br doctor --bundle /tmp/incident-$(date +%Y%m%d-%H%M%S).tar.gz
# add --include-db to attach beads.db/-wal/-shm/-journal bytes,
# --include-jsonl to attach issues.jsonl (e-mail addresses redacted)
```

The bundle (`br.doctor.bundle.v1`, a gzip'd tar; `--json` prints a receipt
listing every member) covers the list above:

| Item | Member |
|---|---|
| 1-3 database family | `db-family.json` (presence, size, mtime, SHA-256 of every family member, certificates and namespace files included); bytes under `db/` only with `--include-db` |
| 4 `issues.jsonl` | `issues.jsonl.summary.json` (SHA-256, size, record count); the file itself only with `--include-jsonl`, redacted |
| 5 `br doctor --json` | `doctor.json` (+ `doctor.json.stderr.txt` when stderr was non-empty; the exit code is in `manifest.json`) |
| 6 `br doctor --repair --dry-run --json` | `doctor-repair-dry-run.json` |
| 7 environment | `manifest.json` (`br_version`, `git_sha`, `platform`, `engine` block with the engine version and sidecar inventory), `version.txt` |
| 8 timeline | `db-dump.json` `recent_events` (newest `--bundle-events N`, default 200); the failing command and its output are still the reporter's to add |
| 9 `.br_history/` | `listings.json` (names, sizes, mtimes of `.beads/`, `.beads/.br_recovery/`, `.beads/.br_history/`; no backup bytes) |
| 10 `metadata` table | `db-dump.json` `metadata` (read-only engine connection; an open failure is recorded, never fatal) |
| 11 `sqlite_master` | `db-dump.json` `sqlite_master` |
| — | `health.json`, `sync-status.json`, `where.json`, `config-list.txt`, and copies of `metadata.json` / `config.yaml` |

Every text member has e-mail addresses replaced with `<redacted-email>`.
The command never overwrites an existing file and takes no workspace lock
itself (the captured commands take their own, exactly as a hand-run would).

## Severity Escalation Rules

- Multiple Degraded anomalies do NOT escalate to Recoverable
- Any single Recoverable anomaly blocks writes until resolved
- Any single Unsafe anomaly blocks ALL operations
- Composite health = max(individual severities)

## Contract Versioning

This contract is versioned alongside the `AnomalyClass` enum in `src/health.rs`.
Adding a new variant is backwards-compatible. Changing severity of an existing
variant requires a migration note in the changelog.
