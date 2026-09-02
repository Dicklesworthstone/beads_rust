# docs/ index

Which documents describe the code as it is, and which are historical plans.
Current documents are kept true by tests or generators where one exists; a
historical document is a record of intent and may describe modules, commands,
or numbers that never shipped or have since changed.

## Current (describes the code on `main`)

| Document | What it is | Kept true by |
|---|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Module map, storage layer, sync, config, errors, health vocabulary | manual review; example JSON captured from real runs (`<!-- from: ... -->` markers) |
| [CLI_REFERENCE.md](CLI_REFERENCE.md) | Per-command reference | manual; generator tracked in `beads_rust-wqmw.5` |
| [AGENT_INTEGRATION.md](AGENT_INTEGRATION.md), [agent/](agent/) | Agent-facing formats, robot mode, friendliness report, agent changelog | manual |
| [SYNC_SAFETY.md](SYNC_SAFETY.md), [SYNC_MAINTENANCE_CHECKLIST.md](SYNC_MAINTENANCE_CHECKLIST.md), [E2E_SYNC_TESTS.md](E2E_SYNC_TESTS.md) | Sync safety model, maintenance checklist, sync e2e guide | `tests/e2e_sync_git_safety.rs` and the static `Command::new` check |
| [reliability/HEALTH_CONTRACT.md](reliability/HEALTH_CONTRACT.md) | Workspace health levels and anomaly taxonomy | vocabulary from `src/health.rs` |
| [reliability/ENGINE_OPERATING_MODEL.md](reliability/ENGINE_OPERATING_MODEL.md) | FrankenSQLite decision, August 2026 incident, containment, sidecars, bump checklist | manual; receipts attached per release |
| [COORDINATION_EVIDENCE.md](COORDINATION_EVIDENCE.md) | `br coordination status` contract | `tests/e2e_coordination.rs` |
| [GH384_ACCEPTANCE_MATRIX.md](GH384_ACCEPTANCE_MATRIX.md) | Workflow capacity acceptance matrix | `tests/e2e_workflow_capacity_scopes.rs` |
| [TEST_HARNESS.md](TEST_HARNESS.md), [TESTING_GUIDELINES.md](TESTING_GUIDELINES.md), [SNAPSHOT_TESTING.md](SNAPSHOT_TESTING.md), [E2E_COVERAGE_MATRIX.md](E2E_COVERAGE_MATRIX.md) | How to run and extend the suites; RCH time caps; coverage matrix | manual (matrix regeneration tracked in `beads_rust-wqmw.4`) |
| [CI_SUPPLY_CHAIN.md](CI_SUPPLY_CHAIN.md) | Workflow action pin policy and proof commands | `tests/workflow_*.rs` |
| [INSTALLING.md](INSTALLING.md), [VCS_INTEGRATION.md](VCS_INTEGRATION.md), [TROUBLESHOOTING.md](TROUBLESHOOTING.md) | Install, non-git VCS, troubleshooting | manual |
| [SWARM_SCALE_TUNING.md](SWARM_SCALE_TUNING.md), [operations/](operations/) | Lock timeouts, swarm topology, runbooks | manual |
| [ARTIFACT_LOG_SCHEMA.md](ARTIFACT_LOG_SCHEMA.md), [WRITE_COMBINING_QUEUE_DESIGN.md](WRITE_COMBINING_QUEUE_DESIGN.md) | Artifact log schema; write-combining design (dormant module, see ARCHITECTURE.md) | manual |
| audit_*.md, snapshot_review_*.md, fsqlite_trailing_pages_report.md | Dated audit and review records | frozen |

## Historical (planning documents; do not treat as a description of the code)

| Document | Written | Why it is historical |
|---|---|---|
| [porting/PLAN_TO_PORT_BEADS_WITH_SQLITE_AND_ISSUES_JSONL_TO_RUST.md](porting/PLAN_TO_PORT_BEADS_WITH_SQLITE_AND_ISSUES_JSONL_TO_RUST.md) | 2026-01 | The port plan; some listed commands (`duplicates`, `edit`, `compact`, `cleanup`, `--robot-help`) were never built or were folded into others |
| [porting/PROPOSED_ARCHITECTURE_FOR_BR_USING_RUST_BEST_PRACTICES.md](porting/PROPOSED_ARCHITECTURE_FOR_BR_USING_RUST_BEST_PRACTICES.md) | 2026-01 | Proposed a `Storage` trait and a module split that were not built; see ARCHITECTURE.md for what was |
| [porting/EXISTING_BEADS_STRUCTURE_AND_ARCHITECTURE.md](porting/EXISTING_BEADS_STRUCTURE_AND_ARCHITECTURE.md) | 2026-01 | Survey of the Go beads being ported |
| [plans/RICH_INTEGRATION_PLAN.md](plans/RICH_INTEGRATION_PLAN.md) | 2026-01 | Shipped; checklist reconciled 2026-09-02 with a Deferred section |
| [plans/BRIDGE_PLAN_2026_09_01.md](plans/BRIDGE_PLAN_2026_09_01.md) | 2026-09 | Live bridge plan from the reality check; revised in place, beads generated from it |
| [progress/](progress/) | various | Dated progress notes |
