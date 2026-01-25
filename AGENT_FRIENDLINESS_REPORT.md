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
- Added machine-readable artifacts: `ROBOT_MODE_EXAMPLES.jsonl`, `CLI_SCHEMA.json`
- Added agent smoke tests: `scripts/agent_smoke_test.sh`
- Removed `rm -rf` usage from local scripts/tests to comply with the no-deletion policy in `AGENTS.md`

## Current Agent Surfaces

### Machine output formats

- JSON: `--json` (global) or `--format json` (command-level when supported)
- TOON: `--format toon` (decode via `tru --decode`)
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

Many failures emit a structured JSON error object on stderr. A canonical example is captured in:

- `agent_baseline/errors/show_not_found.json`

And the machine schema is available via:

```bash
br schema error --format json
```

## Interface Modality Decision

Decision: CLI-only (no MCP surface in this repo).

Rationale:

- br is frequently used as a local, composable CLI primitive (shell pipes, git hooks, scripts).
- MCP would add distribution + auth + permission surface area that is not required for the core value.
- The existing CLI already covers the key agent needs: triage (`ready`), mutation (`update/close`), and sync (`sync --flush-only`).

## Gaps / Next Improvements

- No dynamic `--help-json` surface yet; `CLI_SCHEMA.json` is an interim static artifact.
- Many commands return bare arrays/objects rather than a consistent `{data, metadata, errors}` envelope.
- Schema outputs include `generated_at` (useful, but not deterministic byte-for-byte).

## Scorecard (1-5)

| Dimension | Score | Notes |
|----------:|:-----:|-------|
| Documentation | 5 | Root `AGENTS.md` + agent-first entrypoints under `docs/agent/` |
| CLI ergonomics | 5 | Task-focused commands; consistent flags across major read flows |
| Robot/machine mode | 5 | JSON everywhere; TOON for many read commands |
| Schemas | 4 | `br schema` exists; TOON key folding is documented |
| Errors | 4 | Structured envelope w/ hints; stderr routing needs care when piping |
| Consistency | 4 | Some commands also have `--robot`; others rely on `--json`/`--format` |
| Overall | 4.5 | High maturity; remaining work is mostly polish/consistency |
