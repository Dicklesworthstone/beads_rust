//! Read-only swarm coordination diagnostics.

use crate::cli::{
    CoordinationOwnerKindArg, CoordinationStatusArgs, OutputFormat,
    resolve_output_format_basic_with_outer_mode,
};
use crate::config;
use crate::coordination::{
    AgentMailAgentSnapshot, AgentMailReservationSnapshot, ClaimAssessmentInput, ClaimOwnerKind,
    CoordinationClaimRow, CoordinationComment, CoordinationIssueRow, CoordinationStatusOutput,
    CoordinationWorkspaceCounts, ReservationEvidence, ReservationEvidenceProvenance,
    advisory_for_claim, assess_claim, reservation_evidence_from_snapshots,
};
use crate::error::{BeadsError, Result};
use crate::format::{sanitize_terminal_inline, truncate_title};
use crate::model::{Comment, Issue, Status};
use crate::output::{OutputContext, OutputMode};
use crate::storage::{ListFilters, ReadyFilters, ReadySortPolicy, SqliteStorage};
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Execute `br coordination status`.
///
/// # Errors
///
/// Returns an error if the workspace cannot be opened or read.
pub fn execute_status(
    args: &CoordinationStatusArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    execute_status_inner(args, cli, outer_ctx, &beads_dir, None, None)
}

/// Execute `br coordination status` using the caller's pre-opened storage.
///
/// # Errors
///
/// Returns an error if coordination rows cannot be loaded.
pub fn execute_status_with_storage_ctx(
    args: &CoordinationStatusArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    execute_status_inner(args, cli, outer_ctx, beads_dir, None, Some(storage_ctx))
}

fn execute_status_inner(
    args: &CoordinationStatusArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    preloaded_storage: Option<&SqliteStorage>,
    preloaded_storage_ctx: Option<&config::OpenStorageResult>,
) -> Result<()> {
    let owned_storage_ctx = if preloaded_storage.is_some() || preloaded_storage_ctx.is_some() {
        None
    } else {
        Some(config::open_storage_with_cli(beads_dir, cli)?)
    };
    let storage = preloaded_storage
        .or_else(|| preloaded_storage_ctx.map(|ctx| &ctx.storage))
        .or_else(|| owned_storage_ctx.as_ref().map(|ctx| &ctx.storage))
        .ok_or_else(|| BeadsError::internal("coordination status missing open storage handle"))?;

    let output_format = resolve_output_format_basic_with_outer_mode(
        args.format,
        outer_ctx.inherited_output_mode(),
        args.robot,
    );
    let snapshots = SnapshotContext::from_args(args)?;
    let quiet = cli.quiet.unwrap_or(false);
    let ctx = OutputContext::from_output_format(output_format, quiet, true);
    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    let owner_kind = owner_kind_from_arg(args.owner_kind);
    let generated_at = Utc::now();
    let output = if snapshots.has_snapshots() {
        build_coordination_status_output(
            storage,
            owner_kind,
            args.comments,
            &snapshots,
            generated_at,
        )?
    } else {
        build_coordination_status_without_snapshots(
            storage,
            owner_kind,
            args.comments,
            generated_at,
        )?
    };

    match output_format {
        OutputFormat::Json => ctx.json_pretty(&output),
        OutputFormat::Toon => ctx.toon_with_stats(&output, args.stats),
        OutputFormat::Text | OutputFormat::Csv => print_text_output(&output),
    }

    Ok(())
}

/// Build the shared coordination status envelope without offline Agent Mail
/// snapshots. MCP serve uses this path to mirror `br coordination status --json`
/// while keeping the resource read-only and filesystem-local.
pub(crate) fn build_coordination_status_without_snapshots(
    storage: &SqliteStorage,
    owner_kind: ClaimOwnerKind,
    comment_limit: usize,
    generated_at: DateTime<Utc>,
) -> Result<CoordinationStatusOutput> {
    build_coordination_status_output(
        storage,
        owner_kind,
        comment_limit,
        &SnapshotContext::default(),
        generated_at,
    )
}

fn build_coordination_status_output(
    storage: &SqliteStorage,
    owner_kind: ClaimOwnerKind,
    comment_limit: usize,
    snapshots: &SnapshotContext,
    generated_at: DateTime<Utc>,
) -> Result<CoordinationStatusOutput> {
    let filters = ListFilters {
        statuses: Some(vec![Status::InProgress]),
        include_deferred: true,
        sort: Some("updated_at".to_string()),
        ..ListFilters::default()
    };
    let issues = storage.list_issues(&filters)?;
    let issue_ids = issues
        .iter()
        .map(|issue| issue.id.clone())
        .collect::<Vec<_>>();
    let labels_by_issue = storage.get_labels_for_issues(&issue_ids)?;
    let (dependency_counts, dependent_counts) =
        storage.count_relation_counts_for_issues(&issue_ids)?;
    let comment_scan_limit = if snapshots.has_reservation_snapshot() {
        comment_limit.max(1)
    } else {
        comment_limit
    };
    let comments_by_issue =
        storage.get_latest_comments_for_issues(&issue_ids, comment_scan_limit)?;

    let claims: Vec<CoordinationClaimRow> = issues
        .into_iter()
        .map(|issue| {
            let issue_id = issue.id.clone();
            let comments = comments_by_issue
                .get(&issue_id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let agent = snapshots.agent_for(issue.assignee.as_deref());
            let reservation = snapshots.reservation_for(
                &issue_id,
                issue.assignee.as_deref(),
                comments,
                generated_at,
            );
            let owner_kind = if agent.is_some() {
                ClaimOwnerKind::SwarmAgent
            } else {
                owner_kind
            };
            build_claim_row(
                issue,
                ClaimRowContext {
                    owner_kind,
                    reservation,
                    agent,
                    generated_at,
                    labels: labels_by_issue.get(&issue_id).cloned().unwrap_or_default(),
                    dependency_count: dependency_counts.get(&issue_id).copied().unwrap_or(0),
                    dependent_count: dependent_counts.get(&issue_id).copied().unwrap_or(0),
                    comments,
                    comment_limit,
                },
            )
        })
        .collect();
    let workspace = workspace_counts(storage, claims.len())?;

    let mut output = CoordinationStatusOutput::new(generated_at, workspace, claims);
    // GitHub #384 phase 6: capacity occupancy, absent when unconfigured.
    output.capacity = storage
        .capacity_snapshot()?
        .into_iter()
        .map(crate::format::CapacityStat::from_snapshot)
        .collect();
    Ok(output)
}

struct ClaimRowContext<'a> {
    owner_kind: ClaimOwnerKind,
    reservation: ReservationEvidence,
    agent: Option<AgentMailAgentSnapshot>,
    generated_at: DateTime<Utc>,
    labels: Vec<String>,
    dependency_count: usize,
    dependent_count: usize,
    comments: &'a [Comment],
    comment_limit: usize,
}

fn build_claim_row(issue: Issue, context: ClaimRowContext<'_>) -> CoordinationClaimRow {
    let latest_comments = latest_comments(context.comments, context.comment_limit);
    let issue_id = issue.id;
    let assessment = assess_claim(ClaimAssessmentInput {
        assignee: issue.assignee.clone(),
        updated_at: issue.updated_at,
        now: context.generated_at,
        owner_kind: context.owner_kind,
        reservation: context.reservation,
    });
    let advisory = advisory_for_claim(&issue_id, &assessment);
    let issue = CoordinationIssueRow {
        id: issue_id,
        title: issue.title,
        status: issue.status,
        priority: issue.priority,
        issue_type: issue.issue_type,
        labels: context.labels,
        dependency_count: context.dependency_count,
        dependent_count: context.dependent_count,
        latest_comments,
    };

    CoordinationClaimRow {
        issue,
        agent: context.agent,
        assessment,
        reclaim_allowed_by_policy: advisory.reclaim_allowed_by_policy,
        required_human_confirmation: advisory.required_human_confirmation,
        evidence_summary: advisory.evidence_summary,
        suggested_commands: advisory.suggested_commands,
    }
}

#[derive(Default)]
struct SnapshotContext {
    reservations: Option<Vec<AgentMailReservationSnapshot>>,
    agents_by_name: HashMap<String, AgentMailAgentSnapshot>,
}

impl SnapshotContext {
    fn from_args(args: &CoordinationStatusArgs) -> Result<Self> {
        let reservations = args
            .reservations
            .as_deref()
            .map(|path| read_snapshot_rows(path, "reservation", "reservations"))
            .transpose()?;
        let agents = args
            .agents
            .as_deref()
            .map(|path| read_snapshot_rows(path, "agent", "agents"))
            .transpose()?
            .unwrap_or_default();
        let agents_by_name = agents
            .into_iter()
            .map(|agent: AgentMailAgentSnapshot| (agent_key(&agent.name), agent))
            .collect();

        Ok(Self {
            reservations,
            agents_by_name,
        })
    }

    fn agent_for(&self, assignee: Option<&str>) -> Option<AgentMailAgentSnapshot> {
        assignee
            .map(agent_key)
            .and_then(|key| self.agents_by_name.get(&key).cloned())
    }

    const fn has_reservation_snapshot(&self) -> bool {
        self.reservations.is_some()
    }

    fn has_snapshots(&self) -> bool {
        self.reservations.is_some() || !self.agents_by_name.is_empty()
    }

    fn reservation_for(
        &self,
        issue_id: &str,
        assignee: Option<&str>,
        comments: &[Comment],
        now: DateTime<Utc>,
    ) -> ReservationEvidence {
        reservation_evidence_from_snapshots(
            issue_id,
            assignee,
            comments,
            self.reservations.as_deref(),
            now,
        )
    }
}

fn agent_key(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn read_snapshot_rows<T>(path: &Path, kind: &str, array_key: &str) -> Result<Vec<T>>
where
    T: DeserializeOwned,
{
    let raw = fs::read_to_string(path).map_err(|err| {
        BeadsError::validation(
            "coordination_snapshot",
            format!("failed to read {kind} snapshot {}: {err}", path.display()),
        )
    })?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => parse_snapshot_value(value, path, kind, array_key),
        Err(json_error) => parse_snapshot_jsonl(trimmed, path, kind).map_err(|line_error| {
            BeadsError::validation(
                "coordination_snapshot",
                format!(
                    "failed to parse {kind} snapshot {} as JSON ({json_error}) or JSONL ({line_error})",
                    path.display()
                ),
            )
        }),
    }
}

fn parse_snapshot_value<T>(value: Value, path: &Path, kind: &str, array_key: &str) -> Result<Vec<T>>
where
    T: DeserializeOwned,
{
    match value {
        Value::Array(_) => serde_json::from_value(value).map_err(|err| {
            BeadsError::validation(
                "coordination_snapshot",
                format!("invalid {kind} snapshot array {}: {err}", path.display()),
            )
        }),
        Value::Object(mut object) => {
            if let Some(rows) = object.remove(array_key) {
                return serde_json::from_value(rows).map_err(|err| {
                    BeadsError::validation(
                        "coordination_snapshot",
                        format!(
                            "invalid {kind} snapshot field `{array_key}` in {}: {err}",
                            path.display()
                        ),
                    )
                });
            }
            serde_json::from_value(Value::Object(object))
                .map(|row| vec![row])
                .map_err(|err| {
                    BeadsError::validation(
                        "coordination_snapshot",
                        format!("invalid {kind} snapshot object {}: {err}", path.display()),
                    )
                })
        }
        _ => Err(BeadsError::validation(
            "coordination_snapshot",
            format!(
                "{kind} snapshot {} must be a JSON object, JSON array, or JSONL stream",
                path.display()
            ),
        )),
    }
}

fn parse_snapshot_jsonl<T>(
    raw: &str,
    path: &Path,
    kind: &str,
) -> std::result::Result<Vec<T>, String>
where
    T: DeserializeOwned,
{
    let mut rows = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let row = serde_json::from_str(trimmed).map_err(|err| {
            format!(
                "line {} in {} is not a valid {kind} row: {err}",
                idx + 1,
                path.display()
            )
        })?;
        rows.push(row);
    }
    Ok(rows)
}

fn latest_comments(comments: &[Comment], limit: usize) -> Vec<CoordinationComment> {
    comments
        .iter()
        .rev()
        .take(limit)
        .map(CoordinationComment::from)
        .collect()
}

fn workspace_counts(
    storage: &SqliteStorage,
    in_progress_count: usize,
) -> Result<CoordinationWorkspaceCounts> {
    Ok(CoordinationWorkspaceCounts {
        open: status_count(storage, &Status::Open)?,
        ready: storage
            .get_ready_issues_for_command_output(
                &ReadyFilters::default(),
                ReadySortPolicy::Priority,
            )?
            .len(),
        blocked: storage.get_blocked_ids()?.len(),
        in_progress: in_progress_count,
        deferred: status_count(storage, &Status::Deferred)?,
        closed: status_count(storage, &Status::Closed)?,
    })
}

fn status_count(storage: &SqliteStorage, status: &Status) -> Result<usize> {
    storage.count_issues_with_filters(&ListFilters {
        statuses: Some(vec![status.clone()]),
        include_closed: matches!(status, &Status::Closed | &Status::Tombstone),
        include_deferred: matches!(status, &Status::Deferred),
        ..ListFilters::default()
    })
}

const fn owner_kind_from_arg(arg: CoordinationOwnerKindArg) -> ClaimOwnerKind {
    match arg {
        CoordinationOwnerKindArg::SwarmAgent => ClaimOwnerKind::SwarmAgent,
        CoordinationOwnerKindArg::Human => ClaimOwnerKind::Human,
        CoordinationOwnerKindArg::Unknown => ClaimOwnerKind::Unknown,
    }
}

fn print_text_output(output: &CoordinationStatusOutput) {
    println!(
        "Coordination status ({} in-progress claim{}):",
        output.summary.total_claims,
        if output.summary.total_claims == 1 {
            ""
        } else {
            "s"
        }
    );
    println!(
        "Workspace: open {} | ready {} | blocked {} | deferred {} | closed {}",
        output.summary.workspace.open,
        output.summary.workspace.ready,
        output.summary.workspace.blocked,
        output.summary.workspace.deferred,
        output.summary.workspace.closed
    );

    if output.claims.is_empty() {
        println!("No in-progress claims.");
        return;
    }

    for claim in &output.claims {
        let issue_id = sanitize_terminal_inline(&claim.issue.id);
        println!(
            "- {} [{} {}] {}",
            issue_id,
            claim.issue.priority,
            claim.issue.issue_type,
            truncate_title(&claim.issue.title, 72)
        );
        println!(
            "  assignee: {} | age: {}m | classification: {} | next_action: {}",
            claim
                .assessment
                .assignee
                .as_deref()
                .map(sanitize_terminal_inline)
                .unwrap_or_else(|| "(unassigned)".into()),
            claim.assessment.updated_age_minutes,
            claim.assessment.classification.as_str(),
            claim.assessment.recommended_action.as_str()
        );
        println!(
            "  deps: {} | dependents: {} | labels: {}",
            claim.issue.dependency_count,
            claim.issue.dependent_count,
            text_labels(&claim.issue.labels)
        );
        if let Some(agent) = &claim.agent {
            println!(
                "  agent: {} | last_active: {} | contact: {}",
                sanitize_terminal_inline(&agent.name),
                agent.last_active_ts,
                sanitize_terminal_inline(&agent.contact_policy)
            );
        }
        if let Some((state, holder, provenance)) = reservation_text(&claim.assessment.reservation) {
            println!(
                "  reservation: {} holder={} path={} matched_on={}",
                state,
                sanitize_terminal_inline(holder),
                sanitize_terminal_inline(&provenance.path_pattern),
                text_match_reasons(provenance)
            );
        }
        println!(
            "  advisory: reclaim_allowed_by_policy={} | required_human_confirmation={} | evidence: {}",
            claim.reclaim_allowed_by_policy,
            claim.required_human_confirmation,
            sanitize_terminal_inline(&claim.evidence_summary)
        );
        for command in &claim.suggested_commands {
            println!(
                "  suggested_command: {}",
                sanitize_terminal_inline(&command.command)
            );
        }
        if let Some(comment) = claim.issue.latest_comments.first() {
            println!(
                "  latest_comment: {}: {}",
                sanitize_terminal_inline(&comment.author),
                truncate_title(&comment.text, 96)
            );
        }
    }
}

fn text_labels(labels: &[String]) -> String {
    if labels.is_empty() {
        return "(none)".to_string();
    }

    labels
        .iter()
        .map(|label| sanitize_terminal_inline(label).into_owned())
        .collect::<Vec<_>>()
        .join(",")
}

fn reservation_text(
    reservation: &ReservationEvidence,
) -> Option<(&'static str, &str, &ReservationEvidenceProvenance)> {
    match reservation {
        ReservationEvidence::Active {
            holder,
            provenance: Some(provenance),
            ..
        } => Some(("active", holder, provenance)),
        ReservationEvidence::Expired {
            holder,
            provenance: Some(provenance),
            ..
        } => Some(("expired", holder, provenance)),
        _ => None,
    }
}

fn text_match_reasons(provenance: &ReservationEvidenceProvenance) -> String {
    provenance
        .matched_on
        .iter()
        .map(|reason| reason.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::{
        ClaimRowContext, SnapshotContext, build_claim_row, latest_comments, owner_kind_from_arg,
        parse_snapshot_value,
    };
    use crate::cli::CoordinationOwnerKindArg;
    use crate::coordination::{
        AgentMailAgentSnapshot, AgentMailReservationSnapshot, ClaimClassification, ClaimOwnerKind,
        RecommendedAction, ReservationEvidence,
    };
    use crate::model::{Comment, Issue, IssueType, Priority, Status};
    use chrono::{Duration, TimeZone, Utc};
    use serde_json::json;
    use std::path::Path;

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 8, 9, 0, 0)
            .single()
            .expect("valid timestamp")
    }

    fn issue(updated_at: chrono::DateTime<Utc>, assignee: Option<&str>) -> Issue {
        Issue {
            id: "bd-claim".to_string(),
            title: "Claimed issue".to_string(),
            status: Status::InProgress,
            priority: Priority(1),
            issue_type: IssueType::Task,
            assignee: assignee.map(str::to_string),
            updated_at,
            ..Issue::default()
        }
    }

    #[test]
    fn build_claim_row_uses_shared_no_snapshot_classification() {
        let row = build_claim_row(
            issue(now() - Duration::hours(2), Some("TopazFox")),
            ClaimRowContext {
                owner_kind: ClaimOwnerKind::SwarmAgent,
                reservation: ReservationEvidence::NoSnapshot,
                agent: None,
                generated_at: now(),
                labels: vec!["coordination".to_string()],
                dependency_count: 2,
                dependent_count: 3,
                comments: &[],
                comment_limit: 2,
            },
        );

        assert_eq!(row.issue.id, "bd-claim");
        assert_eq!(row.issue.labels, ["coordination"]);
        assert_eq!(row.issue.dependency_count, 2);
        assert_eq!(row.issue.dependent_count, 3);
        assert_eq!(
            row.assessment.classification,
            ClaimClassification::NoMailSnapshot
        );
        assert_eq!(
            row.assessment.recommended_action,
            RecommendedAction::InspectMail
        );
        assert!(!row.reclaim_allowed_by_policy);
        assert!(row.suggested_commands.is_empty());
    }

    #[test]
    fn latest_comments_are_newest_first_and_bounded() {
        let comments = vec![
            Comment {
                id: 1,
                issue_id: "bd-claim".to_string(),
                author: "a".to_string(),
                body: "old".to_string(),
                created_at: now() - Duration::hours(2),
            },
            Comment {
                id: 2,
                issue_id: "bd-claim".to_string(),
                author: "b".to_string(),
                body: "new".to_string(),
                created_at: now(),
            },
        ];

        let latest = latest_comments(&comments, 1);

        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].text, "new");
    }

    #[test]
    fn owner_kind_arg_maps_to_coordination_policy() {
        assert_eq!(
            owner_kind_from_arg(CoordinationOwnerKindArg::Human),
            ClaimOwnerKind::Human
        );
        assert_eq!(
            owner_kind_from_arg(CoordinationOwnerKindArg::Unknown),
            ClaimOwnerKind::Unknown
        );
    }

    #[test]
    fn snapshot_context_matches_agent_names_case_insensitively() {
        let mut context = SnapshotContext::default();
        context.agents_by_name.insert(
            "topazfox".to_string(),
            AgentMailAgentSnapshot {
                name: "TopazFox".to_string(),
                task_description: "coordination work".to_string(),
                last_active_ts: now(),
                contact_policy: "auto".to_string(),
            },
        );

        let agent = context
            .agent_for(Some(" topazFOX "))
            .expect("agent snapshot should match");

        assert_eq!(agent.name, "TopazFox");
    }

    #[test]
    fn snapshot_context_reports_reservation_snapshot_presence() {
        let mut context = SnapshotContext::default();
        assert!(!context.has_reservation_snapshot());

        context.reservations = Some(Vec::new());
        assert!(context.has_reservation_snapshot());
    }

    #[test]
    fn parse_snapshot_value_rejects_malformed_rows() {
        let value = json!({
            "reservations": [
                {
                    "holder": "TopazFox",
                    "path_pattern": "src/coordination.rs"
                }
            ]
        });

        let result = parse_snapshot_value::<AgentMailReservationSnapshot>(
            value,
            Path::new("reservations.json"),
            "reservation",
            "reservations",
        );

        assert!(result.is_err(), "missing timestamp fields must be invalid");
    }
}
