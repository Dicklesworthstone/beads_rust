# Schemas

br provides a schema surface describing the primary machine-readable outputs.

## Emit schemas

```bash
br schema all --format json
br schema issue-details --format json
br schema error --format json
```

TOON is also supported:

```bash
br schema all --format toon
```

## If `br schema` is missing

If `br schema --help` fails with "unrecognized subcommand", you're running an older `br` binary.

Options:

1. Use `br upgrade` (if available in your build).
2. Build from source in this repo and use the local binary:

```bash
CARGO_TARGET_DIR=target cargo build
./target/debug/br schema all --format json
```

As a fallback, this repo also includes a captured snapshot bundle under:

- `agent_baseline/schemas/`

## Key folding (TOON)

When emitting TOON, br may "fold" nested keys into dotted keys (safe folding) to save tokens.
Example: `schemas.IssueDetails` instead of `{ "schemas": { "IssueDetails": ... } }`.

If you need to parse TOON as JSON, decode with `tru`:

```bash
br schema issue-details --format toon | tru --decode | jq .
```
