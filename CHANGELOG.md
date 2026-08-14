# Changelog

All notable changes to **br** (beads\_rust) -- a local-first, non-invasive git issue tracker built in Rust.

Project inception: 2026-01-15. Repository: <https://github.com/Dicklesworthstone/beads_rust>.

This changelog is organized by capability rather than diff order. Each version section groups changes into what they mean for users, not how they fell out of the commit graph. Commit links are live and point to the canonical GitHub URL.

**Notation**

- **Release** = published GitHub Release with pre-built binaries attached.
- **Tag** = git tag only (no binaries; used for rapid stabilization cuts during CI iteration).
- Commit links: `https://github.com/Dicklesworthstone/beads_rust/commit/<HASH>`
- Release links: `https://github.com/Dicklesworthstone/beads_rust/releases/tag/<TAG>`

---

## v0.3.0 -- 2026-08-14 (Release)

Storage-engine generation upgrade: FrankenSQLite 0.1.18 → 0.3.1 with the
asupersync 0.4.3 runtime, plus the accumulated test/lint debt cleanup from
the doctor/sync workstream merge (GitHub #409) and a set of verified bug
fixes. Version moves to 0.3.0 because fsqlite types are part of br's public
library API (`BeadsError::Database(FrankenError)`, storage signatures), and
swapping the fsqlite major line through that surface is a breaking change
for library consumers. CLI behavior is compatible apart from the fixes
below.

### FrankenSQLite 0.3.1 + asupersync 0.4.3 (storage engine generation)

- fsqlite 0.2.0 made the engine API `async` end to end and 0.3.0 moved its
  runtime family to asupersync 0.4.3. br's storage layer stays fully
  synchronous through the new `src/franken_sync.rs` facade: a thread-local
  current-thread asupersync runtime drives each `!Send` engine future to
  completion on the calling thread (`Runtime::block_on`), taken out of its
  slot while polling so reentrant SQL builds a fresh runtime instead of
  re-entering the same one. The facade adds a bounded `BusyRecovery` retry
  (fsqlite 0.2+ open-recovery windows fail fast where 0.1.x waited) and a
  stale-schema `prepare()` refresh retry (cross-connection DDL visibility).
- The engine upgrade brings the fsqlite 0.3.x correctness wave to br
  workspaces, including the allocator page-aliasing, committed-freelist
  resurrection, and concurrent-writer EOF-growth fixes behind the B-tree /
  freelist corruption classes reported in GitHub #426 and #428, and the
  concurrent-open `BusyRecovery` retry fix behind multi-agent open storms.
- New direct dependency `asupersync = "=0.4.3"` (default-features off),
  matching the fsqlite family requirement exactly. The optional `mcp`
  feature still carries fastmcp-rust 0.3.2's independent asupersync 0.3.x
  line until FastMCP republishes on 0.4.x; the two coexist as distinct
  crates.

### Missing database no longer deadlocks recovery (GitHub #414, #420, #409 cluster A)

- A missing `beads.db` cannot contain a pending sync-merge receipt, but the
  startup gate treated the *inability to inspect* it as a fail-closed
  refusal — so a JSONL-only checkout (or a deleted database) was locked out
  of `br init`, `br sync --import-only --rebuild`, `br doctor --repair`, and
  every no-db-then-import flow, and a stray ancestor `.beads/` without a
  database blocked `br init` for every directory beneath it. Startup now
  classifies a definitively-absent database as "no pending merge": the
  advisory inspection treats NotFound as absent, and the mutation gate binds
  the absent inode under the held database-family authority so the
  subsequent writable open can initialize or rebuild without an unchecked
  replacement window. (Equivalent fix independently proposed in PR #422 —
  thanks!)

### `br list --status` rejects unknown values (GitHub #418)

- `br list --status all` is now the same meta-value `br lint` accepts (no
  status filter), and an unrecognized status value is a validation error
  naming the built-in vocabulary instead of silently matching zero issues
  with exit code 0. Custom statuses stay first-class: a value declared in
  `.beads/policy.yaml` (`workflow.statuses`) or present on at least one
  issue in the database passes validation.

### `br blocked` rich output keeps line breaks (PR #421)

- Prepared rich `Text` records in the blocked renderer went through a path
  that dropped each Text's line ending, collapsing headers, issue rows, and
  detail rows onto one line in rich mode. They now render through
  `print_text`, which honors the ending. (Diagnosis matches PR #421 —
  thanks!)

### Test and lint debt paid down (GitHub #409)

- Startup-gate cluster A is fixed in code (above); stale schema-version
  literals now assert against `CURRENT_SCHEMA_VERSION`; the stranded
  additive-reconciliation test drives the real `br create` surface instead
  of an unimplemented `--id` flag; drifted message assertions accept the
  current write-lock refusal contract.
- `cargo clippy --all-targets -- -D warnings` is green again: ~100
  pedantic/nursery findings in the merged workstream code were fixed
  individually (used-underscore renames, by-ref parameters, heap-allocating
  the 1 MiB / 64 KiB stack buffers, boxing the large
  `PendingSyncMergeInspection::Valid` variant, `let...else` rewrites,
  merged match arms, `trailing_zeros` bit tests), with documented targeted
  allows where a change would break cross-file contracts. The 2026-08
  nightly's new `assert_is_empty` style lint joined the crate's stylistic
  allow-list.

### Dependencies

- clap 4.6.6, clap_complete 4.6.9, schemars 1.2.2, similar 3.1.2,
  toml =1.1.4 (dev), plus lockfile refreshes (thiserror 2.0.20,
  libc 0.2.189, once_cell 1.21.4, regex 1.13.1, flate2 1.1.9, lru 0.18.2
  for RUSTSEC-2026-0253). Supersedes Dependabot PR #425.

---

## v0.2.22 -- 2026-08-06 (Release)

Windows companion fix to v0.2.21 (GitHub #412 follow-up).

### Windows database-identity witness no longer tracks content

- On Windows the database-inode authority witnessed file identity as
  `(length, mtime)` — values the SQLite engine changes on every write — so
  after v0.2.21 removed the whole-file lock conflict, the very next check
  failed with "Database inode changed while its write authority was held"
  and Windows workspaces were still unusable after `br init`. The witness now
  uses the file creation time, which is stable across in-place writes and
  changes on atomic replacement, matching the (dev, inode) semantics the
  authority has always used on Unix. Linux and macOS binaries are unchanged
  apart from the version number.

---

## v0.2.21 -- 2026-08-05 (Release)

Emergency fix for a v0.2.20 regression that made workspaces unusable on
macOS and Windows.

### Database-family authority no longer fights the SQLite engine

- v0.2.20's database-family write authority locked the database inode with a
  whole-file OS lock (`flock` on Unix, whole-file `LockFileEx` on Windows).
  The SQLite engine holds its own advisory locks on that same inode, and on
  macOS/BSD `flock` and POSIX record locks share one kernel lock table (they
  conflict even within a single process), while on Windows `LockFileEx` is
  mandatory and blocked even schema-header reads. Every command after
  `br init` failed with `Database error: database is busy` (macOS) or
  `database schema is missing or unreadable` (Windows); Linux was unaffected
  only because `flock` and record locks never interact there
  ([#412](https://github.com/Dicklesworthstone/beads_rust/issues/412)).
- The inode authority is now a one-byte range lock at `i64::MAX - 1` — an
  offset the engine never locks and no database ever reaches — via
  `fcntl(F_OFD_SETLK)` on Unix and a one-byte `LockFileEx` on Windows.
  Hard-link-alias exclusion (the reason the inode lock exists) is preserved
  and now covered by a regression test, as is engine open/query under a held
  authority.
- The same conflict masked stale-schema detection: a pre-v17 database now
  surfaces the reviewed `SchemaMismatch` migration guidance again instead of
  an unactionable "busy" error that also blocked `br doctor`.

---

## v0.2.19 -- 2026-07-11 (Release)

This release restores downloadable binaries after the tag-only `v0.2.18` cut
and makes identifier allocation and doctor repair behavior fail closed under
the edge cases found during release verification.

### Identifier correctness

- MCP issue creation now derives adaptive hash sizing from the real database
  issue count instead of a constant placeholder
  ([#381](https://github.com/Dicklesworthstone/beads_rust/issues/381)).
- Identifier generation propagates lookup failures, checks every candidate,
  and returns an explicit collision error after bounded exhaustion instead of
  ever returning an unchecked identifier.
- Empty or unusable configured prefixes are rejected consistently across CLI,
  configuration, MCP startup, and the core ID API. Property, saturation,
  storage-parity, and MCP regressions cover the complete allocation contract.

### Doctor and sync reliability

- Clean JSONL exports now materialize the merge anchor after all content
  normalizers run, so the anchor is byte-identical to the final exported file
  ([#378](https://github.com/Dicklesworthstone/beads_rust/issues/378)).
- A stale-by-mtime but byte-identical merge anchor is now an idempotent doctor
  no-op: no backup or repair action is recorded when no bytes would change.
- The binary-version detector stops at the nearest package manifest, avoiding
  false beads_rust matches for unrelated repositories nested below the source
  checkout.
- Release fixtures retain the fail-closed repeated-repair contract, while the
  registry-clean lockfile keeps `cargo publish --locked` reproducible.

### CLI and distribution

- Blocked close errors identify the actual blockers and make the explicit
  override discoverable ([#380](https://github.com/Dicklesworthstone/beads_rust/issues/380)).
- The self-updater is migrated to the current `self_update` API and its
  replacement helper, with strict Clippy coverage retained.
- The release also includes the post-`v0.2.16` dependency-import, tombstone,
  wrapping, schema-repair, and MCP reliability fixes that were present in the
  tag-only `v0.2.18` source cut.

### Validation

- Full unit and integration suite, all-target compiler check, strict all-target
  Clippy, rustfmt, targeted doctor regressions, UBS, and the release reliability
  matrix.
- Native/cross release artifacts are architecture-checked and smoke-tested
  before the annotated tag is published.

## v0.2.11 -- 2026-05-21 (Release)

This version stops the ephemeral "in-memory" storage path from leaking
temporary database files into `TMPDIR`.

### Fixes

- `SqliteStorage::open_memory()` is backed by a real temp file
  (`beads_mem_<pid>_<count>.db`) rather than `:memory:`, because FrankenSQLite
  requires real file paths for WAL and schema operations. That file -- and any
  `-wal`/`-shm`/`-journal` sidecars SQLite created next to it -- was never
  cleaned up, so JSONL-only / in-memory paths (`br sync --import-only`, `br
  ready --no-db`, `br list --no-db`, and similar) left stale `beads_mem_*`
  files in the user temp directory on every invocation. Under high-frequency
  agent/CI use these accumulated into real disk pressure
  ([#299](https://github.com/Dicklesworthstone/beads_rust/issues/299)).
- The storage layer now tracks the ephemeral temp path and removes the base
  file plus all sidecars on `Drop` (including the signal-induced shutdown
  path), and also cleans up if construction fails partway through. Persistent
  databases are never touched.

### Validation

- Reproduced the issue's exact scenario against a release build and confirmed
  zero `beads_mem_*` files remain in `TMPDIR` after each command exits.
- Added regression tests asserting the temp file exists while the storage is
  open and is gone (with sidecars) after drop.

## v0.2.10 -- 2026-05-14 (Release)

This version supersedes `v0.2.9` by fixing the remaining Windows release-build
portability issue in the doctor subsystem and tightening installer integrity
failure handling.

### Release Fixes

- Kept the `mimalloc` Windows exclusion from `v0.2.9`, which removed the
  MinGW `libmimalloc-sys` C build failure.
- Gated POSIX-only doctor permission checks behind Unix `cfg`s and added
  conservative non-Unix handling for repair backups, chmod-style operations,
  symlink creation, and undo artifact permissions so the Windows release target
  compiles again.
- Re-cut the release after `v0.2.9` had already been published to crates.io,
  because crates.io package versions are immutable and the Windows doctor
  portability fix changes the release source.
- Installer checksum mismatches now fail closed instead of falling back to a
  source build after artifact verification fails; the regression test now uses
  a local file URL so release-preparation runs do not block on network fallback
  behavior.
- Package-manager manifest templates now track the DSR-published
  `br-<version>-<platform>` archive names and current `v0.2.10` checksums, so
  future manifest automation does not keep looking for stale `br-v...` assets.
- The installer and release workflow now keep the tag name (`vX.Y.Z`) separate
  from package asset names (`br-X.Y.Z-...`), preventing future binary installs
  and manifest updates from disagreeing about release artifact URLs.

### Validation

- `v0.2.10` validation includes the dependency-update checks from `v0.2.8`, the
  allocator fix from `v0.2.9`, and a focused
  `cargo check --target x86_64-pc-windows-gnu --release` pass covering the
  Windows doctor portability fix.
- Post-release fresh-eyes validation also checked the Homebrew/Scoop/AUR
  manifest templates against the published DSR assets.
- Follow-up validation traced the same asset naming rule through `install.sh`,
  `br upgrade`, and the release workflow fragment harness.

## v0.2.9 -- 2026-05-14 (Crates.io, superseded)

This version superseded `v0.2.8` by removing the dependency-level Windows
allocator failure after the dependency refresh, but was itself superseded by
`v0.2.10` for a separate doctor portability fix.

### Release Fixes

- Kept `mimalloc` enabled for Linux and macOS builds, but removed it from
  Windows builds so the MinGW release target no longer fails inside
  `libmimalloc-sys`' bundled C build.
- Re-cut the release after `v0.2.8` had already been published to crates.io,
  because crates.io package versions are immutable and the Windows build fix
  changes the release source.

### Validation

- `v0.2.9` validation includes the same dependency-update checks as `v0.2.8`,
  plus a Windows release-build retry proving the MinGW allocator failure was
  replaced by a separate doctor portability issue.

## v0.2.8 -- 2026-05-14 (Crates.io, superseded)

This version refreshes the dependency stack, including the local `/dp` FastMCP and frankensqlite libraries now published on crates.io, and tightens storage reliability around the updated SQLite engine.

### Dependency Updates

- Updated the fsqlite stack used by storage and sync paths to the latest published local versions: `fsqlite*` `0.1.3` and `fsqlite-vfs` `0.1.4`.
- Confirmed the direct dependency set is otherwise current with `cargo outdated --root-deps-only`.
- Updated `fastmcp-rust` and its FastMCP crate family to `0.3.1`.

### Reliability

- Kept explicit `--lock-timeout` reads on the conservative storage-open path, so users asking for lock-aware behavior do not accidentally route through the read-only fast-open bypass.
- Reduced noisy expected fsqlite diagnostics during transient WAL tail-read fallback while preserving warnings for unexpected blocked-cache failures.
- Tightened concurrency and doctor chokepoint tests around the updated storage behavior.

### Validation

- Passed `cargo check --all-targets --all-features`.
- Passed `cargo clippy --all-targets --all-features -- -D warnings`.
- Passed `cargo fmt --check` and `git diff --check`.
- Passed `cargo test --all-features --no-fail-fast`, including doctests.
- Passed `cargo publish --dry-run --locked --allow-dirty` for `beads_rust v0.2.8`.

## [v0.1.33](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.33) -- 2026-03-23 (Release)

This release supersedes the partial `v0.1.32` fallback build by fixing release automation so `dsr` can produce installer-compatible assets deterministically.

### Release and CI

- **Rust cache pinning** was updated across all GitHub workflows to the current signed `Swatinem/rust-cache` v2.9.1 commit after the prior pinned SHA stopped resolving and broke fallback builds.
- **Release builds now fail closed on missing artifacts**: Linux ARM64 and Windows AMD64 are treated as required release outputs instead of being silently omitted from a published release.
- **Cross-platform fallback coverage improved** by moving Linux ARM64 and Windows AMD64 fallback builds onto Linux-based cross-compilation paths, reducing dependence on specialized remote runners for those targets.

### Testing

- **Single-issue graph rendering** now preserves DFS subtree order in plain output so dependents render contiguously instead of visually nesting under later siblings.
- **List output regression coverage** now reflects the actual plain-output behavior for unknown custom statuses, keeping release validation aligned with user-visible CLI output.

## [v0.1.32](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.32) -- 2026-03-23 (Release)

This release extends cross-project routing coverage, hardens storage for frankensqlite compatibility, and tightens the release pipeline around version and installer correctness.

### Cross-Project Routing

- **Route-aware dependency operations** now auto-flush correctly and enforce cross-project guards when adding or removing dependencies ([`4682499`](https://github.com/Dicklesworthstone/beads_rust/commit/4682499)).
- **Graph, delete, audit log, and lint** now respect external workspace routing instead of assuming the main project database only ([`5a983bc`](https://github.com/Dicklesworthstone/beads_rust/commit/5a983bc), [`d63f56c`](https://github.com/Dicklesworthstone/beads_rust/commit/d63f56c), [`4f232bb`](https://github.com/Dicklesworthstone/beads_rust/commit/4f232bb), [`d231bce`](https://github.com/Dicklesworthstone/beads_rust/commit/d231bce), [`d4df28f`](https://github.com/Dicklesworthstone/beads_rust/commit/d4df28f)).
- **Auto-import propagation** now reaches all routing callsites, fixing path normalization and reducing stale cross-workspace reads ([`506b6cf`](https://github.com/Dicklesworthstone/beads_rust/commit/506b6cf), [`911b793`](https://github.com/Dicklesworthstone/beads_rust/commit/911b793)).

### Storage and Config Hardening

- **Prefix normalization** is now integrated through config, storage, and ID handling so runtime issue prefix mismatches resolve consistently ([`bdc0243`](https://github.com/Dicklesworthstone/beads_rust/commit/bdc0243), [`0575380`](https://github.com/Dicklesworthstone/beads_rust/commit/0575380)).
- **Normalized prefixes** now drop trailing separator characters before ID generation, preventing malformed runtime prefixes from producing awkward double-separator IDs.
- **Frankensqlite compatibility** improved again: batched `DELETE` and other remaining batched `IN`-clause operations were replaced with row-by-row queries to avoid engine-specific breakage ([`ba71494`](https://github.com/Dicklesworthstone/beads_rust/commit/ba71494), [`b9a0f25`](https://github.com/Dicklesworthstone/beads_rust/commit/b9a0f25), [`45b2a4e`](https://github.com/Dicklesworthstone/beads_rust/commit/45b2a4e)).
- **Tombstone state handling** now keeps `closed_at` separate from `deleted_at`, records delete metadata when creating or importing tombstoned issues, and clears delete fields when an issue leaves tombstone state.
- **Doctor** now gives better guidance around root `.gitignore` conflicts and partial-index repair behavior ([`44d47e6`](https://github.com/Dicklesworthstone/beads_rust/commit/44d47e6), [`e6ef576`](https://github.com/Dicklesworthstone/beads_rust/commit/e6ef576)).
- **Agents command** handling is more robust for marker-block parsing, project-scoped search, and JSON output on mutating operations ([`1cf1aa9`](https://github.com/Dicklesworthstone/beads_rust/commit/1cf1aa9)).

### Release and CI

- **Release verification** now asserts that the built binary version exactly matches the release tag, closing a class of silent mis-versioning failures ([`3315bf5`](https://github.com/Dicklesworthstone/beads_rust/commit/3315bf5), [`b2a9ef5`](https://github.com/Dicklesworthstone/beads_rust/commit/b2a9ef5)).
- Packaging metadata and cache pinning were refreshed for release automation, and the Intel macOS build moved to the correct runner label ([`e137852`](https://github.com/Dicklesworthstone/beads_rust/commit/e137852), [`9f9f183`](https://github.com/Dicklesworthstone/beads_rust/commit/9f9f183)).

### Testing

- Added fresh regression coverage around blocked-cache close behavior, fresh-db behavior, custom status snapshots, lint routing, and the CI fixes required to keep those suites green ([`9869aca`](https://github.com/Dicklesworthstone/beads_rust/commit/9869aca), [`86f9c98`](https://github.com/Dicklesworthstone/beads_rust/commit/86f9c98), [`b657c0e`](https://github.com/Dicklesworthstone/beads_rust/commit/b657c0e), [`a1e893e`](https://github.com/Dicklesworthstone/beads_rust/commit/a1e893e), [`4cec0bb`](https://github.com/Dicklesworthstone/beads_rust/commit/4cec0bb), [`fe2ae0a`](https://github.com/Dicklesworthstone/beads_rust/commit/fe2ae0a)).

## [v0.1.31](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.31) -- 2026-03-21 (Release)

Focused hardening for concurrent agent workflows, plus a release-process cleanup pass.

### Storage and Reliability

- **Atomic config writes** using PID-scoped temp files to prevent partial-write corruption ([`e3a00e3`](https://github.com/Dicklesworthstone/beads_rust/commit/e3a00e3)).
- **Graceful missing-dependency fallback** in storage and graph code paths -- dangling dep references no longer crash ([`617572f`](https://github.com/Dicklesworthstone/beads_rust/commit/617572f), [`a1b63dd`](https://github.com/Dicklesworthstone/beads_rust/commit/a1b63dd)).
- **Blocked-cache hardening**: single-row inserts, deferred invalidation, INSERT OR REPLACE semantics, graceful read fallbacks ([`ad27f47`](https://github.com/Dicklesworthstone/beads_rust/commit/ad27f47), [`acedf9d`](https://github.com/Dicklesworthstone/beads_rust/commit/acedf9d), [`f687166`](https://github.com/Dicklesworthstone/beads_rust/commit/f687166)).
- **Lazy config loading** and reduced sync lock contention, with checkpoint-on-close opt-out ([`a690d58`](https://github.com/Dicklesworthstone/beads_rust/commit/a690d58)).
- **Ready-query/storage fast path**: column-projected ready queries, compare-and-set claims, and JSONL size witnesses improve concurrency correctness and reduce unnecessary work ([`9550859`](https://github.com/Dicklesworthstone/beads_rust/commit/9550859)).
- Switch test storage from `:memory:` to temp files for better parity with production ([`5e8f91c`](https://github.com/Dicklesworthstone/beads_rust/commit/5e8f91c)).

### Sync and Concurrency

- **Best-effort JSONL witness refresh**: opportunistic startup witness backfills no longer fail freshness probes when the JSONL file races away mid-refresh.
- **Auto-import SyncConflict downgraded to warning** for concurrent multi-agent writes ([`4bc6681`](https://github.com/Dicklesworthstone/beads_rust/commit/4bc6681)).
- Centralized ID resolution into `resolve_issue_id(s)` helpers across all commands ([`94c9138`](https://github.com/Dicklesworthstone/beads_rust/commit/94c9138)).
- Redundant index removal, simplified event inserts, added dependency thread index ([`311225e`](https://github.com/Dicklesworthstone/beads_rust/commit/311225e)).

### Diagnostics

- **Doctor** now warns when root `.gitignore` hides `.beads/.gitignore` ([`5f1da48`](https://github.com/Dicklesworthstone/beads_rust/commit/5f1da48)).
- LazyLock regex in agents command, defer-first blocked-cache invalidation ([`87e0fe6`](https://github.com/Dicklesworthstone/beads_rust/commit/87e0fe6)).

### Testing

- Concurrent close/update/reopen blocked-cache integrity stress test ([`30d95b4`](https://github.com/Dicklesworthstone/beads_rust/commit/30d95b4)).
- Replace `DirGuard` with explicit db path overrides and extract JSON array test helper ([`95deac1`](https://github.com/Dicklesworthstone/beads_rust/commit/95deac1)).

### Documentation

- Rebuilt `CHANGELOG.md` from git history with live commit links ([`53fef3a`](https://github.com/Dicklesworthstone/beads_rust/commit/53fef3a)).

### CI

- Renamed release body file to `RELEASE_NOTES.md` ([`9689bd2`](https://github.com/Dicklesworthstone/beads_rust/commit/9689bd2)).

---

## [v0.1.30](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.30) -- 2026-03-20 (Release)

A wide capability expansion: richer stats, paginated list JSON, deferred blocked-cache, MCP refinements, and mixed-prefix support.

### New Capabilities

- **Mixed issue ID prefixes**: projects can contain issues from multiple prefix namespaces; prefix enforcement is deferred to an explicit `--rename-prefix` flag ([`d012e19`](https://github.com/Dicklesworthstone/beads_rust/commit/d012e19)).
- **Paginated JSON envelope** for `list` output (`{issues, total, limit, offset, has_more}`) with updated jq documentation ([`580d281`](https://github.com/Dicklesworthstone/beads_rust/commit/580d281), [`3b46f33`](https://github.com/Dicklesworthstone/beads_rust/commit/3b46f33)).
- **Deferred blocked-cache refresh** for dependency mutations to reduce DB lock contention under concurrent writes ([`45232f6`](https://github.com/Dicklesworthstone/beads_rust/commit/45232f6)).
- **Batched mutation commands** with stale-cache pre-marking and routing test coverage ([`cdd9cb4`](https://github.com/Dicklesworthstone/beads_rust/commit/cdd9cb4)).
- **Expanded stats command** with many additional aggregate metrics, formatting improvements, and storage query expansions ([`ac4ff74`](https://github.com/Dicklesworthstone/beads_rust/commit/ac4ff74), [`4703dff`](https://github.com/Dicklesworthstone/beads_rust/commit/4703dff), [`b634768`](https://github.com/Dicklesworthstone/beads_rust/commit/b634768)).
- **Expanded blocked/count/stale/epic/lint commands** with richer output, storage query methods, and E2E test suites ([`0987d6e`](https://github.com/Dicklesworthstone/beads_rust/commit/0987d6e), [`3126725`](https://github.com/Dicklesworthstone/beads_rust/commit/3126725), [`c4f861c`](https://github.com/Dicklesworthstone/beads_rust/commit/c4f861c), [`0333b98`](https://github.com/Dicklesworthstone/beads_rust/commit/0333b98)).
- **Close command** expanded with additional status transitions and simplified label handling ([`0f4f094`](https://github.com/Dicklesworthstone/beads_rust/commit/0f4f094)).
- **Batched blocked-cache refresh** with stale-marking fallback and update command error resilience ([`afa8d06`](https://github.com/Dicklesworthstone/beads_rust/commit/afa8d06)).

### Bug Fixes

- Correct `list` offset after client-side filtering for correct pagination ([`36a5ff8`](https://github.com/Dicklesworthstone/beads_rust/commit/36a5ff8)).
- Resolve concurrent DB corruption false positives in doctor ([`3a1feef`](https://github.com/Dicklesworthstone/beads_rust/commit/3a1feef)).
- Fix `show --json` jq accessor to use array index ([`0d0fc38`](https://github.com/Dicklesworthstone/beads_rust/commit/0d0fc38)).
- Only add `unalias br` when an actual alias definition exists ([`0b7b070`](https://github.com/Dicklesworthstone/beads_rust/commit/0b7b070)).

### Documentation

- Implement community PRs #73, #163, #166: body alias confirmed, RUST_LOG=error docs, broken link fixed ([`144070e`](https://github.com/Dicklesworthstone/beads_rust/commit/144070e)).

### CI

- Clone asupersync in all workflows (path dependency of fsqlite-core) ([`ce2ebe4`](https://github.com/Dicklesworthstone/beads_rust/commit/ce2ebe4)).

---

## [v0.1.29](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.29) -- 2026-03-18 (Release)

Headlined by a major performance upgrade and the introduction of MCP server support.

### Performance

- **Frankensqlite upgraded to v0.1.1** delivering approximately 100x write performance improvement ([`39f3e0e`](https://github.com/Dicklesworthstone/beads_rust/commit/39f3e0e)).

### New Capabilities

- **MCP server** (`br serve`): optional Model Context Protocol server for direct AI agent integration, with hardened tool validation and prompt quality ([`2195144`](https://github.com/Dicklesworthstone/beads_rust/commit/2195144), [`8f35a53`](https://github.com/Dicklesworthstone/beads_rust/commit/8f35a53), [`7a1c17a`](https://github.com/Dicklesworthstone/beads_rust/commit/7a1c17a)).
- **TOON output format** added to graph command ([`02c3bde`](https://github.com/Dicklesworthstone/beads_rust/commit/02c3bde)).
- **Closed-at consistency** enforced in issue validation ([`0e805c4`](https://github.com/Dicklesworthstone/beads_rust/commit/0e805c4)).
- **Updated-before/updated-after filters** for `search_issues` ([`f327da2`](https://github.com/Dicklesworthstone/beads_rust/commit/f327da2)).
- **Default prefix changed** from `bd` to `br` ([`e6e7dcb`](https://github.com/Dicklesworthstone/beads_rust/commit/e6e7dcb)).
- **Delete --hard** now properly purges issues from JSONL ([`e6e7dcb`](https://github.com/Dicklesworthstone/beads_rust/commit/e6e7dcb)).

### Bug Fixes

- Fix hyphenated issue ID prefix parsing via `split_prefix_remainder` ([`8fa3edf`](https://github.com/Dicklesworthstone/beads_rust/commit/8fa3edf)).
- Suppress human output for sync subcommands under `--quiet` ([`3c7961e`](https://github.com/Dicklesworthstone/beads_rust/commit/3c7961e)).
- Orphans command now manages its own JSONL freshness via allow-stale plumbing ([`6c7fb5d`](https://github.com/Dicklesworthstone/beads_rust/commit/6c7fb5d)).
- Propagate subcommand `--robot` flag through OutputContext ([`3cb1741`](https://github.com/Dicklesworthstone/beads_rust/commit/3cb1741)).
- Atomic config writes, empty-comment validation, MCP ID-check ordering ([`1796519`](https://github.com/Dicklesworthstone/beads_rust/commit/1796519)).
- Unicode-width-aware truncation in `dep tree` ([`72b8560`](https://github.com/Dicklesworthstone/beads_rust/commit/72b8560)).
- Exclude deferred issues from `--overdue` listing ([`d4cff76`](https://github.com/Dicklesworthstone/beads_rust/commit/d4cff76)).
- Exclude `in_progress` issues from ready work queue ([`f226f66`](https://github.com/Dicklesworthstone/beads_rust/commit/f226f66)).
- Auto-register ParentChild dependency during import when parent is resolved ([`1290385`](https://github.com/Dicklesworthstone/beads_rust/commit/1290385)).
- Show full transitive cascade closure in delete dry-run preview ([`94c3486`](https://github.com/Dicklesworthstone/beads_rust/commit/94c3486)).

### Security

- **CSV formula injection mitigation** and log permission error handling ([`ab5356d`](https://github.com/Dicklesworthstone/beads_rust/commit/ab5356d)).
- Whitelist table/column pairs in `has_missing_issue_reference` ([`014e676`](https://github.com/Dicklesworthstone/beads_rust/commit/014e676)).

### Storage Hardening

- Harden schema and query paths for fsqlite compatibility ([`47fa201`](https://github.com/Dicklesworthstone/beads_rust/commit/47fa201)).
- Doctor: use `typeof()` instead of `IS NULL` for NULL detection ([`841c49b`](https://github.com/Dicklesworthstone/beads_rust/commit/841c49b)).
- Replace local path deps with git URLs in `[patch.crates-io]` ([`988d5c7`](https://github.com/Dicklesworthstone/beads_rust/commit/988d5c7)).
- Fix schema default, `_beads` support, init env vars ([`758f895`](https://github.com/Dicklesworthstone/beads_rust/commit/758f895)).
- Server-side unassigned filter in MCP instead of post-filtering ([`87cfaa4`](https://github.com/Dicklesworthstone/beads_rust/commit/87cfaa4)).
- Force-flush fix applied to CLI export path ([`6501dff`](https://github.com/Dicklesworthstone/beads_rust/commit/6501dff)).

### Refactoring

- OrphanRenderMode enum replaces ad-hoc if-else output chain ([`f00a2be`](https://github.com/Dicklesworthstone/beads_rust/commit/f00a2be)).
- Remove redundant dependencies fallback for blockers in close ([`847c045`](https://github.com/Dicklesworthstone/beads_rust/commit/847c045)).
- Comprehensive rustfmt and clippy passes across CLI, MCP, storage, config, and format modules ([`36dcf1d`](https://github.com/Dicklesworthstone/beads_rust/commit/36dcf1d), [`aaf383d`](https://github.com/Dicklesworthstone/beads_rust/commit/aaf383d), [`d0ca56f`](https://github.com/Dicklesworthstone/beads_rust/commit/d0ca56f)).

---

## [v0.1.28](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.28) -- 2026-03-13 (Release)

A stabilization release after v0.1.27's large feature landing. Cleaned up stale test artifacts.

- Remove stale `.rebuild-failed` recovery artifacts from test fixtures ([`cd546f9`](https://github.com/Dicklesworthstone/beads_rust/commit/cd546f9)).

---

## [v0.1.27](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.27) -- 2026-03-12 (Release)

Major architecture release: cross-project routing, TOON output everywhere, quiet mode, workspace failure resilience, and database snapshot infrastructure.

### Cross-Project Routing

- **Issue routing** with batched dispatch -- commands can now operate on issues in other workspaces via redirect configuration ([`be49fef`](https://github.com/Dicklesworthstone/beads_rust/commit/be49fef), [`9b43240`](https://github.com/Dicklesworthstone/beads_rust/commit/9b43240)).
- Routing extended to all mutation commands ([`9b43240`](https://github.com/Dicklesworthstone/beads_rust/commit/9b43240)).

### TOON Output Format

- TOON (Token-Optimized Object Notation) output support added to audit, lint, version, count, epic, stale, history, orphans, and query commands ([`9565af0`](https://github.com/Dicklesworthstone/beads_rust/commit/9565af0), [`6a1618c`](https://github.com/Dicklesworthstone/beads_rust/commit/6a1618c)).
- Complete quiet mode support across all commands ([`9b43240`](https://github.com/Dicklesworthstone/beads_rust/commit/9b43240)).

### Database Resilience

- **Database family snapshot infrastructure** with sidecar quarantine and JSONL safety model ([`e430d4c`](https://github.com/Dicklesworthstone/beads_rust/commit/e430d4c)).
- **Automatic database recovery** during issue mutation commands ([`21a1031`](https://github.com/Dicklesworthstone/beads_rust/commit/21a1031)).
- `probe_issue_mutation_write_path()` diagnostic helper distinguishes corruption from application errors ([`ca701a7`](https://github.com/Dicklesworthstone/beads_rust/commit/ca701a7)).
- Generalized JSONL recovery across all mutation commands with expanded doctor diagnostics ([`1e163ed`](https://github.com/Dicklesworthstone/beads_rust/commit/1e163ed)).
- Deferred blocked-cache refresh with stale-marker protocol ([`674b9bd`](https://github.com/Dicklesworthstone/beads_rust/commit/674b9bd)).

### Storage Engine

- **Incremental blocked-cache updates** with bulk cycle-check adjacency loading ([`d3d3e64`](https://github.com/Dicklesworthstone/beads_rust/commit/d3d3e64)).
- Blocked cache rewritten as atomic DELETE+INSERT with ForeignKeyGuard RAII ([`0a9609e`](https://github.com/Dicklesworthstone/beads_rust/commit/0a9609e)).
- `mutate()` rewritten to delegate to `with_write_transaction` ([`0320d07`](https://github.com/Dicklesworthstone/beads_rust/commit/0320d07)).
- Consolidated `resolve_issue_id`, hardened ID parsing, fixed blocked cache and transaction API ([`9c02816`](https://github.com/Dicklesworthstone/beads_rust/commit/9c02816)).
- Multi-issue update validation, FK handling refactor, improved test isolation ([`a63769f`](https://github.com/Dicklesworthstone/beads_rust/commit/a63769f)).

### Sync

- Deterministic export ordering, streaming git log, simplified import FK handling ([`6e7ea09`](https://github.com/Dicklesworthstone/beads_rust/commit/6e7ea09)).
- Cycle detection switched to lazy per-node BFS, fixed duplicate event recording ([`f2e20d4`](https://github.com/Dicklesworthstone/beads_rust/commit/f2e20d4)).
- Symlink/gitdir invariant bypass prevented via early canonicalization ([`3a878c2`](https://github.com/Dicklesworthstone/beads_rust/commit/3a878c2)).
- Dirty-issue marking optimized with INSERT OR REPLACE, intra-JSONL collision detection fixed ([`ebf0783`](https://github.com/Dicklesworthstone/beads_rust/commit/ebf0783)).

### Import

- Markdown file import now supports `--parent` and `--dry-run` ([`c1b8541`](https://github.com/Dicklesworthstone/beads_rust/commit/c1b8541)).

### Testing

- Workspace failure replay tests and evolution plan framework ([`046c311`](https://github.com/Dicklesworthstone/beads_rust/commit/046c311)).
- Expanded concurrency E2E coverage with interleaved command families ([`66ee59e`](https://github.com/Dicklesworthstone/beads_rust/commit/66ee59e)).
- Git commit detection improvements in dataset registry ([`1910db4`](https://github.com/Dicklesworthstone/beads_rust/commit/1910db4)).

### CI

- Frankensqlite checkout switched from actions/checkout to git clone ([`5b35a80`](https://github.com/Dicklesworthstone/beads_rust/commit/5b35a80)).

---

## [v0.1.26](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.26) -- 2026-03-11 (Release)

### Cross-Project Routing (First Landing)

- **Cross-project issue routing** with batched dispatch for show, blocked, ready, and stats commands ([`be49fef`](https://github.com/Dicklesworthstone/beads_rust/commit/be49fef), [`7391be3`](https://github.com/Dicklesworthstone/beads_rust/commit/7391be3)).

### Bug Fixes

- Re-read JSONL before flush in no-db mode to prevent clobbering concurrent writes ([`968d2e0`](https://github.com/Dicklesworthstone/beads_rust/commit/968d2e0)).
- Improve archive-tar error message and expand init `.gitignore` ([`22366ea`](https://github.com/Dicklesworthstone/beads_rust/commit/22366ea)).
- Minor cleanups across close, comments, defer, delete, dep, epic, label, q, reopen ([`dc62fff`](https://github.com/Dicklesworthstone/beads_rust/commit/dc62fff)).

---

## [v0.1.25](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.25) -- 2026-03-11 (Release)

A dense release with deep improvements across nearly every subsystem.

### New Capabilities

- **`sync_equals()` implementation** for semantic 3-way merge comparison replacing timestamp/content-hash heuristics ([`caace45`](https://github.com/Dicklesworthstone/beads_rust/commit/caace45), [`a05462d`](https://github.com/Dicklesworthstone/beads_rust/commit/a05462d)).
- **Bidirectional dep traversal** and improved cycle detection ([`004bab8`](https://github.com/Dicklesworthstone/beads_rust/commit/004bab8)).
- **SyncConflict error** to prevent silent data loss on auto-import ([`1017b00`](https://github.com/Dicklesworthstone/beads_rust/commit/1017b00)).
- **Assignee defaults** and stats overhaul with richer output ([`47c0c89`](https://github.com/Dicklesworthstone/beads_rust/commit/47c0c89)).
- **Long/pretty output modes** with box-drawing tree connectors ([`a81fa2b`](https://github.com/Dicklesworthstone/beads_rust/commit/a81fa2b)).
- **Today/yesterday time keywords**, DST-safe helpers, and multiline markdown import ([`7e0d26d`](https://github.com/Dicklesworthstone/beads_rust/commit/7e0d26d)).
- **Exclude `in_progress` issues** from ready output ([`2a409df`](https://github.com/Dicklesworthstone/beads_rust/commit/2a409df)).
- **ID resolution refactoring**, search regex optimization, update/audit improvements ([`4fe1e6a`](https://github.com/Dicklesworthstone/beads_rust/commit/4fe1e6a)).
- **Create command enhancements** and merge bug test improvements ([`5cc9d1f`](https://github.com/Dicklesworthstone/beads_rust/commit/5cc9d1f)).
- Track sync created/updated counts separately, fix comment collision safety ([`6c92895`](https://github.com/Dicklesworthstone/beads_rust/commit/6c92895)).

### Performance

- **Streaming hash update** replaces allocating null-byte substitution ([`7bdedbc`](https://github.com/Dicklesworthstone/beads_rust/commit/7bdedbc)).
- **`to_writer` with reusable buffer** for JSONL serialization ([`8d3c9bf`](https://github.com/Dicklesworthstone/beads_rust/commit/8d3c9bf)).
- **Fast-path SQL limit push-down** when no external dependencies exist ([`9d3473d`](https://github.com/Dicklesworthstone/beads_rust/commit/9d3473d)).
- Move blocked-by computation to Rust, reduce allocations ([`8a5522f`](https://github.com/Dicklesworthstone/beads_rust/commit/8a5522f)).
- Eliminate write contention from read-only CLI commands ([`33335b3`](https://github.com/Dicklesworthstone/beads_rust/commit/33335b3)).
- Named chunk constants, bulk dirty-id inserts, sync import pipeline hardening ([`c059e07`](https://github.com/Dicklesworthstone/beads_rust/commit/c059e07)).

### Storage

- **External-ref uniqueness enforcement** and atomic blocked-cache migration ([`fc656d9`](https://github.com/Dicklesworthstone/beads_rust/commit/fc656d9)).
- Push label filtering into SQL, add timestamp-safe dirty clearing, harden JSONL reader ([`0b88b36`](https://github.com/Dicklesworthstone/beads_rust/commit/0b88b36)).
- Schema v3 migration for NOT NULL filter columns and transient retry in config/metadata writes ([`092fdc2`](https://github.com/Dicklesworthstone/beads_rust/commit/092fdc2)).
- Phased startup lifecycle, child counters, ID collision retry, and storage hardening ([`eb3d0c0`](https://github.com/Dicklesworthstone/beads_rust/commit/eb3d0c0)).
- Enforce tombstone validation and remove dead helpers ([`53df4d4`](https://github.com/Dicklesworthstone/beads_rust/commit/53df4d4)).

### Bug Fixes

- Parse typed YAML values instead of storing everything as strings ([`d393bee`](https://github.com/Dicklesworthstone/beads_rust/commit/d393bee)).
- Handle empty labels array as "(no labels)" in group counts ([`fbe2003`](https://github.com/Dicklesworthstone/beads_rust/commit/fbe2003)).
- Treat Closed-to-Tombstone transition as update, not reopen ([`984f480`](https://github.com/Dicklesworthstone/beads_rust/commit/984f480)).
- Resolve assignee/unassigned mutual exclusion in saved filter merging ([`30d33f1`](https://github.com/Dicklesworthstone/beads_rust/commit/30d33f1)).
- Enforce ID length limit on base hash only, not full hierarchical ID ([`cbacff8`](https://github.com/Dicklesworthstone/beads_rust/commit/cbacff8)).
- Tighten markdown list prefix detection and skip marker-only tokens ([`e384e08`](https://github.com/Dicklesworthstone/beads_rust/commit/e384e08)).
- Preserve blank lines in implicit descriptions and trim dependency whitespace ([`ce42b14`](https://github.com/Dicklesworthstone/beads_rust/commit/ce42b14)).
- DST-safe time parsing fixes ([`fa1fdf8`](https://github.com/Dicklesworthstone/beads_rust/commit/fa1fdf8)).
- Deduplicate parent-child dependencies and harden `get_parent_id` ([`18cfeca`](https://github.com/Dicklesworthstone/beads_rust/commit/18cfeca)).
- Default `RUST_LOG` to `error` for quiet operation ([`94b4347`](https://github.com/Dicklesworthstone/beads_rust/commit/94b4347)).
- Pipe-safety wrap for `curl|bash` truncation edge case in installer ([`bb24002`](https://github.com/Dicklesworthstone/beads_rust/commit/bb24002)).

---

## [v0.1.24](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.24) -- 2026-03-08 (Release)

### New Capabilities

- **InheritedOutputMode** for consistent output format propagation across subcommands ([`b1b9d67`](https://github.com/Dicklesworthstone/beads_rust/commit/b1b9d67)).
- **Enhanced dependency tree visualization** with theming, quiet mode, and search/history improvements ([`e30be1e`](https://github.com/Dicklesworthstone/beads_rust/commit/e30be1e)).
- **SQLite journal support**, git context fixes, atomic ops hardening, quiet mode expansion ([`02a75ec`](https://github.com/Dicklesworthstone/beads_rust/commit/02a75ec)).

### Bug Fixes

- Replace silent depth cap with convergence-based blocked-cache propagation ([`d5f124c`](https://github.com/Dicklesworthstone/beads_rust/commit/d5f124c)).
- Use SQL-aware statement splitter instead of naive `split(';')` ([`45015bc`](https://github.com/Dicklesworthstone/beads_rust/commit/45015bc)).

### Packaging

- Add crates.io exclude list and readme field ([`32c0fb2`](https://github.com/Dicklesworthstone/beads_rust/commit/32c0fb2)).

---

## [v0.1.23](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.23) -- 2026-03-07 (Release)

### New Capabilities

- **`--db` override** respected across all subcommands with graceful fallback ([`b91ee46`](https://github.com/Dicklesworthstone/beads_rust/commit/b91ee46)).
- **Enhanced diff output** for history, CLI help styling, and config validation ([`f81055a`](https://github.com/Dicklesworthstone/beads_rust/commit/f81055a)).

### Bug Fixes

- Remove non-functional musl binary attempt on Linux x86_64 in installer ([`0c9f1de`](https://github.com/Dicklesworthstone/beads_rust/commit/0c9f1de)).

---

## [v0.1.22](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.22) -- 2026-03-07 (Release)

Major robustness release focused on error propagation, transactional safety, and doctor/repair capabilities.

### New Capabilities

- **`doctor --repair`**: rebuild DB from JSONL and harden import pipeline ([`3150f9e`](https://github.com/Dicklesworthstone/beads_rust/commit/3150f9e)).
- **Automatic SQLite database recovery** from JSONL export ([`4d35e55`](https://github.com/Dicklesworthstone/beads_rust/commit/4d35e55)).
- **Windows/zip support** in installer ([`bbf674f`](https://github.com/Dicklesworthstone/beads_rust/commit/bbf674f)).
- Add `-d`, `--parent`, `-e` flags to `br q`; warn on list truncation ([`fe18252`](https://github.com/Dicklesworthstone/beads_rust/commit/fe18252)).
- Config prefix inference from JSONL in `load_config` to prevent `bd-*` fallback ([`382832d`](https://github.com/Dicklesworthstone/beads_rust/commit/382832d)).

### Error Propagation and Safety

- Comprehensive error propagation, transactional imports, and safety hardening ([`f93df50`](https://github.com/Dicklesworthstone/beads_rust/commit/f93df50)).
- Record Reopened event when transitioning from terminal to non-terminal status ([`30ee737`](https://github.com/Dicklesworthstone/beads_rust/commit/30ee737)).
- Label dedup and rename hardening, comment parsing safety, transactional export finalization ([`887e6f7`](https://github.com/Dicklesworthstone/beads_rust/commit/887e6f7)).
- Preserve existing deps/labels when bulk query returns incomplete results ([`9bda6ca`](https://github.com/Dicklesworthstone/beads_rust/commit/9bda6ca)).
- Wire up `--hard` flag to actually purge issues from DB ([`e11f18f`](https://github.com/Dicklesworthstone/beads_rust/commit/e11f18f)).
- Skip full schema rebuild on runtime-compatible legacy databases ([`440b1dc`](https://github.com/Dicklesworthstone/beads_rust/commit/440b1dc)).
- Default busy timeout, coalesce optional text on import ([`f183d90`](https://github.com/Dicklesworthstone/beads_rust/commit/f183d90)).

### Bug Fixes

- Map musl target to correct artifact name for self-update ([`d1c564a`](https://github.com/Dicklesworthstone/beads_rust/commit/d1c564a)).
- Ensure self-update archive extraction works in release builds ([`a555c9e`](https://github.com/Dicklesworthstone/beads_rust/commit/a555c9e)).
- Add musl static build for Linux portability ([`15ca9c9`](https://github.com/Dicklesworthstone/beads_rust/commit/15ca9c9)).
- Restrict raw SQL API surface and improve doctor repair robustness ([`71b83cb`](https://github.com/Dicklesworthstone/beads_rust/commit/71b83cb)).
- Runtime-compatible schema repair and hardened table rebuild safety ([`23ef6bf`](https://github.com/Dicklesworthstone/beads_rust/commit/23ef6bf)).
- Fix config command syntax in README ([`718f6f3`](https://github.com/Dicklesworthstone/beads_rust/commit/718f6f3)).

---

## [v0.1.21](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.21) -- 2026-03-04 (Release)

Frankensqlite stabilization, parallel write safety, blocked cache fixes, and Claude Code skill.

### New Capabilities

- **Official Claude Code skill** for br ([`578d02f`](https://github.com/Dicklesworthstone/beads_rust/commit/578d02f)).
- **Rust 2024 let-chains** adopted across codebase with idiomatic clippy patterns ([`070d149`](https://github.com/Dicklesworthstone/beads_rust/commit/070d149)).
- Auto-flush and auto-import flags resolved from merged config layers ([`d4586cb`](https://github.com/Dicklesworthstone/beads_rust/commit/d4586cb)).

### Bug Fixes

- **Fix parallel write data loss** from dead `busy_timeout` ([`f83a9b0`](https://github.com/Dicklesworthstone/beads_rust/commit/f83a9b0)).
- Refresh blocked cache after dep changes, fix cycle detection, atomicity, and perf ([`84e71cd`](https://github.com/Dicklesworthstone/beads_rust/commit/84e71cd)).
- Repair 4 bugs in `rebuild_issues_table` schema migration ([`3a4faf2`](https://github.com/Dicklesworthstone/beads_rust/commit/3a4faf2)).
- Address 5 community-reported bugs: #104, #105, #106, #107, #108 ([`c6529f4`](https://github.com/Dicklesworthstone/beads_rust/commit/c6529f4)).
- Remove PRIMARY KEY from config/metadata tables and clean up migrations ([`648d46b`](https://github.com/Dicklesworthstone/beads_rust/commit/648d46b)).
- Add frankensqlite compatibility for schema checks and SQL queries ([`ce0b143`](https://github.com/Dicklesworthstone/beads_rust/commit/ce0b143)).
- Bump fsqlite to 7ca6ff1 fixing B-tree cursor and page-count header ([`0e4b5df`](https://github.com/Dicklesworthstone/beads_rust/commit/0e4b5df)).

---

## [v0.1.20](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.20) -- 2026-02-26 (Release)

### New Capabilities

- **Draft status variant** for pre-execution issues ([`82560a5`](https://github.com/Dicklesworthstone/beads_rust/commit/82560a5)).

### Bug Fixes

- Resolve 6 community-reported issues: #85, #86, #87, #88, #91, #92 ([`75dd6f1`](https://github.com/Dicklesworthstone/beads_rust/commit/75dd6f1)).
- Update fsqlite for macOS `c_short` VFS lock fix ([`cd5bc27`](https://github.com/Dicklesworthstone/beads_rust/commit/cd5bc27)).
- Update fsqlite for macOS type mismatch fix ([`6a7678c`](https://github.com/Dicklesworthstone/beads_rust/commit/6a7678c)).

### CI

- Switch to gnu targets, pin fsqlite to GitHub HEAD for pure-Rust UnixVfs ([`4adeb86`](https://github.com/Dicklesworthstone/beads_rust/commit/4adeb86)).
- Validate required artifacts before release ([`cb8d822`](https://github.com/Dicklesworthstone/beads_rust/commit/cb8d822)).

---

## [v0.1.19](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.19) -- 2026-02-23 (Release)

CI stabilization release.

- Allow partial release and temporarily disable linux\_arm64 ([`e67031b`](https://github.com/Dicklesworthstone/beads_rust/commit/e67031b)).

---

## v0.1.18 -- 2026-02-23 (Tag)

- Switch Linux release builds from musl to gnu for GLIBC compatibility ([`bec2a3f`](https://github.com/Dicklesworthstone/beads_rust/commit/bec2a3f)).

---

## v0.1.17 -- 2026-02-23 (Tag)

- Fix CI target installation for all platforms ([`2292139`](https://github.com/Dicklesworthstone/beads_rust/commit/2292139)).

---

## v0.1.16 -- 2026-02-23 (Tag)

- Bump version for release attempt ([`729edf8`](https://github.com/Dicklesworthstone/beads_rust/commit/729edf8)).

---

## v0.1.15 -- 2026-02-23 (Tag)

### New Capabilities

- **`agents --dry-run --json`** produces distinct output with `dry_run`/`would_action` fields ([`312b40d`](https://github.com/Dicklesworthstone/beads_rust/commit/312b40d)).
- GITHUB\_TOKEN support for self-update ([`a0993d5`](https://github.com/Dicklesworthstone/beads_rust/commit/a0993d5)).
- Map Rust target triples to release asset names for self-update ([`b687c5a`](https://github.com/Dicklesworthstone/beads_rust/commit/b687c5a)).
- Mark children of deferred epics as blocked in ready cache ([`3867e97`](https://github.com/Dicklesworthstone/beads_rust/commit/3867e97)).

### Licensing

- Updated to MIT with OpenAI/Anthropic Rider ([`b91c42b`](https://github.com/Dicklesworthstone/beads_rust/commit/b91c42b)).

### Dependencies

- Switch toon\_rust from git to crates.io (tru v0.2.0) ([`b483206`](https://github.com/Dicklesworthstone/beads_rust/commit/b483206)).
- Switch fsqlite deps from local paths to crates.io v0.1.0 ([`6c6ade6`](https://github.com/Dicklesworthstone/beads_rust/commit/6c6ade6)).

---

## [v0.1.14](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.14) -- 2026-02-14 (Release)

The "frankensqlite migration" release -- the entire storage backend was migrated from rusqlite to frankensqlite.

### Storage Migration

- **Full migration from rusqlite to frankensqlite** -- a pure-Rust SQLite implementation ([`d3d9bce`](https://github.com/Dicklesworthstone/beads_rust/commit/d3d9bce), [`c269721`](https://github.com/Dicklesworthstone/beads_rust/commit/c269721), [`8d9d3e7`](https://github.com/Dicklesworthstone/beads_rust/commit/8d9d3e7), [`bee3172`](https://github.com/Dicklesworthstone/beads_rust/commit/bee3172)).
- Batch upsert, FTS5 search, and migration framework for SQLite backend ([`61920c6`](https://github.com/Dicklesworthstone/beads_rust/commit/61920c6)).
- Skip DDL/migration when SQLite schema is already current ([`ee23dc2`](https://github.com/Dicklesworthstone/beads_rust/commit/ee23dc2)).

### New Capabilities

- **Atomic claim guard** with `claim.exclusive` config and IMMEDIATE transaction ([`0a52ac7`](https://github.com/Dicklesworthstone/beads_rust/commit/0a52ac7), [`8df2de9`](https://github.com/Dicklesworthstone/beads_rust/commit/8df2de9)).
- **Show command** now displays design, notes, acceptance\_criteria, external\_ref fields ([`e727f6c`](https://github.com/Dicklesworthstone/beads_rust/commit/e727f6c)).
- **NothingToDo exit code** for idempotent operations ([`e727f6c`](https://github.com/Dicklesworthstone/beads_rust/commit/e727f6c)).
- **Sync preflight guardrails** for JSONL import validation ([`e539185`](https://github.com/Dicklesworthstone/beads_rust/commit/e539185)).
- **History subcommand** enhanced with session timeline and storage improvements ([`d569adc`](https://github.com/Dicklesworthstone/beads_rust/commit/d569adc)).

### Bug Fixes

- Windows path canonicalization using `dunce` to strip `\\?\` prefix ([`4cf7717`](https://github.com/Dicklesworthstone/beads_rust/commit/4cf7717)).
- Fix `IssueUpdate::is_empty` to account for `expect_unassigned` flag ([`2fb071c`](https://github.com/Dicklesworthstone/beads_rust/commit/2fb071c)).
- Log warning on malformed `blocked_by` JSON instead of silent fallback ([`1444e29`](https://github.com/Dicklesworthstone/beads_rust/commit/1444e29)).
- Use UNION instead of UNION ALL in recursive descendant CTE ([`1a3976d`](https://github.com/Dicklesworthstone/beads_rust/commit/1a3976d)).
- Complete self\_update feature gates for `--no-default-features` ([`3fa391a`](https://github.com/Dicklesworthstone/beads_rust/commit/3fa391a)).
- Eliminate silent error fallbacks in storage and sync ([`44edef1`](https://github.com/Dicklesworthstone/beads_rust/commit/44edef1)).
- Streamline release preflight to version-tag check only ([`79e26c9`](https://github.com/Dicklesworthstone/beads_rust/commit/79e26c9)).

---

## [v0.1.13](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.13) -- 2026-02-01 (Release)

### New Capabilities

- **Shell completions** for all CLI arguments using clap\_complete ([`603c53b`](https://github.com/Dicklesworthstone/beads_rust/commit/603c53b), [`4c2f107`](https://github.com/Dicklesworthstone/beads_rust/commit/4c2f107), [`676f7fb`](https://github.com/Dicklesworthstone/beads_rust/commit/676f7fb)).
- **`ready --parent` and `--recursive`** flags for scoped issue filtering ([`ab56d79`](https://github.com/Dicklesworthstone/beads_rust/commit/ab56d79)).
- **`--rename-prefix`** sync option ([`70ec1de`](https://github.com/Dicklesworthstone/beads_rust/commit/70ec1de)).
- Config key completion and enriched completion candidates ([`70ec1de`](https://github.com/Dicklesworthstone/beads_rust/commit/70ec1de)).
- BEADS\_CACHE\_DIR expanded to SQLite database files ([`e764632`](https://github.com/Dicklesworthstone/beads_rust/commit/e764632)).

### Bug Fixes

- Detect and warn about conflicting br installations ([`bc7341d`](https://github.com/Dicklesworthstone/beads_rust/commit/bc7341d)).
- Prevent claiming blocked issues ([`e45fa66`](https://github.com/Dicklesworthstone/beads_rust/commit/e45fa66)).
- Normalize labels during JSONL export for consistent round-trip hashing ([`b5e83fd`](https://github.com/Dicklesworthstone/beads_rust/commit/b5e83fd)).
- Allow rename-prefix import and clean prefixes ([`e648e0b`](https://github.com/Dicklesworthstone/beads_rust/commit/e648e0b)).
- Clear duplicate external refs when renaming prefixes ([`bbffe2c`](https://github.com/Dicklesworthstone/beads_rust/commit/bbffe2c)).
- Honor `--json` flag in flush, import, and status output ([`df184e1`](https://github.com/Dicklesworthstone/beads_rust/commit/df184e1), [`4827a7e`](https://github.com/Dicklesworthstone/beads_rust/commit/4827a7e)).
- Flush storage after undefer to persist state changes ([`57d0528`](https://github.com/Dicklesworthstone/beads_rust/commit/57d0528)).
- Add `is_template` column migration and update ready index ([`ef9a19f`](https://github.com/Dicklesworthstone/beads_rust/commit/ef9a19f)).
- Replace panics with safe fallbacks ([`b5a687b`](https://github.com/Dicklesworthstone/beads_rust/commit/b5a687b)).
- Backfill dependency type column ([`1439290`](https://github.com/Dicklesworthstone/beads_rust/commit/1439290)).
- Legacy schema column backfill ([`1518fe1`](https://github.com/Dicklesworthstone/beads_rust/commit/1518fe1)).

### Performance

- Optimize hot SQL paths and add performance PRAGMAs ([`a97fac5`](https://github.com/Dicklesworthstone/beads_rust/commit/a97fac5)).

### CI

- Use musl for Linux builds to fix GLIBC compatibility ([`7217ae0`](https://github.com/Dicklesworthstone/beads_rust/commit/7217ae0)).
- Architecture-appropriate minisign binary on ARM64 ([`f0c72b5`](https://github.com/Dicklesworthstone/beads_rust/commit/f0c72b5)).

---

## [v0.1.12](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.12) -- 2026-01-28 (Release)

### Bug Fixes

- Escape LIKE pattern special characters in search queries ([`81266c8`](https://github.com/Dicklesworthstone/beads_rust/commit/81266c8)).

### Testing

- Comprehensive JSON output snapshot tests ([`dcaf4e0`](https://github.com/Dicklesworthstone/beads_rust/commit/dcaf4e0)).
- E2E output mode consistency tests ([`4e564ac`](https://github.com/Dicklesworthstone/beads_rust/commit/4e564ac)).
- CSV escaping and saved query override tests ([`4933e1b`](https://github.com/Dicklesworthstone/beads_rust/commit/4933e1b)).

---

## v0.1.11 -- 2026-01-28 (Tag)

### New Capabilities

- **`--wrap` flag** for `br blocked` command ([`1652796`](https://github.com/Dicklesworthstone/beads_rust/commit/1652796)).
- Structured error validation and error parity tests ([`153aa06`](https://github.com/Dicklesworthstone/beads_rust/commit/153aa06)).
- Storage ID hash parity test ([`b6f02f2`](https://github.com/Dicklesworthstone/beads_rust/commit/b6f02f2)).

### Bug Fixes

- Fix label test isolation and ID parsing for new output format ([`b9cf078`](https://github.com/Dicklesworthstone/beads_rust/commit/b9cf078)).

---

## v0.1.10 -- 2026-01-28 (Tag)

### New Capabilities

- **TOON output format** for token-optimized serialization ([`b1882b8`](https://github.com/Dicklesworthstone/beads_rust/commit/b1882b8)).
- **Schema command** for emitting JSON Schema documents ([`9da03ba`](https://github.com/Dicklesworthstone/beads_rust/commit/9da03ba)).
- **Nix flake support** ([`d5e9821`](https://github.com/Dicklesworthstone/beads_rust/commit/d5e9821)).
- **BEADS\_CACHE\_DIR** for monorepo transient file support ([`fc747cb`](https://github.com/Dicklesworthstone/beads_rust/commit/fc747cb)).
- **VCS Integration guide** for non-git version control systems ([`7596071`](https://github.com/Dicklesworthstone/beads_rust/commit/7596071)).
- **`--wrap` flag** for text output ([`a122c1b`](https://github.com/Dicklesworthstone/beads_rust/commit/a122c1b)).
- ACFS lesson registry sync via GitHub Actions ([`8d5908d`](https://github.com/Dicklesworthstone/beads_rust/commit/8d5908d)).

### Bug Fixes

- Fix clippy nightly lints for CI compatibility ([`5f7b306`](https://github.com/Dicklesworthstone/beads_rust/commit/5f7b306)).
- Make `semver` dep non-optional to fix `--no-default-features` build ([`33e4968`](https://github.com/Dicklesworthstone/beads_rust/commit/33e4968)).

---

## v0.1.9 -- 2026-01-23 (Tag)

### New Capabilities

- **`--status` flag** for `br create` command ([`cac47de`](https://github.com/Dicklesworthstone/beads_rust/commit/cac47de)).
- Enhanced CLI commands with improved output and filtering ([`af40d04`](https://github.com/Dicklesworthstone/beads_rust/commit/af40d04)).
- Extended storage layer with improved schema and operations ([`f8577aa`](https://github.com/Dicklesworthstone/beads_rust/commit/f8577aa)).
- Improved sync operations and output formatting ([`e58e90b`](https://github.com/Dicklesworthstone/beads_rust/commit/e58e90b)).

### Bug Fixes

- Fix `blocked_issues_cache` reference in `get_ready_issues` SQL ([`27fa5dd`](https://github.com/Dicklesworthstone/beads_rust/commit/27fa5dd)).
- Allow dots in ID prefixes and skip prefix check with `--force` ([`6d5d0a1`](https://github.com/Dicklesworthstone/beads_rust/commit/6d5d0a1)).
- Update install shell script for improved reliability ([`64d86fb`](https://github.com/Dicklesworthstone/beads_rust/commit/64d86fb)).

---

## v0.1.8 -- 2026-01-22 (Tag)

The largest single version by commit count. Introduced rich terminal output, self-update, conformance testing, and numerous foundational features.

### Rich Terminal Output

- **Rich output foundation** with components and themed panels ([`d85e89a`](https://github.com/Dicklesworthstone/beads_rust/commit/d85e89a), [`736a5ca`](https://github.com/Dicklesworthstone/beads_rust/commit/736a5ca)).
- Rich output integrated across all major commands: stats, dep, sync, label, comments, delete, and more ([`eb6b57a`](https://github.com/Dicklesworthstone/beads_rust/commit/eb6b57a), [`2df0736`](https://github.com/Dicklesworthstone/beads_rust/commit/2df0736), [`f3055cc`](https://github.com/Dicklesworthstone/beads_rust/commit/f3055cc), [`6a95245`](https://github.com/Dicklesworthstone/beads_rust/commit/6a95245), [`6f2d1f0`](https://github.com/Dicklesworthstone/beads_rust/commit/6f2d1f0)).
- OutputContext pattern for JSON mode detection ([`741bd50`](https://github.com/Dicklesworthstone/beads_rust/commit/741bd50)).

### Self-Update

- **`br upgrade`** command with self-update infrastructure ([`b8cf57e`](https://github.com/Dicklesworthstone/beads_rust/commit/b8cf57e)).
- Signature verification for self-update ([`22b04e6`](https://github.com/Dicklesworthstone/beads_rust/commit/22b04e6)).

### Conformance Testing

- **bd/br conformance test harness** for verifying compatibility with the original beads ([`2634839`](https://github.com/Dicklesworthstone/beads_rust/commit/2634839), [`fcfe04e`](https://github.com/Dicklesworthstone/beads_rust/commit/fcfe04e)).
- Benchmark regression detection scripts ([`98a8a92`](https://github.com/Dicklesworthstone/beads_rust/commit/98a8a92)).

### New Capabilities

- **CSV output format** for list command ([`2f008ac`](https://github.com/Dicklesworthstone/beads_rust/commit/2f008ac), [`c04507f`](https://github.com/Dicklesworthstone/beads_rust/commit/c04507f)).
- **Orphans command** to find stale referenced issues ([`0a22a2b`](https://github.com/Dicklesworthstone/beads_rust/commit/0a22a2b)).
- **Markdown bulk import** parser for `br create --file` ([`2b601db`](https://github.com/Dicklesworthstone/beads_rust/commit/2b601db), [`60cdfb7`](https://github.com/Dicklesworthstone/beads_rust/commit/60cdfb7)).
- **No-db JSONL mode** for operating without SQLite ([`2a424b2`](https://github.com/Dicklesworthstone/beads_rust/commit/2a424b2)).
- **3-way merge algorithm** and CLI integration for sync ([`ee50802`](https://github.com/Dicklesworthstone/beads_rust/commit/ee50802), [`246475a`](https://github.com/Dicklesworthstone/beads_rust/commit/246475a)).
- **External dependency resolution** for cross-project coordination ([`4522ca3`](https://github.com/Dicklesworthstone/beads_rust/commit/4522ca3)).
- **Lint command** implementation wired into CLI ([`b891454`](https://github.com/Dicklesworthstone/beads_rust/commit/b891454)).
- **`source_repo` field** for multi-repo support ([`30b668c`](https://github.com/Dicklesworthstone/beads_rust/commit/30b668c)).
- Gate columns and DATETIME type migration ([`7990eae`](https://github.com/Dicklesworthstone/beads_rust/commit/7990eae)).
- Configurable width for IssueTable component ([`953010e`](https://github.com/Dicklesworthstone/beads_rust/commit/953010e)).
- Auto-detect issue prefix from JSONL during migration ([`3a38b45`](https://github.com/Dicklesworthstone/beads_rust/commit/3a38b45)).
- AI coding skills auto-installation via installer ([`18d3e28`](https://github.com/Dicklesworthstone/beads_rust/commit/18d3e28)).
- `--allow-stale` workaround for prefix validation on read-only commands ([`2eea2e1`](https://github.com/Dicklesworthstone/beads_rust/commit/2eea2e1)).

### Storage

- Lazy DB lookups in show/update commands for performance ([`5934996`](https://github.com/Dicklesworthstone/beads_rust/commit/5934996)).
- Deterministic event ordering and ID collision handling improvements ([`ba82e32`](https://github.com/Dicklesworthstone/beads_rust/commit/ba82e32)).
- Content\_hash computed on create ([`6163410`](https://github.com/Dicklesworthstone/beads_rust/commit/6163410)).
- Optimized list command to avoid N+1 count queries ([`8a8c5f9`](https://github.com/Dicklesworthstone/beads_rust/commit/8a8c5f9)).
- Removed redundant blocked cache rebuilds in close/reopen ([`c998026`](https://github.com/Dicklesworthstone/beads_rust/commit/c998026)).

### Sync Safety

- Sync JSONL allowlist and opt-in flag ([`cc605b2`](https://github.com/Dicklesworthstone/beads_rust/commit/cc605b2)).
- Structured sync safety logging ([`90544e2`](https://github.com/Dicklesworthstone/beads_rust/commit/90544e2)).
- Path validation hardening ([`6d30f92`](https://github.com/Dicklesworthstone/beads_rust/commit/6d30f92)).
- Export error policies ([`6d30f92`](https://github.com/Dicklesworthstone/beads_rust/commit/6d30f92)).

### Bug Fixes

- BFS depth limit to prevent infinite loops in cyclic graphs ([`88e4c96`](https://github.com/Dicklesworthstone/beads_rust/commit/88e4c96)).
- Correct dep tree test to verify dependency traversal direction ([`1579204`](https://github.com/Dicklesworthstone/beads_rust/commit/1579204)).
- Hash collision vulnerability fix and dep tree logic ([`458a77b`](https://github.com/Dicklesworthstone/beads_rust/commit/458a77b)).
- Path traversal check fix to allow valid filenames with dots ([`76abe36`](https://github.com/Dicklesworthstone/beads_rust/commit/76abe36)).
- Exclude tombstoned issues from label counts ([`b8e210f`](https://github.com/Dicklesworthstone/beads_rust/commit/b8e210f)).
- Correct `desperate fallback` ID format to be parseable ([`368d804`](https://github.com/Dicklesworthstone/beads_rust/commit/368d804)).
- Skip auto-flush when `--no-db` mode is active ([`9d0e93c`](https://github.com/Dicklesworthstone/beads_rust/commit/9d0e93c)).
- History backup parsing and test robustness ([`2479c97`](https://github.com/Dicklesworthstone/beads_rust/commit/2479c97)).
- Enable auto-import for mutating commands to prevent data loss ([`24fd16c`](https://github.com/Dicklesworthstone/beads_rust/commit/24fd16c)).
- Allow custom issue types and fix routing test ([`e1d5175`](https://github.com/Dicklesworthstone/beads_rust/commit/e1d5175)).
- Improve parsing for hyphenated prefixes with word-like hashes ([`eef627c`](https://github.com/Dicklesworthstone/beads_rust/commit/eef627c)).
- Create empty `issues.jsonl` on init for bv compatibility ([`18d214d`](https://github.com/Dicklesworthstone/beads_rust/commit/18d214d)).
- Fix dep add message direction and orphans robot mode JSON ([`b59632d`](https://github.com/Dicklesworthstone/beads_rust/commit/b59632d)).

---

## [v0.1.7](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.7) -- 2026-01-18 (Release)

First release with pre-built binaries across all six platforms.

### New Capabilities

- **Bulletproof installer** with fallback to source build ([`f09877d`](https://github.com/Dicklesworthstone/beads_rust/commit/f09877d)).
- **AGENTS.md blurb detection** and management ([`cbd9e95`](https://github.com/Dicklesworthstone/beads_rust/commit/cbd9e95)).
- Cache buster added to all install URLs ([`0837c63`](https://github.com/Dicklesworthstone/beads_rust/commit/0837c63)).

### Bug Fixes

- Use `shasum` on macOS for checksum generation ([`d2e6131`](https://github.com/Dicklesworthstone/beads_rust/commit/d2e6131)).
- Handle `BASH_SOURCE` unbound when piped to bash ([`f978117`](https://github.com/Dicklesworthstone/beads_rust/commit/f978117)).
- Normalize usernames and version numbers in test snapshots ([`0154711`](https://github.com/Dicklesworthstone/beads_rust/commit/0154711)).
- Fix `macos-13` retired runner ([`3742b50`](https://github.com/Dicklesworthstone/beads_rust/commit/3742b50)).

### CI

- Skip conformance tests when bd binary unavailable ([`0f08f0b`](https://github.com/Dicklesworthstone/beads_rust/commit/0f08f0b), [`55c355b`](https://github.com/Dicklesworthstone/beads_rust/commit/55c355b)).

### Platforms

| Platform | Architecture | Asset |
|----------|-------------|-------|
| Linux | x86\_64 (glibc) | `br-v0.1.7-linux_amd64.tar.gz` |
| Linux | x86\_64 (musl, static) | `br-v0.1.7-linux_amd64_musl.tar.gz` |
| Linux | ARM64 | `br-v0.1.7-linux_arm64.tar.gz` |
| macOS | x86\_64 (Intel) | `br-v0.1.7-darwin_amd64.tar.gz` |
| macOS | ARM64 (Apple Silicon) | `br-v0.1.7-darwin_arm64.tar.gz` |
| Windows | x86\_64 | `br-v0.1.7-windows_amd64.zip` |

---

## v0.1.6 -- 2026-01-18 (Tag)

- Fix import order for `cargo fmt` in CI ([`16c7f36`](https://github.com/Dicklesworthstone/beads_rust/commit/16c7f36)).

---

## v0.1.5 -- 2026-01-18 (Tag)

- Add bd skip check to all conformance test files ([`66518a9`](https://github.com/Dicklesworthstone/beads_rust/commit/66518a9)).

---

## v0.1.4 -- 2026-01-18 (Tag)

- Add bd skip check to conformance tests ([`9f51da7`](https://github.com/Dicklesworthstone/beads_rust/commit/9f51da7)).

---

## v0.1.3 -- 2026-01-18 (Tag)

- Add bd skip check to benchmark\_datasets tests ([`641374e`](https://github.com/Dicklesworthstone/beads_rust/commit/641374e)).

---

## v0.1.2 -- 2026-01-18 (Tag)

- Skip benchmark tests when bd binary unavailable ([`21ec1ad`](https://github.com/Dicklesworthstone/beads_rust/commit/21ec1ad)).

---

## v0.1.1 -- 2026-01-18 (Tag)

- Consolidate target patterns in `.gitignore` ([`609cb9f`](https://github.com/Dicklesworthstone/beads_rust/commit/609cb9f)).
- Remove accidentally committed build artifacts ([`34444b4`](https://github.com/Dicklesworthstone/beads_rust/commit/34444b4)).

---

## [v0.1.0](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.1.0) -- 2026-01-18 (Release, Draft)

Initial public release. A Rust port of [Steve Yegge's beads](https://github.com/steveyegge/beads), frozen at the "classic" SQLite + JSONL architecture.

### Core Feature Set

- **Full CLI** with all classic beads commands: init, create, list, show, update, close, reopen, delete, dep, blocked, ready, search, stale, count, stats, sync, doctor, q (quick capture).
- **SQLite + JSONL hybrid storage**: SQLite for fast local queries, JSONL for git-friendly collaboration.
- **Non-invasive design**: never executes git commands, never touches files outside `.beads/`, never auto-commits.
- **Agent-first**: every command supports `--json` for AI coding agent integration.
- **Dependency tracking** with cycle detection, blocked/ready issue management, and dependency trees.
- **Label system** with add/remove/list/list-all operations.
- **Comments** with add/list operations.
- **Config system** with layered precedence: CLI flags > env vars > project config > user config > defaults.

### Architecture (Built During Pre-v0.1.0 Development)

259 commits from project inception (2026-01-15) built the entire system from documentation and planning through to a fully functional CLI. Key milestones:

- Comprehensive porting plan and legacy architecture documentation ([`38cd152`](https://github.com/Dicklesworthstone/beads_rust/commit/38cd152), [`a376186`](https://github.com/Dicklesworthstone/beads_rust/commit/a376186)).
- Behavioral specs for all classic bd commands ([`15e4908`](https://github.com/Dicklesworthstone/beads_rust/commit/15e4908), [`76eb243`](https://github.com/Dicklesworthstone/beads_rust/commit/76eb243)).
- Core model types ([`562e021`](https://github.com/Dicklesworthstone/beads_rust/commit/562e021)).
- Classic CLI command scaffold ([`16c98b8`](https://github.com/Dicklesworthstone/beads_rust/commit/16c98b8)).
- Search, comments, doctor, sync commands ([`5444b9b`](https://github.com/Dicklesworthstone/beads_rust/commit/5444b9b), [`43b523b`](https://github.com/Dicklesworthstone/beads_rust/commit/43b523b), [`229ec5a`](https://github.com/Dicklesworthstone/beads_rust/commit/229ec5a)).
- E2E and conformance test suites ([`5304ba7`](https://github.com/Dicklesworthstone/beads_rust/commit/5304ba7)).
- Sync safety hardening with JSONL allowlist, path validation, and export error policies ([`cc605b2`](https://github.com/Dicklesworthstone/beads_rust/commit/cc605b2), [`6d30f92`](https://github.com/Dicklesworthstone/beads_rust/commit/6d30f92)).

### Platforms

Cross-platform binaries: Linux (x86\_64, aarch64), macOS (x86\_64, Apple Silicon), Windows.

### Installation

```bash
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/beads_rust/main/install.sh?$(date +%s)" | bash
```
