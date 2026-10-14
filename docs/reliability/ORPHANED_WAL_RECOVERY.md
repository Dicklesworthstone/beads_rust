# Missing SHM with committed WAL frames (GH #562)

A missing shared-memory index is not evidence that the write-ahead log is
expendable. On Unix, a tracker can remain readable through br's private
snapshot while writable admission and `doctor migrate-schema recover` refuse
with `SYNC_CONFLICT`. The WAL may contain the only copy of committed changes.
This is the intentional protection for [GH #556](https://github.com/Dicklesworthstone/beads_rust/issues/556),
not permission to discard the WAL or rebuild the database from JSONL.

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
python3 tests/e2e_scripts/wal_recovery_562.py --self-test
python3 tests/e2e_scripts/wal_recovery_562.py --br target/debug/br
```

The first command exercises real stock-SQLite commits and checkpointing, busy
result rejection, a main-only post-checkpoint observation, full logical-data
comparison, and the Linux OFD exclusion boundary. It **does not test br**.
The second uses the actual executable for missing-SHM diagnostics, unchanged
read/refusal families, absolute/relative `--db`, quoted filenames, ambient
workspace isolation, disposable rehearsal, and a post-checkpoint br write.
Neither mode accepts a user-supplied database; observations and logs remain in
new temporary directories. The existing #556 harness remains unchanged.
The WAL regression workflow runs both harnesses and retains their evidence.

Native Cargo checks, Clippy, formatting, inline Rust tests, and the actual-br
mode remain required for qualification. Stock-SQLite-only results must never
be reported as a passing br build or proof that automatic recovery is safe.
