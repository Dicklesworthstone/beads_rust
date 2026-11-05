//! Guarded recovery of dependency-unblocked work (GitHub #595).
//!
//! The graph detector identifies candidates, never intent. A blocked status is
//! changed only with current, explicit dependency-only provenance or exact-ID
//! operator authorization. Preview and application share this analyzer; the
//! latter repeats it inside the ordinary atomic update transaction.

use super::{
    CapacityActingContext, CapacityBatchTransition, Connection, EventAttribution, IssueUpdate,
    MutationContext, Row, SqliteStorage, parse_datetime_value,
};
use crate::close_policy::{CapacityPolicy, Workflow, WorkflowCapacityWarning};
use crate::error::{BeadsError, Result};
use crate::model::{Dependency, DependencyType, EventType, Issue, IssueType, Status};
use chrono::Utc;
use fsqlite_types::SqliteValue;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

const BLOCK_PROVENANCE_SCHEMA: &str = "br.dependency_block.v1";
const BLOCK_PROVENANCE_EVENT: &str = "dependency_blocked";
pub(super) const DEPENDENCY_UNBLOCK_OPERATION: &str = "dependency_unblock";
const PREVIEW_PLAN_TOKEN: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// An exact repair request. IDs and authorizations are full issue IDs, not
/// prefixes. `context` binds the caller's workspace, JSONL and policy inputs.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DependencyUnblockRequest {
    pub ids: Vec<String>,
    pub authorized_ids: Vec<String>,
    pub target_status: Option<String>,
    pub context: String,
}

/// One inspected record, including the reason it will be changed or preserved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DependencyUnblockCandidate {
    pub id: String,
    pub eligible: bool,
    pub reason: String,
    pub detail: String,
    pub provenance: String,
    pub from_status: String,
    pub to_status: Option<String>,
    pub blockers: Vec<String>,
}

/// A non-mutating proposal. The token is required again when applying it.
#[derive(Debug, Clone, Serialize)]
pub struct DependencyUnblockPlan {
    pub plan_token: String,
    pub candidates: Vec<DependencyUnblockCandidate>,
    pub planned_ids: Vec<String>,
    pub skipped_ids: Vec<String>,
    pub capacity_warnings: Vec<WorkflowCapacityWarning>,
}

/// Committed changes, separately from records deliberately preserved.
#[derive(Debug, Clone, Serialize)]
pub struct DependencyUnblockReport {
    pub plan_token: String,
    pub changed: Vec<String>,
    pub skipped: Vec<String>,
    pub candidates: Vec<DependencyUnblockCandidate>,
    /// Primary changes are committed, but connection/cache maintenance needs
    /// attention before JSONL publication or another mutation is attempted.
    pub maintenance_error: Option<String>,
    pub capacity_warnings: Vec<WorkflowCapacityWarning>,
}

pub(super) struct DependencyUnblockGuard<'a> {
    request: &'a DependencyUnblockRequest,
    expected_token: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct DependencyBlockProof {
    schema: String,
    previous_status: String,
    blockers: Vec<String>,
    updated_at: String,
    issue_fingerprint: String,
}

/// Classification shared by the JSONL doctor detector and transactional repair.
pub(crate) struct FullyUnblockedIssue {
    pub(crate) id: String,
    pub(crate) excluded_reasons: Vec<&'static str>,
    pub(crate) stale_blocked: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockerFate {
    Live,
    Satisfied,
    Dangling,
    NonGating,
}

pub(crate) fn blocker_fate(target: &str, issue_by_id: &HashMap<&str, &Issue>) -> BlockerFate {
    if target.starts_with("external:") {
        return BlockerFate::Live;
    }
    match issue_by_id.get(target) {
        Some(issue) if issue.status.is_terminal() => BlockerFate::Satisfied,
        Some(issue) if issue.is_template => BlockerFate::NonGating,
        Some(_) => BlockerFate::Live,
        None => BlockerFate::Dangling,
    }
}

pub(crate) fn forward_blocking_targets(issue: &Issue) -> Vec<&str> {
    issue
        .dependencies
        .iter()
        .filter(|dependency| {
            matches!(
                dependency.dep_type,
                DependencyType::Blocks
                    | DependencyType::ConditionalBlocks
                    | DependencyType::WaitsFor
            ) && dependency.depends_on_id != issue.id
        })
        .map(|dependency| dependency.depends_on_id.as_str())
        .collect()
}

/// Direct blockers propagate down the hierarchy before epic child-rollup gates
/// are added. Otherwise an epic waiting on children would block those children.
fn hierarchy_gated_issue_ids<'a>(
    issues: &'a [Issue],
    issue_by_id: &HashMap<&str, &Issue>,
) -> HashSet<&'a str> {
    let mut children_by_parent: HashMap<&str, Vec<&str>> = HashMap::new();
    for issue in issues {
        for dependency in &issue.dependencies {
            if dependency.dep_type == DependencyType::ParentChild
                && !issue.id.starts_with("external:")
                && !dependency.depends_on_id.starts_with("external:")
            {
                children_by_parent
                    .entry(dependency.depends_on_id.as_str())
                    .or_default()
                    .push(issue.id.as_str());
            }
        }
    }

    let mut gated: HashSet<&str> = issues
        .iter()
        .filter(|issue| {
            forward_blocking_targets(issue)
                .iter()
                .any(|target| blocker_fate(target, issue_by_id) == BlockerFate::Live)
        })
        .map(|issue| issue.id.as_str())
        .collect();
    let mut queue: Vec<&str> = gated.iter().copied().collect();
    while let Some(parent_id) = queue.pop() {
        if let Some(children) = children_by_parent.get(parent_id) {
            for &child_id in children {
                if gated.insert(child_id) {
                    queue.push(child_id);
                }
            }
        }
    }
    for (parent_id, children) in &children_by_parent {
        if issue_by_id
            .get(parent_id)
            .is_some_and(|parent| parent.issue_type == IssueType::Epic)
            && children.iter().any(|child_id| {
                issue_by_id
                    .get(child_id)
                    .is_some_and(|child| !child.status.is_terminal() && !child.is_template)
            })
        {
            gated.insert(*parent_id);
        }
    }
    gated
}

fn classify_fully_unblocked(issue: &Issue, hierarchy_gated: bool) -> FullyUnblockedIssue {
    let mut excluded_reasons = Vec::new();
    match &issue.status {
        Status::Open | Status::Blocked | Status::Closed | Status::Tombstone | Status::Draft => {}
        Status::InProgress => excluded_reasons.push("claimed (status in_progress)"),
        Status::Deferred => excluded_reasons.push("status deferred"),
        Status::Pinned => excluded_reasons.push("status pinned"),
        Status::Custom(_) => excluded_reasons.push("custom status"),
    }
    if issue.defer_until.is_some_and(|until| until > Utc::now()) {
        excluded_reasons.push("defer_until in the future");
    }
    if issue.pinned {
        excluded_reasons.push("pinned");
    }
    if issue.ephemeral {
        excluded_reasons.push("ephemeral");
    }
    if issue.id.contains("-wisp-") {
        excluded_reasons.push("wisp");
    }
    if issue.is_template {
        excluded_reasons.push("template");
    }
    if hierarchy_gated {
        excluded_reasons.push("blocked through the parent-child hierarchy");
    }
    FullyUnblockedIssue {
        id: issue.id.clone(),
        excluded_reasons,
        stale_blocked: issue.status == Status::Blocked && !hierarchy_gated && !issue.is_template,
    }
}

/// The existing `stale_blocked` partition, independent of storage/cache state.
/// External references remain blocking because this detector cannot prove
/// their completion. Missing local targets retain the doctor's dangling-edge
/// classification; repair requires explicit authorization for those records.
pub(crate) fn fully_unblocked_issues(issues: &[Issue]) -> Vec<FullyUnblockedIssue> {
    let issue_by_id: HashMap<&str, &Issue> = issues
        .iter()
        .map(|issue| (issue.id.as_str(), issue))
        .collect();
    let hierarchy_gated = hierarchy_gated_issue_ids(issues, &issue_by_id);
    issues
        .iter()
        .filter(|issue| !issue.status.is_terminal() && !issue.status.is_draft())
        .filter(|issue| {
            let targets = forward_blocking_targets(issue);
            !targets.is_empty()
                && targets
                    .iter()
                    .all(|target| blocker_fate(target, &issue_by_id) != BlockerFate::Live)
        })
        .map(|issue| classify_fully_unblocked(issue, hierarchy_gated.contains(issue.id.as_str())))
        .collect()
}

impl SqliteStorage {
    /// Inspect dependency-unblocked work without changing tracker state.
    ///
    /// # Errors
    /// Returns an error for malformed requests, unavailable storage or invalid
    /// workflow configuration. Ineligible records remain visible in the plan.
    pub fn preview_dependency_unblock(
        &self,
        request: &DependencyUnblockRequest,
        actor: &str,
    ) -> Result<DependencyUnblockPlan> {
        self.with_read_transaction(|storage| {
            Self::dependency_unblock_plan_in_tx(
                &storage.conn,
                request,
                actor,
                &storage.workflow_transition_policy,
                &storage.workflow_capacity_policy,
                &storage.pending_event_attribution_for_review(),
            )
        })
    }

    /// Apply exactly the reviewed eligible batch through the normal workflow,
    /// capacity, audit and cache mutation path. Recheck the entire witness
    /// inside `BEGIN IMMEDIATE`, including when the proposed batch is empty.
    ///
    /// # Errors
    /// A stale preview, failed precondition or pre-commit interruption rejects
    /// the entire batch. Errors certifying lost database authority after
    /// COMMIT retain their committed-state classification; callers must not
    /// interpret those errors as rollback or automatically retry the repair.
    /// Known post-commit maintenance failures are returned in the report so
    /// the exact committed IDs remain available to callers.
    pub fn apply_dependency_unblock(
        &mut self,
        request: &DependencyUnblockRequest,
        expected_token: &str,
        actor: &str,
    ) -> Result<DependencyUnblockReport> {
        let plan = self.preview_dependency_unblock(request, actor)?;
        require_plan_token(&plan, expected_token)?;
        let updates = dependency_unblock_updates(&plan, actor)?;
        let guard = DependencyUnblockGuard {
            request,
            expected_token,
        };
        let maintenance_error = match self.update_issues_atomically_guarded(
            &updates,
            actor,
            Some(&guard),
        ) {
            Ok(_) => None,
            Err(error @ BeadsError::CommittedMaintenanceFailure { .. }) => Some(error.to_string()),
            Err(error) if error.primary_mutation_committed() => {
                return Err(BeadsError::WithContext {
                    context: format!(
                        "dependency repair for [{}] with review {} committed but its current database state cannot be certified; inspect before retrying",
                        plan.planned_ids.join(", "),
                        plan.plan_token,
                    ),
                    source: Box::new(error),
                });
            }
            Err(error) => return Err(error),
        };
        Ok(DependencyUnblockReport {
            plan_token: plan.plan_token,
            changed: plan.planned_ids,
            skipped: plan.skipped_ids,
            candidates: plan.candidates,
            maintenance_error,
            capacity_warnings: self.take_capacity_warnings(),
        })
    }

    pub(super) fn validate_dependency_unblock_guard_in_tx(
        conn: &Connection,
        guard: &DependencyUnblockGuard<'_>,
        actor: &str,
        workflow: &Workflow,
        capacity: &CapacityPolicy,
        attribution: &EventAttribution,
    ) -> Result<()> {
        check_repair_interruption()?;
        let plan = Self::dependency_unblock_plan_in_tx(
            conn,
            guard.request,
            actor,
            workflow,
            capacity,
            attribution,
        )?;
        require_plan_token(&plan, guard.expected_token)
    }

    #[allow(clippy::too_many_lines)]
    fn dependency_unblock_plan_in_tx(
        conn: &Connection,
        request: &DependencyUnblockRequest,
        actor: &str,
        workflow: &Workflow,
        capacity: &CapacityPolicy,
        attribution: &EventAttribution,
    ) -> Result<DependencyUnblockPlan> {
        if actor.trim().is_empty() {
            return Err(BeadsError::validation(
                "actor",
                "dependency repair requires a named actor",
            ));
        }
        workflow.validate_ready_status_group()?;
        let ready_statuses = workflow.ready_status_group();
        let request = normalized_request(request, &ready_statuses)?;
        let issues = load_dependency_graph(conn)?;
        let issue_by_id: HashMap<&str, &Issue> = issues
            .iter()
            .map(|issue| (issue.id.as_str(), issue))
            .collect();
        let selected = selected_ids(&request, &issues, &issue_by_id)?;
        let classifications = fully_unblocked_issues(&issues);
        let classification_by_id: HashMap<&str, &FullyUnblockedIssue> = classifications
            .iter()
            .map(|classification| (classification.id.as_str(), classification))
            .collect();
        let mut candidates = Vec::with_capacity(selected.len());
        for id in &selected {
            let issue = issue_by_id[id.as_str()];
            let proof = current_dependency_block_proof(conn, issue)?;
            let authorized = request.authorized_ids.contains(id);
            let mut candidate = candidate_for_issue(
                issue,
                &issue_by_id,
                classification_by_id.get(id.as_str()).copied(),
                proof.as_ref(),
                &request,
                &ready_statuses,
                actor,
                authorized,
            );
            if candidate.eligible {
                let preflight =
                    candidate_update(&candidate, actor, PREVIEW_PLAN_TOKEN).and_then(|update| {
                        Self::enforce_workflow_transition_batch_in_tx(
                            conn,
                            workflow,
                            &[(id.clone(), update)],
                        )
                    });
                if let Err(error) = preflight {
                    candidate.eligible = false;
                    candidate.reason = "workflow_policy".to_string();
                    candidate.detail = error.to_string();
                }
            }
            candidates.push(candidate);
        }

        let transitions = candidates
            .iter()
            .filter(|candidate| candidate.eligible)
            .map(|candidate| {
                let issue = issue_by_id[candidate.id.as_str()];
                CapacityBatchTransition {
                    issue_id: candidate.id.clone(),
                    from: Some("blocked".to_string()),
                    to: candidate.to_status.clone().unwrap_or_default(),
                    issue_type: None,
                    current_assignee: issue.assignee.clone(),
                    prospective_assignee: issue.assignee.clone(),
                    ownership_handoff: false,
                }
            })
            .collect::<Vec<_>>();
        let mut capacity_warnings = Vec::new();
        if capacity.is_active() && !transitions.is_empty() {
            // The ordinary enforcement loader audits expired exemptions. A
            // preview must instead use the observational loader, then the same
            // policy calculation with that index.
            let exemptions = Self::load_capacity_exemption_index_read_only(conn, capacity)?;
            let acting = CapacityActingContext::new(actor, attribution);
            match Self::evaluate_workflow_capacity_with_exemptions(
                conn,
                capacity,
                &transitions,
                &acting,
                &exemptions,
            ) {
                Ok(warnings) => capacity_warnings = warnings,
                Err(error) => {
                    for candidate in candidates.iter_mut().filter(|candidate| candidate.eligible) {
                        candidate.eligible = false;
                        candidate.reason = "capacity_policy".to_string();
                        candidate.detail = error.to_string();
                    }
                }
            }
        }

        let state = tables_digest(
            conn,
            &[
                "issues",
                "dependencies",
                "labels",
                "comments",
                "events",
                "config",
                "gate_results",
                "gate_result_history",
                "capacity_exemptions",
                "capacity_exemption_history",
                "capacity_occupancy",
            ],
        )?;
        let witness = serde_json::to_vec(&(
            "br.dependency_unblock.v1",
            state,
            &request,
            actor,
            workflow,
            capacity,
            attribution,
            &candidates,
            &capacity_warnings,
        ))?;
        let plan_token = format!(
            "sha256:{}",
            crate::util::hex_encode(&Sha256::digest(witness))
        );
        let planned_ids = candidates
            .iter()
            .filter(|candidate| candidate.eligible)
            .map(|candidate| candidate.id.clone())
            .collect();
        let skipped_ids = candidates
            .iter()
            .filter(|candidate| !candidate.eligible)
            .map(|candidate| candidate.id.clone())
            .collect();
        Ok(DependencyUnblockPlan {
            plan_token,
            candidates,
            planned_ids,
            skipped_ids,
            capacity_warnings,
        })
    }

    /// Validate explicit dependency-only intent before touching any row.
    #[allow(clippy::too_many_lines)]
    pub(super) fn prepare_dependency_block_proofs_in_tx(
        conn: &Connection,
        updates: &[(String, IssueUpdate)],
        actor: &str,
        workflow: &Workflow,
    ) -> Result<BTreeMap<String, DependencyBlockProof>> {
        let mut proofs = BTreeMap::new();
        if !updates
            .iter()
            .any(|(_, update)| update.blocked_by_dependencies)
        {
            return Ok(proofs);
        }
        workflow.validate_ready_status_group()?;
        let issues = load_dependency_graph(conn)?;
        let by_id = issues
            .iter()
            .map(|issue| (issue.id.as_str(), issue))
            .collect::<HashMap<_, _>>();
        let ready_statuses = workflow.ready_status_group();
        for (id, update) in updates
            .iter()
            .filter(|(_, update)| update.blocked_by_dependencies)
        {
            let issue = by_id
                .get(id.as_str())
                .ok_or_else(|| BeadsError::IssueNotFound { id: id.clone() })?;
            if update.status != Some(Status::Blocked)
                || !ready_statuses
                    .iter()
                    .any(|status| status == issue.status.as_str())
                || !actionable_repair_status(issue.status.as_str())
            {
                return Err(BeadsError::validation(
                    "blocked_by_dependencies",
                    format!(
                        "issue {id}: --blocked-by-dependencies requires a real transition from a ready status to blocked"
                    ),
                ));
            }
            let assignee = update.assignee.as_ref().unwrap_or(&issue.assignee);
            let defer_until = update.defer_until.as_ref().unwrap_or(&issue.defer_until);
            if actor.trim().is_empty()
                || owned_by_other(assignee.as_deref(), actor)
                || issue.pinned
                || issue.ephemeral
                || issue.is_template
                || issue.id.contains("-wisp-")
                || defer_until.is_some_and(|until| until > Utc::now())
                || update.reclaim.is_some()
                || update.expect_unassigned
            {
                return Err(BeadsError::validation(
                    "blocked_by_dependencies",
                    format!(
                        "issue {id}: dependency-only provenance cannot override ownership, claim, deferral or ready-work exclusions"
                    ),
                ));
            }
            let mut blockers = forward_blocking_targets(issue)
                .into_iter()
                .filter(|target| blocker_fate(target, &by_id) == BlockerFate::Live)
                .map(str::to_string)
                .collect::<Vec<_>>();
            blockers.sort();
            blockers.dedup();
            if blockers.is_empty() {
                return Err(BeadsError::validation(
                    "blocked_by_dependencies",
                    format!("issue {id}: no live forward dependency blocker exists to record"),
                ));
            }
            proofs.insert(
                id.clone(),
                DependencyBlockProof {
                    schema: BLOCK_PROVENANCE_SCHEMA.to_string(),
                    previous_status: issue.status.as_str().to_string(),
                    blockers,
                    updated_at: String::new(),
                    issue_fingerprint: String::new(),
                },
            );
        }
        Ok(proofs)
    }

    pub(super) fn record_dependency_block_proof_in_tx(
        conn: &Connection,
        ctx: &mut MutationContext,
        id: &str,
        mut proof: DependencyBlockProof,
    ) -> Result<()> {
        let issue = Self::get_issue_from_conn(conn, id)?
            .ok_or_else(|| BeadsError::IssueNotFound { id: id.to_string() })?;
        proof.updated_at = issue.updated_at.to_rfc3339();
        proof.issue_fingerprint = issue_fingerprint(conn, id)?;
        ctx.record_event(
            EventType::Custom(BLOCK_PROVENANCE_EVENT.to_string()),
            id,
            Some(serde_json::to_string(&proof)?),
        );
        Ok(())
    }
}

fn normalized_request(
    request: &DependencyUnblockRequest,
    ready_statuses: &[String],
) -> Result<DependencyUnblockRequest> {
    let mut request = request.clone();
    request.ids.sort();
    request.ids.dedup();
    request.authorized_ids.sort();
    request.authorized_ids.dedup();
    if let Some(target) = request.target_status.as_mut() {
        *target = target.trim().to_lowercase();
        if !ready_statuses.contains(target) || !actionable_repair_status(target) {
            return Err(BeadsError::validation(
                "to_status",
                format!(
                    "repair target '{target}' must be an actionable status in workflow.status_groups.ready"
                ),
            ));
        }
    }
    Ok(request)
}

fn selected_ids(
    request: &DependencyUnblockRequest,
    issues: &[Issue],
    by_id: &HashMap<&str, &Issue>,
) -> Result<BTreeSet<String>> {
    let selected: BTreeSet<String> = if request.ids.is_empty() {
        issues
            .iter()
            .filter(|issue| issue.status == Status::Blocked)
            .map(|issue| issue.id.clone())
            .collect()
    } else {
        request.ids.iter().cloned().collect()
    };
    for id in selected.iter().chain(request.authorized_ids.iter()) {
        if !by_id.contains_key(id.as_str()) {
            return Err(BeadsError::validation(
                "issue_ids",
                format!("dependency repair requires exact existing IDs; '{id}' was not found"),
            ));
        }
    }
    for id in &request.authorized_ids {
        if !selected.contains(id) {
            return Err(BeadsError::validation(
                "authorize",
                format!("authorized issue '{id}' is outside the selected repair records"),
            ));
        }
    }
    Ok(selected)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn candidate_for_issue(
    issue: &Issue,
    by_id: &HashMap<&str, &Issue>,
    classification: Option<&FullyUnblockedIssue>,
    proof: Option<&DependencyBlockProof>,
    request: &DependencyUnblockRequest,
    ready_statuses: &[String],
    actor: &str,
    authorized: bool,
) -> DependencyUnblockCandidate {
    let mut blockers = forward_blocking_targets(issue)
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    blockers.sort();
    blockers.dedup();
    let dangling = blockers
        .iter()
        .any(|target| blocker_fate(target, by_id) == BlockerFate::Dangling);
    let trustworthy_proof = proof.filter(|_| !dangling);
    let target = request.target_status.clone().or_else(|| {
        trustworthy_proof
            .filter(|proof| ready_statuses.contains(&proof.previous_status))
            .map(|proof| proof.previous_status.clone())
            .or_else(|| {
                if ready_statuses.iter().any(|status| status == "open") {
                    Some("open".to_string())
                } else if ready_statuses.len() == 1 {
                    ready_statuses.first().cloned()
                } else {
                    None
                }
            })
    });
    let provenance = if authorized {
        "explicit_authorization"
    } else if trustworthy_proof.is_some() {
        "dependency_only"
    } else {
        "ambiguous"
    };
    let (reason, detail) = if issue.status != Status::Blocked {
        (
            "not_blocked",
            "The current status is not blocked".to_string(),
        )
    } else if blockers.is_empty() {
        (
            "no_dependency_history",
            "No forward blocking dependency exists; preserve manual or external blocks".to_string(),
        )
    } else if classification.is_none() {
        (
            "remaining_blockers",
            "A live or unresolved external dependency still blocks this issue".to_string(),
        )
    } else if classification.is_some_and(|entry| !entry.stale_blocked) {
        (
            "hierarchy_or_template",
            "A parent-child gate or template exclusion still applies".to_string(),
        )
    } else if let Some(entry) = classification.filter(|entry| !entry.excluded_reasons.is_empty()) {
        ("ready_exclusion", entry.excluded_reasons.join(", "))
    } else if owned_by_other(issue.assignee.as_deref(), actor) {
        (
            "ownership",
            format!(
                "Assigned to {}; repair never transfers another actor's work",
                issue.assignee.as_deref().unwrap_or_default()
            ),
        )
    } else if !authorized && trustworthy_proof.is_none() {
        ("authorization_required", "Dependency-only intent is unproven; preview again with --authorize and this exact ID if its manual/external block is resolved".to_string())
    } else if target
        .as_deref()
        .is_none_or(|status| !actionable_repair_status(status))
    {
        (
            "target_required",
            "Choose an actionable workflow ready status with --to-status".to_string(),
        )
    } else {
        (
            "eligible",
            "No dependency, hierarchy, ownership or ready-work exclusion remains".to_string(),
        )
    };
    DependencyUnblockCandidate {
        id: issue.id.clone(),
        eligible: reason == "eligible",
        reason: reason.to_string(),
        detail,
        provenance: provenance.to_string(),
        from_status: issue.status.as_str().to_string(),
        to_status: target,
        blockers,
    }
}

fn actionable_repair_status(status: &str) -> bool {
    !status.trim().is_empty()
        && matches!(
            status.parse::<Status>(),
            Ok(Status::Open | Status::Custom(_))
        )
}

fn owned_by_other(assignee: Option<&str>, actor: &str) -> bool {
    assignee
        .map(str::trim)
        .filter(|owner| !owner.is_empty())
        .is_some_and(|owner| owner != actor.trim())
}

fn candidate_update(
    candidate: &DependencyUnblockCandidate,
    actor: &str,
    token: &str,
) -> Result<IssueUpdate> {
    let status = candidate.to_status.as_deref().ok_or_else(|| {
        BeadsError::internal("eligible dependency repair candidate lacks a target status")
    })?;
    let comment = format!(
        "Dependency-unblocked by {actor}: no live blockers or parent-child gates remain. Dependency history: {}. Authorization: {}. Review: {token}.",
        candidate.blockers.join(", "),
        candidate.provenance,
    );
    // Preview and apply use the same validator as the actual insertion. In
    // particular, an invalid actor or NUL-bearing audit must not be advertised
    // as an eligible change that will inevitably fail during application.
    super::validate_new_comment(&candidate.id, actor, &comment)?;
    Ok(IssueUpdate {
        status: Some(status.parse()?),
        transition_comment: Some(comment),
        ..IssueUpdate::default()
    })
}

fn dependency_unblock_updates(
    plan: &DependencyUnblockPlan,
    actor: &str,
) -> Result<Vec<(String, IssueUpdate)>> {
    plan.candidates
        .iter()
        .filter(|candidate| candidate.eligible)
        .map(|candidate| {
            Ok((
                candidate.id.clone(),
                candidate_update(candidate, actor, &plan.plan_token)?,
            ))
        })
        .collect()
}

fn require_plan_token(plan: &DependencyUnblockPlan, expected: &str) -> Result<()> {
    if expected == plan.plan_token {
        return Ok(());
    }
    Err(BeadsError::validation_with_hint(
        "expect",
        "dependency repair preview is stale or does not match this request; no issues were changed",
        "Run the same doctor unblock preview again, inspect its candidates, and apply its new plan_token.",
    ))
}

fn current_dependency_block_proof(
    conn: &Connection,
    issue: &Issue,
) -> Result<Option<DependencyBlockProof>> {
    if issue.status != Status::Blocked {
        return Ok(None);
    }
    let rows = conn.query_with_params(
        "SELECT comment FROM events WHERE issue_id = ? AND event_type = ? ORDER BY id DESC LIMIT 1",
        &[
            SqliteValue::from(issue.id.as_str()),
            SqliteValue::from(BLOCK_PROVENANCE_EVENT),
        ],
    )?;
    let Some(raw) = rows
        .first()
        .and_then(|row| row.get(0))
        .and_then(SqliteValue::as_text)
    else {
        return Ok(None);
    };
    let Ok(proof) = serde_json::from_str::<DependencyBlockProof>(raw) else {
        return Ok(None);
    };
    if proof.schema != BLOCK_PROVENANCE_SCHEMA
        || proof.blockers.is_empty()
        || !actionable_repair_status(&proof.previous_status)
        || proof.updated_at != issue.updated_at.to_rfc3339()
        || proof.issue_fingerprint != issue_fingerprint(conn, &issue.id)?
    {
        return Ok(None);
    }
    Ok(Some(proof))
}

#[allow(clippy::too_many_lines)]
fn load_dependency_graph(conn: &Connection) -> Result<Vec<Issue>> {
    let rows = conn.query(
        "SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                status, priority, issue_type, assignee, owner, estimated_minutes,
                created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                due_at, defer_until, external_ref, source_system, source_repo,
                deleted_at, deleted_by, delete_reason, original_type, compaction_level,
                compacted_at, compacted_at_commit, original_size, sender, ephemeral,
                pinned, is_template, source_repo_path, agent_context, prerequisites
         FROM issues ORDER BY id",
    )?;
    // The ordinary decoder accepts legacy values by defaulting malformed
    // fields. Repair must not erase an ownership or ready-work exclusion
    // through such a default; use the existing lossless preservation decoder.
    let mut issues = rows
        .iter()
        .map(SqliteStorage::issue_from_row_for_preservation)
        .collect::<Result<Vec<_>>>()?;
    let issue_ids = checked_issue_ids(&issues)?;
    let mut dependencies: HashMap<String, Vec<Dependency>> = HashMap::new();
    let mut edge_keys = HashSet::new();
    for row in conn.query(
        "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id
         FROM dependencies ORDER BY issue_id, depends_on_id, type",
    )? {
        let text = |index, name| -> Result<&str> {
            row.get(index)
                .and_then(SqliteValue::as_text)
                .ok_or_else(|| {
                    BeadsError::validation("dependencies", format!("non-text dependency {name}"))
                })
        };
        let optional_text = |index, name| -> Result<Option<String>> {
            match row.get(index) {
                Some(SqliteValue::Null) => Ok(None),
                Some(SqliteValue::Text(value)) => Ok(Some(value.to_string())),
                _ => Err(BeadsError::validation(
                    "dependencies",
                    format!("non-text dependency {name}"),
                )),
            }
        };
        let dependency = Dependency {
            issue_id: text(0, "issue_id")?.to_string(),
            depends_on_id: text(1, "depends_on_id")?.to_string(),
            dep_type: text(2, "type")?.parse()?,
            created_at: parse_datetime_value(row.get(3))?,
            created_by: optional_text(4, "created_by")?,
            metadata: optional_text(5, "metadata")?,
            thread_id: optional_text(6, "thread_id")?,
        };
        if !issue_ids.contains(&dependency.issue_id)
            || dependency.depends_on_id.trim().is_empty()
            || dependency.depends_on_id.contains('\0')
            || dependency.depends_on_id.trim() != dependency.depends_on_id
        {
            return Err(BeadsError::validation(
                "dependencies",
                "dependency repair refuses an orphan source or malformed dependency target",
            ));
        }
        if !edge_keys.insert((
            dependency.issue_id.clone(),
            dependency.depends_on_id.clone(),
            dependency.dep_type.as_str().to_string(),
        )) {
            return Err(BeadsError::validation(
                "dependencies",
                "dependency repair refuses duplicate stored dependency edges",
            ));
        }
        dependencies
            .entry(dependency.issue_id.clone())
            .or_default()
            .push(dependency);
    }
    for issue in &mut issues {
        issue.dependencies = dependencies.remove(&issue.id).unwrap_or_default();
    }
    Ok(issues)
}

fn checked_issue_ids(issues: &[Issue]) -> Result<HashSet<String>> {
    let mut ids = HashSet::with_capacity(issues.len());
    for issue in issues {
        if !crate::validation::is_valid_id_format(&issue.id) {
            return Err(BeadsError::validation(
                "issue_ids",
                format!(
                    "dependency repair refuses malformed stored issue ID '{}'",
                    issue.id
                ),
            ));
        }
        if !ids.insert(issue.id.clone()) {
            return Err(BeadsError::validation(
                "issue_ids",
                format!(
                    "dependency repair refuses duplicate stored issue ID '{}'",
                    issue.id
                ),
            ));
        }
    }
    Ok(ids)
}

/// Preserve SQLite value types, float bits and row multiplicity in a canonical
/// row witness. Sorting makes the digest independent of query-plan row order.
fn rows_digest(rows: &[Row]) -> Result<String> {
    let mut encoded = rows
        .iter()
        .map(|row| {
            let values = row
                .values()
                .iter()
                .map(|value| match value {
                    SqliteValue::Null => serde_json::json!(["null"]),
                    SqliteValue::Integer(value) => serde_json::json!(["integer", value]),
                    SqliteValue::Float(value) => serde_json::json!(["float", value.to_bits()]),
                    SqliteValue::Text(value) => serde_json::json!(["text", value.as_ref()]),
                    SqliteValue::Blob(value) => {
                        serde_json::json!(["blob", crate::util::hex_encode(value)])
                    }
                })
                .collect::<Vec<_>>();
            serde_json::to_vec(&values).map_err(BeadsError::from)
        })
        .collect::<Result<Vec<_>>>()?;
    encoded.sort();
    Ok(crate::util::hex_encode(&Sha256::digest(
        serde_json::to_vec(&encoded)?,
    )))
}

fn tables_digest(conn: &Connection, tables: &[&str]) -> Result<String> {
    let mut fingerprints = BTreeMap::new();
    for table in tables {
        // All table names are fixed internal constants, never user input.
        let rows = conn.query(&format!("SELECT * FROM {table}"))?;
        fingerprints.insert(*table, rows_digest(&rows)?);
    }
    Ok(crate::util::hex_encode(&Sha256::digest(
        serde_json::to_vec(&fingerprints)?,
    )))
}

fn issue_fingerprint(conn: &Connection, id: &str) -> Result<String> {
    let mut fingerprints = BTreeMap::new();
    for (table, column) in [
        ("issues", "id"),
        ("dependencies", "issue_id"),
        ("labels", "issue_id"),
        ("comments", "issue_id"),
    ] {
        let rows = conn.query_with_params(
            &format!("SELECT * FROM {table} WHERE {column} = ?"),
            &[SqliteValue::from(id)],
        )?;
        fingerprints.insert(table, rows_digest(&rows)?);
    }
    Ok(crate::util::hex_encode(&Sha256::digest(
        serde_json::to_vec(&fingerprints)?,
    )))
}

pub(super) fn check_repair_interruption() -> Result<()> {
    if crate::shutdown::is_requested() {
        return Err(BeadsError::validation(
            "dependency_repair",
            "repair interrupted before commit; the entire repair transaction was rolled back",
        ));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static FAIL_REPAIR_AFTER_UPDATES: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static FAIL_REPAIR_BEFORE_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_REPAIR_AFTER_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) fn committed_maintenance_result<T>(result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        if error.primary_mutation_committed() {
            return error;
        }
        BeadsError::CommittedMaintenanceFailure {
            operation: DEPENDENCY_UNBLOCK_OPERATION.to_string(),
            source: Box::new(error),
        }
    })
}

#[cfg(test)]
pub(super) fn after_dependency_unblock_commit() -> Result<()> {
    if FAIL_REPAIR_AFTER_COMMIT.with(std::cell::Cell::get) {
        return Err(BeadsError::Config(
            "injected post-commit dependency repair maintenance failure".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn before_dependency_unblock_commit() -> Result<()> {
    #[cfg(test)]
    if FAIL_REPAIR_BEFORE_COMMIT.with(std::cell::Cell::get) {
        return Err(BeadsError::validation(
            "dependency_repair",
            "injected interruption after audit, dirty and cache writes",
        ));
    }
    check_repair_interruption()
}

pub(super) fn after_dependency_unblock_update(applied: usize) -> Result<()> {
    #[cfg(test)]
    if FAIL_REPAIR_AFTER_UPDATES.with(|remaining| remaining.get() == Some(applied)) {
        return Err(BeadsError::validation(
            "dependency_repair",
            "injected interruption after a real update",
        ));
    }
    #[cfg(not(test))]
    let _ = applied;
    check_repair_interruption()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::close_policy::CapacityLimit;
    use crate::storage::{ReadyFilters, ReadySortPolicy};

    const ACTOR: &str = "repair-operator";
    const BLOCKER: &str = "bd-repair-blocker";

    fn issue(id: &str) -> Issue {
        let now = Utc::now();
        Issue {
            id: id.to_string(),
            title: format!("Repair fixture {id}"),
            created_at: now,
            updated_at: now,
            ..Issue::default()
        }
    }

    fn unblocked_fixture(dependents: &[&str]) -> SqliteStorage {
        let mut storage = SqliteStorage::open_memory().unwrap();
        storage.create_issue(&issue(BLOCKER), ACTOR).unwrap();
        for id in dependents {
            storage.create_issue(&issue(id), ACTOR).unwrap();
            storage
                .add_dependency(id, BLOCKER, "blocks", ACTOR)
                .unwrap();
            storage
                .update_issue(
                    id,
                    &IssueUpdate {
                        status: Some(Status::Blocked),
                        blocked_by_dependencies: true,
                        ..IssueUpdate::default()
                    },
                    ACTOR,
                )
                .unwrap();
        }
        storage
            .update_issue(
                BLOCKER,
                &IssueUpdate {
                    status: Some(Status::Closed),
                    ..IssueUpdate::default()
                },
                ACTOR,
            )
            .unwrap();
        storage
    }

    fn logical_snapshot(storage: &SqliteStorage) -> String {
        tables_digest(
            &storage.conn,
            &[
                "issues",
                "dependencies",
                "labels",
                "comments",
                "events",
                "dirty_issues",
                "blocked_issues_cache",
                "metadata",
                "close_metadata",
                "capacity_exemptions",
                "capacity_exemption_history",
                "capacity_occupancy",
            ],
        )
        .unwrap()
    }

    fn ready_ids(storage: &SqliteStorage) -> BTreeSet<String> {
        storage
            .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::default())
            .unwrap()
            .into_iter()
            .map(|issue| issue.id)
            .collect()
    }

    fn open_capacity(hard: u32, soft: Option<u32>) -> Workflow {
        let mut workflow = Workflow::default();
        workflow.capacity.statuses.insert(
            "open".to_string(),
            CapacityLimit {
                hard: Some(hard),
                soft,
            },
        );
        workflow.capacity.exemptions.providers = vec!["operator".to_string()];
        workflow
    }

    #[test]
    fn interrupted_repairs_roll_back_real_updates_and_final_audit_cache_writes() {
        for fail_after_audit in [false, true] {
            let mut storage = unblocked_fixture(&["bd-repair-b", "bd-repair-c"]);
            let request = DependencyUnblockRequest::default();
            let plan = storage.preview_dependency_unblock(&request, ACTOR).unwrap();
            assert_eq!(plan.planned_ids, ["bd-repair-b", "bd-repair-c"]);
            let before = logical_snapshot(&storage);
            FAIL_REPAIR_AFTER_UPDATES.with(|flag| flag.set((!fail_after_audit).then_some(1)));
            FAIL_REPAIR_BEFORE_COMMIT.with(|flag| flag.set(fail_after_audit));
            let result = storage.apply_dependency_unblock(&request, &plan.plan_token, ACTOR);
            FAIL_REPAIR_AFTER_UPDATES.with(|flag| flag.set(None));
            FAIL_REPAIR_BEFORE_COMMIT.with(|flag| flag.set(false));
            let error = result.unwrap_err();
            assert!(
                error.to_string().contains(if fail_after_audit {
                    "after audit, dirty and cache writes"
                } else {
                    "after a real update"
                }),
                "{error}"
            );
            assert_eq!(
                logical_snapshot(&storage),
                before,
                "rollback must restore statuses, comments, events, dirty markers and cache together"
            );
            assert!(ready_ids(&storage).is_empty());

            let report = storage
                .apply_dependency_unblock(&request, &plan.plan_token, ACTOR)
                .unwrap();
            assert_eq!(report.changed, ["bd-repair-b", "bd-repair-c"]);
            assert!(report.maintenance_error.is_none());
            assert_eq!(
                ready_ids(&storage),
                BTreeSet::from(["bd-repair-b".to_string(), "bd-repair-c".to_string()])
            );
            for id in &report.changed {
                let comments = storage.get_comments(id).unwrap();
                assert_eq!(comments.len(), 1);
                assert_eq!(comments[0].author, ACTOR);
                assert!(comments[0].body.contains(BLOCKER));
                assert!(comments[0].body.contains(&plan.plan_token));
            }
        }
    }

    #[test]
    fn transaction_guard_rechecks_a_reopened_blocker_even_for_an_empty_batch() {
        let mut storage = unblocked_fixture(&["bd-repair-b"]);
        let request = DependencyUnblockRequest::default();
        let plan = storage.preview_dependency_unblock(&request, ACTOR).unwrap();
        let updates = dependency_unblock_updates(&plan, ACTOR).unwrap();
        storage
            .update_issue(
                BLOCKER,
                &IssueUpdate {
                    status: Some(Status::Open),
                    ..IssueUpdate::default()
                },
                "competing-actor",
            )
            .unwrap();
        let guard = DependencyUnblockGuard {
            request: &request,
            expected_token: &plan.plan_token,
        };
        let before = logical_snapshot(&storage);
        for batch in [updates.as_slice(), &[]] {
            let error = storage
                .update_issues_atomically_guarded(batch, ACTOR, Some(&guard))
                .unwrap_err();
            assert!(error.to_string().contains("preview is stale"), "{error}");
            assert_eq!(logical_snapshot(&storage), before);
        }
        assert_eq!(
            storage.get_issue("bd-repair-b").unwrap().unwrap().status,
            Status::Blocked
        );
        assert!(!ready_ids(&storage).contains("bd-repair-b"));
    }

    #[test]
    fn preview_leaves_expired_capacity_exemptions_untouched_and_apply_audits_expiry() {
        let mut storage = unblocked_fixture(&["bd-repair-b"]);
        storage
            .create_issue(&issue("bd-repair-slot"), ACTOR)
            .unwrap();
        storage.set_workflow_policy(open_capacity(2, Some(2)));
        storage
            .grant_capacity_exemption(
                "bd-repair-slot",
                "status",
                "open",
                "operator",
                "temporary capacity",
                Some(Utc::now() + chrono::Duration::hours(2)),
                ACTOR,
            )
            .unwrap();
        storage
            .with_connection_write_transaction(|conn| {
                conn.execute_with_params(
                    "UPDATE capacity_exemptions SET expires_at = ? WHERE issue_id = ?",
                    &[
                        SqliteValue::from((Utc::now() - chrono::Duration::hours(1)).to_rfc3339()),
                        SqliteValue::from("bd-repair-slot"),
                    ],
                )?;
                Ok(())
            })
            .unwrap();
        let before = logical_snapshot(&storage);
        let request = DependencyUnblockRequest::default();
        let plan = storage.preview_dependency_unblock(&request, ACTOR).unwrap();
        assert_eq!(plan.planned_ids, ["bd-repair-b"]);
        assert_eq!(plan.capacity_warnings.len(), 1);
        assert_eq!(plan.capacity_warnings[0].prospective, 2);
        assert_eq!(
            logical_snapshot(&storage),
            before,
            "preview must not audit lazy expiry"
        );
        assert!(
            storage
                .list_capacity_exemptions(Some("bd-repair-slot"))
                .unwrap()[0]
                .ended_at
                .is_none()
        );

        let report = storage
            .apply_dependency_unblock(&request, &plan.plan_token, ACTOR)
            .unwrap();
        assert_eq!(report.changed, ["bd-repair-b"]);
        assert_eq!(report.capacity_warnings, plan.capacity_warnings);
        assert!(
            storage
                .list_capacity_exemptions(Some("bd-repair-slot"))
                .unwrap()[0]
                .ended_at
                .is_some()
        );
        let history = storage
            .get_capacity_exemption_history(Some("bd-repair-slot"))
            .unwrap();
        assert_eq!(history.last().unwrap().action, "expire");
    }

    #[test]
    fn committed_maintenance_failure_reports_durable_changes_and_canonical_warnings() {
        let mut storage = unblocked_fixture(&["bd-repair-b"]);
        storage.set_workflow_policy(open_capacity(1, Some(1)));
        let request = DependencyUnblockRequest::default();
        let plan = storage.preview_dependency_unblock(&request, ACTOR).unwrap();
        FAIL_REPAIR_AFTER_COMMIT.with(|flag| flag.set(true));
        let result = storage.apply_dependency_unblock(&request, &plan.plan_token, ACTOR);
        FAIL_REPAIR_AFTER_COMMIT.with(|flag| flag.set(false));
        let report = result.unwrap();
        assert_eq!(report.changed, ["bd-repair-b"]);
        assert!(
            report
                .maintenance_error
                .as_deref()
                .unwrap()
                .contains("committed")
        );
        assert_eq!(report.capacity_warnings, plan.capacity_warnings);
        assert_eq!(
            storage.get_issue("bd-repair-b").unwrap().unwrap().status,
            Status::Open
        );
        assert!(ready_ids(&storage).contains("bd-repair-b"));
        assert_eq!(storage.get_comments("bd-repair-b").unwrap().len(), 1);
        let after = logical_snapshot(&storage);
        assert!(
            storage
                .apply_dependency_unblock(&request, &plan.plan_token, ACTOR)
                .is_err()
        );
        assert_eq!(
            logical_snapshot(&storage),
            after,
            "the old plan cannot replay committed audit"
        );

        let lost_authority = BeadsError::CommittedStateUnwitnessed {
            operation: "dependency repair test".to_string(),
            source: Box::new(BeadsError::Config("database inode replaced".to_string())),
        };
        let classified = committed_maintenance_result::<()>(Err(lost_authority)).unwrap_err();
        assert!(matches!(
            classified,
            BeadsError::CommittedStateUnwitnessed { .. }
        ));
    }

    #[test]
    fn invalid_generated_audit_is_rejected_during_preview_without_writes() {
        let mut storage = unblocked_fixture(&["bd-repair-b"]);
        let request = DependencyUnblockRequest::default();
        for actor in ["x".repeat(201), "invalid\0actor".to_string()] {
            let before = logical_snapshot(&storage);
            let plan = storage
                .preview_dependency_unblock(&request, &actor)
                .unwrap();
            assert!(plan.planned_ids.is_empty());
            assert_eq!(plan.candidates[0].reason, "workflow_policy");
            assert_eq!(logical_snapshot(&storage), before);
            let report = storage
                .apply_dependency_unblock(&request, &plan.plan_token, &actor)
                .unwrap();
            assert!(report.changed.is_empty());
            assert_eq!(logical_snapshot(&storage), before);
        }
    }

    #[test]
    fn malformed_and_duplicate_records_cannot_be_collapsed_into_a_repair_graph() {
        let valid = issue("bd-repair-valid");
        let duplicate = checked_issue_ids(&[valid.clone(), valid]).unwrap_err();
        assert!(duplicate.to_string().contains("duplicate stored issue ID"));

        let storage = unblocked_fixture(&["bd-repair-b"]);
        // Deliberately corrupt a referenced primary key; normal FK enforcement
        // would reject the fixture before the preservation decoder can inspect it.
        storage.conn.execute("PRAGMA foreign_keys = OFF").unwrap();
        storage
            .conn
            .execute_with_params(
                "UPDATE issues SET id = ? WHERE id = ?",
                &[
                    SqliteValue::from("malformed id"),
                    SqliteValue::from("bd-repair-b"),
                ],
            )
            .unwrap();
        SqliteStorage::restore_foreign_keys(&storage.conn, "malformed repair fixture").unwrap();
        let before = logical_snapshot(&storage);
        let error = storage
            .preview_dependency_unblock(&DependencyUnblockRequest::default(), ACTOR)
            .unwrap_err();
        assert!(error.to_string().contains("malformed stored issue ID"));
        assert_eq!(logical_snapshot(&storage), before);

        for column in ["assignee", "pinned", "is_template"] {
            let storage = unblocked_fixture(&["bd-repair-b"]);
            storage
                .conn
                .execute_with_params(
                    &format!("UPDATE issues SET {column} = ? WHERE id = ?"),
                    &[
                        SqliteValue::Blob(vec![0x80].into()),
                        SqliteValue::from("bd-repair-b"),
                    ],
                )
                .unwrap();
            let before = logical_snapshot(&storage);
            let error = storage
                .preview_dependency_unblock(&DependencyUnblockRequest::default(), ACTOR)
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("unrepresentable {column}")),
                "{error}"
            );
            assert_eq!(logical_snapshot(&storage), before);
        }
    }
}
