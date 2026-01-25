# Quickstart (Agents)

Goal: in under 30 seconds, list actionable work, claim it, complete it, and sync.

## 1) Initialize (once per repo)

```bash
br init --prefix bd
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
br update bd-abc123 --status in_progress --claim --format json
```

## 4) Close + explain why

```bash
br close bd-abc123 --reason "Implemented X; tests pass" --format json
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
  - Some commands also accept `--robot` (alias for `--json`).
- When scripting, route stderr separately; errors may be emitted as structured JSON on stderr.

## Agent smoke test

To sanity-check JSON/TOON outputs and env precedence:

```bash
./scripts/agent_smoke_test.sh
```
