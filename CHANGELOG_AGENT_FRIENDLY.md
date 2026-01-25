# Agent-Friendly Changelog

This file tracks agent-facing changes (docs, robot output surfaces, schemas, safety behavior).

## 2026-01-25

- Added agent-first doc entrypoints under `docs/agent/`.
- Added `agent_baseline/` snapshots (README/help/schema + small example outputs).
- Added `ROBOT_MODE_EXAMPLES.jsonl` and `CLI_SCHEMA.json` as static, machine-readable artifacts.
- Removed `rm -rf` usage from local scripts/tests to comply with the no-deletion policy in `AGENTS.md`.
