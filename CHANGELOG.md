# Changelog

All notable changes to **br** (beads\_rust) -- a local-first, non-invasive git issue tracker built in Rust.

Project inception: 2026-01-15. Repository: <https://github.com/Dicklesworthstone/beads_rust>.

This changelog is organized by capability rather than diff order. Each version section groups changes into what they mean for users, not how they fell out of the commit graph. Commit links are live and point to the canonical GitHub URL.

**Notation**

- **Release** = published GitHub Release with pre-built binaries attached.
- **Tag** = git tag only (no binaries; used for rapid stabilization cuts during CI iteration).
- Commit links: `https://github.com/Dicklesworthstone/beads_rust/commit/<HASH>`
- Release links: `https://github.com/Dicklesworthstone/beads_rust/releases/tag/<TAG>`

**Scope window:** every version from inception (v0.1.0, 2026-01-18) through the current
release (v0.5.7, 2026-08-29). The full per-version detail is in the sections
below; the timeline names the recent line and the milestone anchors.

## Version Timeline

Recent line (0.5.x — storage-safety and the multi-process corruption program):

| Version | Date | Kind | Headline |
|---|---|---|---|
| [v0.5.10](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.5.10) | 2026-09-04 | Release | Six silent-loss fixes (#466–#477, #487); acceptance checklist edits; `doctor --bundle`; FrankenSQLite 0.3.15; linearizability, model-based, README, and Go-bd proof suites; schema-witness fast open |
| v0.5.9 | 2026-09-03 | Tag | Tagged, no binaries: the hosted lib shard passed, then the new multi-process stress gate ran its script with relative paths that its own `cd` broke; fixed and re-cut as v0.5.10 |
| v0.5.8 | 2026-09-03 | Tag | Tagged, no binaries: the first hosted run of the release gates caught a binary unit test broken by the fsqlite 0.3.15 sidecar behavior; fixed and re-cut |
| [v0.5.7](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.5.7) | 2026-08-29 | Release | `doctor health` schema-incompatibility tripwire (#464); FrankenSQLite 0.3.13 reader-slot follow-up |
| [v0.5.6](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.5.6) | 2026-08-29 | Release | Review-round correctness/hygiene fixes (agent_context merge, blocked-cache, temp-sidecar leak) |
| [v0.5.5](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.5.5) | 2026-08-29 | Release | FrankenSQLite 0.3.12 engine fix (cross-process WAL reader registration) for the page-aliasing corruption |
| [v0.5.4](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.5.4) | 2026-08-28 | Release | Sole-opener WAL checkpoint containment; Windows doctor/long-path fixes; #463 query fix |
| [v0.5.3](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.5.3) | 2026-08-27 | Release | Storage-correctness follow-up; adopts published FrankenSQLite 0.3.11 schema fixes |
| v0.5.2 | 2026-08-25 | Release | Stabilization cut on the 0.5 line |
| v0.5.1 / v0.5.0 | 2026-08-25 | Tag | Rapid 0.5 stabilization tags (no binaries) |

Milestone anchors (see the per-version sections below for detail):

| Version | Date | Kind | Milestone |
|---|---|---|---|
| [v0.4.1](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.4.1) | 2026-08-24 | Release | Windows recovery wave |
| [v0.4.0](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.4.0) | 2026-08-22 | Release | 0.4 line |
| [v0.3.0](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.3.0) | 2026-08-14 | Release | FrankenSQLite engine generation upgrade (0.1.18 → 0.3.1) |
| [v0.2.22](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.2.22) | 2026-08-06 | Release | Late 0.2 stabilization |
| [v0.2.10](https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.2.10) | 2026-05-14 | Release | Mid 0.2 line |
| v0.1.0 | 2026-01-18 | Tag | First tagged cut (inception 2026-01-15) |

## Representative commits

The commits to inspect first for the current storage-safety program (the
multi-process page-aliasing corruption and its fallout), newest first. Full
per-version detail is in the sections below.

- [8b504e5c](https://github.com/Dicklesworthstone/beads_rust/commit/8b504e5c) — `br doctor health` reads the schema `user_version` from the database header (engine-free) and reports `db=schema_incompatible` (exit 1) instead of `healthy` on a tracker that refuses every mutation, #464 (v0.5.7).
- [6a6839f9](https://github.com/Dicklesworthstone/beads_rust/commit/6a6839f9) — `sync_equals` now compares `agent_context`, stopping silent merge data loss (v0.5.6).
- [e202f67b](https://github.com/Dicklesworthstone/beads_rust/commit/e202f67b) — blocked-cache: upgrade an incremental refresh to a full rebuild when already stale (v0.5.6).
- [18840874](https://github.com/Dicklesworthstone/beads_rust/commit/18840874) — reap all FrankenSQLite sidecars for ephemeral temp databases, a #299 regression (v0.5.6).
- [2327e82f](https://github.com/Dicklesworthstone/beads_rust/commit/2327e82f) — move br to FrankenSQLite 0.3.12, adopting the engine-side corruption fix (v0.5.5).
- [dedfbed7](https://github.com/Dicklesworthstone/beads_rust/commit/dedfbed7) — sole-opener WAL checkpoint containment (v0.5.4).
- [60ea6031](https://github.com/Dicklesworthstone/beads_rust/commit/60ea6031) — #463 partial-index / `INDEXED BY` text-list query fix (v0.5.4).
- [4b28ee31](https://github.com/Dicklesworthstone/beads_rust/commit/4b28ee31), [2551c8b2](https://github.com/Dicklesworthstone/beads_rust/commit/2551c8b2) — Windows doctor directory-fsync + long-path handling, #450 / #456 / #462 (v0.5.4).

The engine-side fix itself lives in FrankenSQLite (cross-repo, so not linked to
this repo): commits `55c186682` + `5946b3b7c` in
<https://github.com/Dicklesworthstone/frankensqlite>, shipped as fsqlite 0.3.12.

---

## Unreleased -- on `main` after v0.5.10

- Writes to a database on a filesystem that does not persist POSIX permission
  bits — a Windows drive under WSL (`/mnt/<drive>`) mounted without the
  `metadata` option, FAT/exFAT — no longer fail with the engine's bare
  `Database error: unable to open database file: '<db>-fsqlite-ns-gate'`, and
  a database whose namespace sidecars are group/other-accessible no longer
  wedges every command (#491, and the #403 wedge that had come back as
  `Pending sync-merge state is unknown because fsqlite namespace sidecar
  repair would require mutation before the pending-saga verdict`, exit 6).
  FrankenSQLite creates `-fsqlite-ns-gate`/`-fsqlite-ns-use` with
  `open(O_CREAT|O_EXCL, 0600)` and then refuses them when `fstat` reports
  any group/other bit; such a mount reports its fixed mask (0777) for every
  file and ignores every chmod, so the first write open created the gate
  and failed on it, while `count` on a sidecar-less snapshot never touched
  the gate (sidecar-less read-only admission) and worked. Now: the
  authority-gated owner-only mode repair runs before the pending-saga
  verdict (it changes no byte of any family member, and without it no
  verdict is reachable), so a merely loosened sidecar heals and the command
  proceeds on a POSIX filesystem; a chmod the filesystem accepts but ignores,
  or refuses for the owner, is classified as the filesystem limitation and
  refused up front with the mount, the `metadata` remedy, and the
  keep-it-on-the-Linux-filesystem alternative (exit 7, `CONFIG_ERROR`); an
  engine `CannotOpen` that names a sidecar is explained after the fact by its
  actual cause (mount mask on a sidecar the engine just created, pre-existing
  loose mode, foreign owner, extra hard links) instead of reading as database
  corruption; `br doctor` names the mount limitation when the database carries
  the same mask and its `--repair` chmod verifies that the mode actually
  changed; and the reviewed additive reconcile path repairs sidecar modes
  under its authority before its non-repairing open. Reads are unchanged.
  Writing on such a mount also needs the engine change in FrankenSQLite
  (namespace sidecars accept a mount-imposed mask), which lands with the next
  fsqlite release; br's own sidecar checks mirror the engine's rule (a
  group/other exposure bounded by the database file's, per principal class
  and GID) and switch on automatically once the locked `fsqlite` in
  `Cargo.lock` is 0.3.18 or newer, with a parity test that opens such a
  family through the linked engine so the two cannot drift. Until that bump
  br refuses with the explanation above.
- Exit-code note: a `Configuration error` raised inside the startup
  pending-sync-merge gate (a sidecar the filesystem cannot repair, or a
  database whose schema version is newer than this binary supports) now
  surfaces as itself — `CONFIG_ERROR`, exit 7 — instead of being wrapped in
  a `Sync conflict: Refusing storage open because pending sync-merge state
  could not be inspected ...` (exit 6). Both map to the categories in the
  documented exit-code table; only the future-schema case changes code.

- CI runs again: every workflow pins the manifest's toolchain
  (`nightly-2026-08-31`) instead of floating `nightly` (release.yml still
  said 2026-08-25 after the manifest moved), the audit gate installs
  `cargo-insta` as a prebuilt binary (source builds failed on a newer
  nightly's `rustix`), the runtime index diagnostics are rustfmt-clean
  (the formatting check had failed on every push since they landed), and
  the CI concurrency group is keyed by event so the six-hourly scheduled
  run no longer cancels a push run's shards mid-flight, and the Quick E2E
  job's timeout covers a cold compile (it was cancelled at 9:57 of 10).
  The `misc` shard's golden `init` file-set tests now treat the sole-opener
  lease file (`.br-db-openers-<hash>.lock`, #457) as engine state like the
  WAL sidecars; they had been red since that lease landed on 2026-08-28.
  The audit gate's snapshot-freshness step runs only the four insta-using
  targets instead of `--workspace` (162 binaries from cold overran the job).
  The Quick E2E harness (`scripts/e2e.sh`) compiles its six test binaries
  before the per-test 180 s timeout starts; a cold release compile inside
  that budget had been reported as the first test's failure without the
  test ever running. The job also runs the harness in the dev profile
  (`E2E_PROFILE=dev`; release stays the local default): linking six test
  binaries with the release profile's fat LTO killed the hosted runner
  ("runner has received a shutdown signal") on two consecutive runs before
  any test ran. The audit gate's suspect-close-reason script matches
  the same literal hedge phrases as the doctor rule it mirrors
  (`audit.suspect_close_reasons`); its greedy `forced.*close.*cycle` regex
  flagged the audit's own policy bead (30ci) for naming
  `forced_cycle_close_audit.sh` in its close reason, so the job had failed
  on every hosted run. The sync-safety filesystem witness
  (`sync_safety_witness.sh`) accepts the digest-named write-authority locks
  (`.br-db-write-<hash>.lock`, `.br-jsonl-write-<hash>.lock`) that `br sync`
  has created since the July write-authority work, matching the Rust
  allowlist in `e2e_sync_git_safety.rs` it mirrors; it had flagged them as
  out-of-allowlist writes. The witness also pins `LC_ALL=C` so its
  `sort`/`comm` file diff cannot abort on locale collation of the
  digest-named files.

- `br upgrade --version 0.5.10` no longer 404s: a bare version is looked up
  as the `v`-prefixed release tag GitHub actually carries (found while
  installing v0.5.10 through the shipped 0.5.7 binary; the no-version path
  was unaffected).
- Measured on the same host with the same loops as before the schema-witness
  fast open (20 sequential operations each, installed release binaries):
  `create` 144 → 112 ms, `update --priority` 238 → 188 ms, `show --json`
  53 → 45 ms per operation from v0.5.7 to v0.5.10 (bead naul5).
- Dependencies (`/library-updater`, one at a time, each gated on RCH; see
  `UPGRADE_LOG.md`): the FrankenSQLite family 0.3.15 → 0.3.16 (page-allocator
  EOF-growth double-grant closed under the reserved append lock, bd-9inpb;
  WAL appended-tail reads no longer rescan the whole WAL per page on
  writable open and checkpoint, GH#382; FTS5 work br does not use;
  frankensqlite#407 is fixed upstream but after the 0.3.16 tag, so the
  multi-label counting workaround and its ignored probe stay), fastmcp-rust
  =0.7.1 → =0.8.1 (caller-owned runtime contexts, which `br serve` already
  provided; `log` 0.4.34 by its pin), toon_rust/`tru` 0.2.3 → 0.2.4. Held:
  asupersync =0.4.9 and toml =1.1.4, both exact pins of fastmcp 0.8.1.
- `br update` and `br close` reuse the connection `main` already opened at
  startup for the auto-import probe and the auto-flush instead of opening a
  second one for the write (the remaining fixed cost measured on the update
  path, bead naul5). The connection goes back to `main` after the write so
  the auto-flush still runs through the connection that performed it; a
  pre-opened connection for a different workspace is left alone, and routed
  external batches keep opening their own. `br create` already worked this
  way.
- Import no longer rolls back with `Import semantic verification failed ...
  (differing fields: content_hash, created_at, external_ref, title,
  updated_at)` when two records of one JSONL file collapse onto the same
  issue (an external-ref collision remapped onto an existing id, then an
  explicit record for that id). The verifier added on 2026-08-27 compared
  the persisted row against every record applied to the issue, including
  the superseded first one; it now checks the last record applied. The
  `misc` CI shard's `jsonl_import_export` binary had been red on this since
  that verifier landed (bead 72j0i).
- The model-based storage property test (`tests/model_based_storage.rs`)
  predicts dependency cycles the way the storage detects them: parent-child
  edges run parent → child in the blocker graph and `related` edges are
  never cycle-checked. Its reference model walked every edge as
  issue → dependency, so a `blocks` chain plus a reversed parent-child edge
  (a real deadlock the storage refuses) made the test expect success; the
  hosted `misc` shard hit that case once the import verifier fix let the
  binary run. A deterministic regression case covers both orientations and
  the non-blocking edge.
- The multi-process linearizability check (`tests/linearizability_multiprocess.rs`)
  ends its workload with a quiescent read pass (one `show --json` per issue,
  appended to the history) so a linearization has to end in the state the
  database actually reached. Two overlapping writes on one issue with no
  later read admit two valid end states, and the check compared the
  published JSONL against whichever the search found first; on the hosted
  `misc` shard (run 33881198051) that produced a false "published JSONL
  diverges from the linearized final state" for one issue's priority.
- `tests/e2e_upgrade.rs`'s pinned-version dry run accepts a reported lookup
  failure (non-zero exit with a message) as well as the named target
  version. It required the output to contain the version, `NetworkError`,
  or a lowercase `error`, and the unauthenticated GitHub API rate limit
  hosted runners hit yields `Error: Upgrade failed: ...`, none of those; the
  e2e m–z shard failed on it in the same run. The parse-acceptance check
  stays, and the failure message now carries the outputs.

## v0.5.10 -- 2026-09-04 (Release)

Every v0.5.7 user has the bugs below. This cut carries the six user-reported
silent-loss fixes, the acceptance-checklist and incident-bundle features, the
FrankenSQLite 0.3.15 engine, and the proof suites (linearizability, model-based
storage, README examples, Go-bd conformance) that now gate the release.
Two tags preceded it without binaries. v0.5.8 failed the first hosted run of
the release gates on a binary unit test that the fsqlite 0.3.15 sidecar
behavior had broken (`-shm` now outlives every open), which the RCH gates had
never exercised because `cargo test --lib` skips `src/main.rs` tests. v0.5.9
passed that shard and then failed the new multi-process stress gate, whose
script changes into a throwaway directory before running the binary it was
handed as a relative path. Both were fixed and the release re-cut.

- `br doctor --selftest` pins its child commands to the throwaway workspace
  (`BEADS_DIR`), so a temp root that sits inside a real checkout no longer
  makes the selftest's `br init` discover that tracker and refuse; found
  when the selftest e2e ran on an RCH worker whose `TMPDIR` is inside the
  repository.

### Performance

- Ordinary database opens trust the recorded runtime-schema witness: when the
  metadata witness names the connection's exact SQLite schema cookie, the
  open skips the eleven-table PRAGMA walk (about 24 ms, run twice per
  mutating command) and the witness re-write, as the read-only fast open
  already did. Any DDL moves the cookie, so a tampered schema still takes
  the full check and is healed. Measured with the eight-stream
  linearizability workload on the same worker class: 334 operations in 30 s
  after the change versus 224–235 before (bead naul5 carries the per-step
  profile of a mutation).

### Silent-loss fixes (#466, #467, #471, #473, #474, #475, #476, #477)

- `br gate report` writes `gate_results` as well as `gate_result_history`
  (#466) -- 70e7fed9.
- The first mutation in a fresh clone (tracked `.beads/*`, no `beads.db`) no
  longer stalls its auto-flush for the 30 s write-lock timeout and then exits
  0 with a stale `issues.jsonl`: the JSONL rebuild kept the JSONL-family
  flock for the rest of the command and the flush re-acquired the same
  sidecar through a second descriptor, blocking on itself. The flush now
  reuses the held authority (#487) -- 26f57a8e, with
  `tests/repro_fresh_clone_first_mutation.rs`.
- `br sync --import` no longer refuses a JSONL record that spells an absent
  text field as `""` (`acceptance_criteria`, `design`, `notes`, `assignee`,
  and the other optional text columns): the storage re-reads those as NULL,
  and strict import verification compared the record against its own
  literal form, rolling back the whole import with "does not match its
  normalized JSONL payload". The refusal now also names the differing
  fields. Found by the `db_bloat` doctor fixture (bead hrhx).
- `br doctor --repair` no longer discards the append-only tables `events`,
  `gate_results`, `gate_result_history`, `close_metadata`,
  `capacity_occupancy`, `capacity_exemptions`, and
  `capacity_exemption_history`: the rebuild snapshots and restores them
  (#471) -- 70e7fed9.
- `--bypass-policy` closes are exported to JSONL, so a bypassed close is
  visible on every clone, not only on the machine that made it (#474) --
  70e7fed9.
- `br update` refuses to replace a non-empty `description`, `design`,
  `acceptance_criteria`, `notes`, or `agent_context` with different content
  unless `--force` is given; writing into an empty field and re-writing the
  identical value stay silent (#467) -- d461a399. `--check-acceptance`,
  `--uncheck-acceptance`, and `--add-acceptance` tick, untick, and append
  acceptance items in place without `--force` (#477) -- c96c6623.
  `--append-notes <TEXT>` appends a line to `notes` without rewriting the
  field (repeatable, idempotent on re-run, conflicts with `--notes`, never
  needs `--force`); `--json` reports the resulting field (#480).
- `br show --json` and the MCP `show_issue` tool add `acceptance_items`
  (1-based `index`, `text`, `checked`) whenever `acceptance_criteria` holds
  a checklist, and the MCP `update_issue` tool takes `check_acceptance`,
  `uncheck_acceptance`, and `add_acceptance` with the same selectors as the
  CLI flags; both surfaces validate and rewrite through one shared helper
  (#477, bead iw7k.5).
- Engine sidecars left behind without their database file (`-wal-cert`,
  `-fsqlite-ns-gate`, `-wal`, ...) no longer wedge the fresh-database
  install: the writable recovery path and `br init` move them to
  `.beads/.br_recovery/<name>.<stamp>.orphaned-sidecar` first (bead avhq;
  `tests/repro_avhq.rs`).
- The reviewed schema-migration witness hashes DDL as tokens (comments,
  whitespace, parentheses, identifier quotes, and keyword case ignored), so
  the from==to maintenance-only plan no longer fails its `schema_sha256`
  attestation on the `VACUUM INTO` candidate whose table text FrankenSQLite
  re-serializes (bead 891u).
- The four pending-merge CLI gate scenarios that only existed as
  `#[ignore]`d unit tests now run end to end in
  `tests/e2e_basic_lifecycle.rs` against a receipt left by a real
  interrupted merge (bead mzpz).
- `br doctor --bundle <out.tar.gz>` writes the incident-evidence archive
  `docs/reliability/HEALTH_CONTRACT.md` had promised as "not yet
  implemented": doctor/health/sync/where captures with exit codes, listings,
  database-family hashes, the `metadata` and `sqlite_master` tables and
  recent events via a read-only engine connection, redacted copies of
  `metadata.json`/`config.yaml`; database bytes and `issues.jsonl` only with
  `--include-db` / `--include-jsonl`; e-mail addresses redacted; never
  overwrites (bead v7o2.4).
- The release reliability job and the scheduled CI reliability gates run
  `scripts/br-stress.sh` (8 processes, 60 s mixed workload against a copy of
  this repo's `.beads/`), failing on any corruption post-condition (bead
  dk45.4).
- The fsqlite grouped/HAVING IN-subquery count defect behind the multi-label
  AND detour (bead ro3m) is now pinned down by two tests: the literal
  statement counts correctly on fsqlite 0.3.15, the same statement with bound
  parameters counts 0. The detour stays; the ignored bound-parameter probe
  flips green on a fixed engine and names the detour to remove.
- Every `br` example in README.md's bash blocks now runs against the binary
  (`tests/e2e_readme_examples.rs`: 100 commands, placeholder ids, recipe
  variables, the README's own policy for the capacity example), and the
  README `config.yaml` example is checked for the prefix and default
  priority it promises. The capacity-exemption example was corrected (it
  limited no status and used a past expiry) (bead wqmw.2).
- A model-based differential test (`tests/model_based_storage.rs`) replays
  random operation sequences against `SqliteStorage` and an engine-free
  reference model, with fixed seeds for the GitHub #426 dependency-removal
  chain and GitHub #461 comment retention (bead dk45.7).
- The snapshot suite is green on main again: bv robot goldens are pinned to
  bv 0.22.0 and skip loudly on any other release; the reconcile dry-run
  byte-identity tests apply the GitHub #476 read-mark contract (beads
  zxfz.5, 3r45.3).
- A multi-process linearizability check (`tests/linearizability_multiprocess.rs`)
  drives eight concurrent `br` process streams (create, close, reopen,
  priority, labels, comments, dependency edges, `show --json`) against one
  workspace for 30 s, records every call with its invoke/return times, and
  searches each issue's history for a linearization consistent with real-time
  order and a sequential model; the published JSONL must then equal the
  linearized final state, `integrity_check` must be ok, and issue rowids must
  be dense. It runs in the CI and release reliability jobs (bead dk45.8).
- The Go-bd conformance suite runs again, against bd v0.46.0 built from the
  workflow's pinned commit: the harnesses now create their paired
  workspaces under an isolated temp root (with `TMPDIR` inside the
  repository, `br` had been walking up into the repository's own `.beads`
  and waiting on its write lock), honor `BD_BINARY` everywhere, introspect
  schemas in-process instead of through the `sqlite3` CLI, run `br` at
  `RUST_LOG=error`, and read br's paginated `list`/`blocked` payloads.
  Remaining differences are recorded in docs/TEST_HARNESS.md ("Known
  divergences from bd 0.46.0") and encoded as allowlist entries or ignore
  reasons: plain-text formats, `dep cycles` now agreeing under #391,
  pagination, dependency provenance fields, schema drift, and bd's silent
  `show` of an unknown id (bead zxfz.3).
- The doctor fixture suite runs on hosts without the `sqlite3` CLI
  (`db_bloat` uses python's sqlite3 module), the WAL-without-SHM fixture
  accepts the `-shm` that fsqlite 0.3.15 recreates on every open, and a new
  `config_unknown_keys` fixture plus a manifest exception row close the
  coverage-manifest gap for the two doctor checks added after v0.5.7
  (bead hrhx).
- Reviewed additive reconciliation has unit coverage for the three witness
  cases that were only asserted in prose: a byte-identical source replaced
  under a new inode with its mtime restored, a same-size rewrite with the
  mtime restored (only the raw content hash differs), and a superseded review
  token; each refuses before any row changes and the regenerated plan applies
  (bead 3r45.1).
- The #467 guard is proportional to what a write would lose (#481): a
  whole-field write that keeps at least half the current length passes
  (with a warning when it also keeps fewer than half the current words);
  clearing the field or shrinking it below half its length still needs
  `--force`, and the refusal states the ratio.
- `br sync --reconcile-additive --dry-run` is reachable and plan mode no
  longer hangs; the reviewed sync modes accept `--dry-run` (#473) --
  d461a399.
- JSONL import into an empty database (every `--no-db` invocation, first
  imports, rebuilds) proves the relation tables empty once and skips the
  per-record relation probe; import phases log elapsed times at `-vv`, and
  the `--no-db` rebuild shows progress on an interactive stderr instead of
  waiting silently (#483).
- `br list --tree` groups children under their parents with tree connectors
  in text output (#475) -- d461a399.
- Sync preserves authoritative cross-issue comment IDs (d2393c99) and rejects
  cross-issue comment ID collisions on import (34ca862b).
- Read-only database opens are strictly observational: the main file, WAL,
  and journal stay byte-identical and only the WAL reader-mark bytes of the
  `-shm` index may change; new doctor check `db.read_only_open_observational`
  (#476) -- 8e3300bc, 3d4fdc0f.

### Agent ergonomics

- `br label add/remove` take several labels per call, positionally
  (`br label add br-abc backend urgent`) or with repeated or comma-separated
  `-l`; a label placed before the issue id gets a hint instead of
  "Issue not found" -- 6bda0a41, ccaf0580.
- Priority filters accept ranges and lists (`0-1`, `P0-P2`, `0,2`) in
  `list`, `ready`, `blocked`, and `count`; a backwards range says so --
  6bda0a41, ccaf0580.
- Config key registry: `br config schema` lists every key br reads with
  aliases, types, and defaults; `br config set` warns on keys br does not
  read and names the nearest known ones; doctor check `config.unknown_keys`
  -- 95b0f620, 611d05af.
- `br doctor explain <finding-id|check-name>` shows the registry entry, a
  live observation, the fixers that can act on it, and next commands;
  `--list` enumerates; unknown ids exit 4 with suggestions -- ea7563d3.
- `br doctor --selftest [--selftest-dir DIR] [--keep]` drives the installed
  binary through a full issue lifecycle in a throwaway workspace and prints a
  `br.doctor.selftest.v1` receipt with platform and filesystem facts (case
  sensitivity, `renameat2` NOREPLACE support) -- a31e4e91.
- Dependency-type typos in `dep add --type` suggest the intended type; the
  #467 overwrite guard and priority errors carry their fix in the `hint`
  field of `--json` output as well as the text `Hint:` line -- ccaf0580.
- `br show` renders Markdown descriptions in Rich mode; `TERM=dumb` selects
  plain output on every detection path -- 5236351f, 950db95f.
- `br serve` (MCP) no longer holds `.beads/.write.lock` for its whole
  lifetime: the bootstrap open's result kept a clone of the database-family
  write authority alive, so every mutating tool (`create_issue`,
  `update_issue`, `close_issue`, `manage_dependencies`) waited 30 s and was
  cancelled, and CLI writes in other processes timed out while a server
  ran. Found by the new stdio protocol e2e (`tests/e2e_mcp_protocol.rs`),
  which drives the 2026-07-28 era end to end and checks the mutations
  through the CLI and `issues.jsonl` -- 06d738e5. `br capabilities`
  reports `build_features` (`mcp`, `self_update`).

### Sync: history knobs, per-mutation I/O, comment identity (#484, #485, #486)

- The per-mutation auto-flush (CLI and `br serve`) honors
  `sync.history_enabled: false` and `BR_HISTORY_MIN_INTERVAL_SECS` again;
  since bff86a41 it had snapshotted `.br_history` with the compiled-in
  defaults on every mutation, and only `br sync --flush-only` respected the
  knobs. One resolver, `config::resolved_history_config_from_layer`, feeds
  every exporter (#484) -- d03b2230.
- A one-line `br update` on an existing `issues.jsonl` reads the file once
  (one immutable snapshot feeds the conflict scan, the change scan, the
  replacement writer, and the history backup), verifies the staged output
  by digest instead of re-parsing every issue, proves the rename by inode
  instead of re-hashing, and deduplicates history from the digest stored in
  the snapshot's `.meta.json`; previously 11 full reads and 2 fsync'd
  writes. Read commands prove freshness with a `stat` against the new
  `jsonl_fast_witness` metadata (content hash bound to size, mtime, ctime,
  device, inode) instead of hashing the whole JSONL on every start; a
  same-size rewrite with a restored mtime is still detected because the
  ctime moves (#485) -- 07e60d95, 58d70456.
- Comment identity for sync is the payload `(issue, created_at, author,
  text)`, not the per-database rowid: two clones that each add a comment
  both mint id 1, and the merged JSONL now imports on both sides (the id
  that collides is reassigned locally) instead of refusing (`main`) or
  rolling back every command (v0.5.7). The 34ca862b cross-issue
  duplicate-id guard is removed; same-issue duplicates are still rejected
  (#486) -- 07e60d95.

### Engine: FrankenSQLite 0.3.13 -> 0.3.15

- 0.3.14 (ebc34bd7): generation-tagged checkpoint backfill watermark (the
  8 ms -> 320 ms per-statement schema-size cliff br trackers hit, engine
  GH#402), FTS5 stock-compat writer fix (GH#404), `PRAGMA wal_checkpoint`
  cumulative-nBackfill parity, FTS5 legacy-automerge origin-poisoning fix.
- 0.3.15 (52e66763): FTS5 `optimize` in-place migration, contentless
  empty-re-encode guard, legacy origin-poison self-heal.

### Release, CI, and docs plumbing

- One signed release asset family `br-<version>-<platform>` with `.sha256`
  and `.minisig`; the crates.io publish step is idempotent -- da7593b2.
- `ci.yml` leaned out: tracker and docs-only pushes skip CI, a six-hourly
  schedule covers drift, clippy runs with `-D warnings`, tests run as five
  file-name-derived shards via `scripts/test-shard.sh`, and the five-platform
  build, benchmarks, and reliability gates run on PRs, schedule, and manual
  dispatch instead of every push -- fc000cfe. The workflows themselves were
  still disabled when this section was written.
- README corrected against the binary (config keys, install flags, global
  flags, environment variables, `--robot` scope, command tables) and guarded
  by a test that checks every table example against `--help` -- fc000cfe;
  ARCHITECTURE and agent docs refreshed -- 5a1c3d55.
- `self_update` moved off the `1.0.0-rc` line to 1.3.0 (API unchanged since
  rc.6; `br upgrade` still does its own `.sha256` sidecar verification), and
  GitHub Action pins `taiki-e/install-action` v2.87.2 and
  `softprops/action-gh-release` v3.0.3 verified against upstream tags
  (Dependabot #478, #479) -- 4e0203fe.

## v0.5.7 -- 2026-08-29 (Release)

A doctor-consistency fix for agents plus the FrankenSQLite **0.3.13** engine
follow-up, cut after a five-round fresh-eyes review of the 0.5.x storage-safety
program. No schema or public-API changes.

### Doctor consistency: the health schema tripwire (#464)

`br doctor health` is the sub-200 ms, engine-free liveness gate agents run
before touching a tracker. It previously reported `healthy  ... db=ok`
(exit 0) whenever the `beads.db` file merely existed — even when the database
carried a schema `user_version` this binary refuses to mutate ("Schema version
mismatch: expected 17, found N"), so a "healthy" verdict could sit on a tracker
that rejected every write.

Health now reads the checkpointed schema version straight from the SQLite file
header (a pure byte read — no engine open, still well under budget) and, when a
present, non-zero version differs from the one this binary requires, reports
`findings_present` (exit 1) with `db=schema_incompatible` and a
`schema=<found>/<expected>` token, plus `schema_user_version` /
`schema_expected` / `schema_compatible` JSON fields. A non-SQLite or unset
header stays indeterminate, so a genuine current-schema database is never
falsely flagged. This makes the fast tripwire agree with what `br create`/`br
add` actually do, and points operators at `br doctor migrate-schema`.

### Engine: FrankenSQLite 0.3.13

Moves br to FrankenSQLite [0.3.13](https://github.com/Dicklesworthstone/frankensqlite/releases/tag/v0.3.13),
a GH#399 follow-up that rounds out the cross-process WAL reader-slot lifecycle:
on a failed reader-slot *release*, the engine now restores the stale slot id
(mirroring the register path) instead of dropping it while the shm SHARED claim
stays held. The 0.3.12 corruption fix (cross-process WAL reader registration +
checkpoint-horizon gating) it builds on remains in place. Engine commit:
`b1c4609d9` in <https://github.com/Dicklesworthstone/frankensqlite>.

### Issue triage

Independently verified and closed: **#465** (fresh `br init` + `br doctor`
gitignore check — reconciled in v0.5.6; the init template and doctor detector
now share one `INNER_GITIGNORE_EXPECTATIONS` list), **#428** (migrate-schema
16→17 integrity — the migration is now two-phase and integrity-gated, and the
engine freelist/orphaned-page corruption family is fixed; verified clean on the
real v0.2.19 schema-16 fixture), and **#464** above.

---

## v0.5.6 -- 2026-08-29 (Release)

Correctness, reliability, and resource-hygiene fixes from a deep multi-agent
review pass over the whole codebase. No schema or public-API changes; br stays
on FrankenSQLite 0.3.12.

### Data integrity

- Three-way merge and import de-duplication no longer silently drop a peer's
  `agent_context` (the beads_rust#297 inherited-governance field). It is a live,
  JSONL-serialized payload field but was missing from `Issue::sync_equals`, so
  two issues differing only in `agent_context` compared equal and one blob was
  discarded with no conflict or note; `persisted_import_issue_equals` had
  re-added it as a workaround, confirming the omission. It is now part of the
  equality check, with a regression test.
  ([6a6839f9](https://github.com/Dicklesworthstone/beads_rust/commit/6a6839f9))
- The blocked-issues cache can no longer report a blocked issue as *ready* after
  an unrelated write. An incremental cache refresh only recomputes its own
  parent-child component but then declared the whole cache fresh; if a prior
  deferred write (e.g. `add_dependency`) had marked the cache stale without
  writing the table, that change stayed missing. The mutation transaction now
  upgrades an incremental refresh to a full rebuild whenever the cache is
  already stale, honoring the "next non-deferred write rebuilds the cache"
  invariant.
  ([e202f67b](https://github.com/Dicklesworthstone/beads_rust/commit/e202f67b))

### Resource hygiene

- Ephemeral in-memory databases (`open_memory`) now delete every FrankenSQLite
  sidecar on teardown, not just `-wal`/`-shm`/`-journal`. The namespace
  (`-fsqlite-ns-gate`/`-fsqlite-ns-use`), WAL-cert (`-wal-cert`/`-wal-cert-head`),
  and `.fsqlite-migration-state` sidecars had been leaking into `TMPDIR` (a
  regression of the #299 cleanup — thousands of orphaned files observed); the
  cleanup now uses the full engine sidecar set and both coverage tests assert
  it.
  ([18840874](https://github.com/Dicklesworthstone/beads_rust/commit/18840874))

### Documentation

- `parse_flexible_timestamp`'s rustdoc now correctly states that it errors on a
  nonexistent local time (the skipped hour of a DST spring-forward) and resolves
  an ambiguous one (the repeated hour of a fall-back) to the earlier instant,
  instead of the reverse.
  ([e86336a5](https://github.com/Dicklesworthstone/beads_rust/commit/e86336a5))

## v0.5.5 -- 2026-08-29 (Release)

Engine fix for the multi-process page-aliasing corruption. br moves to
FrankenSQLite 0.3.12, which fixes the root cause end-to-end; the caller-side
containment from v0.5.4 stays in place as defense in depth.

### Storage safety

- Bumped the FrankenSQLite family to **0.3.12**. The engine now registers each
  on-disk read transaction as a cross-process WAL reader (a `WAL_READ_LOCK`
  slot with its pinned frame published to `aReadMark`) and gates the WAL
  checkpoint reset on the peer-reader horizon: a `wal_checkpoint(TRUNCATE)` no
  longer resets the WAL out from under a peer process mid-read, and a peer's
  commit-fold no longer reclaims live pages across a WAL-generation boundary.
  This is the root-cause fix for the page-aliasing corruption in #457, #458,
  #460, and #461 (FrankenSQLite #399 / #385 leaf-1). Verified: the
  concurrent-process TRUNCATE-checkpoint discriminator that reproduced this now
  runs every round clean with stock `integrity_check` ok (it corrupted in round
  0 before the fix); the multi-process `scripts/br-stress.sh` gate stays clean
  on the 0.3.12 engine.
- br's sole-opener checkpoint containment (v0.5.4) is retained as defense in
  depth; the two protections are independent.

## v0.5.4 -- 2026-08-28 (Release)

Storage-safety and Windows follow-up. Real migrated workspaces exposed
FrankenSQLite 0.3.11 page-aliasing corruption under short-lived multi-process
br workloads (#457, #458, #460, #461); the engine-side fix is tracked upstream
in FrankenSQLite #399/#385. br stays on FrankenSQLite — a memory-safe,
FFI-free Rust engine — and this release hardens the caller-side containment,
adds the multi-process stress gate that reproduces the corruption, and fixes
the Windows doctor/long-path defects.

### Storage safety and recovery

- WAL checkpoints now run only when br can prove it is the sole opener of
  the database. Every persistent open holds a shared `.br-db-openers-*.lock`
  lease next to the database; the periodic PASSIVE checkpoint, the
  quiescent-point TRUNCATE, and the exit-time TRUNCATE first upgrade that
  lease to exclusive and are skipped when another process has the database
  open, while new openers wait out an in-flight checkpoint. FrankenSQLite's
  multi-process checkpoint does not yet register peer read snapshots
  (FrankenSQLite #399/#385), and a checkpoint racing another short-lived br
  process is the interleaving behind the page-aliasing corruption in #457,
  #460, and #461; the same engine reproducer shows concurrent processes that
  never checkpoint stay clean.
- Force-import and recovery re-read every imported issue inside the
  transaction and compare normalized semantic fields, so field shifts or an
  unaddressable source ID fail before publication.
- Byte-identical duplicate comments in a damaged export are deduplicated for
  recovery; conflicting duplicates still fail closed.
- Repeated `comments add` exports preserve every prior comment ID and body;
  regression coverage reads the published JSONL directly so duplicated
  newest-comment rows cannot pass through cross-store agreement (#461).
- `show` checks the immutable JSONL ID set before reporting a missing partial
  ID, preventing corrupt lookup state from masquerading as a normal miss.
- History byte-budget pruning now removes only verified snapshot/metadata
  pairs and refuses mismatched evidence.
- Text-mode `list --limit N` with no other filter no longer depends on an
  `INDEXED BY` hint that a planner may refuse with `no query solution`; the
  paged list query spells its template predicate so the partial list index
  stays provably applicable instead (#463).
- `scripts/br-stress.sh` runs the multi-process mixed workload that exposed
  #457 against a copy of a real `.beads/` family and fails on any integrity,
  count, recovery-artifact, or doctor regression.

### Windows doctor and sync maintenance

- Doctor repair reuses the initially opened `actions.jsonl` handle instead of
  reopening the run log, fixing repeated Windows repair failures caused by
  file-sharing rules (#450).
- Creating the `.doctor/latest` convenience link is best effort when Windows
  returns `ERROR_PRIVILEGE_NOT_HELD`; run-directory receipts remain canonical,
  and undo discovers them without relying on the link (#456).
- Doctor run-directory, report, and mutate durability steps no longer open a
  directory handle for fsync on Windows, which always fails with
  `ERROR_ACCESS_DENIED` and turned every `doctor --repair` into an
  `os error 5` refusal there (#450, #456).
- Long workspace paths on Windows: br's no-replace `MoveFileExW` renames
  (fresh-database install, recovery staging, base-snapshot publication) use
  the extended-length (`\\?\`) spelling once the absolute path nears
  `MAX_PATH`, and sync path validation compares plain and extended-length
  spellings as the same location instead of rejecting `.beads/issues.jsonl`
  as outside its own directory (#462).
- Additive `sync --reconcile` gained hash-bound dry-run/apply receipts,
  source-path migration integration, and expanded history/preflight coverage.

### Release reproducibility

- FastMCP is now truly exact-pinned at `=0.7.0` as the manifest comment
  intended.

## v0.5.3 -- 2026-08-27 (Release)

Storage-correctness and release-hardening follow-up. The dependency stack now
consumes the published FrankenSQLite 0.3.11 schema fixes, machine-readable
search output has one stable envelope, oversized parent filters stay within
SQLite's parameter limit, and recovery paths preserve their evidence on more
fail-closed exits.

### Storage engine and schema correctness

- The complete `fsqlite` family moves to 0.3.11. Consecutive column `CHECK`
  clauses are now extracted from the original DDL coordinate system even when
  comments precede `CREATE TABLE`, so both the maximum and non-empty title
  constraints remain enforced.
- Compound table and column `CHECK` expressions with nested grouping now keep
  parseable source on the live schema path rather than becoming malformed and
  silently unenforced until reopen. VDBE code generation also fails closed if
  malformed internal CHECK metadata ever reaches it.
- `ALTER TABLE main.<staging> RENAME TO ...` now targets the persistent table
  when a same-named TEMP staging table is visible. The TEMP binding survives,
  persisted DDL is canonical and unqualified, and stock SQLite can reopen the
  result with a clean `PRAGMA integrity_check`.
- Reviewed schema comparisons normalize redundant partial-index grouping and
  canonical `CHECK` spellings without weakening the fail-closed contract;
  rebuilds also retain the full issues-index inventory.

### Search and large dependency sets

- JSON and TOON search results always use the stable
  `{issues, hidden_closed_count}` envelope, including zero matches and cases
  where no closed issue was hidden.
- Ready/list parent-membership filters are chunked instead of exceeding
  SQLite's bound-parameter ceiling on large graphs.

### Recovery, sync, and test isolation

- JSONL freshness checks hash the ledger before accepting it as current, and
  fail-closed database replacement no longer falls through to JSONL recovery.
- Schema-repair failures preserve the original authority and each recovery
  action retains its own snapshot rather than overwriting earlier evidence.
- Create, list-sort, golden-output, recovery, and benchmark fixtures now use
  isolated workspaces so an enclosing Beads checkout cannot change results.
- The Windows release installer validates and extracts ZIP artifacts on stock
  Git-for-Windows environments.

---

## v0.5.2 -- 2026-08-25 (Release)

Cross-platform correctness release driven by a full GitHub-issue triage pass.
Windows sync and schema migration are structurally repaired, stale engine
sidecars self-heal on write, `--rename-prefix` finally does what its name says,
and search stops hiding the fact that it hid something.

### Windows: JSONL export and sync repaired (#413)

- Every JSONL authority comparison (pinned route vs canonical sidecar
  authority, sibling-parent checks, `.beads` containment) now resolves both
  operands through one shared comparable-path convention — verbatim
  (`\\?\C:\...`) and 8.3-alias aware on Windows, byte-equal fast path
  elsewhere — instead of comparing lexical spellings against
  `fs::canonicalize` output byte-for-byte
  ([470d86a6](https://github.com/Dicklesworthstone/beads_rust/commit/470d86a6)).
  Reparse-route rejection still runs before any comparison-time resolution.
- Conditional JSONL publication is now implemented on Windows
  (rename-under-verified-authority) instead of being a hard platform error;
  downgraded publications surface through the existing
  `publication_atomicity` receipt field.

### Windows: reviewed schema migration repaired (#443, #446)

- The migration candidate's durability barrier synced through a read-only
  handle; Windows `FlushFileBuffers` requires write access, so every
  `migrate-schema apply` failed with os error 5 before installation began.
  The sync now uses a writable handle
  ([d8993ffb](https://github.com/Dicklesworthstone/beads_rust/commit/d8993ffb)).
- Every rename in the retention/install/rollback pipeline retries briefly on
  Windows sharing violations (AV/indexer handles) and names the stage and
  both paths on exhaustion; `failed.json` receipts now record `failed_stage`.

### Storage: stale WAL-certificate sidecars self-heal on write (#441)

- A write that fails because `-wal-cert` carries an unsupported record
  version (typically after an engine-generation upgrade) now quarantines
  `-wal-cert`/`-wal-cert-head` into verified `.br_recovery` backups, reopens,
  and retries once — the WAL itself is never touched, and the recovery path
  has no dependence on the JSONL merge anchor.

### Sync: `--rename-prefix` preserves ids and emits a mapping (#442)

- Prefix renames now replace only the prefix segment
  (`oldp-cargo-license-spdx-ay8` → `newp-cargo-license-spdx-ay8`); a doubled
  prefix collapses exactly once; regeneration happens only on collision and
  is flagged. Sync output gains a `prefix_renames` old→new receipt (omitted
  when empty), and the old id continues to be stashed in `external_ref`.

### Search: hidden-closed indication (#445)

- `br search` still excludes closed issues by default, but text output now
  prints `note: N closed match(es) hidden; rerun with --all to include them`
  and JSON/TOON gain `hidden_closed_count` when (and only when) matches were
  hidden; the legacy bare-array shape is preserved otherwise.

### Reviewed schema migration: 13–16 → 17 upgrades repaired

- Every reviewed migration from an older schema failed its canonical-shape
  attestation on all platforms: the engine's `VACUUM INTO` re-serializes DDL
  text (comments stripped, predicates re-parenthesized), so the candidate's
  partial-index predicates and issues CHECK constraints no longer
  token-matched the canonical schema. The maintenance pipeline now re-creates
  every auxiliary index from the canonical DDL and rebuilds the issues table
  when its declaration text drifted, so migrated databases carry the exact
  canonical spellings.
- Plan-token semantics tightened: re-applying the token of an already
  completed run now refuses with a clear re-plan instruction while the
  applied generation is still live, and legitimately re-derives after an
  undo. Both real-fixture upgrade e2e suites (schema 15 and 16) pass again.

### Test and release infrastructure

- Linux gnu release artifacts are now cross-compiled with cargo-zigbuild
  against an explicit glibc 2.28 floor, asserted with readelf before
  packaging; musl artifacts are asserted to carry zero GLIBC references
  (#444). `install.sh` additionally routes glibc hosts older than the gnu
  artifact floor to the statically linked musl artifact, closing the
  loader-failure gap on Debian 12 / Ubuntu 22.04 / RHEL 9 / AL2023 even if
  a future floor drifts.
- The linked-worktree vcs-status e2e test is hermetic against host git
  config, and effective-config probes honor caller `GIT_CONFIG_SYSTEM` /
  `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_NOSYSTEM` read-location overrides (#437).
- GitHub Action pin `taiki-e/install-action` bumped to v2.86.5 (verified
  upstream); `similar` 3.2.0 and `vergen-gix` 10.0.2 lockfile bumps.

---

## v0.5.1 -- 2026-08-25 (Tag)

Safety-focused storage and recovery release, with a retained profile-driven
speedup for structured `ready` output. The user-facing changes are deliberately
narrow: ready/scheduler robot output is more truthful, while database opens,
recovery, compaction, and reviewed schema migration now fail closed across more
replacement, crash, and schema-drift races.

### Ready and scheduler output correctness

- Structured `ready` output now hydrates labels with one batched lookup after
  candidate filtering and truncation instead of running a correlated aggregate
  for every row. JSON and TOON retain their complete label contract, text keeps
  its lean projection, and a custom-only ready status group such as `[rework]`
  no longer loses its sole candidate
  ([79fb0e1f](https://github.com/Dicklesworthstone/beads_rust/commit/79fb0e1f),
  [7cf131ba](https://github.com/Dicklesworthstone/beads_rust/commit/7cf131ba),
  [8050afa4](https://github.com/Dicklesworthstone/beads_rust/commit/8050afa4)).
- Empty `ready` text distinguishes a completed workspace, filters that hide
  otherwise-ready work, and active work that is not currently actionable; it
  no longer invents an exhaustive blocked/deferred/in-progress diagnosis.
- Scheduler recommendations carry the same labels in the issue payload and
  domain-contention evidence, treat blank or whitespace-only assignees as
  unassigned, and honor custom-only ready status groups
  ([346974cf](https://github.com/Dicklesworthstone/beads_rust/commit/346974cf),
  [685333c1](https://github.com/Dicklesworthstone/beads_rust/commit/685333c1)).

### Fast-open and schema safety

- A read-only fast-open process that waits for write authority now discards its
  pre-lock connection, reopens the canonical database inode, and rechecks JSONL
  freshness and pending-merge state against the frozen startup route. Probe
  errors are fatal instead of being downgraded into potentially stale output
  ([8050afa4](https://github.com/Dicklesworthstone/beads_rust/commit/8050afa4),
  [b541b73f](https://github.com/Dicklesworthstone/beads_rust/commit/b541b73f)).
- Runtime compatibility attests the full writable schema generation: supported
  `user_version`, tables and columns, defaults, primary/foreign keys, indexes,
  partial predicates, checks, table options, and the absence of persistent
  triggers. A schema-cookie witness keeps the steady-state fast path cheap and
  invalidates after DDL or contract changes
  ([346974cf](https://github.com/Dicklesworthstone/beads_rust/commit/346974cf)).
- Writable and authority-gated healing paths inspect both the stable database
  header and committed WAL page-one frames before opening or changing sidecar
  modes. A future schema, malformed committed WAL, unreadable main header, or
  changing recovery boundary is refused byte-neutrally before migration or
  downgrade can occur
  ([041c4a6c](https://github.com/Dicklesworthstone/beads_rust/commit/041c4a6c),
  [cac52b43](https://github.com/Dicklesworthstone/beads_rust/commit/cac52b43)).
- Pending sync-merge protection now classifies reconcile apply and capacity
  exemption mutations as writes that must refuse; reconcile dry-runs and
  exemption listings remain available as read-only diagnostics.

### Recovery, compaction, and reviewed migration

- Database replacement paths retain and reverify exact inode authority through
  installation, finalization, rollback, and recovery. No-replace moves refuse
  foreign targets, staged backup generations are fingerprinted before restore,
  and namespace durability failures either restore the attested original or
  return an explicit uncertain state without mixing generations
  ([1feb09dd](https://github.com/Dicklesworthstone/beads_rust/commit/1feb09dd),
  [21eac9c8](https://github.com/Dicklesworthstone/beads_rust/commit/21eac9c8),
  [cd5ac3e1](https://github.com/Dicklesworthstone/beads_rust/commit/cd5ac3e1)).
- `doctor migrate-schema apply` persists a receipt-bound commit-ready marker
  before installing its compacted candidate. A retry can distinguish the exact
  original, exact installed generation, and an unrecognized generation after a
  crash; apply and undo preserve raw-family witnesses and keep recovery
  artifacts when proof is incomplete. Migration rebuilds also refuse staging
  table collisions, including TEMP-schema shadows.
- Windows replacement uses an atomic no-replace move and retained-handle file
  identity rather than creation-time equality, closing same-timestamp and
  replace-between-check-and-use gaps
  ([8050afa4](https://github.com/Dicklesworthstone/beads_rust/commit/8050afa4),
  [66972c60](https://github.com/Dicklesworthstone/beads_rust/commit/66972c60)).

### Performance evidence and limits

- The matched-host A/B that replaced correlated ready-label aggregation with
  post-truncation batching measured `62.802528 -> 45.135305 ms` median
  (`28.131388%` lower) and `72.785260 -> 50.091848 ms` p95 (`31.178582%`
  lower), with byte-identical JSON/TOON/text outputs and the text control within
  the `5%` regression guardrail. Those numbers compare the recorded
  `e2e814af` and `79fb0e1f` revisions; they are not presented as an
  exact-current profile of the later safety wave.
- The profile-first campaign completed its 50 serial passes and retained only
  measured candidates. Rejected, reverted, within-noise, and blocked ideas now
  have retry predicates in `docs/progress/perf-negative-results.md`; the broad
  `2x` median-and-tail target was not achieved.
- A database carrying the previous runtime-witness contract performs the full
  schema attestation on lock-free reads until an ordinary writable open records
  the current witness. On the pre-release v0.5.0 runtime logic carried
  unchanged into v0.5.1, a balanced 100-sample `release-perf` profile measured
  the prior-contract path at `90.605341 ms` median / `101.779796 ms` p95 and
  the current-witness fast path at `51.977261 ms` / `67.686561 ms`, reductions
  of `42.633337%` / `33.497056%`. The conservative current-witness control with
  fast-open disabled measured `84.227769 ms` / `116.585020 ms`; output was
  identical and all database witnesses and integrity checks remained valid.
  The v0.5.1 binary differs in release metadata, and its release tree includes
  the corrected replay harness, so those figures are deliberately not
  described as byte-identical v0.5.1 profiling. The read-only path
  intentionally does not persist a witness.

### Packaging and regression coverage

- The locked HTTP/2 stack moves from `h2` 0.4.15 to 0.4.17, incorporating the
  upstream fix for [RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258.html)
  and its follow-up stream/error-budget hardening
  ([10bcadca](https://github.com/Dicklesworthstone/beads_rust/commit/10bcadca)).
- For v0.5.1, Cargo and Nix source metadata moved to 0.5.1; the checksummed
  Homebrew, AUR, and Scoop definitions now use the published DSR-built archive
  hashes. Cargo points at the repository's rider-bearing `LICENSE` via
  `license-file` instead of incorrectly declaring the package as plain SPDX MIT
  ([413f9948](https://github.com/Dicklesworthstone/beads_rust/commit/413f9948)).
- Every platform archive now contains the rider-bearing `LICENSE`. DSR signed
  all seven archives, including the Windows zip, and verified each signature
  against the recoverable key `RWTQoKUb...` (ID `36B847D11BA5A0D0`) before
  publication. The public release includes every canonical archive, checksum
  sidecar, `.minisig`, SBOM, and SLSA provenance file. No GitHub Actions
  workflow was used. Raw v0.4.1 sidecars already used this key; v0.5.1 corrects
  the documentation drift. `RWS7nGFf...` remains v0.4.0 history, while the
  older CI-era `RWSp4vEO...` key is unrecoverable
  ([ada0b658](https://github.com/Dicklesworthstone/beads_rust/commit/ada0b658),
  [78fc31a1](https://github.com/Dicklesworthstone/beads_rust/commit/78fc31a1),
  [f64d9de2](https://github.com/Dicklesworthstone/beads_rust/commit/f64d9de2)).
- Package-manager metadata no longer represents the custom rider as plain MIT:
  Homebrew uses `:cannot_represent`, AUR uses
  `LicenseRef-MIT-OpenAI-Anthropic-Rider`, Nix uses `licenses.unfree`, and Scoop
  links the repository license. The v0.5.1 Homebrew and AUR definitions install
  the bundled rider-bearing file, Nix installs it from source, and Scoop
  receives it inside the release zip
  ([f64d9de2](https://github.com/Dicklesworthstone/beads_rust/commit/f64d9de2)).
- New inline and end-to-end counterexamples cover custom ready groups, scheduler
  label/assignee evidence, canonical-inode fast-open reprobes, config-route
  drift, future schemas in WAL, same-cookie version changes, runtime schema
  shape drift, schema-migration crash resume/rollback, and Windows replacement
  identity. Workflow-fragment tests pin the archive/signature inventory,
  current-vs-retired key boundary, and missing-secret/missing-sidecar failures.
  Performance figures above remain revision-scoped; this changelog does not
  promote earlier gate runs into exact-current release certification.

## v0.5.0 -- 2026-08-25 (Tag)

Unpublished stabilization tag. After an earlier topology failure stopped the
release gate before this target ran, the canonical reliability replay exposed
historical fixture captures that no longer modeled their manifests. Two
malformed-primary cases paired a plain-text database with a committed
legacy-schema WAL, while the WAL-without-SHM case restored an ASCII placeholder
instead of a valid current-family WAL. Runtime correctly refused both ambiguous
families. The replay setup now preserves those historical artifacts outside the
active family and constructs the intended current topology. No v0.5.0 GitHub
Release, crates.io package, or platform assets were published. v0.5.1
supersedes this tag without moving or rewriting it.

## v0.4.1 -- 2026-08-24 (Release)

Windows recovery wave. v0.4.0 was effectively unusable on Windows (GitHub
#438): every command -- including `br init` on a fresh repo -- failed with
`internal error: windows lock files are closed`. This release restores
Windows to working order and fixes a write-blocking recovery bug affecting
migrated workspaces on every platform.

### Windows: total 0.4.0 breakage fixed (GitHub #438)

- The storage stack moves to fsqlite 0.3.9
  ([70a0ea4b](https://github.com/Dicklesworthstone/beads_rust/commit/70a0ea4b)).
  Root cause was upstream: a Windows read-only open became sidecar-less in
  fsqlite 0.3.x, but four lock-path consumers were missed, so every
  read-only open+close cycle -- which br's sync-authority inspection
  performs on every command -- failed. Fixed in frankensqlite
  `2fda33ab0` + `7ed48fee1` (shipped in fsqlite 0.3.9), verified on real
  Windows hardware against both the 0.3.6 and 0.3.8 failure modes.

### Windows: clean process exit, honest exit codes (GitHub #439, [0b74719e](https://github.com/Dicklesworthstone/beads_rust/commit/0b74719e))

- Every `br` command on Windows previously panicked at exit with
  `threads should not terminate unexpectedly` (0xC0000409), corrupting
  `$LASTEXITCODE` even for successful commands. Deliberate exits are now
  routed through a single shutdown funnel
  ([05ab336d](https://github.com/Dicklesworthstone/beads_rust/commit/05ab336d))
  that flushes stdout/stderr and exits via `TerminateProcess` with the
  real code on Windows (plain `std::process::exit` elsewhere), after
  storage is dropped so WAL checkpointing (#270) is preserved. The
  Windows call is the crate's third sanctioned `unsafe_code` carve-out
  ([eb068133](https://github.com/Dicklesworthstone/beads_rust/commit/eb068133)).

### Recovery no longer rejects migrated workspaces (GitHub #440, [08404d53](https://github.com/Dicklesworthstone/beads_rust/commit/08404d53))

- Any write that triggered automatic JSONL recovery failed with
  `Prefix mismatch ... expected bd` when `issues.jsonl` contained legacy
  non-`bd-`-prefixed ids (e.g. imported/migrated trackers). Recovery and
  rebuild paths now skip prefix validation for the workspace's own
  sidecar -- consistent with auto-import and reconcile, which already
  did -- and round-trip mixed prefixes untouched, with an e2e regression
  test proving ids survive un-renamed.

### Storage and sync correctness

- Fresh-database replacement now verifies its witness before snapshot
  import queries, and the linear witness survives transaction retries
  ([c87e1b43](https://github.com/Dicklesworthstone/beads_rust/commit/c87e1b43),
  [2676ba7a](https://github.com/Dicklesworthstone/beads_rust/commit/2676ba7a)).
- The profile-first campaign retained fast-open synchronized default reads
  and the short-circuited empty external-epic join. Relation-count fusion was
  explicitly reverted after it missed the retention gates
  ([70928a3f](https://github.com/Dicklesworthstone/beads_rust/commit/70928a3f)).
  v0.4.1 also shipped SQL-side label projection for structured `ready` output
  (GitHub #309,
  [0064a5dd](https://github.com/Dicklesworthstone/beads_rust/commit/0064a5dd)).

## v0.4.0 -- 2026-08-22 (Release)

Restricted-filesystem robustness wave plus one CLI capability. The headline
fixes make `br` behave correctly where the filesystem is not a friendly
local ext4/APFS mount: sandboxed (Landlock/locked-down) environments, WSL2
DrvFS/9p mounts, and plain Unix pipelines. Version moves to 0.4.0 for the
new comment-search/`show` surface and the dependency-line moves below (the
storage stack steps to fsqlite 0.3.6 and the MCP feature to fastmcp-rust
0.7.0); CLI behavior is otherwise compatible apart from the fixes.

### Comment search and inherited context in `show` ([6b6a860f](https://github.com/Dicklesworthstone/beads_rust/commit/6b6a860f))

- `br search` now matches comment bodies, not just issue title/body fields.
- `br show --json` emits the structured `inherited_context` chain, so agents
  can read root-item context (the binding-rules pattern) without walking the
  dependency graph themselves. Golden coverage pins both surfaces
  ([7b72f7d2](https://github.com/Dicklesworthstone/beads_rust/commit/7b72f7d2)).

### Die like a Unix filter, not with a core dump (GitHub #434, [80ee8690](https://github.com/Dicklesworthstone/beads_rust/commit/80ee8690))

- `br list | head` (any text-mode output into a closed pipe) previously
  aborted with SIGABRT: Rust ignores SIGPIPE, bare `println!` panics on
  EPIPE, and the `panic="abort"` profile turned that into an abort with a
  macOS crash report. Text-mode commands now restore `SIGPIPE` to
  `SIG_DFL` at startup and die silently by signal 13 like every other Unix
  filter.
- The robot contracts are unchanged: `--json`, `--toon`, and robot output
  keep the deliberate exit-0-on-EPIPE contract from
  [d14ec978](https://github.com/Dicklesworthstone/beads_rust/commit/d14ec978),
  and `br serve` keeps treating EPIPE as a hard error. The restore is the
  crate's second sanctioned `unsafe_code` carve-out, documented in
  Cargo.toml.

### Locked-down and sandboxed environments (GitHub #436, [f0fb2dec](https://github.com/Dicklesworthstone/beads_rust/commit/f0fb2dec), [5614bd40](https://github.com/Dicklesworthstone/beads_rust/commit/5614bd40))

- Stable-route traversal opened `/` and every ancestor directory
  `O_RDONLY`; under a Landlock (or similar) policy that only grants the
  workspace, that read of `/` was refused and sync failed outright. On
  Linux the traversal anchors now open `O_PATH|O_DIRECTORY|O_NOFOLLOW`,
  which performs no readable open at all (symlink hostility checks are
  preserved); macOS/BSD behavior is unchanged.
- The immutable JSONL snapshot backing was an anonymous tempfile in
  `TMPDIR` — outside the sandbox grant. It is now allocated beside the
  JSONL inside `.beads/`, falling back to `TMPDIR` only if the workspace
  refuses it.

### WSL2 DrvFS/9p export publication (GitHub #419, [ca316245](https://github.com/Dicklesworthstone/beads_rust/commit/ca316245), [4ed3b1af](https://github.com/Dicklesworthstone/beads_rust/commit/4ed3b1af))

- `br sync --flush-only` on 9p/DrvFS failed with EINVAL: the filesystem
  rejects flagged (`RENAME_NOREPLACE`-style) renames, so the
  `beads.base.jsonl` anchor was never written and orphaned `.tmp` files
  accumulated. Publication now falls back to a witness-re-verified plain
  rename when the flagged rename reports
  EINVAL/ENOSYS/ENOTSUP/EOPNOTSUPP — destination errors still surface, and
  a planted-negative test proves the fallback refuses to clobber a
  destination that was mutated underneath it.
- Downgraded publications are visible, not silent: `FlushResult` carries
  `publication_atomicity: "replace_under_authority"` (emitted only when
  downgraded), with a matching text/rich warning line. Issue #419 stays
  open until the fix is confirmed on real WSL2 by the reporter.

### Linked git worktrees resolve to the primary checkout (GitHub #429, [44c7a6f0](https://github.com/Dicklesworthstone/beads_rust/commit/44c7a6f0))

- In a linked worktree, `.beads` discovery followed the worktree's own
  root and produced a second, divergent workspace. Discovery now resolves
  through `.git`-file indirection to the primary checkout's `.beads`.

### Doctor accuracy

- Satisfied blockers report Ok; only genuinely dangling edges warn
  (GitHub #432, [1b385c23](https://github.com/Dicklesworthstone/beads_rust/commit/1b385c23)).
- Vacuum/namespace sidecars are recognized by probing, not by textual
  pattern match (GitHub #427, [7a1a990e](https://github.com/Dicklesworthstone/beads_rust/commit/7a1a990e)).
- A non-regular `.write.lock` node (symlink, directory, FIFO) fails closed
  ([2f32dcfc](https://github.com/Dicklesworthstone/beads_rust/commit/2f32dcfc));
  an absent `sqlite3` CLI is reported as benign host state rather than a
  warning ([5e638d23](https://github.com/Dicklesworthstone/beads_rust/commit/5e638d23)).
- The fsqlite 0.3.6 migration-state sidecar family is created, audited,
  and repaired alongside the database
  ([d2148c72](https://github.com/Dicklesworthstone/beads_rust/commit/d2148c72),
  [42a4b1f2](https://github.com/Dicklesworthstone/beads_rust/commit/42a4b1f2)).

### Sync integrity

- Stored-hash health is rejected when the database does not cover the
  JSONL's ids, so a partially-imported workspace can no longer pass the
  freshness check on hashes alone
  ([876663fc](https://github.com/Dicklesworthstone/beads_rust/commit/876663fc)).

### Dependencies

- Storage engine: fsqlite 0.3.6 (registry graph), with fsqlite's `json`
  default feature disabled so serde_json's Number representation stays out
  of TOON output
  ([57a16b2b](https://github.com/Dicklesworthstone/beads_rust/commit/57a16b2b),
  [ee081bb0](https://github.com/Dicklesworthstone/beads_rust/commit/ee081bb0)),
  plus a workaround for fsqlite 0.3.6's NULL `COUNT(*)` over grouped
  IN-subqueries ([42a4b1f2](https://github.com/Dicklesworthstone/beads_rust/commit/42a4b1f2)).
- Runtime lockstep: asupersync `=0.4.9` and cap-primitives `=4.0.3`
  ([d1d64bc7](https://github.com/Dicklesworthstone/beads_rust/commit/d1d64bc7)),
  with the optional `mcp` feature moved to fastmcp-rust 0.7.0 — the
  published release that shares the `=0.4.9` pin. (The asupersync bump
  alone had left the crates.io graph unresolvable: fastmcp-rust 0.6.0
  exact-pins `=0.4.8`.)

### Validation

- Full `cargo test --all-features --no-fail-fast` on the release tree
  (linux x86_64, 128-core builder): **21,992 passed, 117 ignored** across 149
  test binaries plus doctests. Six initial failures, all dispositioned:
  two version-propagation tests (plugin manifest, agent-baseline
  `version.json`) exposed an incomplete bump and were fixed by completing
  it — their binaries re-ran green (177 + 174 passed); three bv
  robot-goldens hard-require exactly bv v0.19.0 (the host had v0.20.0) and
  pass with v0.19.0 pinned on PATH (260 passed); one linked-worktree
  vcs-status test fails on hosts with git-lfs filters registered in
  system/global gitconfig — reproduced identically at the pre-release
  commit 5614bd40, i.e. pre-existing and environment-coupled, tracked in a
  follow-up issue.
- `cargo clippy` at `-D warnings` and rustfmt clean via the dsr quality
  gate on the release tree.

---

## v0.3.2 -- 2026-08-14 (Release)

Correctness follow-up to the v0.3.1 stabilization tag. The complete
all-features suite exposed a low-frequency scratch-database open failure under
high parallel load before v0.3.1 was published, so the immutable fix ships as
the next patch version rather than rewriting that tag.

### Validation

- Full `cargo test --all-features --no-fail-fast` on the release tree
  ([4104c31e](https://github.com/Dicklesworthstone/beads_rust/commit/4104c31e)): 21,491 passed, 0 failed (doctests included).
- Release assets arch-verified per platform: the darwin_amd64 slot is a
  genuine x86_64 Mach-O built with the pinned cross target and executes
  under Rosetta; linux musl binaries are statically linked; the linux_amd64
  asset was download-verified against SHA256SUMS with a full init/create
  round-trip.

### Fixed

- Ephemeral `SqliteStorage::open_memory()` databases now retry a bounded eight
  times when FrankenSQLite transiently returns `CannotOpen` for br's generated
  `beads_mem_<pid>_<counter>.db` path. Each failed attempt is fully cleaned and
  retried with a new unique path; every other error still returns immediately.
  This closes intermittent `sync --import-only --force` and deterministic merge
  failures seen only when the full integration matrix opened many scratch
  databases concurrently.
- Workspace-evolution failures now include the child command's stdout, stderr,
  and log path instead of reporting only exit code 2, preserving the evidence
  needed to distinguish storage failures from assertion failures.

## v0.3.1 -- 2026-08-14 (Tag)

Same-day follow-up to v0.3.0: asupersync moved to 0.4.4, the lockfile was
refreshed to latest-compatible across the graph, and two load-sensitive
release-gate tests were repaired.

### Dependencies

- asupersync 0.4.3 → 0.4.4 (with its franken-kernel/evidence/decision
  family). Upstream 0.4.x preserves the 0.4.3 public API; the notable 0.4.4
  change (spawned-task results surviving cancellation acknowledgement) does
  not affect br's block_on bridge, which spawns no tasks.
- Pinned the Rust toolchain to `nightly-2026-08-13` so local, RCH, and
  cross-platform release builds use the same compiler, rustfmt, and Clippy
  lint inventory instead of resolving a host-dependent floating nightly.
- Full `cargo update` lockfile refresh: ~45 transitive crates to latest
  compatible (aho-corasick 1.1.5, aws-lc-rs 1.18, blake3 1.8.6, cc 1.4.3,
  futures 0.3.34 family, http 1.5, icu 2.3 family, and others). Direct
  dependencies were already at latest stable (sha2 0.11, rand 0.10,
  signal-hook 0.4.4, similar 3.1.2); the deliberate holdouts remain
  self_update (pre-release pin), cap-primitives (=4.0.2 by design), and
  fastmcp-rust's own asupersync 0.3.10 line (awaiting its 0.4.x republish).

### Fixed

- v0.3.0's `darwin_amd64` release asset (and its `darwin_x86_64` alias)
  contained an arm64 binary: the local-release pipeline built the Intel slot
  on an Apple Silicon host without forcing `--target x86_64-apple-darwin`,
  so Intel Macs downloaded a binary that cannot run. The v0.3.0 assets were
  rebuilt from the same tag and replaced in place (checksums regenerated,
  Rosetta-verified), and the release pipeline now pins the cross target for
  the darwin/amd64 slot. v0.2.22 and earlier were unaffected (CI built them
  with explicit targets).
- The installer's `/tmp/br-install.lock` now honors `TMPDIR` (POSIX
  convention), and the installer test suite plants its deliberately stale
  lock inside a per-test TMPDIR — parallel installer tests could previously
  race each other's recovery of the shared planted lock.
- The concurrent `sync --flush-only` failure-injection scenario now drains
  both subprocesses simultaneously. Previously, inherited verbose dependency
  tracing could fill the lock holder's captured stderr while the harness was
  synchronously waiting on the losing child, creating a test-only pipe/lock
  deadlock. The repair was verified with a live GDB backtrace and 50 repeated
  contention runs.
- The sync-merge receipt boundary scenario now asserts its actual semantic
  precondition (`receipt cutoff < tombstone expiry`) instead of requiring an
  arbitrary five seconds of unused headroom, removing load-sensitive false
  failures without weakening the post-expiry replay proof.

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

### Engine-upgrade adaptations in the storage layer

- The doctor's SQLite file-family audits now cover fsqlite 0.2+'s
  parallel-WAL durability-certificate sidecars (`-wal-cert`,
  `-wal-cert-head`) alongside the classic and namespace sidecars, so legacy
  repair audits account for the whole on-disk family.
- Writable connections, including explicit read-write compatibility opens,
  select the engine's serialized mode at open
  (`PRAGMA fsqlite.concurrent_mode = OFF`), matching br's existing workspace
  write-lock serialization (#243). This avoids a concurrent-mode
  `BusySnapshot` self-conflict when a legacy schema migration copies from a
  table and drops that source in the same transaction, while preserving the
  migration's original crash-atomic copy/drop/rename boundary.
- Missing-database JSONL recovery quarantines the complete orphaned fsqlite
  0.3 sidecar family (`-wal-cert`, `-wal-cert-head`, `-fsqlite-ns-gate`, and
  `-fsqlite-ns-use`) into verified `.br_recovery` backups before initializing
  the replacement database, preserving the old bytes without letting stale
  engine state block or contaminate the rebuild.
- The sync bridge retries `BusySnapshot` on self-contained autocommit
  statements (the statement is the whole transaction, so retrying it is
  exactly the engine contract's "retry the whole transaction"), in addition
  to the existing bounded `BusyRecovery` retry.

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

### Routed mutations no longer self-deadlock on the target write lock

- Routed `update`, `close`, `defer`, `undefer`, `delete`, and `reopen`
  against an external workspace (`.beads/routes.jsonl`) acquired the
  target's database-family write lock but never marked it as held before
  the storage open tried to take the same `.beads/.write.lock` from a
  second descriptor in the same process — flock-style locks conflict
  across descriptors, so every routed mutation hung until the 30s lock
  timeout. They now mark the held lock the same way `comments`, `dep`,
  `label`, and the read-side routed commands always did.

### Read-only fast open restored (with its migration barrier intact)

- Read-only commands that waive auto-import and auto-flush
  (`--no-auto-import --no-auto-flush list/show/ready/...`) again bypass the
  startup writer-lock queue and open the current-schema database read-only,
  re-landing the fast-open contract from [1b75961a](https://github.com/Dicklesworthstone/beads_rust/commit/1b75961a) that the #412 rescue
  snapshot ([251b501b](https://github.com/Dicklesworthstone/beads_rust/commit/251b501b)) had reverted — a held `.write.lock` no longer blocks
  the whole read matrix, and `sync --reconcile --dry-run` proceeds under
  lock contention as documented.
- The conservative path's implicit-migration barrier moved with it: a
  fast-open miss on an existing database now re-runs the pending-merge and
  schema-version inspection under the database-family authority it acquires
  for the writable fallback, so a stale-schema database still refuses into
  the reviewed `br doctor migrate-schema plan` workflow (and a pending sync
  merge still refuses writable recovery) instead of silently auto-migrating.

### `br serve` no longer deadlocks against its own startup lock

- The MCP server acquired the database-family write lock at startup (for
  the pending-merge mutation gate and a preopened storage context), then
  `run_serve` and every per-request mutation handler tried to take the same
  flock through fresh descriptors in the same process — so `br serve` hung
  before ever reaching the stdio transport, and SIGINT could not shut it
  down. Serve now skips the storage preopen entirely, releases the gate
  authority (and its marked `Arc` clones) once the pending-merge verdict is
  final, and the server plus its handlers manage locking per request as
  designed. Combined with cancel-context wiring through
  `run_transport_returning_with_cx`, SIGINT/SIGTERM now terminate `br
  serve` promptly.

### Flush anchor publication is fail-closed again

- `br sync` flush certified an export and cleared dirty state even when
  publishing the `beads.base.jsonl` merge anchor failed, leaving `sync
  --status` reporting "In sync" while the three-way-merge ancestor was
  stale. The anchor is published before export finalization again and a
  publication failure is a hard error naming the anchor path; dirty state
  stays set so a plain retry flush converges.

### CI supply-chain inventory refreshed

- The merged actions-group bump (#406) updated workflow SHA pins without
  refreshing `.github/action-pins.jsonl` / `action-pin-upstreams.jsonl`;
  the inventory now records the same upstream-verified identities
  (actions/checkout v7.0.1, dtolnay/rust-toolchain default-branch head
  2026-07-16, taiki-e/install-action v2.85.5, actions/setup-go v7.0.0,
  softprops/action-gh-release v3.0.2), each SHA re-verified against its
  upstream tag before recording.

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

### Validation

- Full `cargo test --all-features --no-fail-fast` on the release tree:
  21,490 passed, 0 failed (doctests included), up from 21,415 passed /
  70 failed at the start of the migration wave.
- `cargo clippy --all-targets --all-features -- -D warnings` clean
  (pedantic + nursery at deny); `cargo fmt --check` clean.
- Repo `.beads` database migrated v15 → v17 through the reviewed
  `doctor migrate-schema` plan/apply workflow (rehearsed on an isolated
  copy first; receipts under `.beads/.br_recovery/schema-migrations/`,
  post-apply integrity verified with both engines).

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
