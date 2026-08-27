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
  - [capacity](#capacity)
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
| `-s, --status <STATUS>` | Filter by status (can repeat; `all` matches every status) |
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
| `--transition-comment <TEXT>` | Add a fresh comment atomically with a status transition |
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
| `--transition-comment <TEXT>` | Add a fresh comment atomically with the close transition |
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
  `capacity_kind`, `capacity_name`, `scope`, optional `scope_key`,
  `counting_mode`, `current`, `prospective`, `soft_limit`, optional
  `hard_limit`, and `policy_path`.
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
- Audited issue-specific exemptions (`br capacity exempt`, see
  [capacity](#capacity)) let a named issue enter a named capacity without
  consuming a slot; evidence then reports counted and exempt totals
  separately.
- Optional multi-agent admission scopes are configured under
  `workflow.capacity.scopes` (see below).
- Occupancy is observable without mutating anything: once any capacity is
  configured, `br stats --json` and `br coordination status --json` carry a
  `capacity` array (one row per repository capacity plus one per occupied
  scope partition) with `counted`, `aggregate_parents_excluded`, `exempt`,
  `soft_limit`, `hard_limit`, `remaining`, and a `state` of
  `healthy`/`soft-limit`/`at-hard`/`over-hard`; human `br stats` prints the
  matching CAPACITY/COUNTED/AGGREGATES/EXEMPT/SOFT/HARD/REMAINING/STATE
  table. The block is absent when no capacity is configured, keeping the
  legacy payload shapes byte-stable, and the snapshot never writes (lazy
  exemption expiry stays pending for the next enforcement observation).
  The complete GitHub #384 acceptance matrix lives in
  [GH384_ACCEPTANCE_MATRIX.md](GH384_ACCEPTANCE_MATRIX.md).

**Multi-agent admission scopes (`workflow.capacity.scopes`):**

Beyond the repository totals, capacity limits can partition occupancy by who
is doing the work:

```yaml
workflow:
  statuses: [open, in_progress, in_review, rework, closed]
  capacity:
    scopes:
      actor:
        statuses:
          in_progress:
            hard: 2
      harness:
        groups:
          active_work:
            statuses: [in_progress, in_review, rework]
            hard: 6
      subtree:
        statuses:
          in_progress:
            soft: 2
```

- Recognized scopes: `repository`, `actor`, `assignee`, `harness`,
  `session`, and `subtree`. Each scope carries its own `statuses`/`groups`
  limit tables; every applicable scope composes with (never replaces) the
  repository-level limits, and a transition must satisfy all of them inside
  the same admission transaction.
- Partition keys: `actor` uses the resolved CLI actor; `harness` uses the
  self-reported `--harness`/`BR_HARNESS` attribution; `session` uses the
  env-only `BR_SESSION` attribution; `assignee` uses the issue's prospective
  assignee; `subtree` uses the issue's root ancestor over parent-child
  edges. Every committed status transition records its admitting
  actor/agent/harness/session in the project-local `capacity_occupancy`
  table (never synced to JSONL, never written by import), and scoped counts
  are measured against those records.
- This is admission control for cooperating agent harnesses, not
  authentication or process supervision: attribution is self-reported, and
  a transition that carries no key for a scope is not subject to that
  scope's limits (`actor` always has a key; `assignee` skips unassigned
  transitions).
- Only partitions whose count would increase are checked, so departures,
  cross-partition handoffs, and same-key finish-and-claim swaps in one
  batch remain admissible at the cap. Active exemptions free their issue's
  slot in every scope measuring the same capacity.
- Scoped counting is plain per-issue occupancy: `counting.hierarchy` modes
  and `admission` rules remain repository-scope features. Scoped evidence
  reports `counting_mode: "all"`, carries the partition key in `scope_key`,
  and uses `workflow.capacity.scopes.<scope>...` policy paths; repository
  evidence keeps its pre-scope shape byte-for-byte (no `scope_key` field).

**Hierarchy-aware counting (`workflow.capacity.counting`):**

By default every matching issue counts. `counting.hierarchy` changes how
occupancy is measured so an aggregate parent and its executable child do not
each consume a slot. Only `parent-child` dependency edges participate;
`blocks` and `related` edges never affect counting.

```yaml
workflow:
  capacity:
    counting:
      hierarchy: leaf_work
    groups:
      active_work:
        statuses: [in_progress, in_review]
        hard: 8
```

| Mode | Behavior |
|------|----------|
| `all` | Every matching issue counts. The default; unchanged from earlier phases. |
| `leaf_work` | An issue does not count while a descendant is already counted in the same capacity. Active leaves count one, aggregate parents count zero, and a parent begins counting when its last active descendant leaves. |
| `roots` | Count active work streams by their highest matching ancestor: an issue counts only when no ancestor is active in the same capacity. |
| `weighted` | Sum explicit `counting.weights` over matching issues. |

For a group containing `in_progress` and `in_review`, an epic → parent →
{child A `in_progress`, child B `in_review`} tree consumes two `leaf_work`
slots, not four.

Weights are resolved per issue as: `counting.weights.issues.<id>`, then
`counting.weights.types.<type>` (case-insensitive), then
`counting.weights.default`, then `1`. A weight of `0` is the explicit,
visible way to declare that a parent represents no independent execution
beyond its children. Configuring `counting.weights` without
`hierarchy: weighted` is rejected while loading the policy rather than
silently ignored.

```yaml
workflow:
  capacity:
    counting:
      hierarchy: weighted
      weights:
        default: 1
        types:
          epic: 0
        issues:
          br-migration: 3
```

- Hierarchy counting runs inside the same `BEGIN IMMEDIATE` transaction as
  admission, so concurrent transitions cannot disagree about the tree.
- Each issue is counted at most once per capacity. Parent-child cycles from
  imported data are condensed into one component, so every active member of a
  cycle stays counted rather than cancelling out.
- Under `leaf_work` and `roots`, capacity evidence adds
  `aggregate_parents_excluded`: the number of active issues that did not count
  because a relative already covers their work stream. `counting_mode` reports
  the active mode. Under `all` both the field and the message text are
  unchanged from earlier phases.

**Derived rollup status:**

`br show --json` adds a `rollup` object to any issue that has local
parent-child children, without mutating the issue's own status:

```json
{"id":"br-epic","status":"open",
 "rollup":{"status":"in_progress","descendants":{"in_progress":1,"closed":2}}}
```

`rollup.status` is the furthest-along non-terminal descendant status —
ranked by position in `workflow.statuses` when configured, otherwise by a
built-in `draft < deferred < open < blocked < in_progress` ladder — or
`closed` when every descendant is terminal. `rollup.descendants` counts the
whole strict subtree by stored status. Issues without children omit the key
entirely, and the JSONL fallback show paths do not emit it.

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
of the corpus, so a bounded, relevance-ordered result set is the default. Text
and CSV output explicitly note when more matches exist; JSON/TOON reports
`limit`, `offset`, and `has_more`.

**Closed issues are excluded by default** (tombstones always). When that
exclusion hides matches, text output ends with a trailing note
(`note: N closed match(es) hidden; rerun with --all to include them`), and
JSON/TOON output always uses the stable wrapper
`{"issues": [...], "hidden_closed_count": N, "limit": N, "offset": N,
"has_more": bool}`. The hidden count is zero when nothing was hidden or the
selected corpus already includes closed issues. Pass `--all` (or a terminal
`--status` such as `closed`) to include closed issues.

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

An issue reachable through more than one parent (a diamond) is listed under
every parent, but only its first occurrence expands the subtree beneath it.
Later occurrences are marked `(shown above)` in text output and carry
`"repeat": true` in `--json`. This keeps the output bounded by the size of the
dependency graph instead of by the number of distinct paths through it, which
is what made deep traversals of shared-dependency graphs explode
([#392](https://github.com/Dicklesworthstone/beads_rust/issues/392)).
`truncated` is unrelated and still means "children exist but `--max-depth`
stopped the walk".

**Cycle semantics
([#391](https://github.com/Dicklesworthstone/beads_rust/issues/391)):**

- Only *blocking* dependency types are cycle-checked when an edge is added:
  `blocks`, `conditional-blocks`, `waits-for`, and `parent-child`.
  `related`, `discovered-from`, and custom types are never cycle-checked,
  and `br dep cycles` uses the same blocking edge set, so an edge the add
  path accepted can never fail a cycle health check afterwards
  (`--blocking-only` is a compatible alias of the default).
- **Epic containment participates in blocking cycles, reversed:** depending
  on an epic means depending on its entire subtree, now and future, so the
  traversal walks parent → child containment edges. A consequence: once any
  issue's blocks-chain reaches an epic, no *descendant* of that epic may add
  a `blocks` edge back into that chain — the add is rejected as a cycle even
  though no `child → parent` edge exists. This is the intended
  "depend-on-epic = depend-on-all-descendants" rule; the rejection's cycle
  may traverse containment edges that the error path does not list. To
  express a loose association that must never gate work or trip cycle
  checks, use `-t related`.

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
| `close-eligible [--dry-run] [--transition-comment <TEXT>]` | Atomically close eligible epics; attach one fresh transition comment to each |

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
| `--transition-comment <TEXT>` | Add a fresh comment atomically with each status transition |
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
| `report <ID> --gate <NAME> --provider <NAME> --status pass\|fail [--to <STATUS>]` | Append a transition-scoped gate result (external systems / reviewers report here) |
| `list <ID>` | List recorded gate results and the computed required-gate status for the issue's next transitions |

**`report` options:**
| Option | Description |
|--------|-------------|
| `--gate <NAME>` | Gate name (e.g. `ci_green`, `security_sign_off`, `min_reviewers`) |
| `--provider <NAME>` | Reporting provider (e.g. `ci`, `security`, `reviewer:alice`) |
| `--status <pass\|fail>` | Result status |
| `--to <STATUS>` | Target transition; optional only when exactly one configured target requires this gate |
| `--note <TEXT>` | Optional free-form note recorded with the result |
| `--robot` | Machine-readable JSON output |

**`list` options:**
| Option | Description |
|--------|-------------|
| `--robot` | Machine-readable JSON output |

A re-report appends to immutable history; only the latest result from each
`(gate, provider)` in the exact `(issue, source, target, status revision)` scope
is effective. Leaving and later re-entering the source status creates a new
revision, so an earlier review pass cannot authorize the new attempt. Legacy
pre-v15 unscoped results remain audit-visible but never satisfy a transition.
The built-in `min_reviewers` gate is satisfied by at least N distinct
reviewer providers (provider name `reviewer`, or namespaced `reviewer:<who>` /
`reviewer-<who>`) reporting `pass`. Example policy:

```yaml
workflow:
  strict: true
  required_fields:
    in_review:
      - transition_comment
    "in_progress -> in_review":
      - acceptance_criteria
      - transition_comment
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

`required_fields` accepts exact `"from -> to"` keys and bare target-status
keys; matching rules compose. `acceptance_criteria` validates the prospective
field value and rejects any unchecked markdown checklist item.
`transition_comment` must be a new non-empty comment carried by the same
request; old comments are intentionally ignored. Validation and comment/status
mutation share one transaction, and a failed item rolls back the entire
repository-local batch. Supply comments with `update`, `close`, `defer`, and
`undefer` via `--transition-comment`; `reopen --reason` and
`epic close-eligible --transition-comment` use the same atomic path.

---

### capacity

Audited issue-specific capacity exemptions (GitHub #384 phase 4). A
long-lived external blocker may legitimately remain in a limited status;
an exemption lets that one issue occupy one named capacity without
consuming a slot, while staying visible in queue metrics. Exemption records
are project-local metadata (like gate results) and are not synced through
JSONL.

```bash
br capacity exempt <ID> --status <NAME>|--group <NAME> --provider <P> --reason <TEXT> [--expires <WHEN>]
br capacity renew <ID> --status <NAME>|--group <NAME> --provider <P> [--expires <WHEN>] [--reason <TEXT>]
br capacity revoke <ID> --status <NAME>|--group <NAME> --provider <P> [--reason <TEXT>]
br capacity exemptions [<ID>] [--history]
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `exempt <ID>` | Grant an exemption: one issue, one named capacity. Requires an authorized `--provider` and a non-empty `--reason`. |
| `renew <ID>` | Extend an active exemption's expiry. Expired or ended exemptions cannot be renewed — grant a new one so the audit trail shows the gap. |
| `revoke <ID>` | Withdraw an active exemption. Deliberately not provider-gated so cleanup stays possible after policy edits; provider and actor are still recorded. |
| `exemptions [<ID>]` | List exemption state (`active`, `expired`, `revoked`, `left_status`) and, with `--history`, the append-only audit trail. |

**Common options:**
| Option | Description |
|--------|-------------|
| `--status <NAME>` | The named status capacity (a status with a configured limit, or one observed by an admission rule) |
| `--group <NAME>` | The named capacity group from `workflow.capacity.groups` |
| `--provider <NAME>` | Approving provider; grants/renewals require it to be listed in policy |
| `--reason <TEXT>` | Rationale recorded with the action (mandatory for grants) |
| `--expires <WHEN>` | Expiry: RFC3339, `YYYY-MM-DD`, or relative (`+7d`) |
| `--robot` | Machine-readable JSON output |

Authorization lives in `.beads/policy.yaml`:

```yaml
workflow:
  statuses: [open, in_progress, blocked, closed]
  capacity:
    statuses:
      blocked:
        hard: 12
    exemptions:
      providers: [operator]     # empty/absent disables granting entirely
      require_expiry: true      # optional: every grant must carry an expiry
      max_ttl_seconds: 1209600  # optional: cap the expiry horizon
```

Semantics:

- An active, authorized exemption removes its issue from the named
  capacity's counted total everywhere that capacity is measured — its own
  limit and any admission rule observing the same status or group. Evidence
  and warnings then carry a separate `exempt` total alongside
  `current`/`prospective` (and `aggregate_parents_excluded` under hierarchy
  counting, where an exempted issue still suppresses its ancestors, so an
  exemption can only lower a count, never raise one).
- Leaving the applicable status set ends the exemption in the same
  transaction, with an audited `left_status` record; re-entry needs a new
  grant.
- Expired exemptions count again immediately; the audited `expire` record
  is written by the first committed observation.
- Removing a provider from `exemptions.providers` withdraws the effect of
  every exemption it granted without rewriting audit history; re-listing
  the provider restores them.
- Ordinary labels can never grant an exemption, and grant/renew/revoke/
  expire actions all land in the append-only `capacity_exemption_history`
  audit table.

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
- Publishes JSONL/base/manifest files with checked temporary-file replacement;
  database mutations use transactions and operation-specific rollback guards
- Safety guards prevent accidental data loss
- `--status` does not probe Git; its stable `git_export` compatibility object
  reports `available: false`, `reason: "not_probed"`, and
  `diagnostic_command: "br vcs-status --json"`

**Modes (exactly one required):**
| Option | Description |
|--------|-------------|
| `--flush-only` | Export database to JSONL |
| `--import-only` | Import JSONL into database |
| `--merge` | Three-way merge `.beads/beads.base.jsonl`, SQLite, and JSONL |
| `--reconcile` | Additively reconcile JSONL into the database (lossless, previewable) |
| `--status` | Show sync status (read-only) |
| `--witness` | Compute a deterministic read-only JSONL integrity witness |
| `--reconcile-additive` | Plan a lossless exact-ID JSONL-to-SQLite reconciliation (read-only by default) |

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
| `--rename-prefix` | During import, rewrite mismatched issue-ID prefixes into the configured default prefix, preserving the id remainder |
| `--skip-invalid-records` | With `--import-only`, explicitly salvage valid JSONL records while preserving and reporting every rejected source line |
| `--rebuild` | During import, rebuild SQLite from JSONL and remove DB entries absent from JSONL |
| `--dry-run` | With `--reconcile`, preview the plan without any mutation |
| `--apply` | Apply a conflict-free `--reconcile-additive` plan transactionally |
| `--expect-plan-sha256 <SHA256>` | Required with additive `--apply`; must equal the exact reviewed dry-run token |
| `--resolve-source-id <ISSUE_ID>` | Explicitly choose the allowed non-lifecycle JSONL scalar fields for one reviewed shared-ID conflict when JSONL is not older; repeat per ID |
| `--robot` | Machine-readable output |

**Merge semantics:**
- `--merge` uses `.beads/beads.base.jsonl` as the common ancestor and compares it with the local SQLite database and current JSONL file.
- Without an explicit conflict policy, semantic conflicts stop the command. This covers both-modified, delete-vs-modify, and convergent same-ID creation conflicts.
- `--force-db` keeps local SQLite changes for conflicts, `--force-jsonl` keeps JSONL changes for conflicts, and `--force` chooses the side with the newer timestamp.
- `--force-db`, `--force-jsonl`, and `--force` are mutually exclusive for `--merge`.

**Reconcile semantics:**
- `--reconcile` classifies every JSONL row against full issue state — never the cached content hash — so it repairs the "false equal" state where `br sync --status` reports synchronized while the JSONL holds rows the database never imported.
- Classification is timestamp-newer-wins with tombstone protection: JSONL-only rows are created, strictly newer JSONL rows update in place, and everything else is skipped. Deletion is structurally impossible in this mode.
- Apply runs in one write transaction bound to plan-time witnesses (JSONL content hash + stat, event-table shape). Any divergence between plan and apply rolls the whole transaction back.
- Audit events are never created, modified, or deleted; the apply transaction verifies the events table is byte-stable and aborts otherwise.
- Existing dependencies, labels, comments, dirty markers, and tombstones survive unless superseded by an applied row. Database-only issues are never touched; they (and skipped rows whose local copy still diverges) mark the database for a future flush.
- No JSONL, base snapshot, manifest, or history writes ever happen; the same transaction repairs the stored content hash and stat witness so the staleness short-circuit becomes truthful again.
- `--dry-run` is read-only: no write transaction, no metadata/cache/dirty changes, no file writes of any kind.
- Both modes emit a versioned receipt (`br.sync.reconcile.v1` — see `br schema all`, key `SyncReconcileReceipt`) with source/target witnesses, created/updated/skipped/deleted counts (deleted is always 0), bounded id previews, relation counts, and before/after event counts.
- `--force`, `--rename-prefix`, and `--orphans` are rejected with `--reconcile`; dangling dependency references are cleaned only from rows the reconcile itself wrote.

**Rebuild semantics:**
- `--rebuild` is valid only with explicit import mode: `br sync --import-only --rebuild`.
- JSONL is authoritative. After import, entries present only in SQLite are removed; deletion tombstones are preserved when applicable.
- `--rebuild` is rejected with every non-import mode, including `--flush-only`, `--merge`, `--status`, and `--witness`.
- Recovery artifacts are preserved under `.beads/.br_recovery/` when br has to move aside a damaged SQLite family before rebuilding.
- If open-time recovery rebuilt the database before a semantic import flag such as `--rename-prefix` could apply, br prints a rerun command that includes the needed flags.

**Prefix rename semantics (`--rename-prefix`):**
- Only the prefix segment is replaced; the id remainder (descriptive slug and hash) is preserved: `oldp-cargo-license-spdx-ay8` becomes `newp-cargo-license-spdx-ay8`.
- A doubled prefix collapses exactly once, never recursively: `oldp-oldp-central-build-inputs-3un` becomes `newp-central-build-inputs-3un`.
- If the preserved id would collide with an existing id (or the old id has no separable prefix), that issue falls back to a freshly generated id and the receipt marks it with `fallback` (`regenerated-on-collision` or `regenerated-unparseable-id`).
- Each renamed issue's old id is stashed in its `external_ref` when that field was empty.
- The import output reports every rewrite as a `prefix_renames` list of `{old_id, new_id, fallback?}` entries (text and `--json`/`--robot`); use it to fix up external references. The field is omitted from JSON when no rename happened.
- Without `--force`, the import short-circuits (skipping the rename) when the JSONL content hash is unchanged since the last import; and a following `br sync --flush-only` needs `--force` to write the renamed ids back to the JSONL.

**Malformed-record salvage (`--skip-invalid-records`):**
- This is an explicit recovery operation and is valid only with `--import-only`; normal import remains fail-closed on every invalid record.
- Merge-conflict markers are never skipped. Resolve `<<<<<<<` / `=======` / `>>>>>>>` regions before salvage.
- br validates each nonblank line as a complete issue record, rejects invalid or duplicate records, and refuses to publish an empty survivor set.
- Before replacing the tracked JSONL, br stores the exact original bytes in a protected, non-rotating `.beads/.br_history/*pre-salvage*.jsonl` backup with target metadata.
- The cleaned generation is staged, revalidated, conditionally published under the JSONL-family write authority, and then imported from the exact published snapshot.
- Text output names every rejected line up to the normal human witness limit. `--json`/`--robot` emits the complete `salvage` receipt, including source/recovered digests, all line/error entries, the exact backup path, and publication atomicity.

**Additive reconciliation semantics:**
- `br sync --reconcile-additive --robot` is the default dry-run. It opens the current database read-only, compares exact issue IDs, and emits a hash-bound `br.sync.additive-reconciliation.v2` receipt plus a `plan_sha256` review token.
- The planner preserves SQLite-only issues, audit events, close metadata, gate-result history, runtime config, and every unmodified relation row. It never performs content-hash identity merges, physical deletes, JSONL writes, base-snapshot writes, or merge-note writes.
- JSONL-only IDs are created. For a shared ID, only an `open`/`in_progress` to `closed` transition whose scalar diff is limited to `status`, `updated_at`, `closed_at`, and `close_reason` is accepted automatically. Other drift is a conflict. Exact-ID `--resolve-source-id` is limited to the documented non-lifecycle scalar whitelist and is rejected when JSONL is older than SQLite.
- Explicit resolution never authorizes relation drift, tombstone resurrection, live-to-tombstone conversion, external-reference collision, orphan dependencies, or a newly introduced blocking cycle. Superfluous, duplicate, blank, and unknown resolution IDs are rejected.
- Comment IDs are storage-local surrogates. Every comment on a newly created issue is deterministically allocated from the next contiguous database-owned range and witnessed without changing issue ownership, author, text, or timestamp.
- `--apply` requires `--expect-plan-sha256`, re-resolves the terminal workspace and configured database, acquires that workspace's writer lock, re-plans, and compares the complete plan to the reviewed token before mutation. Source drift, database or raw-storage drift, resolution-set drift, event drift, child-table drift, count drift, schema/sequence drift, health-gate failure, or cache-projection mismatch rolls the transaction back.
- Stale or missing `issues.content_hash` values are explicit token-bound repairs. They never alter issue timestamps, relations, dirty tracking, or audit events, and a second plan must be a true no-op.
- The receipt distinguishes distinct conflicted issues from total conflict observations; includes complete ID/diff/remap manifests and their SHA-256 digests; and reports pre-existing, projected, and newly introduced blocking-cycle components.

**Examples:**
```bash
# Export to JSONL explicitly; useful as a final check before committing .beads/
br sync --flush-only

# Import from JSONL
br sync --import-only

# Recover valid rows from a historical JSONL containing malformed records
br sync --import-only --skip-invalid-records --json

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

# Preview an additive reconcile (read-only), then apply it
br sync --reconcile --dry-run --json
br sync --reconcile --json
# Inspect a lossless additive recovery plan
br sync --reconcile-additive --robot > /tmp/additive-plan.json

# If a scalar conflict is intentionally source-authoritative, re-plan with the
# exact ID. Repeat the flag for each independently reviewed conflict.
br sync --reconcile-additive \
  --resolve-source-id bd-example \
  --robot > /tmp/additive-plan.json

# Apply only the exact conflict-free plan that was reviewed.
br sync --reconcile-additive \
  --resolve-source-id bd-example \
  --apply \
  --expect-plan-sha256 "$(jq -r .plan_sha256 /tmp/additive-plan.json)" \
  --robot

# Check sync status
br sync --status

# Explicitly inspect the JSONL export's Git visibility
br vcs-status --json
```

`br sync --status` also runs a cheap DB↔JSONL **coverage probe**: the
exportable DB issue count (tombstones included, ephemerals/wisps excluded)
is compared against the JSONL's unique id count. When the byte/hash signals
say "current" but the sets differ — e.g. stored metadata lies about a
partial or lost import — the status reports **coverage drift** instead of
"In sync" (JSON: `coverage: {db_exportable_issues, jsonl_unique_ids}` and
`coverage_drift: true`) and points at `br sync --reconcile --dry-run`
(lossless) or `br sync --import-only --rebuild` (JSONL-authoritative).
The `--import-only` stored-hash shortcut applies the same invariant and
falls through to a real additive import instead of skipping.

```bash

# Export with verbose logging
br sync --flush-only -v
```

---

### vcs-status

Explicitly inspect Git visibility for the configured JSONL export. This is a
separate, user-requested diagnostic capability; no `br sync` mode delegates to
it or executes Git.

```bash
br vcs-status [--jsonl PATH] [--allow-external-jsonl] [--timeout-ms MILLISECONDS] [--json|--robot]
```

The machine-readable `br.vcs-export-status.v2` record reports:

- `observation_atomic: false`, because its exact evidence is collected by
  sequential probes rather than a transactional Git snapshot;
- repository `object_format` (`sha1` or `sha256`);
- exact HEAD and stage-zero index identities (`mode`, `object_type`, and
  `object_id`), plus explicit unmerged stages;
- `index_clean`, computed from exact HEAD/index mode and object identity;
- `worktree_state`: `clean`, `modified`, `deleted`, `untracked`, `ignored`,
  `unmerged`, `comparison_unavailable`, or `absent`;
- optional `worktree_clean`, which is omitted rather than guessed when an
  exact comparison is unavailable;
- a stable `worktree_comparison_reason` for unsupported index flags/modes,
  configured content transforms, unmerged indexes, or changed file identity;
- filter-free `worktree_raw_git_blob_hash` and `worktree_raw_sha256`, computed
  in-process from one securely opened immutable JSONL snapshot.

Repository and index evidence remains available when only the worktree
comparison is unavailable. Top-level unavailable results are reserved for
repository/probe failures and carry stable reasons such as `git_unavailable`,
`not_git_repository`, `path_unavailable`, `probe_timed_out`,
`probe_output_limit`, or `probe_failed`.

The command uses one shared execution budget across observation phases. It redirects stdout and
stderr to separate anonymous temporary files and polls each file against a
fixed limit. The probe clock starts before secure source capture. Capture,
Git subprocesses, capture reads, and in-process blob hashing check the shared
deadline between bounded operations; an individual filesystem read cannot
itself be preempted. On timeout or runner failure, br terminates and reaps the
direct child before returning; mandatory cleanup may extend past the probe
budget. This preserves distinct `probe_timed_out` and `probe_output_limit`
results without waiting for pipe EOF from a descendant that only inherited an
output descriptor.

Prompts, optional locks, hooks, fsmonitor, untracked-cache writes, paging, lazy
object fetches, and inherited Git redirections are disabled, and pathspecs are
literal. Fixed-key config probes intentionally observe effective
system/global/common/worktree settings and honor the caller's
`GIT_CONFIG_SYSTEM`/`GIT_CONFIG_GLOBAL`/`GIT_CONFIG_NOSYSTEM` read-location
overrides, so they see the same configuration files the caller's own `git`
would read; any configured content transform makes
the comparison unavailable without exposing its path. The command also
inspects repository-local attributes before comparing raw worktree bytes and
never executes clean/process filters or text conversions. Its sequential
HEAD/index/config/worktree observations are explicitly non-atomic. The selected
Git executable is nevertheless trusted: this diagnostic is not a process
sandbox and does not promise to terminate arbitrary daemonized descendants. External
paths require `--allow-external-jsonl` before the leaf is opened and are
reported only as a SHA-256 descriptor; raw external paths and Git stderr are
not emitted.

```bash
# Inspect the configured .beads/issues.jsonl
br vcs-status
br vcs-status --json

# Inspect an explicitly authorized external JSONL without exposing its path
br vcs-status \
  --jsonl /private/export/issues.jsonl \
  --allow-external-jsonl \
  --json
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

#### Reviewed schema migration

Ordinary commands never upgrade an existing database across a schema-version
boundary. If the database is on a supported older version, use the explicit
receipt-bound lifecycle:

```bash
# Read-only inspection. Save and review the complete JSON receipt.
br doctor migrate-schema plan --json > migration-plan.json

# Apply only if the database still matches the exact reviewed plan.
br doctor migrate-schema apply \
  --plan-token "$(jq -r .plan_token migration-plan.json)" \
  --json > migration-applied.json

# Verify that undo is still safe without changing the database.
br doctor migrate-schema undo \
  "$(jq -r .run_id migration-applied.json)" \
  --dry-run --json

# Restore the exact pre-migration SQLite family if necessary.
br doctor migrate-schema undo \
  "$(jq -r .run_id migration-applied.json)" \
  --json
```

`plan` accepts the explicitly reviewed transitions from source schemas 13, 14,
15 (the #388 gate-history schema), and 16 (the #384 capacity-exemptions schema
created by the released v0.2.19 binary) to the current schema. Each source runs
exactly the version-gated step chain it is missing — content-hash rebuild
(13→14), transition-scoped gate history (→15), capacity exemptions (→16), and
capacity occupancy (→17). Its deterministic token binds the absolute database
path, a complete logical row/schema witness, and the exact migration forecast.
The receipt reports every raw SQLite family member, but raw page/WAL/SHM/journal
layout is deliberately not token-bound: process close and checkpoint may
rewrite or retire those files without changing database semantics. `apply`
captures and verifies a fresh byte-exact family backup after logical-token
validation and before migration.

`apply` re-plans under database-family write authority and rejects stale tokens
before allocating a run. It writes a verified, private recovery bundle and a
prepared receipt before running the reviewed steps in one `BEGIN IMMEDIATE`
transaction. The applied receipt records the complete before/after witnesses,
actual effects, post-commit attestation, and an undo command. A committed
migration that fails post-commit attestation still receives an undo-capable
receipt and is reported as an error.

`undo` hash-validates the prepared/applied receipt chain and recovery bundle,
then refuses if the complete post-migration logical state changed. Raw-family
equality is the fail-closed fallback only when post-commit logical attestation
could not be captured; ordinary SQLite checkpoint churn is not mistaken for
user data. Undo moves the applied SQLite family into a retained quarantine
before restoring the byte-exact pre-state; it never deletes the displaced
state. An interrupted undo resumes component by component, and a completed
undo is idempotent. Recovery directories are mode `0700` and receipt/backup
files are mode `0600` on Unix. Runs are retained under
`.beads/.br_recovery/schema-migrations/`.

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
`coordination-status`, `additive-reconciliation`, `vcs-status`, `error`, and
`commands`.

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
