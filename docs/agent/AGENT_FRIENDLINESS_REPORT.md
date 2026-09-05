# Agent-Friendliness Report: br (beads_rust)

Bead: bd-3s2 (re-underwriting)
Audit date: 2026-01-25
Auditor: WildAnchor (Codex / GPT-5)

## Executive Summary

br is already strongly agent-friendly (non-invasive CLI + machine outputs), and now has a clear
schema surface plus token-efficient TOON output for many read commands.

Recent work (this repo):

- Added agent-first doc entrypoints: `docs/agent/`
- Captured a baseline snapshot pack: `agent_baseline/`
- Added machine-readable artifacts: `agent_baseline/examples/robot_mode_examples.jsonl`, `agent_baseline/schemas/cli_schema.json`
- Added agent smoke tests: `scripts/agent_smoke_test.sh`
- Removed `rm -rf` usage from local scripts/tests to comply with the no-deletion policy in `AGENTS.md`

## Current Agent Surfaces

### Machine output formats

- JSON: `--json` (global) or `--format json` (command-level when supported)
- TOON: `--format toon` (decode via `tru --decode --expand-paths safe`)
- Default format (if you omit `--format`/`--json`): `BR_OUTPUT_FORMAT` > `TOON_DEFAULT_FORMAT`

### Schema surface

br can emit JSON Schemas for primary machine outputs:

```bash
br schema all --format json
br schema issue-details --format json
br schema error --format json
```

TOON is also supported:

```bash
br schema all --format toon
```

### Error envelope

In JSON mode, failures emit a structured JSON error object on stdout (the same stream as success data — selected by exit code; see `ERRORS.md`). A canonical example is captured in:

- `agent_baseline/errors/show_not_found.json`

And the machine schema is available via:

```bash
br schema error --format json
```

## Interface Modality Decision

Decision (revised 2026-09-02): CLI-first, with an optional MCP surface.

The original CLI-only decision predates `br serve`. Today:

- The CLI remains the primary, composable primitive (shell pipes, scripts,
  `--json`/`--robot`, `br capabilities`, `br schema`, `br robot-docs`).
- `br serve` (built with `--features mcp`, non-default) exposes the same
  workspace over MCP stdio: 7 tools (`list_issues`, `show_issue`,
  `create_issue`, `update_issue`, `close_issue`, `manage_dependencies`,
  `project_overview`), 12 resources (`beads://project/info`,
  `beads://issue/{id}`, `beads://schema`, `beads://labels`,
  `beads://issues/ready`, `beads://issues/blocked`, `beads://issues/in_progress`,
  `beads://coordination/status`, `beads://events/recent`,
  `beads://issues/deferred`, `beads://graph/health`, `beads://issues/bottlenecks`),
  and 4 prompts (`triage`, `status_report`, `plan_next_work`, `polish_backlog`).
  It never listens on a network port, never runs git, and uses the same
  write lock, audit events, and JSONL auto-flush as the CLI.
- Use the CLI for scripts; use MCP when an agent benefits from discoverable
  tools, resources, and prompts. Stdio protocol tests live in
  `tests/e2e_mcp_protocol.rs` and run with `--features mcp`; unit tests live
  in `src/mcp/`.

## Gaps / Next Improvements

- No dynamic `--help-json` surface yet; `agent_baseline/schemas/cli_schema.json` is an interim static artifact.
- Many commands return bare arrays/objects rather than a consistent `{data, metadata, errors}` envelope.
- Schema outputs include `generated_at` (useful, but not deterministic byte-for-byte).

## Scorecard (1-5)

| Dimension | Score | Notes |
|----------:|:-----:|-------|
| Documentation | 5 | Root `AGENTS.md` + agent-first entrypoints under `docs/agent/` |
| CLI ergonomics | 5 | Task-focused commands; consistent flags across major read flows |
| Robot/machine mode | 5 | JSON everywhere; TOON for many read commands |
| Schemas | 4 | `br schema` exists; TOON key folding is documented |
| Errors | 4 | Structured envelope w/ hints on stdout in JSON mode; partial batches emit payload + envelope (see ERRORS.md) |
| Consistency | 4 | Some commands also have `--robot`; others rely on `--json`/`--format` |
| Overall | 4.5 | High maturity; remaining work is mostly polish/consistency |
