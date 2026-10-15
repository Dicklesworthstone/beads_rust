# AGENTS.md (Agent-First Entry Point)

This folder is a thin, agent-first map into the existing `br` documentation set.

If you are an AI coding agent working in this repo, start here:

- Safety + workflow rules: `AGENTS.md` (repo root)
- First 30 seconds: `docs/agent/QUICKSTART.md`
- Machine output (JSON/TOON): `docs/agent/ROBOT_MODE.md`
- Schemas + key folding notes: `docs/agent/SCHEMA.md`
- Error shapes + exit codes: `docs/agent/ERRORS.md`
- Example flows: `docs/agent/EXAMPLES.md`

Reference docs (deeper, more verbose):

- CLI flags/commands: `docs/CLI_REFERENCE.md`
- Agent integration guide: `docs/AGENT_INTEGRATION.md`

For any web requests you must make with curl or otherwise, identify yourself honestly: use a User-Agent that names this project, e.g. "beads_rust-agent (+https://github.com/Dicklesworthstone/beads_rust)". Never borrow another product's or company's crawler or client User-Agent, and never spoof a client to get past a block.
