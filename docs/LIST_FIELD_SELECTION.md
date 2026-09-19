# Compact structured issue discovery

`br list` supports explicit column selection in JSON and TOON output through
`--fields`. This extends the existing CSV argument; ordinary text output and
CSV behavior are unchanged. The CLI help/completion inventory still describes
the older CSV surface; this guide documents the additional structured behavior.

## Select columns, not fewer issues

```bash
# Complete matching work surface, without descriptions or other long text.
br list --json --fields id,title,status,priority,issue_type

# Include ownership when it is needed for the decision.
br list --status open --json --fields id,title,assignee,priority

# Retain relationship evidence explicitly.
br list --json --fields id,title,labels,dependency_count,dependent_count

# Same selection in TOON.
br list --format toon --fields id,title,status,priority

# Explicit pagination remains available and disclosed in the envelope.
br list --json --fields id,title --sort title --limit 20 --offset 20

# A filter can inspect a column that is absent from the result.
br list --json --fields id,title --desc-contains regression
```

Omitting `--fields` preserves the existing full issue schema. Selection does
not change which issues match, their ordering, or the default unlimited list.
The `issues`, `total`, `limit`, `offset`, and `has_more` envelope is retained.
Use `br show <id> --json` to retrieve a selected issue's full details later.

A selected optional field with no value is emitted as `null`; selected labels
are an array, including `[]` when empty. Priority and relation counts remain
numbers. Status/type and timestamps use their existing model serializers.
IDs are not implicitly added: include `id` when the next step needs it.

Field names are exact, case-sensitive JSON keys. Surrounding whitespace is
trimmed and duplicate names are removed. Unknown names and empty comma entries
are validation errors, including when no issues match. For example,
`--fields id,titel` and `--fields id,,title` fail rather than silently producing
an incomplete projection. Use `issue_type`, not `type`.

## Supported fields

```text
id,title,status,priority,issue_type,assignee,owner,created_at,updated_at,
created_by,description,design,acceptance_criteria,prerequisites,notes,
closed_at,close_reason,due_at,defer_until,estimated_minutes,external_ref,
source_repo,source_repo_path,labels,dependency_count,dependent_count
```

## Implementation and limits

Core-column selections (`id`, `title`, `status`, `priority`, `issue_type`, with
optional labels/counts) reuse the existing narrow text-list storage projection.
Selections requiring other columns, or client-side filters that inspect full
records, retain full issue loading. The storage projection's existing fallback
for unsupported filter shapes is preserved. Field selection is therefore not
a promise that every query avoids loading long text.

Only requested labels/counts are loaded for the returned page, using existing
storage methods. JSON serialization writes selected fields directly rather
than creating full JSON rows and dropping keys afterward. TOON uses the
standard encoder. Relation-read errors propagate before serialization begins.

This is an additive response to the discovery gap in `beads_rust-epyys`:
reduce irrelevant columns instead of silently hiding work behind a new default
row cap. No new flag or default-schema switch is introduced. This does not
claim constant-memory execution, single-snapshot reads, or a measured speedup.

## Regression coverage and qualification

`src/cli/commands/list_fields.rs` contains five unit/storage tests for selector
validation, duplicate/order handling, typed serialization and nulls, supported
field coverage, and narrow/full query parity. `tests/repro_list_fields.rs`
adds five CLI tests covering full versus selected page equality, filters before
pagination, relations and formats, empty/invalid selections, and 101 issues
across the large-page query threshold. CLI JSON assertions parse the entire
stdout document.

These tests were added and source-reviewed, not executed in the editing
environment: Rust, Cargo, RCH, and rustfmt were unavailable. Native compiler,
Clippy, formatting, and runtime qualification remain required. No payload
numbers from a different corpus are reused here. The Bead remains open for
qualification and integration of this guide into the shared README, AGENTS,
CLI reference, help, and completion surfaces.
