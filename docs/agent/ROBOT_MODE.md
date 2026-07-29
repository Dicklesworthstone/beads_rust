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

- In JSON/robot mode, the machine-readable result goes to stdout: success
  data on exit `0`, the structured JSON error envelope on non-zero exits
  (see `docs/agent/ERRORS.md`, including the two-document partial-batch
  contract).
- Diagnostics/logging (`RUST_LOG`) and non-fatal structured warnings
  (e.g. `AUTO_FLUSH_FAILED`) go to stderr — never the error envelope.

Practical pattern:

```bash
br ready --format json 2>/dev/null | jq .
# On non-zero exit, the envelope is the last JSON document on stdout:
br show br-NOTEXIST --json 2>/dev/null | jq -s '.[-1].error' || true
```

## Text wrapping (human output)

When using text output, `--wrap` wraps long lines instead of truncating.

## TOON decode tool (`tru`)

If you want to decode TOON back into nested JSON for piping, you need `tru`
with safe path expansion because br emits safe folded keys.

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
