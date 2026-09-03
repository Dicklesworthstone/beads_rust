# Storage Engine Operating Model

**Status:** current as of 2026-09-02 (fsqlite 0.3.14, br 0.5.7 + main)
**Owner bead:** `beads_rust-dk45` (Track B of the 2026-09-01 bridge plan)

This document is the record of how `br` relates to its storage engine, what
went wrong in August 2026, what contains it today, and what must pass before
the engine is changed again. It exists because the August incident was
reconstructed from one bead's comment trail; nothing in `docs/` said any of
this.

---

## 1. The decision: FrankenSQLite only, no C SQLite, no FFI

`br` links no C SQLite. The storage engine is
[FrankenSQLite](https://github.com/Dicklesworthstone/frankensqlite) (`fsqlite`
family of crates, pinned in `Cargo.toml`, published on crates.io), driven
through the synchronous `src/franken_sync.rs` bridge over the engine's async
API.

Why:
- memory safety end to end (`unsafe_code = "deny"` in `Cargo.toml`, with four
  documented carve-outs none of which touch the engine);
- a single Rust toolchain for build, test, release, and `cargo install --git`;
- concurrent-writer support the classic engine does not offer.

What would change the decision: an engine defect that cannot be contained on
the br side **and** cannot be fixed upstream. In August 2026 a stock-SQLite
backend was built as an emergency alternative (`811c8277`, `783c1140`, released
once as v0.5.4) and the operator rejected it; it was reverted in `a704e8b8` and
`be9fc296`. That rejection is the standing decision.

## 2. What happened in August 2026

| Date | Event |
|---|---|
| 2026-08-27 | v0.5.3 (fsqlite 0.3.11) malforms migrated databases under concurrent multi-process writes: GH #457 (page aliasing), #458 (field-shifted import record), #460 (freelist corruption), #461 (`comments add` destroying prior comment bodies in `issues.jsonl`). |
| 2026-08-28 | Root cause isolated upstream: FrankenSQLite's multi-process checkpoint did not register against peer processes' read snapshots (frankensqlite #399; #385 and #329 own the fix). The engine-side discriminator showed four concurrent br-shaped processes stay clean when they never checkpoint, corrupt after several rounds with PASSIVE checkpoints at exit, and corrupt in the first round with the `wal_checkpoint(TRUNCATE)` br ran at process exit. |
| 2026-08-28 | Containment landed in `dedfbed7` (see §3). Stress receipts on worker hz3 against the real 975-issue family: before, 8 workers × 60 s left 32 self-heal recovery artifacts and "invalid B-tree page type flag: 0x00" reads; after, 8 × 60 s and 8 × 90 s ended with `integrity_check` ok, DB == JSONL, zero new artifacts, zero corruption signatures. |
| 2026-08-29 | `v0.5.4` (tag on `47fd9d0e`, the stock-SQLite build with `rusqlite 0.40.2 bundled`, no `fsqlite` dependency) was published at 04:39 UTC and superseded about an hour later by v0.5.5 on FrankenSQLite 0.3.12 with the containment below; v0.5.6 (0.3.12) and v0.5.7 (0.3.13) followed the same day. Anyone still on v0.5.4 is running the rejected backend and should upgrade. |
| 2026-09-01 | fsqlite 0.3.14 (`ebc34bd7`). GH #476 (read-only inspection appearing to write the header) traced to the WAL-index reader-mark array, which any WAL-correct reader must write; contract restated in `3d4fdc0f` and pinned at the storage layer by `beads_rust-dk45.2`. |

## 3. Containment: checkpoints only as the provable sole opener

Implemented in `src/sync/mod.rs` (`DatabaseOpenerLease`) and
`src/storage/sqlite.rs` (`admit_checkpoint`, `CheckpointAdmission`).

- Every persistent open of a database holds a **shared opener lease**, the
  file `.beads/.br-db-openers-<hash>.lock` beside the database (sibling of
  `.br-db-write-<hash>.lock`), for the lifetime of the storage handle.
- The periodic PASSIVE checkpoint, `checkpoint_full` at quiescent points, and
  the exit-time TRUNCATE (`SqliteStorage::drop`, only when the handle made
  mutations, #270) first **upgrade to the exclusive hold** and are **skipped
  when another process has the database open** (`CheckpointAdmission::PeersPresent`).
- New openers wait out an in-flight exclusive hold, so no process starts
  reading a WAL that is being reset.
- The lease is advisory. It degrades to "checkpoints disabled", never to a
  blocked command. Read-only commands leave `mutation_count` at zero and never
  checkpoint on teardown.

Consequence for operators: under a busy swarm the WAL can grow because
checkpoints are skipped while peers are present; `br doctor` reports `wal_size`
and the sole-opener state (`beads_rust-dk45.5` adds an `engine` block with the
lease holder).

## 4. Database family and sidecar inventory

FrankenSQLite creates these beside any database path it opens, including
`VACUUM INTO` temp targets (`src/config/mod.rs`, single source of truth for
the suffix lists; `doctor`'s family walk reads the same constants):

| File | Owner | Purpose | Doctor treatment |
|---|---|---|---|
| `beads.db` | br | main database | `db.exists`, `db.open`, `sqlite.integrity_check` |
| `beads.db-wal`, `-shm`, `-journal` | engine | classic WAL / WAL-index / rollback journal | `db.sidecars`, `wal_size`; `-shm` reader marks (offsets 100..120) are the one thing a read-only open may write |
| `beads.db-wal-cert`, `-wal-cert-head` | engine (0.2+) | parallel-WAL durability certificates | derived state; a certificate written by a different engine generation makes every cert-regenerating write fail while reads stay healthy (GH #441); br quarantines it into `.br_recovery/` so the engine regenerates it |
| `beads.db-fsqlite-ns-gate`, `-fsqlite-ns-use` | engine (0.1.18+) | multi-process namespace admission | refused by the engine when any `0o077` mode bit is set (reads as "unable to open database file"); the lock-free read-only opener declines instead of chmod-ing; ordinary open repairs the mode under authority |
| `beads.db.fsqlite-migration-state` | engine | migration bookkeeping | carried with the family |
| `.br-db-write-<hash>.lock`, `.br-db-openers-<hash>.lock` | br | write authority and opener lease | `write_lock`, engine block |
| `.br_recovery/` | br | forensic backups taken before recovery rebuilds (whole family) | `db.recovery_artifacts` (info), `db.recovery_artifacts.aged` (warn past `RECOVERY_AGED_TTL_DAYS = 30`), `db.foreign_recovery_debris` |
| `.br_history/` | br | bounded JSONL snapshots (`br history`) | `br_history.size` |

Recovery artifacts are never removed automatically; `br doctor --repair`
offers to quarantine only the aged ones. Removal is an operator decision.

## 5. Read-only contract (GH #476)

A current-schema read-only open (`SqliteStorage::open_current_read_only`) and
every inspection built on it leave the main file, `-wal`, and `-journal`
byte-identical and may change `-shm` only inside the WAL-index reader-mark
array (`SHM_READ_MARK_RANGE`). The contract is enforced three ways:

- `src/storage/sqlite.rs` tests `open_current_read_only_is_observational_*`
  (no WAL, live uncheckpointed WAL, leftover `-shm`);
- the doctor test for pending-merge inspection (`assert_database_family_read_only`);
- the runtime doctor check `db.read_only_open_observational`, which runs the
  same probe on a private copy of the family in every `br doctor`, so an
  engine bump that starts writing on open is caught on the installed engine
  rather than in a user's workspace.

## 6. Engine bump checklist

Before merging any change to the `fsqlite*` lines in `Cargo.toml` (Dependabot
or manual):

1. Read the fsqlite changelog for pager, WAL, B-tree, checkpoint, or VFS
   changes and note them in the PR.
2. `cargo test --lib` green (through RCH: `rch exec -- cargo test --lib`).
3. Stress gate: `scripts/br-stress.sh <br-binary> <real-.beads-dir> 8 60` and
   `... 8 90` against a real migrated family, not a fresh workspace; both must
   pass every post-condition listed in the script header (integrity ok, DB ==
   JSONL, no new `.br_recovery/` artifacts, no doctor errors, no unexpected
   stderr signatures). Attach both receipts to the PR.
4. `br doctor --json` on the stressed copy: `db.read_only_open_observational`
   and `db.sidecars` ok.
5. `cargo test --test model_based_storage` (random operation sequences against
   the engine-free reference model) and
   `cargo test --test linearizability_multiprocess -- --nocapture` (eight
   concurrent `br` process streams for 30 s; every per-issue history must
   linearize against the sequential model and the published JSONL must equal
   the linearized final state) green. Both run in the CI and release
   reliability jobs; a violation writes the merged history and the failing
   partition under `target/test-artifacts/linearizability/`.
6. Re-run the repro tests for the open engine escalations (§7); record which
   ones now pass and remove the matching workarounds in the same PR.

A release without these receipts is not a release.

## 7. Open upstream escalations

| Bead | Symptom in br | Upstream |
|---|---|---|
| `beads_rust-ro3m` | `SELECT COUNT(*) ... WHERE id IN (SELECT ... GROUP BY ... HAVING COUNT(DISTINCT label) = ?)` counts 0 when the labels and threshold are bound parameters (the literal statement is right; confirmed on fsqlite 0.3.15); br routes multi-label AND counting through candidate ids. Probe: `grouped_having_in_subquery_count_with_bound_params` (ignored; run with `--ignored` after an engine bump), guard: `multi_label_and_count_matches_list` | [frankensqlite#407](https://github.com/Dicklesworthstone/frankensqlite/issues/407), filed 2026-09-03 |
| `beads_rust-f3r4` | B-tree rowid-order corruption after 264 sequential dep-remove writes (GH #426) | not filed: the #426 sequence passes on fsqlite 0.3.15; `gh426_sequential_dependency_removals_keep_projections_and_integrity` (tests/model_based_storage.rs) guards it |
| `beads_rust-ajui` | migrate-schema 16→17 reports success but leaves the DB failing `integrity_check` (GH #428) | not filed: br-side fix landed (migration requires a clean fresh-connection integrity witness, `doctor_subsystems/schema_migration.rs`; `tests/e2e_schema_migration_upgrade.rs`); bead closed |
| `beads_rust-891u` | `VACUUM INTO` re-serializes DDL so the raw `sqlite_master` hash never matches the witness | not filed (re-serialized DDL is not an upstream defect once the witness ignores formatting); br-side landed: the witness hashes DDL tokens (`ddl_token_fingerprint`, `schema_witness_survives_vacuum_into_reserialization`) |
| `beads_rust-avhq` | orphaned `-wal-cert`/`-ns` sidecars wedge open when the DB file is absent | br-side landed: `quarantine_orphaned_sidecars` before the fresh install (`tests/repro_avhq.rs`) |
| resolved | GH #457/#460/#461 page aliasing under concurrent checkpoints | frankensqlite #399 (fix tracked in #385, #329); contained by §3 |
| resolved | trailing zero pages rejected where SQLite accepts them | `docs/fsqlite_trailing_pages_report.md` |

`beads_rust-dk45.6` gives each open row a `tests/repro_*.rs` and an upstream
issue link.

## 8. Escalation template

When filing upstream, include:

```
Engine: fsqlite <version> (Cargo.lock), br <version> (<commit>)
Platform: <os/arch>, filesystem <type>
Workload: <br-stress.sh args or command sequence>, N processes, duration
Symptom: <exact error text or integrity_check output>
Artifacts: .br_recovery/<ts>/ listing, doctor --json (engine block, db.* checks),
           byte offsets when a family file changed unexpectedly
Repro: <tests/repro_*.rs name or shell sequence, minimal>
Containment on br side: <what br does today to avoid it>
```

Link the upstream issue from the bead and from this table.
