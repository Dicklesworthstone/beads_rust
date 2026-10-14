# Missing SHM with committed WAL frames (GH #562)

A missing shared-memory index is not evidence that the write-ahead log is
expendable. On Unix, a tracker can remain readable through br's private
snapshot while writable admission and `doctor migrate-schema recover` refuse
with `SYNC_CONFLICT`. The WAL may contain the only copy of committed changes.
This is the intentional protection for [GH #556](https://github.com/Dicklesworthstone/beads_rust/issues/556),
not permission to discard the WAL or rebuild the database from JSONL.

## Recover into a new database without changing the original

From a repository checkout, on 64-bit Linux with Python 3.10+ and its stock
`sqlite3` module:

```sh
python3 scripts/recover_orphaned_wal.py \
  --db '/absolute/path/to/selected.db' \
  --output '/absolute/path/to/new-recovery-directory'
```

Stop all br and external SQLite users first. The tool obtains both br's
inode-authority byte and SQLite's main-file lock range, nonblocking, and
retains them throughout capture and verification. No source database is
opened through SQLite, written, checkpointed, renamed, deleted, or chmodded.
A write-capable main-file descriptor is necessary for the exclusive OFD lock,
not for writing database bytes. Inputs must be regular single-link files.
Source directory ancestry must be trusted; advisory locks do not defend
against hostile software deliberately ignoring the lock protocol.

The output directory must not exist. A successful exit emits a JSON receipt
naming `<output>/beads.db`, a standalone reconstructed database with the last
committed WAL page images already in its main file. This is a functional
copy-recovery path, **not automatic in-place repair and not a new br backend**.
The runtime backend remains FrankenSQLite. Python's stock SQLite is used only
as an independent verifier on private copies; no sqlite3 shell is invoked.

The implementation validates the entire WAL's header, salts, chained
checksums, page sizes/numbers, frame boundaries and final commit. It refuses
corrupt, partial, old-generation or uncommitted tails rather than silently
salvaging a valid prefix. It independently reads a private main-plus-WAL copy
with stock SQLite and requires both integrity checks and equality of schema,
typed table data, accessible hidden rowids, user version and application ID.
A main-only copy that is structurally valid but lacks the WAL commit fails.
Virtual tables, inaccessible hidden rowids, nonempty rollback/migration state,
and unknown `database-name-*` sidecars require separate engine review.

The default `--max-bytes` is 1073741824 (1 GiB) per input family component and
for the recovered database; an explicit larger value is possible. Allow space
for the preserved family, independent oracle, and recovered image. Capture and
publication retain private permissions (directories 0700, files owner-only).
The tool does not copy workspace configuration, JSONL, or policy files.

`before/` preserves every recognized source family component, including
certificates and namespace records. These engine-specific sidecars are not
interpreted, transferred to the new database identity, or used as proof of
classic-WAL validity. `prepared.json` and `verified.json` retain raw hashes,
file identities/metadata, and logical witnesses. `complete.json` records a
finished recovery. Publication uses Linux `RENAME_NOREPLACE`, not a replacing
rename, and synchronizes files/directories. Existing outputs are never
clobbered, including a competing entry appearing during recovery.

On refusal, disk failure, or interruption, leave any `.br-wal-copy-*` staging
folder and its evidence intact. A nonzero exit, `failed.json`, or a missing
`complete.json` is not success. A process can die after directory publication
but before the completion receipt: never adopt that uncompleted candidate.
Retry the untouched source using another new destination after investigating.

Verify expected issue records before adopting the copy, especially recently
closed issues and their `closed_at` and `close_reason`. WAL checksums cannot
prove that a valid foreign WAL was originally paired with this main file.
Matching the independent reader proves equivalence to the supplied family's
classic SQLite state, not provenance or recovery of changes absent from it.
An operator can inspect the completed candidate without importing ambient JSONL:

```sh
br --db '/absolute/path/to/new-recovery-directory/beads.db' \
  --no-auto-import --no-auto-flush list --all --json
```

Keep the original and all evidence. Do not overwrite the original file, copy
old namespace/certificate sidecars over the new identity, or resume a shared
workspace against the new database until its br reads and a deliberate write
have been qualified. The tool does not switch a workspace automatically.

## Inspect the selected database without recovering it

```sh
br --db '/absolute/path/to/selected.db' info --json
br --db '/absolute/path/to/selected.db' doctor --json
```

The engine block includes `wal_recovery` when a regular WAL is longer than its
32-byte header and the on-disk SHM is absent or shorter than its two headers.
It reports `code: WAL_CHECKPOINT_REQUIRED`, `reason: missing_shm` or `short_shm`,
the sampled WAL byte count, and these explicit conditions:

- `automatic_recovery_supported: false`
- `requires_quiescence: true`
- `checkpoint_success_row: 0|0|0`
- `integrity_check_success_row: ok`

`checkpoint_argv` and `integrity_check_argv` target the selected database, not
ambient `BEADS_DIR` or an assumed `beads.db`. Human output also includes quoted
POSIX-shell commands. The shell dialect is labelled; argv is the portable
representation. Non-UTF-8 paths do not produce lossy executable commands.

This is metadata-only advice, not a WAL validator, lock acquisition, or repair
operation. It does not assert that the frames are complete, committed, or
uncheckpointed. Even one partial frame warrants preservation. A missing advice
block does not establish health: corrupt initialized indexes and unreadable or
non-regular sidecars require separate investigation. This diagnosis is disabled
where the engine does not use an on-disk WAL index.

## Operator-controlled checkpoint

The workaround in [GH #562](https://github.com/Dicklesworthstone/beads_rust/issues/562)
was verified with stock SQLite 3.46.1. br does not invoke it automatically.

1. Stop **all** br processes and external SQLite users, including reader TUIs,
   and keep them stopped through preservation, checkpoint, and verification.
   A free `.br-db-openers-*` lease does not establish this condition: stock
   SQLite does not participate in that lease.
2. Preserve the complete stopped database family in a separate location. Keep
   the main file, WAL, any SHM and rollback journal, and the engine's certificate
   and namespace sidecars together. Do not delete sidecars, overwrite evidence,
   or copy only the main file as a purported backup of a WAL-bearing database.
3. Make another disposable family copy and rehearse there first. Point br at
   the copy to obtain commands for that copy. Verify expected records from an
   independent observation before and after; an old JSONL mirror is not proof.
4. Only after the rehearsal passes and the original remains quiescent, use the
   reported command for the original selected path. For a simple POSIX path:

   ```sh
   sqlite3 -batch -bail '/absolute/path/to/selected.db' 'PRAGMA wal_checkpoint(TRUNCATE);'
   ```

   Require the returned row to be **exactly `0|0|0`**. A zero process exit code
   is insufficient: a busy checkpoint can return a busy row without a shell
   error. A failed or partial checkpoint can have backfilled some pages; do
   not describe it as a byte-preserving refusal, delete its WAL, or claim
   recovery succeeded. Retain evidence and investigate contention or damage.
5. Run the reported read-only `PRAGMA integrity_check;` command and require
   `ok`. Also verify expected committed records, especially `status`,
   `closed_at`, and `close_reason`. A structurally valid database can still be
   missing a committed change; integrity alone cannot detect that loss.
   Confirm ordinary br reads and a deliberate write work before resuming other
   database users. Retain the pre-recovery family until verification is done.

The commands are advice sampled at diagnosis time, not identity-bound handles.
Do not execute them blindly after moving/replacing a database, changing its
path, or allowing another process to reopen it. Avoid normal br writes before
checkpoint verification; they remain guarded rather than silently repairing.

## Why the existing lease is not enough for automatic recovery

`src/sync/db_inode_lock.rs` deliberately places br's ordinary write-authority
byte outside SQLite's lock range. Its separate `RecoveryLock` instead takes
an OFD write lock over SQLite's main-file pending/reserved/shared range. That
excludes external SQLite users, including arriving connections, but it also
conflicts with a new SQLite connection opened by the recovery code itself.
`with_recovery_exclusion` consequently requires a filesystem-only callback.
Dropping that guard before opening an engine would create an exclusion gap.

A safe automatic path still needs a qualified engine-level lock handoff or
checkpoint primitive, correct WAL/certificate handling, durable crash behavior,
and retained pre/post evidence. Passing WAL checksums or a private-copy
checkpoint is not sufficient authority to mutate a live family. This change
does not relax `require_frame_free_recovery` or any #556 payload guard.

See SQLite's [checkpoint API](https://sqlite.org/c3ref/wal_checkpoint_v2.html)
and [WAL format and locking](https://sqlite.org/walformat.html) for the engine
semantics underlying the quiescence and result checks.

## Regression qualification

```sh
python3 tests/e2e_scripts/test_recover_orphaned_wal.py
python3 tests/e2e_scripts/test_recover_orphaned_wal.py --br target/debug/br
python3 tests/e2e_scripts/wal_recovery_562.py --self-test
python3 tests/e2e_scripts/wal_recovery_562.py --br target/debug/br
```

The copy-recovery suite executes the production recovery script with real
SQLite-generated WALs: 28 tests passed on 64-bit Linux with SQLite 3.46.1.
Coverage includes two checksum byte orders, 512/65536-byte pages, multiple
commits and committed shrink, source identity exchange, real lock contention,
no-clobber races, rejected lost-WAL candidates, retained permissions/evidence,
and process death during replay, verification, and publication. Its `--br`
mode additionally generates a tracker with the actual binary, preserves a
WAL-only closure, runs copy recovery, and requires successful br reads and a
new write while source and ambient families remain unchanged. That mode is
wired into CI but was not run in the editing environment (no br executable).

The checkpoint harness's `--self-test` exercises real stock-SQLite commits and
checkpointing, busy result rejection, a main-only post-checkpoint observation,
full logical-data comparison, and the Linux OFD exclusion boundary. It **does
not test br**. Its `--br` mode uses the actual executable for missing-SHM
diagnostics, unchanged read/refusal families, absolute/relative `--db`, quoted
filenames, ambient workspace isolation, disposable rehearsal, and a
post-checkpoint br write. Neither harness accepts a user-supplied database;
observations and logs remain in new temporary directories. Existing #556
regressions remain intact. CI runs the copy-only suite before the Rust build,
the actual-br modes after it, and retains all fixture/evidence directories.

Native Cargo checks, Clippy, formatting, inline Rust tests, and the actual-br
modes remain required for br qualification. Stock-SQLite-only results must
never be reported as a passing br build or proof of safe automatic recovery.
