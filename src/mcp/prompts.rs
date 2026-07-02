//! MCP prompt handlers for the beads issue tracker.
//!
//! Prompts provide guided workflows that pre-fetch project data and return
//! structured messages to help agents perform common tasks.

use std::collections::HashMap;
use std::sync::Arc;

use fastmcp_rust::{
    Content, McpContext, McpResult, Prompt, PromptArgument, PromptHandler, PromptMessage, Role,
};
use serde_json::json;

use std::collections::HashSet;

use crate::model::{Issue, IssueType, Status};
use crate::storage::{ListFilters, SqliteStorage};

use super::{BeadsState, ensure_not_shutting_down, mcp_ready_issues, to_mcp};

// ---------------------------------------------------------------------------
// Display limits — extracted from magic numbers for maintainability
// ---------------------------------------------------------------------------

const UNASSIGNED_FETCH_LIMIT: usize = 50;
const UNASSIGNED_DISPLAY_LIMIT: usize = 15;
const READY_DISPLAY_LIMIT: usize = 10;
const DEFERRED_FETCH_LIMIT: usize = 50;
const DEFERRED_DISPLAY_LIMIT: usize = 15;
const STATUS_REPORT_IN_PROGRESS_LIMIT: usize = 20;
const STATUS_REPORT_BLOCKED_LIMIT: usize = 10;
const STATUS_REPORT_LABELS_LIMIT: usize = 10;
const BOTTLENECK_FETCH_LIMIT: usize = 10_000;
const BOTTLENECK_DISPLAY_LIMIT: usize = 10;
const QUICK_WINS_DISPLAY_LIMIT: usize = 5;
const COMPLETENESS_FETCH_LIMIT: usize = 100;
const QUALITY_SAMPLE_LIMIT: usize = 10;
const ORPHAN_DISPLAY_LIMIT: usize = 15;
const DEPENDENCY_HEALTH_FETCH_LIMIT: usize = 500;

/// Validate a prompt argument against known values. Returns the validated value
/// and an optional warning if the input was unrecognized and defaulted.
fn validate_prompt_arg<'a>(
    value: &'a str,
    valid_options: &[&str],
    default: &'a str,
    arg_name: &str,
) -> (&'a str, Option<String>) {
    if valid_options.contains(&value) {
        (value, None)
    } else {
        (
            default,
            Some(format!(
                "Unrecognized {arg_name} '{value}'. Valid options: {}. Defaulting to '{default}'.",
                valid_options.join(", ")
            )),
        )
    }
}

/// Gather blocked-issues context as a formatted string.
fn blocked_context(storage: &SqliteStorage) -> McpResult<String> {
    let blocked = storage.get_blocked_issues().map_err(to_mcp)?;
    if blocked.is_empty() {
        return Ok("No blocked issues.".into());
    }
    let blocked_json: Vec<_> = blocked
        .iter()
        .map(|(issue, blockers)| {
            json!({
                "id": issue.id,
                "title": issue.title,
                "priority": issue.priority,
                "blocked_by": blockers,
            })
        })
        .collect();
    Ok(format!(
        "Blocked issues ({}):\n{}",
        blocked.len(),
        serde_json::to_string_pretty(&blocked_json).unwrap_or_default()
    ))
}

/// Gather unassigned-issues context as a formatted string.
fn unassigned_context(storage: &SqliteStorage) -> McpResult<String> {
    let filters = ListFilters {
        include_closed: false,
        unassigned: true,
        limit: Some(UNASSIGNED_FETCH_LIMIT),
        ..ListFilters::default()
    };
    let unassigned = storage.list_issues(&filters).map_err(to_mcp)?;
    if unassigned.is_empty() {
        return Ok("All open issues are assigned.".into());
    }
    let unassigned_json: Vec<_> = unassigned
        .iter()
        .take(UNASSIGNED_DISPLAY_LIMIT)
        .map(|issue| {
            json!({
                "id": issue.id,
                "title": issue.title,
                "priority": issue.priority,
                "type": issue.issue_type,
            })
        })
        .collect();
    Ok(format!(
        "Unassigned open issues ({}):\n{}",
        unassigned.len(),
        serde_json::to_string_pretty(&unassigned_json).unwrap_or_default()
    ))
}

/// Gather ready-issues context as a formatted string.
fn ready_context(state: &BeadsState, storage: &SqliteStorage) -> McpResult<String> {
    let ready = mcp_ready_issues(state, storage)?;
    if ready.is_empty() {
        return Ok("No ready issues.".into());
    }
    let ready_json: Vec<_> = ready
        .iter()
        .take(READY_DISPLAY_LIMIT)
        .map(|issue| {
            json!({
                "id": issue.id,
                "title": issue.title,
                "priority": issue.priority,
                "type": issue.issue_type,
            })
        })
        .collect();
    Ok(format!(
        "Ready for work ({} total, showing top {READY_DISPLAY_LIMIT}):\n{}",
        ready.len(),
        serde_json::to_string_pretty(&ready_json).unwrap_or_default()
    ))
}

/// Gather deferred-issues context as a formatted string.
fn deferred_context(storage: &SqliteStorage) -> McpResult<String> {
    let filters = ListFilters {
        statuses: Some(vec![Status::Deferred]),
        include_closed: false,
        include_deferred: true,
        limit: Some(DEFERRED_FETCH_LIMIT),
        ..ListFilters::default()
    };
    let deferred = storage.list_issues(&filters).map_err(to_mcp)?;
    if deferred.is_empty() {
        return Ok("No deferred issues.".into());
    }
    let deferred_json: Vec<_> = deferred
        .iter()
        .take(DEFERRED_DISPLAY_LIMIT)
        .map(|issue| {
            json!({
                "id": issue.id,
                "title": issue.title,
                "priority": issue.priority,
                "defer_until": issue.defer_until,
            })
        })
        .collect();
    Ok(format!(
        "Deferred issues ({}):\n{}",
        deferred.len(),
        serde_json::to_string_pretty(&deferred_json).unwrap_or_default()
    ))
}

/// Return the triage instruction text for a given focus area.
fn triage_instruction(focus: &str) -> &'static str {
    match focus {
        "blocked" => {
            "Review the blocked issues above. For each one:\n\
             1. Check what's blocking it (use show_issue on the blockers)\n\
             2. Determine if the blocker can be resolved or if the dependency should be removed\n\
             3. Suggest concrete next steps to unblock progress"
        }
        "unassigned" => {
            "Review the unassigned issues above. For each one:\n\
             1. Assess priority and urgency\n\
             2. Suggest who should own it or if it should be deferred/closed\n\
             3. Flag any that are duplicates or no longer relevant"
        }
        "deferred" => {
            "Review the deferred issues above. For each one:\n\
             1. Check if the defer_until date has passed or is approaching\n\
             2. Determine if the issue should be re-activated, re-deferred, or closed\n\
             3. Flag any that are no longer relevant"
        }
        _ => {
            "Perform a full backlog triage:\n\
             1. Review blocked issues — can any be unblocked?\n\
             2. Review unassigned issues — who should own them?\n\
             3. Review deferred issues — should any be re-activated?\n\
             4. Review ready work — are priorities correct?\n\
             5. Identify any issues that should be closed, merged, or re-prioritized\n\
             6. Summarize your findings and recommended actions"
        }
    }
}

// ---------------------------------------------------------------------------
// 1. triage — guided backlog triage workflow
// ---------------------------------------------------------------------------

pub struct TriagePrompt(Arc<BeadsState>);
impl TriagePrompt {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl PromptHandler for TriagePrompt {
    fn definition(&self) -> Prompt {
        Prompt {
            name: "triage".into(),
            description: Some(
                "Guide an agent through backlog triage: review blocked items, \
                 unassigned work, and prioritization."
                    .into(),
            ),
            arguments: vec![PromptArgument {
                name: "focus".into(),
                description: Some(
                    "Focus area: 'blocked' (stuck items), 'unassigned' (needs owner), \
                     'deferred' (postponed items), or 'all' (full triage). Default: all"
                        .into(),
                ),
                required: false,
            }],
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn get(
        &self,
        _ctx: &McpContext,
        arguments: HashMap<String, String>,
    ) -> McpResult<Vec<PromptMessage>> {
        ensure_not_shutting_down()?;

        let raw_focus = arguments.get("focus").map_or("all", String::as_str);
        let (focus, focus_warning) = validate_prompt_arg(
            raw_focus,
            &["all", "blocked", "unassigned", "deferred"],
            "all",
            "focus",
        );

        let storage = self.0.open_read_storage().map_err(to_mcp)?;

        let mut parts: Vec<String> = Vec::new();

        if let Some(w) = focus_warning {
            parts.push(format!("Note: {w}"));
        }

        let total = storage.count_all_issues().map_err(to_mcp)?;
        let active = storage.count_active_issues().map_err(to_mcp)?;
        parts.push(format!(
            "Project has {total} total issues ({active} active/non-closed)."
        ));

        if focus == "all" || focus == "blocked" {
            parts.push(blocked_context(&storage)?);
        }
        if focus == "all" || focus == "unassigned" {
            parts.push(unassigned_context(&storage)?);
        }
        if focus == "all" || focus == "deferred" {
            parts.push(deferred_context(&storage)?);
        }
        if focus == "all" {
            parts.push(ready_context(&self.0, &storage)?);
        }

        Ok(vec![
            PromptMessage {
                role: Role::User,
                content: Content::text(format!(
                    "Here is the current state of the issue tracker:\n\n{}",
                    parts.join("\n\n")
                )),
            },
            PromptMessage {
                role: Role::User,
                content: Content::text(triage_instruction(focus).to_string()),
            },
        ])
    }
}

// ---------------------------------------------------------------------------
// 2. status_report — project status report generation
// ---------------------------------------------------------------------------

pub struct StatusReportPrompt(Arc<BeadsState>);
impl StatusReportPrompt {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl PromptHandler for StatusReportPrompt {
    fn definition(&self) -> Prompt {
        Prompt {
            name: "status_report".into(),
            description: Some(
                "Generate a project status report with counts, in-progress work, \
                 blockers, and recent activity."
                    .into(),
            ),
            arguments: vec![PromptArgument {
                name: "period".into(),
                description: Some(
                    "Report period: 'today', 'week', 'month', or 'all'. Default: all".into(),
                ),
                required: false,
            }],
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn get(
        &self,
        _ctx: &McpContext,
        arguments: HashMap<String, String>,
    ) -> McpResult<Vec<PromptMessage>> {
        ensure_not_shutting_down()?;

        let raw_period = arguments.get("period").map_or("all", String::as_str);
        let (period, period_warning) = validate_prompt_arg(
            raw_period,
            &["all", "today", "week", "month"],
            "all",
            "period",
        );

        let storage = self.0.open_read_storage().map_err(to_mcp)?;

        let total = storage.count_all_issues().map_err(to_mcp)?;
        let active = storage.count_active_issues().map_err(to_mcp)?;
        let labels = storage.get_unique_labels_with_counts().map_err(to_mcp)?;
        let blocked = storage.get_blocked_issues().map_err(to_mcp)?;
        let ready = mcp_ready_issues(&self.0, &storage)?;

        let in_progress_filters = ListFilters {
            statuses: Some(vec![Status::InProgress]),
            include_closed: false,
            limit: Some(50),
            ..ListFilters::default()
        };
        let in_progress = storage.list_issues(&in_progress_filters).map_err(to_mcp)?;

        let mut data = json!({
            "counts": {
                "total": total,
                "active": active,
                "blocked": blocked.len(),
                "ready": ready.len(),
                "in_progress": in_progress.len(),
            },
            "in_progress": in_progress.iter().take(STATUS_REPORT_IN_PROGRESS_LIMIT).map(|i| {
                json!({"id": i.id, "title": i.title, "priority": i.priority, "assignee": i.assignee})
            }).collect::<Vec<_>>(),
            "blocked": blocked.iter().take(STATUS_REPORT_BLOCKED_LIMIT).map(|(i, b)| {
                json!({"id": i.id, "title": i.title, "blocked_by": b})
            }).collect::<Vec<_>>(),
            "top_labels": labels.iter().take(STATUS_REPORT_LABELS_LIMIT).map(|(name, count)| {
                json!({"label": name, "count": count})
            }).collect::<Vec<_>>(),
        });

        if let Some(w) = period_warning {
            data["warning"] = json!(w);
        }

        let context_text = serde_json::to_string_pretty(&data).unwrap_or_default();

        let period_instruction = match period {
            "today" => "Focus on what changed today and what's immediately actionable.",
            "week" => "Summarize the week's progress, what was completed, and what's ahead.",
            "month" => "Provide a monthly overview of trends, velocity, and strategic priorities.",
            _ => "Provide a comprehensive status overview.",
        };

        Ok(vec![
            PromptMessage {
                role: Role::User,
                content: Content::text(format!(
                    "Here is the current project data:\n\n{context_text}"
                )),
            },
            PromptMessage {
                role: Role::User,
                content: Content::text(format!(
                    "Generate a project status report. {period_instruction}\n\n\
                     Include:\n\
                     1. Executive summary (2-3 sentences)\n\
                     2. Key metrics (total, active, blocked, in-progress)\n\
                     3. Current work in progress\n\
                     4. Blockers and risks\n\
                     5. Recommendations for next actions"
                )),
            },
        ])
    }
}

// ---------------------------------------------------------------------------
// 3. plan_next_work — bv-inspired graph-aware work planning
// ---------------------------------------------------------------------------

/// Compute bottleneck context: issues that block the most other open issues.
fn bottleneck_context(storage: &SqliteStorage) -> McpResult<String> {
    let edges = storage.get_blocks_dep_edges().map_err(to_mcp)?;
    let open_filters = ListFilters {
        include_closed: false,
        limit: Some(BOTTLENECK_FETCH_LIMIT),
        ..ListFilters::default()
    };
    let open_issues = storage.list_issues(&open_filters).map_err(to_mcp)?;
    let open_ids: HashSet<&str> = open_issues.iter().map(|i| i.id.as_str()).collect();

    // Count how many open issues each open issue blocks
    let mut blocks_count: HashMap<&str, usize> = HashMap::new();
    for (blocked, blocker) in &edges {
        if open_ids.contains(blocker.as_str()) && open_ids.contains(blocked.as_str()) {
            *blocks_count.entry(blocker.as_str()).or_default() += 1;
        }
    }

    let mut ranked: Vec<_> = blocks_count.into_iter().collect();
    ranked.sort_by_key(|b| std::cmp::Reverse(b.1));

    if ranked.is_empty() {
        return Ok("No dependency bottlenecks detected.".into());
    }

    let issue_map: HashMap<&str, &Issue> = open_issues.iter().map(|i| (i.id.as_str(), i)).collect();

    let bottleneck_json: Vec<_> = ranked
        .iter()
        .take(BOTTLENECK_DISPLAY_LIMIT)
        .filter_map(|(id, count)| {
            issue_map.get(id).map(|i| {
                json!({
                    "id": i.id,
                    "title": i.title,
                    "priority": i.priority,
                    "status": i.status,
                    "blocks_count": count,
                })
            })
        })
        .collect();

    Ok(format!(
        "Bottleneck issues (block the most work, resolve first):\n{}",
        serde_json::to_string_pretty(&bottleneck_json).unwrap_or_default()
    ))
}

/// Identify quick wins: high-priority, ready, with low estimated effort.
fn quick_wins_context(state: &BeadsState, storage: &SqliteStorage) -> McpResult<String> {
    let ready = mcp_ready_issues(state, storage)?;

    if ready.is_empty() {
        return Ok("No quick wins available — no ready issues.".into());
    }

    // Quick wins: ready issues with high priority or low estimated effort
    let mut wins: Vec<_> = ready
        .iter()
        .filter(|i| {
            i.priority.0 <= 2 // critical, high, or medium
                || i.estimated_minutes.is_some_and(|m| m <= 30)
        })
        .take(QUICK_WINS_DISPLAY_LIMIT)
        .map(|i| {
            json!({
                "id": i.id,
                "title": i.title,
                "priority": i.priority,
                "estimated_minutes": i.estimated_minutes,
            })
        })
        .collect();

    if wins.is_empty() {
        // Fall back to first few ready issues
        wins = ready
            .iter()
            .take(QUICK_WINS_DISPLAY_LIMIT)
            .map(|i| {
                json!({
                    "id": i.id,
                    "title": i.title,
                    "priority": i.priority,
                })
            })
            .collect();
    }

    Ok(format!(
        "Quick wins (high-impact, ready to start):\n{}",
        serde_json::to_string_pretty(&wins).unwrap_or_default()
    ))
}

pub struct PlanNextWorkPrompt(Arc<BeadsState>);
impl PlanNextWorkPrompt {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl PromptHandler for PlanNextWorkPrompt {
    fn definition(&self) -> Prompt {
        Prompt {
            name: "plan_next_work".into(),
            description: Some(
                "Graph-aware work planning: identifies bottlenecks, quick wins, \
                 and the optimal next action based on dependency analysis. \
                 Inspired by bv's PageRank-based prioritization."
                    .into(),
            ),
            arguments: vec![PromptArgument {
                name: "goal".into(),
                description: Some(
                    "What you're trying to achieve: 'unblock' (clear bottlenecks), \
                     'quick-wins' (fast impact), or 'balanced' (default — considers both)"
                        .into(),
                ),
                required: false,
            }],
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn get(
        &self,
        _ctx: &McpContext,
        arguments: HashMap<String, String>,
    ) -> McpResult<Vec<PromptMessage>> {
        ensure_not_shutting_down()?;

        let raw_goal = arguments.get("goal").map_or("balanced", String::as_str);
        let (goal, goal_warning) = validate_prompt_arg(
            raw_goal,
            &["balanced", "unblock", "quick-wins"],
            "balanced",
            "goal",
        );

        let storage = self.0.open_read_storage().map_err(to_mcp)?;

        let total = storage.count_all_issues().map_err(to_mcp)?;
        let active = storage.count_active_issues().map_err(to_mcp)?;
        let blocked = storage.get_blocked_issues().map_err(to_mcp)?;
        let ready = mcp_ready_issues(&self.0, &storage)?;

        let mut parts: Vec<String> = Vec::new();

        if let Some(w) = goal_warning {
            parts.push(format!("Note: {w}"));
        }

        parts.push(format!(
            "Project: {total} total, {active} active, {} blocked, {} ready.",
            blocked.len(),
            ready.len(),
        ));

        if goal == "balanced" || goal == "unblock" {
            parts.push(bottleneck_context(&storage)?);
            parts.push(blocked_context(&storage)?);
        }
        if goal == "balanced" || goal == "quick-wins" {
            parts.push(quick_wins_context(&self.0, &storage)?);
        }
        if goal == "balanced" {
            parts.push(ready_context(&self.0, &storage)?);
        }

        let instruction = match goal {
            "unblock" => {
                "Based on the bottleneck analysis above, recommend what to work on next.\n\n\
                 Decision framework (from bv graph analysis):\n\
                 - High blocks_count = high impact — resolve these first to unblock the most work\n\
                 - If a bottleneck is blocked itself, trace the chain to find the root blocker\n\
                 - Consider removing unnecessary dependencies (manage_dependencies action 'remove')\n\n\
                 Provide:\n\
                 1. The single most impactful issue to work on next and WHY\n\
                 2. What it will unblock when resolved\n\
                 3. Any dependency chains that should be simplified"
            }
            "quick-wins" => {
                "Based on the ready issues above, recommend quick wins.\n\n\
                 Quick win criteria:\n\
                 - Ready (not blocked, not deferred)\n\
                 - High priority or low estimated effort\n\
                 - Resolving it may unblock other work\n\n\
                 Provide:\n\
                 1. Top 3 quick wins to tackle now\n\
                 2. Expected impact of each\n\
                 3. Suggested order of execution"
            }
            _ => {
                "Based on the analysis above, recommend what to work on next.\n\n\
                 Decision framework (from bv graph analysis):\n\
                 - FIRST: Check bottlenecks — high blocks_count issues should be resolved first\n\
                 - THEN: Quick wins — high-priority, low-effort, ready items for fast progress\n\
                 - WATCH: Blocked items — trace dependency chains to find root blockers\n\n\
                 Pattern recognition:\n\
                 - High blocks_count + high priority = CRITICAL — drop everything\n\
                 - High blocks_count + low priority = priority mismatch — consider upgrading\n\
                 - Zero blocks_count + ready = leaf task — safe to parallelize\n\n\
                 Provide:\n\
                 1. The #1 recommended next action and why\n\
                 2. 2-3 alternative actions if #1 is not feasible\n\
                 3. Any priority mismatches or dependency issues to address"
            }
        };

        Ok(vec![
            PromptMessage {
                role: Role::User,
                content: Content::text(format!(
                    "Here is the current project state with dependency analysis:\n\n{}",
                    parts.join("\n\n")
                )),
            },
            PromptMessage {
                role: Role::User,
                content: Content::text(instruction.to_string()),
            },
        ])
    }
}

// ---------------------------------------------------------------------------
// 4. polish_backlog — beads-workflow-inspired quality review
// ---------------------------------------------------------------------------

/// Analyze issue completeness: missing descriptions, test plans, etc.
fn completeness_context(storage: &SqliteStorage) -> McpResult<String> {
    let filters = ListFilters {
        include_closed: false,
        limit: Some(COMPLETENESS_FETCH_LIMIT),
        ..ListFilters::default()
    };
    let issues = storage.list_issues(&filters).map_err(to_mcp)?;

    let mut no_description: Vec<&Issue> = Vec::new();
    let mut short_description: Vec<&Issue> = Vec::new();
    let mut no_priority_set: Vec<&Issue> = Vec::new();

    for issue in &issues {
        match &issue.description {
            None => no_description.push(issue),
            Some(d) if d.len() < 50 => short_description.push(issue),
            _ => {}
        }
        // Default priority is medium (value=2) — flag if still default
        // and issue is non-trivial (not a chore/question)
        if issue.priority.0 == 2
            && !matches!(issue.issue_type, IssueType::Chore | IssueType::Question)
        {
            no_priority_set.push(issue);
        }
    }

    let mut parts: Vec<String> = Vec::new();

    if !no_description.is_empty() {
        let ids: Vec<_> = no_description
            .iter()
            .take(QUALITY_SAMPLE_LIMIT)
            .map(|i| json!({"id": i.id, "title": i.title}))
            .collect();
        parts.push(format!(
            "Issues with NO description ({}):\n{}",
            no_description.len(),
            serde_json::to_string_pretty(&ids).unwrap_or_default()
        ));
    }

    if !short_description.is_empty() {
        let ids: Vec<_> = short_description.iter().take(QUALITY_SAMPLE_LIMIT).map(|i| {
            json!({"id": i.id, "title": i.title, "desc_length": i.description.as_ref().map(String::len)})
        }).collect();
        parts.push(format!(
            "Issues with very short descriptions (<50 chars, {}):\n{}",
            short_description.len(),
            serde_json::to_string_pretty(&ids).unwrap_or_default()
        ));
    }

    if !no_priority_set.is_empty() {
        let ids: Vec<_> = no_priority_set
            .iter()
            .take(QUALITY_SAMPLE_LIMIT)
            .map(|i| json!({"id": i.id, "title": i.title, "type": i.issue_type}))
            .collect();
        parts.push(format!(
            "Issues still at default priority (medium) that may need review ({}):\n{}",
            no_priority_set.len(),
            serde_json::to_string_pretty(&ids).unwrap_or_default()
        ));
    }

    if parts.is_empty() {
        return Ok("All open issues have descriptions and non-default priorities.".into());
    }

    Ok(parts.join("\n\n"))
}

/// Analyze dependency health: orphan issues, shallow chains.
fn dependency_health_context(storage: &SqliteStorage) -> McpResult<String> {
    let filters = ListFilters {
        include_closed: false,
        include_deferred: true, // deferred issues are still part of the project
        limit: Some(DEPENDENCY_HEALTH_FETCH_LIMIT),
        ..ListFilters::default()
    };
    let open_issues = storage.list_issues(&filters).map_err(to_mcp)?;
    let edges = storage.get_blocks_dep_edges().map_err(to_mcp)?;
    let open_ids: HashSet<&str> = open_issues.iter().map(|i| i.id.as_str()).collect();

    // Find orphan issues (no dependencies in or out)
    let mut connected: HashSet<&str> = HashSet::new();
    for (from, to) in &edges {
        if open_ids.contains(from.as_str()) {
            connected.insert(from.as_str());
        }
        if open_ids.contains(to.as_str()) {
            connected.insert(to.as_str());
        }
    }

    let orphans: Vec<_> = open_issues
        .iter()
        .filter(|i| !connected.contains(i.id.as_str()))
        .take(ORPHAN_DISPLAY_LIMIT)
        .map(|i| json!({"id": i.id, "title": i.title, "priority": i.priority}))
        .collect();

    let mut parts: Vec<String> = Vec::new();

    if !orphans.is_empty() {
        parts.push(format!(
            "Orphan issues (no dependencies — consider linking or are they standalone?):\n{}",
            serde_json::to_string_pretty(&orphans).unwrap_or_default()
        ));
    }

    let open_edge_count = edges
        .iter()
        .filter(|(f, t)| open_ids.contains(f.as_str()) && open_ids.contains(t.as_str()))
        .count();

    parts.push(format!(
        "Dependency coverage: {} open issues, {} dependency edges, {} connected, {} orphans.",
        open_issues.len(),
        open_edge_count,
        connected.len(),
        orphans.len()
    ));

    Ok(parts.join("\n\n"))
}

pub struct PolishBacklogPrompt(Arc<BeadsState>);
impl PolishBacklogPrompt {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl PromptHandler for PolishBacklogPrompt {
    fn definition(&self) -> Prompt {
        Prompt {
            name: "polish_backlog".into(),
            description: Some(
                "Review and polish issue quality: check descriptions for completeness, \
                 validate dependencies, identify orphans and missing test plans. \
                 Inspired by beads-workflow's 'check your beads N times' principle."
                    .into(),
            ),
            arguments: vec![PromptArgument {
                name: "focus".into(),
                description: Some(
                    "Focus area: 'completeness' (descriptions & priorities), \
                     'dependencies' (graph health & orphans), or 'all' (full review). Default: all"
                        .into(),
                ),
                required: false,
            }],
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn get(
        &self,
        _ctx: &McpContext,
        arguments: HashMap<String, String>,
    ) -> McpResult<Vec<PromptMessage>> {
        ensure_not_shutting_down()?;

        let raw_focus = arguments.get("focus").map_or("all", String::as_str);
        let (focus, focus_warning) = validate_prompt_arg(
            raw_focus,
            &["all", "completeness", "dependencies"],
            "all",
            "focus",
        );

        let storage = self.0.open_read_storage().map_err(to_mcp)?;

        let total = storage.count_all_issues().map_err(to_mcp)?;
        let active = storage.count_active_issues().map_err(to_mcp)?;

        let mut parts: Vec<String> = Vec::new();

        if let Some(w) = focus_warning {
            parts.push(format!("Note: {w}"));
        }

        parts.push(format!(
            "Project has {total} total issues ({active} active/non-closed)."
        ));

        if focus == "all" || focus == "completeness" {
            parts.push(completeness_context(&storage)?);
        }
        if focus == "all" || focus == "dependencies" {
            parts.push(dependency_health_context(&storage)?);
        }

        let instruction = match focus {
            "completeness" => {
                "Review the completeness analysis above. For each issue flagged:\n\n\
                 1. Check if the description follows the bead anatomy structure:\n\
                    - Background (why this exists)\n\
                    - Technical Approach (how to implement)\n\
                    - Success Criteria (how to verify done)\n\
                    - Test Plan (unit + E2E tests)\n\
                    - Considerations (edge cases, risks)\n\n\
                 2. For issues with no/short descriptions, use show_issue to review, \
                    then use update_issue to add a proper description.\n\n\
                 3. For issues at default priority, assess actual importance and update.\n\n\
                 Self-containment rule: each issue should be understandable WITHOUT \
                 consulting any external plan or document."
            }
            "dependencies" => {
                "Review the dependency health analysis above.\n\n\
                 For orphan issues:\n\
                 1. Determine if they genuinely standalone or if dependencies are missing\n\
                 2. Use manage_dependencies to add blocking relationships where appropriate\n\
                 3. Consider if orphans should be linked as sub-issues (parent-child)\n\n\
                 For the dependency graph:\n\
                 1. Are all blocking relationships explicit?\n\
                 2. Can any dependency chains be shortened to improve parallelization?\n\
                 3. Are there implicit dependencies that should be made explicit?\n\n\
                 Goal: every issue should have clear dependencies so that 'ready' \
                 accurately reflects what can be worked on in parallel."
            }
            _ => {
                "Perform a full backlog polish (beads-workflow style).\n\n\
                 This is the 'check your beads N times, implement once' workflow.\n\n\
                 Pass 1 — Completeness:\n\
                 1. Review issues with missing/short descriptions\n\
                 2. Ensure each description follows the bead anatomy structure:\n\
                    Background → Technical Approach → Success Criteria → Test Plan → Considerations\n\
                 3. Verify priorities are set deliberately, not left at defaults\n\n\
                 Pass 2 — Dependencies:\n\
                 4. Review orphan issues — add missing dependencies\n\
                 5. Check that blocking relationships are explicit and correct\n\
                 6. Look for opportunities to shorten dependency chains\n\n\
                 Pass 3 — Quality:\n\
                 7. Flag any duplicate or overlapping issues\n\
                 8. Identify issues that should be split (too large) or merged (too granular)\n\
                 9. Ensure test coverage: every feature should have companion test criteria\n\n\
                 Summarize findings and use update_issue/manage_dependencies to fix issues."
            }
        };

        Ok(vec![
            PromptMessage {
                role: Role::User,
                content: Content::text(format!(
                    "Here is the backlog quality analysis:\n\n{}",
                    parts.join("\n\n")
                )),
            },
            PromptMessage {
                role: Role::User,
                content: Content::text(instruction.to_string()),
            },
        ])
    }
}
