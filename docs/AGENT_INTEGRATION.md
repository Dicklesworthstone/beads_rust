# AI Agent Integration Guide

This guide covers how AI coding agents can effectively use `br` (beads_rust) for issue tracking and workflow management.

---

## Table of Contents

- [Overview](#overview)
- [Quick Start for Agents](#quick-start-for-agents)
- [JSON Mode](#json-mode)
- [Agent Contract Inventory](#agent-contract-inventory)
- [Workflow Patterns](#workflow-patterns)
- [Parsing JSON Output](#parsing-json-output)
- [Error Handling](#error-handling)
- [MCP Server](#mcp-server)
- [Robot Mode Flags](#robot-mode-flags)
- [Degraded Coordination Without Agent Mail](#degraded-coordination-without-agent-mail)
- [Swarm-Scale Tuning](#swarm-scale-tuning)
- [Agent-Specific Configuration](#agent-specific-configuration)
- [Best Practices](#best-practices)

---

## Overview

`br` is designed with AI coding agents in mind:

- **JSON output** for all commands (`--json` flag)
- **Machine-readable errors** with structured error codes
- **Non-interactive** - no prompts, no TUI in normal operation
- **Deterministic** - same input produces same output
- **Fast** - millisecond response times for most operations

### Key Principles

1. **Always use `--json`** for programmatic access
2. **Check exit codes** for success/failure
3. **Parse structured errors** for recovery hints
4. **Use `br ready`** to find actionable work
5. **Run a final export check** with `br sync --flush-only` before committing `.beads/`

---

## Quick Start for Agents

```bash
# Initialize (if needed)
br init

# Find work
br ready --json --limit 5

# Claim and work
br update br-123 --claim --json
# ... do the work ...
br close br-123 --reason "Implemented feature X" --json

# Create discovered work
br create "Found bug during implementation" -t bug -p 1 --deps discovered-from:br-123 --json

# Session end: mutations auto-flush by default, but this is an idempotent final check
br sync --flush-only
```

---

## JSON Mode

### Enabling JSON Output

```bash
# Flag on any command
br list --json
br show br-123 --json
br create "Title" --json

# Equivalent (when the command supports --format)
br list --format json
br ready --format json

# Robot mode alias (same as --json)
br ready --robot
br close br-123 --robot
```

### TOON Output (Token-Efficient)

Many read-style commands support TOON output via `--format toon`:

```bash
br ready --format toon --limit 10
br show br-123 --format toon
```

Decode TOON to JSON when you need to pipe into JSON tools:

```bash
br ready --format toon --limit 10 | tru --decode --expand-paths safe | jq '.[0]'
```

### Environment Defaults

If you omit `--format` / `--json`, br can default the output format via env vars:

- `BR_OUTPUT_FORMAT` (highest precedence)
- `TOON_DEFAULT_FORMAT` (fallback)
- `RUST_LOG=error` (recommended for routine agent runs so stderr stays clean unless you're debugging internals)

Example:

```bash
export TOON_DEFAULT_FORMAT=toon
export RUST_LOG=error
br list --limit 5          # defaults to TOON
br list --json --limit 5   # JSON always wins
```

### JSON Output Characteristics

- **Always valid JSON** - parseable even on errors
- **Arrays for lists** - `br list`, `br ready`, `br search`
- **Objects for single items** - `br show`, `br create`
- **Structured errors** - error object with code and hints

### Example Output

```bash
$ br ready --json --limit 2
```
```json
[
  {
    "id": "br-abc123",
    "title": "Implement user auth",
    "status": "open",
    "priority": 1,
    "issue_type": "feature",
    "assignee": "",
    "dependency_count": 0,
    "dependent_count": 2
  },
  {
    "id": "br-def456",
    "title": "Fix login bug",
    "status": "open",
    "priority": 0,
    "issue_type": "bug",
    "assignee": "alice",
    "dependency_count": 1,
    "dependent_count": 0
  }
]
```

---

## Agent Contract Inventory

This inventory names the machine-readable and workflow surfaces that agents
parse, discover, or treat as coordination contracts. Future contract tests for
these surfaces must stay local-first and deterministic: no live Agent Mail
calls, network access, git mutations, background daemons, or long-running MCP
clients. Use offline fixtures, in-process helpers, temp workspaces, snapshots,
and schema/TOON decoding instead.

| Surface | Producer | Agent consumer | Stable contract | Current verifier or evidence | Gap / follow-up bead |
|---------|----------|----------------|-----------------|------------------------------|----------------------|
| `br schema all` and `br schema commands` targets | `src/cli/commands/schema.rs` (`build_schemas`, `build_commands`) | Agents discovering output shapes before parsing CLI results | JSON Schema documents plus command envelopes with `jq_filter`, `items_at`, and schema names | `tests/e2e_schema.rs`, `tests/conformance_schema.rs`, `tests/snapshots/schema_output.rs`, `agent_baseline/schemas/schema_all.json`, `agent_baseline/cli_schema.json` | `beads_rust-vqs1` adds emitted-target self-checks; `beads_rust-p1g4` validates command-shape paths against live fixtures |
| JSON output for `list`, `show`, `ready`, `blocked`, `stale`, `search`, counts, labels, deps, comments, stats, and status | CLI command implementations plus the output/context layers | Shell-based agents, MCP adapters, docs examples, and baseline fixtures | Valid JSON on stdout, stable object/list envelopes, structured errors on failure | `tests/snapshots/json_output.rs`, `tests/snapshots/robot_output.rs`, `tests/e2e_create_output.rs`, `tests/common/json_baseline.rs`, `tests/fixtures/json_baseline/`, `agent_baseline/examples/*.json` | `beads_rust-p1g4` checks command metadata against actual JSON; `beads_rust-8bq8` collects the verifier commands |
| TOON output for agent-read commands | CLI format handling and `toon_rust` integration | Token-sensitive agents using `--format toon` or `TOON_DEFAULT_FORMAT=toon` | TOON decodes to the documented JSON structure with safe folded-key expansion | `tests/snapshots/toon_output.rs`, `agent_baseline/examples/*.toon`, this guide's decode example | `beads_rust-q5jt` cross-checks JSON/TOON semantic parity, including nested coordination fields |
| Coordination status evidence (`br.coordination.v1`) | `br coordination status`, coordination model code, and optional offline reservation/agent snapshots | Agents deciding whether an `in_progress` claim is fresh, stale, reclaimable, or blocked by missing Mail evidence | Read-only evidence envelope; no automatic reclaim, no Agent Mail calls, and no git operations | `docs/COORDINATION_EVIDENCE.md`, schema entries for `CoordinationStatusOutput` and `CoordinationClaimRow`, agent workflow examples in this guide | `beads_rust-p1g4` covers command-shape extraction; `beads_rust-q5jt` covers JSON/TOON parity for coordination output |
| `agent_baseline/` examples and schemas | Curated baseline artifacts generated from representative `br` commands | Agents bootstrapping behavior from examples before running the binary | Checked-in JSON, TOON, schema, and journey artifacts that mirror current CLI contracts | `agent_baseline/README_first_80_lines.md`, `agent_baseline/AGENT_JOURNEY_NOTES.md`, `agent_baseline/examples/`, `agent_baseline/schemas/`, `agent_baseline/robot_mode_examples.jsonl` | `beads_rust-8bq8` defines the one-command verifier; later fixture work should prevent stale baseline artifacts |
| Snapshot and golden tests for agent output | `tests/snapshots/*`, storage golden snapshots, and focused e2e fixtures | Release reviewers and agents checking whether a contract changed intentionally | Deterministic expected output for representative commands and storage states | `tests/snapshots/cli_output.rs`, `tests/snapshots/json_output.rs`, `tests/snapshots/robot_output.rs`, `tests/snapshots/schema_output.rs`, `tests/snapshots/toon_output.rs`, `tests/storage_golden_snapshot.rs` | `beads_rust-vqs1`, `beads_rust-p1g4`, and `beads_rust-q5jt` make the snapshots harder to update incompletely |
| MCP resources, tools, and prompts | `src/mcp/resources.rs`, `src/mcp/tools.rs`, `src/mcp/prompts.rs`, and `src/mcp/mod.rs` behind the `mcp` feature | MCP-capable agents using `br serve` instead of shelling out | Stdio-only local server surface with stable resource URIs, tool names, prompt names, and JSON payloads | README and CLI reference MCP sections, this guide's MCP section, in-process MCP code paths | `beads_rust-hu4b` ties MCP metadata and representative payloads to CLI contract fixtures without live clients or network services |
| README and docs command examples | `README.md`, `docs/CLI_REFERENCE.md`, this guide, `docs/SWARM_SCALE_TUNING.md`, `docs/COORDINATION_EVIDENCE.md` | Human operators and agents copying workflow commands | Examples use robot-safe flags, avoid hidden git automation, and state Mail/network boundaries accurately | Review plus `git diff --check`; relevant e2e/snapshot tests cover many listed commands indirectly | `beads_rust-8bq8` documents when to run the full drift verifier before changing docs or examples |
| `bv` robot handoff expectations | External `bv` binary plus repo guidance in `AGENTS.md`, this guide, and `docs/SWARM_SCALE_TUNING.md` | Agents selecting work by graph priority before claiming with `br` and Agent Mail | Agents use only `--robot-*` or `--recipe ... --robot-*` flags; bare `bv` is interactive and outside `br`'s control | Documented workflow examples only; `bv` is outside the `br` binary and test harness | Keep this as an offline documentation contract; `br` tests should not shell out to live `bv` |

The child beads intentionally split coverage by failure mode. Use
`beads_rust-vqs1` when schema target discovery drifts, `beads_rust-p1g4` when
command metadata and live JSON disagree, `beads_rust-q5jt` when TOON and JSON
semantics diverge, `beads_rust-hu4b` when MCP metadata or payloads fall behind,
and `beads_rust-8bq8` when agents need a single RCH-friendly verifier command
sequence.

### Agent Contract Drift Verifier

Run the full agent-facing contract verifier before changing any surface that
agents parse or copy:

- `src/cli/commands/schema.rs` command metadata, schema names, `jq_filter`, or
  `items_at` paths
- CLI JSON or TOON output for agent-read commands
- MCP resources, tools, prompts, descriptions, or representative payloads
- README/docs examples that agents may copy
- `agent_baseline/` schemas, JSON examples, TOON examples, help text, or journey
  notes

Agent sessions should run the verifier through RCH:

```bash
BR_AGENT_CONTRACT_USE_RCH=1 ./scripts/verify-agent-contracts.sh
```

The script itself only runs deterministic Cargo tests. With
`BR_AGENT_CONTRACT_USE_RCH=1`, each Cargo target is delegated to `rch exec --`
so agent sessions use the normal remote-compilation path when workers are
available. The contract tests do not run git, project network calls, live Agent
Mail, MCP stdio clients, background services, or fixture update modes. The
script unsets `INSTA_UPDATE` and `UPDATE_AGENT_BASELINE` so a verification run
detects drift instead of regenerating snapshots.

The verifier currently covers:

- schema document goldens: `schema_document_golden_json_all` and
  `schema_document_golden_toon_all`
- command-shape live fixture extraction:
  `schema_command_shapes_match_live_json_outputs`
- JSON/TOON semantic parity:
  `agent_json_and_toon_outputs_match_semantically`
- checked-in agent baselines:
  `agent_baseline_snapshots_match_current_binary`
- optional MCP metadata and payload contracts:
  `cargo test --lib --features mcp mcp_contract`

---

## Workflow Patterns

### Standard Agent Workflow

```
┌─────────────────────────────────────────────────────────────┐
│  1. DISCOVER                                                │
│     br ready --json                                         │
│     → Find unblocked, undeferred issues                     │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│  2. CLAIM                                                   │
│     br update <id> --claim --json                           │
│     → Sets assignee + status=in_progress atomically         │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│  3. WORK                                                    │
│     Implement the task...                                   │
│     → If you find new work:                                 │
│       br create "New issue" --deps discovered-from:<id>     │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│  4. COMPLETE                                                │
│     br close <id> --reason "Done" --json                    │
│     → Optionally: --suggest-next for chained work           │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│  5. FINAL EXPORT CHECK (at session end)                     │
│     br sync --flush-only                                    │
│     → Confirm JSONL is current before committing .beads/    │
└─────────────────────────────────────────────────────────────┘
```

### Claiming Work

```bash
# Atomic claim (recommended)
br update br-123 --claim --json

# Manual claim (equivalent)
br update br-123 --status in_progress --assignee "$BD_ACTOR" --json
```

### Stale Claims and Abandoned Work

`br ready` intentionally hides `in_progress` issues. That keeps agents from
stealing active work, but it also means a crashed session can hide an otherwise
ready issue. Treat an `in_progress` issue as an abandoned-claim candidate only
after checking `updated_at`, the assignee, and the coordination trail.

A claim is normally fresh if it was updated recently or the assignee is still
reachable. In swarm sessions, wait at least two hours since `updated_at` unless
the human operator explicitly says the pane is dead. For human-owned or unclear
claims, use one business day as the default threshold.

Before reclaiming:

```bash
br show <id> --json
br comments list <id> --json
git status --short
```

If Agent Mail is healthy, also check the thread and file reservations for the
issue ID. If the stale owner left session metadata, pane IDs, intended files, or
an Agent Mail name in comments, use that evidence when deciding whether the work
is abandoned. Do not reclaim when the old claim is fresh, the owner is actively
editing the same files, or the dirty tree contains unclear overlapping changes.

When reclaiming abandoned work, leave an audit comment before touching files:

```bash
br comments add <id> --author "$BD_ACTOR" \
  --message "reclaim: previous in_progress claim appears abandoned; evidence: updated_at=<timestamp>, assignee=<name>, no active reservation or pane" \
  --json
br update <id> --claim --json
```

If Agent Mail is down, include the intended file scope in the same comment or a
follow-up degraded-coordination comment. The newest assignee owns the claim, but
the old owner can still return; in that case, coordinate in the bead thread and
split or hand off the work instead of silently overwriting each other.

`br scheduler --json` uses the same coordination age policy for its
`evidence.stale_claim` object, but it deliberately assumes
`reservation_status: "no_snapshot"`. Treat `classification: "no_mail_snapshot"`
and `recommended_action: "inspect_mail"` as a prompt to gather Agent Mail
evidence, not as permission to reclaim the bead.

For a read-only preflight, use coordination status with an explicit reservation
snapshot when available:

```bash
br coordination status --reservations reservations.json --agents agents.jsonl --json
```

MCP-capable agents can read `beads://coordination/status` for the same
`br.coordination.v1` evidence envelope without shelling out. The MCP resource is
read-only and does not call Agent Mail, so it reports
`reservation.state == "no_snapshot"` unless you use the CLI command above with
offline reservation and agent snapshots.

Operator runbook for a queue that appears dry:

```bash
# 1. Confirm actionable work and graph-priority output agree
br ready --json
bv --robot-next

# 2. Inspect hidden in-progress claims without mutating them
br list --status in_progress --json
br coordination status --json

# 3. If a claim looks stale, inspect the local issue trail
br show <id> --json
br comments list <id> --json
git status --short
```

If Agent Mail is healthy, add reservation and liveness snapshots before making
any ownership decision. `br` consumes those snapshots offline; it does not call
Agent Mail itself:

```bash
br coordination status \
  --reservations reservations.jsonl \
  --agents agents.jsonl \
  --json
```

Safe reclaim is still a manual, auditable sequence. Review
`required_human_confirmation`, `reclaim_allowed_by_policy`, and
`suggested_commands` first:

```bash
br coordination status --reservations reservations.jsonl --agents agents.jsonl --json \
  | jq '.claims[] | {id: .issue.id, action: .assessment.recommended_action, reclaim_allowed_by_policy, required_human_confirmation, suggested_commands}'

br comments add <id> --author "$BD_ACTOR" \
  --message "reclaim: previous in_progress claim appears abandoned; evidence: updated_at=<timestamp>, assignee=<name>, no active reservation or pane" \
  --json
br update <id> --claim --json
```

Only run the final two commands when the advisory output and human policy allow
it. `br coordination status` never auto-reclaims, never runs git, and never
creates or releases Agent Mail reservations.

The output is advisory only. `reclaim_allowed_by_policy=true` means the local
policy and supplied snapshot evidence allow the documented audit-comment plus
claim sequence. `suggested_commands` is empty for fresh claims, active
reservations, missing or malformed snapshots, and human or unknown owners.
`required_human_confirmation=true` means ask the owner or operator instead of
copying a claim command.

When a coordination snapshot matters for a handoff or review, record it through
the audit log before taking follow-up action:

```bash
br coordination status --json \
  | br audit coordination --stdin --command "br coordination status --json" --json
```

This appends one `coordination_incident` interaction per claim to the existing
`.beads/interactions.jsonl` flight recorder. The recorded fields are bounded and
normalized: `issue_id`, `classification`, `recommended_action` as
`suggested_action`, `evidence_summary`, the producing `command`, and a stable
`snapshot_hash`. After a human or agent reviews the evidence, label the
interaction with `br audit label <interaction-id> --label reviewed --json`.

### Creating Related Issues

```bash
# Bug discovered during feature work
br create "Edge case causes crash" \
  -t bug \
  -p 1 \
  --deps discovered-from:br-123 \
  --json

# Subtask for epic
br create "Implement auth middleware" \
  -t task \
  --parent br-epic-456 \
  --json
```

### Closing with Suggestions

```bash
# Close and get next unblocked work
br close br-123 --suggest-next --json
```

Returns:
```json
{
  "closed": "br-123",
  "unblocked": ["br-456", "br-789"]
}
```

### Degraded Coordination Without Agent Mail

The normal swarm workflow uses MCP Agent Mail for file reservations and
threaded coordination. If Mail is unavailable, `br` still provides enough
advisory state to avoid silent overlap. This fallback is intentionally weaker
than Mail reservations, so keep scopes narrow and prefer another ready issue if
there is any sign of collision.

1. Confirm the coordination channel is actually degraded. For agents, that
   usually means the Agent Mail health check or reservation call failed. Record
   the failure in the bead, not just in the terminal transcript.

2. Claim the bead with an actor or session identity:

   ```bash
   export AGENT_NAME="${AGENT_NAME:-codex-agent}"
   br update <id> --status in_progress --assignee "$AGENT_NAME" --json
   ```

3. Add an issue comment naming the intended files before editing:

   ```bash
   br comments add <id> --author "$AGENT_NAME" \
     --message "degraded-coordination: Agent Mail unavailable; files: src/foo.rs, tests/foo.rs" \
     --json
   ```

4. Check the local collision surface:

   ```bash
   git status --short
   br list --status in_progress --json
   br comments list <id> --json
   ```

   If another live claim or comment names the same files, do not rely on the
   fallback comment as a lock. Pick different ready work, split the file scope,
   or wait for the other agent to finish.

5. If the edit surface changes, add another comment before touching the new
   files. At completion, close the bead with a reason that states Mail was
   unavailable, then run `br sync --flush-only` and commit the code plus
   `.beads/` changes together.

6. If you find old `in_progress` work while Mail is degraded, use the stale
   claim protocol above. A stale claim is not automatically safe to take just
   because Mail is unavailable; require age plus evidence that the owner is no
   longer active.

This protocol does not replace Agent Mail. It is a shared audit trail for
degraded sessions so abandoned work can be found through `br list --status
in_progress --json`, `br comments list <id> --json`, and git history.

---

## Parsing JSON Output

### Python Example

```python
import json
import subprocess


class BrError(RuntimeError):
    def __init__(self, exit_code, envelope, partial, stdout, stderr):
        error = envelope.get("error", {})
        message = error.get("message") or f"br exited {exit_code}"
        super().__init__(message)
        self.exit_code = exit_code
        self.envelope = envelope
        # Payload document from a partially applied batch (e.g. `close`
        # with a blocked issue in the list), or None. See docs/agent/ERRORS.md.
        self.partial = partial
        self.code = error.get("code")
        self.hint = error.get("hint")
        self.stdout = stdout
        self.stderr = stderr


def _parse_json_documents(text):
    """Parse a stream of concatenated JSON documents (stdout may carry a
    partial-batch payload followed by the error envelope)."""
    decoder = json.JSONDecoder()
    docs, idx, text = [], 0, text.strip()
    while idx < len(text):
        doc, end = decoder.raw_decode(text, idx)
        docs.append(doc)
        idx = end
        while idx < len(text) and text[idx].isspace():
            idx += 1
    return docs


def br_command(*args):
    """Run br command and return parsed stdout JSON."""
    result = subprocess.run(
        ['br', '--json', *args],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        # The structured envelope is the LAST JSON document on STDOUT
        # (stderr carries human diagnostics only — docs/agent/ERRORS.md).
        docs = _parse_json_documents(result.stdout) if result.stdout.strip() else []
        envelope = docs[-1] if docs and isinstance(docs[-1], dict) and "error" in docs[-1] else {}
        partial = docs[0] if len(docs) > 1 else None
        raise BrError(result.returncode, envelope, partial, result.stdout, result.stderr)
    return json.loads(result.stdout)

# Find ready work
ready = br_command('ready', '--limit', '5')
for issue in ready:
    print(f"{issue['id']}: {issue['title']}")

# Claim first issue
if ready:
    br_command('update', ready[0]['id'], '--claim')
```

### JavaScript/Node Example

```javascript
const { spawnSync } = require('node:child_process');

// stdout may carry several concatenated JSON documents: a partial-batch
// payload followed by the error envelope (see docs/agent/ERRORS.md).
function parseJsonDocuments(text) {
  const docs = [];
  let rest = text.trim();
  while (rest.length > 0) {
    let depth = 0, inStr = false, esc = false, end = -1;
    for (let i = 0; i < rest.length; i++) {
      const c = rest[i];
      if (esc) { esc = false; continue; }
      if (inStr) {
        if (c === '\\') esc = true;
        else if (c === '"') inStr = false;
        continue;
      }
      if (c === '"') inStr = true;
      else if (c === '{' || c === '[') depth++;
      else if (c === '}' || c === ']') {
        depth--;
        if (depth === 0) { end = i + 1; break; }
      }
    }
    if (end < 0) break;
    docs.push(JSON.parse(rest.slice(0, end)));
    rest = rest.slice(end).trim();
  }
  return docs;
}

function br(...args) {
  const result = spawnSync('br', ['--json', ...args], {
    encoding: 'utf-8',
    stdio: ['ignore', 'pipe', 'pipe']
  });
  if (result.status !== 0) {
    // The structured envelope is the LAST JSON document on STDOUT
    // (stderr carries human diagnostics only).
    const docs = result.stdout.trim() ? parseJsonDocuments(result.stdout) : [];
    const last = docs[docs.length - 1];
    const envelope = last && typeof last === 'object' && 'error' in last ? last : {};
    const error = envelope.error || {};
    const err = new Error(error.message || `br exited ${result.status}`);
    err.exitCode = result.status;
    err.code = error.code;
    err.hint = error.hint;
    err.envelope = envelope;
    // Payload document from a partially applied batch, if any.
    err.partial = docs.length > 1 ? docs[0] : undefined;
    throw err;
  }
  return JSON.parse(result.stdout);
}

// Find ready work
const ready = br('ready', '--limit', '5');
console.log(`Found ${ready.length} ready issues`);

// Claim and work
if (ready.length > 0) {
  br('update', ready[0].id, '--claim');
}
```

### jq Examples

```bash
# Get IDs of all ready issues
br ready --json | jq -r '.[].id'

# Get high-priority bugs
br list --json -t bug -p 0 -p 1 | jq '.issues[] | "\(.id): \(.title)"'

# Count by status
br list --json -a | jq '.issues | group_by(.status) | map({status: .[0].status, count: length})'

# Find my assigned work
br list --json --assignee $(whoami) | jq '.issues[].title'
```

---

## Error Handling

### Exit Codes

| Code | Category | Example |
|------|----------|---------|
| 0 | Success | Command completed |
| 1 | Internal | Unexpected error |
| 2 | Database | Not initialized |
| 3 | Issue | Issue not found |
| 4 | Validation | Invalid priority value |
| 5 | Dependency | Cycle detected |
| 6 | Sync/JSONL | Parse error |
| 7 | Config | Missing config |
| 8 | I/O | File not found |

### Structured Error Response

With `--json`, the machine-readable result is written to stdout: successful command data on exit `0`, and the structured error envelope on non-zero exits (stderr carries human diagnostics, `RUST_LOG` tracing output, and non-fatal structured warnings such as `AUTO_FLUSH_FAILED` — never the envelope). On a partial-batch failure the envelope is preceded by a payload document describing what did apply — parse stdout as a stream of JSON documents and treat the last one's `error` key as the envelope. See [docs/agent/ERRORS.md](agent/ERRORS.md) for the full contract.

```json
{
  "error": {
    "code": "ISSUE_NOT_FOUND",
    "message": "Issue not found: br-xyz999",
    "hint": "Run 'br list' to see available issues.",
    "retryable": false,
    "context": {
      "searched_id": "br-xyz999"
    }
  }
}
```

### Error Recovery Patterns

```python
def safe_close(issue_id, reason):
    """Close with retry on transient errors."""
    for attempt in range(3):
        try:
            return br_command('close', issue_id, '-r', reason)
        except RuntimeError as e:
            if 'database locked' in str(e) and attempt < 2:
                time.sleep(0.5)
                continue
            raise
```

---

## MCP Server

`br serve` exposes the issue tracker as a Model Context Protocol server. It is
an alternative to shelling out to `br --json ...` when an MCP-capable agent wants
tool discovery, resource reads, guided prompts, and structured tool errors.

### Build and Start

The MCP server is feature-gated and is not included in default builds:

```bash
cargo build --release --features mcp
RUST_LOG=error ./target/release/br serve --actor codex
```

Installed binary:

```bash
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git beads_rust --locked --features mcp
RUST_LOG=error br serve --actor codex
```

Transport is stdio. Configure your MCP client to launch `br` as a child process;
do not point it at a port.

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

### Exposed Surface

Tools:

- `list_issues`
- `show_issue`
- `create_issue`
- `update_issue`
- `close_issue`
- `manage_dependencies`
- `project_overview`

Resources:

- `beads://project/info`
- `beads://issues/{id}`
- `beads://schema`
- `beads://labels`
- `beads://issues/ready`
- `beads://issues/blocked`
- `beads://issues/in_progress`
- `beads://coordination/status`
- `beads://issues/deferred`
- `beads://issues/bottlenecks`
- `beads://graph/health`
- `beads://events/recent`

Prompts:

- `triage`
- `status_report`
- `plan_next_work`
- `polish_backlog`

### Safety and Locking

MCP serve uses the same local storage contract as the CLI:

- It opens the current workspace discovered from the process working directory
  and CLI overrides.
- It does not run git, push, pull, or talk to remote services.
- It does not listen on a network socket; access is limited to the client that
  starts the stdio process.
- Mutating tools acquire the workspace `.write.lock`, write audit events with
  the configured `--actor`, and attempt the normal JSONL auto-flush after a
  successful mutation.
- Handlers open fresh SQLite connections rather than sharing one long-lived
  connection across MCP calls.

### When to Prefer MCP

Use MCP when an agent is already MCP-native, needs to discover available actions
without memorizing CLI flags, or should receive structured recovery data such as
`suggested_tool_calls`. Use shell commands with `--json` for short scripts,
bulk pipelines, and workflows that need standard Unix composition with `jq`.

---

## Robot Mode Flags

These flags enable machine-friendly output:

| Flag | Description |
|------|-------------|
| `--json` | JSON output for all data |
| `--robot` | Alias for `--json` |
| `--silent` | Output only essential data (e.g., just ID for create) |
| `--quiet` | Suppress non-error output |
| `--no-color` | Disable ANSI colors |

### Combining Flags

```bash
# Machine-friendly create
br create "New issue" --silent
# Output: br-abc123

# Quiet mode with JSON
br close br-123 --quiet --json
# Outputs JSON, no status messages
```

---

## Swarm-Scale Tuning

For 256GB+ RAM and 64+ core agent hosts, see
[Swarm-Scale Tuning](SWARM_SCALE_TUNING.md). It covers conservative defaults,
high-core build hygiene, `.write.lock` timeout profiles, Agent Mail reservation
patterns, MCP serve topology, performance evidence collection, and rollback
rules for future snapshot/cache/controller features.

---

## Agent-Specific Configuration

### Claude Code / Anthropic Agents

```bash
# Set actor for audit trail
export BD_ACTOR="claude-agent"
export RUST_LOG=error

# Workflow
br ready --json --limit 10
br update <id> --claim
# ... work ...
br close <id> --reason "Completed by Claude"
br sync --flush-only  # final JSONL export check before committing .beads/
```

### Cursor AI

```bash
# Initialize in project
br init --prefix cursor
export RUST_LOG=error

# Use with Cursor's tool system
br ready --json
br show <id> --json
```

### Aider

```bash
# Aider integration
export BD_ACTOR="aider-$(date +%Y%m%d)"

# Check work before session
br ready --json | head -5
```

### GitHub Copilot Workspace

```bash
# Copilot-friendly workflow
br ready --json --assignee copilot
br update <id> --status in_progress --assignee copilot
```

---

## Best Practices

### DO

1. **Always use `--json`** for programmatic access
2. **Check exit codes** before parsing output
3. **Set `BD_ACTOR`** for audit trail attribution
4. **Use `--claim`** for atomic status+assignee updates
5. **Create discovered issues** with `--deps discovered-from:<id>`
6. **Run a final JSONL export check** at session end with `br sync --flush-only`
7. **Use `br ready`** to find actionable work
8. **Include reasons** when closing issues
9. **Use degraded comments** only when Agent Mail reservations are unavailable

### DON'T

1. **Don't parse human output** - use `--json` instead
2. **Don't edit JSONL directly** - always use br commands
3. **Don't skip sync** - other agents need your changes
4. **Don't hold issues indefinitely** - close or unassign if stuck
5. **Don't create duplicate issues** - search first
6. **Don't ignore errors** - check exit codes and error messages

### Session Management

```bash
# Session start
br ready --json > /tmp/session_start.json

# Session end checklist
br sync --flush-only  # idempotent; mutations normally auto-flushed already
git add .beads/
git commit -m "Update issues"
```

### Concurrent Agent Safety

```bash
# Use lock timeout for busy databases
br list --json --lock-timeout 5000

# Check for stale data
br sync --status --json
```

---

## Integration with bv (beads_viewer)

For advanced analysis, use `bv` robot commands:

```bash
# Priority analysis
bv --robot-priority | jq '.recommendations[0]'

# Dependency insights
bv --robot-insights | jq '.Bottlenecks'

# Execution plan
bv --robot-plan | jq '.parallel_groups'
```

See [AGENTS.md](../AGENTS.md) for detailed bv integration.

---

## Troubleshooting

### Common Issues

**"Database not initialized"**
```bash
br init
```

**"Issue not found"**
```bash
# Use partial ID matching
br show abc  # Matches br-abc123

# List to find correct ID
br list --json | jq '.issues[].id'
```

**"Database locked"**
```bash
# Increase lock timeout
br list --json --lock-timeout 10000
```

**"Cycle detected"**
```bash
# Check for cycles
br dep cycles --json

# Remove problematic dependency
br dep remove br-123 br-456
```

### Debug Logging

```bash
# Enable debug output
RUST_LOG=debug br ready --json 2>debug.log

# Verbose mode
br sync --flush-only -vv
```

---

## See Also

- [CLI_REFERENCE.md](CLI_REFERENCE.md) - Complete command reference
- [AGENTS.md](../AGENTS.md) - Agent development guidelines
- [README.md](../README.md) - Project overview
- [SYNC_SAFETY.md](SYNC_SAFETY.md) - Sync safety model
