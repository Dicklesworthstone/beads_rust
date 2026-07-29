# Snapshot Testing Policy

This project uses [insta](https://insta.rs/) for snapshot tests. Snapshot tests are powerful — they freeze a known-good output and catch every regression — but they fail loudly when a deliberate, intentional change happens. This document explains how to handle that without burying real regressions under blanket-accept commits.

## TL;DR

When `cargo test --test snapshots` reports a snapshot failure:

1. Run `cargo insta review` and inspect each delta one by one.
2. For each delta, decide: is this a **deliberate, documented change** or a **regression**?
3. **Document every accepted delta** in `docs/snapshot_review_<YYYY>_<MM>_<DD>.md` with one row per delta — file, classification, accept-or-reject, rationale.
4. **Commit the review log** alongside the snapshot deltas. Future audits use these logs to verify intent.
5. CI gate (`beads_rust-6jmq`) fails the build if `*.snap.new` files are present without a corresponding review log.

## Why we don't blanket-accept

A snapshot drift can hide:

- A regression in a code path that's behind a feature flag (passes other tests, fails snapshot).
- A subtle change to JSON field ordering that breaks downstream parsers.
- An unintended change to error-message wording that customer support has been training agents on.
- A new field accidentally exposed in a `--robot` envelope that should have been gated by feature flag.

The cost of mistaking one of these for a deliberate change is paid silently for weeks. The cost of one extra `git commit -m "audit log"` is paid once, immediately, in plain sight.

## Cause classes (from the 2026-05-09 audit)

When categorizing each delta, prefer one of these labels for consistency across review logs:

- **A**: Feature addition (new CLI flag, new field, new option).
- **B**: Output format change (pretty → compact, ordering changed by a downstream library).
- **C**: New command or subcommand added.
- **D**: Test-fixture change (intentional, e.g., new tempdir-name normalization).
- **E**: Pure insta metadata (`assertion_line:` etc.) — semantic content unchanged.
- **R**: REGRESSION — a real bug surfaced by the snapshot. **Do not accept** until fixed.

## Determinism: why some snapshots leak non-deterministic values

Snapshots that capture `source_repo`, `created_at`, `created_by`, content_hashes, or generated IDs need normalization in `tests/snapshots/mod.rs::normalize_json` to remain stable across runs. The 2026-05-09 audit added `source_repo` normalization (`SOURCE_REPO` token); any future snapshot that captures a tempdir-rooted path needs the same treatment.

If a snapshot fails on the second run (after acceptance) with a different value than the accepted one, that's a **leaky non-determinism**, not a deliberate change. Add normalization to `normalize_json` before accepting.

## CI gate (post-`beads_rust-6jmq`)

The CI workflow runs `cargo insta test --check`. If any `*.snap.new` files would be written, CI fails. To pass CI:

1. Run `cargo insta review` locally.
2. Accept the deltas you intend.
3. Write `docs/snapshot_review_<DATE>.md` documenting each.
4. Commit BOTH the snapshot files AND the review log.

## Per-delta review log template

```markdown
# Snapshot Review Log — YYYY-MM-DD

**Operator:** <your-actor-name>
**Bead:** <bead-id> (<bead-title>)
**Trigger:** <what command/test reported the failure>

## Per-delta review

| # | Test | Cause class | Accept? | Rationale |
|--:|------|------------|--------:|-----------|
| 1 | `<test_name>` | A/B/C/D/E/R | ✓ or ✗ | <one-sentence rationale> |
| 2 | ... | ... | ... | ... |

## Verdict

<Summary: how many accepted, how many rejected, any regressions found>

## Verification commands

```bash
cargo test --release --test snapshots
```
```

## Audit-trail context

- 2026-05-09 audit (bead `beads_rust-l6xl`) added the policy after PR #283 (`--slug`) shipped without snapshot refresh and accumulated 21 stale `.snap` files alongside the live ones.
- Past audit logs live in `docs/snapshot_review_<DATE>.md`.

## Related skills (Claude Code)

- [`testing-golden-artifacts`](https://...) — the broader pattern this policy implements.
- The CI gate referenced by this doc lives in bead `beads_rust-6jmq`.
