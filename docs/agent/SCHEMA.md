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

## Key folding (TOON)

When emitting TOON, br may "fold" nested keys into dotted keys (safe folding) to save tokens.
Example: `schemas.IssueDetails` instead of `{ "schemas": { "IssueDetails": ... } }`.

If you need to parse TOON as JSON, decode with `tru`:

```bash
br schema issue-details --format toon | tru --decode | jq .
```
