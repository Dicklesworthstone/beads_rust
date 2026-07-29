# Audit: ID-Pinning Anti-Pattern Project-Wide Sweep (2026-05-09)

**Operator:** audit-2026-05-09
**Bead:** beads_rust-lnqc
**Trigger:** Sibling bead `beads_rust-jsgu` discovered the ID-pinning anti-pattern in `tests/storage_ready.rs` and proposed this project-wide sweep to find/migrate other instances.

## TL;DR

**Project-wide audit found the anti-pattern is RARE in this codebase.** A grep for the headline pattern `assert_eq!(<actual>, "<prefix>-<hash>")` returned only **10 hits across 5 files**, and **all 10 are benign** (parser tests, fixture round-trips, normalization-rule output assertions, or hardcoded actor names — not generated-ID pinning).

The anti-pattern was localized to the 3 `tests/storage_ready.rs::ready_sort_policy_*` tests that `jsgu` already fixed.

## Methodology

```bash
rg --no-heading -n 'assert_eq!\([^,]*,\s*"(test|tmp|br|bd|proj)-[a-zA-Z0-9]{4,}"' tests/
```

This finds any `assert_eq!(<lhs>, "<prefix>-<suffix>")` where the suffix has 4+ alphanumeric characters and the prefix matches one of the project's known ID prefixes.

## Per-hit classification

| # | File | Line | Excerpt | Category | Action |
|---|------|-----:|---------|----------|--------|
| 1 | `tests/common/harness.rs` | 1523 | `assert_eq!(id, "bd-abc123")` | Parser test (`test_parse_created_id`): input is the literal string, asserting parser extracts correctly | KEEP — not ID-pinning |
| 2 | `tests/common/scenarios.rs` | 3307 | `assert_eq!(value["id"], "bd-HASH")` | NormalizationRules test: input is `"bd-abc123"`, rule masks to `"bd-HASH"` placeholder, assertion verifies the mask | KEEP — `bd-HASH` is the rule's documented placeholder, not a generated ID |
| 3 | `tests/common/scenarios.rs` | 3848 | `assert_eq!(value["id"], "bd-HASH")` | Same as #2 | KEEP |
| 4 | `tests/common/scenarios.rs` | 3849 | `assert_eq!(value["parent_id"], "bd-HASH")` | Same as #2 | KEEP |
| 5 | `tests/common/scenarios.rs` | 3850 | `assert_eq!(value["blocked_by_id"], "bd-HASH")` | Same as #2 | KEEP |
| 6 | `tests/common/scenarios.rs` | 3893 | `assert_eq!(value["id"], "bd-abc123")` | Inverse rule test: when `normalize_ids` is false, input ID `"bd-abc123"` should pass through unchanged | KEEP — fixture round-trip, not generated-ID pinning |
| 7 | `tests/e2e_audit.rs` | 154 | `assert_eq!(entries[0]["issue_id"], "bd-test123")` | Audit log round-trip: test creates an audit entry tagged with issue_id="bd-test123" and verifies it round-trips | KEEP — fixture round-trip |
| 8 | `tests/e2e_audit.rs` | 927 | `assert_eq!(entries[0]["actor"], "test-agent")` | Actor-name assertion (NOT an issue ID) | KEEP — actor name, not ID-pinning |
| 9 | `tests/e2e_comments.rs` | 170 | `assert_eq!(comment["author"], "test-user")` | Author-name assertion (NOT an issue ID) | KEEP — author name, not ID-pinning |
| 10 | `tests/e2e_coordination.rs` | 571 | `assert_eq!(entries[0]["issue_id"], "bd-stale")` | Coordination test: fixture creates a record with issue_id="bd-stale" and verifies round-trip | KEEP — fixture round-trip |

## Conclusion

The project-wide audit found **0 true ID-pinning anti-patterns** in the test suite (post-jsgu cleanup). The pattern uncovered by `jsgu` was localized to the 3 ready_sort_policy_* tests, which have already been migrated to use `tests/common/ordering.rs` invariant-based assertions.

The hits found by the regex sweep are all legitimate test patterns:
- **Parser tests** that hardcode an ID string as input and assert correct extraction.
- **Normalization-rule tests** that hardcode `"bd-HASH"` as the documented placeholder output.
- **Fixture round-trips** that pre-create an entity with a known ID, then assert the ID round-trips through some pipeline.
- **Actor/user names** (not IDs) like `"test-agent"`, `"test-user"`.

These should NOT be migrated; doing so would weaken the tests.

## Lint enforcement

The sibling bead `beads_rust-s5se` adds a CI lint that runs the same regex but excludes lines annotated with a sentinel comment (`// invariant: ...` or similar). With this audit's confirmation that **no migrations are needed**, the lint can be added without any code churn — it will pass cleanly on the current state and only fail on FUTURE introductions of the anti-pattern.

## Reproduction

```bash
cd /data/projects/beads_rust
rg --no-heading -n 'assert_eq!\([^,]*,\s*"(test|tmp|br|bd|proj)-[a-zA-Z0-9]{4,}"' tests/ | wc -l
# Expected: 10 (or fewer if any of the 10 are removed by future work)
```
