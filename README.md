# br - Beads Rust

<div align="center">
  <img src="docs/assets/br_illustration.webp" alt="br - Fast, non-invasive issue tracker for git repositories" width="600">
</div>

<div align="center">

[![License: MIT](https://img.shields.io/badge/License-MIT%2BOpenAI%2FAnthropic%20Rider-blue.svg)](./LICENSE)
[![Rust](https://img.shields.io/badge/rust-nightly-orange.svg)](https://www.rust-lang.org/)
[![SQLite](https://img.shields.io/badge/storage-SQLite-green.svg)](https://www.sqlite.org/)

</div>

A Rust port of Steve Yegge's [beads](https://github.com/steveyegge/beads), frozen at the "classic" SQLite + JSONL architecture I built my Agent Flywheel tooling around.

[Quick Start](#quick-start) | [Commands](#commands) | [Configuration](#configuration) | [VCS Integration](#vcs-integration) | [FAQ](#faq)

<div align="center">
<h3>Quick Install</h3>

```bash
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/beads_rust/main/install.sh?$(date +%s)" | bash
```

<p><em>Works on Linux, macOS, and Windows (WSL). Auto-detects your platform and downloads the right binary.</em></p>

<p><em>Useful install flags: <code>--skip-skills</code> to skip all Claude Code / Codex skills, or <code>--no-migration-skill</code> to skip just the bd-to-br-migration skill (handy for clean agent sandboxes where you're only using <code>br</code>).</em></p>
</div>

---

## Why This Project Exists

I (Jeffrey Emanuel) LOVE [Steve Yegge's Beads project](https://github.com/steveyegge/beads). Discovering it and seeing how well it worked together with my [MCP Agent Mail](https://github.com/Dicklesworthstone/mcp_agent_mail) was a truly transformative moment in my development workflows and professional life. This quickly also led to [beads_viewer (bv)](https://github.com/Dicklesworthstone/beads_viewer), which added another layer of analysis to beads that gives swarms of agents the insight into what beads they should work on next to de-bottleneck the development process and increase velocity. I'm very grateful for finding beads when I did and to Steve for making it.

At this point, my [Agent Flywheel](http://agent-flywheel.com/tldr) System is built around beads operating in a specific way. As Steve continues evolving beads toward [GasTown](https://github.com/steveyegge/gastown) and beyond, our use cases have naturally diverged. The hybrid SQLite + JSONL-git architecture that I built my tooling around (and independently mirrored in MCP Agent Mail) is being replaced with approaches better suited to Steve's vision.

Rather than ask Steve to maintain a legacy mode for my niche use case, I created this Rust port that freezes the "classic beads" architecture I depend on. The command is `br` to distinguish it from the original `bd`.

**This isn't a criticism of beads**; Steve's taking it in exciting directions. It's simply that my tooling needs a stable snapshot of the architecture I built around, and maintaining my own fork is the right solution for that. Steve has given his full endorsement of this project.

---

## TL;DR

### The Problem

You need to track issues for your project, but:
- **GitHub/GitLab Issues** require internet, fragment context from code, and don't work offline
- **TODO comments** get lost, have no status tracking, and can't express dependencies
- **External tools** (Jira, Linear) add overhead, require context switching, and cost money

### The Solution

**br** is a local-first issue tracker that stores issues in SQLite with JSONL export for git-friendly collaboration. It provides dependency-aware issue tracking, machine-readable output, sync/recovery tooling, and agent-friendly workflows without leaving your repository.

```bash
br init                              # Initialize in your repo
br create "Fix login timeout" -p 1   # Create high-priority issue
br ready                             # See what's actionable
br coordination status --json        # Inspect hidden in-progress claims
br close br-abc123                   # Close when done; JSONL auto-flushes by default
br sync --flush-only                 # Optional final export check before git commit
```

### Why br?

| Feature | br | GitHub Issues | Jira | TODO comments |
|---------|-----|---------------|------|---------------|
| Works offline | **Yes** | No | No | Yes |
| Lives in repo | **Yes** | No | No | Yes |
| Tracks dependencies | **Yes** | Limited | Yes | No |
| Zero cost | **Yes** | Free tier | No | Yes |
| No account required | **Yes** | No | No | Yes |
| Machine-readable | **Yes** (`--json`) | API only | API only | No |
| Git-friendly sync | **Yes** (JSONL) | N/A | N/A | N/A |
| Non-invasive | **Yes** | N/A | N/A | Yes |
| AI agent integration | **Yes** | Limited | Limited | No |

---

## Quick Example

```bash
# Initialize br in your project
cd my-project
br init

# Add agent instructions to AGENTS.md (creates file if needed)
br agents --add --force

# Create issues with priority (0=critical, 4=backlog)
br create "Implement user auth" --type feature --priority 1
# Created: br-7f3a2c

br create "Set up database schema" --type task --priority 1
# Created: br-e9b1d4

# Auth depends on database schema
br dep add br-7f3a2c br-e9b1d4

# See what's ready to work on (not blocked)
br ready
# br-e9b1d4  P1  task     Set up database schema

# Claim and complete work
br update br-e9b1d4 --status in_progress
br close br-e9b1d4 --reason "Schema implemented"

# Now auth is unblocked
br ready
# br-7f3a2c  P1  feature  Implement user auth

# Mutations auto-flushed JSONL by default; run an idempotent final export check
br sync --flush-only
git add .beads/ && git commit -m "Update issues"
```

---

## Design Philosophy

### 1. Non-Invasive by Default

For normal issue tracking and sync, br keeps its state in `.beads/` and leaves
git handoff to you. It never commits, pushes, pulls, installs hooks, or runs as a
background service.

Some explicit commands intentionally step outside that default storage boundary:
`br agents` edits requested agent-instruction files, `br doctor --repair` can fix
the project `.gitignore`, `br config edit/set` updates config files,
`br completions -o` writes shell completion files, `br upgrade` updates the
installed binary, and git-reporting commands such as `br changelog`, `br
orphans`, commit-activity `br stats`, and the explicitly requested bounded
`br vcs-status` diagnostic inspect git state/history.

```bash
# Normal issue state lives under .beads/
ls -la .beads/
# beads.db       # SQLite database
# issues.jsonl   # Git-friendly export
# config.yaml    # Optional config
```

### 2. SQLite + JSONL Hybrid

**SQLite** for fast local queries. **JSONL** for git-friendly collaboration.

```bash
# Local: Fast queries via SQLite
br list --priority 0-1 --status open --assignee alice

# Collaboration: JSONL merges cleanly in git
git diff .beads/issues.jsonl
# +{"id":"br-abc123","title":"New feature",...}
```

### 3. Explicit Over Implicit

State changes are explicit. Successful mutating commands update SQLite and
auto-flush JSONL by default, but `br` still never commits, pushes, pulls, or
imports remote changes without a command. Git-inspection behavior is limited to
explicit reporting commands and reads history only.

```bash
# Mutations auto-flush .beads/issues.jsonl by default
br close br-abc123 --reason "Done"

# Re-run export after --no-auto-flush/config changes, recovery, or as a final check
br sync --flush-only

# Import is explicit (not automatic)
br sync --import-only

# Merge divergent DB and JSONL edits using the saved base snapshot
br sync --merge

# Additively pull JSONL rows the database is missing (previewable, lossless)
br sync --reconcile --dry-run
br sync --reconcile
# Recover JSONL-only rows without deleting SQLite-only rows.
# Review the dry-run receipt, then bind apply to that exact plan.
plan="$(br sync --reconcile-additive --robot)"
plan_sha256="$(printf '%s\n' "$plan" | jq -r .plan_sha256)"
br sync --reconcile-additive --apply \
  --expect-plan-sha256 "$plan_sha256" --robot

# Rebuild SQLite from authoritative JSONL after recovery/corruption
br sync --import-only --rebuild

# Git operations are YOUR responsibility
git add .beads/ && git commit -m "..."
```

### 4. Agent-First Design

Every command supports `--json` for AI coding agents:

```bash
br list --json | jq '.issues[] | select(.priority <= 1)'
br ready --json  # Structured output for agents
br show br-abc123 --json
br capabilities --format json
br capabilities --format json --command "create"
br robot-docs guide
```

For routine operator or agent use, prefer `RUST_LOG=error br ...` to suppress internal Rust dependency logs while preserving normal stdout/JSON output:

```bash
RUST_LOG=error br ready --json
RUST_LOG=error br sync --flush-only
```

### 5. Rich Terminal Output

Interactive terminals get enhanced visual output:

```bash
# Rich mode (default in TTY)
br list           # Formatted tables with colors
br show br-abc    # Styled panels with metadata

# Plain mode (piped or --no-color)
br list | cat     # Clean text, no ANSI codes

# JSON mode (--json or --robot)
br list --json    # Structured output for tools ({issues, total, limit, offset, has_more})
```

Output mode is auto-detected:
- **Rich**: Interactive TTY with color support
- **Plain**: Piped output or `NO_COLOR` environment
- **JSON**: Machine-readable (`--json` flag)
- **Quiet**: Minimal output (`--quiet` flag)

### 6. Focused Local Scope

br has grown into a full CLI surface for local issue tracking: routing, recovery,
TOON/JSON schemas, MCP support, conformance checks, and sync safety tools are
all part of the current scope. The focus is still local-first operation, explicit
git/VCS handoff, and no background services installed behind your back.

Agent-facing output contracts have a focused verifier:

```bash
BR_AGENT_CONTRACT_USE_RCH=1 ./scripts/verify-agent-contracts.sh
```

Run it before changing schema command metadata, CLI JSON/TOON output, MCP
resources/tools/prompts, README/docs examples, or `agent_baseline/` artifacts.
With `BR_AGENT_CONTRACT_USE_RCH=1`, the script delegates each Cargo target to
`rch exec --`. The contract tests do not run git, project network calls, live
Agent Mail, MCP clients, or fixture update modes.

---

## Comparison vs Alternatives

### br vs Original beads (Go)

| Aspect | br (Rust) | beads (Go) |
|--------|-----------|------------|
| Git operations | **No automatic commits/pushes/pulls**; reporting commands can inspect git history | Auto-commit, hooks |
| Storage | SQLite + JSONL | Dolt/SQLite |
| Background daemon | **No** | Yes |
| Hook installation | **Manual** | Automatic |
| Binary size | ~5-8 MB | ~30+ MB |
| Scope | Local CLI, sync, recovery, and agent workflows | Feature-rich ecosystem |

**When to use br:** You want a stable, local-first issue tracker with explicit sync, dependency-aware planning, and machine-readable output.

**When to use beads:** You want advanced features like Linear/Jira sync, RPC daemon, automatic hooks.

### br vs GitHub Issues

| Aspect | br | GitHub Issues |
|--------|-----|---------------|
| Works offline | **Yes** | No |
| Lives in repo | **Yes** | Separate |
| Dependencies | **Yes** | Workarounds |
| Custom fields | Via labels | Limited |
| Machine API | `--json` flag | REST API |
| Cost | Free | Free (limits) |

### br vs Linear/Jira

| Aspect | br | Linear/Jira |
|--------|-----|-------------|
| Setup time | 1 command | Account + config |
| Cost | Free | $8-15/user/mo |
| Works offline | **Yes** | Limited |
| Learning curve | CLI | GUI + workflows |
| Git integration | Native | Webhooks |

---

## Installation

### Quick Install (Recommended)

```bash
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/beads_rust/main/install.sh?$(date +%s)" | bash
```

### From Source

```bash
# Requires Rust nightly
git clone https://github.com/Dicklesworthstone/beads_rust.git
cd beads_rust
cargo build --release
./target/release/br --help

# Or install globally
cargo install --path . --locked
```

### Cargo Install

```bash
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git beads_rust --locked
```

The explicit `beads_rust` package selector avoids ambiguity with the
repository's fuzz package. `--locked` makes Cargo use the dependency versions
validated against the repository's pinned nightly toolchain.

> **Note:** `cargo install` places binaries in `~/.cargo/bin/`, while the install script uses `~/.local/bin/`. If you have both in PATH, ensure the desired location has higher priority to avoid running an outdated version. Run `which br` to verify which binary is active.

### Claude Code Plugin (agent instructions)

The official `br` skill ships as a Claude Code plugin, so agents get the
workflow rules without you copying `SKILL.md` around:

```bash
/plugin marketplace add Dicklesworthstone/beads_rust
/plugin install beads@beads-rust
```

The plugin installs **agent instructions only** — it does not install the `br`
binary, so pair it with one of the install methods above. The optional
`bd-to-br-migration` skill stays opt-in and is not part of the plugin; install
it with `install.sh --with-migration-skill` if you are still migrating from
`bd`. Codex users keep using `install.sh`, which writes the same skill into
`${CODEX_HOME:-~/.codex}/skills`.

### Disable Self-Update

```bash
# Build without self-update feature
cargo build --release --no-default-features

# Or install without it
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git beads_rust --locked --no-default-features
```

### Enable MCP Server Support

`br serve` is optional and is not built by the default feature set. Build with
the `mcp` feature when you want an AI agent to talk to `br` over the Model
Context Protocol instead of shelling out to CLI commands.

```bash
cargo build --release --features mcp

# Or install globally with MCP support
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git beads_rust --locked --features mcp
```

Run it from an initialized beads workspace:

```bash
RUST_LOG=error br serve --actor codex
```

The server uses MCP over stdio. It is launched by an MCP client, does not listen
on a network port, and uses the same SQLite database, JSONL export path, write
locks, audit events, and sync safety model as the normal CLI. It does not run
git. Use shell/JSON
commands for simple scripts; use MCP when an agent benefits from discoverable
tools, resources, prompts, and structured recovery hints. MCP clients can read
`beads://coordination/status` for the same `br.coordination.v1` stale-claim
evidence shape as `br coordination status --json`; use the CLI snapshot flags
when Agent Mail reservation or liveness evidence is required.

The MCP tool surface is `list_issues`, `show_issue`, `create_issue`,
`update_issue`, `close_issue`, `manage_dependencies`, and `project_overview`.
The resource surface is `beads://project/info`, `beads://issues/{id}`,
`beads://schema`, `beads://labels`, `beads://issues/ready`,
`beads://issues/blocked`, `beads://issues/in_progress`,
`beads://coordination/status`, `beads://issues/deferred`,
`beads://issues/bottlenecks`, `beads://graph/health`, and
`beads://events/recent`.

### Verify Installation

```bash
br --version
# br 0.3.2
```

---

## Quick Start

### 1. Initialize in Your Project

```bash
cd my-project
br init
# Initialized beads workspace in .beads/
```

### 2. Create Your First Issue

```bash
br create "Fix login timeout bug" \
  --type bug \
  --priority 1 \
  --description "Users report login times out after 30 seconds"
# Created: br-a1b2c3
```

### 3. Add Labels

```bash
br label add br-a1b2c3 backend auth
```

### 4. Check Ready Work

```bash
br ready
# Shows issues that are open, not blocked, not deferred
```

### 5. Claim and Work

```bash
br update br-a1b2c3 --status in_progress --assignee "$(git config user.email)"
```

### 6. Close When Done

```bash
br close br-a1b2c3 --reason "Increased timeout to 60s, added retry logic"
```

### 7. Sync to Git

```bash
br sync --flush-only        # Idempotent final JSONL export check
git add .beads/             # Stage changes
git commit -m "Fix: login timeout (br-a1b2c3)"
```

---

## Commands

### Issue Lifecycle

| Command | Description | Example |
|---------|-------------|---------|
| `init` | Initialize workspace | `br init` |
| `create` | Create issue | `br create "Title" -p 1 --type bug` |
| `q` | Quick capture (ID only) | `br q "Fix typo"` |
| `show` | Show issue details | `br show br-abc123` |
| `update` | Update issue | `br update br-abc123 --priority 0` |
| `close` | Close issue | `br close br-abc123 --reason "Done"` |
| `reopen` | Reopen closed issue | `br reopen br-abc123` |
| `delete` | Delete issue (tombstone) | `br delete br-abc123` |
| `defer` | Schedule issue for later | `br defer br-abc123 --until tomorrow` |
| `undefer` | Make deferred issue ready again | `br undefer br-abc123` |

### Querying

| Command | Description | Example |
|---------|-------------|---------|
| `list` | List issues | `br list --status open --priority 0-1` |
| `ready` | Actionable work | `br ready` |
| `blocked` | Blocked issues | `br blocked` |
| `search` | Full-text search | `br search "authentication"` |
| `stale` | Stale issues | `br stale --days 30` |
| `coordination status` | Hidden in-progress claim diagnosis | `br coordination status --json` |
| `count` | Count with grouping | `br count --by status` |
| `query` | Manage saved queries | `br query save mine --status open --assignee alice` |

### Dependencies

| Command | Description | Example |
|---------|-------------|---------|
| `dep add` | Add dependency | `br dep add br-child br-parent` |
| `dep import` | Bulk import dependency JSONL | `br dep import edges.jsonl --robot` |
| `dep remove` | Remove dependency | `br dep remove br-child br-parent` |
| `dep list` | List dependencies | `br dep list br-abc123` |
| `dep tree` | Dependency tree | `br dep tree br-abc123` |
| `dep cycles` | Find cycles | `br dep cycles` |

### Labels

| Command | Description | Example |
|---------|-------------|---------|
| `label add` | Add labels | `br label add br-abc123 backend urgent` |
| `label remove` | Remove label | `br label remove br-abc123 urgent` |
| `label list` | List issue labels | `br label list br-abc123` |
| `label list-all` | All labels in project | `br label list-all` |

### Comments

| Command | Description | Example |
|---------|-------------|---------|
| `comments add` | Add comment | `br comments add br-abc123 "Found root cause"` |
| `comments list` | List comments | `br comments list br-abc123` |

### Planning & Reporting

| Command | Description | Example |
|---------|-------------|---------|
| `epic` | Manage epic rollups | `br epic status --eligible-only` |
| `graph` | Show what an issue unblocks (its dependents) | `br graph br-abc123` |
| `graph --dependencies` | Show what is blocking an issue | `br graph br-abc123 --dependencies` |
| `lint` | Check issues for missing template sections | `br lint --status all` |
| `orphans` | List open issues referenced in commits | `br orphans` |
| `changelog` | Generate changelog from closed issues | `br changelog --since-tag v0.1.44` |
| `history` | Manage local history backups | `br history list` |
| `status` | Alias for project statistics | `br status` |

### Agents & Tooling

| Command | Description | Example |
|---------|-------------|---------|
| `agents` | Manage AGENTS.md workflow instructions | `br agents --add --force` |
| `audit` | Record and label agent interactions | `br audit record --kind note` |
| `capabilities` | Describe machine-readable contracts and safety guarantees | `br capabilities --format json` |
| `completions` | Generate shell completions | `br completions zsh` |
| `info` | Show workspace diagnostics | `br info` |
| `robot-docs` | Print concise docs for automation agents | `br robot-docs guide` |
| `schema` | Emit JSON Schemas for outputs | `br schema all --format json` |
| `vcs-status` | Explicit bounded JSONL Git visibility | `br vcs-status --json` |
| `where` | Show active `.beads` directory | `br where` |

### Sync & System

| Command | Description | Example |
|---------|-------------|---------|
| `sync` | Explicit DB ↔ JSONL modes | `br sync --flush-only` |
| `sync --witness` | Read-only deterministic JSONL witness | `br sync --witness --robot` |
| `sync --reconcile-additive` | Lossless exact-ID recovery plan/apply | `br sync --reconcile-additive --robot` |
| `doctor` | Run diagnostics | `br doctor` |
| `doctor migrate-schema` | Plan/apply/undo an explicit receipt-bound schema upgrade | `br doctor migrate-schema plan --json` |
| `stats` | Project statistics | `br stats` |
| `config` | Manage config | `br config list` |
| `upgrade` | Self-update | `br upgrade` |
| `version` | Show version | `br version` |

### Global Flags

| Flag | Description |
|------|-------------|
| `--json` | JSON output (machine-readable) |
| `--quiet` / `-q` | Suppress output |
| `--verbose` / `-v` | Increase verbosity (-vv for debug) |
| `--no-color` | Disable colored output |
| `--db <path>` | Override database path |

---

## Configuration

br uses layered configuration:

1. **CLI flags** (highest priority)
2. **Environment variables**
3. **Project config**: `.beads/config.yaml`
4. **User config**: `~/.config/beads/config.yaml`
5. **Defaults** (lowest priority)

### Example Config

```yaml
# .beads/config.yaml

# Default issue ID prefix for newly created issues
id:
  prefix: "proj"

# Default values for new issues
defaults:
  priority: 2
  type: "task"
  assignee: "team@example.com"

# Output formatting
output:
  color: true
  date_format: "%Y-%m-%d"

# Sync behavior
sync:
  auto_import: false
  auto_flush: false
```

### Config Commands

```bash
# Show all config
br config list

# Get specific value
br config get id.prefix

# Set value
br config set defaults.priority=1

# Open in editor
br config edit
```

### Workflow Policy (`.beads/policy.yaml`)

Workflow behavior is configured separately in `.beads/policy.yaml`. One use is
defining a **configurable ready status group**: which statuses `br ready` treats
as actionable work. By default only `open` is ready, but projects with a review
workflow can widen it so review-returned work (e.g. `rework`) resurfaces through
the same `br ready --json` entrypoint:

```yaml
# .beads/policy.yaml
workflow:
  status_groups:
    ready:
      - open
      - rework
```

- Default (when unset): `[open]` — no change for existing repos.
- Returned issues keep their real status (`{"status":"rework"}` in `--json`).
- The `defer_until` time-gate still applies to non-`deferred` members;
  `--include-deferred` additionally surfaces `deferred` work and drops the gate.
- Under `workflow.strict: true`, the ready group must be a subset of
  `workflow.statuses` or `br ready` rejects it with a clear error.
- `br ready` (text/json/toon/robot) and `br scheduler` all honor the group.

See `docs/CLI_REFERENCE.md` (the `ready` command) for full details.

The same policy file can enforce **atomic repository-level workflow capacity**.
Hard limits are checked inside the same `BEGIN IMMEDIATE` transaction that
creates an issue or changes its status, so two concurrent agents cannot both
claim the final slot:

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
```

Individual status and named multi-status group limits allow the configured
count and reject only transitions that would exceed it. Admission thresholds
are exclusive (`count < threshold`) and can inspect a different queue before a
matching transition. Rejected multi-field updates roll back completely;
transitions that drain an already-overfull queue remain allowed. Capacity is
disabled when `workflow.capacity` is absent.

Soft thresholds admit the transition but emit actionable evidence. Human output
prints a warning; JSON and TOON preserve their legacy shape when no warning is
present and otherwise return the successful result together with a structured
`warnings` array. Multi-target `update`/`--claim`, `close`, `reopen`, `defer`,
and `undefer` operations preflight the final prospective state and commit every
status mutation for one repository in a single transaction. A capacity-neutral
swap therefore succeeds regardless of request order, while any hard-limit or
validation failure rolls back the complete repository-local batch. Each route
in a multi-repository command is an independent SQLite transaction, so an
earlier repository may already be committed if a later route fails; br does not
claim distributed atomicity across repositories.

Workflows can also require satisfied acceptance criteria and a fresh comment
for an exact transition or every transition entering a target status. This is
enabled by `required_fields` itself and does not require strict status mode:

```yaml
workflow:
  required_fields:
    in_review:
      - transition_comment
    "in_progress -> in_review":
      - acceptance_criteria
      - transition_comment
```

Pass the request-scoped comment with `br update --transition-comment`,
`br close --transition-comment`, `br defer --transition-comment`, or
`br undefer --transition-comment`; `br reopen --reason` is its transition
comment, and `br epic close-eligible --transition-comment` applies one comment
to every epic in its atomic batch. A historical comment never satisfies the
rule. The prospective acceptance-criteria value must be non-empty and contain
no unchecked markdown boxes. Any failure rolls back the status, other field
updates, comments, and every sibling mutation in the repository-local batch.

Named workflow gate verdicts are likewise bound to the issue's current status
revision and an explicit target. Use `br gate report ... --to <STATUS>` when a
gate can authorize multiple targets; the flag is optional only when the policy
has one matching target. Leaving and later re-entering review invalidates prior
passes without deleting their append-only audit history. Pre-v15 unscoped gate
rows remain visible through `br gate list` but can never authorize a transition.

Capacity counts every matching issue by default. `workflow.capacity.counting.
hierarchy` measures occupancy across `parent-child` edges instead, so an
aggregate parent and its executable child do not each consume a slot:

```yaml
workflow:
  capacity:
    counting:
      hierarchy: leaf_work   # all | leaf_work | roots | weighted
```

Under `leaf_work` an active leaf counts one and a parent with active counted
descendants counts zero, so an epic → parent → {child, child} tree consumes
two slots rather than four; the parent starts counting once its last active
descendant leaves. `roots` counts each work stream by its highest active
ancestor, and `weighted` sums explicit `counting.weights` (per issue, then
per type, then a default), where a weight of `0` is the audited way to say a
parent carries no independent execution. Only `parent-child` edges
participate — `blocks` and `related` never affect counting — and the walk
happens inside the same transaction as admission. Under `leaf_work`/`roots`,
capacity evidence reports `counting_mode` plus `aggregate_parents_excluded`.

`br show --json` also exposes a derived `rollup` for any issue with local
children (`{"status": "in_progress", "descendants": {...}}`), letting an epic
stay `open` while reporting that its subtree has started.

Audited issue-specific **capacity exemptions** let one named issue occupy one
named capacity without consuming a slot — the escape hatch for a long-lived
external blocker that legitimately stays in a limited status:

```yaml
workflow:
  capacity:
    exemptions:
      providers: [operator]     # who may grant; empty disables granting
      require_expiry: true      # optional: every grant must carry an expiry
```

```bash
br capacity exempt br-abc --status blocked \
  --provider operator \
  --reason "Awaiting an external regulatory decision" \
  --expires 2026-08-15
```

Grants, renewals, revocations, and observed expirations are all recorded in an
append-only audit table. Exempt issues stay visible in queue metrics, capacity
evidence reports counted and exempt totals separately, leaving the applicable
status ends the exemption, and expired exemptions count again. See
`docs/CLI_REFERENCE.md` (the `capacity` command) for full semantics.

Optional **multi-agent admission scopes** partition capacity beyond the
repository total — per acting actor, per issue assignee, per self-reported
harness (`--harness`/`BR_HARNESS`) or session (`BR_SESSION`), or per
subtree root over parent-child edges:

```yaml
workflow:
  capacity:
    scopes:
      actor:
        statuses:
          in_progress:
            hard: 2
      harness:
        statuses:
          in_progress:
            hard: 6
```

Every applicable scope composes with the repository limits inside the same
admission transaction; a partition with no key (e.g. no harness reported)
is simply not subject to that scope. This is cooperative admission control,
not process supervision — attribution stays self-reported. Scoped evidence
carries the partition key as `scope_key` and a
`workflow.capacity.scopes.<scope>...` policy path. Once any capacity is
configured, `br stats` and `br coordination status` report per-capacity
occupancy (counted/exempt/limits/remaining/state, including occupied scope
partitions) in human, JSON, and TOON output. See `docs/CLI_REFERENCE.md`
for full semantics and `docs/GH384_ACCEPTANCE_MATRIX.md` for the complete
GitHub #384 acceptance matrix.

### Environment Variables

| Variable | Description |
|----------|-------------|
| `BD_DB` / `BD_DATABASE` | Override database path |
| `BEADS_JSONL` | Override JSONL path (requires `--allow-external-jsonl`) |
| `RUST_LOG` | Logging level (debug, info, warn, error) |

Recommended default for normal CLI use:

```bash
export RUST_LOG=error
```

This keeps successful commands readable by suppressing low-level dependency logging. Remove or override it when debugging `br` internals.

---

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                         CLI (br)                              │
│  Commands: create, list, ready, close, sync, etc.            │
└──────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────┐
│                      Storage Layer                            │
│  ┌─────────────────┐              ┌─────────────────────┐    │
│  │  SqliteStorage  │◄────────────►│  JSONL Export/Import │    │
│  │                 │   sync       │                     │    │
│  │  - WAL mode     │              │  - Atomic publish   │    │
│  │  - Dirty track  │              │  - Content hashing  │    │
│  │  - Blocked cache│              │  - Merge support    │    │
│  └────────┬────────┘              └──────────┬──────────┘    │
└───────────│──────────────────────────────────│───────────────┘
            │                                  │
            ▼                                  ▼
     .beads/beads.db                    .beads/issues.jsonl
     (Primary storage)                  (Git-friendly export)
```

### Data Flow

```
User Action                    br Command              Storage
───────────────────────────────────────────────────────────────
Create issue        ──►      br create        ──►    SQLite INSERT
                                              ──►    Mark dirty

Update issue        ──►      br update        ──►    SQLite UPDATE
                                              ──►    Mark dirty

Query issues        ──►      br list          ──►    SQLite SELECT

Export to git       ──►      br sync --flush-only
                                              ──►    Write JSONL + clear dirty flags

Pull from git       ──►      git pull         ──►    JSONL updated
                    ──►      br sync --import-only
                                              ──►    Merge to SQLite
```

Bare `br sync` is intentionally refused; choose `--flush-only`, `--import-only`,
`--merge`, `--reconcile`, `--reconcile-additive`, `--status`, or `--witness`
so the data direction and authority are explicit. `br sync --status` never
probes Git; run `br vcs-status --json` only when Git visibility is explicitly
wanted.

### Safety Model

`br sync` is designed to be **provably safe**:

| Guarantee | Implementation |
|-----------|----------------|
| Sync never executes git | No runtime `Command::new("git")` calls in `src/sync/` or `src/cli/commands/sync.rs` |
| Sync uses an allowlist for writes | Default writes stay in `.beads/`; external JSONL paths require `--allow-external-jsonl` or an explicit external DB/JSONL family and `.git/` paths are still rejected |
| Checked publication and transactions | JSONL/base/manifest publication uses checked temporary replacement; database mutations use transactions and operation-specific rollback |
| No data loss | Guards prevent overwriting non-empty JSONL with empty DB |

---

## Troubleshooting

### Error: "Database locked"

**Cause:** Another process has the database open.

```bash
# Check for other br processes
pgrep -f "br "

# Force close and retry
br sync --status  # Safe read-only check
```

### Error: "Issue not found"

**Cause:** Issue ID doesn't exist or was deleted.

```bash
# Check if issue exists
br list --json | jq '.issues[] | select(.id == "br-abc123")'

# Check for similar IDs
br list | grep -i "abc"
```

### Error: "Prefix mismatch"

**Cause:** This now only applies when you explicitly ask br to enforce or rewrite
prefixes during import. Mixed prefixes in a project are supported by default.

```bash
# Check your default creation prefix
br config get id.prefix

# Import while rewriting IDs into your configured default prefix
br sync --import-only --rename-prefix
```

If you want to preserve imported IDs exactly as-is, omit `--rename-prefix`.

### Error: "Stale database"

**Cause:** JSONL has issues that don't exist in database.

```bash
# Check sync status
br sync --status

# Lossless recovery: preview, then additively pull the missing/newer rows
# (never deletes, never writes JSONL, preserves all audit events)
br sync --reconcile --dry-run
br sync --reconcile

# Force import (may lose local changes)
br sync --import-only --force

# If JSONL is authoritative, rebuild SQLite to match it exactly
br sync --import-only --rebuild
```

The reconcile path also repairs the "false equal" state where `br sync
--status` reports synchronized (the stored content hash matches the file)
while the JSONL still holds rows the database never imported.

`--rebuild` is an explicit import-mode operation. It is valid only with
`--import-only`; after import it removes database entries that are absent from
JSONL, while preserving deletion tombstones used by sync.

### Sync Issues After Git Merge

```bash
# 1. Check for JSONL merge conflicts
git status .beads/

# 2. If conflicts, resolve manually then:
br sync --import-only

# 3. If both SQLite and JSONL changed cleanly, run a three-way merge:
br sync --merge

# 4. If database seems stale:
br doctor
```

`br sync --merge` uses `.beads/beads.base.jsonl` as the common ancestor. If the
same issue changed on both sides, br stops and asks for an explicit policy:
`--force-db` keeps the local SQLite version, `--force-jsonl` keeps the JSONL
version, and `--force` keeps the newer timestamp.

### Command Output is Garbled

```bash
# Disable colors
br list --no-color

# Or use JSON output
br list --json | jq '.issues'
```

---

## Limitations

br intentionally does **not** support:

| Feature | Reason |
|---------|--------|
| **Automatic git commits** | Non-invasive philosophy |
| **Git hook installation** | User-controlled, add manually if desired |
| **Background daemon** | Simple CLI, no processes to manage |
| **Dolt backend** | SQLite + JSONL only |
| **Linear/Jira sync** | Focused scope |
| **Web UI** | CLI-first (see beads_viewer for TUI) |
| **Automatic multi-repo sync** | Route-aware commands can target configured workspaces, but git/VCS sync remains explicit per repo |
| **Real-time collaboration** | Git-based async collaboration |

---

## FAQ

### Q: How do I integrate with beads_viewer (bv)?

br works seamlessly with [beads_viewer](https://github.com/Dicklesworthstone/beads_viewer):

```bash
# Use bv for interactive TUI
bv

# Use br for CLI/scripting
br ready --json | jq
```

### Q: Can I use br with AI coding agents?

Yes! br is designed for AI agent integration:

```bash
# Agents can use --json for structured output
br list --json
br ready --json
br show br-abc123 --json
br coordination status --json
br capabilities --format json
br capabilities --format json --command "comments add"
br robot-docs guide

# Create issues programmatically
br create "Title" --json  # Returns created issue as JSON
```

When `br ready --json` is empty but `bv --robot-next` or a human operator
suspects work is hidden behind old claims, use `br coordination status --json`
alongside Agent Mail reservations. The command is read-only: it does not call
Agent Mail, does not run git, and never auto-reclaims a bead.

See [AGENTS.md](AGENTS.md) for the complete agent integration guide.

### Q: How do I migrate from the original beads?

br uses the same JSONL format as classic beads:

```bash
# Copy your existing issues.jsonl
cp /path/to/beads/.beads/issues.jsonl .beads/

# Import into br
br sync --import-only
```

### Q: Why Rust instead of Go?

- **Smaller binary:** ~5-8 MB vs ~30+ MB
- **Memory safety:** No runtime garbage collection
- **Operational fit:** The CLI, release pipeline, and agent tooling are already Rust-based
- **Personal preference:** The author's flywheel tooling is Rust-based

### Q: How do dependencies work?

```bash
# Issue A depends on Issue B (A is blocked until B is closed)
br dep add br-A br-B

# Now br-A won't appear in `br ready` until br-B is closed
br ready  # Only shows br-B

# Close the blocker
br close br-B

# Now br-A is ready
br ready  # Shows br-A
```

### Q: How do I handle merge conflicts in JSONL?

JSONL is line-based, so conflicts are usually easy to resolve:

```bash
# After git merge with conflicts
git status .beads/issues.jsonl

# Edit to resolve (each line is one issue)
vim .beads/issues.jsonl

# Mark resolved and import
git add .beads/issues.jsonl
br sync --import-only
```

If git merged `.beads/issues.jsonl` without textual conflict markers but both
SQLite and JSONL have independent br changes, use the sync merge path instead:

```bash
br sync --merge

# If br reports semantic conflicts, choose one resolution policy:
br sync --merge --force-db     # keep local SQLite changes
br sync --merge --force-jsonl  # keep JSONL changes
br sync --merge --force        # keep the newer timestamp
```

### Q: Can I customize the issue ID prefix?

Yes:

```bash
br config set id.prefix=myproj
# New issues: myproj-abc123
```

You can also mix multiple prefixes in the same project. The configured prefix is
the default for newly created issues, not a restriction on existing IDs.

### Q: Where is data stored?

```
.beads/
├── beads.db        # SQLite database (primary storage)
├── issues.jsonl    # JSONL export (for git)
├── config.yaml     # Project configuration
├── routes.jsonl    # Optional cross-project prefix routes
└── metadata.json   # Workspace metadata
```

### Q: Can one workspace refer to issues in another workspace?

Yes, with explicit cross-project routing. Add one JSON object per line to
`.beads/routes.jsonl`:

```jsonl
{"prefix":"api-","path":"../api"}
{"prefix":"ops-","path":"/srv/projects/ops/.beads"}
```

When an issue ID starts with a routed prefix, route-aware commands resolve that
ID against the target workspace's `.beads` directory. The path can point at a
project root or directly at a `.beads`/`_beads` directory; relative paths are
resolved from the workspace root, and town-level routing can also be discovered
from a parent with `mayor/town.json`.

Common route-aware operations include `show`, `update`, `close`, `reopen`,
`delete`, `defer`, `comments`, `label`, `dep`, `graph`, `audit`, and `lint`.
Routed mutations acquire the target workspace write lock and update the target
workspace's storage, not the caller's local database.

This is not automatic multi-repo synchronization. Routed issue operations still
do not push or pull remote repositories, copy issues between repositories, or
provide real-time collaboration. Routes are a local dispatch table for explicit
cross-workspace operations. Commit and synchronize each affected repository's
`.beads/` files through your normal VCS workflow.

External dependency status checks use explicit dependency IDs such as
`external:api:api-123` together with configured `external_projects.<name>` paths.
They let `ready`, `blocked`, `show`, `dep`, and `stats` account for blockers in
other workspaces without importing those issues into the local database.

---

## AI Agent Integration

br is designed for AI coding agents. See [AGENTS.md](AGENTS.md) for:

- JSON output schemas
- Workflow patterns
- Integration with MCP Agent Mail
- Degraded coordination when Agent Mail is unavailable
- Robot mode flags
- Best practices

For CI and release workflow edits, use
[CI_SUPPLY_CHAIN.md](docs/CI_SUPPLY_CHAIN.md) as the canonical maintenance
policy for immutable GitHub Action pins, workflow fragment harnesses, update
audits, and required proof commands.

You can also emit machine-readable JSON Schema documents directly:

```bash
br schema all --format json | jq '.schemas.Issue'
br schema issue-details --format toon
```

---

## VCS Integration

Using non-git version control? See [VCS_INTEGRATION.md](docs/VCS_INTEGRATION.md) for
equivalent commands and workflows.

Quick example:

```bash
# Agent workflow
br ready --json | jq '.[0]'           # Get top priority
br update br-abc --status in_progress # Claim work
# ... do work ...
br close br-abc --reason "Completed"  # Done; JSONL auto-flushes by default
br sync --flush-only                  # Final export check before staging .beads/
```

---

## Community Projects

- [**Beads Task-Issue Tracker**](https://github.com/w3dev33/beads-task-issue-tracker) — A desktop GUI for `br`, built with Tauri + Nuxt. Reads the same SQLite + JSONL files that `br` produces, providing a graphical interface for browsing and managing issues.

---

## About Contributions

Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

---

## License

MIT License (with OpenAI/Anthropic Rider) — see [LICENSE](LICENSE) for details.

---

<div align="center">
  <sub>Built with Rust. Powered by SQLite. Synced with Git.</sub>
</div>
