# Dependency Upgrade Log

**Date:** 2026-01-18  |  **Project:** beads_rust  |  **Language:** Rust

## Summary
- **Updated:** 4  |  **Skipped:** 1  |  **Failed:** 0  |  **Needs attention:** 0

## Updates

### criterion: 0.7.0 → 0.8.1
- **Breaking:** None documented; version bump aligns criterion-plot dependency
- **Migration:** None required
- **Tests:** ✓ Passed (649)

### rusqlite: 0.32.1 → 0.38.0
- **Breaking:** `usize`/`u64` ToSql/FromSql disabled by default; statement cache optional; min SQLite 3.34.1
- **Migration:** Added `fallible_uint` feature flag to re-enable `usize` ToSql support
- **Tests:** ✓ Passed (649)

### unicode-width: 0.1.14 → 0.2.2
- **Breaking:** Control characters now return `Some(1)` instead of `None`
- **Migration:** Code already uses `unwrap_or(0)` which handles the change gracefully
- **Tests:** ✓ Passed (649)

### indicatif: 0.17.11 → 0.18.3
- **Breaking:** None found in project usage
- **Tests:** ✓ Passed (649)

---

## Skipped

### vergen-gix: 1.0.9 → 9.1.0
- **Reason:** Blocked by Rust version constraint; vergen-gix 9.x may require newer Rust than 1.85
- **Action:** Investigate if project can bump rust-version in rust-toolchain.toml, or wait for compat release

---

## Transitive Updates (via cargo update)

These were automatically updated as dependencies of direct dependencies:
- hashlink: 0.9.1 → 0.11.0 (rusqlite dependency)
- libsqlite3-sys: 0.30.1 → 0.36.0 (rusqlite dependency)
- criterion-plot: 0.6.0 → 0.8.1 (criterion dependency)
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
