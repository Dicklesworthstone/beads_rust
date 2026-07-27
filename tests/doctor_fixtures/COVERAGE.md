# Doctor fixture coverage manifest (fm- finding ids → fixtures)

Maps every declared `fm-<subsystem>-<slug>` finding id to the fixture(s)
that exercise it, so coverage drift is visible in review. The mapping was
built empirically (beads_rust-yx0b): each fixture's planted state was run
through `br doctor --json` and the non-ok `details.finding_id` values
recorded; assert-side coverage (special env or flags driven by the
fixture's own `assert.sh`) is marked `assert`.

**Drift gate**: `tests/e2e_doctor_fixture_suite.rs::fixture_coverage_manifest_is_complete`
asserts that (a) every fm id in the `br doctor capabilities` envelope has a
row here, (b) every fixture named here exists on disk, and (c) every
fixture directory on disk is mentioned somewhere in this file. Adding a new
finding id or fixture without updating this manifest fails the gate.

Kinds: `detect` — the planted state fires the finding under plain
`br doctor --json`; `assert` — the fixture's `assert.sh` drives a special
invocation (env override or flag) to pin the finding/check contract;
`exception` — no fixture can fire the id, reason given inline.

| finding id | fixtures | kind |
|---|---|---|
| fm-agent_coordination-suspect-close-reason | audit_suspect_close_reasons | detect |
| fm-agent_coordination-workflow-status-out-of-set | workflow_status_out_of_set | detect |
| fm-caches_indexes-blocked-cache-stale | blocked_cache_stale, blocked_cache_content_mismatch, blocked_cache_table_missing, duplicate_config_rows, duplicate_metadata_rows | detect |
| fm-caches_indexes-comments-orphans | comments_orphans | detect |
| fm-caches_indexes-db-bloat-vs-jsonl | db_bloat | detect |
| fm-caches_indexes-dependencies-orphans | dependencies_orphans | detect |
| fm-caches_indexes-dirty-bitmap-divergence | dirty_bitmap_orphans | detect |
| fm-caches_indexes-export-hash-cache-divergence | export_hash_cache_divergence, duplicate_metadata_rows | detect |
| fm-caches_indexes-labels-orphans | labels_orphans | detect |
| fm-caches_indexes-partial-index-stale | — | exception: fixer-scope id only (gates the REINDEX repair path via the fixer filter); no check emits it as a finding, so no fixture can fire it. Advertised in the capabilities envelope via `fixers[].filter_ids` (beads_rust-oow2) |
| fm-concurrency_primitives-orphaned-write-lock | orphaned_write_lock, mcp_serve_stale_write_lock | assert: the probe contract (GH #395) classifies a free stale-mtime lock as ok via a non-blocking flock probe under `BR_DOCTOR_STALE_LOCK_THRESHOLD_SECS=0`; the warn path (`stale_unprobed`) is unreachable on a workspace whose doctor startup succeeds, because an unopenable lock degrades startup first (see permissions_write_lock_unwritable) |
| fm-configs-gitignore-leaking-beads | gitignore_leaking_beads, gitignore_bare_pattern, inner_gitignore_append | detect |
| fm-configs-metadata-json-stale | metadata_json_drift, metadata_json_malformed | detect |
| fm-configs-startup-cache-poisoned | startup_cache_poisoned | detect |
| fm-configs-yaml-malformed | config_yaml_malformed | detect |
| fm-dependencies-dead-closed-blocking-edges | dep_dead_closed_blocking_edges | detect |
| fm-dependencies-fully-unblocked-open-issues | dep_dead_closed_blocking_edges | detect |
| fm-external_artifacts-binary-version-mismatch | binary_version_mismatch | detect |
| fm-external_artifacts-multiple-br-in-path | multiple_br_in_path | detect: the fixture builds its own $PATH with two `br` entries; on dual-install developer hosts the finding also fires ambiently in every fixture |
| fm-observability-doctor-runs-dir-grows-unbounded | doctor_runs_dir_growth | detect |
| fm-observability-rust-log-noisy-breaks-json | rust_log_noisy_breaks_json | assert: the fixture's assert.sh runs doctor under a deliberately noisy RUST_LOG; the harness itself pins RUST_LOG=error, so the finding cannot fire under the plain run |
| fm-permissions-beads-dir-readonly | permissions_beads_dir_readonly | detect |
| fm-permissions-config-yaml-mode-leaks-secrets | config_yaml_secret_mode | detect |
| fm-permissions-doctor-runs-not-creatable | doctor_runs_not_creatable | detect |
| fm-permissions-gitignore-not-writable-blocks-repair | root_gitignore_not_writable | detect |
| fm-permissions-jsonl-world-writable | jsonl_world_writable | detect |
| fm-permissions-recovery-dir-not-writable | recovery_dir_not_writable | detect |
| fm-routes_external-route-target-missing | routes_target_missing, routes_jsonl_corrupt | detect |
| fm-routes_external-routes-jsonl-corrupt | routes_jsonl_corrupt | detect |
| fm-schemas-issue-column-order-divergence | — | exception: only emitted via the `schema.inspect` inspection-error path (schema checks failed to RUN, not a detected column-order divergence); no deterministic real-world plant reaches it without first tripping db.open/schema.tables instead |
| fm-schemas-missing-required-column | schemas_missing_required_column, empty_database_with_jsonl | detect |
| fm-schemas-missing-required-table | schemas_missing_required_table, blocked_cache_table_missing, empty_database_with_jsonl | detect |
| fm-state_files-base-jsonl-missing-or-stale | base_jsonl_missing_post_flush, base_jsonl_stale_regen, base_jsonl_symlink_quarantine | detect |
| fm-state_files-br-history-grows-unbounded | br_history_growth | detect |
| fm-state_files-dirty-flag-divergence | dirty_flag_divergence, dirty_bitmap_orphans | detect |
| fm-state_files-empty-or-truncated-database | db_missing_with_jsonl | detect |
| fm-state_files-jsonl-conflict-markers | jsonl_conflict_markers | detect |
| fm-state_files-jsonl-crlf-line-endings | jsonl_crlf_to_lf | detect |
| fm-state_files-jsonl-duplicate-ids | jsonl_duplicate_ids | detect |
| fm-state_files-jsonl-malformed-utf8 | jsonl_malformed_utf8, jsonl_bom_strip | detect |
| fm-state_files-jsonl-missing-trailing-newline | jsonl_trailing_newline | detect |
| fm-state_files-jsonl-oversized | jsonl_oversized | detect |
| fm-state_files-jsonl-row-count-mismatch | jsonl_row_count_mismatch | detect |
| fm-state_files-jsonl-utf8-bom-prefix | jsonl_bom_strip | detect |
| fm-state_files-merge-artifact-stuck | merge_artifact_stuck | detect |
| fm-state_files-no-db-mode-db-checks-skipped | no_db_mode_marker | assert: the fixture's assert.sh runs `br doctor --no-db --json` and pins the ok-status marker check + finding id; a full run must not carry the marker |
| fm-state_files-orphan-tmp-files | orphan_tmp_quarantine | detect |
| fm-state_files-orphaned-write-lock | permissions_write_lock_unwritable | detect: env-skip protocol (exit 3) on hosts where permission bits do not bind (root / CAP_DAC_OVERRIDE) |
| fm-state_files-recovery-artifacts-orphaned | recovery_artifacts_orphaned, recovery_artifacts_aged | detect |
| fm-state_files-sqlite-page-malformed | sqlite_page_malformed, doctor_mutates_without_fix | detect |
| fm-state_files-wal-oversized | wal_oversized_checkpoint | detect |
| fm-state_files-wal-shm-sidecar-orphan | orphan_shm_sidecar, wal_without_shm | detect: orphan_shm_sidecar fires the error; wal_without_shm pins the tolerated ok-path (WAL without SHM is legal after a clean close) |

## Fixtures without a dedicated fm- finding id

Meta-fixtures that pin contracts other than a specific finding; listed so
the reverse direction of the drift gate (every fixture dir is mentioned
here) stays complete:

- `healthy_workspace_baseline` — asserts the ABSENCE of findings on a
  freshly initialized workspace (the false-positive gate).
- `sqlite_version_downgrade` — pins the engine-version downgrade check's
  tolerated path; the check has no fm mapping in
  `CHECK_NAME_TO_FINDING_ID`.
- `doctor_mutates_without_fix` — meta-contract: a detect-only doctor run
  must not mutate the workspace (also listed above under
  fm-state_files-sqlite-page-malformed, whose corruption it reuses).
- `empty_database_with_jsonl` — plants a present-but-schema-empty DB;
  primary rows above: missing-required-table/column.
