# #619 scaling qualification — 2026-11-25

## Verdict and evidence classes

The fix bounds **compatibility-mirror hydration**, not the complete cost of a
one-field CLI write. Source inspection establishes two remaining linear terms:
text-ID `IN (...)` queries scan the issue table, and writable schema preflight
reads/checksums the valid WAL frame prefix. A bounded number of changed rows
therefore does not imply constant CPU, read traffic, RSS, lock duration or
concurrent throughput.

| Evidence | Result | What it establishes |
|---|---|---|
| Application and locked-engine source review | Complete | Scope of the removed mirror and remaining issue/WAL scans |
| Native measurement sensitivity controls | Passed | Python fixture memory does not set measured CLI RSS |
| Checkpointed, eight-issue identical-binary calibration | Passed | Real serial/writer mutations and exact correctness checks work |
| Retained-WAL, eight-issue identical-binary calibration | Passed | Lease-based WAL retention, writer waves and full export checks work |
| Timeout negative control | Passed | Incomplete measurements are rejected with retained failure evidence |
| Current-candidate 1k/4k/16k comparison | Pending | No candidate speedup or empirical scaling result is claimed here |
| Current-candidate facade/recovery tests | Pending | Calibration uses the released binary, not the changed facade |

The JSON files named `*-calibration*` compare **the same executable against
itself**. Their timings are diagnostics of the measurement machinery, not a
result for the fix. Some calibration work overlapped memory-pressured
compilation; it must not be repurposed as latency evidence.

## Source and binary identity

- Application source reviewed: `3d3b16555f0dacdc7d49c37f431dcd903bd70be4`.
- Initial fix: `b6bb3b4b49a5b6a5ef0f66c511d237792d4e3e44`.
- Migration/metrics follow-up: `282b69fce0a8c58a081603a2dbdc95dd967c4702`.
- Coherent engine-family repair: `139f80e979b41eb4579351b03a81dd9511d3888f`.
- Locked engine: fsqlite family 0.4.10, VFS 0.4.11, Asupersync 0.5.0.
- `Cargo.lock` SHA-256:
  `4cd8a344cfbb326a83d5a24f0d09df8cd6f7acfa7ea5c8a035ff64e93c8a7a5e`.
- Pinned toolchain: nightly-2026-08-31, rustc 1.100.0-nightly
  (`908501772`, 2026-08-30).
- Calibration binary: official Linux amd64 br 0.7.6, SHA-256
  `73cbef7f11680b8349ef5b465d002e404272cc6c06b0f95a86035b488b1844b9`.
- The official release archive was checked against its published SHA-256:
  `56baae2df5e7cc6f6537e05fd5f560192a9d5ceb02659c61b10b9610b45eb3ef`.
- Host: Linux 6.18.57 x86_64, glibc 2.41, AMD EPYC 9V74,
  nine visible CPUs and approximately 9.7 GiB total memory.

The [ordinary-opener control patch](old-opener-control.patch) changes exactly
two API calls in a separate checkout of the same source. It retains
canonical-path selection, existing-only refusal, identity checking, serialized
mode, migration maintenance and checkpoint policy. Current-marker fixtures
avoid mixing first-open migration work into the opener comparison. This is a
causal control, not a historical-release reconstruction or production patch.

## What remains proportional to workload size

### Issue-count scans

The label-only CLI uses the bulk path even for one exact ID. Its issue checks,
timestamp mutation and issue readback use `WHERE id IN (...)`:

```sql
SELECT id, status FROM issues WHERE id IN (?1);
UPDATE issues SET updated_at = ?1 WHERE id IN (?2);
```

`issues.id` is a text primary key. The pinned engine's indexed IN fast paths
cover integer-literal index probes and rowid/integer-primary-key probes. The
text-ID list falls through to a full SELECT scan or an UPDATE rowid-collection
scan. An ordinary equality predicate, `WHERE id = ?1`, is separately eligible
for an index seek.

Primary source: [pinned VDBE code generation](https://github.com/Dicklesworthstone/frankensqlite/blob/92aeb2b9762f2f38154fde74c4055959c6ebe294/crates/fsqlite-vdbe/src/codegen.rs).
The inspected regions are the SELECT IN fast-path selection, the
`index_integer_in_list_target` helper, and the UPDATE `Rewind`/`Next`
collection pass. The published crate's `.cargo_vcs_info.json` binds it to this
revision.

Lazy column decoding can avoid decoding unrelated long descriptions during
membership filtering. Consequently a linear scan can be substantially cheaper
than rebuilding the entire compatibility mirror while retaining linear
dependence on issue count. Do not infer a full-payload scan from the presence
of `Rewind`/`Next` alone.

### WAL validation

[`SqliteStorage::open_with_timeout`](../../../../src/storage/sqlite.rs) calls
`preflight_effective_schema_before_writable_open`, which invokes
`scan_wal_schema_preflight`. It reads and checksums the valid frame prefix
with a one-frame buffer. This occurs on every writable open, including warm
filesystem/engine-index cases and explicit no-auto-sync commands. Its CPU
and logical read work are proportional to the validated WAL prefix; it is
not solely a cold-engine cost.

Concurrent openers can prevent sole-opener checkpointing. A busy workload can
therefore retain the WAL and repeatedly pay this scan. The bounded-opener fix
does not change the application's writer serialization or checkpoint rules.

### Other limits

- Partial IDs load and inspect all issue IDs; use exact IDs for the point-write
  matrix.
- Scalar mutations can hash the full target issue. Keep target payload fixed
  when varying unrelated data.
- Status/dependency mutations can inspect all parent-child edges or rebuild
  a stale blocked cache. Label-only evidence does not qualify these paths.
- Default auto-import/flush and portable history can perform store-sized
  work while retaining writer authority. The isolated benchmark explicitly
  disables automatic import and flush.
- Missing/stale/malformed migration markers intentionally retain ordinary
  first-open migration and must be measured separately.
- The synthetic matrix has no substantial unrelated comments/dependency
  population. Preservation of empty relation tables is not history-heavy
  scaling evidence.

## Why the hydration counter is insufficient

The existing
[`existing_writable_connections_avoid_whole_store_hydration`](../../../../src/franken_sync.rs)
regression uses 32 issues with 10,000-byte descriptions and 32 original events.
Its old-opener positive control must hydrate at least 32 rows, while the
candidate expects zero through point reads, writes, rollback, savepoints and
reopen. This is a useful mechanism/correctness test.

The same test then selects **all** issues and events to verify preservation,
yet still expects a zero hydration counter. That directly shows why the
counter cannot certify bounded total reads, memory or wall time. The counter
does not count all pager-backed query materialization.

## Measurement and correctness controls

The [Python harness](../../../../scripts/bench_bounded_write.py) uses real CLI
initialization/import, fresh child processes, alternating paired order and
fixed deterministic corpora. Each implementation owns its own initialized
store; different release schemas are never mixed. Setup and verification are
outside the native child's measured interval.

The [native supervisor](../../../../scripts/bench_bounded_write_usage.c)
performs a separate exec before forking the measured CLI. Directly waiting on
a process forked by the data-heavy Python harness creates a false RSS floor
on Linux. The retained sensitivity control demonstrates the distinction:

| Instrument control | Peak RSS bytes |
|---|---:|
| Outer Python wait4 with a retained 64 MiB allocation | 76,808,192 |
| Native helper's actual `true` child under that parent | 393,216 |
| Actual child allocating 16 MiB | 23,199,744 |

See [usage-helper-controls.json](usage-helper-controls.json). CPU, wall time,
faults and block counts are also recorded per real CLI child. Block counts
are not a measurement of all logical SQL read bytes.

The default serial lane requires zero physical WAL frames before and after
every sample. Released import/flush left one bookkeeping frame in calibration;
an explicitly logged, untimed real label add/remove drains it through ordinary
checkpointing before the initial state is captured. The zero-frame rule was
not relaxed. Writer waves can retain frames temporarily and have a separately
logged drain before the next checkpointed wave.

Checks require exact durable label sets, exact dirty IDs, exact audit-event
multiplicity and label-bearing comments, unchanged prior audit events and
unrelated row hashes, complete export payloads, and independent integrity and
foreign-key checks on private quiescent database-family copies. Live stores
are never opened by the stock-SQLite verification connection.

An early calibration exposed an incorrect test assumption about event layout:
the bulk-label route stores `Added label X` / `Removed label X` in `comment`,
with null old/new values. The final oracle verifies that exact layout and
label content instead of requiring the unrelated single-label helper's layout.

Retained-WAL samples grow the WAL themselves. Use recorded before/after size
ranges and separate growth campaigns; a pooled group is not a stationary-WAL
latency estimate. Concurrent completion time includes queue wait and process
work, not a directly instrumented writer-lock hold duration.

## Reproduction

Build candidate and control with identical locked dependencies, feature sets
and compiler settings. Apply the retained two-call patch only in the separate
control checkout. Use RCH where available; this cloud workspace did not have
RCH or configured workers.

```sh
python3 scripts/bench_bounded_write.py \
  --baseline /absolute/br-ordinary-opener \
  --baseline-source 3d3b165-plus-old-opener-control \
  --baseline-lockfile /absolute/control/Cargo.lock \
  --candidate /absolute/br-candidate \
  --candidate-source 3d3b165 \
  --candidate-lockfile /absolute/candidate/Cargo.lock \
  --comparison-kind same-tree-opener \
  --output /absolute/new-619-results \
  --sizes 1000,4000,16000 --repeats 5 --warmups 1 \
  --writers 4,16 --writer-repeats 3
```

Use `--targets 2` to match the original issue's target. A separate
`--retain-wal --wal-growth-updates N` run varies retained WAL at fixed issue
count. The script requires Python's standard library and a C compiler; it
retains the compiled helper, compiler identity and source hashes.

Run the existing facade and bulk-label regressions plus recovery/storage
coverage on the candidate before calling it runtime-qualified. The tracked
bead `beads_rust-22s` remains open until its actual acceptance criteria are met.
