# wal_oversized_checkpoint

- **FM**: `fm-state_files-wal-oversized` (P2) — `.beads/beads.db-wal`
  exceeds the 32 MB oversized threshold. A WAL that grows unboundedly
  means SQLite's auto-checkpoint has been blocked (long-running read
  snapshot, peer process holding the WAL open) — disk usage climbs
  and crash recovery slows.
- **Subsystem**: state_files
- **Detect**: `wal_size` check goes to `warn` when the WAL sidecar
  size exceeds `WAL_OVERSIZED_BYTES` (32 MB). Pure `fs::metadata`
  size lookup — no DB open required to detect.
- **Repair contract**: SAFETY — `--repair` runs
  `PRAGMA wal_checkpoint(TRUNCATE)` against the selected SQLite
  database via the legacy chokepoint wrapper. Data-equivalent:
  the database's logical state is unchanged, only WAL storage shrinks.
- **Op variant**: `record_legacy_mutation` (NOT `Op::DbExec` —
  checkpoint pragmas must run outside an explicit transaction,
  which `Op::DbExec`'s `BEGIN IMMEDIATE` would violate). Records
  one `legacy_op` line in `actions.jsonl` per snapshot target
  (`beads.db`, `beads.db-wal`, `beads.db-shm`) under `fixer_id =
  doctor.wal_checkpoint_truncate`.

## What this fixture proves

This is the first Phase 9 fixture to exercise the legacy
chokepoint path (`record_legacy_mutation`) end-to-end:

1. Detector correctly identifies oversized WAL (`detect` stage:
   stat the planted 33MB WAL file directly — see caveat below).
2. `--repair` invokes the chokepoint wrapper, which writes
   `legacy_op` entries to `actions.jsonl` with the correct
   `fixer_id`.
3. Workspace remains queryable post-repair (`br list --json` succeeds).
4. Doctor stops flagging `wal_size` after the fixer runs.

## SQLite WAL-lifecycle caveat

`br doctor` (Full mode) runs `check_sqlite_cli_integrity` which
spawns the `sqlite3` CLI against the LIVE database. The CLI opens
the DB and on close considers our zero-padded WAL "fully consumed"
(the valid prefix from `br create` is auto-checkpointed; the zero
tail is past-EOF garbage) and removes the file. This happens AFTER
`check_wal_oversized` has already recorded `wal_size warn` in the
report, so the fixer still activates correctly — but the chokepoint
snapshot taken at fixer-call time captures an absent WAL.

Consequences for this fixture:

- The `detect` stage does NOT call `br doctor --json` (which would
  remove the planted WAL before the harness's `--repair` runs).
  Instead it stats `.beads/beads.db-wal` on disk to verify the
  planted >32MB state.
- The `post_undo` stage cannot assert byte-deterministic restore
  of the 33MB inflated WAL, since the snapshot at fixer-call time
  was already empty. The undo path is therefore "soft" — it only
  asserts the workspace remains functional.

This is a SQLite WAL-format limitation, not a chokepoint defect.
The fixer's behavior on a truly oversized live WAL (with
un-checkpointed frames preserving the file across CLI invocations)
is covered by unit tests at `tests/...` in `doctor.rs`
(`test_check_wal_oversized_warns_on_oversized`,
`test_fix_wal_oversized_truncates`).
