# Linked-engine attestation before source-index recovery

The packaged `br-wal-recover` requires its own linked FrankenSQLite to read
the proposed shared index before `--restore-shm` can install it, or
`--resume-from` can report successful revalidation. It also requires that
engine to complete a write, rollback and checkpoint rehearsal in a **separate,
new disposable database**. There is no companion flag or environment setting
to bypass these checks. Copy-only and explicit last-commit-copy modes retain
their existing verification contracts.

These are runtime gates, not evidence that a particular build has already
passed native tests or a live checkpoint implementation in ordinary `br`.
The prerequisites and source/evidence preservation rules in
[the recovery runbook](ORPHANED_WAL_RECOVERY.md) still apply.

## Protected read admission

The helper continues to hold the source database and namespace exclusions. It
makes another private main/WAL/SHM family under `native-rehearsal/`; the native
process receives that private path, never the selected source. For resume,
SHM comes from the actual installed index, not a newly regenerated substitute.
The parent retains the private index's dead-man-switch and recovery,
checkpoint and writer locks while the child reads. The supplied family's
native open is read-only and explicitly closes without checkpointing on both
result paths. A failed initial witness never enters the write rehearsal.

An independent stock-SQLite reader produces `expected.json`: schema, table
names, column projections, accessible hidden rowids, typed row counts/hashes,
user version and application ID. The wire format distinguishes null, integer,
IEEE floating-point bits, UTF-8 text and arbitrary blobs. Length-framed encoded
rows are sorted without dropping duplicates, so query ordering is irrelevant.
Stored shadow tables are covered; virtual cursor handles are not table data.
Existing exact-page attestation remains required for the previously admitted
virtual-table and shadowed-rowid schemas.

## Disposable native write and checkpoint rehearsal

The original #562 family was readable but unwritable. Read-only agreement is
therefore not enough for the updated companion to report native success.
After the protected read succeeds, the native process allocates a new
`native-write-*` sandbox beneath the private evidence directory. On Unix its
parent must be owner-controlled and not group/other-writable. Only that owned
sandbox object can open a writable engine connection; a caller-supplied input
path cannot be used as the SQL write target. All copies use no-clobber files,
no-follow regular-file inputs, byte limits and before/after hashes.

The linked engine performs this workload on the newly copied database:

1. Create a sandbox-specific table and commit a row containing integer, null,
   real, Unicode text and a binary payload. Close without checkpointing.
2. Open a fresh connection and require the exact committed row. Update it in
   an explicit transaction, roll back, and require the old row again.
3. Remove only the table this rehearsal just created. Compare every original
   schema/table/metadata witness to the independent **pre-write** manifest.
4. Execute a standalone `PRAGMA wal_checkpoint(TRUNCATE)` and require exactly
   one integer row `(0,0,0)`. Busy, partial, non-WAL and malformed rows refuse.
5. Make a second new copy containing **only the checkpointed main file**. A
   fresh native read must again match the full pre-write manifest and must
   leave that main image unchanged. Recheck the supplied input-family hashes.

The checkpoint is confined to the disposable sandbox. It neither truncates
the selected source WAL nor releases the parent's source/index exclusion to
run a live engine writer. This is not a new live-checkpoint recovery path.
Automatic checkpoints are disabled on rehearsal writers, and every admitted
connection is explicitly closed without an implicit checkpoint, including
failed verification paths. Failed and completed sandboxes are retained.

`native-write.json` uses schema `br.orphaned_wal.native_write.v1`, binds the
same manifest hash, and records the relative sandbox name, reopen/rollback
checks, private checkpoint row and verified main-only image hash. It explicitly
states `scope: fresh-private-copy` and `source_checkpoint_performed: false`.
The sandbox contains `prepared.json`, then either `complete.json` or a
stage-labelled `failed.json` when that failure record can be written. A failed
completion-file write still fails the native probe. Existing receipts are
never overwritten. Paths are relative in the completion record because the
outer evidence directory is subsequently published by a no-replace rename.

The primary native-probe v1 wire receipt remains compatible with the parent
parser. **This updated binary** emits `verified: true` only after both phases
and durable private completion evidence succeed. The schema string or the
engine version alone does not prove that an older verifier did this workload;
the existing executable-hash binding identifies the actual implementation.

## Parent verification and source installation

The parent requires one strict JSON receipt with the expected schema, engine
identity, known engine version and exact manifest hash, and binds the executable
hash. A nonzero exit, 60-second timeout, malformed/ambiguous receipt, changed
expectation or private main/WAL/index mutation refuses before source install.
Only normal private read-mark changes are allowed. Full WAL/index validation
and original source/evidence re-attestation still run independently.

The parent also requires the native write evidence at runtime, not only in
CI. A `verified: true` native-probe response without `native-write.json` is
refused. Preparation, completion, manifest binding, sandbox-specific probe
table, exact integer checkpoint tuple and the main-only image witness must
agree; conflicting failure evidence is a refusal. Evidence directories and
files must be owner-controlled, regular/no-follow where applicable, and bound
to their retained descriptors. Path traversal, extra hard links, ambiguous
JSON and an existing independent-check directory are not admitted.

After those checks the parent creates `native-rehearsal/independent-checkpoint/`
and copies only the native checkpoint's main file there. Stock SQLite reads
this fresh private image with `mode=ro&immutable=1`, checks integrity, and
recomputes the complete original schema/typed-row/metadata manifest. Native
sidecars cannot mask a missing main-file commit. Matching image hashes or a
healthy integrity result are insufficient if the original WAL-only data,
accessible rowids, value types or schema changed. No source SQL or checkpoint
is performed by this independent check. It finishes while source and namespace
exclusion is still held, before SHM installation or resume completion.

The retained `independent-checkpoint/verified.json` and the recovery receipt's
`native_probe.independent_write` contain schema
`br.orphaned_wal.native_write_check.v1`, the independent SQLite version,
manifest hash, native write-receipt hash, and verified main-only length/hash.
The parent rechecks native evidence identities and bytes after observation.
The independent image and proof remain with successful or failed evidence;
prior runs are never overwritten. Older read-only native replies cannot
satisfy this write-evidence requirement just by using the same v1 schema.

Receipts retain `native_probe`; its manifest, stdout/stderr logs and private
write evidence live in the new evidence directory. Source installation still
uses the existing no-replace, evidence-first path. A refusal is not permission
to delete SHM, truncate WAL, change old receipts or rebuild from JSONL.

The standalone Python operator script still defaults to stock-SQLite-only
verification. It does not acquire native proof merely because the packaged
companion supports this gate. Its hidden verifier hook is the companion's
internal handoff, not a claim that an arbitrary external program is trusted.

## Qualification and limits

`python3 tests/e2e_scripts/test_wal_native_gate.py` runs explicitly labelled
fake-responder controls for the parent protocol. The separate command
`python3 tests/e2e_scripts/test_wal_native_write.py --self-test` runs seven
stock-SQLite fixture/receipt-oracle controls, **not the Rust implementation**.
The latter passed during this change, including deliberate lost-WAL data with
valid integrity and matching image hashes, read-only-success substitution,
incomplete checkpoint tuples, incorrect manifest binding and path escape.

The parent-verification update ran all 30 gate controls and 21 existing resume
helper regressions on Linux, Python 3.13.5 and SQLite 3.46.1. Gate controls use
an explicitly fake native responder and real stock-SQLite checkpoint images;
they do not execute Rust. On the exact previous helper, two planted cases
were accepted: a read-only reply with no write evidence and a matching-hash,
integrity-valid main image missing its WAL-only commit. The updated runtime
rejects both. These are injected controls, not observed production data loss.
The existing workflow already invokes both the expanded controls and their
separate installed-executable mode. No workflow change is needed. Native
companion/actual-br execution, full-suite and power-loss qualification were
not performed during this parent-verification update.

Native qualification has no fake or interpreter fallback:

```sh
cargo test --locked --bin br-wal-recover
python3 tests/e2e_scripts/test_wal_native_gate.py \
  --recovery-binary /absolute/path/to/installed/br-wal-recover
python3 tests/e2e_scripts/test_wal_native_write.py \
  --recovery-binary /absolute/path/to/installed/br-wal-recover
```

Four new Rust tests cover checkpoint result shape, no-clobber/bounded copying,
unsafe input links and the missing-sandbox refusal. Four actual-companion
cases require restoration and resume to produce native write evidence, check
the resulting main-only image independently with stock SQLite, and test
pre-write manifest refusal and completion-receipt collisions. The workflow
runs these native cases only after successful companion installation, alongside
all prior actual-`br` read/write checks. Cargo, RCH, rustfmt and compiled
binaries were unavailable locally; native compilation/execution is still
unqualified, not a claimed pass.

The rehearsal proves only the checked engine's behavior for the copied classic
family and this particular transaction. It does not prove WAL provenance,
source certificate compatibility, a future `br create`, schema migration,
concurrent-writer safety or power-loss durability. Namespace/certificate state
created for the disposable engine identity is not transplanted to the source.
Native read or write disagreement blocks packaged source-index recovery.
The manifest/receipt is bounded to 4 MiB; row witnesses, input copies and the
checkpointed main-only image honor their byte bounds. Engine working files,
query results and sorted encodings still consume additional space and memory.
No constant-memory or complete pre-allocation memory bound is claimed.
