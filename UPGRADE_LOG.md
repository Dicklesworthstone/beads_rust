# Dependency Upgrade Log

## In progress: 2026-09-11 (beads_rust-4e2n1)

- Registry inventory checked against `Cargo.lock`: all direct dependencies already resolve to latest stable except `fastmcp-rust` 0.8.1 → 0.9.0, `asupersync` 0.4.9 → 0.4.11, and `toml` 1.1.4 → 1.1.6. FrankenSQLite remains current at 0.3.18; no engine bump is needed.
- Published FastMCP 0.9.0 requires `asupersync =0.4.10` and its client requires `toml =1.1.5`. These three manifest changes are one indivisible compatibility update: changing any one independently makes Cargo resolution fail. Newer asupersync 0.4.11 and TOML 1.1.6 remain excluded by these upstream exact pins.
- Research: [FastMCP published dependencies](https://crates.io/api/v1/crates/fastmcp-rust/0.9.0/dependencies), [client dependencies](https://crates.io/api/v1/crates/fastmcp-client/0.9.0/dependencies), [v0.9.0 changelog](https://github.com/Dicklesworthstone/fastmcp_rust/blob/v0.9.0/CHANGELOG.md). Stable tag `fd440f3d361a58e5578c02f306e0dcf80cf8e479` preserves the caller-owned-context transport API br uses. Removed context-free stdio and synchronous HTTP APIs are not used here; feature selection remains unchanged. Runtime `current_thread`, `block_on`, and `request_cx_with_budget` bodies are unchanged in [asupersync 0.4.10](https://github.com/Dicklesworthstone/asupersync/blob/v0.4.10/src/runtime/builder.rs); scheduler changes still require storage regression coverage. [TOML 1.1.5](https://github.com/toml-rs/toml/blob/toml-v1.1.6/crates/toml/CHANGELOG.md) fixes owned integer/float conversion.
- Baseline: `a22c251b`; `cargo test --locked --lib --all-features` through RCH on ovh-a passed 3,138 tests, with zero failures and nine existing ignores (2026-09-12 00:51 UTC). Remote process and outer command exited zero. Controller disk exhaustion prevented durable lease receipt updates; the remote test result remains observed, but no persisted lease receipt is claimed.
- Resolver updated the eight FastMCP crates, asupersync, TOML and `syn` 3.0.3 → 3.0.5. [FastMCP derive](https://crates.io/api/v1/crates/fastmcp-derive/0.9.0/dependencies) pins syn exactly; its patch changes foreign `safe fn` parsing and lexical-error spans. Cargo also reselected already-present compatible edges: gix-imara-diff's hashbrown 0.17.1 → 0.16.1 (`>=0.15, <=0.17`) and tempfile's getrandom 0.4.3 → 0.3.4 (`>=0.3.0, <0.5`). These edge changes are included in candidate qualification.
- Security audit against freshly cloned RustSec revision `b50980aad8b8f14f77e25a97b32dd94bf008b0af` reports zero vulnerabilities across 633 locked packages, including a second run with no project exclusions. That second run reports only the existing unmaintained `rustls-pemfile` notice (RUSTSEC-2025-0134). Its documented exception remains; removed the obsolete gix-date RUSTSEC-2025-0140 exception because the resolved graph no longer triggers it. Receipts: `/tmp/br-4e2n1-evidence-20260912/dependency-audit*.json`.
- Controller root filesystem filled during concurrent work. A separate checkout at `/tmp/br-4e2n1-release-20260912` preserves the pending update without deleting or reverting the original checkout. Candidate tests still use explicit Cargo manifest/lockfile overlays on the frozen original base, through RCH.
- Candidate library gate passed through strict RCH on ovh-a: 3,138 passed, zero failed, nine existing ignores; 48.09 seconds of test execution, completed 2026-09-12 01:06 UTC. Outer command exited zero and returned clean-overlay fingerprint `8193c7e1641641bae241ac6c2fc3d59c02c546359c04163cc54ff4b3d9e9ddd2`. MCP protocol/shutdown and package-manifest integration qualification is running next.
- Release infrastructure: dedicated RCH daemons on Mac (PID 81765) and ts2 (PID 95603), each with only the opposite host as a worker, passed live capability probes. Linux/GNU Windows DSR commands will launch on Mac and compile on ts2; Darwin commands will launch on ts2 and compile on Mac. Source-derived routing is accepted by pinned DSR `87d0be6c`; actual DSR build/artifact round trips remain unproved. Configs and probes are retained in `/tmp/br-4e2n1-dsr-rch/` and `/tmp/br-4e2n1-evidence-20260912/`; no shared daemon/config changes or local builds.
- Candidate MCP/package integration gate passed through RCH at 2026-09-12 01:13 UTC: 22 protocol tests, one shutdown test, and ten package-manifest tests; zero failures or ignores. Compilation took 6m24s. The 38-target `e2e-a-l` shard is next. This RCH frontend rejects combining `--job` with `--clean-overlay`, so the shard's existing membership is expanded into direct Cargo `--test` arguments to preserve the frozen-source overlay contract.
- The release canary now exercises deferred-claim refusal, distinct prerequisites and acceptance criteria, prospective class transitions, parallel typed dependencies and bounded ready output. Its Linux harness qualification passed 82 commands against the all-feature development binary; this is not release-binary qualification. The initial harness expected the wrong error code for a prohibited workflow edge (`POLICY_VIOLATION` instead of the existing `VALIDATION_FAILED`); corrected that assertion and reran in a new retained workspace. Evidence: `/tmp/br-4e2n1-evidence-20260912/canary-harness-qualification-v2.log`.
- Full `e2e-a-l` shard passed at 2026-09-12 01:26 UTC: all 38 Cargo test targets, 6,521 test invocations, zero failures and four existing ignores, outer exit zero. The final manifest-comment correction changes the clean-overlay fingerprint to `dee6fc39c65b8d7c73d953080cfde6fc18be34f76eaa9f49f59de156ea1d3823`. The 39-target `e2e-m-z` shard is running on ovh-a; the independent 50-target storage shard is compiling on ts2.
- Full `e2e-m-z` shard passed at 2026-09-12 01:37 UTC: all 39 targets, 6,521 test invocations, zero failures and two existing ignores, outer exit zero. The 22-target miscellaneous shard now runs with the pinned reference tools explicitly on PATH and `BD_BINARY` set. A dedicated Mac qualification commit `4de8920fc13e4281690dd5369b53970cc5ae12b7` preserves all 4,169 tracked entries from `a22c251b` with only the two candidate manifest/lockfile changes; exact Git blob/mode comparison reports zero other differences. This temporary test commit is not the release source or a published commit.
- Conformance prerequisites: ovh-a's installed bd 0.40.0 and bv 0.24.1 differ from the test pins. Downloaded bd 0.46.0 and bv 0.22.0 from their published GitHub releases, verified their archive SHA-256 sidecars and the copied executable hashes, and placed them in a dedicated `/tmp/br-4e2n1-reference-bin` directory without replacing installed tools. Executable hashes: bd `8993761b844c84f76d0128efcd237adaf2f0358fec2eae8337125c881f1c0565`; bv `70a534fc61e928918b08e7812cc2419c56da5c66d0a834b26172d7b2e11cac33`.
- Publication access: br, Homebrew tap and Scoop bucket have Actions disabled; GitHub credentials have push access to both venue repositories. AUR authentication was denied on Mac, ts2 and the controller. For the latter two, verified the host key against the [AUR's published Ed25519 fingerprint](https://aur.archlinux.org/?setlang=en) and used a dedicated known-hosts file. The outstanding user question concerns the publishing identity, not permission to release.
- RCH cache diagnosis: per-command temporary Cargo homes changed registry source paths and forced each shard to rebuild dependencies. Subsequent ovh-a commands will explicitly reuse this campaign's existing Cargo home at `/data/tmp/rch/beads_rust/ce318454fb5948b8/.rch-tmp/rch-cargo-cache-ovh-a`; compiler profiles and assertions remain unchanged. The ts2 cache and target paths are also recorded for a warm retry if its cold 50-target build reaches the declared cap.
- Docker on the Mac completed its existing-container restoration at 2026-09-12 01:42 UTC after a normal application start. A new retained, network-disabled `python:3.12-slim` container executed successfully and reported `aarch64`; no existing containers or processes were stopped or removed. The final seven-target DSR configuration passed `repos validate`. Both are infrastructure checks, not release-binary qualification.
- The 22-target miscellaneous/conformance/snapshot shard passed through RCH at 2026-09-12 01:53 UTC: 3,284 test invocations, zero failures, 72 existing ignores, outer exit zero. This includes the randomized storage model and long sequential dependency-removal regression. The ts2 cold storage build reached its declared 1,800-second cap before tests began (remote/outer 137); its source receipt and cache remain intact. A four-job retry was refused before execution because the dedicated worker advertises two slots. The next invocation preserves two jobs, explicitly reuses the existing Cargo home and target, and declares 3,600 seconds prospectively. No local fallback or test pass is claimed for either refused/timed-out invocation.
- The installed RCH 1.0.64 source-receipt path rewrites the requested target directory underneath a fresh per-build source root; only the explicitly requested Cargo registry cache survived. The bounded ts2 retry is consequently rebuilding dependencies. The ovh-a clean-overlay path uses a durable target pool and now retains the explicit registry cache as intended.
- All-target, all-feature Clippy with `-D warnings` passed through RCH at 2026-09-12 01:57 UTC (3m26s); all-target, all-feature Cargo check passed at 01:58 UTC (1m00s), both outer exit zero. `cargo fmt --check` and `git diff --check` also passed. Default-feature library/binary tests are running next.
- Expanded native canary qualification now passes 95 commands, including actual schema-15 and schema-16 fixtures migrated to schema 19, issue-field/count preservation, unchanged source JSONL, and no-op re-planning. A first invocation used an already-removed RCH source path; retained that failed harness log and reran with separately copied frozen fixtures in a new workspace. Passing development-binary SHA-256: `1ab6de0e785bb2a418e3b0a39936e1f31a8b95612bbdc9b349323fe8b66d0bcf`; receipt `/tmp/br-4e2n1-evidence-20260912/canary-harness-qualification-v4.log`. The eventual release binaries still need the same qualification.
- Version availability checked at 2026-09-12 02:00 UTC: no remote `v0.6.0` tag, GitHub release endpoint 404 and crates.io version endpoint 404. The new prerequisites/workflow capabilities warrant 0.6.0; version changes remain pending completion of the test gate.
- Pinned DSR source review found that its strict ELF validator rejects custom `linux/musl_*` target keys even when configuration validation passes. Corrected the plan before build execution: five canonical primary targets, plus a second two-target recipe using canonical Linux keys mapped to musl triples and explicit musl artifact names. Both recipes validate; packaging retains both original run states and the explicit key mapping. This follows the prior release's successful musl route without altering DSR's validator.
- Default-feature `cargo test --locked --lib --bins` passed through RCH at 2026-09-12 02:05 UTC: 3,058 library tests with two existing ignores, plus 66 binary tests; zero failures and outer exit zero. No-default-feature library qualification is next.
- No-default-feature library tests passed through RCH at 2026-09-12 02:12 UTC: 3,045 passed, zero failed, two existing ignores, outer exit zero. All-feature binary tests and the seven ordinary benchmark test targets are running next; scheduled ignored stress benchmarks remain outside this release test invocation, with no new ignores or filters added.
- All-feature binary tests plus all seven ordinary benchmark targets passed at 2026-09-12 02:13 UTC: 1,259 test invocations, zero failures, 22 existing ignores, outer exit zero. Signing readiness also passed on trj using the unchanged br key `36B847D11BA5A0D0`; its private key stays there. The Mac's unrelated default DSR key `69B3955C8D2E62A8` will not sign br assets. AUR authentication was denied on trj as well.
- `cargo test --locked --all-features --doc` exited zero at 2026-09-12 02:14 UTC, but executed no test cases: all ten existing doctests are ignored. This supplies no additional runtime coverage. The separate `docs_examples` integration target ran in the passed miscellaneous shard; no doctest ignores were added or changed.
- Once ovh-a's other gates finished, moved the unchanged 50-target storage shard to its warm all-feature cache. Gracefully cancelled only owned ts2 build `30017382718111748` at 2026-09-12 02:16 UTC; the cancellation released both slots, the Cargo PID was absent and the isolated queue was empty. No tests ran in that cancelled retry. Retained both ts2 build directories. Fresh ts2 health: load 3.97 on 128 CPUs, 192 GiB available memory and 1.8 TiB disk free. The dedicated release scheduler will advertise eight slots for sequential Linux/Windows cross builds; Mac native builds remain two jobs.
- The full 50-target storage/property/regression/workflow shard passed on ovh-a at 2026-09-12 02:20 UTC: 4,066 test invocations, zero failures, two existing ignores, outer exit zero. The dependency test gate is complete across all-feature library/integration shards, default and no-default library configurations, binary/ordinary benchmark targets, all-target checks and Clippy, formatting and security audit. The doc command executed no cases, as recorded above. Proceeding to 0.6.0 metadata and focused version-sensitive checks; no release binaries or publication yet.

- Release preparation committed as `b1cfebe05437463e91a353cf2bedafac27266f5b`, with local annotated `v0.6.0` tag (not pushed). Focused version-sensitive checks passed through RCH at 2026-09-12 02:25 UTC: 625 tests, zero failures or ignores. The clean Mac release checkout preserves all 4,169 tracked entries. Primary strict DSR build started at 02:30 UTC for five targets; the two musl targets require a separate completed DSR run because the pinned validator accepts canonical Linux platform keys. No release venue is published yet.

- First DSR attempt stopped before compilation because its locked offline source-closure check lacked FastMCP 0.9.0 in the command hosts' ambient registry caches. Fetched the unchanged locked dependencies on Mac and ts2. A retry correctly refused reusing the first output directory; the next attempt uses a fresh output path and retains both refusals. Its two-target concurrency is bounded by one DSR slot per command host, mapping to one RCH build per actual worker; Mac compilation stays at two jobs and ts2 at eight.
- The exact crates.io package contains 1,080 files and has SHA-256 `0d0fbac9a6c83b1ee48ab585f3d5f3fe9c8005a05cab7e039502d9293ed1cecf`. Every extracted file was byte-compared to the archive; a temporary verification-only Git index explicitly includes packaged files ignored by the repository's normal ignore rules. Its default-feature binary compiled through RCH on ovh-a at 2026-09-12 02:39 UTC. Runtime qualification and the optional-feature package build are pending. SPDX and CycloneDX inventories both include all 633 frozen lockfile packages; these are source inventories, not claims that every optional dependency is compiled into each binary.

- Exact extracted-package qualification is complete: default binary passed 95 CLI/migration commands, 29 PTY commands, and all 45 doctor selftest steps; its first doctor invocation failed because the requested parent directory did not exist, then passed after creating that directory. The all-feature package build passed at 02:45 UTC, and all 22 packaged MCP protocol tests passed at 02:52 UTC, with zero failures or ignores. The package SHA remains unchanged. This is focused package qualification, not a claim that fixture-dependent repository tests all run from the published archive.
- DSR run `4659395f-e8d0-4516-8760-d0767213f8cb` now compiles Linux/Windows through RCH. Its two Mac attempts failed before compilation because the dedicated route used Linux's `/data` staging path; corrected only that route's `transfer.remote_base` to `/Users/Shared/dsr-sources/br-4e2n1-rch-mac`. Separate Mac run `5aa83804-8eef-4cbe-939e-f5feb467fff9` is compiling on Apple Silicon. Packaging will explicitly select three successful Linux/Windows results, two successful Mac results, and two successful musl results from their actual run records; no failed target is selected or reclassified.

- First two accepted release binaries passed native qualification: Linux GNU amd64 (`21b967c1ae68df1a2e8eb2256d13b8e57d293d89e331933919076104832ddbc0`, 27,772,512 bytes; max GLIBC 2.28) and Apple Silicon (`822657f6d9d52f4d81e4483467614ca32f96f79dde796c9b2601d05e2cb80401`, 16,138,704 bytes). Each passed 95 CLI/migration commands, 29 real-PTY commands, and 45 doctor selftest steps, reporting the frozen release commit and default `self_update` feature. Windows release compilation reports five unused-code warnings; the retained v0.5.12 Windows log has the same five diagnostics (`windows-warning-baseline-0512.txt`). No new Windows warning is claimed, and the Linux Clippy result is not presented as a Windows Clippy run.

### Execution checklist

- Five of seven raw release binaries are accepted and runtime-qualified. Windows GNU amd64 passed 92 CLI/migration commands and 45 doctor steps; Linux GNU arm64 and Intel macOS each passed 95 CLI/migration commands, 29 PTY commands and 45 doctor steps. Intel execution used Rosetta on Apple Silicon, not physical Intel hardware. GNU arm64's maximum required GLIBC is 2.28.
- The first Intel DSR attempt returned 137 without a valid result, although its underlying RCH job subsequently returned zero. That artifact was not selected. The resumed DSR attempt succeeded. DSR reused its compiler-log pathname; the attempted archive raced with truncation and contains early retry output. The original result and orchestration logs remain, but the original full compiler log does not. `darwin-amd64-log-retention-note.txt` documents this limitation.
- The Linux arm64 migration canary failed on a writable macOS Docker bind mount with a database-identity change. The published 0.5.12 binary reproduces the same failure; the 0.6.0 binary passes the entire canary on a native Docker volume. Open P1 bead `beads_rust-q93wv` tracks the limitation. Retained both failed workspaces; no migration repair is claimed. Release notes identify native macOS or native Linux storage as the qualified migration routes.
- Musl amd64 compilation returned zero at 2026-09-12 03:39 UTC; DSR artifact collection remains pending. Musl arm64 was refused before compilation by RCH's active-project exclusion, despite available slots. Its original refusal/result were archived before resuming; subsequent builds will run sequentially without bypassing that exclusion.
- The exact crates.io publication dry run passed and its actual upload payload (`package/tmp-crate/beads_rust-0.6.0.crate`) matches the qualified SHA-256. Nothing has been published. Both DSR-verified SBOMs cover all 633 locked packages.

- [x] Read project and skill instructions; check clean tracked source and previous release records.
- [x] Inventory every direct dependency against the registry and lockfile.
- [x] Claim release bead and reserve manifest, lockfile, and upgrade logs.
- [x] Complete stable-tag API and dependency-pin research.
- [x] Finish baseline tests; update the coupled dependency unit and pass its library tests.
- [x] Record retained exact pins and audit the resolved dependency graph.
- [x] Pass all-feature library and targeted MCP/package-manifest gates.
- [x] Pass all 38 `e2e-a-l` targets through RCH.
- [x] Pass all 39 `e2e-m-z` targets through RCH.
- [x] Pass all 50 storage/property/regression/workflow targets through RCH.
- [x] Pass miscellaneous/conformance/snapshot targets using pinned bd 0.46.0 and bv 0.22.0.
- [x] Pass default-feature tests and the no-default-feature library gate.
- [x] Pass binary tests and ordinary non-ignored benchmark targets; run the doc gate and explicitly record its zero executable cases.
- [x] Pass all-target Cargo check and Clippy with warnings denied; verify formatting.
- [x] Qualify the expanded native release canary against the Linux development binary.
- [x] Update changelog from verified commits; select and bump the next available release version after the test gate.
- [x] Freeze source, version, features and lockfile.
- [ ] Build Linux GNU amd64/arm64, Linux musl amd64/arm64, macOS amd64/arm64, and Windows amd64 through RCH using the established DSR release flow.
- [ ] Verify canonical archive contents, seven-target size budgets, SHA sidecars, Minisign signatures and SBOMs.
- [ ] Run native CLI/migration canaries, Unix PTY checks and doctor selftests on all seven release binaries; verify GNU ABI floors, static musl linkage and Windows DLL imports.
- [ ] Stage the GitHub draft, download/verify all 24 assets and exercise the downloaded binaries before publication.
- [ ] Publish GitHub and verify all 24 assets again through unauthenticated public downloads.
- [x] Package the frozen crate and byte-compare all 1,080 extracted files.
- [x] Compile the exact package's default and all-feature binaries through RCH; pass CLI, migration, PTY, doctor, and MCP protocol checks.
- [ ] Publish that exact package and verify the crates.io registry checksum.
- [ ] Update and verify the Homebrew tap and real installation.
- [ ] Update and verify the Scoop bucket and real Windows installation.
- [ ] Build both Arch binary packages and verify native amd64 installation/integrity before AUR publication.
- [ ] Publish and read back AUR when an authorized SSH identity is available; retain the explicit blocker meanwhile.
- [ ] Exercise the public installer and an actual old-to-new self-update.
- [ ] Commit publication metadata and evidence summaries; close only fulfilled obligations. Historical AUR blockers stay separate.

---

**Date:** 2026-09-04 | **Project:** beads_rust | **Language:** Rust (nightly-2026-08-31, edition 2024) | **Manifest:** Cargo.toml

## Summary (2026-09-04)

- **Inventory:** 56 direct dependency entries (41 `[dependencies]`, 1 build, 14 dev); 20 entries behind crates.io's latest stable at start: the `fsqlite*` family (15 crates, 0.3.15 → 0.3.16, published 2026-09-04), `asupersync` (=0.4.9 → 0.4.10, two entries), `fastmcp-rust` (=0.7.1 → 0.8.1), `toon_rust`/`tru` (0.2.3 → 0.2.4), `toml` (dev, =1.1.4 → 1.1.5). Everything else already at latest stable (vergen-gix 10.0.3, rand 0.10.2, clap 4.6.6, serde 1.0.229, chrono 0.4.45, regex 1.13.1, thiserror 2.0.20, insta 1.48.0, proptest 1.11.0, criterion 0.8.2, ...).
- **Method:** one dependency at a time; research from crates.io metadata and the upstream changelog/compare; manifest + lockfile update; the relevant test targets through RCH (`rch exec -- cargo ...`; RCH caps one command at 5 min for builds and 30 min for tests, so each entry names the exact targets run); log here before moving on.
- **Order:** tru → toml → fastmcp-rust → fsqlite family (engine-bump checklist, `docs/reliability/ENGINE_OPERATING_MODEL.md` §6). asupersync stays at =0.4.9: fastmcp-rust 0.8.1 still pins `=0.4.9` exactly and the `mcp` build must carry one asupersync (bead beads_rust-fiop); fsqlite 0.3.16 accepts `>=0.4.3,<0.5`.
- **Result:** updated 3 lines (fsqlite family ×15 manifest entries / 20 crates, fastmcp-rust ×8 crates plus its `log` pin, tru), skipped 2 (asupersync, toml — both held by fastmcp's exact pins), failed 0, rolled back 0. Landed as one commit (5e81e796) after every dependency had passed its own RCH gates; the hosted CI push run and a dispatched `Reliability Gates` run are the receipts for the whole tree (ids below).

## Updates (2026-09-04)

### toon_rust (`tru`): 0.2.3 → 0.2.4

- **Changelog:** [v0.2.3...v0.2.4](https://github.com/Dicklesworthstone/toon_rust/compare/v0.2.3...v0.2.4): installer download retries, hardened git-metadata detection in its build.rs, dependency bumps (clap_complete, assert_cmd, vergen-gix 9→10), and its *optional* asupersync pinned to `=0.3.4` — a feature br does not enable, so no second asupersync enters the graph (verified against the crates.io dependency list for 0.2.4). No changes to `encode`/`encode_lines`/`EncodeOptions`/`KeyFoldingMode`, the surface br uses in `src/output/context.rs`.
- **Breaking changes:** none for br's usage.
- **Lockfile:** `cargo update -p tru` moved only `tru`; the same write pruned sixty orphaned entries (the old vergen 9 / `gix-*` tree that nothing referenced since vergen-gix moved to 10) — no other version changed, vergen-gix stays 10.0.3.
- **Tests (RCH):** `cargo clippy --lib --bins -- -D warnings` clean; `cargo test --lib output::` 41/41; `cargo test --test e2e_create_output` (TOON output) 7/7.

### fastmcp-rust (optional `mcp` feature): =0.7.1 → =0.8.1

- **Changelog:** [v0.8.1](https://github.com/Dicklesworthstone/fastmcp_rust/releases/tag/v0.8.1) (first published 0.8; v0.8.0 was a quarantined candidate). Pre-1.0 minor release: caller-owned asupersync contexts at library boundaries (client constructors take `&Cx`; returning server runners and custom-transport runners require a caller-owned context), the facade no longer exports `block_on`, cancel-correct admission/cleanup, transport fixes (stdio partial-frame deadline, WebSocket cancellation, SSE write-half close), packaging (redis-tasks and safe-icon-rendering features removed; plist/quick-xml advisory clear; `license-file` metadata).
- **Breaking changes for br:** none. `br serve` already builds its own current-thread asupersync runtime, mints a request `Cx`, and runs `ServerBuilder::…build().run_transport_returning_with_cx(&cx, StdioTransport::stdio())` (`src/mcp/mod.rs`), which is exactly the caller-owned-context shape 0.8 requires. No source changes.
- **Pins:** fastmcp-rust/fastmcp-client 0.8.1 keep the exact pins `asupersync =0.4.9`, `serde =1.0.229`, `serde_json =1.0.151`, `toml =1.1.4`, `rustix =1.1.4` (all equal to what br already resolves) and add `log =0.4.34`, which moved `log` 0.4.33 → 0.4.34 in the lockfile. The eight `fastmcp-*` workspace crates moved together (0.7.1 → 0.8.1); nothing else changed. The manifest comment on the asupersync pin now cites 0.8.1.
- **Tests (RCH):** `cargo test --lib --features mcp mcp::` 75 passed / 7 ignored (pre-existing ignores); `cargo test --features mcp --test e2e_mcp_protocol` 1/1; `cargo test --features mcp --test e2e_mcp_shutdown` 1/1 (SIGINT returns through `main` and the DB reopens — the runtime-ownership change did not disturb the cancellation path); `cargo clippy --lib --bins --features mcp -- -D warnings` clean.

### fsqlite family (15 manifest entries, 20 crates in the lock): 0.3.15 → 0.3.16

- **Changelog:** [v0.3.16](https://github.com/Dicklesworthstone/frankensqlite/releases/tag/v0.3.16) (2026-09-03; crates.io 2026-09-04). Engine-relevant items per the §6 checklist (pager/WAL/B-tree/checkpoint/VFS):
  - **Pager:** the EOF-growth double-grant is closed (bd-9inpb, `6f61702f9`). Two connections growing the file concurrently could both allocate the same fresh EOF page and commit it ("2nd reference to page N", lost rows); `commit_flush` now re-derives the pre-floor snapshot size under the RESERVED append lock and refuses a batch whose fresh pages fall in `(snapshot_db_size, durable_floor]` with a retryable `BusySnapshot` (first committer wins). Upstream repro: 3 double-grants in 76 eight-writer runs before, 0 in 80 after; perf-neutral at 1–8 writers. This is the corruption class br's concurrent writers live next to, so it is the reason to take the bump.
  - **WAL / checkpoint / open:** `reclaim_disowned_in_range` (run by `checkpoint` and by the on-open reclamation sweep) no longer rescans every WAL frame header per page; an `AppendedTailIndex` keyed on generation, frame count, and last-frame checksum indexes a stable tail once (`8d012706a`, cass GH#382). Same answers, bounded cost on large WALs.
  - **Not relevant to br:** the FTS5 lazy-read, savepoint undo-log, and incremental-append work (br creates no FTS5 tables); the upstream lockfile refresh (their manifest ranges are unchanged; asupersync stays `>=0.4.3,<0.5`, satisfied by our `=0.4.9`).
  - **Open escalation:** frankensqlite#407 (bead ro3m) was fixed upstream on 2026-09-04 in `007822add`/`efdf9e2a0`, eleven and fourteen commits **after** the v0.3.16 tag. The probe `grouped_having_in_subquery_count_with_bound_params --ignored` still fails on 0.3.16 (baseline on 0.3.15 also failed), so the `multi_label_and` counting detour and the ignore stay; the ignore text, the code comment, and the §7 row now say 0.3.16 and point at the fix.
- **Breaking changes:** none; no API change in the facade br uses (`src/franken_sync.rs`). `tinyvec` stays 1.12.0 in our lock (upstream notes 1.13.0 fails to compile for them).
- **Lockfile:** the 15 manifest crates plus the five `fsqlite-ext-*` crates moved 0.3.15 → 0.3.16; no other version changed.
- **Tests (RCH, §6 items 2, 5, 6):** `cargo test --lib` 2969 passed / 4 ignored (150 s on hz4); `cargo test --test model_based_storage` 163/163 (the 120-case property run plus the GH#426 chain and the blocker-direction regression; 864 s on a worker shared with two other cold builds); `cargo test --test linearizability_multiprocess -- --nocapture` 166/166 — 361 operations in 30 s over eight process streams, none failed, every history linearizable, 30 issues observed at quiescence, published JSONL equal to the observed final state; `cargo clippy --lib --bins -- -D warnings` clean; the ro3m probe re-run as above.
- **Stress gate and doctor (§6 items 3–4):** not runnable locally (RCH cannot deliver a built `br` within its caps); the hosted CI `Reliability Gates` job (manual dispatch) runs the failure-corpus replay, crash-injection matrix, the single-workspace and concurrent stress harnesses, the multi-process stress, and the linearizability check on the pushed tree — its run id is recorded below once it completes.

## Skipped (2026-09-04)

- `toml =1.1.4` (dev; 1.1.5 available): `cargo update -p toml` refuses — `fastmcp-client` pins `toml = "=1.1.4"` exactly, at 0.7.1 and still at 0.8.1, and the `mcp` build can carry only one `toml` 1.1.x. 1.1.5 is a single fix (`DeValue::make_owned` on integers/floats) that br's manifest tests do not exercise. Revisit when fastmcp-client moves its pin.
- `asupersync =0.4.9` (0.4.10 available): held by fastmcp-rust 0.8.1's exact `=0.4.9` pin; 0.4.10 is observability/regex scanner work (bounded PII, payment-card, and phone scanners) with no runtime-contract change noted, so nothing is lost by waiting for fastmcp to move its pin.

---

**Date:** 2026-08-14 | **Project:** beads_rust | **Language:** Rust

## Summary

- **Updated:** fsqlite family (15 crates) 0.1.18 → 0.3.1; new direct `asupersync =0.4.4`; FastMCP's asupersync line 0.3.9 → 0.3.10; 11 minor/patch lockfile bumps | **Skipped:** 2 (with reasons) | **Failed:** 0

## Discovery

- Manifest: `Cargo.toml`; lock file: `Cargo.lock`.
- crates.io max stable at completion: `fsqlite* = 0.3.1` (all 15 pinned members published), `asupersync = 0.4.4`, `fastmcp-rust = 0.3.2` (unchanged; still on the asupersync 0.3.x line).
- All other direct dependencies were already at latest stable or covered by existing caret ranges; only lockfile refreshes were needed (supersedes Dependabot PR #425).

## Updates

### fsqlite stack: 0.1.18/0.1.19 → 0.3.1 (with asupersync 0.4.4)

- **Breaking (upstream 0.2.0):** the entire engine API became `async fn` with `!Send` futures (`Connection::open`, `execute*`, `query*`, `prepare`, `close*`, `compat::open_with_flags`).
- **Breaking (upstream 0.3.0):** the runtime family moved from asupersync 0.3.10 to `>=0.4.3,<0.5`; 0.3.x and 0.4.x asupersync types are non-interchangeable.
- **Migration:** added `src/franken_sync.rs`, a synchronous facade that drives every engine future to completion on the calling thread via a thread-local current-thread `asupersync` Runtime (`Runtime::block_on`; the proven cass/sqlmodel bridge pattern). The runtime is taken out of its slot while polling so reentrant SQL builds a fresh runtime instead of re-entering `block_on`. The facade carries a bounded `BusyRecovery` retry (restores 0.1.x observable behavior around fsqlite 0.2+ ns-lifecycle recovery windows) and a stale-schema `prepare()`-refresh retry (fsqlite 0.2.1+ cross-connection DDL visibility). All `Connection`/`Row` imports across storage, sync, config, doctor subsystems, CLI, and integration tests moved to `crate::franken_sync::` / `beads_rust::franken_sync::`; `Row`, `SqliteValue`, and `FrankenError` re-export unchanged. Every writable open, including the explicit read-write compatibility path used by reconciliation, selects serialized engine mode to match br's workspace write lock. Missing-database recovery now quarantines all orphaned fsqlite 0.3 sidecars into verified backups before rebuilding from JSONL. `Drop` drives a best-effort close so writes through a dropped connection stay visible to later opens (#270 contract).
- **asupersync:** new direct dependency `asupersync = { version = "=0.4.4", default-features = false }` (initially =0.4.3; bumped same day when upstream published 0.4.4), matching the fsqlite family requirement so one runtime version serves the whole default graph. The 0.4.4 cancellation-contract refinement (spawned-task results surviving cancel acknowledgement) does not affect br's `block_on` bridge, which spawns no tasks.
- **mcp feature caveat:** published `fastmcp-rust 0.3.2` still requires `asupersync ^0.3.4`, so `--features mcp` builds carry both asupersync 0.3.x and 0.4.4 (they are distinct crates under Cargo's 0.x rules and coexist). This resolves to a single 0.4.4 line once fastmcp republishes against 0.4.x.
- **Engine-fix relevance:** fsqlite 0.3.0/0.3.1 fix the allocator page-aliasing, committed-freelist resurrection, and concurrent-writer EOF-growth corruption classes plus concurrent-open `BusyRecovery` fail-fasts — the classes behind beads_rust issues #426 and #428 and the concurrent-open regression that blocked the earlier (abandoned) `harmonize/vlsf2` migration attempt.
- **Tests:** see Validation below.

### Minor/patch dependency updates (supersedes Dependabot PR #425)

- clap 4.6.4 → 4.6.6, clap_complete 4.6.7 → 4.6.9, schemars 1.2.1 → 1.2.2, similar 3.1.1 → 3.1.2 (manifest floors + lock).
- toml (dev-dependency, exact pin) =1.1.2 → =1.1.4.
- FastMCP's independent asupersync line 0.3.9 → 0.3.10, including its
  `franken-{kernel,evidence,decision}` 0.3.10 family and consolidated crypto
  dependency graph.
- lru 0.18.1 → 0.18.2 for fsqlite-core/fsqlite-planner, fixing
  RUSTSEC-2026-0253's panic-safety use-after-free in `LruCache::pop`.
- Lockfile-only refreshes: thiserror 2.0.20, libc 0.2.189, once_cell 1.21.4, regex 1.13.1, flate2 1.1.9.
- **Breaking:** none found for this project's usage in any of these lines.

### Lint-gate remediation (issue #409 cluster E)

- The 2026-08 nightly clippy added `assert_is_empty` (pedantic), which fired ~125 times on test `assert!(x.is_empty())` calls; added to the Cargo.toml stylistic allow-list alongside the existing entries (rewriting those asserts is churn, not safety).
- The remaining ~100 pedantic/nursery findings in the merged doctor/sync workstream code were fixed individually (renamed used-underscore bindings, by-ref parameters, heap-allocating the 1 MiB and 64 KiB stack buffers, boxing the large `PendingSyncMergeInspection::Valid` variant, `let...else` rewrites, merged match arms, `trailing_zeros` bit tests, per-function `too_many_lines` allows per codebase pattern, and documented targeted allows where a fix would change cross-file signatures or MSRV-unavailable APIs are involved).

## Skipped

- `self_update 1.0.0-rc.x`: pre-release line retained (crates.io max stable is the older 0.44); per policy, pre-release pins are preserved.
- `cap-primitives = "=4.0.2"`: exact pin retained by design (sync's hostile-path boundary).

## Needs Attention

- `fastmcp-rust`: republish against asupersync 0.4.x will let the `mcp` feature collapse to a single asupersync (tracked informally; sibling checkout already pins =0.4.3 at version 0.3.2, unpublished).
- `rich_rust 0.2.2` retains lru 0.16.4, which cargo-audit reports under the
  same informational panic-safety advisory. Its caches use ordinary
  `String`/`Style` keys rather than caller-provided panicking `Drop` types;
  upgrading requires a new `rich_rust` release because 0.2.2 constrains lru
  to the 0.16 line.

## Validation

- `cargo check --all-targets` passed after the migration.
- `cargo fmt --check` clean.
- `cargo clippy --all-targets --all-features -- -D warnings` clean
  (pedantic + nursery at deny).
- `br serve` SIGINT shutdown test passes
  (`e2e_mcp_shutdown::serve_sigint_returns_through_main_and_preserves_reopenable_db`)
  after fixing a same-process write-lock self-deadlock that predated the
  engine upgrade.
- Targeted regression suites on the settled tree: `e2e_read_only_fast_open`
  160/160, `e2e_sync_reconcile` 180/180, `e2e_sync_failure_injection`
  179/179, `e2e_sync_status_health` 166/166, `e2e_sync_artifacts` 169/169,
  doctor fixture suite 65/65, storage_deps + e2e_relations cycle clusters
  green.
- Full `cargo test --all-features --no-fail-fast` on the settled tree:
  **21,490 passed, 0 failed** across every test binary (doctests included),
  up from 21,415 passed / 70 failed at the start of the migration wave.
