# Initialized zero-page WAL-index recovery (#507)

## Fault and scope

Issue #507 describes both copies of the WAL-index header saying `isInit=1`
while `szPage`, `mxFrame`, `nPage`, and both salts are zero, beside a nonempty
valid WAL. FrankenSQLite 0.4.4 can refuse both read and write admission with
`BusyRecovery` in this state. It is not evidence that the durable database is
corrupt, nor evidence of an unfinished schema migration: schema 19 is affected.

The #504 commit, `71a39f0f9ea4ce831c469eb6dc47d903fea78afd`, added schema-based
diagnostics. It did not introduce a new VACUUM implementation. #507 reports an
interruption in the recommended migration/maintenance workflow; that causal
engine sequence still requires an interruption reproducer.

The index contains regenerable state. Committed, unexported records can remain
in the WAL. Never discard the WAL or rebuild from an older JSONL snapshot to
work around this admission failure.

## Implemented containment

The existing `br doctor migrate-schema recover` workflow already holds br
write authority and sole-opener admission, preserves the complete family,
rehearses recovery on a private copy, checks protected database/WAL/journal
bytes, and compares complete logical witnesses before reporting success.
Previously its identity-bound engine open simply failed on the copied poisoned
index as well as on the live one.

The sync bridge now adds a narrowly gated preflight to that identity-bound
writable open. Only the exact duplicate initialized, zero-page signature beside
a valid WAL header qualifies; `BusyRecovery` alone never does. The preflight
runs before opening the poisoned family, avoiding dependence on cleanup of a
partially failed engine admission. Ordinary and read-only opens remain
non-repairing; they can log `WAL_INDEX_POISONED` without changing the error type
or bypassing pending-merge checks.

Before quarantining any index, the preflight:

1. Verifies a retained main-file identity and regular, non-symlink, single-link
   files. It acquires an exclusive engine namespace and an OFD lock spanning
   SQLite's main-file pending/reserved/shared range. The existing br authority
   byte is deliberately outside that range and alone is insufficient to
   exclude an external SQLite reader.
2. Rechecks the signature under exclusion; validates the WAL header, page-size
   binding, frame salts and cumulative checksums, and complete committed tail.
   It refuses partial, corrupt, uncommitted, and old-generation tails rather
   than guessing where durable data ends.
3. Hashes the main database, WAL, and complete index; syncs a prepared receipt
   in a newly reserved private `.br-wal-index-*` directory; moves only `-shm`
   to that directory; syncs the directories; and verifies the retained bytes.
4. Releases exclusion and performs identity-bound engine admission once. Every
   identity-bound recovery connection closes without checkpointing, including
   when the index was already absent or healthy. The policy is installed before
   connection configuration, not after it. This keeps initial recovery,
   interrupted recovery, and repeated recovery from rewriting protected WAL or
   main-image bytes during teardown.

The outer doctor's rehearsal, full backup, and post-recovery attestation are
unchanged. This operation does not import JSONL, migrate schema, clear pending
merge metadata, delete certificates, or sweep migration artifacts.

An interruption after the rename leaves a missing regenerable index and a
retained copy, not a truncated WAL. There is no automatic rollback that would
reinstall the poison. Keep the evidence directory when diagnosing a later
failure. The prepared receipt is not a claim that engine recovery completed;
the outer recovery receipt supplies that attestation.

The quarantine preflight is enabled only on Linux, Android, macOS, and iOS,
where the existing sanctioned lock module has an OFD-lock implementation.
Other platforms fail closed; do not replace this exclusion with `flock`.

## Restart and close-retry safety

The initial containment set its no-checkpoint policy only when that invocation
actually moved a poisoned index. That was not restart-safe: after a process
exited between quarantine and engine admission, the next invocation saw an
already-missing index and ordinary close could checkpoint the protected WAL.
Repeating recovery on an already-healthy index had the same problem. The outer
doctor byte witness could detect this only after teardown changed the payload.

Recovery close policy now belongs to the connection's purpose, not to whether
this process happened to perform the rename. All identity-bound recovery opens
install it before executing configuration SQL. Normal opens retain their
existing close behavior. A caller's explicit `close_without_checkpoint_in_place`
request is also sticky across failure: retrying with `close()` or
`close_in_place()` cannot silently regain checkpoint permission.

Test-only boundary hooks exit a subprocess without running destructors after
the prepared receipt is durable, after index rename, after directory durability,
and after engine admission. The parent verifies retained evidence, the exact
main/WAL bytes, all unexported records, and pending metadata, then recovers and
closes twice. These tests model abrupt process loss, not power-loss durability.
The hooks do not exist in non-test builds.

`tests/repro_507_recovery.rs` additionally exercises the real CLI against the
current canonical schema: explicit poisoned-index recovery, repeat recovery,
missing-index restart, reads and subsequent writes, WAL-only pending-merge
refusal, and corrupt-WAL/live-peer refusal. The fixtures contain three issues
and two dependencies absent from JSONL, plus committed title changes only in
WAL. Recovery must not import, export, checkpoint, or clear the pending gate.

## Qualification

Focused Rust tests accompany the implementation for signature discrimination,
WAL checksums/tails, observational reads, retained quarantine evidence,
identity drift, unsafe aliases, peer exclusion, and preservation of DB-only
issues, dependencies, and pending metadata through recovery and close.

Suggested focused runs, using the repository's RCH execution policy:

```sh
rch exec -- cargo test --lib franken_sync::wal_index::tests
rch exec -- cargo test --lib franken_sync::tests::failed_no_checkpoint_close_keeps_its_policy_on_retry
rch exec -- cargo test --lib sync::db_inode_lock::tests
rch exec -- cargo test --test repro_507_recovery
rch exec -- cargo test --test e2e_schema_migration_upgrade
rch exec -- cargo check --all-targets
rch exec -- cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

These Rust commands were not run in the authoring environment: it has no Rust
or RCH toolchain. Independent Python/stock-SQLite checks on Linux verified the
WAL checksum convention, raw SHM salt layout, exclusion of an idle external
SQLite WAL reader, coexistence with br's distant authority byte, and retention
of an OFD lock across unrelated descriptor closes. Those checks are not a
substitute for executing the Rust regressions against FrankenSQLite.
The restart, close-retry, and CLI regressions added in the follow-up likewise
remain unexecuted in this environment; no passing-Rust-test claim is made.

## Remaining engine work

This containment makes the explicit recovery entry point capable of dealing
with the reported cache signature; it does not claim to prevent the engine
from producing that signature. The durable upstream repair should publish only
a fully valid initialized WAL-index header and perform canonical reconstruction
from the committed WAL prefix under the engine's recovery locks. Read-only
observation must not be converted into an implicit writer.

The migration VACUUM path also needs fault-injection coverage at source open,
VACUUM INTO, source close, candidate maintenance, and candidate installation.
Classify and retain or clean only demonstrably owned private candidates after
all handles close. Do not sweep live sidecars or rely solely on Drop: process
kill and the release panic-abort profile bypass normal Rust cleanup. No VACUUM
artifact sweep or upstream dependency change is part of this containment.
