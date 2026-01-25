# Robot Mode (JSON/TOON)

br supports machine-readable output for agent/tooling integration.

## Choosing an output format

- JSON: `--format json` (or `--json`)
- TOON: `--format toon` (token-optimized object notation)

Some commands also accept `--robot` as an alias for `--json` (see the command's `--help`).

## Environment defaults

If you omit `--format` / `--json`, br can default formats via env vars:

- `BR_OUTPUT_FORMAT` (highest precedence)
- `TOON_DEFAULT_FORMAT` (fallback)

Supported values: `text`, `json`, `toon` (and for some commands, `csv`).

Example:

```bash
export TOON_DEFAULT_FORMAT=toon
br list --limit 5          # defaults to TOON
br list --json --limit 5   # JSON always wins
```

## stderr vs stdout

- Normal successful outputs go to stdout.
- Diagnostics/logging go to stderr.
- Some failures emit a structured JSON error object on stderr (see `docs/agent/ERRORS.md`).

Practical pattern:

```bash
br ready --format json 2>ready.stderr.json | jq .
```

## Text wrapping (human output)

When using text output, `--wrap` wraps long lines instead of truncating.

## TOON decode tool (`tru`)

If you want to decode TOON back into JSON for piping, you need `tru` (the TOON CLI).

If `tru` is not available, prefer `--format json` / `--json` instead.

Quick check:

```bash
command -v tru && tru --version
```

## Smoke test

Run:

```bash
./scripts/agent_smoke_test.sh
```
