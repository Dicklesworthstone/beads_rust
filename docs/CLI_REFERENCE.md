# br CLI Reference

Comprehensive reference for all `br` (beads_rust) commands.

---

## Table of Contents

- [Global Options](#global-options)
- [Cross-Project Routing](#cross-project-routing)
- [Core Commands](#core-commands)
  - [init](#init)
  - [create](#create)
  - [q (quick capture)](#q-quick-capture)
  - [list](#list)
  - [show](#show)
  - [update](#update)
  - [close](#close)
  - [reopen](#reopen)
  - [delete](#delete)
- [Query Commands](#query-commands)
  - [ready](#ready)
  - [blocked](#blocked)
  - [search](#search)
  - [count](#count)
  - [stale](#stale)
- [Organization Commands](#organization-commands)
  - [dep](#dep)
  - [graph](#graph)
  - [label](#label)
  - [epic](#epic)
  - [comments](#comments)
- [Workflow Commands](#workflow-commands)
  - [defer / undefer](#defer--undefer)
  - [orphans](#orphans)
  - [query (saved queries)](#query-saved-queries)
  - [gate](#gate)
- [Sync & Config](#sync--config)
  - [sync](#sync)
  - [config](#config)
- [Agent Integration](#agent-integration)
  - [capabilities](#capabilities)
  - [robot-docs](#robot-docs)
  - [serve](#serve)
- [Diagnostics & Info](#diagnostics--info)
  - [agents](#agents)
  - [stats / status](#stats--status)
  - [doctor](#doctor)
  - [info](#info)
  - [where](#where)
  - [schema](#schema)
  - [version](#version)
  - [audit](#audit)
  - [history](#history)
  - [changelog](#changelog)
  - [lint](#lint)
- [Utilities](#utilities)
  - [upgrade](#upgrade)
  - [completions](#completions)
- [Exit Codes](#exit-codes)
- [Environment Variables](#environment-variables)
- [JSON Output Schemas](#json-output-schemas)

---

## Global Options

These options apply to all commands:

| Option | Description |
|--------|-------------|
| `--db <PATH>` | Database path (auto-discover `.beads/*.db` if not set) |
| `--actor <NAME>` | Actor name for audit trail |
| `--json` | Output as JSON (machine-readable) |
| `--no-daemon` | Force direct mode (no daemon) |
| `--no-auto-flush` | Skip automatic JSONL export after mutations |
| `--no-auto-import` | Skip automatic import check |
| `--allow-stale` | Allow stale DB (bypass freshness check warning) |
| `--lock-timeout <LOCK_TIMEOUT>` | SQLite busy/write-lock timeout in milliseconds |
| `--no-db` | JSONL-only mode (no DB connection) |
| `-v, --verbose` | Increase logging verbosity (-v, -vv) |
| `-q, --quiet` | Quiet mode (errors only) |
| `--no-color` | Disable colored output |
| `-h, --help` | Print help |
| `-V, --version` | Print version |

By default, successful mutating commands auto-flush SQLite changes to
`.beads/issues.jsonl`, so the JSONL file is normally ready to stage after the
command completes. Use `--no-auto-flush` to skip that export for a single
command. `br sync --flush-only` remains useful as an idempotent final export
check before committing, after `--no-auto-flush`, after disabling auto-flush in
config, or during recovery.

---

## Cross-Project Routing

`br` can route explicit issue IDs to another workspace when their prefix matches
`.beads/routes.jsonl`. This is useful for town or multi-repository setups where
one project needs to inspect or update an issue owned by another project.

Each route is one JSON object per line:

```jsonl
{"prefix":"api-","path":"../api"}
{"prefix":"ops-","path":"/srv/projects/ops/.beads"}
```

Route resolution:

1. Extract the issue prefix before the final hyphen, including the hyphen, so
   hyphenated prefixes such as `document-intelligence-` route correctly.
2. Search the local `.beads/routes.jsonl`.
3. If a parent town root with `mayor/town.json` exists, search its
   `.beads/routes.jsonl`.
4. Resolve `path` as a project root or a direct `.beads`/`_beads` directory.
5. Follow a target `.beads/redirect` file when present.

Current route-aware commands include common issue-ID operations such as `show`,
`update`, `close`, `reopen`, `delete`, `defer`, `comments`, `label`, `dep`,
`graph`, `audit`, and `lint`. Routed write operations acquire the target
workspace's `.write.lock` and mutate the target workspace, not the caller's
local database.

Safety boundaries:

- Routing never runs git, copies repositories, or performs network sync.
- Routing is not real-time collaboration; each affected repository still needs
  its own normal `br sync --flush-only`/VCS commit flow.
- Routes are prefix dispatch rules. They do not import external issues into the
  local database.
- Cross-project dependency status checks use explicit IDs such as
  `external:api:api-123` plus config keys like `external_projects.api=../api`.

---

## Core Commands

### init

Initialize a beads workspace in the current directory.

```bash
br init [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--prefix <PREFIX>` | Issue ID prefix (e.g., "bd", "proj") |
| `--force` | Overwrite existing database |
| `--backend <BACKEND>` | Backend type placeholder; currently ignored and always uses SQLite |

**Examples:**
```bash
# Initialize with default prefix
br init

# Initialize with custom prefix
br init --prefix myproj

# Force reinitialize
br init --force
```

---

### create

Create a new issue.

```bash
br create [OPTIONS] [TITLE]
```

**Arguments:**
- `TITLE` - Issue title (can also use `--title-flag`)

**Options:**
| Option | Description |
|--------|-------------|
| `-t, --type <TYPE>` | Issue type (task, bug, feature, epic, chore, docs, question) |
| `-p, --priority <PRIORITY>` | Priority (0-4 or P0-P4, where 0=critical) |
| `-d, --description <TEXT>` | Issue description |
| `--slug <SLUG>` | Human-readable slug embedded in the generated ID (lowercase ASCII alphanumerics + single hyphens, capped at 48 chars; see [Slug normalization](#slug-normalization)) |
| `-a, --assignee <NAME>` | Assign to person |
| `--owner <EMAIL>` | Set owner email |
| `-l, --labels <LABELS>` | Labels (comma-separated) |
| `--parent <ID>` | Parent issue ID (creates parent-child dependency) |
| `--deps <DEPS>` | Dependencies (format: `type:id,type:id`) |
| `-e, --estimate <MINUTES>` | Time estimate in minutes |
| `--due <DATE>` | Due date (RFC3339 or relative like `+2d`, `tomorrow`) |
| `--defer <DATE>` | Defer until date |
| `--external-ref <REF>` | External reference (e.g., `gh-123`) |
| `--ephemeral` | Mark as ephemeral (not exported to JSONL) |
| `-s, --status <STATUS>` | Initial status (`open`, `deferred`, `in_progress`, `closed`) |
| `--dry-run` | Preview without creating |
| `--silent` | Output only issue ID |
| `-f, --file <PATH>` | Create issues from markdown file (bulk import) |

**Examples:**
```bash
# Simple task
br create "Fix login bug"

# High-priority bug with details
br create "Critical security issue" -t bug -p 0 -d "XSS vulnerability in form input"

# Feature with assignee and labels
br create "Add dark mode" -t feature -a alice -l "ui,enhancement"

# Task with due date
br create "Deploy to production" --due "+3d"

# Bulk import from markdown
br create -f issues.md

# Human-readable slug embedded in the ID
br create "Fix login bug on mobile" --slug "fix-login-mobile"
# → Created: <prefix>-fix-login-mobile-<hash>  (e.g., br-fix-login-mobile-8cda)
```

#### Slug normalization

The `--slug` flag embeds a normalized slug between the configured prefix and
the uniquifying hash suffix. Normalization rules (implemented in
`src/util/id.rs::normalize_slug`):

- Lowercased ASCII alphanumeric characters are kept.
- Runs of any other character (whitespace, punctuation, Unicode) collapse to a
  single hyphen.
- Leading and trailing hyphens are stripped.
- Length is capped at **48 characters** after normalization; if the cap leaves
  a trailing hyphen, that hyphen is also stripped.
- A slug that normalizes to an empty string falls back to the standard
  hash-only ID (no slug embedded).

Examples:

| Input | Normalized output | Resulting ID shape |
|-------|-------------------|--------------------|
| `"Fix Login Bug"` | `fix-login-bug` | `<prefix>-fix-login-bug-<hash>` |
| `"a/b/c"` | `a-b-c` | `<prefix>-a-b-c-<hash>` |
| `"café-résumé"` | `caf-r-sum` (Unicode dropped) | `<prefix>-caf-r-sum-<hash>` |
| `"!!!"` | `` (empty → fallback) | `<prefix>-<hash>` |

#### Downstream `--slug` integration

Three commits made `--slug` end-to-end:
- [`5c0af3d4`](https://github.com/Dicklesworthstone/beads_rust/commit/5c0af3d4) `feat(create): --slug for human-readable issue IDs (#283)` — the feature itself.
- [`f454486f`](https://github.com/Dicklesworthstone/beads_rust/commit/f454486f) `fix(sync): accept slugged IDs in prefix guard` — sync's prefix guard now tolerates slugged IDs during import/export.
- [`52ff1722`](https://github.com/Dicklesworthstone/beads_rust/commit/52ff1722) `feat(orphans): scan all candidate-issue prefixes when finding commit refs` — `br orphans` finds commit references to slugged IDs.

The full lifecycle round-trip (create with slug → show → update → close → orphans references) is verified by `tests/e2e_scripts/slug_round_trip.sh` (added by `beads_rust-l6xl`).

---

### q (quick capture)

Quick capture - create issue and print only the ID.

```bash
br q [OPTIONS] <TITLE>
```

Same options as `create`, but outputs only the issue ID for scripting.

**Example:**
```bash
# Capture and immediately assign
ISSUE=$(br q "Quick fix needed")
br update $ISSUE --assignee me
```

---

### list

List issues with filtering and sorting.

```bash
br list [OPTIONS]
```

**Filter Options:**
| Option | Description |
|--------|-------------|
| `-s, --status <STATUS>` | Filter by status (can repeat) |
| `-t, --type <TYPE>` | Filter by issue type (can repeat) |
| `--assignee <NAME>` | Filter by assignee |
| `--unassigned` | Show only unassigned issues |
| `--id <ID>` | Filter by specific IDs (can repeat) |
| `-l, --label <LABEL>` | Filter by label (AND logic, can repeat) |
| `--label-any <LABEL>` | Filter by label (OR logic, can repeat) |
| `-p, --priority <PRIORITY>` | Filter by priority (can repeat) |
| `--priority-min <N>` | Filter by minimum priority |
| `--priority-max <N>` | Filter by maximum priority |
| `--title-contains <TEXT>` | Title contains substring |
| `--desc-contains <TEXT>` | Description contains substring |
| `--notes-contains <TEXT>` | Notes contains substring |
| `-a, --all` | Include closed issues |
| `--deferred` | Include deferred issues |
| `--overdue` | Filter for overdue issues |

**Output Options:**
| Option | Description |
|--------|-------------|
| `--limit <N>` | Maximum results (0=unlimited; default: unlimited — the full work surface). Pass `--limit N` to cap. |
| `--sort <FIELD>` | Sort by: priority, created_at, updated_at, title |
| `-r, --reverse` | Reverse sort order |
| `--long` | Long output format |
| `--pretty` | Tree/pretty output format |
| `--wrap` | Wrap long lines instead of truncating in text output |
| `--format <FMT>` | Output format: text, json, csv, toon |
| `--stats` | Show token savings stats when using TOON output |
| `--fields <FIELDS>` | CSV fields (comma-separated) |

**Examples:**
```bash
# All open issues
br list

# High-priority bugs
br list -t bug -p 0 -p 1

# My assigned work
br list --assignee $(whoami)

# Export to CSV
br list --format csv --fields id,title,status,priority > issues.csv

# JSON for scripting
br list --json | jq '.issues[].id'
```

---

### show

Show detailed issue information.

```bash
br show [IDS]...
```

**Options:**
| Option | Description |
|--------|-------------|
| `--format <FMT>` | Output format: text, json, toon |
| `--wrap` | Wrap long lines instead of truncating in text output |
| `--stats` | Show token savings stats when using TOON output |

**Examples:**
```bash
# Show single issue
br show bd-abc123

# Show multiple issues
br show bd-abc123 bd-def456

# JSON output
br show bd-abc123 --json
```

---

### update

Update one or more issues.

```bash
br update [OPTIONS] [IDS]...
```

**Options:**
| Option | Description |
|--------|-------------|
| `--title <TEXT>` | Update title |
| `--description <TEXT>` | Update description |
| `--design <TEXT>` | Update design notes |
| `--acceptance-criteria <TEXT>` | Update acceptance criteria |
| `--notes <TEXT>` | Update additional notes |
| `-s, --status <STATUS>` | Change status |
| `-p, --priority <N>` | Change priority |
| `-t, --type <TYPE>` | Change issue type |
| `--assignee <NAME>` | Assign (empty string clears) |
| `--owner <EMAIL>` | Set owner (empty string clears) |
| `--claim` | Atomic claim (assignee=actor + status=in_progress) |
| `--force` | Force update even if issue is blocked |
| `--due <DATE>` | Set due date (empty string clears) |
| `--defer <DATE>` | Set defer date (empty string clears) |
| `--estimate <MINUTES>` | Set time estimate |
| `--add-label <LABEL>` | Add label(s) |
| `--remove-label <LABEL>` | Remove label(s) |
| `--set-labels <LABELS>` | Replace all labels |
| `--parent <ID>` | Reparent (empty string removes) |
| `--external-ref <REF>` | Set external reference |
| `--session <ID>` | Set `closed_by_session` when closing |

**Examples:**
```bash
# Claim a task
br update bd-abc123 --claim

# Change status
br update bd-abc123 -s in_progress

# Update multiple issues
br update bd-abc123 bd-def456 -p 1

# Add labels
br update bd-abc123 --add-label "urgent,reviewed"
```

---

### close

Close one or more issues.

```bash
br close [OPTIONS] [IDS]...
```

**Options:**
| Option | Description |
|--------|-------------|
| `-r, --reason <TEXT>` | Close reason |
| `-f, --force` | Close even if blocked by open dependencies |
| `--suggest-next` | Return newly unblocked issues |
| `--session <ID>` | Session ID for tracking |
| `--robot` | Machine-readable output |

**Examples:**
```bash
# Close with reason
br close bd-abc123 -r "Completed in PR #42"

# Close multiple
br close bd-abc123 bd-def456 -r "Sprint complete"

# Force close blocked issue
br close bd-abc123 --force

# Close and get next work
br close bd-abc123 --suggest-next --json
```

---

### reopen

Reopen a closed issue.

```bash
br reopen [OPTIONS] [IDS]...
```

**Options:**
| Option | Description |
|--------|-------------|
| `-r, --reason <TEXT>` | Reason for reopening, stored as a comment |
| `--robot` | Machine-readable output |

---

### delete

Delete an issue (creates tombstone).

```bash
br delete [OPTIONS] <IDS>...
```

**Options:**
| Option | Description |
|--------|-------------|
| `--reason <TEXT>` | Delete reason (default: `delete`) |
| `--from-file <PATH>` | Read IDs from file (one per line, `#` comments ignored) |
| `--cascade` | Delete dependents recursively |
| `--force` | Bypass dependent checks, orphaning dependents |
| `--hard` | Prune tombstones from JSONL immediately |
| `--dry-run` | Preview only, no changes |

---

## Query Commands

### ready

List issues ready to work on (unblocked, not deferred).

```bash
br ready [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--limit <N>` | Maximum results (0=unlimited; default: unlimited — the full ready set). Pass `--limit N` to cap. |
| `--assignee <NAME>` | Filter by assignee |
| `--unassigned` | Show only unassigned |
| `-l, --label <LABEL>` | Filter by label (AND logic) |
| `--label-any <LABEL>` | Filter by label (OR logic) |
| `-t, --type <TYPE>` | Filter by type |
| `-p, --priority <N>` | Filter by priority |
| `--sort <POLICY>` | Sort: hybrid (default), priority, oldest |
| `--include-deferred` | Include deferred issues |
| `--parent <ID>` | Filter to children of a parent issue |
| `-r, --recursive` | Include all descendants with `--parent` |
| `--wrap` | Wrap long lines instead of truncating in text output |
| `--format <FMT>` | Output format: text, json, toon |
| `--stats` | Show token savings stats when using TOON output |
| `--robot` | Machine-readable output |

**Examples:**
```bash
# My ready work
br ready --assignee $(whoami)

# Unassigned high-priority
br ready --unassigned -p 0 -p 1

# JSON for agent integration
br ready --json --limit 10
```

**Configurable ready status group (`.beads/policy.yaml`):**

By default, `br ready` treats only `open` issues as actionable. Projects with a
review workflow can widen what "ready" means — so review-returned work (e.g.
`rework`) resurfaces through the same `br ready --json` entrypoint instead of
forcing workflow knowledge into every agent prompt:

```yaml
workflow:
  status_groups:
    ready:
      - open
      - rework
```

Semantics:
- **Default:** when `workflow.status_groups.ready` is absent (or empty), the
  group is `[open]` — exactly the pre-#354 behavior (zero change for existing
  repos).
- **Status preserved:** returned issues keep their real status, so a `rework`
  item still emits `{"status":"rework"}` in `--json`/`--format toon`/`--robot`.
- **Validation:** when `workflow.strict: true` (and `workflow.statuses` is set),
  every member of the ready group must be in `workflow.statuses`; an
  out-of-vocabulary member is rejected with a clear error. Without `strict`, the
  group is accepted as-is.
- **Deferred interaction:** the `defer_until` time-gate still applies to every
  non-`deferred` member of the group, so a configured member with a future
  `defer_until` stays out of `br ready` until it elapses. `--include-deferred`
  additionally surfaces `deferred` work and drops the time-gate, without
  double-counting `deferred` if it is also listed in the group.
- **Scope:** `br ready`, `br ready --json`, `br ready --robot`,
  `br ready --format toon`, and `br scheduler` all use the same ready group.

**Atomic workflow capacity (`.beads/policy.yaml`):**

Repository-level hard limits and transition-scoped admission guards are
configured under `workflow.capacity`. Every referenced status must be declared
in `workflow.statuses`; unknown fields, zero thresholds, undeclared references,
and a soft threshold greater than its hard threshold fail closed while loading
the policy.

```yaml
workflow:
  statuses: [open, in_progress, in_review, rework, closed]
  capacity:
    statuses:
      in_progress:
        hard: 3
    groups:
      active_work:
        statuses: [in_progress, in_review, rework]
        hard: 5
    admission:
      - name: drain_review_before_starting
        transitions:
          from: [open]
          to: [in_progress]
        require_below:
          statuses:
            in_review: 2
          groups:
            active_work: 5
```

- A hard limit of `N` admits the transition that reaches `N` and rejects a
  transition that would reach `N + 1`.
- Named groups count the union of their configured statuses without duplicate
  members.
- Admission requirements are exclusive: `in_review: 2` requires the
  prospective observed count to remain below 2 for matching transitions.
- Enforcement and mutation share one `BEGIN IMMEDIATE` transaction. Rejections
  therefore cannot race another writer and roll back every field in the update.
- Draining an overfull status/group is always allowed. JSONL import remains a
  state-replication path rather than a new-work admission path.
- Reaching a soft threshold still commits. Human output emits an actionable
  warning; JSON and TOON add a structured `warnings` array only when warnings
  exist, preserving the legacy success shape below the threshold.
- Each warning contains `issue_id`, `from_status`, `to_status`,
  `capacity_kind`, `capacity_name`, `scope`, `counting_mode`, `current`,
  `prospective`, `soft_limit`, optional `hard_limit`, and `policy_path`.
  `update` wraps its normal array as `{updated, warnings}` and `create` as
  `{created, warnings}`; commands that already return an object add `warnings`
  to that object. The wrapper is never introduced below the soft threshold.
- Multi-target `update`/`--claim`, `close`, `reopen`, `defer`, and `undefer`
  commands evaluate the repository's final prospective state and commit all
  status changes in one transaction. Hard-limit and late validation failures
  roll back the entire repository-local batch; capacity-neutral swaps do not
  depend on request order.
- Routed commands transact each repository independently. There is no
  distributed transaction across repositories, so an earlier route may already
  be committed if a later route fails and cross-repository atomicity is
  intentionally not claimed.
- Omitting `workflow.capacity` preserves existing behavior exactly.
- The current enforcement layer has fixed `repository` scope and `all`
  counting. Hierarchy-aware counting, audited exemptions, actor/assignee/
  harness/session/subtree scopes, and capacity observability are later phases
  tracked in GitHub issue #384.

---

### scheduler

Rank ready work for agent swarms with explainable evidence.

```bash
br scheduler [OPTIONS]
br schedule [OPTIONS]   # alias
```

`scheduler` starts from the same ready-work definition as `ready`, then scores a
bounded candidate set with deterministic evidence terms for priority,
dependency impact, stale claims, fairness, and domain contention. JSON and TOON
output include `schema: "br.scheduler.v1"` plus a fallback policy so agents can
parse the result safely and preserve conservative ordering when evidence ties.
The `evidence.stale_claim` object uses the shared coordination policy with
`reservation_status: "no_snapshot"` because `scheduler` does not parse Agent
Mail snapshots. A stale assigned row can therefore recommend `inspect_mail`, but
it is not proof that the claim is abandoned; run `br coordination status` with
reservation evidence before reclaiming ownership.

**Options:**
| Option | Description |
|--------|-------------|
| `--limit <N>` | Maximum recommendations (0=unlimited; default: unlimited — every scored recommendation) |
| `--candidate-limit <N>` | Maximum ready candidates to score (default: 512, 0=unlimited) |
| `--stale-claim-hours <N>` | Non-negative claim age threshold for stale-claim evidence (default: 2) |
| `--format <FMT>` | Output format: text, json, toon |
| `--stats` | Show token savings stats when using TOON output |
| `--robot` | Machine-readable output |

**Examples:**
```bash
# Top swarm recommendations with evidence
br scheduler --json --limit 10

# Token-efficient parseable output
br scheduler --format toon --stats
```

---

### coordination status

Diagnose hidden `in_progress` claims without mutating ownership.

```bash
br coordination status [OPTIONS]
```

`coordination status` emits the `br.coordination.v1` evidence envelope used to
spot stale claims, missing Agent Mail evidence, and active reservation matches.
The command is read-only: it never calls Agent Mail directly and never changes
issue status or assignee.

**Options:**
| Option | Description |
|--------|-------------|
| `--owner-kind <KIND>` | Fallback ownership policy: swarm-agent, human, or unknown |
| `--comments <N>` | Latest comments to include per claim (default: 2) |
| `--reservations <PATH>` | Offline Agent Mail reservation snapshot (JSON array, wrapper object, or JSONL) |
| `--agents <PATH>` | Offline Agent Mail agent snapshot (JSON array, wrapper object, or JSONL) |
| `--format <FMT>` | Output format: text, json, toon |
| `--stats` | Show token savings stats when using TOON output |
| `--robot` | Machine-readable output |

JSON/TOON claim rows include advisory fields:
`reclaim_allowed_by_policy`, `required_human_confirmation`,
`evidence_summary`, and `suggested_commands`. Suggested commands are emitted
only when the policy has enough evidence to propose the documented audit-comment
plus `br update --claim` sequence. Fresh claims, active reservations, missing or
invalid snapshots, and human/unknown ownership do not emit reclaim commands.

**Examples:**
```bash
# Inspect current in-progress claims
br coordination status --json

# Queue-dry diagnosis: ready work may be hidden behind old claims
br ready --json
bv --robot-next
br list --status in_progress --json
br coordination status --json

# Use offline Agent Mail snapshots without requiring a live MCP service
br coordination status --reservations reservations.json --agents agents.jsonl --json

# Review advisory reclaim output before copying any suggested command
br coordination status --reservations reservations.json --agents agents.jsonl --json \
  | jq '.claims[] | {id: .issue.id, reclaim_allowed_by_policy, required_human_confirmation, suggested_commands}'
```

---

### blocked

List blocked issues.

```bash
br blocked [OPTIONS]
```

Shows issues that are blocked by other open issues.

**Options:**
| Option | Description |
|--------|-------------|
| `--limit <N>` | Maximum results (default: 50, 0=unlimited) |
| `--detailed` | Include full blocker details in text output |
| `--wrap` | Wrap long lines instead of truncating in text output |
| `-t, --type <TYPE>` | Filter by type |
| `-p, --priority <N>` | Filter by priority |
| `-l, --label <LABEL>` | Filter by label |
| `--format <FMT>` | Output format: text, json, toon |
| `--stats` | Show token savings stats when using TOON output |
| `--robot` | Machine-readable output |

---

### search

Full-text search across issues.

```bash
br search <QUERY> [OPTIONS]
```

Supports all filter options from `list`. Unlike `list`/`ready` (which are
complete by default), `search` results are **capped at 50 by default**
(`--limit <N>`, `0`=unlimited) — a broad text query can match a large fraction
of the corpus, so a bounded, relevance-ordered result set is the default.

**Examples:**
```bash
# Search in all fields
br search "authentication"

# Search with filters
br search "bug" -t bug --assignee alice
```

---

### count

Count issues with optional grouping.

```bash
br count [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--by <FIELD>` | Group by: status, type, priority, assignee, label |
| `--by-status` | Group by status |
| `--by-priority` | Group by priority |
| `--by-type` | Group by issue type |
| `--by-assignee` | Group by assignee |
| `--by-label` | Group by label |
| `--status <STATUS>` | Filter by status (repeatable or comma-separated) |
| `--type <TYPE>` | Filter by issue type (repeatable or comma-separated) |
| `--priority <PRIORITY>` | Filter by priority (repeatable or comma-separated) |
| `--assignee <NAME>` | Filter by assignee |
| `--unassigned` | Only include unassigned issues |
| `--include-closed` | Include closed issues; use `--status tombstone` for tombstones |
| `--include-templates` | Include template issues |
| `--title-contains <TEXT>` | Title contains substring |

**Examples:**
```bash
# Total count
br count

# Count by status
br count --by status

# Count by assignee
br count --by assignee --json
```

---

### stale

List stale issues (not updated recently).

```bash
br stale [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--days <N>` | Issues not updated in N days (default: 30) |
| `--status <STATUS>` | Filter by status (repeatable or comma-separated) |

**Abandoned in-progress claims:**

`br ready` does not show `in_progress` issues. To audit hidden work, combine
`stale` with an explicit in-progress listing and inspect the claim evidence:

```bash
br stale --days 1 --json
br list --status in_progress --json
br show <id> --json
br comments list <id> --json
```

An `in_progress` issue is a reclaim candidate when `updated_at` is old, the
assignee or session metadata no longer points to an active worker, and recent
comments or Agent Mail reservations do not show live work. Default thresholds
are two hours for automated swarm claims and one business day for human or
unclear claims.

Before reclaiming, add an audit comment with the evidence, then claim:

```bash
br comments add <id> --author "$BD_ACTOR" \
  --message "reclaim: previous in_progress claim appears abandoned; evidence: updated_at=<timestamp>, assignee=<name>, no active reservation or pane" \
  --json
br update <id> --claim --json
```

There is not a separate reclaim command; the audit comment plus `update --claim`
is the documented recovery workflow.

---

## Organization Commands

### dep

Manage dependencies between issues.

```bash
br dep <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `add <ISSUE> <DEPENDS_ON>` | Add dependency (ISSUE depends on DEPENDS_ON) |
| `remove <ISSUE> <DEPENDS_ON>` | Remove dependency |
| `list <ISSUE>` | List dependencies of an issue |
| `tree <ISSUE>` | Show dependency tree |
| `cycles` | Detect dependency cycles |

**Dependency Types:**
- `blocks` (default) - Target blocks source
- `parent-child` - Hierarchical relationship
- `discovered-from` - Discovered during work on another issue
- `related` - Loosely related issues

**Examples:**
```bash
# Add blocking dependency
br dep add bd-123 bd-456  # bd-123 is blocked by bd-456

# Add with type
br dep add bd-123 bd-456 --type discovered-from

# Show tree
br dep tree bd-123

# Check for cycles
br dep cycles
```

---

### graph

Visualize the dependency graph for one issue or for all active connected
components.

```bash
br graph [OPTIONS] [ISSUE]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--all` | Show graph for all open, in-progress, and blocked issues |
| `--compact` | Print one line per issue |
| `--dot` | Emit Graphviz DOT notation (e.g. `br graph bd-1 --dot \| dot -Tsvg > graph.svg`) |

---

### label

Manage labels on issues.

```bash
br label <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `add [ISSUES]... --label <LABEL>` | Add a label to one or more issues |
| `remove [ISSUES]... --label <LABEL>` | Remove a label from one or more issues |
| `list [ID]` | List labels (optionally for specific issue) |
| `list-all` | List all unique labels with counts |
| `rename <OLD_NAME> <NEW_NAME>` | Rename a label across all issues |

---

### epic

Epic management commands.

```bash
br epic <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `status [--eligible-only]` | Show epic status with child progress and eligibility |
| `close-eligible [--dry-run]` | Close epics that are eligible because all children are closed |

---

### comments

Manage comments on issues.

```bash
br comments <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `add <ID> [TEXT]...` | Add a comment |
| `list <ID>` | List comments |

**Options:**
| Option | Description |
|--------|-------------|
| `--wrap` | Wrap long comment lines when listing |
| `add -f, --file <PATH>` | Read comment text from file |
| `add --author <NAME>` | Override the default author |
| `add --message <TEXT>` | Comment text as an alternative flag |
| `list --wrap` | Wrap long comment lines |

---

## Workflow Commands

### defer / undefer

Defer or undefer issues.

```bash
br defer <IDS>... [OPTIONS]
br undefer <IDS>... [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--until <DATE>` | Defer until date |
| `--robot` | Machine-readable output |

---

### orphans

List orphan issues (referenced in commits but still open).

```bash
br orphans [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--details` | Show detailed commit information |
| `--fix` | Prompt to fix orphans |
| `--robot` | Machine-readable output |

---

### query (saved queries)

Manage saved queries.

```bash
br query <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `save <NAME> [FILTERS...]` | Save the current list-style filter set as a named query |
| `run <NAME> [FILTERS...]` | Run a saved query, merging any additional filters from the CLI |
| `list` | List saved queries |
| `delete <NAME>` | Delete a saved query |

`query save` and `query run` use the same filter flags as `br list`; there is
no free-form query string argument.

---

### gate

Record and inspect workflow gate results (issue #312, layer 2). Gates are
conditions a project can require before a status transition is allowed, defined
in `.beads/policy.yaml` under `workflow.gates` as a map of `"from -> to"`
transitions to required gate conditions. Enforcement happens at the
close/transition chokepoint: a move into a gated state is rejected until every
required gate passes. Gate results are project-local metadata and are not synced
through JSONL.

```bash
br gate report <ID> --gate <NAME> --provider <NAME> --status pass|fail [OPTIONS]
br gate list <ID> [OPTIONS]
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `report <ID> --gate <NAME> --provider <NAME> --status pass\|fail` | Record a gate result (external systems / reviewers report here) |
| `list <ID>` | List recorded gate results and the computed required-gate status for the issue's next transitions |

**`report` options:**
| Option | Description |
|--------|-------------|
| `--gate <NAME>` | Gate name (e.g. `ci_green`, `security_sign_off`, `min_reviewers`) |
| `--provider <NAME>` | Reporting provider (e.g. `ci`, `security`, `reviewer:alice`) |
| `--status <pass\|fail>` | Result status |
| `--note <TEXT>` | Optional free-form note recorded with the result |
| `--robot` | Machine-readable JSON output |

**`list` options:**
| Option | Description |
|--------|-------------|
| `--robot` | Machine-readable JSON output |

A re-report from the same provider for the same gate overwrites the prior
verdict. The built-in `min_reviewers` gate is satisfied by at least N distinct
reviewer providers (provider name `reviewer`, or namespaced `reviewer:<who>` /
`reviewer-<who>`) reporting `pass`. Example policy:

```yaml
workflow:
  strict: true
  gates:
    "in_review -> closed":
      require_all:
        - ci_green
        - min_reviewers: 1
      require_if:
        - label: security-sensitive
          gate: security_sign_off
        - priority: [0, 1]
          gate: security_sign_off
```

---

## Sync & Config

### sync

Sync database with JSONL file.

```bash
br sync [OPTIONS]
```

**SAFETY GUARANTEES:**
- NEVER executes git commands or auto-commits
- NEVER modifies files outside the selected workspace's `.beads/` (unless `--allow-external-jsonl`)
- Uses atomic temp-file-then-rename pattern
- Safety guards prevent accidental data loss

**Modes (one required unless --status):**
| Option | Description |
|--------|-------------|
| `--flush-only` | Export database to JSONL |
| `--import-only` | Import JSONL into database |
| `--merge` | Three-way merge `.beads/beads.base.jsonl`, SQLite, and JSONL |
| `--status` | Show sync status (read-only) |

**Options:**
| Option | Description |
|--------|-------------|
| `-f, --force` | Override safety guards (use with caution) |
| `--force-db` | With `--merge`, resolve conflicts by keeping the local SQLite version |
| `--force-jsonl` | With `--merge`, resolve conflicts by keeping the JSONL version |
| `--allow-external-jsonl` | Allow JSONL path outside `.beads/` |
| `--manifest` | Write manifest file with export summary |
| `--error-policy <POLICY>` | Export error handling: strict, best-effort, partial, required-core |
| `--orphans <MODE>` | Orphan handling: strict, resurrect, skip, allow |
| `--rename-prefix` | During import, rewrite mismatched issue IDs into the configured default prefix |
| `--rebuild` | During import, rebuild SQLite from JSONL and remove DB entries absent from JSONL |
| `--robot` | Machine-readable output |

**Merge semantics:**
- `--merge` uses `.beads/beads.base.jsonl` as the common ancestor and compares it with the local SQLite database and current JSONL file.
- Without an explicit conflict policy, semantic conflicts stop the command. This covers both-modified, delete-vs-modify, and convergent same-ID creation conflicts.
- `--force-db` keeps local SQLite changes for conflicts, `--force-jsonl` keeps JSONL changes for conflicts, and `--force` chooses the side with the newer timestamp.
- `--force-db`, `--force-jsonl`, and `--force` are mutually exclusive for `--merge`.

**Rebuild semantics:**
- `--rebuild` is valid only with explicit import mode: `br sync --import-only --rebuild`.
- JSONL is authoritative. After import, entries present only in SQLite are removed; deletion tombstones are preserved when applicable.
- `--rebuild` is rejected with every non-import mode, including `--flush-only`, `--merge`, `--status`, and `--witness`.
- Recovery artifacts are preserved under `.beads/.br_recovery/` when br has to move aside a damaged SQLite family before rebuilding.
- If open-time recovery rebuilt the database before a semantic import flag such as `--rename-prefix` could apply, br prints a rerun command that includes the needed flags.

**Examples:**
```bash
# Export to JSONL explicitly; useful as a final check before committing .beads/
br sync --flush-only

# Import from JSONL
br sync --import-only

# Merge DB and JSONL after both changed
br sync --merge

# Resolve semantic merge conflicts explicitly
br sync --merge --force-db
br sync --merge --force-jsonl
br sync --merge --force

# Rebuild SQLite from authoritative JSONL
br sync --import-only --rebuild

# Rebuild while rewriting imported IDs to the configured prefix
br sync --import-only --rebuild --rename-prefix

# Check sync status
br sync --status

# Export with verbose logging
br sync --flush-only -v
```

---

### config

Configuration management.

```bash
br config <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `list [--project | --user]` | List available config options |
| `get <KEY>` | Get a specific config value |
| `set <KEY=VALUE>` or `set <KEY> <VALUE>` | Set a config value |
| `delete <KEY>` | Delete a config value; `unset` is an alias |
| `edit` | Open the user config file in `$EDITOR` |
| `path` | Show config file paths |

**Examples:**
```bash
# List all config
br config list

# Get specific value
br config get id.prefix

# Set value
br config set id.prefix=myproj
br config set id.prefix myproj

# Edit in editor
br config edit
```

---

## Agent Integration

### capabilities

Describe br's machine-readable command contracts, safety guarantees, supported
output formats, exit-code categories, and environment variables.

```bash
br capabilities [OPTIONS]
```

Use this as the first discovery call in automation:

```bash
br capabilities --format json
br capabilities --format json --command "create"
br capabilities --format json --command "comments add"
br capabilities --format json --command "dep add"
br capabilities --format json --command "query save"
br capabilities --format json --command "update"
```

**Options:**
| Option | Description |
|--------|-------------|
| `--command <COMMAND_PATH>` | Include detailed metadata for one command path, e.g. `create` or `comments add` |
| `--format <FMT>` | Output format: text, json, toon |
| `--stats` | Show token savings stats when using TOON output |

JSON and TOON output include `contract_version`,
`recommended_entrypoints`, `features`, `commands`, `global_flags`,
`exit_codes`, `env_vars`, and `safety`. When `--command` is supplied, output
also includes `command_detail` with canonical path, aliases, subcommands,
positionals, options, defaults, possible values, examples, command-specific
safety notes, and workspace/safety contract metadata.

---

### robot-docs

Print concise in-tool documentation for automation agents.

```bash
br robot-docs guide [OPTIONS]
```

Text mode prints a short handbook under 80 lines. JSON and TOON modes wrap the
same guide with `contract_version`, `line_count`, and canonical commands.

**Options:**
| Option | Description |
|--------|-------------|
| `--format <FMT>` | Output format: text, json, toon |
| `--stats` | Show token savings stats when using TOON output |

**Example:**

```bash
br robot-docs guide
br robot-docs guide --format json
```

---

### serve

Start an MCP (Model Context Protocol) server on stdio.

```bash
br serve [OPTIONS]
```

`serve` is only available in binaries built with the optional `mcp` feature:

```bash
cargo build --release --features mcp
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git beads_rust --locked --features mcp
```

**Options:**

| Option | Description |
|--------|-------------|
| `--actor <NAME>` | Actor name recorded for mutations (default: `mcp`) |

**Transport:** stdio. An MCP client launches `br serve`; `br` does not open a
network listener.

**Tools:** `list_issues`, `show_issue`, `create_issue`, `update_issue`,
`close_issue`, `manage_dependencies`, `project_overview`.

**Resources:** `beads://project/info`, `beads://issues/{id}`,
`beads://schema`, `beads://labels`, `beads://issues/ready`,
`beads://issues/blocked`, `beads://issues/in_progress`,
`beads://coordination/status`, `beads://issues/deferred`,
`beads://issues/bottlenecks`, `beads://graph/health`,
`beads://events/recent`.

**Prompts:** `triage`, `status_report`, `plan_next_work`, `polish_backlog`.

**Safety:** MCP mutations use the same local storage, audit trail, `.write.lock`,
and JSONL auto-flush behavior as CLI mutations. The server never runs git and
does not synchronize repositories. `beads://coordination/status` is read-only
and does not call Agent Mail; use `br coordination status --reservations
<PATH> --agents <PATH> --json` when reservation evidence is required.

**Example MCP client entry:**

```json
{
  "mcpServers": {
    "br": {
      "command": "br",
      "args": ["serve", "--actor", "codex"],
      "env": {
        "RUST_LOG": "error"
      }
    }
  }
}
```

Use `serve` when an MCP-native agent benefits from tool/resource discovery and
structured recovery hints. Use `br --json ...` when a shell pipeline or `jq`
script is simpler.

---

## Diagnostics & Info

### agents

Manage the Beads workflow section in an `AGENTS.md` file.

```bash
br agents [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--add` | Add beads workflow instructions to `AGENTS.md` |
| `--remove` | Remove beads workflow instructions from `AGENTS.md` |
| `--update` | Update beads workflow instructions to the latest version |
| `--check` | Check status only (default behavior) |
| `--dry-run` | Preview changes without modifying files |
| `-f, --force` | Skip confirmation prompts |

---

### stats / status

Show project statistics.

```bash
br stats [OPTIONS]
br status [OPTIONS]  # alias
```

**Options:**
| Option | Description |
|--------|-------------|
| `--by-type` | Show breakdown by issue type |
| `--by-priority` | Show breakdown by priority |
| `--by-assignee` | Show breakdown by assignee |
| `--by-label` | Show breakdown by label |
| `--activity` | Include recent activity stats explicitly |
| `--no-activity` | Skip recent activity stats |
| `--activity-hours <HOURS>` | Activity window in hours (default: 24) |
| `--format <FMT>` | Output format: text, json, toon |
| `--stats` | Show token savings stats when using TOON output |
| `--robot` | Machine-readable output |

---

### doctor

Run diagnostics and optionally repair issues.

```bash
br doctor [OPTIONS]
```

Checks database integrity, schema compatibility, and configuration.

**Options:**
| Option | Description |
|--------|-------------|
| `--repair` | Attempt to repair detected issues by rebuilding DB from JSONL |
| `--allow-repeated-repair` | Allow another JSONL rebuild after prior failed recovery evidence |

---

### info

Show workspace diagnostics and metadata.

```bash
br info [--schema] [--whats-new] [--thanks]
```

---

### where

Show the active `.beads` directory (after redirects, if any).

```bash
br where
```

---

### schema

Emit JSON Schemas for agent/tooling integrations.

```bash
br schema [TARGET] [OPTIONS]
```

**Targets:** `all`, `issue`, `issue-with-counts`, `issue-details`,
`ready-issue`, `stale-issue`, `blocked-issue`, `tree-node`, `statistics`,
`error`.

**Options:**
| Option | Description |
|--------|-------------|
| `--format <FMT>` | Output format: text, json, toon |
| `--stats` | Show token savings stats when using TOON output |

---

### version

Show version information.

```bash
br version
```

---

### audit

Record and label agent interactions.

```bash
br audit [OPTIONS]
```

Appends to `.beads/interactions.jsonl`.

**Subcommands:**
| Command | Description |
|---------|-------------|
| `record` | Append one interaction entry |
| `coordination` | Record coordination status rows as audit interactions |
| `label` | Label a prior interaction entry |
| `log` | View audit entries for an issue |
| `summary` | Summarize interaction counts |

#### audit coordination

`audit coordination` turns a `br coordination status` snapshot into durable
`coordination_incident` rows in the existing `.beads/interactions.jsonl` audit
log. It does not create a second coordination datastore.

```bash
br coordination status --json \
  | br audit coordination --stdin --command "br coordination status --json" --json
```

Input may be a `br.coordination.v1` status object with `claims`, a JSON array,
or JSONL rows where each row is either a claim or a wrapper with `claims`.
Each recorded row stores bounded normalized fields in `extra`: `command`,
`issue_id`, `classification`, `evidence_summary`, `snapshot_hash`, and
`suggested_action`. The snapshot hash is computed from stable JSON with object
keys normalized, so equivalent key order produces the same hash.

The text output prints one interaction id per recorded claim. JSON and TOON
output return:

```json
{
  "recorded": 1,
  "snapshot_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "ids": ["int-..."]
}
```

---

### history

Manage local history backups.

```bash
br history <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `list` | List backups |
| `restore <BACKUP>` | Restore from backup |

**Notes:**
- Backups are created during `br sync --flush-only` when overwriting a JSONL file inside `.beads/`, including custom `BEADS_JSONL` paths that still target `.beads/`.

---

### changelog

Generate changelog from closed issues.

```bash
br changelog [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--since <DATE>` | Include issues closed since date |
| `--format <FMT>` | Output format: markdown, json |

---

### lint

Check issues for missing template sections.

```bash
br lint [OPTIONS]
```

---

## Utilities

### upgrade

Upgrade br to the latest version.

```bash
br upgrade [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--check` | Check for updates without installing |
| `--force` | Force reinstall current version |

---

### completions

Generate shell completions.

```bash
br completions <SHELL>
```

**Shells:** bash, zsh, fish, powershell

**Example:**
```bash
# Add to ~/.bashrc
br completions bash >> ~/.bashrc
source ~/.bashrc
```

---

## Exit Codes

| Code | Category | Description |
|------|----------|-------------|
| 0 | Success | Command completed successfully |
| 1 | Internal | Internal error |
| 2 | Database | Database error (not initialized, schema mismatch) |
| 3 | Issue | Issue error (not found, ambiguous ID) |
| 4 | Validation | Validation error (invalid input) |
| 5 | Dependency | Dependency error (cycle detected, self-dependency) |
| 6 | Sync/JSONL | Sync error (parse error, conflict markers) |
| 7 | Config | Configuration error |
| 8 | I/O | I/O error (file not found, permission denied) |

---

## Environment Variables

| Variable | Description |
|----------|-------------|
| `BEADS_DIR` | Override `.beads` directory location |
| `BEADS_JSONL` | Override JSONL file path (requires `--allow-external-jsonl`) |
| `BD_ACTOR` | Default actor name for audit trail |
| `EDITOR` | Editor for `br config edit` |
| `NO_COLOR` | Disable colored output (any value) |
| `RUST_LOG` | Logging level (debug, info, warn, error) |

Recommended routine default:

```bash
export RUST_LOG=error
```

This keeps successful commands readable by suppressing low-level dependency logs. Override it with `debug`/`trace` when troubleshooting.

---

## JSON Output Schemas

### Issue Object (list, show, ready)

```json
{
  "id": "bd-abc123",
  "title": "Issue title",
  "description": "Full description text",
  "design": "",
  "acceptance_criteria": "",
  "notes": "",
  "status": "open",
  "priority": 2,
  "issue_type": "task",
  "assignee": "alice",
  "owner": "owner@example.com",
  "created_at": "2025-01-15T10:30:00Z",
  "created_by": "bob",
  "updated_at": "2025-01-16T14:20:00Z",
  "close_reason": "",
  "closed_by_session": "",
  "source_system": "",
  "deleted_by": "",
  "delete_reason": "",
  "sender": "",
  "dependency_count": 0,
  "dependent_count": 3
}
```

### Dependency Object

```json
{
  "issue_id": "bd-abc123",
  "depends_on_id": "bd-def456",
  "dep_type": "blocks",
  "created_at": "2025-01-15T10:30:00Z",
  "created_by": "alice"
}
```

### Sync Status Object

```json
{
  "db_path": ".beads/beads.db",
  "jsonl_path": ".beads/issues.jsonl",
  "db_modified": "2025-01-16T14:20:00Z",
  "jsonl_modified": "2025-01-16T14:15:00Z",
  "db_issue_count": 150,
  "jsonl_issue_count": 148,
  "dirty_count": 2,
  "status": "db_newer"
}
```

### Error Object

```json
{
  "error_code": 3,
  "message": "Issue not found: bd-xyz999",
  "kind": "not_found",
  "recovery_hints": ["Check the issue ID", "Use 'br list' to find issues"]
}
```

---

## See Also

- [README.md](../README.md) - Project overview
- [AGENTS.md](../AGENTS.md) - Agent integration guide
- [SYNC_SAFETY.md](SYNC_SAFETY.md) - Sync safety model
