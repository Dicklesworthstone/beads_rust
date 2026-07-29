# Snapshot Review Log — 2026-05-09

**Operator:** audit-2026-05-09
**Bead:** beads_rust-l6xl ([P1] Refresh insta snapshot goldens after `--slug` feature, PR #283)
**Trigger:** `cargo test --release --test snapshots --test e2e_schema` reported 21 failed snapshot tests; insta wrote `*.snap.new` files alongside.
**Process:** for each delta, classified the cause; only legitimate intentional changes were accepted.

## Per-delta review

| # | Test | Cause class | Accept? | Rationale |
|--:|------|------------|--------:|-----------|
| 1 | `cli_output::create_help` | A | ✓ | `--slug` flag (commit `5c0af3d4 feat(create): --slug for human-readable issue IDs (#283)`); plus benign insta `assertion_line` metadata. |
| 2 | `cli_output::help_output` | C | ✓ | Adds 4 new top-level commands: `capabilities`, `coordination`, `robot-docs`, `scheduler`. Column-width reformatting is automatic from the longer command names. All 4 are legitimate features added between snapshot capture and now. |
| 3 | `error_messages::error_dependency_cycle` | B | ✓ | JSON output format changed from pretty-printed to compact (single-line). Source: `src/output/context.rs::json` uses `serde_json::to_writer` (compact) for streaming-output performance. Same content, different whitespace. |
| 4 | `error_messages::error_invalid_priority` | B | ✓ | Same as #3. |
| 5 | `error_messages::error_issue_not_found` | B | ✓ | Same as #3. |
| 6 | `error_messages::error_self_dependency` | B | ✓ | Same as #3. |
| 7 | `error_messages::update_closed_issue` | B | ✓ | Same as #3. |
| 8 | `history_diff_output::history_diff_json_summary_v0_1_0_to_v0_1_1` | B | ✓ | Same as #3. |
| 9 | `jsonl_format::issues_jsonl_export` | E | ✓ | Only insta `assertion_line` metadata change; content identical. |
| 10 | `json_output::create_json_output` | B | ✓ | Same as #3. |
| 11 | `json_output::list_filtered_json_output` | B | ✓ | Same as #3. |
| 12 | `json_output::list_json_output` | B | ✓ | Same as #3. |
| 13 | `json_output::list_priority_ordering_json_output` | B | ✓ | Same as #3. |
| 14 | `json_output::representative_list_json_output` | B | ✓ | Same as #3 (large diff because the snapshot is a multi-issue list; `81` removed, `3` added is the pretty→compact compression ratio). |
| 15 | `json_output::representative_show_json_output` | B | ✓ | Same as #3 (large diff for the same reason). |
| 16 | `json_output::search_json_output` | B | ✓ | Same as #3. |
| 17 | `json_output::show_json_output` | B | ✓ | Same as #3. |
| 18 | `json_output::show_multiple_ids_json_output` | B | ✓ | Same as #3. |
| 19 | `robot_output::robot_ready_output` | B | ✓ | Same as #3. |
| 20 | `schema_output::schema_all_json_output` | C | ✓ | Adds `capabilities` and `robot-docs guide` schema entries (matching the new commands in #2). |
| 21 | `schema_output::schema_all_toon_output` | C | ✓ | Same as #20 — `capabilities` and `robot-docs guide` schema entries added. |

## Cause classes

- **A** — `--slug` feature (commit `5c0af3d4`): adds a `--slug <SLUG>` option to `br create`. Surfaced in 1 test.
- **B** — JSON format change (pretty → compact): `src/output/context.rs::json` switched from `serde_json::to_string_pretty` to `serde_json::to_writer` for streaming performance. Same content; different whitespace. Surfaced in 13 tests.
- **C** — New commands (`capabilities`, `coordination`, `robot-docs`, `scheduler`): added during the audit window between snapshot capture (mostly 2026-04-21) and now (2026-05-09). Surfaced in 3 tests.
- **E** — Pure insta metadata (`assertion_line`): no semantic change, just insta tracking the source line. Surfaced in 1 test.

## Verdict

All 21 deltas are legitimate intentional changes; **none hides a regression**. Accepting all via `cargo insta accept`.

## Verification commands

```bash
cd /data/projects/beads_rust
RCH_DISABLED=1 cargo test --release --no-fail-fast --test snapshots --test e2e_schema 2>&1 | grep "test result"
# Expected: all-pass after acceptance
```

## Audit-trail context

- `docs/SNAPSHOT_TESTING.md` (new file in `beads_rust-l6xl`) defines the policy: every deliberate output change requires `cargo insta review` + a per-delta review log committed to `docs/snapshot_review_<YYYY>_<MM>_<DD>.md`.
- Future deltas without a corresponding review log are caught by the CI gate added in `beads_rust-6jmq`.
- The `--slug` feature itself is documented in backfill bead `beads_rust-zxuz`.

## Addendum: post-`m3mi` snapshot delta (2026-05-09)

After `beads_rust-m3mi` added the new `audit.suspect_close_reasons` doctor check, the `cli_output::snapshot_doctor_output` golden needed to be updated to include the new check's output line.

| # | Test | Cause class | Accept? | Rationale |
|--:|------|------------|--------:|-----------|
| 22 | `cli_output::snapshot_doctor_output` | C (new doctor check added by `m3mi`) | ✓ | Single additive line `OK audit.suspect_close_reasons` between `sqlite.integrity_check` and `counts.db_vs_jsonl`. Direct consequence of `m3mi`'s production-code addition; output is correct. |
