# Dependency Upgrade Log

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
