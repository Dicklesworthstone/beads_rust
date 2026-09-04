# Dependency Upgrade Log

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
