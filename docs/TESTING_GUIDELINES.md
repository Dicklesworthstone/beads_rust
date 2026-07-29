# Testing Guidelines

Practical conventions for tests in `beads_rust`. Created 2026-05-09 as part of the audit-driven cleanup; the rules here are enforced by tests in `tests/no_id_pinning.rs` and the patterns in `tests/common/ordering.rs`.

## Don't pin generated content-hash IDs in assertions

**Anti-pattern:**

```rust
let issues = ready_via_storage(&storage);
assert_eq!(issues[0].id, "test-c75c9ac8"); // ❌ FRAGILE
```

This breaks whenever:
- The hash function changes.
- A fixture's title is rephrased.
- The DB inserts at a different timestamp.
- An `id`-tiebreak in `ORDER BY` re-shuffles equal-priority results.

The only invariant you actually want to assert is the **relative ordering**, not the specific IDs.

**Use the helpers in `tests/common/ordering.rs` instead:**

```rust
use common::ordering::{
    assert_priority_ordered,
    assert_oldest_first,
    assert_hybrid_ordered,
    assert_no_duplicate_ids,
    assert_contains_exactly_one,
};

let issues = ready_via_storage(&storage);
assert_priority_ordered(&issues);          // P0 < P1 < P2 < ...
assert_no_duplicate_ids(&issues);          // every issue exactly once
```

For the full list of helpers and their semantics, read `tests/common/ordering.rs`'s rustdoc.

## Legitimate exceptions (annotate)

Some patterns LOOK like ID-pinning but are legitimate. The lint at `tests/no_id_pinning.rs` allows them when annotated with `// invariant: <reason>`:

```rust
// Parser test: input is the literal string "bd-abc123"; the assertion
// verifies the parser extracts it correctly.
assert_eq!(parse_created_id("Created bd-abc123: foo"), "bd-abc123");
// invariant: parser test, not ID-pinning

// Normalization rule: rule masks any ID to "bd-HASH"; the assertion
// verifies the mask, not a generated ID.
let mut value = json!({ "id": "bd-abc123" });
rules.apply(&mut value);
assert_eq!(value["id"], "bd-HASH");
// invariant: NormalizationRules placeholder, not a generated ID

// Fixture round-trip: the test sets the ID, runs through some pipeline,
// asserts it round-trips unchanged.
let bead = Bead { id: "bd-test123".into(), .. };
storage.create(&bead)?;
assert_eq!(storage.get_id_for(bead.title), "bd-test123");
// invariant: fixture round-trip, ID is set by the test

// Actor / author name (not a generated issue ID).
assert_eq!(audit_entry["actor"], "test-agent");
// invariant: hardcoded actor name, not an issue ID
```

Pre-known legitimate hits (from `docs/audit_id_pinning_2026_05_09.md`) are baked into `tests/no_id_pinning.rs::KNOWN_LEGITIMATE_HITS`; if you move one of those lines, update the `KNOWN_LEGITIMATE_HITS` constant accordingly.

## Use deterministic test fixtures

For test fixtures that need a fixed ID, set the ID explicitly via the builder; don't rely on auto-generation:

```rust
let issue = fixtures::IssueBuilder::new("Test issue")
    .with_id("br-fixed-id")
    .build();
storage.create_issue(&issue, "tester")?;
// Now assertions can reference "br-fixed-id" knowing it's a fixture, not a generated hash.
```

## Use the SOURCE_REPO normalization for snapshot tests

Snapshot tests (`tests/snapshots.rs`) use `tests/snapshots/mod.rs::normalize_json` to mask non-deterministic values:

- `id`, `issue_id`, `depends_on_id`, `blocks_id` → `"ISSUE_ID"`
- `created_at`, `updated_at`, `closed_at`, etc. → `"TIMESTAMP"`
- `content_hash` → `"HASH"`
- `created_by`, `assignee`, `owner`, etc. → `"ACTOR"`
- `source_repo` → `"SOURCE_REPO"` (added 2026-05-09 by `beads_rust-l6xl`)

If a snapshot includes a new non-deterministic field, add it to `normalize_json` BEFORE accepting the snapshot.

## Use the ordering / determinism helpers

| Helper | Purpose |
|--------|---------|
| `assert_priority_ordered(&issues)` | P0 < P1 < P2 < ... ordering invariant |
| `assert_oldest_first(&issues)` | created_at ASC ordering invariant |
| `assert_hybrid_ordered(&issues)` | high-tier (P≤1) before low-tier (P>1) |
| `assert_no_duplicate_ids(&issues)` | no ID appears twice |
| `assert_contains_exactly_one(&issues, pred, name)` | exactly one matches predicate |

These are in `tests/common/ordering.rs` (added by `beads_rust-jsgu`).

## Naming conventions

- `e2e_*.rs` — full CLI subprocess tests against a real workspace.
- `repro_*.rs` — regression tests for specific bug IDs.
- `proptest_*.rs` — property-based tests using the `proptest` crate.
- `conformance_*.rs` — schema / output-shape conformance against documented contracts.
- `storage_*.rs` — direct storage-layer tests.
- `tests/e2e_scripts/*.sh` — shell harness that drives the released binary; emits structured JSONL event logs to `/tmp/<harness>_<UTC-ts>.jsonl`.

## CI gates (post-`beads_rust-6jmq`)

Once `beads_rust-6jmq` lands, the CI pipeline runs:

- `cargo test --release --no-fail-fast` (full suite, including `no_id_pinning` lint)
- `cargo clippy --all-targets -- -D warnings`
- `cargo fmt --check`
- `cargo insta test --check` (snapshot freshness — `beads_rust-l6xl`)
- `bash tests/e2e_scripts/forced_cycle_close_audit.sh` (`beads_rust-30ci`)
- `bash tests/e2e_scripts/sync_safety_witness.sh` (`beads_rust-yyxo`)

Pass-or-fail of those gates blocks merge. Document new gates here when adding them.

## Audit-driven test additions (2026-05-09)

| Bead | What it added |
|------|---------------|
| `yyxo` | 5 unit tests + 4 e2e tests + 4 property tests + `sync_safety_witness.sh` (sync allowlist + recovery PC-RECOVERY invariant) |
| `l6xl` | 4 unit tests for `--slug` normalization + 2 e2e tests + `slug_round_trip.sh` + `SOURCE_REPO` normalization in `normalize_json` |
| `uelt` | 3 single-parent contract tests + 4 dep-type matrix tests + `proptest_parent_child.rs` + `parent_child_lifecycle.sh` |
| `jsgu` | 3 invariant-based ready-sort rewrites + `tests/common/ordering.rs` helper module + 2 e2e tests + `ready_ordering_witness.sh` |
| `750p` | `orphans_with_logging.sh` + auto-import-before-scan production fix |
| `mjmk` | extended fixture in `e2e_concurrency.rs` + `concurrency_witness_logged.sh` 50-iter no-flake harness |
| `44rc` | semantic JSON parse rewrite + `markdown_import_witness.sh` |
| `30ci` | `forced_cycle_close_audit.sh` + AGENTS.md policy section |
| `s5se` | `tests/no_id_pinning.rs` lint test |
| `lnqc` | `docs/audit_id_pinning_2026_05_09.md` (audit found 0 true ID-pinning hits) |
| `6plg` | `docs/audit_bd_to_br_2026_05_09.md` (audit found migration essentially complete) |
