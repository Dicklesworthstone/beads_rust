# Dependency Upgrade Log

**Date:** 2026-01-18  |  **Project:** beads_rust  |  **Language:** Rust

## Summary
- **Updated:** 4  |  **Skipped:** 1  |  **Failed:** 0  |  **Needs attention:** 0

## Updates

### criterion: 0.7.0 -> 0.8.1
- **Breaking:** None documented; version bump aligns criterion-plot dependency
- **Migration:** None required
- **Tests:** Passed (649)

### rusqlite: 0.32.1 -> 0.38.0
- **Breaking:** `usize`/`u64` ToSql/FromSql disabled by default; statement cache optional; min SQLite 3.34.1
- **Migration:** Added `fallible_uint` feature flag to re-enable `usize` ToSql support
- **Tests:** Passed (649)

### unicode-width: 0.1.14 -> 0.2.2
- **Breaking:** Control characters now return `Some(1)` instead of `None`
- **Migration:** Code already uses `unwrap_or(0)` which handles the change gracefully
- **Tests:** Passed (649)

### indicatif: 0.17.11 -> 0.18.3
- **Breaking:** None found in project usage
- **Tests:** Passed (649)

---

## Skipped

### vergen-gix: 1.0.9 -> 9.1.0
- **Reason:** Blocked by Rust version constraint; vergen-gix 9.x may require newer Rust than 1.85
- **Action:** Investigate if project can bump rust-version in rust-toolchain.toml, or wait for compat release

---

## Transitive Updates (via cargo update)

These were automatically updated as dependencies of direct dependencies:
- hashlink: 0.9.1 -> 0.11.0 (rusqlite dependency)
- libsqlite3-sys: 0.30.1 -> 0.36.0 (rusqlite dependency)
- criterion-plot: 0.6.0 -> 0.8.1 (criterion dependency)
- Various gix-* crates (vergen-gix dependencies)

---

## Commands Used

```bash
# Check for outdated dependencies
cargo outdated

# Update specific package
cargo update -p rusqlite

# Run tests after each update
cargo test --lib
```

---
---

# Upgrade Session: 2026-02-19

**Date:** 2026-02-19  |  **Project:** beads_rust  |  **Language:** Rust

## Summary
- **Updated:** 24  |  **Replaced:** 1 (serde_yaml -> serde_yml)  |  **Skipped:** 4  |  **Failed:** 0

## Toolchain

### Rust nightly: unversioned -> nightly-2026-02-19
- **Change:** Pinned `rust-toolchain.toml` from generic `nightly` to `nightly-2026-02-19` (rustc 1.95.0-nightly, 2026-02-18)
- **Reason:** Reproducible builds; generic `nightly` channel was corrupted on this machine
- **Tests:** All pass

## Major Updates (Breaking Changes)

### schemars: 0.8 -> 1.0
- **Breaking:** `RootSchema` removed (replaced by `Schema`), `schema_name()` returns `Cow<'static, str>` instead of `String`, `schemars::gen` module renamed to `schemars::generate`, feature `chrono` renamed to `chrono04`, schemas now target JSON Schema draft 2020-12
- **Migration:** Updated Cargo.toml features, replaced `RootSchema` with `Schema` in `schema.rs`, updated manual `JsonSchema` impl in `model/mod.rs` to use new trait signatures
- **Files changed:** `Cargo.toml`, `src/model/mod.rs`, `src/cli/commands/schema.rs`
- **Tests:** Pass

### serde_yaml 0.9 -> serde_yml crate alias backed by serde_norway 0.9.42 (REPLACED, UPDATED)
- **Reason:** `serde_yaml` is deprecated and unmaintained. The first replacement used `serde_yml`, but the `serde_yml` package was later archived after unsoundness reports. The current manifest keeps the local `serde_yml` crate alias for compatibility while backing it with the maintained `serde_norway` package.
- **Migration:** Renamed all `serde_yaml::` references to `serde_yml::` across 4 source files, then repointed the alias from package `serde_yml` to package `serde_norway`. Fixed a `match_same_arms` clippy lint exposed by the reformatting.
- **Files changed:** `Cargo.toml`, `src/error/mod.rs`, `src/config/mod.rs`, `src/cli/commands/config.rs`, `tests/conformance.rs`
- **Tests:** Pass

### crossterm: 0.28 -> 0.29
- **Breaking:** `KeyModifiers` display now uses `+` separators; new default `derive-more` feature
- **Impact:** This project only uses `crossterm::style::Stylize` -- no impact
- **Tests:** Pass

### rand: 0.9.2 -> 0.10
- **Breaking:** `Rng` extension trait renamed to `RngExt`, `StdRng` no longer implements `Clone`, `SeedableRng::from_os_rng()` removed
- **Migration:** Updated `use rand::Rng` to `use rand::RngExt` in `tests/bench_synthetic_scale.rs`
- **Files changed:** `Cargo.toml`, `tests/bench_synthetic_scale.rs`
- **Tests:** Pass

## Minor/Patch Updates (No Breaking Changes)

| Crate | From | To | Notes |
|-------|------|----|-------|
| clap | 4.5 | 4.5.60 | Semver-compatible |
| clap_complete | 4.5 | 4.5.66 | Semver-compatible |
| serde | 1.0 | 1.0.228 | Semver-compatible |
| serde_json | 1.0 | 1.0.149 | Semver-compatible |
| chrono | 0.4 | 0.4.43 | Semver-compatible |
| sha2 | 0.10 | 0.10.9 | Semver-compatible |
| anyhow | 1.0 | 1.0.102 | Semver-compatible |
| tracing | 0.1 | 0.1.44 | Semver-compatible |
| tracing-subscriber | 0.3 | 0.3.22 | Semver-compatible |
| indicatif | 0.18 | 0.18.4 | Semver-compatible |
| dunce | 1.0 | 1.0.5 | Semver-compatible |
| once_cell | 1.19 | 1.21 | Semver-compatible |
| regex | 1.11 | 1.12 | Semver-compatible |
| unicode-width | 0.2 | 0.2.2 | Semver-compatible |
| semver | 1.0 | 1.0.27 | Semver-compatible |
| tempfile | 3.10 | 3.25 | Semver-compatible |
| assert_cmd | 2.0 | 2.1 | Semver-compatible |
| predicates | 3.1 | 3.1.4 | Semver-compatible |
| criterion | 0.8 | 0.8.2 | Semver-compatible |
| walkdir | 2.4 | 2.5 | Semver-compatible |
| insta | 1.38 | 1.46 | Semver-compatible |
| proptest | 1.6 | 1.10 | Semver-compatible |

## Skipped

| Crate | Version | Reason |
|-------|---------|--------|
| rusqlite | 0.38 | Already at latest stable |
| thiserror | 2.0.18 | Already at latest stable |
| rich_rust | 0.2.0 | Already at latest stable |
| toon_rust | git:788589d | Git dependency, preserving pinned revision |

## Validation

- `cargo check --all-targets`: Pass
- `cargo clippy --all-targets -- -D warnings`: Pass
- `cargo fmt --check`: Pass
- `cargo test --lib`: 775 passed, 0 failed
- `cargo test --test conformance`: 320 passed, 0 failed, 26 ignored
- `cargo test` (full): 1 pre-existing failure in `e2e_close_blocked_requires_force` (unrelated to upgrades)

---

# Upgrade Session: 2026-04-20

**Date:** 2026-04-20  |  **Project:** beads_rust  |  **Language:** Rust

## Summary
- **Updated:** 2 (sibling /dp libraries)  |  **Skipped:** 2  |  **Failed:** 0

## Sibling Project Updates

### rich_rust: 0.2.0 -> 0.2.1
- **Breaking:** None (patch release)
- **Highlights:** Removes nightly-only feature gate (now builds on stable Rust 2024 edition); syntax highlighting fix in `Prompt`/`Select`; dep bumps
- **Tests:** cargo check + `cargo test --lib --all-features` pass (1240 tests)

### toon_rust (tru): 0.2.0 -> 0.2.2
- **Breaking:** None (patch-bump range)
- **Highlights:** RUSTSEC-2026-0009 fix (time 0.3.45 -> 0.3.47 to eliminate stack-exhaustion DoS); MSRV bump to 1.88 (project already at 1.88)
- **Tests:** cargo check + `cargo test --lib --all-features` pass (1240 tests)

## Skipped (already at latest)

| Crate | Local `/dp` version | crates.io | beads_rust pin |
|-------|--------------------|-----------|----------------|
| fsqlite (+ full workspace) | 0.1.2 / 0.1.3 (vfs) | 0.1.2 / 0.1.3 | matches |
| fastmcp-rust | 0.2.1 | 0.2.1 | matches |

## Pre-existing Test Failures (NOT caused by these upgrades)

Confirmed by running the same tests against the HEAD commit with Cargo.toml unchanged:

- `e2e_sync_tombstone_protection` (tests/e2e_basic_lifecycle.rs:1168): tombstone resurrection through a modified JSONL + `sync --import-only --force` is not blocked as the test expects (status flips from `tombstone` to `open`). Flag for follow-up issue; v0.1.44 shipped with this same failure.
- `config::tests::open_storage_with_cli_recovers_malformed_schema_db_{from_valid_jsonl, with_in_progress_issue}`: fail ONLY when local `.cargo/config.toml` patches fsqlite-* to the `/data/projects/frankensqlite` workspace. Both pass cleanly against the crates.io fsqlite 0.1.2 the release pipeline uses.

## Validation

- `cargo check --all-targets --all-features` (crates.io fsqlite): Pass
- `cargo test --lib --all-features` (crates.io fsqlite): 1240 passed, 0 failed
- `cargo test --all-features --tests`: 160 passed, 1 pre-existing failure (see above)
