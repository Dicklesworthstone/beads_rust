# Bridge Plan: beads_rust (`br`)

**Reality check date:** 2026-09-01
**Plan revision:** 3 (two ambition rounds applied in place; see §9)
**Baseline:** installed `br 0.5.7` = Cargo.toml 0.5.7 = latest GitHub release (2026-08-29); `main` at `ebc34bd7` (fsqlite 0.3.14)
**Gap count:** 6 critical, 14 major, 9 minor (29 gaps)
**Beads:** 16 open / 5 in_progress / 954 closed at check time; every open bead is unblocked
**Estimated work:** ~3 focused agent-weeks of code plus ~1 week of docs/tracker hygiene; engine-boundary items depend on upstream FrankenSQLite

This document is the Phase 2 output of the reality-check workflow. It is revised **in place** during ambition rounds and then converted into beads with the frozen Phase 3a template. Every gap carries enough context that a bead generated from it stands alone: background, current code locations, target, success criteria, implementation steps, tests and logging, dependencies, and bead coverage.

Guiding principles for every gap:
1. **Proof over prose.** A gap is closed only when a named test, gate, or receipt demonstrates it. Every implementation bead has a companion test with structured logging so failures are diagnosable from CI output alone.
2. **Structural parity, not copied lists.** Where the same check must run in four places (CI, release, DSR, local), one manifest drives all four.
3. **Docs as data.** Anything that can be generated from `br capabilities`, `Cargo.toml`, or the module tree is generated and checked, never hand-copied.
4. **No silent state.** Dormant code, stale claims, disabled workflows, and ignored tests each carry a reason, an owner, and a revisit trigger, or they are removed with approval.

---

## 1. Where the project actually is

### 1.1 What the reality check established

| Claim | Evidence |
|---|---|
| The CLI is real, not scaffolding | 88 leaf subcommands; exhaustive `match` in `src/main.rs:585-910` with no catch-all; zero `todo!`/`unimplemented!`/TODO/FIXME in production code; only stub is `br doctor explain` (`src/cli/commands/doctor_subsystems/surface.rs:1951`); only ignored flag is `br doctor capabilities --command` (`surface.rs:146`) |
| The shipped binary works end to end | 83-step lifecycle smoke against installed 0.5.7 covering init, create, deps, ready/blocked, labels, comments, claim, defer/undefer, close/reopen, epic, lint, auto-flush, all sync modes, doctor, migrate-schema plan, capabilities/schema/robot-docs, TOON, completions, config, agents, orphans, changelog, tombstone delete, capacity hard limit, cross-project routing, external deps. 81 passed; the 2 failures were README syntax the CLI rejects (Gap 6) |
| Sync safety invariant holds | `grep -rn 'Command::new.*git' src/sync/ src/cli/commands/sync.rs` is empty; bare `br sync` refused; 13 git-safety e2e tests in `tests/e2e_sync_git_safety.rs` |
| Latency is far inside the plan's targets | On the project's own 977-issue tracker: `ready`, `list --limit 0`, `show`, `stats`, `blocked`, `sync --status` each ~10 ms; `doctor --json` ~0.8 s |
| Unit suite on `main` has exactly one known failure | `cli::commands::doctor::tests::pending_sync_merge_authority_inspector_is_coherent_and_byte_identical` (GitHub #476) fails deterministically at `src/cli/commands/doctor.rs:14257`; the 11 lib failures recorded in bead `beads_rust-9krz` now pass; partial full run 1066 passed / 1 failed / 5 ignored |
| `cargo fmt --check` clean | local run |
| `cargo clippy --all-targets -- -D warnings` | **inconclusive**: killed by RCH's 5-minute cap on two workers; the last CI clippy run (2026-08-19) failed |
| Full integration suite | **could not complete** through RCH: a cold compile of 162 test binaries exceeds the 30-minute cap; UPGRADE_LOG.md (2026-08-14) recorded 21,490 passed / 0 failed |

### 1.2 Vision checklist (condensed)

Status legend: WORKING, PARTIAL, STUB, UNPROVEN, NOT_STARTED, DEFERRED, WRONG_APPROACH, REGRESSED.

| # | Goal | Source | Status | Gap |
|---|---|---|---|---|
| V1 | Classic bd command set ported (CRUD, deps, labels, comments, sync, stale, orphans) | porting plan | WORKING | — |
| V2 | SQLite + JSONL hybrid frozen; no Dolt | porting plan, README | WORKING | — |
| V3 | Non-invasive: never runs git for sync, no hooks, no daemon | README §Design 1, 3 | WORKING | — |
| V4 | Schema compatible with Go bd | PROPOSED_ARCHITECTURE | WORKING (superset) | — |
| V5 | Hash-based IDs, content-hash dedup | README, AGENTS.md | WORKING (hash bytes intentionally diverged at schema v14) | G7 |
| V6 | Output parity with Go bd proven by conformance tests | porting plan | UNPROVEN (workflow disabled; skips without a real `bd`) | G12 |
| V7 | Every command supports `--json`; clean stdout; structured errors with exit codes | README §Design 4, AGENTS.md | WORKING | — |
| V8 | TOON output and env precedence | AGENTS.md | WORKING | — |
| V9 | Rich TTY output, Plain when piped, NO_COLOR | README §Design 5 | WORKING | — |
| V10 | Syntax highlighting and markdown rendering in `show` | RICH_INTEGRATION_PLAN §5 | STUB / unwired | G14 |
| V11 | Sync never touches `.git/`; `.beads/` allowlist; atomic publish; conflict-marker refusal; `--force` gating | README §Safety Model | WORKING | — |
| V12 | "No data loss" guarantee | README §Safety Model | REGRESSED in Aug (GH #457/#458/#460/#461), fixed in 0.5.5-0.5.7; #471/#474 fixed only at HEAD | G1, G2, G4, G27 |
| V13 | 3-way merge, reconcile, reconcile-additive with hash-bound plans, salvage, source-path migration | README §Troubleshooting | WORKING (#473 dry-run fix unreleased) | G2 |
| V14 | Local history backups with list/diff/restore/prune | README | WORKING | — |
| V15 | Workflow policy: ready groups, capacity, required fields, gates | README §Workflow Policy | WORKING (#466 gate_results fix unreleased) | G2 |
| V16 | Coordination status / stale-claim evidence | README FAQ, COORDINATION_EVIDENCE | WORKING | G5 |
| V17 | Cross-project routing, town discovery, external deps | README FAQ | WORKING | — |
| V18 | Doctor: diagnostics, repair sessions, schema migration plan/apply/undo, fixtures | README, docs | WORKING except `doctor explain` stub, `--bundle` absent | G13, G21 |
| V19 | MCP server: 7 tools, 12 resources, 4 prompts, same lock model | README, AGENTS.md | WORKING in code; protocol behavior UNPROVEN by tests | G11 |
| V20 | Startup < 100 ms cold / < 50 ms warm; "br faster than bd" | PROPOSED_ARCHITECTURE App. C, porting plan | UNPROVEN (benches ignored, self-comparing budget) | G10 |
| V21 | Regression budgets enforced in CI | ci.yml bench job | DISABLED | G3, G10 |
| V22 | Pluggable `Storage` trait; module decomposition | PROPOSED_ARCHITECTURE §1.1, §5.1 | NOT_STARTED / WRONG_APPROACH (one 38k-line file) | G15 |
| V23 | Write-combining queue; S3-FIFO cache | WRITE_COMBINING_QUEUE_DESIGN, `src/cache.rs` | DEFERRED, dormant code | G9 |
| V24 | Cross-platform single-binary releases, signed, checksummed, installer, package manifests | README §Installation | WORKING | G19, G23 |
| V25 | `cargo install --git ... --locked` works from crates.io deps | README | WORKING (0.5.7 on crates.io) | — |
| V26 | Self-update | README | WORKING | — |
| V27 | Property, fuzz, snapshot, failure-injection, concurrency testing | docs/TESTING_GUIDELINES, ci.yml | WORKING (but no gate runs them) | G3 |
| V28 | Zero unsafe code | AGENTS.md | PARTIAL by design (`deny` + 4 carve-outs) | G7 |
| V29 | Docs are accurate enough for agents to act on | AGENTS.md purpose | FAILING (README config keys, AGENTS.md structure, ARCHITECTURE.md claims) | G6, G7, G8 |
| V30 | Beads are the single source of truth for status | AGENTS.md | FAILING (5 stale claims; August work untracked) | G5 |
| V31 | Windows support | README; GH #438/#439/#413/#419 | PARTIAL (open beads txwk, gc8l; no Windows test shard) | G17, G3 |
| V32 | Broken-pipe safety for text output | GH #434, bead 3fna | PARTIAL | G17 |
| V33 | Acceptance-criteria as structured data | GH #477 | NOT_STARTED | G20 |
| V34 | Agent-first: mistakes get actionable hints | README §Design 4, error taxonomy | PARTIAL (README-shaped mistakes get "Issue not found") | G28 |
| V35 | "Does this binary work on this machine?" is answerable in one command | implied by Aug platform bug stream (#438, #439, #413, #419, #444) | NOT_STARTED | G26 |

### 1.3 Would completing all open beads close the gap?

**No.** The 16 open beads are 8 sync bugs, 6 storage/engine bugs (4 of them upstream FrankenSQLite escalations), and 3 test tasks. They cover parts of G4, G16 and G17 only. Nothing tracks G1, G2, G3, G5-G15, G18-G29. The 5 in_progress beads are all stale agent claims; one (`beads_rust-uri0`) is finished per its own comments.

---

## 2. Critical gaps (block the vision or violate a stated guarantee)

### Gap 1: Read-only authority inspection mutates database bytes — REGRESSED → WORKING

**Current state:** `SqliteStorage::inspect_pending_sync_merge_under_authority` (`src/storage/sqlite.rs:19771`) opens the database through `open_current_read_only` (`src/storage/sqlite.rs:2503`), which registers a `DatabaseOpenerLease` and calls `open_with_flags(..., SQLITE_OPEN_READ_ONLY)`, then reads `connection_user_version`, then closes. The unit test at `src/cli/commands/doctor.rs:14234` snapshots db/wal/shm/journal bytes before and after (`database_family_bytes`) and fails: the main-file header differs in the change-counter / version-valid-for region (bytes 24-27 and 92-95 of the SQLite header). This is GitHub #476, reproduced on `main` at fsqlite 0.3.13 and 0.3.14. The invariant matters because this path runs during recovery when br must not disturb a database it does not own.

**Target state:** the inspection leaves every database-family file byte-identical; the unit test passes on Linux and macOS; the root cause is known and either fixed in fsqlite or isolated in br; the invariant is pinned at the storage layer and monitored at runtime.

**Success criteria:**
- [ ] `cargo test --lib pending_sync_merge_authority_inspector_is_coherent` passes on Linux and macOS.
- [ ] New storage-level test `open_current_read_only_is_byte_identical` asserts identity on a fresh v17 database with (a) no WAL, (b) a live uncheckpointed WAL, (c) a stale `-shm`; it logs a hex diff of the first 100 header bytes on failure.
- [ ] A doctor check `db.read_only_open_is_observational` runs the probe on a temp copy of the family and reports the diff offsets when it fails.
- [ ] If the cause is upstream, a minimal repro is filed against frankensqlite and linked; the br mitigation is documented in `docs/reliability/ENGINE_OPERATING_MODEL.md` (Gap 4).

**Implementation plan:**
1. Localize the write with a three-point byte snapshot inside the test: after `open_with_flags`, after `connection_user_version`, after `conn.close()`. Log which step changes offsets 24-27 / 92-95 and whether `-wal`/`-shm` appear.
2. Check fsqlite 0.3.14 for an immutable/observational open mode; if present, use it here. If read-only open still writes the header, file upstream with the repro and the byte offsets.
3. Make the br path observational regardless of engine behavior: read `user_version` and the pending-receipt state from a **shadow copy** of the family (copy db + wal + shm to a `tempfile::tempdir()` under the held authority, open the copy, discard). The sync code already has a byte-snapshot pattern; reuse it. Cost is bounded by DB size and this path runs only during recovery.
4. Add the storage-level test and the doctor check; wire the doctor check into `report.json` with `details.finding_id = "db-read-only-open-not-observational"`.

**Tests and logging:** unit test with hex diffs; doctor check emits `tracing::warn!` with offsets and file suffix; e2e in `tests/e2e_doctor_chokepoint.rs` runs the check on a fixture with a live WAL.
**Dependencies:** none. Blocks Gap 2.
**Estimated complexity:** M
**Vision goals served:** V12, V18
**Bead coverage:** NONE. Create a bug bead; link GH #476.

### Gap 2: Six user-reported fixes exist only at HEAD; release 0.5.8 — PARTIAL → WORKING

**Current state:** Commits after the `v0.5.7` tag (`d2393c99`, `70e7fed9`, `d461a399`, `676e57bc`, `10ea8ece`, `087ce812`, `34ca862b`, `ebc34bd7`) fix GH #466 (`gate_results` never written), #467 (`br update` silently replacing non-empty text fields), #471 (`doctor --repair` discarding `events`, `gate_results`, `gate_result_history`, `close_metadata`, `capacity_*` tables), #473 (`--reconcile-additive --dry-run` unreachable), #474 (bypass-policy audit never exported), #475 (`br list --tree`), two cross-issue comment-ID collision bugs, and bump fsqlite to 0.3.14. Users on 0.5.7 have all of these, three of which are silent data-loss or silent-audit-loss class.

**Target state:** `v0.5.8` released with these fixes behind a real gate, a CHANGELOG entry naming each issue, and a post-release canary receipt.

**Success criteria:**
- [ ] Release asset `br --version` prints 0.5.8; `cargo install --git ... --locked` resolves 0.5.8; crates.io shows 0.5.8.
- [ ] Release gate (Gap 3) passed on the release commit, including `cargo test --lib` and the stress gate (Gap 4).
- [ ] CHANGELOG.md lists #466, #467, #471, #473, #474, #475, the comment-ID fixes, fsqlite 0.3.14.
- [ ] Post-release canary (Gap 26's lifecycle self-test) passes against the downloaded asset on linux_amd64, darwin_arm64, windows_amd64, and the receipt is attached to the release.

**Implementation plan:**
1. Land Gap 1, or quarantine #476 with `#[ignore = "GH-476 ..."]` plus the doctor runtime check so the invariant is still watched.
2. Bump `Cargo.toml`, README "Verify Installation", `.claude-plugin/plugin.json`, CHANGELOG; run the version audit (Gap 19).
3. Tag; let `release.yml` build; make the crates.io publish step idempotent (Gap 23).
4. Run the canary; attach the receipt to the release notes and the bead close reason.

**Tests and logging:** the canary itself; release notes include the gate receipt JSON.
**Dependencies:** Gap 1 (or quarantine), Gap 3, Gap 19, Gap 23.
**Estimated complexity:** S
**Vision goals served:** V12, V13, V15
**Bead coverage:** NONE. One release bead; the per-fix traceability is the CHANGELOG plus the GH issues.

### Gap 3: Quality gates are off, and no single gate definition exists — DISABLED → WORKING

**Current state:** `gh workflow list --all` shows CI, Security Audit, Conformance, Doctor, Full E2E & Benchmarks, Notify ACFS, and Update Package Manifests `disabled_manually`; only Release and Dependabot run. The last CI run on main (2026-08-19) failed at "Clippy (all features)" and "Check for yanked dependencies". `release.yml` "Release Reliability Gates" runs four targeted tests only. DSR skips lib tests (bead `9krz`). Coverage is `continue-on-error`. Locally, RCH caps kill `clippy --all-targets` at 5 minutes and `cargo test` at 30 minutes; a cold compile of 162 test binaries does not fit. Four places (ci.yml, release.yml, DSR, `scripts/ci-local.sh`) each hand-list what to run, and they have already drifted.

**Target state:** one gate manifest drives CI, release, DSR, and local runs; every push to main and every release runs fmt, clippy (all-features and no-default-features), `cargo test --lib`, sharded integration tests, doctor fixtures, and the stress gate, each inside GitHub and RCH time caps; results are visible and required.

**Success criteria:**
- [ ] `gates.toml` at repo root lists named gates (`fmt`, `clippy-all`, `clippy-min`, `check`, `lib`, `shard-e2e-1..n`, `shard-storage`, `shard-conformance`, `doctor-fixtures`, `stress`, `bench`, `version-audit`) with command, timeout, and required-for `{push, release, local}`.
- [ ] `scripts/gate.sh <name|all|release|push>` executes gates from the manifest; `ci.yml`, `release.yml`, DSR, and `ci-local.sh` all call `scripts/gate.sh` and contain no hand-written cargo test lists.
- [ ] `tests/gate_manifest.rs` asserts every workflow's cargo invocations come from the manifest and every `tests/*.rs` binary is assigned to exactly one shard.
- [ ] `gh workflow list --all` shows CI, Security Audit, Doctor, Conformance active; three consecutive main pushes are green.
- [ ] Each shard finishes in < 25 minutes from cold on an RCH worker; documented in AGENTS.md (Gap 25).
- [ ] Coverage job either enforces a threshold or is removed (Gap 22).

**Implementation plan:**
1. Diagnose the 2026-08-19 failures on a warm worker: run clippy all-features and `cargo audit --deny yanked` (install cargo-audit); fix lints per the existing allow-list policy; replace or bump the yanked crate.
2. Measure per-binary compile+run time on one warm worker (`cargo test --test X --no-run` timings) and partition `tests/*.rs` into shards of ≤ 20 minutes; write `gates.toml`.
3. Implement `scripts/gate.sh` (bash, reads the manifest with a tiny TOML parser or a checked-in generated shell fragment) and the manifest test.
4. Rewrite `ci.yml` jobs as a matrix over shards; add a `windows-latest` shard running `e2e_sync_artifacts`, `e2e_wrap`, `e2e_terminal_sanitization`, and the Windows auto-export scenario (Gap 17).
5. Extend `release.yml` gates with fmt, clippy, lib, stress (Gap 4), version audit (Gap 19), and the post-release canary job (Gap 26).
6. Re-enable workflows one at a time with `gh workflow enable`, watching each; add `scripts/check-workflows-enabled.sh` that fails if any expected workflow is disabled, and run it in the Security Audit job.
7. Record the policy in `docs/CI_SUPPLY_CHAIN.md`.

**Tests and logging:** `gate_manifest.rs`; each gate prints a one-line receipt `gate=<name> status=<pass|fail> elapsed=<s>` to stdout and to `artifacts/gates/<name>.json` for upload.
**Dependencies:** none. Blocks Gap 2, 4, 10, 11, 12, 16, 22, 25.
**Estimated complexity:** L
**Vision goals served:** V21, V27, V6, V31
**Bead coverage:** PARTIAL (`hrhx` doctor fixtures on RCH). Everything else uncovered.

### Gap 4: Storage-engine reliability is contained, not governed — PARTIAL → WORKING

**Current state:** In August, v0.5.3 on fsqlite 0.3.11 corrupted a healthy database family under concurrent multi-process writes (GH #457, #458, #460, #461). A stock-SQLite backend was built and reverted by the operator; containment landed as a sole-opener WAL checkpoint lease (`.br-db-openers-*.lock`) with stress receipts. Open beads `ro3m`, `f3r4`, `ajui`, `891u`, `avhq` are engine-boundary issues; Gap 1 is another. In_progress bead `uri0` is finished per its own comments and still open at P0. No document states the operating model or what must pass before an fsqlite bump; `scripts/br-stress.sh` exists but gates nothing.

**Target state:** the operating model is written; every engine-boundary bug has an upstream issue and a br regression test; fsqlite bumps and releases are gated by a stress harness and by the differential/linearizability harness of Gap 27; doctor reports engine state.

**Success criteria:**
- [ ] `docs/reliability/ENGINE_OPERATING_MODEL.md`: FrankenSQLite-only decision and why (no FFI, memory safety), containment mechanism, sidecar files (`-wal-cert`, `-wal-cert-head`, `-ns-gate`, `-ns-use`, `.fsqlite-migration-state`), recovery-artifact policy, the fsqlite-bump checklist, and the escalation template.
- [ ] `stress` gate in `gates.toml` runs `scripts/br-stress.sh` at N=8 for 60 s and 90 s, asserts zero recovery artifacts, `integrity_check` ok, rowid monotonicity on `issues`, and DB↔JSONL parity; required for release and for any PR touching `Cargo.toml` fsqlite lines.
- [ ] Beads `ro3m`, `f3r4`, `ajui`, `891u`, `avhq` each link an upstream issue and carry a `tests/repro_*.rs`.
- [ ] `br doctor --json` gains `engine: {name, version, sole_opener_lease, recovery_artifacts, sidecars}`.
- [ ] `uri0` closed with its recorded outcome.

**Implementation plan:**
1. Close `uri0` (outcome: containment at `dedfbed7`, stock-SQLite reverted at `a704e8b8`, releases 0.5.5-0.5.7, root cause upstream fsqlite #399).
2. Write the operating-model doc from the uri0 comment trail, `docs/fsqlite_trailing_pages_report.md`, and `docs/SWARM_SCALE_TUNING.md`.
3. Harden `scripts/br-stress.sh` into a gate with a JSON receipt; add the post-conditions above.
4. For each engine bead: minimal `tests/repro_*.rs` (ignored only with the upstream link in the reason), upstream issue, fix-version tracking.
5. Add the doctor engine block; surface it in `br info --json` too.

**Tests and logging:** stress receipt JSON with per-process op counts, error counts, artifact list; repro tests log the fsqlite version under test.
**Dependencies:** Gap 3 (gate wiring). Feeds Gap 27.
**Estimated complexity:** L (br side) plus upstream time
**Vision goals served:** V12, V2
**Bead coverage:** PARTIAL (`ro3m`, `f3r4`, `ajui`, `891u`, `avhq`, `uri0`). New beads: operating-model doc, stress gate, doctor engine block.

### Gap 5: The tracker no longer reflects the work — FAILING → WORKING

**Current state:** 270 commits and three releases landed between 2026-08-18 and 2026-09-01 against 12 closed beads; closed-per-month went 165 → 111 → 62 → 11. All five in_progress beads are stale agent claims by the AGENTS.md rule: `0v1.2.4` (36 days, the graph's only blocker), `3r45.1` and `3r45.2` (36 days), `mwxp` (6 days, bypassed by `9krz`/`891u`), `uri0` (4 days, finished). GH issues are fixed from commits without beads. Doctor on this repo's `.beads/` warns about a stale merge base and four retained recovery artifacts.

**Target state:** every in_progress bead has a live owner; every GH-issue fix references a bead; the backlog contains a bead for every gap in this plan; stale claims are surfaced automatically.

**Success criteria:**
- [ ] `br coordination status --json` reports zero stale claims.
- [ ] `uri0`, `9krz` closed with outcome-bearing reasons; `0v1.2.4` verified against `tests/e2e_sync_git_safety.rs` and closed or re-scoped; `3r45.1`, `3r45.2`, `mwxp` reclaimed with an audit comment or returned to open.
- [ ] Every gap here has at least one bead; `bv --robot-insights | jq .Cycles` is null; `bv --robot-triage` recommendations are not flat.
- [ ] AGENTS.md "Session Protocol" adds: a GH issue closed as fixed must cite a bead ID; `scripts/gate.sh stale-claims` prints `br coordination status` and fails on claims older than the AGENTS.md thresholds, run in the Doctor workflow on a schedule.
- [ ] This repo's `.beads/` doctor is green (Gap 24).

**Implementation plan:**
1. For each stale claim: `br show`, evidence comment, then close or `br update --status open --assignee ''`.
2. Generate beads from this plan (Phase 3a) with labels `reality-check-2026-09-01`, `wave-N`, and area labels.
3. Add the rule to AGENTS.md and the scheduled stale-claim gate.

**Tests and logging:** the stale-claim gate prints each stale claim with `updated_at`, assignee, idle hours.
**Dependencies:** none.
**Estimated complexity:** S
**Vision goals served:** V30, V16
**Bead coverage:** NONE.

### Gap 26: No one-command answer to "does this binary work here?" — NOT_STARTED → WORKING

**Current state:** The August issue stream was dominated by platform-specific breakage users discovered only in production: Windows panicking on every command (#438, #439), Windows auto-export authority mismatch (#413), `renameat2` EINVAL on DrvFS/9p (#419), GLIBC floor on Debian 12 (#444), stale `-wal-cert` wedging writes (#441). The reality check needed an 83-step ad-hoc script to establish that the shipped binary works. Nothing ships with br that exercises its own lifecycle on the user's filesystem.

**Target state:** `br doctor --selftest` (or `br selftest`) creates a throwaway workspace under the system temp dir (or `--dir`), runs the full lifecycle (init, create, deps, ready, claim, close, auto-flush, every sync mode, reconcile plan, history, doctor, routing to a second temp workspace, capacity policy), verifies each step, deletes nothing outside its temp dir, and prints a receipt with platform, filesystem type, rename-atomicity mode, engine version, and per-step timings. It is the post-release canary and the first thing an issue template asks for.

**Success criteria:**
- [ ] `br doctor --selftest --json` exits 0 on linux, macOS, Windows, WSL2/DrvFS and prints `{platform, fs_type, rename_mode, engine, steps: [{name, ok, ms}], ok}`.
- [ ] Runs in < 10 s; leaves no files outside its temp dir; never touches the caller's `.beads/`.
- [ ] `release.yml` post-release job downloads each asset and runs it; receipts attached to the release.
- [ ] `.github/ISSUE_TEMPLATE/bug_report.md` asks for the selftest receipt.
- [ ] `tests/e2e_selftest.rs` asserts the receipt schema and that a deliberately broken temp dir (read-only) yields a clear failing step rather than a panic.

**Implementation plan:** implement as a doctor subsystem (`doctor_subsystems/selftest.rs`) reusing the CLI handlers in-process where possible and spawning `current_exe()` for steps that need a fresh process (auto-flush, lock contention); promote the 2026-09-01 smoke script's step list into the Rust test table; expose the receipt via `br schema doctor-selftest`.

**Tests and logging:** each step logs `selftest.step name=<..> ok=<..> ms=<..>`; failures include the command, stdout tail, and stderr tail.
**Dependencies:** none for the command; Gap 3 for the release job.
**Estimated complexity:** M
**Vision goals served:** V35, V31, V24
**Bead coverage:** NONE.

---

## 3. Major gaps (significantly degrade the vision)

### Gap 6: README describes commands and config that do not exist — WRONG → WORKING

**Current state:**
- Config example (README ~606-628) uses `id.prefix`, `defaults.priority`, `defaults.type`, `defaults.assignee`, `output.color`, `output.date_format`; the code reads `issue_prefix`/`issue-prefix`/`prefix` (`src/config/mod.rs:5531`), `default_priority`/`default-priority` (`:6769`), `default_type`/`default-type` (`:6779`), `display.color`; no default-assignee or date-format key exists. `br config set` writes any key silently.
- `br label add <id> backend urgent` fails ("Issue not found: backend"): `parse_issues_and_label` (`src/cli/commands/label.rs:171-204`) takes only the last positional as the label; `--label` is `Option<String>` (`src/cli/mod.rs:2276`).
- `br list --priority 0-1` is rejected; `ListArgs.priority: Vec<String>` (`src/cli/mod.rs:1794`) accepts single values; ranges need `--priority-min/max`.
- "Verify Installation" prints `br 0.5.2`; "~5-8 MB" vs 26.5 MB; `install.sh --no-migration-skill` does not exist; `--robot` presented as global; Global Flags table omits 7 globals; Environment table lists 4 of ~25 variables; Commands tables omit `gate`, `capacity`, `scheduler`, `serve`, `list --tree`, nested `label rename`, `history list/diff/restore`, `query run/list/delete`, `config delete/path`, `audit *`, `doctor *`; `sync.auto_flush: false` example silently disables the documented default.

**Target state:** every README example runs as written; the code accepts the more ergonomic forms; tables are generated from `br capabilities` and checked.

**Success criteria:**
- [ ] `tests/e2e_readme_examples.rs` extracts every fenced bash block in README.md whose lines start with `br `, runs them in a scratch workspace with placeholder IDs substituted, and asserts the documented exit code; failures print the block's line number.
- [ ] `br label add <id> a b`, `br label add <id> -l a -l b`, `br label add <id> -l a,b` all add two labels; same for `remove`.
- [ ] `br list --priority 0-1`, `-p 0,1`, `-p P0-P1` work on `list`, `ready`, `blocked`, `count`.
- [ ] `br config set unknown.key=1` warns "unknown key; nearest: ..." and `br doctor` check `config.unknown_keys` warns; `br config schema --format json` emits the key registry.
- [ ] `scripts/generate-readme-tables.sh` regenerates Commands, Global Flags, Environment, Exit Codes tables from `br capabilities --format json`; `tests/readme_tables.rs` asserts README matches.
- [ ] Version, size, install flags, `--robot` scope, auto-flush example corrected.

**Implementation plan:**
1. `LabelAddArgs.label` → `Vec<String>` with `action = Append`, `value_delimiter = ','`; `parse_issues_and_label` resolves positionals via the existing ID resolver and treats unresolved trailing tokens as labels, with one clear ambiguity error; mirror for remove.
2. A shared `parse_priority_filter(&[String]) -> Result<BTreeSet<Priority>>` in `src/validation/mod.rs` accepting `N`, `PN`, `N-M`, `PN-PM`, comma lists; used by list/ready/blocked/count.
3. `KNOWN_CONFIG_KEYS` registry in `src/config/mod.rs` derived from the getters; `config set` warning; doctor check; `config schema` subcommand.
4. README rewrite of the affected sections; generator script and test.

**Tests and logging:** unit tests for the parsers; e2e for label/priority/config; README example runner logs each block.
**Dependencies:** Gap 3 (CI wiring). Pairs with Gap 28.
**Estimated complexity:** M
**Vision goals served:** V29, V7, V34
**Bead coverage:** NONE.

### Gap 7: AGENTS.md misdescribes the codebase agents work in — WRONG → WORKING

**Current state:** AGENTS.md claims `#![forbid(unsafe_code)]` (actual `deny` with carve-outs at `src/shutdown.rs:89,210`, `src/sync/db_inode_lock.rs:130,316`), fsqlite as "path dependencies" (actual crates.io 0.3.14, 15 crates), sizes 66 KB / 181 KB / 176 KB (actual 125 KB / 1.4 MB / 902 KB), a `src/storage/queries/` dir and `src/format/context.rs` that do not exist, a `Label` type that does not exist, a feature block missing `mcp`, an MCP resource list missing `beads://coordination/status`, a test table missing golden/workflow/bench/manifest/replay suites, a tree missing 13 modules, dispatch attributed to `cli/mod.rs` (actual `src/main.rs:585`), and an unqualified "Go parity" claim.

**Target state:** every structural claim is true and checked; volatile facts removed; a "Running tests under RCH" section exists.

**Success criteria:**
- [ ] `tests/agents_md_contract.rs` parses the Project Structure block and asserts every listed path exists and every `src/*.rs` and `src/*/` module is listed; asserts the Key Dependencies table names every non-dev crate in `Cargo.toml`.
- [ ] Unsafe policy paragraph names `deny` and the four carve-outs with issue numbers.
- [ ] Feature block lists `mcp`; MCP resource list complete; test table complete.
- [ ] Go-parity bullet qualified (needs real `bd`; hash bytes differ since v14).

**Implementation plan:** rewrite the Toolchain, Key Dependencies, Architecture, Project Structure, Key Files, Feature Flags, Core Types, MCP, and RCH sections from the code; add the contract test to the `lib` shard.
**Tests and logging:** the contract test prints each missing or extra path.
**Dependencies:** none.
**Estimated complexity:** S
**Vision goals served:** V29, V28
**Bead coverage:** NONE.

### Gap 8: Architecture and agent docs carry false claims — WRONG → WORKING

**Current state:** `docs/ARCHITECTURE.md`: ~33k LOC (actual ~241k), error keys `recovery_hints`/`kind`/`error_code` (actual `hint`/`code`), table `blocked_cache` (actual `blocked_issues_cache(issue_id, blocked_by, blocked_at)`), health words `drifted`/`quarantined` that do not exist. `docs/agent/AGENT_FRIENDLINESS_REPORT.md`: "no MCP surface in this repo". `AGENT_FRIENDLY_CHANGELOG.md`: one entry in eight months. `RICH_INTEGRATION_PLAN.md`: checklist 100% unchecked. `E2E_COVERAGE_MATRIX.md`: dated 2026-05-08, missing six commands. `HEALTH_CONTRACT.md`: 22 anomaly classes vs 25 in `src/health.rs`. `src/output/mod.rs:6-14` doc comment omits the env step.

**Target state:** each document is correct or marked historical with a pointer to the current source of truth; the examples are captured from real runs.

**Success criteria:**
- [ ] ARCHITECTURE.md as-built section (see Gap 15) with `tokei` output and date; error envelope copied from `br show nope --json`; table names from `schema.rs`; health vocabulary from `health.rs`; `tests/docs_examples.rs` re-runs the captured commands and diffs the JSON keys.
- [ ] AGENT_FRIENDLINESS_REPORT.md and AGENT_FRIENDLY_CHANGELOG.md updated for capabilities, coordination, gate, capacity, scheduler, serve, `list --tree`, update overwrite guard.
- [ ] RICH_INTEGRATION_PLAN.md ticked with a Deferred section; E2E_COVERAGE_MATRIX.md regenerated by `scripts/generate-e2e-matrix.sh` from `tests/` and `br capabilities`; HEALTH_CONTRACT.md lists all classes; `output/mod.rs` doc fixed.
- [ ] `docs/README.md` index marks `porting/` and `plans/` historical unless dated within 90 days.

**Dependencies:** none.
**Estimated complexity:** M
**Vision goals served:** V29
**Bead coverage:** NONE.

### Gap 9: Designed-but-unwired modules (~5,000 lines) — DEFERRED → decided

**Current state:** `src/write_combining.rs` (2,910 lines) referenced only by `tests/bench_contention_replay.rs`; `src/cache.rs` (641) zero references; `src/format/rich.rs`, `format/theme.rs`, `format/syntax.rs` (stub), `format/markdown.rs` (`render_rich_markdown`, real, uncalled), `output/components/{dep_tree,progress,stats}.rs`, `OutputContext::error_panel` have zero production callers. `lib.rs` exports `cache` and `write_combining`. Stale `#[allow(dead_code)] // WP1 scaffold` markers sit on live types (`doctor.rs:258,345`, `sync.rs:557`, `close.rs:309`).

**Target state:** each module is WIRE, REMOVE (needs written approval), or KEEP-DORMANT with reason, owner, revisit trigger; nothing is silently dead; a test fails if a `pub mod` has no non-test caller and no dormant marker.

**Success criteria:**
- [ ] Decision table in ARCHITECTURE.md.
- [ ] KEEP-DORMANT modules start with `//! Status: dormant — <reason>; revisit when <trigger>`; `tests/dormant_modules.rs` asserts each `pub mod` in `lib.rs` is either referenced from `src/` or carries the marker.
- [ ] WIRE modules have a caller and an e2e/golden test; REMOVE modules deleted only after approval with the design doc archived under `docs/plans/archive/`.
- [ ] Stale `#[allow(dead_code)]` markers removed.

**Implementation plan:** recommended decisions — WIRE `format/markdown.rs` into `br show` (Gap 14); WIRE or REMOVE `components/dep_tree.rs`; KEEP-DORMANT `write_combining.rs` with trigger "when `bench_contention_replay` p95 lock wait > 250 ms at N=8"; REMOVE candidates `format/rich.rs`, `format/theme.rs`, `cache.rs` pending approval; `format/syntax.rs` per Gap 14.
**Dependencies:** Gap 14.
**Estimated complexity:** M
**Vision goals served:** V23, V10, V29
**Bead coverage:** NONE.

### Gap 10: Performance promises are unproven — UNPROVEN → WORKING

**Current state:** Targets (< 100 ms cold, < 50 ms warm; "br faster than bd") come from PROPOSED_ARCHITECTURE Appendix C and the porting plan. `tests/bench_cold_warm_start.rs` benches are `#[ignore]`d (1126-1310), self-skip without `bd`, swallow errors; the "enforcing" policy (`:854-861`) hardcodes `p95_delta_pct: Some(0.0)`. `tests/benchmark_comparison.rs` asserts no ordering. CI bench job disabled. Real warm numbers are ~10 ms on 977 issues. README claims a binary size that is wrong by 3x and nothing gates size.

**Target state:** latency, and binary size are asserted with explicit thresholds; regression budgets compare to a committed baseline; bd comparison is real when `bd` is present.

**Success criteria:**
- [ ] `tests/perf_latency_contract.rs` (not ignored): release binary, 1k and 10k synthetic datasets from `bench_synthetic_scale.rs`, p95 of `ready --json`, `list --json --limit 0`, `show --json`, `create --json` warm < 50 ms and cold < 100 ms; thresholds in one constants block; measured numbers printed and written to `artifacts/perf/latency.json`.
- [ ] `bench_cold_warm_start` computes `p95_delta_pct` against a committed `tests/artifacts/baseline/perf-evidence-manifest.json`.
- [ ] `gates.toml` `size` gate: stripped release binary ≤ 30 MB on linux_amd64 (current 26.5 MB) with the number recorded in the receipt; README states the measured size and date.
- [ ] CI bench job re-enabled and green; `benchmark_comparison.rs` emits ratios, fails only under `BR_BENCH_STRICT=1`.

**Dependencies:** Gap 3.
**Estimated complexity:** M
**Vision goals served:** V20, V21
**Bead coverage:** NONE.

### Gap 11: MCP protocol behavior is unproven — UNPROVEN → WORKING

**Current state:** `src/mcp/` implements 7 tools, 12 resources, 4 prompts with 80 unit tests; `run_serve` (`src/mcp/mod.rs:1191`) drives `StdioTransport::stdio()`. Only `tests/e2e_mcp_shutdown.rs` exists and it never exchanges JSON-RPC. `mcp` is a non-default feature.

**Target state:** a stdio JSON-RPC e2e proves the handshake, listings, one read tool, one mutating tool with a database check, resource templates, and error envelopes; the MCP feature is built and tested in CI; the build decision is recorded.

**Success criteria:**
- [ ] `tests/e2e_mcp_protocol.rs` (`#![cfg(feature = "mcp")]`): spawn `br serve --actor test`; send `initialize`, `notifications/initialized`, `tools/list`, `resources/list`, `resources/templates/list`, `prompts/list`, `tools/call create_issue`, `tools/call list_issues`, `resources/read beads://issues/{id}`, `tools/call show_issue` with a hallucinated ID (expects the placeholder-ID error); assert responses against `br schema` documents; verify via `br show --json` and `br audit log` that the issue exists and the event actor is `test`.
- [ ] CI `--all-features` shard runs it; `e2e_mcp_shutdown` retained.
- [ ] `br capabilities --format json` reports `features.mcp`.
- [ ] README records the `mcp`-in-default decision with the measured size delta.

**Dependencies:** Gap 3.
**Estimated complexity:** M
**Vision goals served:** V19
**Bead coverage:** NONE.

### Gap 12: Go-bd conformance is not exercised — UNPROVEN → WORKING

**Current state:** `tests/conformance*.rs` (~228 tests) compare against a real Go `bd` via `BD_BINARY`/PATH and skip when absent; `conformance.yml` builds bd v0.46.0 weekly but is disabled and had failed since 2026-08-03 at "Run conformance tests".

**Target state:** conformance runs weekly and on demand; failures are triaged into documented intentional divergences or bugs; docs state the parity boundary.

**Success criteria:**
- [ ] `conformance.yml` enabled and green, or each failure annotated in `docs/CONFORMANCE_DIVERGENCES.md` with the reason and the test name.
- [ ] `scripts/conformance.sh --fetch-bd` obtains the pinned bd locally.
- [ ] AGENTS.md/README parity statement qualified.

**Dependencies:** Gap 3.
**Estimated complexity:** M
**Vision goals served:** V6
**Bead coverage:** NONE.

### Gap 13: `br doctor explain` is a stub and `doctor capabilities --command` is ignored — STUB → WORKING

**Current state:** `execute_explain` (`surface.rs:1951-1988`) echoes a fixed "WP6 stub" envelope. Doctor checks already carry `details.finding_id` and each run writes `report.json` (`doctor_subsystems/run_dir.rs`). `--command` is bound to `_filter` and unused (`surface.rs:146`).

**Target state:** `br doctor explain <finding-id>` returns evidence, affected paths, remediation, and last observed status from the latest run or a targeted fresh run; `--command` filters or is removed.

**Success criteria:**
- [ ] `br doctor explain fm-state_files-merge-artifact-stuck --json` returns `{finding_id, check_name, status, observed_at, evidence: [...], paths: [...], remediation: {command, dry_run_command, docs_url}}`.
- [ ] Unknown ids exit 4 with nearest ids; `br doctor explain --list` enumerates finding ids.
- [ ] `--command` filters capabilities or is deleted with help updated.
- [ ] e2e in `tests/e2e_doctor_chokepoint.rs`.

**Dependencies:** none. Feeds Gap 21.
**Estimated complexity:** M
**Vision goals served:** V18
**Bead coverage:** NONE.

### Gap 14: Rich-output plan items never closed — STUB → decided/WORKING

**Current state:** Syntax highlighting stub (`src/format/syntax.rs:88`), deferred only in a Cargo.toml comment; markdown renderer uncalled; update-diff display, `--ascii`/`TERM=dumb`/`COLORTERM`, and theme variants planned and unbuilt.

**Target state:** a recorded decision per item; shipped items wired and golden-tested.

**Success criteria:**
- [ ] RICH_INTEGRATION_PLAN.md Deferred section with reason and revisit trigger per item.
- [ ] Markdown rendering: `br show` renders description/design/notes/acceptance as markdown in Rich mode, plain in Plain; goldens in `tests/golden_rich_panels.rs`.
- [ ] Accessibility: `TERM=dumb` or `--ascii` yields box-drawing-free output; test in `tests/e2e_global_flags.rs`.
- [ ] Highlighting: adopt a maintained highlighter with a measured size delta, or remove the module (approval) and stop implying it.

**Dependencies:** Gap 9.
**Estimated complexity:** M
**Vision goals served:** V10, V9
**Bead coverage:** NONE.

### Gap 15: Architecture drifted from the plan without amending the plan — WRONG_APPROACH → decided

**Current state:** PROPOSED_ARCHITECTURE specified a `Storage` trait and a module split; reality is 267 public methods on `SqliteStorage`, `sqlite.rs` 38,031 lines, `doctor.rs` 25.6k, `sync/mod.rs` 24k, `config/mod.rs` 12.4k. Plan commands `duplicates`, `edit`, `compact`, `cleanup`, `--robot-help` were not built.

**Target state:** plan docs say what was built and why; if the operator wants decomposition, it is done isomorphically with proof.

**Success criteria:**
- [ ] ARCHITECTURE.md "As built (2026-09)" section: module map with line counts, why no trait, why the monoliths, intentionally-unbuilt commands.
- [ ] `docs/porting/*.md` and RICH plan carry a historical banner.
- [ ] Optional: isomorphic split of `sqlite.rs` into `storage/{issues,deps,labels,search,events,recovery}.rs` with byte-identical CLI goldens and public-API diff proof.

**Dependencies:** Gap 8; Gap 3 for any split.
**Estimated complexity:** S (docs) / XL (split)
**Vision goals served:** V22, V29
**Bead coverage:** NONE.

### Gap 16: E2E coverage holes and a weak acceptance guard — PARTIAL → WORKING

**Current state:** no e2e for `br agents`; `info --schema` untested; `delete --cascade` no execution scenario; GH384 guard (`tests/e2e_workflow_capacity_scopes.rs:272`) checks names only; nine ignored sync tests (`mzpz`); 14 doctor fixtures need `sqlite3` on RCH (`hrhx`).

**Target state:** every top-level command has an e2e file; the acceptance guard checks behavior; no ignored test lacks an issue link.

**Success criteria:**
- [ ] `tests/e2e_agents.rs`: `--add`, `--force`, idempotency, refusal outside a repo, only the requested file changes.
- [ ] `info --schema` and `delete --cascade` scenarios added.
- [ ] GH384 guard replaced by criterion → test → asserted-expression table verified by running each test.
- [ ] `mzpz` tests un-ignored or moved; `hrhx` fixtures rewritten without `sqlite3`.
- [ ] `tests/ignored_tests_have_links.rs` asserts every `#[ignore = "..."]` reason contains `GH-` or `beads_rust-`.

**Dependencies:** Gap 3.
**Estimated complexity:** M
**Vision goals served:** V27
**Bead coverage:** PARTIAL (`mzpz`, `hrhx`).

### Gap 17: Existing open sync and portability beads — PARTIAL → WORKING

**Current state:** `5a05` (export_hashes upsert lost, GH #435), `gc8l` (renameat2 fallback, GH #419), `txwk` (Windows auto-export, GH #413), `3fna` (text SIGABRT on closed pipe, GH #434), `zoqe` (vcs-status worktree config), `cnz8` (minisig rotation doc, GH #411), `3r45.3`, `3r45.4`, and abandoned `3r45.1`, `3r45.2`, `0v1.2.4`.

**Target state:** each owned, re-verified against current main, closed with receipts; Windows items covered by the Windows shard.

**Success criteria:**
- [ ] Per bead: comment "still reproduces at <sha>" or "fixed by <sha>", with commands.
- [ ] `3fna`: `br list | head -1` exits 0 in text mode (`tests/e2e_broken_pipe.rs`).
- [ ] `0v1.2.4`: closed against `e2e_sync_git_safety.rs` evidence.
- [ ] Windows shard runs the auto-export and `renameat2` fallback scenarios.

**Dependencies:** Gap 3.
**Estimated complexity:** L in aggregate
**Vision goals served:** V11, V12, V31, V32
**Bead coverage:** YES (existing); needs ownership and re-verification.

### Gap 27: No engine-independent oracle for storage correctness — NOT_STARTED → WORKING

**Current state:** The August corruption was found by users, not by tests, because every br test trusts the engine it runs on: failure injection replays crashes, stress runs count artifacts, but nothing checks that the observable state after a sequence of operations equals what a simple reference model says it should be, and nothing checks that concurrent multi-process histories are linearizable with respect to br's own semantics.

**Target state:** two harnesses that would have caught #457/#458/#460/#461/#426 before release: a model-based differential test and a concurrent-history checker, both in the `stress` and `storage` gates.

**Success criteria:**
- [ ] `tests/model_based_storage.rs`: proptest generates sequences of `create/update/close/reopen/dep add/dep remove/label add/remove/comment add/delete` against `SqliteStorage` and against a `BTreeMap`-backed reference model (`tests/common/model.rs`); after every step the projections (`list`, `ready`, `blocked`, `show`, dependency closure, labels, comments) match; on failure it prints the minimal shrunken sequence and both states.
- [ ] `tests/linearizability_multiprocess.rs`: N=8 processes run randomized command mixes against one workspace, each op recorded with `(pid, invoke_ts, return_ts, op, result)` to an append-only history file; a checker (`tests/common/linearizability.rs`, simple WGL/P-compositionality over per-issue histories) verifies every issue's history against the sequential model; DB `integrity_check`, rowid monotonicity, and DB↔JSONL parity are asserted at the end.
- [ ] Both run in `gates.toml` (`storage` shard for the model test at 500 cases; `stress` gate for the history checker) and are required for fsqlite bumps.
- [ ] The rowid-corruption scenario of GH #426 (264 sequential dep-removes) is a fixed seed in the model test.

**Implementation plan:** the reference model mirrors `Issue` fields relevant to projections; per-issue histories keep the checker tractable; reuse the process harness from `tests/e2e_concurrency.rs`; artifacts uploaded on failure.
**Tests and logging:** shrunken sequences and history files are the logging; each op logs pid, latency, and error code.
**Dependencies:** Gap 3, Gap 4.
**Estimated complexity:** L
**Vision goals served:** V12, V27
**Bead coverage:** NONE.

### Gap 28: README-shaped mistakes get unhelpful errors — PARTIAL → WORKING

**Current state:** `br label add smk-asq backend urgent` → "Issue not found: backend"; `br list --priority 0-1` → "Priority must be 0-4"; `br config set id.prefix=x` → silent success; `br label add -l a -l b` → clap "cannot be used multiple times". Each is a plausible agent move and none of the errors say what to do instead. The error taxonomy already supports hints (`src/error/structured.rs:290`, `find_similar_ids` at `:1257`).

**Target state:** every mistake an agent is likely to make from reading the docs yields a hint naming the correct form; hints are asserted by tests.

**Success criteria:**
- [ ] `label add`/`remove`: an unresolved trailing token that does not look like an ID yields `hint: "did you mean -l <token>? labels are passed with -l or after all issue IDs"` (until Gap 6 makes it work outright).
- [ ] Priority parser errors name the accepted forms (`0-4`, `P0-P4`, `N-M`, comma lists, `--priority-min/max`).
- [ ] `config set` unknown key: hint with nearest known keys.
- [ ] `sync` without a mode: already good; add the same shape to `dep add` with a missing type and to `update` when a text field would be overwritten (`--force` hint exists since #467; assert it).
- [ ] `tests/e2e_errors.rs` gains a "docs-shaped mistakes" section asserting each hint string.

**Dependencies:** none; overlaps Gap 6.
**Estimated complexity:** S
**Vision goals served:** V34, V7
**Bead coverage:** NONE.

### Gap 29: Docs, capabilities, and schemas are not one source of truth — PARTIAL → WORKING

**Current state:** `br capabilities --format json` (46 commands, global flags, env vars, exit codes) and `br schema all` already describe the surface, but README tables, AGENTS.md lists, `docs/CLI_REFERENCE.md` (80 KB), `docs/E2E_COVERAGE_MATRIX.md`, and `agent_baseline/` are hand-maintained copies that have drifted. `scripts/verify-agent-contracts.sh` checks the baseline artifacts but not the prose docs.

**Target state:** generated sections in README, AGENTS.md, and CLI_REFERENCE.md are fenced with `<!-- generated:begin name -->` markers and regenerated by one script; a test fails when they drift; `agent_baseline/` is regenerated the same way.

**Success criteria:**
- [ ] `scripts/generate-docs.sh` regenerates every fenced section from `br capabilities`, `br schema`, and `br --help` trees; `tests/generated_docs_in_sync.rs` runs it into a temp copy and diffs.
- [ ] CLI_REFERENCE.md per-command sections are generated from clap help plus a hand-written "notes" block that survives regeneration.
- [ ] `scripts/verify-agent-contracts.sh` calls the generator check.

**Dependencies:** Gap 6, Gap 7.
**Estimated complexity:** M
**Vision goals served:** V29, V7
**Bead coverage:** NONE.

---

## 4. Minor gaps (polish and completeness)

### Gap 18: Repo bloat and tracked junk — WRONG → WORKING

**Current state:** `sample_beads_db_files/` (178 MB of SQLite databases from nine other projects), `custom.db` (root), `temp_test/.beads/daemon.log` and siblings, `refactor/artifacts/` (63 files) are tracked; `.DS_Store` files and an empty `=` file sit untracked. Cargo `exclude` already omits them from the crate.

**Target state:** fixtures live outside the main history or in git-lfs; junk is gone; `.gitignore` prevents recurrence. **Deletion requires explicit written operator approval (AGENTS.md Rule 1).**

**Success criteria:** decision recorded; if approved, index cleaned and clone size stated; tests referencing `custom.db` use `tests/fixtures/`.
**Dependencies:** none. **Complexity:** S. **Vision goals served:** V29. **Bead coverage:** NONE.

### Gap 19: Version and metadata consistency — STALE → WORKING

**Current state:** README says 0.5.2; `.claude-plugin/plugin.json` says 0.5.2; the "Version Audit" CI job is disabled.

**Target state:** one version source; the release gate fails on drift.

**Success criteria:** `version-audit` gate (in `gates.toml`) asserts README, plugin.json, packaging manifests, CHANGELOG top entry, and `agent_baseline/` versions match `Cargo.toml`; runs before release builds.
**Dependencies:** Gap 3. **Complexity:** S. **Vision goals served:** V24. **Bead coverage:** NONE.

### Gap 20: Acceptance criteria are an opaque string (GH #477) — NOT_STARTED → WORKING

**Current state:** free markdown; `--json` returns one string; the #467 guard makes ticking a box require `--force`.

**Target state:** per-item read/write.

**Success criteria:**
- [ ] `br show --json` exposes `acceptance_items: [{index, text, checked}]` parsed from `- [ ]`/`- [x]` lines.
- [ ] `br update <id> --check-acceptance 2,4` / `--uncheck-acceptance 3` / `--add-acceptance "text"` mutate only those lines, bypass the whole-field guard, record an event.
- [ ] Round-trip property test: parse → serialize is byte-identical for untouched lines; unchecked-box rule in `required_fields` (`close_policy.rs`) uses the same parser.

**Dependencies:** none. **Complexity:** M. **Vision goals served:** V33, V7. **Bead coverage:** NONE (GH #477 open).

### Gap 21: `br doctor --bundle` promised but absent — NOT_STARTED → WORKING

**Current state:** `docs/reliability/HEALTH_CONTRACT.md` says "not yet implemented".

**Target state:** `br doctor --bundle <path.tar.gz>` writes report.json, health, sidecar listing, lock states, last N events, redacted config, selftest receipt; documented in TROUBLESHOOTING.md and requested by the issue template.
**Dependencies:** Gap 13, Gap 26. **Complexity:** M. **Vision goals served:** V18. **Bead coverage:** NONE.

### Gap 22: Coverage gate is theater — PARTIAL → decided

**Current state:** `cargo llvm-cov` with `continue-on-error: true`.
**Target state:** a real threshold (no decrease > 0.5 pp vs main) or removal.
**Dependencies:** Gap 3. **Complexity:** S. **Bead coverage:** NONE.

### Gap 23: Release pipeline duplicate assets and idempotency — PARTIAL → WORKING

**Current state:** v0.5.7 carries `br-0.5.7-*` (release.yml, signed) and `beads_rust-0.5.7-*` (second uploader, unsigned, different sizes); the second v0.5.7 run failed at "Publish to crates.io" because the version existed.

**Target state:** one signed asset family; idempotent re-runs.

**Success criteria:** the `beads_rust-*` uploader identified and stopped or aligned; publish step checks crates.io first; INSTALLING.md states canonical names; `tests/e2e_installer.rs` asserts the installer's expected names match release.yml.
**Dependencies:** none. **Complexity:** S. **Vision goals served:** V24. **Bead coverage:** NONE.

### Gap 24: This repository's own tracker hygiene — WARN → WORKING

**Current state:** `br doctor` warns `base_jsonl` older than `issues.jsonl`; four recovery artifacts from 2026-08-20; two foreign `recovery_*` directories (25 MB).
**Target state:** doctor green on the project's own workspace; artifacts triaged with a note or removed with approval.
**Dependencies:** none. **Complexity:** S. **Bead coverage:** NONE.

### Gap 25: Developer experience under RCH — PARTIAL → WORKING

**Current state:** RCH caps (5 min clippy, 30 min test) are undocumented; agents lose an hour discovering them.
**Target state:** AGENTS.md RCH section lists caps and shard commands; `scripts/gate.sh` is the documented entry point.
**Dependencies:** Gap 3. **Complexity:** S. **Bead coverage:** NONE.

---

## 5. Prioritized execution order and swarm tracks

| Wave | Gaps | Rationale |
|---|---|---|
| 0 (today) | 5, 24 | Zero-risk; restores tracker truth before new beads land |
| 1 | 1, 3 | Unblocks a trustworthy release and every proof gap |
| 2 | 2, 19, 23, 26 | Ship 0.5.8 with a real gate, consistent metadata, and a canary |
| 3 | 6, 7, 8, 25, 28, 29 | Stop agents acting on false docs; make docs generated |
| 4 | 4, 17, 27 | Engine governance, existing beads re-verified, engine oracle |
| 5 | 10, 11, 12, 16, 22 | Proof gaps: perf, MCP, conformance, e2e holes, coverage |
| 6 | 9, 13, 14, 20, 21 | Finish or retire partials; small features |
| 7 | 15, 18 | Architecture decision and repo cleanup (operator approval) |

Parallel tracks for a swarm (no file overlap between tracks):

| Track | Gaps | Primary files |
|---|---|---|
| A: release and gates | 3, 19, 23, 25, 2 | `.github/workflows/*`, `gates.toml`, `scripts/gate.sh`, `scripts/ci-local.sh`, CHANGELOG |
| B: storage and engine | 1, 4, 27 | `src/storage/sqlite.rs` (read-only open), `src/cli/commands/doctor.rs` (engine block), `scripts/br-stress.sh`, `tests/model_based_storage.rs`, `tests/linearizability_multiprocess.rs`, `docs/reliability/` |
| C: docs as data | 6 (docs half), 7, 8, 29 | README.md, AGENTS.md, docs/, `scripts/generate-docs.sh`, `tests/agents_md_contract.rs`, `tests/generated_docs_in_sync.rs` |
| D: CLI ergonomics | 6 (code half), 28, 20 | `src/cli/mod.rs`, `src/cli/commands/{label,list,update,config}.rs`, `src/validation/mod.rs`, `src/config/mod.rs`, `tests/e2e_errors.rs` |
| E: doctor | 13, 21, 26 | `src/cli/commands/doctor_subsystems/{surface,selftest}.rs`, `tests/e2e_doctor_chokepoint.rs`, `tests/e2e_selftest.rs` |
| F: proof suites | 10, 11, 12, 16 | `tests/perf_latency_contract.rs`, `tests/e2e_mcp_protocol.rs`, `tests/e2e_agents.rs`, conformance |
| G: output layer | 9, 14, 15 | `src/format/*`, `src/output/*`, `src/cli/commands/show.rs`, ARCHITECTURE.md |
| H: tracker and repo | 5, 17, 18, 24 | `.beads/`, existing beads, repo index (approval-gated) |

## 6. Dependency graph

```mermaid
graph TD
  G5[Gap 5 tracker hygiene] --> G3
  G1[Gap 1 read-only byte identity] --> G2[Gap 2 release 0.5.8]
  G3[Gap 3 gate manifest and CI] --> G2
  G19[Gap 19 version audit] --> G2
  G23[Gap 23 release idempotency] --> G2
  G26[Gap 26 selftest canary] --> G2
  G3 --> G4[Gap 4 engine model]
  G4 --> G27[Gap 27 engine oracle]
  G4 --> G17[Gap 17 existing beads]
  G3 --> G10[Gap 10 perf proof]
  G3 --> G11[Gap 11 MCP e2e]
  G3 --> G12[Gap 12 conformance]
  G3 --> G16[Gap 16 e2e holes]
  G3 --> G22[Gap 22 coverage]
  G3 --> G25[Gap 25 RCH docs]
  G6[Gap 6 README + parsers] --> G29[Gap 29 generated docs]
  G7[Gap 7 AGENTS.md] --> G29
  G7 --> G8[Gap 8 arch docs]
  G8 --> G15[Gap 15 as-built architecture]
  G14[Gap 14 rich decisions] --> G9[Gap 9 dormant modules]
  G13[Gap 13 doctor explain] --> G21[Gap 21 doctor bundle]
  G26 --> G21
  G6 --> G28[Gap 28 hints]
```

## 7. Verification plan (after all bridge work)

- [ ] V1-V4, V7-V9, V11, V13-V17, V35: `br doctor --selftest --json` passes on the released asset for linux_amd64, darwin_arm64, windows_amd64; receipts attached to the release.
- [ ] V6: conformance workflow green or divergences documented.
- [ ] V10: goldens for markdown rendering; highlighting decision recorded.
- [ ] V12: `cargo test --lib` green including byte-identity; stress gate and Gap 27 harnesses green on the release commit.
- [ ] V18: `br doctor explain <id>` returns evidence; `--bundle` produces a tarball.
- [ ] V19: `e2e_mcp_protocol` green in the `--all-features` shard.
- [ ] V20-V21: latency contract green; size gate green; bench job green with a committed baseline.
- [ ] V22, V23: ARCHITECTURE.md as-built and dormant-module table exist; `dormant_modules.rs` green.
- [ ] V24: single signed asset family; version audit green.
- [ ] V27: all CI workflows enabled and green for three consecutive main pushes; `gate_manifest.rs` green.
- [ ] V28, V29: `agents_md_contract.rs`, `e2e_readme_examples.rs`, `generated_docs_in_sync.rs` green.
- [ ] V30: `br coordination status --json` shows zero stale claims; every gap has a closed bead with a receipt.
- [ ] V31-V34: Windows shard green; broken-pipe test green; acceptance-item API tests green; docs-shaped-mistake hints asserted.

## 8. Explicit decisions for the operator

1. Approve or decline deletion of dormant modules (`format/rich.rs`, `format/theme.rs`, `cache.rs`, possibly `format/syntax.rs`).
2. Approve or decline moving `sample_beads_db_files/` and removing `custom.db`, `temp_test*/`, stale `refactor/artifacts/` from the index.
3. Decide whether `mcp` joins the default feature set once the size delta is measured.
4. Decide whether the `Storage` trait / monolith split is wanted or the plan is amended to "as built".
5. Decide whether the coverage job gets a threshold or is removed.

## 9. Revision log

- **r1 (2026-09-01):** initial 25-gap plan from the reality check.
- **r2 (ambition round 1):** added tests-and-logging lines to every gap; replaced hand-listed CI steps with a gate manifest and `scripts/gate.sh` (Gap 3); added the Windows test shard; added the binary-size gate and latency contract details (Gap 10); added `br doctor --selftest` as post-release canary (Gap 26); added docs-shaped-mistake hints (Gap 28); added swarm tracks (§5).
- **r3 (ambition round 2):** added the engine-independent oracle (Gap 27: model-based differential test plus multi-process linearizability checker) as the mechanism that would have caught the August corruption class; added docs-as-data generation with drift tests (Gap 29); added `dormant_modules.rs` and `ignored_tests_have_links.rs` structural tests; added `config schema`; added the explicit operator-decision list (§8); tightened dependency graph.
