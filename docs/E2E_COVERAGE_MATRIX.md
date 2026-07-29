# E2E Coverage Matrix - br CLI Commands

> Single source of truth for CLI command coverage and E2E scenario mapping.
> Generated for beads_rust-rkuz; refreshed for beads_rust-9ow6.1 on 2026-05-08.

## Overview

| Category | Total Commands | Covered | Gaps | Coverage % |
|----------|----------------|---------|------|------------|
| Core CRUD | 8 | 7 | 1 | 88% |
| Querying | 9 | 9 | 0 | 100% |
| Dependencies | 5 | 5 | 0 | 100% |
| Labels | 5 | 5 | 0 | 100% |
| Comments | 2 | 2 | 0 | 100% |
| Epics | 2 | 2 | 0 | 100% |
| Sync | 1 | 1 | 0 | 100% |
| Config | 6 | 6 | 0 | 100% |
| Diagnostics | 6 | 5 | 1 | 83% |
| History | 4 | 4 | 0 | 100% |
| Queries (Saved) | 4 | 4 | 0 | 100% |
| Audit | 2 | 2 | 0 | 100% |
| Special | 7 | 5 | 2 | 71% |
| **TOTAL** | **61** | **57** | **4** | **93%** |

---

## Command Categories

### Legend

| Symbol | Meaning |
|--------|---------|
| ✅ | Covered by E2E tests |
| 🔶 | Partial coverage (some flags untested) |
| ❌ | No E2E coverage |
| 📖 | Read-only command |
| ✏️ | Mutating command |
| 🌐 | Network/external dependency |
| ⚠️ | Destructive operation |

---

## 1. Core CRUD Operations ✏️

| Command | Flags | Mutating | Test File(s) | Status |
|---------|-------|----------|--------------|--------|
| `init` | `--prefix`, `--force`, `--backend` | ✏️ | `e2e_basic_lifecycle.rs` | ✅ |
| `create` | `--title`, `--type`, `--priority`, `--description`, `--assignee`, `--owner`, `--labels`, `--parent`, `--deps`, `--estimate`, `--due`, `--defer`, `--external-ref`, `--ephemeral`, `--dry-run`, `--silent`, `--file` | ✏️ | `e2e_basic_lifecycle.rs`, `e2e_create_output.rs` | ✅ |
| `q` (quick) | `--priority`, `--type`, `--labels` | ✏️ | `e2e_quick_capture.rs` | ✅ |
| `update` | `--title`, `--description`, `--design`, `--acceptance-criteria`, `--notes`, `--status`, `--priority`, `--type`, `--assignee`, `--owner`, `--claim`, `--due`, `--defer`, `--estimate`, `--add-label`, `--remove-label`, `--set-labels`, `--parent`, `--external-ref`, `--session` | ✏️ | `e2e_basic_lifecycle.rs` | ✅ |
| `close` | `--reason`, `--force`, `--suggest-next`, `--session`, `--robot` | ✏️ | `e2e_basic_lifecycle.rs`, `e2e_epic.rs` | ✅ |
| `reopen` | `--reason`, `--robot` | ✏️ | `e2e_basic_lifecycle.rs` | ✅ |
| `delete` | `--reason`, `--from-file`, `--cascade`, `--force`, `--hard`, `--dry-run` | ✏️ ⚠️ | `e2e_basic_lifecycle.rs`, `e2e_errors.rs` | 🔶 |
| `show` | positional IDs | 📖 | `e2e_basic_lifecycle.rs` | ✅ |

**Notes:**
- `create --file` (markdown import) tested in `markdown_import.rs`
- `delete --cascade` still needs an explicit scenario; dependent safety and dry-run behavior are covered in `e2e_errors.rs`.

---

## 2. Querying & Filtering 📖

| Command | Key Flags | Mutating | Test File(s) | Status |
|---------|-----------|----------|--------------|--------|
| `list` | `--status`, `--type`, `--assignee`, `--unassigned`, `--id`, `--label`, `--label-any`, `--priority`, `--priority-min`, `--priority-max`, `--title-contains`, `--desc-contains`, `--notes-contains`, `--all`, `--limit`, `--sort`, `--reverse`, `--deferred`, `--overdue`, `--long`, `--pretty`, `--format`, `--fields` | 📖 | `e2e_basic_lifecycle.rs`, `e2e_list_priority.rs`, `storage_list_filters.rs` | ✅ |
| `ready` | `--limit`, `--assignee`, `--unassigned`, `--label`, `--label-any`, `--type`, `--priority`, `--sort`, `--include-deferred`, `--robot` | 📖 | `e2e_ready.rs`, `e2e_ready_limit.rs`, `storage_ready.rs` | ✅ |
| `blocked` | `--limit`, `--detailed`, `--type`, `--priority`, `--label`, `--robot` | 📖 | `conformance.rs` | ✅ |
| `search` | positional query + all list filters | 📖 | `e2e_basic_lifecycle.rs`, `storage_list_filters.rs` | ✅ |
| `count` | `--by`, `--status`, `--type`, `--priority`, `--assignee`, `--unassigned`, `--include-closed`, `--include-templates`, `--title-contains` | 📖 | `conformance.rs` | ✅ |
| `stale` | `--days`, `--status` | 📖 | `conformance.rs` | ✅ |
| `graph` | positional ID, `--all`, `--compact` | 📖 | `e2e_graph.rs`, `e2e_graph_ordering.rs` | ✅ |
| `stats` | `--json`, `--no-activity` | 📖 | `e2e_stats.rs`, `conformance.rs` | ✅ |
| `status` | alias for `stats` | 📖 | `e2e_concurrency.rs` | ✅ |

---

## 3. Dependencies ✏️/📖

| Command | Key Flags | Mutating | Test File(s) | Status |
|---------|-----------|----------|--------------|--------|
| `dep add` | `--type`, `--metadata` | ✏️ | `e2e_basic_lifecycle.rs`, `storage_deps.rs` | ✅ |
| `dep remove` | - | ✏️ | `storage_deps.rs` | ✅ |
| `dep list` | `--direction`, `--type` | 📖 | `storage_deps.rs` | ✅ |
| `dep tree` | `--max-depth`, `--format` | 📖 | `repro_dep_tree.rs`, `e2e_dep_tree_mermaid.rs`, `e2e_relations.rs` | ✅ |
| `dep cycles` | `--blocking-only` | 📖 | `storage_deps.rs` | ✅ |

**Notes:**
- `dep tree --format=mermaid` is covered by `e2e_dep_tree_mermaid.rs`.

---

## 4. Labels ✏️/📖

| Command | Key Flags | Mutating | Test File(s) | Status |
|---------|-----------|----------|--------------|--------|
| `label add` | `--label` | ✏️ | `e2e_labels.rs`, `conformance_labels_comments.rs` | ✅ |
| `label remove` | `--label` | ✏️ | `e2e_labels.rs` | ✅ |
| `label list` | positional ID | 📖 | `e2e_labels.rs` | ✅ |
| `label list-all` | - | 📖 | `e2e_labels.rs`, `conformance_labels_comments.rs`, `snapshots/json_output.rs` | ✅ |
| `label rename` | positional old/new | ✏️ | `e2e_labels.rs` | ✅ |

**Notes:**
- Both label coverage gaps from the original matrix now have dedicated E2E coverage.

---

## 5. Comments ✏️/📖

| Command | Key Flags | Mutating | Test File(s) | Status |
|---------|-----------|----------|--------------|--------|
| `comments add` | `--file`, `--author`, `--message` | ✏️ | `e2e_comments.rs`, `e2e_comments_stdin.rs`, `conformance_labels_comments.rs` | ✅ |
| `comments list` | positional ID | 📖 | `e2e_comments.rs` | ✅ |

---

## 6. Epics ✏️/📖

| Command | Key Flags | Mutating | Test File(s) | Status |
|---------|-----------|----------|--------------|--------|
| `epic status` | `--eligible-only` | 📖 | `e2e_epic.rs`, `repro_epic_blocking.rs` | ✅ |
| `epic close-eligible` | `--dry-run` | ✏️ | `e2e_epic.rs` | ✅ |

---

## 7. Sync ✏️

| Command | Key Flags | Mutating | Test File(s) | Status |
|---------|-----------|----------|--------------|--------|
| `sync` | `--flush-only`, `--import-only`, `--merge`, `--status`, `--witness`, `--reconcile-additive`, `--apply`, `--expect-plan-sha256`, `--force`, `--force-db`, `--force-jsonl`, `--rename-prefix`, `--rebuild`, `--allow-external-jsonl`, `--manifest`, `--error-policy`, `--orphans`, `--robot` | ✏️ | `e2e_sync_artifacts.rs`, `e2e_sync_failure_injection.rs`, `e2e_sync_fuzz_edge_cases.rs`, `e2e_sync_git_safety.rs`, `e2e_sync_status_health.rs`, `e2e_sync_preflight_integration.rs`, `e2e_basic_lifecycle.rs`, `jsonl_import_export.rs` | ✅ |
| `vcs-status` | `--jsonl`, `--allow-external-jsonl`, `--timeout-ms`, `--robot`, global JSON/TOON | 📖 | `e2e_vcs_status.rs`, VCS runner unit tests | ✅ |

**Safety-critical test files:**
- `e2e_sync_git_safety.rs` - verifies no git operations
- `e2e_sync_preflight_integration.rs` - validates conflict markers rejected

---

## 8. Configuration ✏️/📖

| Command | Key Flags | Mutating | Test File(s) | Status |
|---------|-----------|----------|--------------|--------|
| `config list` | `--project`, `--user` | 📖 | `e2e_config_precedence.rs` | ✅ |
| `config get` | positional key | 📖 | `e2e_config_precedence.rs` | ✅ |
| `config set` | positional key=value | ✏️ | `e2e_config_precedence.rs`, `e2e_env_overrides.rs`, `e2e_workspace_commands.rs` | ✅ |
| `config delete`/`unset` | positional key | ✏️ | `e2e_config_precedence.rs`, `e2e_routing.rs` | ✅ |
| `config edit` | - | ✏️ | `e2e_workspace_commands.rs` | ✅ |
| `config path` | - | 📖 | `e2e_queries.rs`, `e2e_workspace_scenarios.rs`, `e2e_global_flags.rs` | ✅ |

**Notes:**
- Config mutation and path coverage has been added since the original matrix was generated.

---

## 9. Diagnostics 📖

| Command | Key Flags | Mutating | Test File(s) | Status |
|---------|-----------|----------|--------------|--------|
| `doctor` | diagnostics, `--repair`, `migrate-schema plan/apply/undo`, migration `--dry-run` | ✏️ | `e2e_basic_lifecycle.rs`, `e2e_doctor_chokepoint.rs`, `conformance.rs`, focused schema-migration unit tests | ✅ |
| `info` | `--schema`, `--whats-new`, `--thanks` | 📖 | `e2e_workspace_scenarios.rs`, `e2e_env_overrides.rs`, `e2e_global_flags.rs`, `conformance.rs` | 🔶 |
| `where` | - | 📖 | `e2e_basic_lifecycle.rs` | ✅ |
| `version` | - | 📖 | `e2e_basic_lifecycle.rs` | ✅ |
| `lint` | positional IDs, `--type`, `--status` | 📖 | `e2e_lint.rs` | ✅ |
| `schema` | target, `--format` | 📖 | `e2e_schema.rs`, `snapshots/schema_output.rs` | ✅ |

**Gaps:**
- `info --schema` still needs explicit E2E coverage. `--whats-new` and `--thanks` have quiet/JSON/TOON coverage.

---

## 10. History ✏️/📖

| Command | Key Flags | Mutating | Test File(s) | Status |
|---------|-----------|----------|--------------|--------|
| `history list` | - | 📖 | `e2e_history.rs`, `e2e_history_custom_path.rs` | ✅ |
| `history diff` | positional file | 📖 | `e2e_history.rs` | ✅ |
| `history restore` | `--force` | ✏️ | `e2e_history.rs`, `e2e_history_restore_prune.rs` | ✅ |
| `history prune` | `--keep`, `--older-than` | ✏️ ⚠️ | `e2e_history.rs`, `e2e_history_restore_prune.rs`, `e2e_env_overrides.rs` | ✅ |

**Notes:**
- Restore and prune now have focused destructive-path coverage in isolated workspaces.

---

## 11. Saved Queries ✏️/📖

| Command | Key Flags | Mutating | Test File(s) | Status |
|---------|-----------|----------|--------------|--------|
| `query save` | `--description` + list filters | ✏️ | `e2e_queries.rs` | ✅ |
| `query run` | positional name + list filters | 📖 | `e2e_queries.rs` | ✅ |
| `query list` | - | 📖 | `e2e_queries.rs` | ✅ |
| `query delete` | positional name | ✏️ | `e2e_queries.rs` | ✅ |

---

## 12. Audit ✏️/📖

| Command | Key Flags | Mutating | Test File(s) | Status |
|---------|-----------|----------|--------------|--------|
| `audit record` | `--kind`, `--issue-id`, `--model`, `--prompt`, `--response`, `--tool-name`, `--exit-code`, `--error`, `--stdin` | ✏️ | `e2e_audit.rs` | ✅ |
| `audit label` | `--label`, `--reason` | ✏️ | `e2e_audit.rs` | ✅ |

---

## 13. Special Commands

| Command | Key Flags | Mutating | Test File(s) | Status |
|---------|-----------|----------|--------------|--------|
| `defer` | `--until`, `--robot` | ✏️ | `e2e_defer.rs` | ✅ |
| `undefer` | `--robot` | ✏️ | `e2e_undefer.rs` | ✅ |
| `orphans` | `--details`, `--fix`, `--robot` | ✏️/📖 | `e2e_orphans.rs` | ✅ |
| `changelog` | `--since`, `--since-tag`, `--since-commit`, `--robot` | 📖 | `e2e_changelog.rs` | ✅ |
| `completions` | positional shell, `--output` | 📖 | `e2e_completions.rs` | ✅ |
| `upgrade` | `--check`, `--force`, `--version`, `--dry-run` | 🌐 ⚠️ | `e2e_upgrade.rs` | 🔶 |
| `agents` | AGENTS.md workflow helpers | 📖/✏️ | - | ❌ |

**Notes:**
- `upgrade` requires `self_update` feature and network; tests are guarded
- `agents` is a newer top-level command and needs first-class E2E coverage.

---

## Gap Summary

### Active Gaps

1. **`agents`** - Newer top-level AGENTS.md workflow command with no E2E row yet.
2. **`info --schema`** - `info`, `--whats-new`, and `--thanks` are covered; schema details are not explicit.
3. **`delete --cascade`** - Dependent safety and dry-run output are covered, but no explicit cascade execution scenario.

### Guarded Or Partial

4. **`upgrade`** - Guarded by feature/network requirements; current tests cover safe paths.

### Cleared Since beads_rust-rkuz

- `doctor`
- `config set/delete/edit/path`
- `label list-all`
- `label rename`
- `history restore/prune`
- `dep tree --format=mermaid`

---

## Datasets Required

| Dataset | Path | Issue Count | Use Cases |
|---------|------|-------------|-----------|
| beads_rust | `/data/projects/beads_rust/.beads` | ~373 | Large dataset, dependencies |
| beads_viewer | `/data/projects/beads_viewer/.beads` | Variable | Medium dataset |
| cass | `/data/projects/coding_agent_session_search/.beads` | Variable | Medium dataset |
| brenner_bot | `/data/projects/brenner_bot/.beads` | Variable | Small dataset |
| Fresh workspace | temp dir | 0 | Init, basic CRUD |

---

## Test Categories

### Read-Only Commands (Safe for Conformance)

```
list, show, ready, blocked, search, count, stale, graph, stats, status
dep list, dep tree, dep cycles
label list, label list-all
comments list
epic status
sync --status
config list, config get, config path
doctor, info, schema, where, version, lint
history list, history diff
query run, query list
orphans (without --fix)
changelog
completions
upgrade --check
agents --check
```

### Mutating Commands (Require Isolation)

```
init, create, q, update, close, reopen, delete
dep add, dep remove
label add, label remove, label rename
comments add
epic close-eligible
sync --flush-only, sync --import-only, sync --merge
config set, config delete, config edit
history restore, history prune
query save, query delete
audit record, audit label
defer, undefer
orphans --fix
upgrade (full)
agents --add, agents --remove, agents --update
```

---

## Environment Variables

| Variable | Purpose | Test Impact |
|----------|---------|-------------|
| `BEADS_DIR` | Override .beads discovery | Tested in `e2e_config_precedence.rs` |
| `BEADS_JSONL` | Override JSONL path | Tested in `e2e_env_overrides.rs` and `e2e_history_custom_path.rs` |
| `BD_ACTOR` / Actor flag | Audit trail identity | Tested in `e2e_env_overrides.rs` |
| `BR_UPGRADE_SKIP` | Skip upgrade tests | Used in CI |
| `BR_E2E_DESTRUCTIVE` | Enable destructive tests | Guards `history prune`, `delete --hard` |

---

## JSON Output Shapes

All commands support `--json` flag. Key shapes validated:

| Command | JSON Shape Location |
|---------|---------------------|
| `list --json` | `tests/snapshots/json_output.rs` |
| `show --json` | `tests/snapshots/json_output.rs` |
| `ready --json` | `tests/snapshots/json_output.rs` |
| `blocked --json` | `conformance.rs` |
| `stats --json` | `conformance.rs` |
| Error output | `tests/snapshots/error_messages.rs` |

---

## Exit Codes

| Code | Meaning | Tested In |
|------|---------|-----------|
| 0 | Success | All tests |
| 1 | General error | `e2e_errors.rs` |
| 2 | Not initialized | `e2e_errors.rs` |
| 3 | Not found | `e2e_errors.rs` |
| 4 | Conflict | `e2e_errors.rs` |
| 5 | Validation error | `e2e_errors.rs` |

---

## References

- [AGENTS.md](../AGENTS.md) - Agent workflow integration
- [SYNC_SAFETY.md](SYNC_SAFETY.md) - Sync safety guarantees
- [E2E_SYNC_TESTS.md](E2E_SYNC_TESTS.md) - Sync test execution guide
- [TROUBLESHOOTING.md](TROUBLESHOOTING.md) - Error codes and JSON schemas

---

*Generated: 2026-01-17*
*Refreshed: 2026-05-08 for beads_rust-9ow6.1*
*Original task: beads_rust-rkuz*
*Original agent: SilentFalcon (opus-4.5)*
