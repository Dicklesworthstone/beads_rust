# Quickstart (Agents)

Goal: in under 30 seconds, list actionable work, claim it, complete it, and sync.

## 1) Initialize (once per repo)

```bash
br init
```

## 2) Find work

Machine-readable:

```bash
br ready --format json --limit 10
```

Token-efficient:

```bash
br ready --format toon --limit 10
```

## 3) Claim + work

```bash
br --json update br-abc123 --status in_progress --claim
```

If Agent Mail file reservations are unavailable, make the degraded claim visible
before editing:

```bash
export AGENT_NAME="${AGENT_NAME:-codex-agent}"
br --json update br-abc123 --status in_progress --assignee "$AGENT_NAME"
br --json comments add br-abc123 --author "$AGENT_NAME" \
  --message "degraded-coordination: Agent Mail unavailable; files: src/foo.rs"
git status --short
br --json list --status in_progress
```

Treat that comment as advisory, not as a lock. Avoid files already named by
another active claim or dirty in the worktree.

## 4) Close + explain why

```bash
br --json close br-abc123 --reason "Implemented X; tests pass"
```

## 5) Sync (end of session)

Export JSONL for git commit (no import):

```bash
br sync --flush-only
```

## Common gotchas

- Preferred flags:
  - Use `--format json` or `--format toon` when the command supports it.
  - `--json` always forces JSON.
  - For mutation commands such as `update` and `close`, prefer global `--json`; do not assume every mutation command has command-local `--format`.
- When scripting with `--json`, parse stdout for BOTH success data and the structured error envelope (selected by exit code); stderr carries human diagnostics, `RUST_LOG` output, and non-fatal structured warnings — never the envelope. See `docs/agent/ERRORS.md`.

## Agent smoke test

To sanity-check JSON/TOON outputs and env precedence:

```bash
./scripts/agent_smoke_test.sh
```
