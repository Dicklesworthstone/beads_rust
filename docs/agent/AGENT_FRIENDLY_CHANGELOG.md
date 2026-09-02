# Agent-Friendly Changelog

This file tracks agent-facing changes (docs, robot output surfaces, schemas, safety behavior).

## 2026-09-02

Catch-up entry written during the 2026-09-01 reality check; each item names
the command or surface an agent can rely on now.

- `br capabilities --format json [--command <name>]`: machine-readable contract
  envelope (commands, global flags, env vars, exit codes, safety guarantees).
- `br robot-docs guide` and `br schema <name> --format json|toon`: concise
  agent docs and JSON Schema documents for robot outputs.
- `br coordination status --json` (`br.coordination.v1`): stale-claim evidence
  for hidden in-progress work; `scripts/stale-claims.sh` wraps it as a gate.
- `br serve` (`--features mcp`): MCP stdio server with 7 tools, 12 resources,
  4 prompts; same lock/audit/flush model as the CLI.
- Workflow policy in `.beads/policy.yaml`: ready status groups, capacity
  limits (statuses, groups, admission, counting modes, exemptions, scopes),
  required transition fields; `br gate report/list`, `br capacity exempt`,
  `br scheduler`.
- `br update`: refuses to overwrite non-empty text fields without `--force`
  (GH #467); `br list --tree` (GH #475); `br sync --reconcile-additive
  --dry-run` reachable (GH #473); bypassed-policy closes exported to JSONL
  (GH #474).
- `br doctor`: `db.read_only_open_observational` check (GH #476 contract),
  `migrate-schema plan|apply|undo`, `--repair` preserves append-only tables
  (GH #471); `explain` is still a stub (tracked in `beads_rust-v7o2.3`).
- Read-only commands use a lock-free current-schema fast open; disable with
  `BR_DISABLE_READ_ONLY_FAST_OPEN=1` when comparing against the locked path.

## 2026-01-25

- Added agent-first doc entrypoints under `docs/agent/`.
- Added `agent_baseline/` snapshots (README/help/schema + small example outputs).
- Added `agent_baseline/examples/robot_mode_examples.jsonl` and `agent_baseline/schemas/cli_schema.json` as static, machine-readable artifacts.
- Removed `rm -rf` usage from local scripts/tests to comply with the no-deletion policy in `AGENTS.md`.
