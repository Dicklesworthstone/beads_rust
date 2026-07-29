//! Pure evidence contracts for swarm coordination diagnosis.
//!
//! This module deliberately does not inspect Agent Mail, read the filesystem, or
//! mutate claims. Callers provide issue metadata and optional reservation
//! evidence, then receive a deterministic classification that future CLI, MCP,
//! scheduler, and audit surfaces can share.

use crate::model::{Comment, IssueType, Priority, Status};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Schema version for coordination status and claim evidence outputs.
pub const COORDINATION_SCHEMA_VERSION: &str = "br.coordination.v1";
/// Swarm-agent claims become stale candidates after two quiet hours.
pub const SWARM_STALE_CANDIDATE_AFTER_MINUTES: i64 = 2 * 60;
/// Extra-conservative marker for likely abandoned swarm claims.
pub const SWARM_ABANDONED_LIKELY_AFTER_MINUTES: i64 = 8 * 60;
/// Human or unclear claims use a one-business-day rule of thumb.
pub const HUMAN_STALE_CANDIDATE_AFTER_MINUTES: i64 = 24 * 60;
/// Extra-conservative marker for likely abandoned human or unclear claims.
pub const HUMAN_ABANDONED_LIKELY_AFTER_MINUTES: i64 = 72 * 60;

/// Who appears to own the current claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClaimOwnerKind {
    /// A named coding-agent swarm participant.
    SwarmAgent,
    /// A human assignee or operator-owned claim.
    Human,
    /// Ownership cannot be confidently classified.
    Unknown,
}

impl ClaimOwnerKind {
    /// Stale-candidate threshold in minutes for this owner class.
    #[must_use]
    pub const fn stale_candidate_after_minutes(self) -> i64 {
        match self {
            Self::SwarmAgent => SWARM_STALE_CANDIDATE_AFTER_MINUTES,
            Self::Human | Self::Unknown => HUMAN_STALE_CANDIDATE_AFTER_MINUTES,
        }
    }

    /// Likely-abandoned threshold in minutes for this owner class.
    #[must_use]
    pub const fn abandoned_likely_after_minutes(self) -> i64 {
        match self {
            Self::SwarmAgent => SWARM_ABANDONED_LIKELY_AFTER_MINUTES,
            Self::Human | Self::Unknown => HUMAN_ABANDONED_LIKELY_AFTER_MINUTES,
        }
    }
}

/// Why an Agent Mail reservation snapshot was associated with an issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReservationSnapshotMatchReason {
    /// Reservation holder matches the issue assignee.
    HolderMatchesAssignee,
    /// Reservation reason or thread id names the issue id.
    IssueId,
    /// A recent issue comment names the reservation path pattern.
    CommentPath,
}

impl ReservationSnapshotMatchReason {
    /// Stable snake_case value used in text projections.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HolderMatchesAssignee => "holder_matches_assignee",
            Self::IssueId => "issue_id",
            Self::CommentPath => "comment_path",
        }
    }
}

/// Minimal offline reservation snapshot row exported from Agent Mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentMailReservationSnapshot {
    pub holder: String,
    pub path_pattern: String,
    pub exclusive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub expires_ts: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released_ts: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// Minimal offline agent-liveness snapshot row exported from Agent Mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentMailAgentSnapshot {
    pub name: String,
    pub task_description: String,
    pub last_active_ts: DateTime<Utc>,
    pub contact_policy: String,
}

/// Provenance copied from a matching reservation snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReservationEvidenceProvenance {
    pub path_pattern: String,
    pub exclusive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub expires_ts: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released_ts: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub matched_on: Vec<ReservationSnapshotMatchReason>,
}

/// Optional Agent Mail reservation evidence supplied by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "state", content = "detail")]
pub enum ReservationEvidence {
    /// No Agent Mail snapshot was supplied, so absence of reservations is not
    /// evidence of abandonment.
    NoSnapshot,
    /// A snapshot was supplied and no matching reservation was found.
    NoReservation,
    /// A matching active reservation exists.
    Active {
        holder: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_at: Option<DateTime<Utc>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provenance: Option<ReservationEvidenceProvenance>,
    },
    /// A matching reservation exists but is no longer active.
    Expired {
        holder: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        released_at: Option<DateTime<Utc>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provenance: Option<ReservationEvidenceProvenance>,
    },
    /// Snapshot data was supplied but could not be trusted.
    InvalidSnapshot { reason: String },
}

/// Stable claim classifications used by coordination-aware surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClaimClassification {
    /// The issue is in progress but has no meaningful assignee.
    Unassigned,
    /// The claim is still within the owner-specific freshness window.
    Fresh,
    /// A live reservation exists, so the claim must not be treated as abandoned.
    BlockedByActiveReservation,
    /// The claim has crossed the stale threshold and no active reservation was
    /// found in a supplied snapshot.
    StaleCandidate,
    /// The claim has crossed a more conservative abandoned threshold.
    AbandonedLikely,
    /// The claim is old enough to inspect, but no Agent Mail snapshot was
    /// supplied.
    NoMailSnapshot,
    /// Evidence conflicts or cannot be trusted.
    Ambiguous,
}

impl ClaimClassification {
    /// Stable snake_case value used in text projections.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unassigned => "unassigned",
            Self::Fresh => "fresh",
            Self::BlockedByActiveReservation => "blocked_by_active_reservation",
            Self::StaleCandidate => "stale_candidate",
            Self::AbandonedLikely => "abandoned_likely",
            Self::NoMailSnapshot => "no_mail_snapshot",
            Self::Ambiguous => "ambiguous",
        }
    }
}

/// Suggested next action for an operator or agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedAction {
    /// Do nothing except continue normal observation.
    Observe,
    /// Ask the assignee or human operator before touching the claim.
    AskOwner,
    /// Inspect Agent Mail reservations or capture a fresh snapshot.
    InspectMail,
    /// The issue is a candidate for the documented audit-comment plus claim
    /// sequence. This is still advisory, not an automatic mutation.
    ReclaimCandidate,
    /// Leave the claim alone because evidence says work may still be active.
    LeaveActive,
}

impl RecommendedAction {
    /// Stable snake_case value used in text projections.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::AskOwner => "ask_owner",
            Self::InspectMail => "inspect_mail",
            Self::ReclaimCandidate => "reclaim_candidate",
            Self::LeaveActive => "leave_active",
        }
    }
}

/// Role for a suggested coordination command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoordinationSuggestedCommandPurpose {
    /// Record the reclaim evidence before touching ownership.
    AddReclaimAuditComment,
    /// Claim the issue after the audit comment is recorded.
    ClaimIssue,
}

/// A command suggestion emitted for an operator to review and run manually.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinationSuggestedCommand {
    pub purpose: CoordinationSuggestedCommandPurpose,
    pub command: String,
}

/// Advisory reclaim guidance for one in-progress claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinationClaimAdvisory {
    pub reclaim_allowed_by_policy: bool,
    pub required_human_confirmation: bool,
    pub evidence_summary: String,
    pub suggested_commands: Vec<CoordinationSuggestedCommand>,
}

/// Evidence categories present in a claim assessment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoordinationEvidenceSource {
    /// Issue status, assignee, and updated timestamp.
    IssueMetadata,
    /// Owner-specific stale and abandoned thresholds.
    PolicyThreshold,
    /// A caller-supplied Agent Mail snapshot.
    AgentMailSnapshot,
    /// Explicit absence of an Agent Mail snapshot.
    NoAgentMailSnapshot,
}

/// Bounded comment excerpt included with a coordination claim row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinationComment {
    pub author: String,
    pub text: String,
    pub created_at: DateTime<Utc>,
}

impl From<&Comment> for CoordinationComment {
    fn from(comment: &Comment) -> Self {
        Self {
            author: comment.author.clone(),
            text: comment.body.clone(),
            created_at: comment.created_at,
        }
    }
}

/// Caller-provided input for one claim assessment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimAssessmentInput {
    pub assignee: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub now: DateTime<Utc>,
    pub owner_kind: ClaimOwnerKind,
    pub reservation: ReservationEvidence,
}

/// Thresholds used when classifying a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimAssessmentPolicy {
    pub stale_candidate_after_minutes: i64,
    pub abandoned_likely_after_minutes: i64,
}

impl ClaimAssessmentPolicy {
    /// Build the default coordination policy for an owner class.
    #[must_use]
    pub const fn for_owner_kind(owner_kind: ClaimOwnerKind) -> Self {
        Self {
            stale_candidate_after_minutes: owner_kind.stale_candidate_after_minutes(),
            abandoned_likely_after_minutes: owner_kind.abandoned_likely_after_minutes(),
        }
    }
}

/// Deterministic assessment for one in-progress claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClaimAssessment {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    pub owner_kind: ClaimOwnerKind,
    pub updated_at: DateTime<Utc>,
    pub updated_age_minutes: i64,
    pub stale_threshold_minutes: i64,
    pub abandoned_threshold_minutes: i64,
    pub reservation: ReservationEvidence,
    pub classification: ClaimClassification,
    pub recommended_action: RecommendedAction,
    pub evidence_sources: Vec<CoordinationEvidenceSource>,
}

/// Issue context attached to one coordination claim row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinationIssueRow {
    pub id: String,
    pub title: String,
    pub status: Status,
    pub priority: Priority,
    pub issue_type: IssueType,
    pub labels: Vec<String>,
    pub dependency_count: usize,
    pub dependent_count: usize,
    pub latest_comments: Vec<CoordinationComment>,
}

/// One in-progress issue plus its deterministic coordination assessment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinationClaimRow {
    pub issue: CoordinationIssueRow,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentMailAgentSnapshot>,
    pub assessment: ClaimAssessment,
    pub reclaim_allowed_by_policy: bool,
    pub required_human_confirmation: bool,
    pub evidence_summary: String,
    pub suggested_commands: Vec<CoordinationSuggestedCommand>,
}

/// Workspace context counts shown next to hidden-claim diagnosis.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinationWorkspaceCounts {
    pub open: usize,
    pub ready: usize,
    pub blocked: usize,
    pub in_progress: usize,
    pub deferred: usize,
    pub closed: usize,
}

/// Count summary for coordination status output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinationSummary {
    pub total_claims: usize,
    pub workspace: CoordinationWorkspaceCounts,
    pub unassigned: usize,
    pub fresh: usize,
    pub blocked_by_active_reservation: usize,
    pub stale_candidate: usize,
    pub abandoned_likely: usize,
    pub no_mail_snapshot: usize,
    pub ambiguous: usize,
}

/// Top-level machine-readable coordination status shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinationStatusOutput {
    pub schema_version: String,
    pub generated_at: DateTime<Utc>,
    pub summary: CoordinationSummary,
    pub claims: Vec<CoordinationClaimRow>,
    /// Workflow capacity occupancy (GitHub #384 phase 6). Absent when no
    /// capacity is configured, preserving the pre-capacity
    /// `br.coordination.v1` payload shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capacity: Vec<crate::format::CapacityStat>,
}

impl CoordinationStatusOutput {
    /// Build a coordination status envelope with current schema version.
    #[must_use]
    pub fn new(
        generated_at: DateTime<Utc>,
        workspace: CoordinationWorkspaceCounts,
        claims: Vec<CoordinationClaimRow>,
    ) -> Self {
        let summary = CoordinationSummary::from_claims(&claims, workspace);
        Self {
            schema_version: COORDINATION_SCHEMA_VERSION.to_string(),
            generated_at,
            summary,
            claims,
            capacity: Vec::new(),
        }
    }
}

impl CoordinationSummary {
    /// Count claim classifications for a status envelope.
    #[must_use]
    pub fn from_claims(
        claims: &[CoordinationClaimRow],
        workspace: CoordinationWorkspaceCounts,
    ) -> Self {
        let mut summary = Self {
            total_claims: claims.len(),
            workspace,
            unassigned: 0,
            fresh: 0,
            blocked_by_active_reservation: 0,
            stale_candidate: 0,
            abandoned_likely: 0,
            no_mail_snapshot: 0,
            ambiguous: 0,
        };

        for claim in claims {
            match claim.assessment.classification {
                ClaimClassification::Unassigned => summary.unassigned += 1,
                ClaimClassification::Fresh => summary.fresh += 1,
                ClaimClassification::BlockedByActiveReservation => {
                    summary.blocked_by_active_reservation += 1;
                }
                ClaimClassification::StaleCandidate => summary.stale_candidate += 1,
                ClaimClassification::AbandonedLikely => summary.abandoned_likely += 1,
                ClaimClassification::NoMailSnapshot => summary.no_mail_snapshot += 1,
                ClaimClassification::Ambiguous => summary.ambiguous += 1,
            }
        }

        summary
    }
}

/// Classify one in-progress claim from caller-supplied evidence.
#[must_use]
pub fn assess_claim(input: ClaimAssessmentInput) -> ClaimAssessment {
    let policy = ClaimAssessmentPolicy::for_owner_kind(input.owner_kind);
    assess_claim_with_policy(input, policy)
}

/// Classify one claim with caller-supplied thresholds.
#[must_use]
pub fn assess_claim_with_policy(
    input: ClaimAssessmentInput,
    policy: ClaimAssessmentPolicy,
) -> ClaimAssessment {
    let assignee = input
        .assignee
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let updated_age_minutes = input
        .now
        .signed_duration_since(input.updated_at)
        .num_minutes()
        .max(0);
    let stale_threshold_minutes = policy.stale_candidate_after_minutes.max(0);
    let abandoned_threshold_minutes = policy
        .abandoned_likely_after_minutes
        .max(stale_threshold_minutes);

    let (classification, recommended_action) = classify_claim(
        assignee.as_deref(),
        input.owner_kind,
        &input.reservation,
        updated_age_minutes,
        stale_threshold_minutes,
        abandoned_threshold_minutes,
    );
    let evidence_sources = evidence_sources_for(&input.reservation);

    ClaimAssessment {
        assignee,
        owner_kind: input.owner_kind,
        updated_at: input.updated_at,
        updated_age_minutes,
        stale_threshold_minutes,
        abandoned_threshold_minutes,
        reservation: input.reservation,
        classification,
        recommended_action,
        evidence_sources,
    }
}

fn classify_claim(
    assignee: Option<&str>,
    owner_kind: ClaimOwnerKind,
    reservation: &ReservationEvidence,
    updated_age_minutes: i64,
    stale_threshold_minutes: i64,
    abandoned_threshold_minutes: i64,
) -> (ClaimClassification, RecommendedAction) {
    if assignee.is_none() {
        return (ClaimClassification::Unassigned, RecommendedAction::Observe);
    }

    if updated_age_minutes < stale_threshold_minutes {
        return (ClaimClassification::Fresh, RecommendedAction::Observe);
    }

    match reservation {
        ReservationEvidence::Active { .. } => (
            ClaimClassification::BlockedByActiveReservation,
            RecommendedAction::LeaveActive,
        ),
        ReservationEvidence::NoSnapshot => (
            ClaimClassification::NoMailSnapshot,
            RecommendedAction::InspectMail,
        ),
        ReservationEvidence::InvalidSnapshot { .. } => (
            ClaimClassification::Ambiguous,
            RecommendedAction::InspectMail,
        ),
        ReservationEvidence::NoReservation | ReservationEvidence::Expired { .. } => {
            if updated_age_minutes >= abandoned_threshold_minutes {
                (
                    ClaimClassification::AbandonedLikely,
                    recommended_reclaim_action(owner_kind),
                )
            } else {
                (
                    ClaimClassification::StaleCandidate,
                    recommended_reclaim_action(owner_kind),
                )
            }
        }
    }
}

const fn recommended_reclaim_action(owner_kind: ClaimOwnerKind) -> RecommendedAction {
    match owner_kind {
        ClaimOwnerKind::SwarmAgent => RecommendedAction::ReclaimCandidate,
        ClaimOwnerKind::Human | ClaimOwnerKind::Unknown => RecommendedAction::AskOwner,
    }
}

fn evidence_sources_for(reservation: &ReservationEvidence) -> Vec<CoordinationEvidenceSource> {
    let mut sources = vec![
        CoordinationEvidenceSource::IssueMetadata,
        CoordinationEvidenceSource::PolicyThreshold,
    ];
    match reservation {
        ReservationEvidence::NoSnapshot => {
            sources.push(CoordinationEvidenceSource::NoAgentMailSnapshot);
        }
        ReservationEvidence::NoReservation
        | ReservationEvidence::Active { .. }
        | ReservationEvidence::Expired { .. }
        | ReservationEvidence::InvalidSnapshot { .. } => {
            sources.push(CoordinationEvidenceSource::AgentMailSnapshot);
        }
    }
    sources
}

/// Build advisory-only reclaim guidance for one claim assessment.
#[must_use]
pub fn advisory_for_claim(
    issue_id: &str,
    assessment: &ClaimAssessment,
) -> CoordinationClaimAdvisory {
    let reclaim_allowed_by_policy = reclaim_allowed_by_policy(assessment);
    let required_human_confirmation =
        matches!(assessment.recommended_action, RecommendedAction::AskOwner);
    let evidence_summary = evidence_summary_for(assessment);
    let suggested_commands = if reclaim_allowed_by_policy {
        reclaim_suggested_commands(issue_id, &evidence_summary)
    } else {
        Vec::new()
    };

    CoordinationClaimAdvisory {
        reclaim_allowed_by_policy,
        required_human_confirmation,
        evidence_summary,
        suggested_commands,
    }
}

fn reclaim_allowed_by_policy(assessment: &ClaimAssessment) -> bool {
    matches!(assessment.owner_kind, ClaimOwnerKind::SwarmAgent)
        && matches!(
            assessment.classification,
            ClaimClassification::StaleCandidate | ClaimClassification::AbandonedLikely
        )
        && matches!(
            assessment.recommended_action,
            RecommendedAction::ReclaimCandidate
        )
        && matches!(
            assessment.reservation,
            ReservationEvidence::NoReservation | ReservationEvidence::Expired { .. }
        )
}

fn evidence_summary_for(assessment: &ClaimAssessment) -> String {
    let assignee = assessment.assignee.as_deref().unwrap_or("(unassigned)");
    format!(
        "updated_at={}, assignee={}, owner_kind={}, age_minutes={}, stale_threshold_minutes={}, abandoned_threshold_minutes={}, reservation_status={}",
        assessment.updated_at,
        assignee,
        owner_kind_summary(assessment.owner_kind),
        assessment.updated_age_minutes,
        assessment.stale_threshold_minutes,
        assessment.abandoned_threshold_minutes,
        reservation_summary(&assessment.reservation)
    )
}

const fn owner_kind_summary(owner_kind: ClaimOwnerKind) -> &'static str {
    match owner_kind {
        ClaimOwnerKind::SwarmAgent => "swarm_agent",
        ClaimOwnerKind::Human => "human",
        ClaimOwnerKind::Unknown => "unknown",
    }
}

fn reservation_summary(reservation: &ReservationEvidence) -> String {
    match reservation {
        ReservationEvidence::NoSnapshot => "no_snapshot".to_string(),
        ReservationEvidence::NoReservation => "no_reservation".to_string(),
        ReservationEvidence::Active {
            holder, expires_at, ..
        } => format!(
            "active(holder={},expires_at={})",
            holder,
            optional_timestamp(expires_at.as_ref())
        ),
        ReservationEvidence::Expired {
            holder,
            released_at,
            ..
        } => format!(
            "expired(holder={},released_at={})",
            holder,
            optional_timestamp(released_at.as_ref())
        ),
        ReservationEvidence::InvalidSnapshot { reason } => {
            format!("invalid_snapshot(reason={reason})")
        }
    }
}

fn optional_timestamp(timestamp: Option<&DateTime<Utc>>) -> String {
    timestamp.map_or_else(|| "none".to_string(), ToString::to_string)
}

fn reclaim_suggested_commands(
    issue_id: &str,
    evidence_summary: &str,
) -> Vec<CoordinationSuggestedCommand> {
    let audit_message = format!(
        "reclaim: previous in_progress claim appears abandoned; evidence: {evidence_summary}; no active reservation found in supplied snapshot; pane_status=not_checked_by_br"
    );
    vec![
        CoordinationSuggestedCommand {
            purpose: CoordinationSuggestedCommandPurpose::AddReclaimAuditComment,
            command: format!(
                "br comments add {} --author \"$AGENT_NAME\" --message {} --json",
                shell_quote(issue_id),
                shell_quote(&audit_message)
            ),
        },
        CoordinationSuggestedCommand {
            purpose: CoordinationSuggestedCommandPurpose::ClaimIssue,
            command: format!("br update {} --claim --json", shell_quote(issue_id)),
        },
    ]
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Derive reservation evidence for one issue from an optional offline snapshot.
#[must_use]
pub fn reservation_evidence_from_snapshots(
    issue_id: &str,
    assignee: Option<&str>,
    comments: &[Comment],
    reservations: Option<&[AgentMailReservationSnapshot]>,
    now: DateTime<Utc>,
) -> ReservationEvidence {
    let Some(reservations) = reservations else {
        return ReservationEvidence::NoSnapshot;
    };

    let mut expired_match = None;
    for reservation in reservations {
        let matched_on = reservation_match_reasons(issue_id, assignee, comments, reservation);
        if matched_on.is_empty() {
            continue;
        }

        let provenance = reservation.provenance(matched_on);
        if reservation.is_active(now) {
            return ReservationEvidence::Active {
                holder: reservation.holder.clone(),
                expires_at: Some(reservation.expires_ts),
                provenance: Some(provenance),
            };
        }

        expired_match.get_or_insert_with(|| ReservationEvidence::Expired {
            holder: reservation.holder.clone(),
            released_at: reservation.released_ts,
            provenance: Some(provenance),
        });
    }

    expired_match.unwrap_or(ReservationEvidence::NoReservation)
}

fn reservation_match_reasons(
    issue_id: &str,
    assignee: Option<&str>,
    comments: &[Comment],
    reservation: &AgentMailReservationSnapshot,
) -> Vec<ReservationSnapshotMatchReason> {
    let mut reasons = Vec::new();
    let holder = reservation.holder.trim();

    if assignee
        .map(str::trim)
        .is_some_and(|value| !value.is_empty() && value.eq_ignore_ascii_case(holder))
    {
        reasons.push(ReservationSnapshotMatchReason::HolderMatchesAssignee);
    }

    if reservation
        .reason
        .as_deref()
        .is_some_and(|reason| reason.contains(issue_id))
        || reservation
            .thread_id
            .as_deref()
            .is_some_and(|thread_id| thread_id == issue_id)
    {
        reasons.push(ReservationSnapshotMatchReason::IssueId);
    }

    let path = reservation.path_pattern.trim();
    if !path.is_empty() && comments.iter().any(|comment| comment.body.contains(path)) {
        reasons.push(ReservationSnapshotMatchReason::CommentPath);
    }

    reasons
}

impl AgentMailReservationSnapshot {
    fn is_active(&self, now: DateTime<Utc>) -> bool {
        self.released_ts.is_none() && self.expires_ts > now
    }

    fn provenance(
        &self,
        matched_on: Vec<ReservationSnapshotMatchReason>,
    ) -> ReservationEvidenceProvenance {
        ReservationEvidenceProvenance {
            path_pattern: self.path_pattern.clone(),
            exclusive: self.exclusive,
            reason: self.reason.clone(),
            expires_ts: self.expires_ts,
            released_ts: self.released_ts,
            thread_id: self.thread_id.clone(),
            matched_on,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentMailReservationSnapshot, COORDINATION_SCHEMA_VERSION, ClaimAssessmentInput,
        ClaimClassification, ClaimOwnerKind, CoordinationClaimRow, CoordinationComment,
        CoordinationEvidenceSource, CoordinationIssueRow, CoordinationStatusOutput,
        CoordinationSuggestedCommandPurpose, CoordinationWorkspaceCounts, RecommendedAction,
        ReservationEvidence, advisory_for_claim, assess_claim, reservation_evidence_from_snapshots,
    };
    use crate::model::{Comment, IssueType, Priority, Status};
    use chrono::{Duration, TimeZone, Utc};

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 8, 9, 0, 0)
            .single()
            .expect("valid timestamp")
    }

    fn input(
        minutes_old: i64,
        owner_kind: ClaimOwnerKind,
        reservation: ReservationEvidence,
    ) -> ClaimAssessmentInput {
        ClaimAssessmentInput {
            assignee: Some("TopazFox".to_string()),
            updated_at: now() - Duration::minutes(minutes_old),
            now: now(),
            owner_kind,
            reservation,
        }
    }

    fn claim_row(id: &str, assessment: super::ClaimAssessment) -> CoordinationClaimRow {
        let advisory = advisory_for_claim(id, &assessment);
        CoordinationClaimRow {
            issue: CoordinationIssueRow {
                id: id.to_string(),
                title: format!("Claim {id}"),
                status: Status::InProgress,
                priority: Priority(1),
                issue_type: IssueType::Task,
                labels: vec!["coordination".to_string()],
                dependency_count: 1,
                dependent_count: 2,
                latest_comments: vec![CoordinationComment {
                    author: "TopazFox".to_string(),
                    text: "latest evidence".to_string(),
                    created_at: now(),
                }],
            },
            agent: None,
            assessment,
            reclaim_allowed_by_policy: advisory.reclaim_allowed_by_policy,
            required_human_confirmation: advisory.required_human_confirmation,
            evidence_summary: advisory.evidence_summary,
            suggested_commands: advisory.suggested_commands,
        }
    }

    #[test]
    fn swarm_claim_below_two_hours_is_fresh() {
        let assessment = assess_claim(input(
            119,
            ClaimOwnerKind::SwarmAgent,
            ReservationEvidence::NoReservation,
        ));

        assert_eq!(assessment.updated_age_minutes, 119);
        assert_eq!(assessment.stale_threshold_minutes, 120);
        assert_eq!(assessment.classification, ClaimClassification::Fresh);
        assert_eq!(assessment.recommended_action, RecommendedAction::Observe);
    }

    #[test]
    fn old_swarm_claim_without_mail_snapshot_requires_mail_inspection() {
        let assessment = assess_claim(input(
            120,
            ClaimOwnerKind::SwarmAgent,
            ReservationEvidence::NoSnapshot,
        ));

        assert_eq!(
            assessment.classification,
            ClaimClassification::NoMailSnapshot
        );
        assert_eq!(
            assessment.recommended_action,
            RecommendedAction::InspectMail
        );
        assert!(
            assessment
                .evidence_sources
                .contains(&CoordinationEvidenceSource::NoAgentMailSnapshot)
        );
    }

    #[test]
    fn absent_reservation_snapshot_match_marks_stale_candidate() {
        let assessment = assess_claim(input(
            120,
            ClaimOwnerKind::SwarmAgent,
            ReservationEvidence::NoReservation,
        ));

        assert_eq!(
            assessment.classification,
            ClaimClassification::StaleCandidate
        );
        assert_eq!(
            assessment.recommended_action,
            RecommendedAction::ReclaimCandidate
        );
    }

    #[test]
    fn very_old_swarm_claim_is_abandoned_likely() {
        let assessment = assess_claim(input(
            8 * 60,
            ClaimOwnerKind::SwarmAgent,
            ReservationEvidence::Expired {
                holder: "TopazFox".to_string(),
                released_at: None,
                provenance: None,
            },
        ));

        assert_eq!(
            assessment.classification,
            ClaimClassification::AbandonedLikely
        );
        assert_eq!(assessment.abandoned_threshold_minutes, 8 * 60);
        assert_eq!(
            assessment.recommended_action,
            RecommendedAction::ReclaimCandidate
        );
    }

    #[test]
    fn active_reservation_blocks_reclaim_even_when_old() {
        let assessment = assess_claim(input(
            12 * 60,
            ClaimOwnerKind::SwarmAgent,
            ReservationEvidence::Active {
                holder: "TopazFox".to_string(),
                expires_at: Some(now() + Duration::minutes(30)),
                provenance: None,
            },
        ));

        assert_eq!(
            assessment.classification,
            ClaimClassification::BlockedByActiveReservation
        );
        assert_eq!(
            assessment.recommended_action,
            RecommendedAction::LeaveActive
        );
    }

    #[test]
    fn human_and_unknown_claims_use_one_business_day_threshold() {
        let human_fresh = assess_claim(input(
            23 * 60,
            ClaimOwnerKind::Human,
            ReservationEvidence::NoReservation,
        ));
        let unknown_stale = assess_claim(input(
            24 * 60,
            ClaimOwnerKind::Unknown,
            ReservationEvidence::NoReservation,
        ));

        assert_eq!(human_fresh.classification, ClaimClassification::Fresh);
        assert_eq!(unknown_stale.stale_threshold_minutes, 24 * 60);
        assert_eq!(
            unknown_stale.classification,
            ClaimClassification::StaleCandidate
        );
        assert_eq!(
            unknown_stale.recommended_action,
            RecommendedAction::AskOwner
        );
    }

    #[test]
    fn future_updated_at_saturates_age_to_zero() {
        let assessment = assess_claim(ClaimAssessmentInput {
            assignee: Some("TopazFox".to_string()),
            updated_at: now() + Duration::minutes(5),
            now: now(),
            owner_kind: ClaimOwnerKind::SwarmAgent,
            reservation: ReservationEvidence::NoReservation,
        });

        assert_eq!(assessment.updated_age_minutes, 0);
        assert_eq!(assessment.classification, ClaimClassification::Fresh);
    }

    #[test]
    fn empty_or_whitespace_assignee_is_unassigned() {
        let assessment = assess_claim(ClaimAssessmentInput {
            assignee: Some("   ".to_string()),
            updated_at: now() - Duration::hours(12),
            now: now(),
            owner_kind: ClaimOwnerKind::SwarmAgent,
            reservation: ReservationEvidence::NoReservation,
        });

        assert_eq!(assessment.assignee, None);
        assert_eq!(assessment.classification, ClaimClassification::Unassigned);
        assert_eq!(assessment.recommended_action, RecommendedAction::Observe);
    }

    #[test]
    fn invalid_snapshot_is_ambiguous_not_abandoned() {
        let assessment = assess_claim(input(
            8 * 60,
            ClaimOwnerKind::SwarmAgent,
            ReservationEvidence::InvalidSnapshot {
                reason: "missing holder field".to_string(),
            },
        ));

        assert_eq!(assessment.classification, ClaimClassification::Ambiguous);
        assert_eq!(
            assessment.recommended_action,
            RecommendedAction::InspectMail
        );
    }

    #[test]
    fn status_output_sets_schema_and_counts_classifications() {
        let fresh = assess_claim(input(
            30,
            ClaimOwnerKind::SwarmAgent,
            ReservationEvidence::NoReservation,
        ));
        let stale = assess_claim(input(
            120,
            ClaimOwnerKind::SwarmAgent,
            ReservationEvidence::NoReservation,
        ));
        let workspace = CoordinationWorkspaceCounts {
            open: 3,
            ready: 1,
            blocked: 1,
            in_progress: 2,
            deferred: 0,
            closed: 5,
        };
        let output = CoordinationStatusOutput::new(
            now(),
            workspace,
            vec![claim_row("bd-fresh", fresh), claim_row("bd-stale", stale)],
        );

        assert_eq!(output.schema_version, COORDINATION_SCHEMA_VERSION);
        assert_eq!(output.summary.total_claims, 2);
        assert_eq!(output.summary.workspace.in_progress, 2);
        assert_eq!(output.summary.fresh, 1);
        assert_eq!(output.summary.stale_candidate, 1);
        assert_eq!(
            output.claims[0].issue.latest_comments[0].text,
            "latest evidence"
        );
    }

    #[test]
    fn coordination_status_schema_declares_required_sections() {
        let schema = schemars::schema_for!(CoordinationStatusOutput);
        let value = serde_json::to_value(schema).expect("schema serializes");
        let required = value["required"].as_array().expect("schema has required");
        for field in ["schema_version", "generated_at", "summary", "claims"] {
            assert!(
                required.iter().any(|entry| entry.as_str() == Some(field)),
                "CoordinationStatusOutput schema should require {field}"
            );
        }
    }

    #[test]
    fn reservation_snapshot_active_match_blocks_reclaim_with_provenance() {
        let reservations = vec![AgentMailReservationSnapshot {
            holder: "TopazFox".to_string(),
            path_pattern: "src/coordination.rs".to_string(),
            exclusive: true,
            reason: Some("beads_rust-sc6u optional snapshot work".to_string()),
            expires_ts: now() + Duration::minutes(30),
            released_ts: None,
            thread_id: Some("beads_rust-sc6u".to_string()),
        }];
        let evidence = reservation_evidence_from_snapshots(
            "beads_rust-sc6u",
            Some("TopazFox"),
            &[],
            Some(&reservations),
            now(),
        );
        let assessment = assess_claim(ClaimAssessmentInput {
            assignee: Some("TopazFox".to_string()),
            updated_at: now() - Duration::hours(12),
            now: now(),
            owner_kind: ClaimOwnerKind::SwarmAgent,
            reservation: evidence,
        });

        assert_eq!(
            assessment.classification,
            ClaimClassification::BlockedByActiveReservation
        );
        assert!(matches!(
            assessment.reservation,
            ReservationEvidence::Active {
                provenance: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn reservation_snapshot_absent_match_marks_no_reservation() {
        let reservations = vec![AgentMailReservationSnapshot {
            holder: "AmberLion".to_string(),
            path_pattern: "src/other.rs".to_string(),
            exclusive: true,
            reason: Some("different bead".to_string()),
            expires_ts: now() + Duration::minutes(30),
            released_ts: None,
            thread_id: Some("beads_rust-other".to_string()),
        }];
        let evidence = reservation_evidence_from_snapshots(
            "beads_rust-sc6u",
            Some("TopazFox"),
            &[],
            Some(&reservations),
            now(),
        );

        assert_eq!(evidence, ReservationEvidence::NoReservation);
    }

    #[test]
    fn reservation_snapshot_comment_path_match_is_enough_to_correlate() {
        let reservations = vec![AgentMailReservationSnapshot {
            holder: "AmberLion".to_string(),
            path_pattern: "src/coordination.rs".to_string(),
            exclusive: true,
            reason: None,
            expires_ts: now() - Duration::minutes(5),
            released_ts: None,
            thread_id: None,
        }];
        let comments = vec![Comment {
            id: 1,
            issue_id: "beads_rust-sc6u".to_string(),
            author: "TopazFox".to_string(),
            body: "files: src/coordination.rs".to_string(),
            created_at: now(),
        }];
        let evidence = reservation_evidence_from_snapshots(
            "beads_rust-sc6u",
            Some("TopazFox"),
            &comments,
            Some(&reservations),
            now(),
        );

        assert!(matches!(
            evidence,
            ReservationEvidence::Expired {
                provenance: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn advisory_emits_commands_only_for_swarm_reclaim_candidates() {
        let stale = assess_claim(input(
            120,
            ClaimOwnerKind::SwarmAgent,
            ReservationEvidence::NoReservation,
        ));
        let advisory = advisory_for_claim("bd-stale", &stale);

        assert!(advisory.reclaim_allowed_by_policy);
        assert!(!advisory.required_human_confirmation);
        assert_eq!(advisory.suggested_commands.len(), 2);
        assert_eq!(
            advisory.suggested_commands[0].purpose,
            CoordinationSuggestedCommandPurpose::AddReclaimAuditComment
        );
        assert_eq!(
            advisory.suggested_commands[1].purpose,
            CoordinationSuggestedCommandPurpose::ClaimIssue
        );
        assert!(
            advisory.suggested_commands[0]
                .command
                .contains("br comments add")
        );
        assert!(advisory.suggested_commands[1].command.contains("br update"));
        assert!(
            advisory.suggested_commands[0]
                .command
                .contains("updated_at=2026-05-08")
        );
        assert!(
            advisory.suggested_commands[0]
                .command
                .contains("assignee=TopazFox")
        );
        assert!(
            advisory.suggested_commands[0]
                .command
                .contains("stale_threshold_minutes=120")
        );
        assert!(
            advisory.suggested_commands[0]
                .command
                .contains("reservation_status=no_reservation")
        );

        let missing_snapshot = assess_claim(input(
            120,
            ClaimOwnerKind::SwarmAgent,
            ReservationEvidence::NoSnapshot,
        ));
        let advisory = advisory_for_claim("bd-stale", &missing_snapshot);

        assert!(!advisory.reclaim_allowed_by_policy);
        assert!(advisory.suggested_commands.is_empty());
    }

    #[test]
    fn advisory_requires_confirmation_for_human_or_unknown_stale_claims() {
        let human = assess_claim(input(
            24 * 60,
            ClaimOwnerKind::Human,
            ReservationEvidence::NoReservation,
        ));
        let unknown = assess_claim(input(
            24 * 60,
            ClaimOwnerKind::Unknown,
            ReservationEvidence::NoReservation,
        ));

        for assessment in [human, unknown] {
            let advisory = advisory_for_claim("bd-stale", &assessment);
            assert!(!advisory.reclaim_allowed_by_policy);
            assert!(advisory.required_human_confirmation);
            assert!(advisory.suggested_commands.is_empty());
        }
    }

    #[test]
    fn advisory_omits_commands_for_fresh_or_active_claims() {
        let fresh = assess_claim(input(
            119,
            ClaimOwnerKind::SwarmAgent,
            ReservationEvidence::NoReservation,
        ));
        let active = assess_claim(input(
            12 * 60,
            ClaimOwnerKind::SwarmAgent,
            ReservationEvidence::Active {
                holder: "TopazFox".to_string(),
                expires_at: Some(now() + Duration::minutes(30)),
                provenance: None,
            },
        ));

        for assessment in [fresh, active] {
            let advisory = advisory_for_claim("bd-stale", &assessment);
            assert!(!advisory.reclaim_allowed_by_policy);
            assert!(!advisory.required_human_confirmation);
            assert!(advisory.suggested_commands.is_empty());
        }
    }
}
