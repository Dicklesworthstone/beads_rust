# WAL reopening overhead: GitHub #598

This investigation concerns
[#598](https://github.com/Dicklesworthstone/beads_rust/issues/598), reported
against br 0.7.6. The source baseline is
`117f81de0d5dae007d972a9ff7f1d73c392d8822`; the corresponding FrankenSQLite
baseline is `074c2033b4dd5212bf2f6c6aa80480ff711dfd9f`.

## Reproduced cause

The official Linux x86_64 release binary has SHA-256
`73cbef7f11680b8349ef5b465d002e404272cc6c06b0f95a86035b488b1844b9`.
It was verified against the release checksum and GitHub asset digest. The
reporter's 5,000-issue corpus, 25,000-byte descriptions, five batches of 400
priority updates, and shared database-opener lease reproduce these WAL sizes:

| Stage | WAL bytes | Complete frames |
| --- | ---: | ---: |
| Initial | 4,152 | 1 |
| 1 | 9,760,312 | 2,369 |
| 2 | 19,949,072 | 4,842 |
| 3 | 29,610,472 | 7,187 |
| 4 | 39,214,192 | 9,518 |
| 5 | 48,784,952 | 11,841 |
| After releasing the lease and checkpointing | 32 | 0 |

At 11,841 frames, both default `show` and explicit no-auto-sync `show` issue
23,690 reads requesting 4,120 bytes, 23,684 reads requesting 24 bytes, and six
32-byte WAL-header reads. The instrumented calls returned 98,171,408 bytes,
with no counter overflow or failed reads. A separate 28-frame replay trace
records two `WAL file opened` events and 56 `startup_open` decisions, with no
`rebuild` or refresh replay. These are fresh independent reproductions.

The two full scans belong to two connections. Startup's advisory pending-merge
inspection opens storage, reads both receipt keys in one transaction, closes
it, and then ordinary startup or command dispatch opens storage again. Each
connection validates the complete WAL checksum chain and constructs its index.
Explicit auto-sync opt-outs do not eliminate the advisory: `Show` still enters
that gate through the command's write-lock classification.

The released reproduction ran while compilation was active. Its raw timings
are contended diagnostic observations, not a speedup claim or a reproduction
of the reporter's absolute CPU times.

## Change and safety boundary

Strict observational startup now inspects the storage it already admitted and
retains that same private snapshot for eligible command dispatch. Explicit
no-auto-sync reads also retain this connection instead of discarding the
advisory's work. The helper still uses the existing coherent two-key metadata
transaction, and propagates inspection errors.

Receipt inspection runs before freshness. A pending, malformed, or unreadable
receipt disables automatic synchronization in both override sets. Its warning
is delivered after startup authority is released. The definitive mutation
inspector and its database-family authority checks are unchanged.

This reuse requires strict read-only storage and the existing startup-import
capability. Sync and Orphans own their storage/freshness lifecycle and retain
their existing path. If an optional local preopen fails, the original advisory
policy still distinguishes missing storage from an unknown receipt: routed
reads can retain their existing fallback, while default missing-local reads
still refuse when startup requires storage.

The connection owns an already verified private snapshot. Reuse does not
trust shared-memory metadata, cache a validation verdict across processes,
skip a WAL checksum, or authorize recovery in the original database family.
Cold recovery remains proportional to WAL bytes. Some commands have additional
command-local opens; the single-open result applies to dispatches consuming
the retained context, including the measured `show` path.

## Regression and measurement entry points

- `tests/e2e_legacy_wal_index.rs` forces actual private recovery of a WAL-only
  edit with an unusable SHM index. Default and explicit no-sync reads each
  recover once, preserve the live family and JSONL, and retain absent, legacy,
  and malformed receipt behavior. Pending cases also carry changed JSONL to
  prove the receipt verdict precedes freshness.
- `tests/e2e_routing.rs` covers missing and corrupt optional local databases
  for routed show, comments, dependency list, and dependency tree. Local-target
  controls refuse; both trackers' artifact inventories remain unchanged.
- The doctor unit test checks that an unreadable metadata table is an error,
  never an absent receipt. Existing pending-merge, Orphans freshness, and
  read-startup concurrency tests remain required controls.
- `scripts/bench_wal_open.py` records fresh-process wall/user/system CPU,
  correct JSON, source and binary identities, WAL size, actual writer overlap,
  checkpoint behavior, and doctor/recovery-inventory postconditions. Timed
  children have no I/O instrumentation. `scripts/bench_wal_io.c` provides
  separate Linux `pread`/`pread64` diagnostics on exact `-wal` file names.

## Validation status on 2026-11-09

The fresh official-release reproduction passed all 109 commands and 84 reader
samples, reproduced every WAL size above, and passed the final doctor and
recovery-inventory checks. Counter calibration and separate replay tracing
also passed. The retained
[manifest](../../tests/artifacts/wal-reopen-598/manifest.json) describes the
raw records in
[reproduction.tar.gz](../../tests/artifacts/wal-reopen-598/reproduction.tar.gz).
Small harness controls also exercised simultaneous reader/writer processes,
per-group overlap attribution, provenance-checked fixture reuse, and disk-backed
temporary snapshot storage.

**The new Rust regressions and matched source-binary timing are not yet
executed.** Three attempts to build the unchanged source baseline were killed
inside published `asupersync` 0.5.0 before compiling the changed application code. The attempts preserved
the pinned toolchain and dependency graph; the latter two used identical
package-specific compiler memory settings. Their peak RSS values were
4,941,464, 2,443,640, and 2,014,372 KiB as host-wide memory availability changed.
The host OOM counter also increased while our compiler was still alive,
demonstrating contention outside that individual process. The provided desktop
uses the same host and offers no separate build capacity.
The [build-validation manifest](../../tests/artifacts/wal-reopen-598/build-validation-manifest.json)
and [complete build records](../../tests/artifacts/wal-reopen-598/build-validation.tar.gz)
retain the commands, source identities, compiler configuration, logs, and
available resource snapshots.

Formatting, patch whitespace, Python syntax, the C counter build/calibration,
and independent source reviews are completed checks. Current all-target Rust
check/Clippy, the new Rust tests, existing recovery/concurrency keepers, matched
candidate CPU/latency measurements, and 130/700 MB source-binary campaigns
remain qualification work. The reproduction is not evidence of a measured
candidate speedup, and this record is not release qualification.

## Engine integration contract

No dependency versions or lockfile entries change here. Beads pins published
facade/core/pager 0.4.10 and WAL/VFS/types 0.4.12 with Asupersync 0.5.0.
FrankenSQLite main is a different 0.4.13 development graph using Asupersync
0.6.2. Its WAL batching work can be adopted only through a compatible published
family and the qualification in
[ENGINE_OPERATING_MODEL.md, section 6](ENGINE_OPERATING_MODEL.md#6-engine-bump-checklist).
This startup correction is independent of that future dependency adoption.
