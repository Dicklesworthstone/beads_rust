# Coordination Evidence Contract

Status: implemented helper contract shared by coordination CLI, MCP, scheduler,
and audit work. The read-only `br coordination status` command diagnoses claims;
the explicit `br coordination reclaim` command performs a guarded, audited
handoff.

## Purpose

Large agent swarms can leave work hidden in `in_progress` when a process dies or
an operator loses track of panes. `br ready` correctly hides active claims, but
operators still need a read-only way to decide whether a hidden claim is fresh,
stale, ambiguous, or protected by an active Agent Mail reservation.

The durable contract is `br.coordination.v1`, implemented as pure data types and
classification helpers in `src/coordination.rs`.

## Non-Goals

- No automatic claim stealing.
- No live Agent Mail calls from `br`.
- No git operations.
- No background daemon.
- No ownership mutation from `coordination status` or the shared classifier.

## Evidence Inputs

The classifier uses only caller-provided evidence:

- Issue metadata: status, assignee, and `updated_at`.
- Owner kind: `swarm_agent`, `human`, or `unknown`.
- Optional Agent Mail snapshot evidence supplied by a caller or fixture.

Missing Agent Mail data is explicit evidence, not proof of abandonment.
An explicit human owner policy takes precedence over matching agent snapshot
metadata, so a human name in an agent export cannot enable reclamation.

## Policy Matrix

| Condition | Classification | Recommended action |
| --- | --- | --- |
| Empty or whitespace assignee | `unassigned` | `observe` |
| Age below owner threshold | `fresh` | `observe` |
| Active matching reservation after stale threshold | `blocked_by_active_reservation` | `leave_active` |
| Stale age but no Agent Mail snapshot | `no_mail_snapshot` | `inspect_mail` |
| Stale age with invalid snapshot | `ambiguous` | `inspect_mail` |
| Stale swarm-agent age with no active reservation | `stale_candidate` | `reclaim_candidate` |
| Abandoned-likely swarm-agent age with no active reservation | `abandoned_likely` | `reclaim_candidate` |
| Stale human or unknown owner with no active reservation | `stale_candidate` | `ask_owner` |
| Abandoned-likely human or unknown owner with no active reservation | `abandoned_likely` | `ask_owner` |

Default thresholds:

- Swarm-agent stale candidate: 120 minutes.
- Swarm-agent abandoned-likely marker: 480 minutes.
- Human or unknown stale candidate: 1440 minutes.
- Human or unknown abandoned-likely marker: 4320 minutes.

These thresholds match the existing AGENTS.md guidance that swarm-agent claims
are stale candidates after two quiet hours, while human or unclear claims use a
one-business-day rule of thumb. The abandoned-likely marker is deliberately more
conservative and remains advisory.

## Output Shape

The top-level machine-readable envelope is:

```text
CoordinationStatusOutput {
  schema_version: "br.coordination.v1",
  generated_at: DateTime<Utc>,
  summary: CoordinationSummary,
  claims: [CoordinationClaimRow]
}
```

Each claim assessment includes:

- assignee after trimming whitespace,
- owner kind,
- updated timestamp and computed age in minutes,
- stale and abandoned thresholds,
- reservation evidence,
- classification,
- recommended action,
- evidence source list.

Each claim row also includes advisory-only reclaim guidance:

- `reclaim_allowed_by_policy`: true only when the shared policy has enough
  evidence to suggest a guarded reclaim attempt for a swarm-agent claim.
- `required_human_confirmation`: true for human or unknown owners that are old
  enough to inspect but must not be reclaimed automatically.
- `evidence_summary`: compact evidence values suitable for an audit comment,
  including `updated_at`, assignee, owner kind, age, thresholds, and
  reservation/snapshot status.
- `suggested_commands`: shell commands for review before execution. An eligible
  claim has one `br coordination reclaim` command carrying its observed
  assignee, `updated_at`, snapshot paths, owner policy, and explicit new actor.
  Commands are empty for fresh claims, active reservations, missing/invalid
  snapshots, and human or unknown ownership.

The v1 schema retains the historical `add_reclaim_audit_comment` and
`claim_issue` purposes so older saved snapshots remain readable. New advice
emits only `reclaim_issue`. Regenerate advice before a handoff instead of
executing the obsolete two-command sequence from a saved snapshot.

Future CLI and MCP surfaces should expose this shape directly in JSON mode and
may convert it to TOON using the normal output layer. Human text output should be
a projection of the same fields, not a separate policy.

## CLI Surface

`br coordination status` enumerates `in_progress` claims and emits the shared
`br.coordination.v1` envelope:

```bash
br coordination status --json
br coordination status --format toon
br coordination status --owner-kind swarm-agent
br coordination status --reservations reservations.json --agents agents.jsonl --json
```

The command is read-only. It opens the same local storage as other list-style
commands, computes local issue counts and relation counts, attaches bounded
latest-comment excerpts, and classifies each claim with either explicit
`no_snapshot` evidence or an operator-provided offline Agent Mail snapshot.

Snapshot files are optional and local-only. `br` does not call Agent Mail or any
network service. Reservation snapshots may be JSON arrays, wrapper objects with a
`reservations` array, or JSONL streams with one row per reservation:

```json
{
  "holder": "TopazFox",
  "path_pattern": "src/coordination.rs",
  "exclusive": true,
  "reason": "beads_rust-sc6u",
  "expires_ts": "2026-05-08T12:00:00Z",
  "released_ts": null,
  "thread_id": "beads_rust-sc6u"
}
```

Agent snapshots use the same JSON/JSONL wrapping rules with an `agents` array:

```json
{
  "name": "TopazFox",
  "task_description": "coordination status work",
  "last_active_ts": "2026-05-08T10:00:00Z",
  "contact_policy": "auto"
}
```

Reservations correlate to claims by assignee/holder, bead id in
`reason`/`thread_id`, or path patterns named in recent comments. A matching
active reservation changes stale-looking work to
`blocked_by_active_reservation` with `leave_active`. Missing snapshots remain
`no_mail_snapshot`; supplied snapshots with no matching reservation become
`no_reservation` evidence and allow the normal stale/reclaim policy to apply.
Malformed or unreadable snapshot files fail with structured validation errors
instead of silently weakening evidence.

## Audit Flight Recorder

`br audit coordination --stdin` records coordination snapshots through the
existing `.beads/interactions.jsonl` audit log. It accepts the same
`br.coordination.v1` status object emitted by `br coordination status --json`,
JSON arrays of claims, or JSONL streams of claim rows/wrapper objects:

```bash
br coordination status --json \
  | br audit coordination --stdin --command "br coordination status --json" --json
```

For every claim in the snapshot, the command appends a `coordination_incident`
interaction with the claim issue id and bounded normalized `extra` fields:

- `command`
- `issue_id`
- `classification`
- `evidence_summary`
- `snapshot_hash`
- `suggested_action`

The snapshot hash is computed over normalized JSON with sorted object keys, so
field order differences do not create separate evidence hashes. Long command and
evidence strings are sanitized for terminal control characters and truncated
before they enter the audit log.

This is a durable flight recorder, not a second coordination state store.
Reviews, follow-up labels, and issue audit views continue to use the ordinary
`br audit label`, `br audit log`, and `br audit summary` surfaces.

## Agent Mail Boundary

Agent Mail remains the collision-avoidance layer. `br` must not depend on a live
MCP service. Coordination commands accept explicit snapshot files describing
reservations and agent liveness. Absence of a usable snapshot is reported as
`no_mail_snapshot` or `ambiguous`, never as proof that reclaiming is safe.
Refresh snapshot files and inspect pane/process activity before a handoff;
`br` cannot detect live activity omitted from an offline snapshot.

## Scheduler Boundary

The scheduler reuses the same claim classifier for stale assignment evidence,
but it does not parse Agent Mail snapshots. Scheduler rows therefore classify
assigned stale work with `reservation_status: "no_snapshot"` and
`recommended_action: "inspect_mail"` instead of claiming abandonment. The
`--stale-claim-hours` option still controls the scheduler's age threshold, while
the classifier keeps the owner/reservation semantics aligned with
`br coordination status`. When in doubt, scheduler output points agents to the
coordination status surface for deeper diagnosis.

## Reclaim Boundary

`reclaim_candidate` is advisory. `coordination status` never executes the
suggested command. After reviewing the evidence and checking external
pane/process activity, an operator or agent can run the emitted guarded handoff.
For a claim observed as owned by `OldAgent` at the timestamp below:

```bash
br coordination reclaim bd-abc123 \
  --expected-assignee=OldAgent \
  --if-unchanged '2026-11-04T00:00:00Z' \
  --reservations reservations.json --agents agents.jsonl \
  --owner-kind swarm-agent \
  --actor="${AGENT_NAME:?Set AGENT_NAME to the reclaiming agent}" --json
```

The command re-reads the supplied snapshot files and applies the same policy to
the current issue. Under the ownership transaction, it requires eligible claim
status and the same assignee and `updated_at` as the observation. It inserts the
reclaim audit comment first, using the new actor as author, then assigns the
issue directly to that actor with status `in_progress`. Audit and ownership commit
together. A failed guard or policy check leaves both unchanged, and concurrent
claimers cannot both succeed.

There is no separate `comments add`, unconditional requeue, or ordinary
`update --claim` step. Adding a comment beforehand changes `updated_at` and
invalidates the guard. `update --claim` retains its refusal of another actor's
work. On a contention refusal, diagnostics expose the current claim and mark
the error `retryable: false`; refresh the evidence rather than replaying the
old command. Repeated use of a successful command is also refused.

Human or unknown ownership keeps the safer `ask_owner` recommendation even after
the stale threshold. Active reservations, fresh claims, and missing or invalid
snapshots remain ineligible. Snapshot evidence is local and advisory; an old
status output cannot authorize overwriting a newer database assignment.
