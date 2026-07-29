//! MCP resource handlers for the beads issue tracker.
//!
//! Resources provide read-only discovery endpoints that agents can inspect
//! before calling tools.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use fastmcp_rust::{
    McpContext, McpError, McpErrorCode, McpResult, Resource, ResourceContent, ResourceHandler,
    ResourceTemplate,
};
use serde_json::{Value, json};

use crate::cli::commands::coordination::build_coordination_status_without_snapshots;
use crate::coordination::ClaimOwnerKind;
use crate::error::StructuredError;
use crate::model::{Event, Issue, Status};
use crate::storage::{ListFilters, SqliteStorage};

use super::{BeadsState, ensure_not_shutting_down, mcp_ready_issues, to_mcp};

fn read_project_config(storage: &SqliteStorage) -> McpResult<HashMap<String, String>> {
    storage.get_all_config().map_err(to_mcp)
}

fn resource_json(uri: &str, value: &Value) -> Vec<ResourceContent> {
    vec![ResourceContent {
        uri: uri.to_string(),
        mime_type: Some("application/json".into()),
        text: Some(value.to_string()),
        blob: None,
    }]
}

fn cached_resource_json<F>(
    state: &BeadsState,
    uri: &str,
    key: String,
    build: F,
) -> McpResult<Vec<ResourceContent>>
where
    F: FnOnce(&SqliteStorage) -> McpResult<Value>,
{
    if let Some(value) = state.cached_read_json(&key) {
        return Ok(resource_json(uri, &value));
    }

    let before = state.capture_read_snapshot_witness();
    let storage = state.open_read_storage().map_err(to_mcp)?;
    let value = build(&storage)?;
    state.store_read_json_snapshot(key, before, &value);

    Ok(resource_json(uri, &value))
}

/// Build a structured "issue not found" error with fuzzy suggestions,
/// mirroring the tools.rs pattern for consistent agent UX.
fn issue_not_found_resource(storage: &SqliteStorage, id: &str) -> McpResult<McpError> {
    let all_ids = storage.get_all_ids().map_err(to_mcp)?;
    let structured = StructuredError::issue_not_found(id, &all_ids);

    let mut data = json!({
        "error_type": "ISSUE_NOT_FOUND",
        "recoverable": true,
        "message": structured.message,
        "discovery_hint": "Use list_issues tool to find valid issue IDs",
    });

    if let Some(hint) = &structured.hint {
        data["hint"] = json!(hint);
    }
    if let Some(ctx) = &structured.context
        && let Some(similar) = ctx.get("similar_ids")
    {
        data["suggestions"] = similar.clone();
    }

    data["suggested_tool_calls"] = json!([{"tool": "list_issues", "arguments": {}}]);

    Ok(McpError::with_data(
        McpErrorCode::ToolExecutionError,
        structured.message,
        data,
    ))
}

const COORDINATION_STATUS_URI: &str = "beads://coordination/status";

fn coordination_status_error(message: impl Into<String>) -> McpError {
    let message = message.into();
    McpError::with_data(
        McpErrorCode::ToolExecutionError,
        format!("failed to build coordination status: {message}"),
        json!({
            "error_type": "COORDINATION_STATUS_FAILED",
            "recoverable": true,
            "message": message,
            "resource": COORDINATION_STATUS_URI,
            "suggested_tool_calls": [
                {"tool": "project_overview", "arguments": {}}
            ],
            "suggested_cli_commands": [
                "br coordination status --json",
                "br show <id> --json",
                "br comments list <id> --json"
            ],
            "snapshot_hint": "This MCP resource is read-only and does not call Agent Mail. Use the CLI --reservations and --agents flags when reservation evidence is required."
        }),
    )
}

fn coordination_status_resource_json_at(
    storage: &SqliteStorage,
    generated_at: DateTime<Utc>,
) -> McpResult<Value> {
    let output = build_coordination_status_without_snapshots(
        storage,
        ClaimOwnerKind::SwarmAgent,
        2,
        generated_at,
    )
    .map_err(|err| coordination_status_error(err.to_string()))?;
    serde_json::to_value(output).map_err(|err| coordination_status_error(err.to_string()))
}

fn coordination_status_resource_json(storage: &SqliteStorage) -> McpResult<Value> {
    coordination_status_resource_json_at(storage, Utc::now())
}

// ---------------------------------------------------------------------------
// 1. project/info — static project metadata
// ---------------------------------------------------------------------------

pub struct ProjectInfoResource(Arc<BeadsState>);
impl ProjectInfoResource {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl ResourceHandler for ProjectInfoResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "beads://project/info".into(),
            name: "Project Info".into(),
            description: Some(
                "Workspace metadata: beads directory, issue prefix, configuration. \
                 Read this first to understand the project context. \
                 Used by: project_overview tool returns similar data with more detail."
                    .into(),
            ),
            mime_type: Some("application/json".into()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        ensure_not_shutting_down()?;

        cached_resource_json(
            &self.0,
            "beads://project/info",
            "resource:project_info".to_string(),
            |storage| {
                let config = read_project_config(storage)?;
                let prefix = self.0.issue_prefix.as_deref().unwrap_or("br");

                Ok(json!({
                    "beads_dir": self.0.beads_dir.display().to_string(),
                    "issue_prefix": prefix,
                    "actor": self.0.actor,
                    "config": config,
                }))
            },
        )
    }
}

// ---------------------------------------------------------------------------
// 2. issues/{id} — individual issue resource template
// ---------------------------------------------------------------------------

pub struct IssueResource(Arc<BeadsState>);
impl IssueResource {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

fn issue_resource_json(storage: &SqliteStorage, id: &str) -> McpResult<Value> {
    let maybe_details = storage
        .get_issue_details(id, true, true, 20)
        .map_err(to_mcp)?;
    let Some(details) = maybe_details else {
        return Err(issue_not_found_resource(storage, id)?);
    };

    let mut result = serde_json::to_value(&details.issue).unwrap_or_default();
    if let Some(obj) = result.as_object_mut() {
        obj.insert("labels".into(), json!(details.labels));
        obj.insert("comments".into(), json!(details.comments));
        obj.insert(
            "dependencies".into(),
            json!(
                details
                    .dependencies
                    .iter()
                    .map(|d| {
                        json!({"id": d.id, "title": d.title, "status": d.status, "dep_type": d.dep_type})
                    })
                    .collect::<Vec<_>>()
            ),
        );
        obj.insert(
            "dependents".into(),
            json!(
                details
                    .dependents
                    .iter()
                    .map(|d| {
                        json!({"id": d.id, "title": d.title, "status": d.status, "dep_type": d.dep_type})
                    })
                    .collect::<Vec<_>>()
            ),
        );
        if let Some(parent) = &details.parent {
            obj.insert("parent".into(), json!(parent));
        }
    }

    Ok(result)
}

impl ResourceHandler for IssueResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "beads://issues/{id}".into(),
            name: "Issue Details".into(),
            description: Some(
                "Full issue details by ID. Discovery: use list_issues tool to find IDs. \
                 Used by: Complements show_issue tool which returns the same data."
                    .into(),
            ),
            mime_type: Some("application/json".into()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: "beads://issues/{id}".into(),
            name: "Issue Details".into(),
            description: Some("Full issue details by ID".into()),
            mime_type: Some("application/json".into()),
            icon: None,
            version: None,
            tags: vec![],
        })
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        ensure_not_shutting_down()?;

        Err(McpError::invalid_params(
            "Provide an issue ID via the URI template: beads://issues/{id}",
        ))
    }

    fn read_with_uri(
        &self,
        _ctx: &McpContext,
        uri: &str,
        params: &HashMap<String, String>,
    ) -> McpResult<Vec<ResourceContent>> {
        ensure_not_shutting_down()?;

        let id = params.get("id").ok_or_else(|| {
            McpError::invalid_params("'id' parameter is required in the URI template")
        })?;

        cached_resource_json(&self.0, uri, format!("resource:issue:{id}"), |storage| {
            issue_resource_json(storage, id)
        })
    }
}

// ---------------------------------------------------------------------------
// 3. schema — JSON schema reference
// ---------------------------------------------------------------------------

pub struct SchemaResource;

impl ResourceHandler for SchemaResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "beads://schema".into(),
            name: "Issue Schema Reference".into(),
            description: Some(
                "Reference for issue fields, valid statuses, priorities, types, \
                 and dependency types. Read this to understand what values are accepted."
                    .into(),
            ),
            mime_type: Some("application/json".into()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        ensure_not_shutting_down()?;

        let schema = json!({
            "statuses": {
                "values": ["open", "in_progress", "blocked", "deferred", "draft", "closed", "pinned"],
                "aliases": {
                    "open": ["new", "todo"],
                    "in_progress": ["wip", "working", "active", "started", "in-progress", "inprogress"],
                    "blocked": ["stuck", "waiting"],
                    "deferred": ["later", "postponed", "backlogged"],
                    "closed": ["done", "completed", "resolved", "fixed", "wontfix", "cancelled"],
                    "pinned": ["sticky", "hold", "on_hold", "on-hold"]
                }
            },
            "priorities": {
                "values": ["critical", "high", "medium", "low", "backlog"],
                "aliases": {
                    "critical": ["p0", "urgent", "asap", "emergency"],
                    "high": ["p1", "important"],
                    "medium": ["p2", "normal", "default", "mid"],
                    "low": ["p3", "minor", "trivial", "nice_to_have", "nice-to-have"],
                    "backlog": ["p4", "someday", "eventually", "whenever"]
                }
            },
            "issue_types": {
                "values": ["task", "bug", "feature", "epic", "chore", "docs", "question"],
                "aliases": {
                    "task": ["issue"],
                    "bug": ["bugfix", "defect", "regression"],
                    "feature": ["feat", "enhancement", "story", "request"],
                    "chore": ["maintenance", "cleanup", "refactor", "tech_debt", "tech-debt"],
                    "docs": ["documentation", "doc"],
                    "question": ["q", "help"]
                }
            },
            "dependency_types": [
                "blocks", "related", "parent-child", "waits-for", "duplicates",
                "supersedes", "caused-by", "conditional-blocks", "discovered-from",
                "replies-to", "relates-to"
            ],
            "issue_fields": {
                "id": "string — unique ID (e.g. br-abc123)",
                "title": "string — 1-500 characters",
                "description": "string|null — detailed description",
                "status": "string — see statuses above",
                "priority": "object — {value: 0-4}",
                "issue_type": "string — see issue_types above",
                "assignee": "string|null",
                "owner": "string|null",
                "labels": "string[] — attached labels",
                "parent": "string|null — parent issue ID (via parent-child dependency; read-only in show_issue)",
                "created_at": "ISO 8601 timestamp",
                "updated_at": "ISO 8601 timestamp",
                "closed_at": "ISO 8601 timestamp|null",
                "close_reason": "string|null",
                "due_at": "ISO 8601 timestamp|null",
                "defer_until": "ISO 8601 timestamp|null",
                "estimated_minutes": "integer|null",
                "external_ref": "string|null — external tracker reference"
            },
            "bead_anatomy": {
                "purpose": "Recommended structure for issue descriptions to ensure self-containment and completeness",
                "sections": {
                    "background": "Why this issue exists — context and motivation",
                    "technical_approach": "How to implement — key design decisions and approach",
                    "success_criteria": "How to verify done — concrete, testable conditions",
                    "test_plan": "Unit and integration tests required — specific test cases",
                    "considerations": "Edge cases, risks, and things to watch out for"
                },
                "principles": [
                    "Self-contained: understandable without consulting external plans",
                    "Granular: one coherent piece of work per issue",
                    "Complete: preserve ALL complexity, do not oversimplify",
                    "Dependency-aware: make ALL blocking relationships explicit",
                    "Test-inclusive: every feature issue should have a companion test plan"
                ]
            }
        });

        Ok(vec![ResourceContent {
            uri: "beads://schema".into(),
            mime_type: Some("application/json".into()),
            text: Some(schema.to_string()),
            blob: None,
        }])
    }
}

// ---------------------------------------------------------------------------
// 4. labels — discovery resource for valid label values
// ---------------------------------------------------------------------------

pub struct LabelsResource(Arc<BeadsState>);
impl LabelsResource {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl ResourceHandler for LabelsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "beads://labels".into(),
            name: "Labels".into(),
            description: Some(
                "All labels in use with issue counts. Read this to discover valid \
                 label values before filtering with list_issues or tagging with update_issue. \
                 Used by: list_issues (labels filter), update_issue (labels_add/labels_remove), \
                 create_issue (labels param)."
                    .into(),
            ),
            mime_type: Some("application/json".into()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        ensure_not_shutting_down()?;

        cached_resource_json(
            &self.0,
            "beads://labels",
            "resource:labels".to_string(),
            |storage| {
                let labels = storage.get_unique_labels_with_counts().map_err(to_mcp)?;

                Ok(json!({
                    "labels": labels.iter().map(|(name, count)| {
                        json!({"name": name, "count": count})
                    }).collect::<Vec<_>>(),
                }))
            },
        )
    }
}

// ---------------------------------------------------------------------------
// 5. issues/ready — actionable work items
// ---------------------------------------------------------------------------

pub struct ReadyIssuesResource(Arc<BeadsState>);
impl ReadyIssuesResource {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl ResourceHandler for ReadyIssuesResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "beads://issues/ready".into(),
            name: "Ready Issues".into(),
            description: Some(
                "Issues ready for work: open, not blocked, not deferred. \
                 Quick view of actionable items sorted by priority. \
                 Used by: project_overview returns the same data. Use list_issues for \
                 filtered queries."
                    .into(),
            ),
            mime_type: Some("application/json".into()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        ensure_not_shutting_down()?;

        let storage = self.0.open_read_storage().map_err(to_mcp)?;
        let ready = mcp_ready_issues(&self.0, &storage)?;

        Ok(resource_json(
            "beads://issues/ready",
            &json!({
                "count": ready.len(),
                "issues": ready.iter().map(|issue| {
                    json!({
                        "id": issue.id,
                        "title": issue.title,
                        "priority": issue.priority,
                        "type": issue.issue_type,
                    })
                }).collect::<Vec<_>>(),
            }),
        ))
    }
}

// ---------------------------------------------------------------------------
// 6. issues/blocked — blocked work items
// ---------------------------------------------------------------------------

pub struct BlockedIssuesResource(Arc<BeadsState>);
impl BlockedIssuesResource {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl ResourceHandler for BlockedIssuesResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "beads://issues/blocked".into(),
            name: "Blocked Issues".into(),
            description: Some(
                "Issues that are blocked by other issues. Shows what's stuck and \
                 which issues are blocking progress. \
                 Used by: manage_dependencies can unblock issues. Use show_issue on \
                 blockers to investigate."
                    .into(),
            ),
            mime_type: Some("application/json".into()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        ensure_not_shutting_down()?;

        cached_resource_json(
            &self.0,
            "beads://issues/blocked",
            "resource:issues_blocked".to_string(),
            |storage| {
                let blocked = storage.get_blocked_issues().map_err(to_mcp)?;

                Ok(json!({
                    "count": blocked.len(),
                    "issues": blocked.iter().map(|(issue, blockers)| {
                        json!({
                            "id": issue.id,
                            "title": issue.title,
                            "blocked_by": blockers,
                        })
                    }).collect::<Vec<_>>(),
                }))
            },
        )
    }
}

// ---------------------------------------------------------------------------
// 7. issues/in_progress — work currently being done
// ---------------------------------------------------------------------------

pub struct InProgressResource(Arc<BeadsState>);
impl InProgressResource {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl ResourceHandler for InProgressResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "beads://issues/in_progress".into(),
            name: "In-Progress Issues".into(),
            description: Some(
                "Issues currently being worked on (status: in_progress). \
                 Shows who is working on what with priorities. \
                 Used by: update_issue to change assignee/status. Use show_issue for \
                 full details."
                    .into(),
            ),
            mime_type: Some("application/json".into()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        ensure_not_shutting_down()?;

        cached_resource_json(
            &self.0,
            "beads://issues/in_progress",
            "resource:issues_in_progress".to_string(),
            |storage| {
                let filters = ListFilters {
                    statuses: Some(vec![Status::InProgress]),
                    include_closed: false,
                    limit: Some(50),
                    ..ListFilters::default()
                };
                let issues = storage.list_issues(&filters).map_err(to_mcp)?;

                Ok(json!({
                    "count": issues.len(),
                    "issues": issues.iter().map(|issue| {
                        json!({
                            "id": issue.id,
                            "title": issue.title,
                            "priority": issue.priority,
                            "type": issue.issue_type,
                            "assignee": issue.assignee,
                        })
                    }).collect::<Vec<_>>(),
                }))
            },
        )
    }
}

// ---------------------------------------------------------------------------
// 8. coordination/status — hidden in-progress claim diagnosis
// ---------------------------------------------------------------------------

pub struct CoordinationStatusResource(Arc<BeadsState>);
impl CoordinationStatusResource {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl ResourceHandler for CoordinationStatusResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: COORDINATION_STATUS_URI.into(),
            name: "Coordination Status".into(),
            description: Some(
                "Read-only stale-claim diagnosis for in-progress work. Mirrors \
                 `br coordination status --json` with the br.coordination.v1 \
                 evidence shape, without network listeners, background daemons, \
                 or direct Agent Mail calls. Use the CLI snapshot flags when \
                 reservation or agent-liveness evidence is required."
                    .into(),
            ),
            mime_type: Some("application/json".into()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        ensure_not_shutting_down()?;

        let storage = self
            .0
            .open_read_storage()
            .map_err(|err| coordination_status_error(err.to_string()))?;
        let value = coordination_status_resource_json(&storage)?;
        Ok(resource_json(COORDINATION_STATUS_URI, &value))
    }
}

// ---------------------------------------------------------------------------
// 9. events/recent — recent audit events
// ---------------------------------------------------------------------------

pub struct EventsResource(Arc<BeadsState>);
impl EventsResource {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl ResourceHandler for EventsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "beads://events/recent".into(),
            name: "Recent Activity".into(),
            description: Some(
                "Recent audit events across all issues: status changes, field updates, \
                 comments added. Shows the 50 most recent events. \
                 Used by: Helpful for understanding what changed recently. \
                 Use show_issue for events on a specific issue."
                    .into(),
            ),
            mime_type: Some("application/json".into()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        ensure_not_shutting_down()?;

        cached_resource_json(
            &self.0,
            "beads://events/recent",
            "resource:events_recent".to_string(),
            |storage| {
                let events = storage.get_all_events(50).map_err(to_mcp)?;

                Ok(json!({
                    "count": events.len(),
                    "events": events.iter().map(|e: &Event| {
                        json!({
                            "issue_id": e.issue_id,
                            "event_type": e.event_type,
                            "actor": e.actor,
                            "old_value": e.old_value,
                            "new_value": e.new_value,
                            "created_at": e.created_at,
                            // Tier 1 attribution (issue #312, Layer 3
                            // capture-only); null when not supplied.
                            "agent_name": e.agent_name,
                            "harness": e.harness,
                            "model": e.model,
                        })
                    }).collect::<Vec<_>>(),
                }))
            },
        )
    }
}

// ---------------------------------------------------------------------------
// 10. issues/deferred — deferred work items
// ---------------------------------------------------------------------------

pub struct DeferredIssuesResource(Arc<BeadsState>);
impl DeferredIssuesResource {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl ResourceHandler for DeferredIssuesResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "beads://issues/deferred".into(),
            name: "Deferred Issues".into(),
            description: Some(
                "Issues that have been deferred (status: deferred, often with defer_until). \
                 Useful for triage — review what has been postponed and whether it should be revisited. \
                 Used by: update_issue to change status. Use show_issue for full details."
                    .into(),
            ),
            mime_type: Some("application/json".into()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        ensure_not_shutting_down()?;

        cached_resource_json(
            &self.0,
            "beads://issues/deferred",
            "resource:issues_deferred".to_string(),
            |storage| {
                let filters = ListFilters {
                    statuses: Some(vec![Status::Deferred]),
                    include_deferred: true,
                    limit: Some(50),
                    ..ListFilters::default()
                };
                let issues = storage.list_issues(&filters).map_err(to_mcp)?;

                Ok(json!({
                    "count": issues.len(),
                    "issues": issues.iter().map(|issue| {
                        json!({
                            "id": issue.id,
                            "title": issue.title,
                            "priority": issue.priority,
                            "type": issue.issue_type,
                            "defer_until": issue.defer_until,
                        })
                    }).collect::<Vec<_>>(),
                }))
            },
        )
    }
}

// ---------------------------------------------------------------------------
// 11. graph/health — dependency graph health metrics (bv-inspired)
// ---------------------------------------------------------------------------

/// Compute the longest path length in the "blocks" DAG from a given node.
/// Uses a `visiting` set to detect cycles and avoid infinite recursion.
fn longest_chain_from(
    node: &str,
    edges: &HashMap<String, Vec<String>>,
    cache: &mut HashMap<String, usize>,
    visiting: &mut HashSet<String>,
) -> usize {
    if let Some(&cached) = cache.get(node) {
        return cached;
    }
    // Cycle detection: if we're already visiting this node, stop.
    if !visiting.insert(node.to_string()) {
        return 0;
    }
    let depth = edges.get(node).map_or(0, |children| {
        children
            .iter()
            .map(|c| 1 + longest_chain_from(c, edges, cache, visiting))
            .max()
            .unwrap_or(0)
    });
    visiting.remove(node);
    cache.insert(node.to_string(), depth);
    depth
}

fn graph_has_cycle(edges: &HashMap<String, Vec<String>>) -> bool {
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();

    for start in edges.keys().map(String::as_str) {
        if visited.contains(start) {
            continue;
        }

        visiting.insert(start);
        let mut stack = vec![(start, 0usize)];
        while let Some((node, child_idx)) = stack.pop() {
            let Some(children) = edges.get(node) else {
                visiting.remove(node);
                visited.insert(node);
                continue;
            };

            if child_idx >= children.len() {
                visiting.remove(node);
                visited.insert(node);
                continue;
            }

            stack.push((node, child_idx + 1));
            let child = children[child_idx].as_str();
            if visited.contains(child) {
                continue;
            }
            if visiting.contains(child) {
                return true;
            }

            visiting.insert(child);
            stack.push((child, 0));
        }
    }

    false
}

/// Compute graph health metrics from the dependency edges.
fn compute_graph_health(storage: &SqliteStorage) -> McpResult<serde_json::Value> {
    let all_edges = storage.get_blocks_dep_edges().map_err(to_mcp)?;
    let open_filters = ListFilters {
        include_closed: false,
        limit: Some(10_000),
        ..ListFilters::default()
    };
    let open_issues = storage.list_issues(&open_filters).map_err(to_mcp)?;
    let open_ids: std::collections::HashSet<&str> =
        open_issues.iter().map(|i| i.id.as_str()).collect();

    // Filter edges to only open→open relationships
    let open_edges: Vec<(String, String)> = all_edges
        .into_iter()
        .filter(|(from, to)| open_ids.contains(from.as_str()) && open_ids.contains(to.as_str()))
        .collect();

    let edge_count = open_edges.len();
    let node_count = open_ids.len();
    let density = if node_count > 1 {
        #[allow(clippy::cast_precision_loss)]
        let d = edge_count as f64 / (node_count as f64 * (node_count as f64 - 1.0));
        (d * 1000.0).round() / 1000.0
    } else {
        0.0
    };

    // Build adjacency list for chain depth computation
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for (from, to) in &open_edges {
        adj.entry(from.clone()).or_default().push(to.clone());
    }

    // Longest chain depth
    let mut depth_cache: HashMap<String, usize> = HashMap::new();
    let mut visiting: HashSet<String> = HashSet::new();
    let max_chain_depth = open_ids
        .iter()
        .map(|id| longest_chain_from(id, &adj, &mut depth_cache, &mut visiting))
        .max()
        .unwrap_or(0);

    // High-fan-out issues (block 3+ others)
    let high_fan_out: Vec<_> = adj
        .iter()
        .filter(|(_, targets)| targets.len() >= 3)
        .map(|(id, targets)| json!({"id": id, "blocks_count": targets.len()}))
        .collect();

    // Stale issues (not updated in 30+ days)
    let thirty_days_ago = chrono::Utc::now() - chrono::Duration::days(30);
    let stale_filters = ListFilters {
        include_closed: false,
        updated_before: Some(thirty_days_ago),
        limit: Some(100),
        ..ListFilters::default()
    };
    let stale_issues = storage.list_issues(&stale_filters).map_err(to_mcp)?;

    let has_cycles = graph_has_cycle(&adj);

    Ok(json!({
        "open_issue_count": node_count,
        "dependency_edge_count": edge_count,
        "density": density,
        "density_interpretation": if density > 0.5 {
            "Very high — issues are heavily coupled, hard to parallelize"
        } else if density > 0.2 {
            "Moderate — some coupling, review if all deps are necessary"
        } else if density > 0.0 {
            "Healthy — dependencies are focused"
        } else {
            "No dependencies — issues are fully independent"
        },
        "max_chain_depth": max_chain_depth,
        "max_chain_interpretation": if max_chain_depth > 5 {
            "Deep chain — critical path is long, hard to parallelize"
        } else if max_chain_depth > 2 {
            "Moderate chain depth"
        } else {
            "Shallow — good parallelization potential"
        },
        "high_fan_out_issues": high_fan_out,
        "cycle_detected": has_cycles,
        "stale_issue_count": stale_issues.len(),
        "stale_threshold_days": 30,
    }))
}

pub struct GraphHealthResource(Arc<BeadsState>);
impl GraphHealthResource {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl ResourceHandler for GraphHealthResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "beads://graph/health".into(),
            name: "Dependency Graph Health".into(),
            description: Some(
                "Graph-level health metrics for the dependency network: density, \
                 chain depth, fan-out hotspots, stale issues, cycle detection. \
                 Inspired by bv's graph analysis. Read this to understand project \
                 structure and identify bottlenecks. \
                 Used by: plan_next_work and triage prompts for context."
                    .into(),
            ),
            mime_type: Some("application/json".into()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        ensure_not_shutting_down()?;

        cached_resource_json(
            &self.0,
            "beads://graph/health",
            "resource:graph_health".to_string(),
            compute_graph_health,
        )
    }
}

// ---------------------------------------------------------------------------
// 12. issues/bottlenecks — highest-impact blockers (bv-inspired)
// ---------------------------------------------------------------------------

/// Compute bottleneck issues: those that block the most other open issues.
/// This is a practical approximation of PageRank/betweenness from bv.
fn compute_bottlenecks(storage: &SqliteStorage) -> McpResult<serde_json::Value> {
    let edges = storage.get_blocks_dep_edges().map_err(to_mcp)?;
    let open_filters = ListFilters {
        include_closed: false,
        limit: Some(10_000),
        ..ListFilters::default()
    };
    let open_issues = storage.list_issues(&open_filters).map_err(to_mcp)?;
    let open_map: HashMap<&str, &Issue> = open_issues.iter().map(|i| (i.id.as_str(), i)).collect();

    // Count how many open issues each open issue blocks
    let mut blocks_count: HashMap<&str, usize> = HashMap::new();
    for (blocked, blocker) in &edges {
        if open_map.contains_key(blocker.as_str()) && open_map.contains_key(blocked.as_str()) {
            *blocks_count.entry(blocker.as_str()).or_default() += 1;
        }
    }

    // Sort by blocks_count descending
    let mut ranked: Vec<_> = blocks_count.into_iter().collect();
    ranked.sort_by_key(|b| std::cmp::Reverse(b.1));

    let bottlenecks: Vec<_> = ranked
        .iter()
        .take(15)
        .filter_map(|(id, count)| {
            open_map.get(id).map(|issue| {
                json!({
                    "id": issue.id,
                    "title": issue.title,
                    "priority": issue.priority,
                    "status": issue.status,
                    "blocks_count": count,
                    "interpretation": if *count >= 5 {
                        "Critical bottleneck — blocks many issues, prioritize resolving"
                    } else if *count >= 3 {
                        "Significant blocker — resolve to unblock multiple work streams"
                    } else {
                        "Blocker — has downstream impact"
                    }
                })
            })
        })
        .collect();

    Ok(json!({
        "count": bottlenecks.len(),
        "issues": bottlenecks,
        "analysis_hint": "Issues sorted by how many other open issues they block. \
            High blocks_count = high PageRank equivalent. Resolve these first to \
            maximize unblocked work.",
    }))
}

pub struct BottlenecksResource(Arc<BeadsState>);
impl BottlenecksResource {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl ResourceHandler for BottlenecksResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "beads://issues/bottlenecks".into(),
            name: "Bottleneck Issues".into(),
            description: Some(
                "Issues that block the most other work, sorted by impact. \
                 Equivalent to bv's PageRank-based prioritization. Payload entries \
                 include blocks_count so agents can rank downstream impact. Resolve these \
                 Used by: plan_next_work prompt uses this data for recommendations."
                    .into(),
            ),
            mime_type: Some("application/json".into()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        ensure_not_shutting_down()?;

        cached_resource_json(
            &self.0,
            "beads://issues/bottlenecks",
            "resource:issues_bottlenecks".to_string(),
            compute_bottlenecks,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Arc;

    use chrono::{Duration, TimeZone, Utc};
    use fastmcp_rust::{Cx, McpContext, Resource, ResourceContent, ResourceHandler};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::{
        BlockedIssuesResource, BottlenecksResource, COORDINATION_STATUS_URI,
        CoordinationStatusResource, DeferredIssuesResource, EventsResource, GraphHealthResource,
        InProgressResource, IssueResource, LabelsResource, ProjectInfoResource,
        ReadyIssuesResource, SchemaResource, coordination_status_resource_json_at, graph_has_cycle,
        issue_not_found_resource, issue_resource_json, read_project_config,
    };
    use crate::cli::commands::coordination::build_coordination_status_without_snapshots;
    use crate::coordination::{COORDINATION_SCHEMA_VERSION, ClaimOwnerKind};
    use crate::mcp::{BeadsState, McpReadSnapshotCache};
    use crate::model::{Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;

    fn mcp_resource_state(temp: &TempDir, read_snapshot: bool) -> Arc<BeadsState> {
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        let db_path = beads_dir.join("beads.db");
        SqliteStorage::open(&db_path).expect("initialize storage");
        Arc::new(BeadsState {
            db_path,
            beads_dir: beads_dir.clone(),
            jsonl_path: beads_dir.join("issues.jsonl"),
            // Robust under heavy parallel-test load (a concurrent auto-flush can
            // hold .write.lock for >25ms); no test asserts the timeout path.
            write_lock_timeout_ms: Some(5_000),
            allow_external_jsonl: false,
            actor: "mcp-resource-test".to_string(),
            issue_prefix: Some("br".to_string()),
            read_snapshot_cache: read_snapshot
                .then(|| std::sync::Mutex::new(McpReadSnapshotCache::default())),
        })
    }

    fn insert_resource_issue(state: &BeadsState, id: &str, title: &str) {
        insert_resource_issue_with_status(state, id, title, Status::Open, None, Utc::now());
    }

    fn insert_resource_issue_with_status(
        state: &BeadsState,
        id: &str,
        title: &str,
        status: Status,
        assignee: Option<&str>,
        updated_at: chrono::DateTime<Utc>,
    ) {
        let mut storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let issue = Issue {
            id: id.to_string(),
            title: title.to_string(),
            status,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: assignee.map(str::to_string),
            created_at: updated_at,
            updated_at,
            ..Issue::default()
        };
        storage
            .create_issue(&issue, "mcp-resource-test")
            .expect("create issue");
    }

    fn fixed_now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 8, 12, 0, 0)
            .single()
            .expect("valid timestamp")
    }

    fn resource_text_json(contents: &[ResourceContent]) -> Value {
        let [content] = contents else {
            return json!({"unexpected_resource_count": contents.len()});
        };
        let Some(text) = content.text.as_deref() else {
            return json!({"missing_text": true});
        };
        serde_json::from_str(text)
            .unwrap_or_else(|err| json!({"parse_error": err.to_string(), "text": text}))
    }

    fn assert_docs_list_resource(uri: &str) {
        for (name, document) in [
            ("README.md", include_str!("../../README.md")),
            (
                "docs/AGENT_INTEGRATION.md",
                include_str!("../../docs/AGENT_INTEGRATION.md"),
            ),
            (
                "docs/CLI_REFERENCE.md",
                include_str!("../../docs/CLI_REFERENCE.md"),
            ),
        ] {
            assert!(
                document.contains(uri),
                "{name} must document MCP resource {uri}"
            );
        }
    }

    fn assert_description_mentions(description: &str, expected_terms: &[&str]) {
        for term in expected_terms {
            assert!(
                description.contains(term),
                "MCP description must mention {term:?}: {description}"
            );
        }
    }

    fn assert_description_does_not_claim_live_services(description: &str) {
        let lower = description.to_ascii_lowercase();
        for forbidden in [
            "http://",
            "https://",
            "network port",
            "network socket",
            "network service",
        ] {
            assert!(
                !lower.contains(forbidden),
                "MCP description implies a live service boundary via {forbidden:?}: {description}"
            );
        }
        if lower.contains("agent mail") {
            assert!(
                lower.contains("without")
                    || lower.contains("does not call")
                    || lower.contains("cli snapshot"),
                "Agent Mail mention must be an explicit non-live boundary: {description}"
            );
        }
        if lower.contains("network listener") {
            assert!(
                lower.contains("without network listener")
                    || lower.contains("no network listener")
                    || lower.contains("does not listen"),
                "network listener mention must be an explicit non-live boundary: {description}"
            );
        }
    }

    fn assert_resource_definition(
        definition: &Resource,
        expected_uri: &str,
        expected_terms: &[&str],
    ) {
        assert_eq!(definition.uri, expected_uri);
        assert_eq!(definition.mime_type.as_deref(), Some("application/json"));
        let description = definition
            .description
            .as_deref()
            .expect("MCP resource descriptions are part of the agent contract");
        assert!(!description.trim().is_empty());
        assert_description_mentions(description, expected_terms);
        assert_description_does_not_claim_live_services(description);
        assert_docs_list_resource(expected_uri);
    }

    fn edge_map(edges: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        edges
            .iter()
            .map(|(from, targets)| {
                (
                    (*from).to_string(),
                    targets.iter().map(|target| (*target).to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn mcp_contract_resource_metadata_matches_docs_and_local_boundary() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_resource_state(&temp, false);
        let definitions: [(Resource, &str, &[&str]); 12] = [
            (
                ProjectInfoResource::new(Arc::clone(&state)).definition(),
                "beads://project/info",
                &["Workspace metadata", "project_overview"][..],
            ),
            (
                IssueResource::new(Arc::clone(&state)).definition(),
                "beads://issues/{id}",
                &["Full issue details", "list_issues", "show_issue"],
            ),
            (
                SchemaResource.definition(),
                "beads://schema",
                &["issue fields", "valid statuses", "dependency types"],
            ),
            (
                LabelsResource::new(Arc::clone(&state)).definition(),
                "beads://labels",
                &["label values", "list_issues", "update_issue"],
            ),
            (
                ReadyIssuesResource::new(Arc::clone(&state)).definition(),
                "beads://issues/ready",
                &["ready for work", "project_overview", "list_issues"],
            ),
            (
                BlockedIssuesResource::new(Arc::clone(&state)).definition(),
                "beads://issues/blocked",
                &["blocked", "dependencies"],
            ),
            (
                InProgressResource::new(Arc::clone(&state)).definition(),
                "beads://issues/in_progress",
                &["in_progress", "assignee"],
            ),
            (
                CoordinationStatusResource::new(Arc::clone(&state)).definition(),
                COORDINATION_STATUS_URI,
                &[
                    "br coordination status --json",
                    "br.coordination.v1",
                    "without network listeners",
                    "direct Agent Mail calls",
                    "CLI snapshot flags",
                ],
            ),
            (
                EventsResource::new(Arc::clone(&state)).definition(),
                "beads://events/recent",
                &["audit events", "show_issue"],
            ),
            (
                DeferredIssuesResource::new(Arc::clone(&state)).definition(),
                "beads://issues/deferred",
                &["deferred", "defer_until"],
            ),
            (
                BottlenecksResource::new(Arc::clone(&state)).definition(),
                "beads://issues/bottlenecks",
                &["block the most", "blocks_count"],
            ),
            (
                GraphHealthResource::new(state).definition(),
                "beads://graph/health",
                &["Graph-level health", "cycle"],
            ),
        ];

        for (definition, uri, expected_terms) in definitions {
            assert_resource_definition(&definition, uri, expected_terms);
        }
    }

    #[test]
    fn mcp_contract_core_resource_payloads_match_documented_shapes() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_resource_state(&temp, false);
        insert_resource_issue(&state, "br-mcp-contract-ready", "contract ready issue");
        let ctx = McpContext::new(Cx::for_testing(), 1);

        let project_info = resource_text_json(
            &ProjectInfoResource::new(Arc::clone(&state))
                .read(&ctx)
                .expect("read project info"),
        );
        assert_eq!(project_info["issue_prefix"], "br");
        assert_eq!(project_info["actor"], "mcp-resource-test");
        assert!(project_info["beads_dir"].as_str().is_some_and(|path| {
            std::path::Path::new(path)
                .file_name()
                .is_some_and(|name| name == ".beads")
        }));
        assert!(project_info["config"].is_object());

        let schema = resource_text_json(&SchemaResource.read(&ctx).expect("read schema"));
        assert!(
            schema["statuses"]["values"]
                .as_array()
                .is_some_and(|values| values.iter().any(|value| value == "in_progress"))
        );
        assert!(
            schema["priorities"]["values"]
                .as_array()
                .is_some_and(|values| values.iter().any(|value| value == "critical"))
        );
        assert!(
            schema["issue_types"]["values"]
                .as_array()
                .is_some_and(|values| values.iter().any(|value| value == "task"))
        );
        assert!(
            schema["dependency_types"]
                .as_array()
                .is_some_and(|values| values.iter().any(|value| value == "blocks"))
        );
        assert!(schema["issue_fields"]["id"].is_string());
        assert!(schema["bead_anatomy"]["sections"].is_object());

        let ready = resource_text_json(
            &ReadyIssuesResource::new(Arc::clone(&state))
                .read(&ctx)
                .expect("read ready issues"),
        );
        assert_eq!(ready["count"].as_u64(), Some(1));
        assert_eq!(ready["issues"][0]["id"], "br-mcp-contract-ready");
        assert_eq!(ready["issues"][0]["title"], "contract ready issue");
        assert_eq!(ready["issues"][0]["type"], "task");

        let coordination = resource_text_json(
            &CoordinationStatusResource::new(state)
                .read(&ctx)
                .expect("read coordination status"),
        );
        assert_eq!(coordination["schema_version"], COORDINATION_SCHEMA_VERSION);
        assert_eq!(coordination["summary"]["total_claims"].as_u64(), Some(0));
        assert!(coordination["claims"].as_array().is_some());
    }

    #[test]
    fn ready_resource_snapshot_matches_direct_json_and_invalidates() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_resource_state(&temp, true);
        insert_resource_issue(&state, "br-ready-resource-1", "ready resource first issue");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let resource = ReadyIssuesResource::new(Arc::clone(&state));

        let first_content = resource.read(&ctx).expect("read ready resource");
        let first = resource_text_json(&first_content);
        assert_eq!(first["count"].as_u64(), Some(1));

        insert_resource_issue(&state, "br-ready-resource-2", "ready resource second issue");
        fs::write(&state.jsonl_path, "{\"id\":\"br-ready-resource-2\"}\n")
            .expect("update jsonl witness");

        let second_content = resource
            .read(&ctx)
            .expect("read ready resource after witness mismatch");
        let second = resource_text_json(&second_content);
        assert_eq!(second["count"].as_u64(), Some(2));
    }

    #[test]
    fn ready_resource_excludes_unsatisfied_external_blockers() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_resource_state(&temp, true);
        insert_resource_issue(
            &state,
            "br-ready-external-blocked",
            "externally blocked ready candidate",
        );
        insert_resource_issue(&state, "br-ready-local", "local ready candidate");
        let mut storage = SqliteStorage::open(&state.db_path).expect("open storage");
        storage
            .add_dependency(
                "br-ready-external-blocked",
                "external:missing:capability",
                "blocks",
                "mcp-resource-test",
            )
            .expect("add external dependency");
        drop(storage);

        let ctx = McpContext::new(Cx::for_testing(), 1);
        let resource = ReadyIssuesResource::new(Arc::clone(&state));
        let content = resource.read(&ctx).expect("read ready resource");
        let ready = resource_text_json(&content);

        assert_eq!(ready["count"].as_u64(), Some(1));
        assert_eq!(ready["issues"][0]["id"], "br-ready-local");
    }

    #[test]
    fn issue_resource_snapshot_matches_direct_json() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_resource_state(&temp, true);
        insert_resource_issue(&state, "br-resource-issue", "resource issue details");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let resource = IssueResource::new(Arc::clone(&state));
        let params = HashMap::from([("id".to_string(), "br-resource-issue".to_string())]);

        let content = resource
            .read_with_uri(&ctx, "beads://issues/br-resource-issue", &params)
            .expect("read issue resource");
        let cached = resource_text_json(&content);
        let direct = {
            let storage = SqliteStorage::open(&state.db_path).expect("open storage");
            issue_resource_json(&storage, "br-resource-issue").expect("direct issue resource")
        };

        assert_eq!(cached, direct);
    }

    #[test]
    fn coordination_status_resource_matches_cli_builder_for_claims() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_resource_state(&temp, false);
        let now = fixed_now();
        insert_resource_issue_with_status(
            &state,
            "br-mcp-claim",
            "resource claim details",
            Status::InProgress,
            Some("TopazFox"),
            now - Duration::minutes(30),
        );

        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let resource_value =
            coordination_status_resource_json_at(&storage, now).expect("resource status");
        let cli_value = serde_json::to_value(
            build_coordination_status_without_snapshots(
                &storage,
                ClaimOwnerKind::SwarmAgent,
                2,
                now,
            )
            .expect("cli status"),
        )
        .expect("serialize cli status");

        assert_eq!(resource_value, cli_value);
        assert_eq!(
            resource_value["schema_version"],
            COORDINATION_SCHEMA_VERSION
        );
        assert_eq!(resource_value["summary"]["total_claims"].as_u64(), Some(1));
        assert_eq!(
            resource_value["claims"][0]["assessment"]["classification"],
            "fresh"
        );
    }

    #[test]
    fn coordination_status_resource_returns_empty_claim_set() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_resource_state(&temp, false);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let resource = CoordinationStatusResource::new(Arc::clone(&state));

        let content = resource.read(&ctx).expect("read coordination resource");
        let value = resource_text_json(&content);

        assert_eq!(content[0].uri, COORDINATION_STATUS_URI);
        assert_eq!(value["schema_version"], COORDINATION_SCHEMA_VERSION);
        assert_eq!(value["summary"]["total_claims"].as_u64(), Some(0));
        assert_eq!(value["claims"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn coordination_status_resource_returns_structured_errors() {
        let storage = SqliteStorage::open_memory().expect("storage");
        storage
            .execute_raw("DROP TABLE issues")
            .expect("drop issues table");

        let err = coordination_status_resource_json_at(&storage, fixed_now())
            .expect_err("coordination storage failure must be structured");
        let data = err.data.expect("structured MCP error data");

        assert_eq!(data["error_type"], "COORDINATION_STATUS_FAILED");
        assert_eq!(data["resource"], COORDINATION_STATUS_URI);
        assert!(data["suggested_tool_calls"].is_array());
        assert!(data["suggested_cli_commands"].is_array());
    }

    #[test]
    fn issue_not_found_resource_surfaces_id_scan_failure() {
        let storage = SqliteStorage::open_memory().expect("storage");
        storage
            .execute_raw("DROP TABLE issues")
            .expect("drop issues table");

        let err = issue_not_found_resource(&storage, "bd-missing")
            .expect_err("ID scan failure must be returned to MCP clients");

        assert!(
            err.to_string().contains("issues") || err.to_string().contains("no such table"),
            "unexpected MCP error: {err}"
        );
    }

    #[test]
    fn read_project_config_surfaces_storage_failure() {
        let storage = SqliteStorage::open_memory().expect("storage");
        storage
            .execute_raw("DROP TABLE config")
            .expect("drop config table");

        let err = read_project_config(&storage)
            .expect_err("config read failure must be returned to MCP clients");

        assert!(
            err.to_string().contains("config") || err.to_string().contains("no such table"),
            "unexpected MCP error: {err}"
        );
    }

    #[test]
    fn graph_has_cycle_detects_three_node_cycle() {
        let edges = edge_map(&[
            ("br-a", &["br-b"]),
            ("br-b", &["br-c"]),
            ("br-c", &["br-a"]),
        ]);

        assert!(graph_has_cycle(&edges));
    }

    #[test]
    fn graph_has_cycle_ignores_acyclic_graph() {
        let edges = edge_map(&[
            ("br-a", &["br-b", "br-c"]),
            ("br-b", &["br-d"]),
            ("br-c", &[]),
        ]);

        assert!(!graph_has_cycle(&edges));
    }
}
