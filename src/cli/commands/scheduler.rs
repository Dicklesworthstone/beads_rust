//! Swarm scheduler command implementation.
//!
//! Ranks ready work with explicit evidence terms for agents that need a stable,
//! machine-readable assignment surface.

use super::auto_import_external_projects_if_stale;
use crate::cli::{OutputFormat, SchedulerArgs, resolve_output_format_basic_with_outer_mode};
use crate::config;
use crate::coordination::{
    ClaimAssessmentInput, ClaimAssessmentPolicy, ClaimClassification, ClaimOwnerKind,
    RecommendedAction, ReservationEvidence, assess_claim_with_policy,
};
use crate::error::{BeadsError, Result};
use crate::format::{ReadyIssue, sanitize_terminal_inline};
use crate::output::{OutputContext, OutputMode};
use crate::storage::sqlite::ListRelationMetadata;
use crate::storage::{ReadyFilters, ReadySortPolicy, SqliteStorage};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

const SCHEDULER_SCHEMA: &str = "br.scheduler.v1";
const PRIORITY_WEIGHT: i64 = 10;
const DEPENDENT_WEIGHT: i64 = 3;
const MAX_DEPENDENT_CONTRIBUTION: i64 = 30;
const SCHEDULER_FULL_METADATA_THRESHOLD: usize = 96;

type SchedulerRelationMetadata = (
    HashMap<String, Vec<String>>,
    HashMap<String, usize>,
    HashMap<String, usize>,
);

#[derive(Debug, Serialize)]
struct SchedulerOutput {
    schema: &'static str,
    generated_at: DateTime<Utc>,
    candidate_count: usize,
    returned_count: usize,
    candidate_limit: Option<usize>,
    fallback_policy: FallbackPolicy,
    recommendations: Vec<SchedulerRecommendation>,
}

#[derive(Debug, Serialize)]
struct FallbackPolicy {
    sort: &'static str,
    candidate_cap: &'static str,
    exhaustion_behavior: &'static str,
}

#[derive(Debug, Serialize)]
struct SchedulerRecommendation {
    rank: usize,
    fallback_rank: usize,
    score: i64,
    issue: ReadyIssue,
    evidence: SchedulerEvidence,
    rationale: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SchedulerEvidence {
    priority: PriorityEvidence,
    dependency_impact: DependencyImpactEvidence,
    stale_claim: StaleClaimEvidence,
    fairness: FairnessEvidence,
    domain_contention: DomainContentionEvidence,
}

#[derive(Debug, Serialize)]
struct PriorityEvidence {
    value: i32,
    contribution: i64,
}

#[derive(Debug, Serialize)]
struct DependencyImpactEvidence {
    dependency_count: usize,
    dependent_count: usize,
    contribution: i64,
}

#[derive(Debug, Serialize)]
struct StaleClaimEvidence {
    assignee: Option<String>,
    owner_kind: ClaimOwnerKind,
    updated_age_minutes: i64,
    stale_threshold_minutes: i64,
    abandoned_threshold_minutes: i64,
    classification: ClaimClassification,
    recommended_action: RecommendedAction,
    reservation_status: &'static str,
    is_stale: bool,
    contribution: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    coordination_status_hint: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct FairnessEvidence {
    unassigned: bool,
    contribution: i64,
    reason: &'static str,
}

#[derive(Debug, Serialize)]
struct DomainContentionEvidence {
    domain: String,
    labels: Vec<String>,
    candidate_count_in_domain: usize,
    contribution: i64,
}

struct ScoredCandidate {
    fallback_rank: usize,
    score: i64,
    issue: ReadyIssue,
    evidence: SchedulerEvidence,
    rationale: Vec<String>,
}

struct ScoringInputs<'a> {
    dependency_counts: &'a HashMap<String, usize>,
    dependent_counts: &'a HashMap<String, usize>,
    labels_by_issue: &'a HashMap<String, Vec<String>>,
    domain_counts: &'a HashMap<String, usize>,
    stale_threshold_minutes: i64,
    now: &'a DateTime<Utc>,
}

/// Execute the scheduler command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or scheduler inputs cannot
/// be loaded.
pub fn execute(
    args: &SchedulerArgs,
    _json: bool,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    execute_inner(args, cli, outer_ctx, &beads_dir, None, None)
}

/// Execute scheduler using the caller's preopened storage context.
///
/// # Errors
///
/// Returns an error if scheduler inputs cannot be loaded.
pub fn execute_with_storage_ctx(
    args: &SchedulerArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    execute_inner(args, cli, outer_ctx, beads_dir, None, Some(storage_ctx))
}

fn execute_inner(
    args: &SchedulerArgs,
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
        .ok_or_else(|| BeadsError::internal("scheduler missing open storage handle"))?;
    let storage_ctx_for_config = preloaded_storage_ctx.or(owned_storage_ctx.as_ref());
    let output_format = resolve_output_format_basic_with_outer_mode(
        args.format,
        outer_ctx.inherited_output_mode(),
        args.robot,
    );
    let quiet = cli.quiet.unwrap_or(false);
    let early_ctx = OutputContext::from_output_format(output_format, quiet, true);

    let output = build_scheduler_output(args, cli, beads_dir, storage, storage_ctx_for_config)?;

    if matches!(early_ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    match output_format {
        OutputFormat::Json => early_ctx.json_pretty(&output),
        OutputFormat::Toon => early_ctx.toon_with_stats(&output, args.stats),
        OutputFormat::Text | OutputFormat::Csv => print_scheduler_text(&output),
    }

    Ok(())
}

fn build_scheduler_output(
    args: &SchedulerArgs,
    cli: &config::CliOverrides,
    beads_dir: &Path,
    storage: &SqliteStorage,
    storage_ctx: Option<&config::OpenStorageResult>,
) -> Result<SchedulerOutput> {
    let now = Utc::now();
    let candidate_limit = (args.candidate_limit > 0).then_some(args.candidate_limit);
    // Honor the configured "ready" status group (#354) so the scheduler scores
    // the same candidate set `br ready` surfaces. Defaults to `[open]`.
    let workflow = crate::close_policy::load_for_beads_dir(beads_dir)?.workflow;
    workflow.validate_ready_status_group()?;
    let mut filters = ReadyFilters {
        limit: candidate_limit,
        ready_statuses: workflow.ready_status_group(),
        ..ReadyFilters::default()
    };
    let mut issues =
        storage.get_ready_issues_for_command_output(&filters, ReadySortPolicy::Priority)?;

    if !issues.is_empty() && storage.has_external_dependencies(true)? {
        let config_layer = load_scheduler_config(beads_dir, storage, storage_ctx, cli)?;
        auto_import_external_projects_if_stale(&config_layer, beads_dir, cli);
        let external_db_paths = config::external_project_db_paths(&config_layer, beads_dir);
        let external_statuses =
            storage.resolve_external_dependency_statuses(&external_db_paths, true)?;
        let external_blockers = storage.external_blockers(&external_statuses)?;
        if !external_blockers.is_empty() {
            if should_refill_scheduler_candidates(candidate_limit, &issues, &external_blockers) {
                // External filtering can remove early fallback rows, so refill
                // the local ready set before applying the scheduler's scoring cap.
                filters.limit = None;
                issues = storage
                    .get_ready_issues_for_command_output(&filters, ReadySortPolicy::Priority)?;
            }
            issues.retain(|issue| !external_blockers.contains_key(&issue.id));
            if let Some(limit) = candidate_limit
                && issues.len() > limit
            {
                issues.truncate(limit);
            }
        }
    }

    let issue_ids = issues
        .iter()
        .map(|issue| issue.id.clone())
        .collect::<Vec<_>>();
    let (labels_by_issue, dependency_counts, dependent_counts) =
        load_scheduler_relation_metadata(storage, &issue_ids)?;
    let domain_counts = count_candidate_domains(&issues, &labels_by_issue);
    let stale_threshold_minutes = stale_threshold_minutes(args.stale_claim_hours)?;
    let scoring_inputs = ScoringInputs {
        dependency_counts: &dependency_counts,
        dependent_counts: &dependent_counts,
        labels_by_issue: &labels_by_issue,
        domain_counts: &domain_counts,
        stale_threshold_minutes,
        now: &now,
    };

    let candidate_count = issues.len();
    let mut scored = issues
        .into_iter()
        .enumerate()
        .map(|(index, issue)| score_candidate(issue, index + 1, &scoring_inputs))
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.fallback_rank.cmp(&right.fallback_rank))
    });

    if args.limit > 0 && scored.len() > args.limit {
        scored.truncate(args.limit);
    }

    let recommendations = scored
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| SchedulerRecommendation {
            rank: index + 1,
            fallback_rank: candidate.fallback_rank,
            score: candidate.score,
            issue: candidate.issue,
            evidence: candidate.evidence,
            rationale: candidate.rationale,
        })
        .collect::<Vec<_>>();

    Ok(SchedulerOutput {
        schema: SCHEDULER_SCHEMA,
        generated_at: now,
        candidate_count,
        returned_count: recommendations.len(),
        candidate_limit,
        fallback_policy: FallbackPolicy {
            sort: "priority ASC, created_at ASC, id ASC",
            candidate_cap: "score at most --candidate-limit ready candidates; 0 means unlimited",
            exhaustion_behavior: "if scoring evidence is tied or incomplete, preserve fallback rank",
        },
        recommendations,
    })
}

fn load_scheduler_config(
    beads_dir: &Path,
    storage: &SqliteStorage,
    storage_ctx: Option<&config::OpenStorageResult>,
    cli: &config::CliOverrides,
) -> Result<config::ConfigLayer> {
    if let Some(storage_ctx) = storage_ctx {
        storage_ctx.load_config(cli)
    } else {
        config::load_config(beads_dir, Some(storage), cli)
    }
}

fn should_refill_scheduler_candidates(
    candidate_limit: Option<usize>,
    issues: &[crate::model::Issue],
    external_blockers: &HashMap<String, Vec<String>>,
) -> bool {
    candidate_limit.is_some()
        && !external_blockers.is_empty()
        && issues
            .iter()
            .any(|issue| external_blockers.contains_key(&issue.id))
}

fn load_scheduler_relation_metadata(
    storage: &SqliteStorage,
    issue_ids: &[String],
) -> Result<SchedulerRelationMetadata> {
    if issue_ids.len() >= SCHEDULER_FULL_METADATA_THRESHOLD {
        let relation_metadata = storage.get_all_list_relation_metadata()?;
        return Ok(project_scheduler_relation_metadata(
            issue_ids,
            &relation_metadata,
        ));
    }

    let labels_by_issue = storage.get_labels_for_issues(issue_ids)?;
    let (dependency_counts, dependent_counts) =
        storage.count_relation_counts_for_issues(issue_ids)?;
    Ok((labels_by_issue, dependency_counts, dependent_counts))
}

fn project_scheduler_relation_metadata(
    issue_ids: &[String],
    relation_metadata: &HashMap<String, ListRelationMetadata>,
) -> SchedulerRelationMetadata {
    let mut labels_by_issue = HashMap::with_capacity(issue_ids.len());
    let mut dependency_counts = HashMap::with_capacity(issue_ids.len());
    let mut dependent_counts = HashMap::with_capacity(issue_ids.len());

    for issue_id in issue_ids {
        if let Some(metadata) = relation_metadata.get(issue_id) {
            if !metadata.labels.is_empty() {
                labels_by_issue.insert(issue_id.clone(), metadata.labels.clone());
            }
            if metadata.dependency_count > 0 {
                dependency_counts.insert(issue_id.clone(), metadata.dependency_count);
            }
            if metadata.dependent_count > 0 {
                dependent_counts.insert(issue_id.clone(), metadata.dependent_count);
            }
        }
    }

    (labels_by_issue, dependency_counts, dependent_counts)
}

fn score_candidate(
    issue: crate::model::Issue,
    fallback_rank: usize,
    inputs: &ScoringInputs<'_>,
) -> ScoredCandidate {
    let labels = inputs
        .labels_by_issue
        .get(&issue.id)
        .cloned()
        .unwrap_or_default();
    let domain = primary_domain(&issue, &labels);
    let dependency_count = *inputs.dependency_counts.get(&issue.id).unwrap_or(&0);
    let dependent_count = *inputs.dependent_counts.get(&issue.id).unwrap_or(&0);
    let domain_count = *inputs.domain_counts.get(&domain).unwrap_or(&1);
    let updated_age_minutes = inputs
        .now
        .signed_duration_since(issue.updated_at)
        .num_minutes()
        .max(0);
    let stale_claim = scheduler_stale_claim_evidence(
        &issue,
        inputs.now,
        updated_age_minutes,
        inputs.stale_threshold_minutes,
    );

    let priority_contribution =
        i64::from(4_i32.saturating_sub(issue.priority.0.clamp(0, 4))) * PRIORITY_WEIGHT;
    let dependency_contribution = usize_to_i64(dependent_count)
        .saturating_mul(DEPENDENT_WEIGHT)
        .min(MAX_DEPENDENT_CONTRIBUTION);
    let is_stale = stale_claim.is_stale;
    let stale_contribution = if is_stale { 4 } else { 0 };
    let (fairness_contribution, fairness_reason) = match issue.assignee.as_deref() {
        None | Some("") => (3, "unassigned work is easiest to allocate fairly"),
        Some(_) if is_stale => (1, "assigned work appears stale enough to revisit"),
        Some(_) => (-2, "freshly assigned work should not attract new agents"),
    };
    let domain_contribution = (6_i64 / usize_to_i64(domain_count).max(1)).max(1);

    let score = priority_contribution
        .saturating_add(dependency_contribution)
        .saturating_add(stale_contribution)
        .saturating_add(fairness_contribution)
        .saturating_add(domain_contribution);

    let issue_id = issue.id.clone();
    let issue_title = issue.title.clone();
    let ready_issue = ReadyIssue::from(issue);
    let evidence = SchedulerEvidence {
        priority: PriorityEvidence {
            value: ready_issue.priority.0,
            contribution: priority_contribution,
        },
        dependency_impact: DependencyImpactEvidence {
            dependency_count,
            dependent_count,
            contribution: dependency_contribution,
        },
        stale_claim: StaleClaimEvidence {
            contribution: stale_contribution,
            ..stale_claim
        },
        fairness: FairnessEvidence {
            unassigned: ready_issue.assignee.is_none(),
            contribution: fairness_contribution,
            reason: fairness_reason,
        },
        domain_contention: DomainContentionEvidence {
            domain: domain.clone(),
            labels,
            candidate_count_in_domain: domain_count,
            contribution: domain_contribution,
        },
    };
    let rationale = vec![
        format!(
            "{issue_id} keeps fallback order {fallback_rank} but scores {score} after evidence weighting"
        ),
        format!(
            "priority {} contributes {priority_contribution}; {dependent_count} dependent issue(s) contribute {dependency_contribution}",
            ready_issue.priority
        ),
        format!(
            "domain {domain} has {domain_count} ready candidate(s), title: {}",
            sanitize_terminal_inline(&issue_title)
        ),
        scheduler_coordination_rationale(
            evidence.stale_claim.classification,
            evidence.stale_claim.recommended_action,
        )
        .to_string(),
    ];

    ScoredCandidate {
        fallback_rank,
        score,
        issue: ready_issue,
        evidence,
        rationale,
    }
}

fn count_candidate_domains(
    issues: &[crate::model::Issue],
    labels_by_issue: &HashMap<String, Vec<String>>,
) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for issue in issues {
        let labels = labels_by_issue.get(&issue.id).cloned().unwrap_or_default();
        let domain = primary_domain(issue, &labels);
        *counts.entry(domain).or_insert(0) += 1;
    }
    counts
}

fn primary_domain(issue: &crate::model::Issue, labels: &[String]) -> String {
    labels
        .first()
        .cloned()
        .unwrap_or_else(|| format!("type:{}", issue.issue_type))
}

fn scheduler_stale_claim_evidence(
    issue: &crate::model::Issue,
    now: &DateTime<Utc>,
    updated_age_minutes: i64,
    stale_threshold_minutes: i64,
) -> StaleClaimEvidence {
    let claim_assessment = assess_claim_with_policy(
        ClaimAssessmentInput {
            assignee: issue.assignee.clone(),
            updated_at: issue.updated_at,
            now: *now,
            owner_kind: ClaimOwnerKind::SwarmAgent,
            reservation: ReservationEvidence::NoSnapshot,
        },
        scheduler_claim_policy(stale_threshold_minutes),
    );
    let is_stale = scheduler_stale_claim_is_stale(claim_assessment.classification);

    StaleClaimEvidence {
        assignee: claim_assessment.assignee,
        owner_kind: claim_assessment.owner_kind,
        updated_age_minutes,
        stale_threshold_minutes: claim_assessment.stale_threshold_minutes,
        abandoned_threshold_minutes: claim_assessment.abandoned_threshold_minutes,
        classification: claim_assessment.classification,
        recommended_action: claim_assessment.recommended_action,
        reservation_status: "no_snapshot",
        is_stale,
        contribution: 0,
        coordination_status_hint: scheduler_coordination_status_hint(
            claim_assessment.recommended_action,
        ),
    }
}

const fn scheduler_claim_policy(stale_threshold_minutes: i64) -> ClaimAssessmentPolicy {
    ClaimAssessmentPolicy {
        stale_candidate_after_minutes: stale_threshold_minutes,
        abandoned_likely_after_minutes: stale_threshold_minutes.saturating_mul(4),
    }
}

const fn scheduler_stale_claim_is_stale(classification: ClaimClassification) -> bool {
    matches!(
        classification,
        ClaimClassification::StaleCandidate
            | ClaimClassification::AbandonedLikely
            | ClaimClassification::NoMailSnapshot
            | ClaimClassification::Ambiguous
    )
}

const fn scheduler_coordination_status_hint(
    recommended_action: RecommendedAction,
) -> Option<&'static str> {
    match recommended_action {
        RecommendedAction::InspectMail => Some(
            "run br coordination status with Agent Mail snapshots before treating this claim as abandoned",
        ),
        RecommendedAction::AskOwner => {
            Some("ask the assignee or operator before changing ownership")
        }
        RecommendedAction::ReclaimCandidate => {
            Some("review br coordination status suggested_commands before reclaiming")
        }
        RecommendedAction::LeaveActive => Some("leave active coordination evidence alone"),
        RecommendedAction::Observe => None,
    }
}

const fn scheduler_coordination_rationale(
    classification: ClaimClassification,
    recommended_action: RecommendedAction,
) -> &'static str {
    match (classification, recommended_action) {
        (ClaimClassification::Unassigned, _) => {
            "coordination policy classifies this as unassigned; no reclaim evidence is needed"
        }
        (ClaimClassification::Fresh, _) => {
            "coordination policy classifies this assignment as fresh"
        }
        (ClaimClassification::NoMailSnapshot, RecommendedAction::InspectMail) => {
            "coordination policy has no Agent Mail snapshot, so this is inspection evidence, not abandonment proof"
        }
        (_, RecommendedAction::InspectMail) => {
            "coordination policy needs Agent Mail evidence before ownership changes"
        }
        (_, RecommendedAction::AskOwner) => {
            "coordination policy requires human or owner confirmation before ownership changes"
        }
        (_, RecommendedAction::ReclaimCandidate) => {
            "coordination policy allows only the documented advisory reclaim sequence"
        }
        (_, RecommendedAction::LeaveActive) => "coordination policy found active work evidence",
        (_, RecommendedAction::Observe) => "coordination policy recommends observation",
    }
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn stale_threshold_minutes(hours: i64) -> Result<i64> {
    if hours < 0 {
        return Err(BeadsError::validation(
            "stale_claim_hours",
            "must be greater than or equal to 0",
        ));
    }

    Ok(hours.saturating_mul(60))
}

fn print_scheduler_text(output: &SchedulerOutput) {
    println!(
        "Scheduler recommendations ({} of {} ready candidates):",
        output.returned_count, output.candidate_count
    );
    for recommendation in &output.recommendations {
        println!(
            "{}. score {} [{}] {}: {}",
            recommendation.rank,
            recommendation.score,
            recommendation.issue.priority,
            recommendation.issue.id,
            sanitize_terminal_inline(&recommendation.issue.title)
        );
        println!(
            "   priority {:+}, dependents {:+}, stale {:+}, fairness {:+}, domain {:+}",
            recommendation.evidence.priority.contribution,
            recommendation.evidence.dependency_impact.contribution,
            recommendation.evidence.stale_claim.contribution,
            recommendation.evidence.fairness.contribution,
            recommendation.evidence.domain_contention.contribution
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        primary_domain, project_scheduler_relation_metadata, should_refill_scheduler_candidates,
        stale_threshold_minutes, usize_to_i64,
    };
    use crate::model::{Issue, IssueType};
    use crate::storage::sqlite::ListRelationMetadata;
    use std::collections::HashMap;

    #[test]
    fn primary_domain_prefers_first_sorted_label() {
        let issue = Issue {
            issue_type: IssueType::Task,
            ..Issue::default()
        };

        assert_eq!(
            primary_domain(&issue, &["backend".to_string(), "api".to_string()]),
            "backend"
        );
    }

    #[test]
    fn primary_domain_falls_back_to_issue_type() {
        let issue = Issue {
            issue_type: IssueType::Feature,
            ..Issue::default()
        };

        assert_eq!(primary_domain(&issue, &[]), "type:feature");
    }

    #[test]
    fn usize_to_i64_saturates_on_overflow() {
        assert_eq!(usize_to_i64(42), 42);
    }

    #[test]
    fn scheduler_relation_projection_keeps_candidate_metadata() {
        let issue_ids = vec!["bd-a".to_string(), "bd-b".to_string()];
        let mut relation_metadata = HashMap::new();
        relation_metadata.insert(
            "bd-a".to_string(),
            ListRelationMetadata {
                labels: vec!["scheduler".to_string()],
                dependency_count: 2,
                dependent_count: 3,
            },
        );
        relation_metadata.insert(
            "bd-other".to_string(),
            ListRelationMetadata {
                labels: vec!["ignored".to_string()],
                dependency_count: 1,
                dependent_count: 1,
            },
        );

        let (labels, dependency_counts, dependent_counts) =
            project_scheduler_relation_metadata(&issue_ids, &relation_metadata);

        assert_eq!(labels["bd-a"], ["scheduler"]);
        assert!(!labels.contains_key("bd-b"));
        assert_eq!(dependency_counts.get("bd-a"), Some(&2));
        assert_eq!(dependent_counts.get("bd-a"), Some(&3));
        assert!(!dependency_counts.contains_key("bd-other"));
        assert!(!dependent_counts.contains_key("bd-other"));
    }

    #[test]
    fn scheduler_refills_only_when_limited_prefix_loses_external_blocker() {
        let mut first = Issue {
            id: "bd-first".to_string(),
            ..Issue::default()
        };
        let second = Issue {
            id: "bd-second".to_string(),
            ..Issue::default()
        };
        let issues = vec![first.clone(), second.clone()];

        assert!(!should_refill_scheduler_candidates(
            Some(2),
            &issues,
            &HashMap::new()
        ));

        let outside_blockers = HashMap::from([(
            "bd-outside-window".to_string(),
            vec!["external:extproj:auth".to_string()],
        )]);
        assert!(!should_refill_scheduler_candidates(
            Some(2),
            &issues,
            &outside_blockers
        ));

        let prefix_blockers = HashMap::from([(
            "bd-first".to_string(),
            vec!["external:extproj:auth".to_string()],
        )]);
        assert!(should_refill_scheduler_candidates(
            Some(2),
            &issues,
            &prefix_blockers
        ));

        first.id = "bd-unlimited".to_string();
        assert!(!should_refill_scheduler_candidates(
            None,
            &[first],
            &prefix_blockers
        ));
    }

    #[test]
    fn stale_threshold_minutes_rejects_negative_hours() {
        let err = stale_threshold_minutes(-1).expect_err("negative stale age should fail");

        assert!(err.to_string().contains("stale_claim_hours"));
    }

    #[test]
    fn stale_threshold_minutes_saturates_large_values() {
        assert_eq!(stale_threshold_minutes(2).unwrap(), 120);
        assert_eq!(stale_threshold_minutes(i64::MAX).unwrap(), i64::MAX);
    }
}
