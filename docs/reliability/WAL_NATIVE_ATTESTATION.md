# Linked-engine attestation before source-index recovery

The packaged `br-wal-recover` now requires its own linked FrankenSQLite to
read the proposed shared index before `--restore-shm` can install it, or
`--resume-from` can report successful revalidation. There is no companion
flag or environment setting to bypass that check. Copy-only and explicit
last-commit-copy modes retain their existing verification contracts.

This is a runtime read-attestation gate, not evidence that a particular build
has already passed native tests, a checkpoint implementation, or automatic
recovery in ordinary `br`. The prerequisites and source/evidence preservation
rules in [the recovery runbook](ORPHANED_WAL_RECOVERY.md) still apply.

## What the gate checks

The helper continues to hold the source database and namespace exclusions. It
makes another private main/WAL/SHM family under `native-rehearsal/`; the native
process receives that private path, never the selected source. For resume,
SHM comes from the actual installed index, not a newly regenerated substitute.
The parent retains the private index's dead-man-switch and recovery,
checkpoint and writer locks while the child reads. The native open is
read-only and is explicitly closed without checkpointing on both result paths.

An independent stock-SQLite reader produces `expected.json`: schema, table
names, column projections, accessible hidden rowids, typed row counts/hashes,
user version and application ID. The wire format distinguishes null, integer,
IEEE floating-point bits, UTF-8 text and arbitrary blobs. Length-framed encoded
rows are sorted without dropping duplicates, so query ordering is irrelevant.
Stored shadow tables are covered; virtual cursor handles are not table data.
Existing exact-page attestation remains required for the previously admitted
virtual-table and shadowed-rowid schemas.

The child must pass integrity and the full manifest comparison. The parent
requires one strict JSON receipt with the expected schema, engine identity,
known engine version and exact manifest hash, and binds the executable hash.
A nonzero exit, 60-second timeout, malformed/ambiguous receipt, changed
expectation or private main/WAL/index mutation refuses before source install.
Only normal private read-mark changes are allowed. Full WAL/index validation
and original source/evidence re-attestation still run independently.

Receipts retain `native_probe`; its manifest and stdout/stderr logs live in
the new evidence directory. Source installation still uses the existing
no-replace, evidence-first path. A refusal is not permission to delete SHM,
truncate WAL, change old receipts or rebuild from JSONL.

The standalone Python operator script still defaults to stock-SQLite-only
verification. It does not acquire native proof merely because the packaged
companion supports this gate. Its hidden verifier hook is the companion's
internal handoff, not a claim that an arbitrary external program is trusted.

## Qualification and limits

`python3 tests/e2e_scripts/test_wal_native_gate.py` runs explicitly labelled
fake-responder controls. These use real disposable SQLite WALs and locks to
check ordering, refusals, source preservation and protocol framing. They are
not native-engine correctness tests. All 18 controls and 21 existing resume
helper tests passed on Linux/Python 3.13.5/SQLite 3.46.1 during implementation.

Native qualification is separate, without a fake or interpreter fallback:

```sh
cargo test --locked --bin br-wal-recover
python3 tests/e2e_scripts/test_wal_native_gate.py \
  --recovery-binary /absolute/path/to/installed/br-wal-recover
```

The WAL workflow runs that actual-binary mode after successful installation,
then retains the existing actual-`br` read/write tests. Four new Rust tests
cover wire framing, identifier quoting, manifest decoding and internal-parser
boundaries. Cargo, RCH, rustfmt and compiled binaries were unavailable in the
editing environment; native compilation, execution and hosted CI were not
qualified there.

The gate proves only the checked engine's private read behavior for this
supplied family. It does not prove WAL provenance, certificate compatibility,
subsequent source writes, schema migration, concurrent-writer safety or
power-loss durability. Native read disagreement now blocks packaged recovery
rather than allowing an unqualified index installation. The manifest/receipt
is bounded to 4 MiB and row witnesses are bounded per table by `--max-bytes`;
query results and sorted encodings still consume memory. No constant-memory
or complete pre-allocation memory bound is claimed.
