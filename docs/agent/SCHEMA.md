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

Those snapshots are checked against the built binary by the
`agent_baseline_snapshots_match_current_binary` test. After intentional schema
changes, regenerate them with:

```bash
UPDATE_AGENT_BASELINE=1 cargo test --test e2e_schema agent_baseline_snapshots_match_current_binary -- --nocapture
```

## Issue checklist fields

Issue JSON, JSONL, TOON, and the `Issue`/`IssueDetails` schemas expose two
independent optional strings:

| Field | Meaning |
|---|---|
| `acceptance_criteria` | Delivery criteria, with optional Markdown checklist items exposed through `acceptance_items` in issue details |
| `prerequisites` | Preparation checklist stored verbatim; independent from dependency edges and acceptance criteria |

CLI `create` and `update` accept `--prerequisites` and
`--acceptance-criteria`. MCP `create_issue` and `update_issue` accept the same
field names with underscores. An absent update field leaves its value alone;
an empty string or MCP update `null` clears the optional value. Empty strings
are normalized to absence when read from storage. Destructive whole-field replacements use the normal overwrite
guard (`--force` / MCP `force: true`). MCP whole-field acceptance input cannot
be combined with the acceptance item edit parameters.

Workflow `required_fields` separates presence and completion:
`acceptance_criteria_present` accepts a nonempty prospective value,
`acceptance_criteria` additionally refuses unchecked checklist items, and
`prerequisites_complete` requires at least one prerequisite checklist item
with every item checked. Matching requirements compose with fresh transition
comments and other policy gates. Force cannot bypass them. A field and status
submitted together are validated against the proposed field values before
their atomic mutation.

## Key folding (TOON)

When emitting TOON, br may "fold" nested keys into dotted keys (safe folding) to save tokens.
Example: `schemas.IssueDetails` instead of `{ "schemas": { "IssueDetails": ... } }`.

If you need to parse TOON as nested JSON, decode with safe path expansion:

```bash
br schema issue-details --format toon | tru --decode --expand-paths safe | jq .
```
