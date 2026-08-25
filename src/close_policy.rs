//! Closure-time policy gates (issue #274 — Phase 1).
//!
//! Loads `.beads/policy.yaml` (when present) and evaluates configured gates
//! against a candidate close. Every gate is opt-in. With no policy file,
//! [`ClosePolicy::is_active`] returns `false` and `br close` behaves exactly
//! as it did before this module existed.
//!
//! # Phase 1 scope
//!
//! - Required-field validation (close reason min length / regex; unchecked
//!   acceptance criteria boxes)
//! - Actor constraints (forbid self-close after `in_progress` was set by the
//!   same actor)
//! - Agent attribution Tier 1 (capture, never reject) via env vars + CLI flags
//! - Typed structured references in close reasons
//!
//! Long-form closeout documents, signatures, and full observability are
//! intentionally out of scope.

use crate::error::{BeadsError, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Default file name for the policy document inside `.beads/`.
pub const POLICY_FILE_NAME: &str = "policy.yaml";

/// Environment variable for agent name (Tier 1 attribution).
pub const ENV_AGENT_NAME: &str = "BR_AGENT_NAME";
/// Environment variable for harness identifier.
pub const ENV_HARNESS: &str = "BR_HARNESS";
/// Environment variable for model identifier.
pub const ENV_MODEL: &str = "BR_MODEL";

/// Top-level policy document loaded from `.beads/policy.yaml`.
///
/// Close-policy structs intentionally accept unknown fields rather than
/// hard-failing the parse: a single typo in `policy.yaml` used to take
/// down `br close` for every operator on the project, with no recovery
/// path (even `--bypass-policy` couldn't help because the parse fires
/// before bypass logic runs). See beads_rust#302.
///
/// Unknown fields surface via [`load_for_beads_dir`], which emits a
/// `tracing::warn!` listing every unknown key it discovered. Operators
/// who want strict parsing back can wire up a future `--strict-policy`
/// flag (out of scope for the #302 fix).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyDocument {
    /// Close-time gates.
    pub close_policy: ClosePolicy,
    /// Status-workflow policy (issue #311). Optional; when the `workflow:`
    /// section is absent the default is permissive (no enforcement), so
    /// existing repos are unaffected.
    pub workflow: Workflow,
    /// When `false`, the `--bypass-policy` CLI flag is rejected. Defaults to
    /// `true` so projects retain the standard escape hatch.
    #[serde(default = "default_true")]
    pub allow_bypass: bool,
}

impl Default for PolicyDocument {
    fn default() -> Self {
        Self {
            close_policy: ClosePolicy::default(),
            workflow: Workflow::default(),
            allow_bypass: default_true(),
        }
    }
}

const fn default_true() -> bool {
    true
}

/// Close-time policy gates.
///
/// Unknown fields are tolerated and surfaced via `tracing::warn!` at load
/// time (see `PolicyDocument` doc-comment and beads_rust#302).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClosePolicy {
    /// Reject closes whose `--reason` text is shorter than `min_length` or
    /// fails the optional anchored regex.
    pub require_close_reason: RequireCloseReason,
    /// Reject closes when the issue body's `## Acceptance Criteria` section
    /// still has unchecked `- [ ]` items.
    pub require_acceptance_criteria_satisfied: ToggleGate,
    /// Reject closes when the closing actor matches the actor who last set
    /// status to `in_progress`.
    pub forbid_self_close_after_in_progress: ToggleGate,
    /// Reject closes when the issue has a `blocks` edge to a dependent that
    /// is currently in `deferred` status (beads_rust#303). Closing a prereq
    /// is the natural touch-point that forces the closer to confront every
    /// deferred dependent before the prereq disappears from the graph.
    pub forbid_close_with_deferred_dependents: ToggleGate,
    /// Tier 1 attribution capture (default: off).
    pub attribution: Attribution,
    /// Typed structured references gate (capability #3 of #274). When
    /// `enabled`, the close `--reason` must contain at least one
    /// `kind:value` reference matching one of `required_kinds` (e.g.
    /// `commit:`, `pr:`, `reviewer:`, `investigation:`). Unknown kinds
    /// satisfy the gate only when explicitly listed in `required_kinds`.
    pub require_typed_references: RequireTypedReferences,
}

impl ClosePolicy {
    /// True when at least one gate is enabled. Used to short-circuit work for
    /// projects that have no policy.yaml or have all gates disabled.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.require_close_reason.enabled
            || self.require_acceptance_criteria_satisfied.enabled
            || self.forbid_self_close_after_in_progress.enabled
            || self.forbid_close_with_deferred_dependents.enabled
            || self.require_typed_references.enabled
            || self.attribution.tier != AttributionTier::Off
    }
}

/// Status-workflow policy (issue #311).
///
/// The `workflow:` namespace owns everything about the allowed status set and
/// (in the future, issue #312) transitions and per-transition gates. It is
/// deliberately a self-contained section so #312's `transitions:` /
/// per-transition gate config can hang off the same block without reworking
/// the schema:
///
/// ```yaml
/// workflow:
///   strict: true
///   statuses: [open, in_progress, blocked, deferred, draft, closed]
///   transitions:                       # issue #312, layer 1
///     open: [in_progress, deferred, closed]
///     in_progress: [in_review, blocked, open]
///     in_review: [closed, in_progress]
///     blocked: [open, in_progress]
///     deferred: [open]
///     # initial: [open, draft]         # statuses allowed when current is unknown
///     # any: [closed]                  # to-statuses allowed from EVERY from-status
/// ```
///
/// When the section is absent the default (`strict: false`, empty `statuses`,
/// empty `transitions`) means no enforcement, matching pre-#311 behavior
/// exactly.
///
/// Transition enforcement (issue #312, layer 1) is opt-in via a non-empty
/// `transitions:` map and is independently gated on `strict`: when
/// `strict: true` *and* `transitions` is non-empty, a status change whose
/// `from -> to` pair is not listed is rejected on `br update`. Absent or
/// empty `transitions` leaves transition behavior exactly as before.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workflow {
    /// When `true` *and* `statuses` is non-empty, a status outside the
    /// configured set is rejected on `create`/`update` and flagged by
    /// `br doctor`. When `false`, `statuses` is advisory only (no
    /// enforcement). The same flag gates transition enforcement (see
    /// `transitions`).
    pub strict: bool,
    /// The allowed status set. Each entry is a canonical-or-custom status
    /// string (e.g. `open`, `in_progress`, `closed`, or a project-specific
    /// value). Empty disables enforcement even when `strict` is `true`.
    #[serde(default)]
    pub statuses: Vec<String>,
    /// Allowed status transitions (issue #312, layer 1). A map of
    /// `from-status -> [allowed to-statuses]`. Two reserved keys widen the
    /// rules:
    ///
    /// - `any` — its to-statuses are allowed *from every* from-status (a
    ///   wildcard source; e.g. allow `closed` from anywhere).
    /// - `initial` — the to-statuses allowed when there is no recorded
    ///   current status (e.g. a `create`, or an issue whose status the caller
    ///   could not resolve). Absent `initial` means any initial status is
    ///   accepted, since there is no `from` state to validate against.
    ///
    /// Comparison is case-insensitive, mirroring `statuses`. A no-op
    /// transition (`from == to`) is always allowed and never consults the
    /// map. Empty map disables transition enforcement entirely (backward
    /// compatible).
    #[serde(default)]
    pub transitions: std::collections::BTreeMap<String, Vec<String>>,
    /// Per-transition gate rules (issue #312, layer 2). A map of
    /// `"from -> to"` (the transition the gates guard) to the set of gate
    /// conditions required before that move is allowed.
    ///
    /// ```yaml
    /// workflow:
    ///   gates:
    ///     "in_review -> closed":
    ///       require_all:
    ///         - ci_green
    ///         - min_reviewers: 1
    ///       require_if:
    ///         - label: security-sensitive
    ///           gate: security_sign_off
    ///         - priority: [0, 1]
    ///           gate: security_sign_off
    /// ```
    ///
    /// The transition key is parsed leniently: `from`/`to` are split on the
    /// first `->` (surrounding whitespace ignored), matched case-insensitively
    /// against the actual `from`/`to` of a status change. An empty map (the
    /// default) disables gate enforcement entirely — projects with no `gates:`
    /// block behave exactly as before layer 2.
    #[serde(default)]
    pub gates: std::collections::BTreeMap<String, GateRule>,
    /// Fields that must be supplied or satisfied for a status transition
    /// (GitHub #388). Keys may be an exact `"from -> to"` transition or a
    /// bare target status that applies to every transition entering that
    /// status. Exact and target rules compose, with duplicate fields removed.
    ///
    /// `transition_comment` is deliberately request-scoped: callers must
    /// supply a non-empty comment with the status mutation, and storage writes
    /// that comment in the same transaction as the transition. Historical
    /// comments never satisfy this requirement.
    #[serde(default)]
    pub required_fields: std::collections::BTreeMap<String, Vec<TransitionRequiredField>>,
    /// Named status groups (issue #354). Currently only `ready` is consumed —
    /// it defines which statuses `br ready` (and the scheduler) treat as
    /// actionable work. When the `status_groups:` block (or the `ready:` key
    /// inside it) is absent, the ready group defaults to `[open]`, preserving
    /// the pre-#354 behavior exactly.
    ///
    /// ```yaml
    /// workflow:
    ///   status_groups:
    ///     ready: [open, rework]
    /// ```
    #[serde(default)]
    pub status_groups: StatusGroups,
    /// Atomic status-capacity and cross-queue admission policy (GitHub #384).
    ///
    /// Capacity is deliberately independent from `workflow.strict`: projects
    /// may keep transition enumeration advisory while still enforcing a hard
    /// repository admission ceiling.  Once any capacity rule is configured,
    /// however, every referenced status must be declared in
    /// `workflow.statuses`; [`Workflow::validate_capacity`] rejects ambiguous
    /// or misspelled names before storage is opened.
    #[serde(default)]
    pub capacity: CapacityPolicy,
}

/// Repository-level workflow capacity policy (GitHub #384).
///
/// Phase 1 implements the race-sensitive core: hard limits for individual
/// statuses and named groups, plus transition-scoped admission rules that
/// inspect other queues.  Phase 3 adds hierarchy-aware counting over
/// parent-child edges via [`CapacityCounting`].  Phase 4 adds audited
/// issue-specific exemptions via [`CapacityExemptionPolicy`].  Phase 5 adds
/// optional multi-agent admission scopes via [`CapacityScopePolicy`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityPolicy {
    /// How occupancy is counted across parent-child hierarchies.
    pub counting: CapacityCounting,
    /// Per-status soft/hard limits, keyed by a declared workflow status.
    pub statuses: std::collections::BTreeMap<String, CapacityLimit>,
    /// Named multi-status capacity pools.
    pub groups: std::collections::BTreeMap<String, CapacityGroup>,
    /// Transition-scoped cross-queue admission guards.
    pub admission: Vec<CapacityAdmissionRule>,
    /// Audited issue-specific exemption authorization (GitHub #384 phase 4).
    pub exemptions: CapacityExemptionPolicy,
    /// Optional multi-agent admission scopes (GitHub #384 phase 5), keyed by
    /// scope name: `repository`, `actor`, `assignee`, `harness`, `session`,
    /// or `subtree`. Each scope carries its own status/group limits that
    /// compose with (never replace) the repository-level limits above.
    pub scopes: std::collections::BTreeMap<String, CapacityScopePolicy>,
}

impl CapacityPolicy {
    /// Whether any capacity behavior is configured.
    #[must_use]
    pub fn is_active(&self) -> bool {
        // Any declared scope key counts, even one with no limits yet:
        // an empty scope must fail validation loudly rather than lie
        // dormant until the operator adds its first threshold.
        !self.statuses.is_empty()
            || !self.groups.is_empty()
            || !self.admission.is_empty()
            || !self.scopes.is_empty()
    }

    /// Whether this policy has no capacity behavior to bind or enforce.
    /// Serde `skip_serializing_if` hook for receipt fields.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.is_active()
    }
}

/// Status/group limits for one admission scope (GitHub #384 phase 5).
///
/// Scoped counting is plain per-issue occupancy: hierarchy-aware counting
/// modes and admission rules remain repository-scope features. Scopes are
/// admission control for cooperating agent harnesses, not process
/// supervision — actor/harness/session keys come from self-reported
/// attribution, and a transition that carries no key for a scope is not
/// subject to that scope's limits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityScopePolicy {
    /// Per-status soft/hard limits within one scope partition.
    pub statuses: std::collections::BTreeMap<String, CapacityLimit>,
    /// Named multi-status pools within one scope partition.
    pub groups: std::collections::BTreeMap<String, CapacityGroup>,
}

impl CapacityScopePolicy {
    /// Whether this scope declares any limit.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.statuses.is_empty() || !self.groups.is_empty()
    }
}

/// The scope dimensions a capacity limit can partition on (GitHub #384
/// phase 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityScopeKind {
    /// The whole repository — identical semantics to the top-level limits.
    Repository,
    /// Partitioned by the acting CLI actor (`--actor` / `BD_ACTOR` / user).
    Actor,
    /// Partitioned by the issue's (prospective) assignee.
    Assignee,
    /// Partitioned by the self-reported harness (`--harness` / `BR_HARNESS`).
    Harness,
    /// Partitioned by the self-reported session (`BR_SESSION`).
    Session,
    /// Partitioned by the issue's root ancestor over parent-child edges.
    Subtree,
}

impl CapacityScopeKind {
    /// Every recognized scope name, in canonical order.
    pub const ALL: [Self; 6] = [
        Self::Repository,
        Self::Actor,
        Self::Assignee,
        Self::Harness,
        Self::Session,
        Self::Subtree,
    ];

    /// Canonical lowercase name used in policy keys and evidence.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::Actor => "actor",
            Self::Assignee => "assignee",
            Self::Harness => "harness",
            Self::Session => "session",
            Self::Subtree => "subtree",
        }
    }

    /// Parse a policy key (case-insensitive, trimmed).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let target = name.trim();
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str().eq_ignore_ascii_case(target))
    }
}

/// Authorization policy for audited issue-specific capacity exemptions
/// (GitHub #384 phase 4).
///
/// Exemptions are never implicit: an empty `providers` list disables
/// granting entirely, and a recorded exemption only excludes its issue from
/// counting while the granting provider remains listed here. Removing a
/// provider from the policy therefore withdraws every exemption it granted
/// without rewriting the audit history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityExemptionPolicy {
    /// Providers authorized to grant, renew, and revoke exemptions
    /// (matched case-insensitively, e.g. `operator`, `release-manager`).
    /// Empty disables granting entirely — ordinary labels or unlisted
    /// actors can never create an exemption.
    pub providers: Vec<String>,
    /// When `true`, every grant and renewal must carry an explicit
    /// expiration; open-ended exemptions are rejected.
    pub require_expiry: bool,
    /// Upper bound on how far in the future a grant or renewal may set its
    /// expiry, in seconds. `None` leaves the horizon to the operator.
    pub max_ttl_seconds: Option<u64>,
}

impl CapacityExemptionPolicy {
    /// Whether any provider is authorized to grant exemptions.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.providers.is_empty()
    }

    /// True when `provider` (case-insensitively, ignoring surrounding
    /// whitespace) is authorized by this policy.
    #[must_use]
    pub fn authorizes(&self, provider: &str) -> bool {
        let target = provider.trim();
        !target.is_empty()
            && self
                .providers
                .iter()
                .any(|candidate| candidate.trim().eq_ignore_ascii_case(target))
    }

    /// Whether this policy has no capacity behavior to bind or enforce.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.is_enabled()
    }
}

/// Soft and hard occupancy thresholds for one capacity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityLimit {
    /// Advisory threshold. Phase 2 exposes successful-transition warnings.
    pub soft: Option<u32>,
    /// Enforced threshold. A prospective count equal to this value is allowed;
    /// a transition that would exceed it is rejected atomically.
    pub hard: Option<u32>,
}

/// A named pool spanning multiple workflow statuses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityGroup {
    /// Declared workflow statuses included in this pool.
    pub statuses: Vec<String>,
    /// Advisory threshold.
    pub soft: Option<u32>,
    /// Enforced threshold.
    pub hard: Option<u32>,
}

impl CapacityGroup {
    /// View the group's thresholds through the common limit type.
    #[must_use]
    pub const fn limit(&self) -> CapacityLimit {
        CapacityLimit {
            soft: self.soft,
            hard: self.hard,
        }
    }
}

/// Source/target matcher for one admission rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityTransitionMatcher {
    /// Source statuses to which the rule applies.
    pub from: Vec<String>,
    /// Target statuses to which the rule applies.
    pub to: Vec<String>,
}

/// Queues that must remain below configured thresholds for an admission rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityRequirements {
    /// Status name -> exclusive upper bound (`count < threshold`).
    pub statuses: std::collections::BTreeMap<String, u32>,
    /// Capacity group name -> exclusive upper bound (`count < threshold`).
    pub groups: std::collections::BTreeMap<String, u32>,
}

/// A transition-scoped cross-queue admission rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityAdmissionRule {
    /// Stable operator-facing name used in diagnostics and policy paths.
    pub name: String,
    /// Source and target statuses matched by this rule.
    pub transitions: CapacityTransitionMatcher,
    /// Every referenced queue must have a prospective count below its bound.
    pub require_below: CapacityRequirements,
}

/// Hierarchy-aware counting configuration for capacity evaluation
/// (GitHub #384, phase 3: "Hierarchy-aware counting").
///
/// Only `parent-child` dependency edges participate in hierarchy
/// accounting; `blocks` and `related` edges never affect counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityCounting {
    /// Counting mode applied to every configured capacity.
    pub hierarchy: CapacityCountingMode,
    /// Explicit weights; only meaningful under `hierarchy: weighted`.
    pub weights: CapacityWeights,
}

/// Counting mode for capacity occupancy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityCountingMode {
    /// Every matching issue counts (the phase-1 behavior and the default).
    #[default]
    All,
    /// An issue does not count while a parent-child descendant is already
    /// counted in the same capacity: active leaves count one, aggregate
    /// parents count zero, and a parent begins counting when its last
    /// active descendant leaves the capacity.
    LeafWork,
    /// Count active work streams by their highest matching ancestor: an
    /// active issue counts only when no parent-child ancestor is active in
    /// the same capacity.
    Roots,
    /// Sum explicit per-issue / per-type weights over matching issues.
    Weighted,
}

impl CapacityCountingMode {
    /// Stable wire string used in structured evidence (`counting_mode`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::LeafWork => "leaf_work",
            Self::Roots => "roots",
            Self::Weighted => "weighted",
        }
    }
}

/// Explicit occupancy weights for [`CapacityCountingMode::Weighted`].
///
/// Zero weights are deliberately legal: they are the audited, visible way
/// to declare that a parent or epic represents no independent execution
/// beyond its children.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityWeights {
    /// Weight for issues with no per-issue or per-type entry (1 when absent).
    pub default: Option<u32>,
    /// Weight per issue type, matched case-insensitively.
    pub types: std::collections::BTreeMap<String, u32>,
    /// Weight per issue ID (exact match); wins over type and default.
    pub issues: std::collections::BTreeMap<String, u32>,
}

impl CapacityWeights {
    /// Whether any weight is explicitly configured.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.default.is_some() || !self.types.is_empty() || !self.issues.is_empty()
    }

    /// Resolve the effective weight for one issue: per-issue entry, then
    /// per-type entry (case-insensitive), then the configured default,
    /// then 1.
    #[must_use]
    pub fn weight_for(&self, issue_id: &str, issue_type: &str) -> u32 {
        if let Some(weight) = self.issues.get(issue_id) {
            return *weight;
        }
        if let Some(weight) = self
            .types
            .iter()
            .find(|(name, _)| name.trim().eq_ignore_ascii_case(issue_type.trim()))
            .map(|(_, weight)| *weight)
        {
            return weight;
        }
        self.default.unwrap_or(1)
    }
}

/// Structured evidence for a hard workflow-capacity rejection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowCapacityViolation {
    pub issue_id: String,
    pub from_status: Option<String>,
    pub to_status: String,
    pub capacity_kind: String,
    pub capacity_name: String,
    pub scope: String,
    /// Partition key within a non-repository scope (GitHub #384 phase 5):
    /// the acting actor/harness/session, the issue's prospective assignee,
    /// or the subtree root id. Absent for repository-scope evidence, keeping
    /// the pre-scope shape byte-stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_key: Option<String>,
    pub counting_mode: String,
    /// Prospective active issues excluded from the count as hierarchy
    /// aggregates (`leaf_work`/`roots` only; absent under `all`/`weighted`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate_parents_excluded: Option<u32>,
    /// Prospective occupancy excluded because the issues hold active,
    /// authorized exemptions for this capacity (absent when zero, keeping
    /// the pre-exemption evidence shape byte-stable). Same unit as
    /// `current`/`prospective`: issue counts, or weights under `weighted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exempt: Option<u32>,
    pub current: u32,
    pub prospective: u32,
    pub soft_limit: Option<u32>,
    pub hard_limit: u32,
    pub policy_path: String,
}

/// Structured evidence emitted after a successful transition reaches or
/// exceeds an advisory workflow-capacity threshold.
///
/// Warnings deliberately mirror the hard-rejection evidence so human and
/// machine consumers can use one stable vocabulary. `hard_limit` remains
/// optional because a project may configure an advisory ceiling without a
/// corresponding hard stop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowCapacityWarning {
    pub issue_id: String,
    pub from_status: Option<String>,
    pub to_status: String,
    pub capacity_kind: String,
    pub capacity_name: String,
    pub scope: String,
    /// Partition key within a non-repository scope (GitHub #384 phase 5).
    /// Absent for repository-scope evidence, keeping the pre-scope shape
    /// byte-stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_key: Option<String>,
    pub counting_mode: String,
    /// Prospective active issues excluded from the count as hierarchy
    /// aggregates (`leaf_work`/`roots` only; absent under `all`/`weighted`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate_parents_excluded: Option<u32>,
    /// Prospective occupancy excluded because the issues hold active,
    /// authorized exemptions for this capacity (absent when zero, keeping
    /// the pre-exemption evidence shape byte-stable). Same unit as
    /// `current`/`prospective`: issue counts, or weights under `weighted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exempt: Option<u32>,
    pub current: u32,
    pub prospective: u32,
    pub soft_limit: u32,
    pub hard_limit: Option<u32>,
    pub policy_path: String,
}

/// Human-readable qualifier appended to capacity diagnostics when a
/// non-default counting mode produced the numbers. Empty for `all` so the
/// phase-1 message text stays byte-stable.
fn counting_mode_display_suffix(counting_mode: &str, excluded: Option<u32>) -> String {
    if counting_mode == "all" {
        return String::new();
    }
    excluded.map_or_else(
        || format!(", counting: {counting_mode}"),
        |excluded| format!(", counting: {counting_mode}, aggregate-excluded: {excluded}"),
    )
}

/// Human-readable qualifier appended to capacity diagnostics when active
/// exemptions excluded occupancy from the counted totals. Empty when no
/// exemption affected the numbers so pre-exemption message text stays
/// byte-stable.
fn exempt_display_suffix(exempt: Option<u32>) -> String {
    exempt.map_or_else(String::new, |excluded| format!(", exempt: {excluded}"))
}

/// Human-readable qualifier naming the scope partition key for scoped
/// capacity diagnostics (GitHub #384 phase 5). Empty for repository-scope
/// evidence so pre-scope message text stays byte-stable.
fn scope_key_display_suffix(scope_key: Option<&str>) -> String {
    scope_key.map_or_else(String::new, |key| format!(" for '{key}'"))
}

impl std::fmt::Display for WorkflowCapacityWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let from = self.from_status.as_deref().unwrap_or("<initial>");
        let counting = format!(
            "{}{}",
            counting_mode_display_suffix(&self.counting_mode, self.aggregate_parents_excluded),
            exempt_display_suffix(self.exempt),
        );
        write!(
            f,
            "transitioned {} from {} to {}; {} {} capacity '{}'{} has reached or exceeded its soft limit (current: {}, prospective: {}, soft: {}{}; policy: {}). Drain existing work before admitting more",
            self.issue_id,
            from,
            self.to_status,
            self.scope,
            self.capacity_kind,
            self.capacity_name,
            scope_key_display_suffix(self.scope_key.as_deref()),
            self.current,
            self.prospective,
            self.soft_limit,
            counting,
            self.policy_path,
        )
    }
}

impl std::fmt::Display for WorkflowCapacityViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let from = self.from_status.as_deref().unwrap_or("<initial>");
        let counting = format!(
            "{}{}",
            counting_mode_display_suffix(&self.counting_mode, self.aggregate_parents_excluded),
            exempt_display_suffix(self.exempt),
        );
        if self.capacity_kind.starts_with("admission_") {
            return write!(
                f,
                "cannot transition {} from {} to {}: {} admission guard '{}' requires the observed queue to remain below {} (current: {}, prospective: {}{}; policy: {})",
                self.issue_id,
                from,
                self.to_status,
                self.scope,
                self.capacity_name,
                self.hard_limit,
                self.current,
                self.prospective,
                counting,
                self.policy_path,
            );
        }
        write!(
            f,
            "cannot transition {} from {} to {}: {} {} capacity '{}'{} would exceed its hard limit (current: {}, prospective: {}, hard: {}{}; policy: {})",
            self.issue_id,
            from,
            self.to_status,
            self.scope,
            self.capacity_kind,
            self.capacity_name,
            scope_key_display_suffix(self.scope_key.as_deref()),
            self.current,
            self.prospective,
            self.hard_limit,
            counting,
            self.policy_path,
        )
    }
}

/// Named status groups under `workflow.status_groups` (issue #354).
///
/// The `ready` group is the set of statuses `br ready` surfaces as actionable
/// work. An empty/absent `ready` list means "use the default" — see
/// [`Workflow::ready_status_group`], which substitutes `[open]` so existing
/// repos behave exactly as before this field existed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusGroups {
    /// Statuses treated as "ready to work on" by `br ready`. Empty means the
    /// default group (`[open]`).
    #[serde(default)]
    pub ready: Vec<String>,
}

/// The canonical default ready status group when none is configured.
pub const DEFAULT_READY_STATUS: &str = "open";

/// The set of gate conditions guarding a single `"from -> to"` transition.
///
/// All `require_all` gates must pass unconditionally. Each `require_if` entry
/// adds its `gate` to the required set only when its condition (a `label` or a
/// `priority`) matches the issue being moved. The two lists compose: a gate
/// can be listed in both, and a conditional gate that does not match simply
/// drops out of the required set for that issue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateRule {
    /// Gates required unconditionally for this transition.
    #[serde(default)]
    pub require_all: Vec<GateSpec>,
    /// Gates required only when their condition matches the issue.
    #[serde(default)]
    pub require_if: Vec<ConditionalGate>,
}

/// A field that workflow policy may require for a status transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionRequiredField {
    /// The issue must have non-empty acceptance criteria with no unchecked
    /// checklist items after applying the prospective update.
    AcceptanceCriteria,
    /// The transition request must carry a new, non-empty comment that is
    /// committed atomically with the status change.
    TransitionComment,
}

/// A single gate condition. Two shapes are accepted in YAML, mirroring the
/// issue spec:
///
/// - A bare string naming an external/internal provider gate
///   (`ci_green`, `security_sign_off`, ...): `- ci_green`.
/// - A single-key mapping `{ min_reviewers: N }` for the built-in
///   reviewer-count gate: `- min_reviewers: 1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateSpec {
    /// A named gate satisfied by a recorded `pass` result from a provider of
    /// the same name (`br gate report <id> --provider <name> --status pass`).
    Named(String),
    /// The built-in `min_reviewers` gate: at least `n` distinct reviewers must
    /// have reported `pass` (provider name `reviewer`, or any provider whose
    /// name starts with `reviewer`).
    MinReviewers(u32),
}

impl GateSpec {
    /// Stable identifier for this gate, used as the required-gate key and in
    /// `report`/`list` output. `min_reviewers` collapses to the canonical
    /// `min_reviewers` name regardless of `n`.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Named(name) => name.as_str(),
            Self::MinReviewers(_) => GATE_MIN_REVIEWERS,
        }
    }
}

impl Serialize for GateSpec {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Named(name) => serializer.serialize_str(name),
            Self::MinReviewers(n) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(GATE_MIN_REVIEWERS, n)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for GateSpec {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Named(String),
            Map(std::collections::BTreeMap<String, serde_yml::Value>),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Named(name) => Ok(Self::Named(name)),
            Raw::Map(map) => {
                if let Some(value) = map.get(GATE_MIN_REVIEWERS) {
                    let n = value.as_u64().ok_or_else(|| {
                        serde::de::Error::custom("min_reviewers must be a non-negative integer")
                    })?;
                    let n = u32::try_from(n)
                        .map_err(|_| serde::de::Error::custom("min_reviewers is too large"))?;
                    Ok(Self::MinReviewers(n))
                } else if let Some((key, _)) = map.iter().next() {
                    Err(serde::de::Error::custom(format!(
                        "unknown gate spec key '{key}' (expected a bare gate name or `min_reviewers: N`)"
                    )))
                } else {
                    Err(serde::de::Error::custom(
                        "empty gate spec (expected a bare gate name or `min_reviewers: N`)",
                    ))
                }
            }
        }
    }
}

/// A conditionally-required gate. The condition (`label` and/or `priority`)
/// is matched against the issue being transitioned; when it matches, `gate`
/// is added to the required set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConditionalGate {
    /// Required label (case-insensitive). When set, the issue must carry this
    /// label for the condition to match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Required priorities. When non-empty, the issue's priority must be one
    /// of these for the condition to match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub priority: Vec<i32>,
    /// The gate to require when the condition matches. May be a named gate or
    /// the built-in `min_reviewers` form via `allowed_*`/`min_reviewers`.
    #[serde(default)]
    pub gate: GateSpec,
}

impl Default for GateSpec {
    fn default() -> Self {
        Self::Named(String::new())
    }
}

/// Canonical name of the built-in reviewer-count gate.
pub const GATE_MIN_REVIEWERS: &str = "min_reviewers";
/// Provider-name prefix recognised as a reviewer for the `min_reviewers` gate.
pub const REVIEWER_PROVIDER_PREFIX: &str = "reviewer";

/// A single recorded gate result for an issue (one provider's verdict).
///
/// `passed` is the effective verdict. `provider` is the reporting system /
/// person (`ci`, `security`, `reviewer:alice`, ...). Stored per
/// `(issue_id, gate, provider)`; a re-report from the same provider for the
/// same gate overwrites the prior verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateResult {
    pub gate: String,
    pub provider: String,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One append-only workflow-gate verdict bound to a status revision and
/// target transition (GitHub #388).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateResultRecord {
    pub id: i64,
    pub issue_id: String,
    pub from_status: String,
    pub to_status: String,
    pub status_revision: i64,
    pub gate: String,
    pub provider: String,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_by: Option<String>,
    pub recorded_at: String,
}

/// The latest capacity-exemption state for one `(issue, capacity)` pair
/// (GitHub #384 phase 4). Like `gate_results`, this is project-local
/// auxiliary metadata: never synced through JSONL. The append-only
/// companion [`CapacityExemptionHistoryRecord`] preserves every action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityExemptionRecord {
    pub issue_id: String,
    /// `status` or `group` — the capacity family the exemption names.
    pub capacity_kind: String,
    /// Canonical (trimmed, lowercased) status or group name.
    pub capacity_name: String,
    /// Authorizing provider from `workflow.capacity.exemptions.providers`.
    pub provider: String,
    /// Mandatory human rationale recorded at grant time.
    pub reason: String,
    /// Actor who ran the granting command (audit attribution).
    pub granted_by: String,
    pub granted_at: String,
    /// RFC3339 expiry; `None` only when policy does not require one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Set when the exemption stopped applying (revoked, expired, or the
    /// issue left the applicable status set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// Why it ended: `revoked`, `expired`, or `left_status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_by: Option<String>,
    /// Derived display state: `active`, `expired`, `revoked`, or
    /// `left_status`. Computed at read time so listing never mutates.
    pub state: String,
}

/// One append-only capacity-exemption audit action (GitHub #384 phase 4):
/// `grant`, `renew`, `revoke`, `expire`, or `left_status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityExemptionHistoryRecord {
    pub id: i64,
    pub issue_id: String,
    pub capacity_kind: String,
    pub capacity_name: String,
    pub action: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub recorded_at: String,
}

impl Workflow {
    /// True when gate enforcement is configured: `strict` is on *and* at least
    /// one `"from -> to"` gate rule is listed. Enforcement short-circuits on
    /// `false`, so projects with no `gates:` block are unaffected.
    #[must_use]
    pub fn gates_enforced(&self) -> bool {
        self.strict && !self.gates.is_empty()
    }

    /// Look up the gate rule guarding the `from -> to` transition, matching the
    /// transition key case-insensitively. Returns `None` when no rule guards
    /// this move.
    #[must_use]
    pub fn gate_rule_for(&self, from: &str, to: &str) -> Option<&GateRule> {
        self.gates.iter().find_map(|(key, rule)| {
            parse_transition_key(key).filter(|(rule_from, rule_to)| {
                rule_from.eq_ignore_ascii_case(from) && rule_to.eq_ignore_ascii_case(to)
            })?;
            Some(rule)
        })
    }

    /// Compute the set of gates required to move `from -> to` for an issue with
    /// the given `labels` and `priority`. Combines `require_all` (always) with
    /// any matching `require_if` entries. Returns an empty vec when no rule
    /// guards the transition. De-duplicated by gate id, preserving source
    /// order (require_all first, then require_if).
    #[must_use]
    pub fn required_gates_for(
        &self,
        from: &str,
        to: &str,
        labels: &[String],
        priority: i32,
    ) -> Vec<GateSpec> {
        let Some(rule) = self.gate_rule_for(from, to) else {
            return Vec::new();
        };
        let mut out: Vec<GateSpec> = Vec::new();
        let push_unique = |spec: &GateSpec, out: &mut Vec<GateSpec>| {
            if !out
                .iter()
                .any(|seen| seen.id().eq_ignore_ascii_case(spec.id()))
            {
                out.push(spec.clone());
            }
        };
        for spec in &rule.require_all {
            push_unique(spec, &mut out);
        }
        for conditional in &rule.require_if {
            if conditional_matches(conditional, labels, priority) {
                push_unique(&conditional.gate, &mut out);
            }
        }
        out
    }

    /// Compute required fields for one transition. Exact `from -> to` rules
    /// and bare target-status rules compose; each field appears at most once.
    #[must_use]
    pub fn required_fields_for(
        &self,
        from: Option<&str>,
        to: &str,
    ) -> Vec<TransitionRequiredField> {
        let mut required = Vec::new();
        for (key, fields) in &self.required_fields {
            let matches = if let Some((rule_from, rule_to)) = parse_transition_key(key) {
                from.is_some_and(|actual_from| {
                    rule_from.eq_ignore_ascii_case(actual_from) && rule_to.eq_ignore_ascii_case(to)
                })
            } else {
                key.trim().eq_ignore_ascii_case(to)
            };
            if !matches {
                continue;
            }
            for field in fields {
                if !required.contains(field) {
                    required.push(*field);
                }
            }
        }
        required
    }

    /// Validate transition-required-field policy at load time.
    ///
    /// # Errors
    ///
    /// Returns a validation error for an empty key/list, malformed transition,
    /// or a status outside a configured strict status vocabulary.
    pub fn validate_required_fields(&self) -> Result<()> {
        for (raw_key, fields) in &self.required_fields {
            let key = raw_key.trim();
            if key.is_empty() {
                return Err(BeadsError::validation(
                    "workflow.required_fields",
                    "required-field rule keys must not be empty",
                ));
            }
            if fields.is_empty() {
                return Err(BeadsError::validation(
                    "workflow.required_fields",
                    format!("required-field rule '{key}' must name at least one field"),
                ));
            }

            if key.match_indices("->").count() > 1 {
                return Err(BeadsError::validation(
                    "workflow.required_fields",
                    format!(
                        "malformed required-field transition '{key}' (expected exactly one 'from -> to' separator or a bare target status)"
                    ),
                ));
            }

            if let Some((from, to)) = parse_transition_key(key) {
                if self.is_enforced() && (!self.allows(from) || !self.allows(to)) {
                    return Err(BeadsError::validation(
                        "workflow.required_fields",
                        format!(
                            "required-field transition '{key}' references a status outside the configured workflow: {}",
                            self.allowed_list()
                        ),
                    ));
                }
            } else {
                if key.contains("->") {
                    return Err(BeadsError::validation(
                        "workflow.required_fields",
                        format!(
                            "malformed required-field transition '{key}' (expected 'from -> to' or a bare target status)"
                        ),
                    ));
                }
                if self.is_enforced() && !self.allows(key) {
                    return Err(BeadsError::validation(
                        "workflow.required_fields",
                        format!(
                            "required-field target '{key}' is outside the configured workflow: {}",
                            self.allowed_list()
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Evaluate policy-required fields against the prospective issue state and
/// request-scoped transition comment.
#[must_use]
pub fn evaluate_transition_required_fields(
    workflow: &Workflow,
    issue_id: &str,
    from: Option<&str>,
    to: &str,
    acceptance_criteria: Option<&str>,
    transition_comment: Option<&str>,
) -> Vec<PolicyViolation> {
    let required = workflow.required_fields_for(from, to);
    let mut violations = Vec::new();

    for field in required {
        match field {
            TransitionRequiredField::AcceptanceCriteria => {
                let criteria = acceptance_criteria.map(str::trim).unwrap_or_default();
                if criteria.is_empty() {
                    violations.push(PolicyViolation {
                        gate: "transition_acceptance_criteria_missing".to_string(),
                        message: format!(
                            "transition '{} -> {to}' requires non-empty acceptance criteria for {issue_id}",
                            from.unwrap_or("initial")
                        ),
                        detail: Some(serde_json::json!({
                            "issue_id": issue_id,
                            "from": from,
                            "to": to,
                            "required_field": "acceptance_criteria",
                            "reason": "missing",
                        })),
                    });
                    continue;
                }

                // This value comes from the dedicated acceptance_criteria
                // column, so every checklist item in it is a criterion even
                // when operators organize the field with custom headings.
                let unchecked = find_unchecked_checklist_items(criteria);
                if !unchecked.is_empty() {
                    violations.push(PolicyViolation {
                        gate: "transition_acceptance_criteria_unchecked".to_string(),
                        message: format!(
                            "transition '{} -> {to}' requires all acceptance criteria to be satisfied for {issue_id}; {} unchecked item(s) remain",
                            from.unwrap_or("initial"),
                            unchecked.len()
                        ),
                        detail: Some(serde_json::json!({
                            "issue_id": issue_id,
                            "from": from,
                            "to": to,
                            "required_field": "acceptance_criteria",
                            "reason": "unchecked",
                            "unchecked": unchecked,
                        })),
                    });
                }
            }
            TransitionRequiredField::TransitionComment => {
                if transition_comment.map(str::trim).is_none_or(str::is_empty) {
                    violations.push(PolicyViolation {
                        gate: "transition_comment_missing".to_string(),
                        message: format!(
                            "transition '{} -> {to}' requires a new non-empty transition comment for {issue_id}",
                            from.unwrap_or("initial")
                        ),
                        detail: Some(serde_json::json!({
                            "issue_id": issue_id,
                            "from": from,
                            "to": to,
                            "required_field": "transition_comment",
                            "reason": "missing",
                        })),
                    });
                }
            }
        }
    }
    violations
}

/// Parse a `"from -> to"` transition key into its two sides, trimming
/// whitespace. Returns `None` when the key has no `->` separator or either
/// side is empty.
fn parse_transition_key(key: &str) -> Option<(&str, &str)> {
    let (from, to) = key.split_once("->")?;
    let from = from.trim();
    let to = to.trim();
    if from.is_empty() || to.is_empty() {
        return None;
    }
    Some((from, to))
}

/// True when a conditional gate's condition matches the issue. An empty
/// condition (no `label`, no `priority`) always matches. When both are set,
/// both must match (logical AND).
fn conditional_matches(conditional: &ConditionalGate, labels: &[String], priority: i32) -> bool {
    if let Some(required_label) = conditional.label.as_deref()
        && !labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case(required_label))
    {
        return false;
    }
    conditional.priority.is_empty() || conditional.priority.contains(&priority)
}

/// Evaluate the configured gates for a `from -> to` transition against the
/// recorded gate results, returning a [`PolicyViolation`] per unsatisfied gate
/// (empty when every required gate passes or no rule guards the move).
///
/// A `Named` gate is satisfied by at least one recorded `pass` from a provider
/// of the same name. `MinReviewers(n)` is satisfied by at least `n` distinct
/// reviewer providers (provider == `reviewer` or starting with `reviewer`)
/// that recorded `pass`.
#[must_use]
pub fn evaluate_gates(
    workflow: &Workflow,
    issue_id: &str,
    from: &str,
    to: &str,
    labels: &[String],
    priority: i32,
    results: &[GateResult],
) -> Vec<PolicyViolation> {
    let required = workflow.required_gates_for(from, to, labels, priority);
    let mut violations = Vec::new();
    for spec in &required {
        match spec {
            GateSpec::Named(name) => {
                let passed = gate_spec_satisfied(spec, results);
                if !passed {
                    let failing: Vec<&str> = results
                        .iter()
                        .filter(|r| r.gate.eq_ignore_ascii_case(name) && !r.passed)
                        .map(|r| r.provider.as_str())
                        .collect();
                    let detail_note = if failing.is_empty() {
                        "no provider has reported a pass".to_string()
                    } else {
                        format!("failing provider(s): {}", failing.join(", "))
                    };
                    violations.push(PolicyViolation {
                        gate: format!("gate_{name}"),
                        message: format!(
                            "transition '{from}' -> '{to}' requires gate '{name}' to pass for {issue_id}, \
                             but {detail_note}. Record a result with \
                             `br gate report {issue_id} --gate {name} --provider <name> --status pass --to {to}`."
                        ),
                        detail: Some(serde_json::json!({
                            "issue_id": issue_id,
                            "gate": name,
                            "from": from,
                            "to": to,
                            "failing_providers": failing,
                        })),
                    });
                }
            }
            GateSpec::MinReviewers(n) => {
                let mut reviewers: Vec<&str> = results
                    .iter()
                    .filter(|r| r.passed && is_reviewer_provider(&r.provider))
                    .map(|r| r.provider.as_str())
                    .collect();
                reviewers.sort_unstable();
                reviewers.dedup();
                let count = u32::try_from(reviewers.len()).unwrap_or(u32::MAX);
                if !gate_spec_satisfied(spec, results) {
                    violations.push(PolicyViolation {
                        gate: format!("gate_{GATE_MIN_REVIEWERS}"),
                        message: format!(
                            "transition '{from}' -> '{to}' requires at least {n} reviewer(s) to pass for \
                             {issue_id}, but only {count} distinct reviewer(s) have. Record a review with \
                             `br gate report {issue_id} --gate {GATE_MIN_REVIEWERS} --provider reviewer:<who> --status pass --to {to}`."
                        ),
                        detail: Some(serde_json::json!({
                            "issue_id": issue_id,
                            "gate": GATE_MIN_REVIEWERS,
                            "from": from,
                            "to": to,
                            "required": n,
                            "actual": count,
                            "reviewers": reviewers,
                        })),
                    });
                }
            }
        }
    }
    violations
}

/// Whether one required gate is satisfied by an effective scoped result set.
/// Kept separate so storage can distinguish a genuinely missing gate from a
/// pass that satisfied an earlier, now-stale status revision.
#[must_use]
pub(crate) fn gate_spec_satisfied(spec: &GateSpec, results: &[GateResult]) -> bool {
    match spec {
        GateSpec::Named(name) => results
            .iter()
            .any(|result| result.gate.eq_ignore_ascii_case(name) && result.passed),
        GateSpec::MinReviewers(required) => {
            let mut reviewers = results
                .iter()
                .filter(|result| result.passed && is_reviewer_provider(&result.provider))
                .map(|result| result.provider.to_ascii_lowercase())
                .collect::<Vec<_>>();
            reviewers.sort_unstable();
            reviewers.dedup();
            u32::try_from(reviewers.len()).unwrap_or(u32::MAX) >= *required
        }
    }
}

/// True when `provider` is recognised as a reviewer for the `min_reviewers`
/// gate: the bare `reviewer` provider, or a namespaced `reviewer:<who>` /
/// `reviewer-<who>` provider.
#[must_use]
pub fn is_reviewer_provider(provider: &str) -> bool {
    let lower = provider.to_ascii_lowercase();
    lower == REVIEWER_PROVIDER_PREFIX
        || lower.starts_with(&format!("{REVIEWER_PROVIDER_PREFIX}:"))
        || lower.starts_with(&format!("{REVIEWER_PROVIDER_PREFIX}-"))
}

/// Reserved `transitions` key whose to-statuses are allowed from every
/// from-status (wildcard source).
pub const TRANSITION_ANY_FROM: &str = "any";
/// Reserved `transitions` key whose to-statuses are allowed when there is no
/// recorded current status (e.g. a create, or an unresolved current status).
pub const TRANSITION_INITIAL: &str = "initial";

impl Workflow {
    /// True when strict enforcement is configured: `strict` is on *and* at
    /// least one allowed status is listed. Enforcement and the doctor
    /// detector short-circuit on `false`.
    #[must_use]
    pub fn is_enforced(&self) -> bool {
        self.strict && !self.statuses.is_empty()
    }

    /// Resolve the configured ready status group (issue #354), substituting the
    /// default `[open]` when nothing is configured. Returns lowercased,
    /// source-order, de-duplicated status names so the query layer can build a
    /// stable `status IN (...)` clause. The default is returned whenever
    /// `workflow.status_groups.ready` is empty, which is the unconfigured case —
    /// preserving pre-#354 behavior exactly.
    #[must_use]
    pub fn ready_status_group(&self) -> Vec<String> {
        let configured = &self.status_groups.ready;
        let source: Vec<String> = if configured.is_empty() {
            vec![DEFAULT_READY_STATUS.to_string()]
        } else {
            configured.clone()
        };
        let mut out: Vec<String> = Vec::with_capacity(source.len());
        for status in source {
            let normalized = status.trim().to_lowercase();
            if normalized.is_empty() {
                continue;
            }
            if !out.contains(&normalized) {
                out.push(normalized);
            }
        }
        if out.is_empty() {
            out.push(DEFAULT_READY_STATUS.to_string());
        }
        out
    }

    /// Validate the configured ready status group (issue #354). When
    /// `workflow.strict` is set *and* `workflow.statuses` is non-empty, every
    /// member of the configured ready group must appear in the allowed status
    /// set; an out-of-vocabulary member is rejected with a clear error. When
    /// not strict (or no `statuses` configured, or no `ready` group
    /// configured), the group is accepted as-is.
    ///
    /// # Errors
    ///
    /// Returns a validation error when strict enforcement is configured and a
    /// member of the ready group is not in `workflow.statuses`.
    pub fn validate_ready_status_group(&self) -> Result<()> {
        // Only validate an explicitly-configured group; the implicit `[open]`
        // default is never rejected.
        if self.status_groups.ready.is_empty() {
            return Ok(());
        }
        if !self.is_enforced() {
            return Ok(());
        }
        let unknown: Vec<String> = self
            .ready_status_group()
            .into_iter()
            .filter(|status| !self.allows(status))
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        Err(BeadsError::validation(
            "workflow.status_groups.ready",
            format!(
                "ready status group contains status(es) not permitted by the project workflow \
                 policy (.beads/policy.yaml workflow.strict): {}. Allowed statuses: {}.",
                unknown.join(", "),
                self.allowed_list()
            ),
        ))
    }

    /// Validate every configured repository-capacity reference and threshold.
    ///
    /// Capacity misspellings must fail closed: unlike advisory workflow
    /// extensions, a silently ignored queue name would make an operator believe
    /// admission is bounded when it is not.  Validation is case-insensitive in
    /// the same way as status parsing and transition matching.
    ///
    /// # Errors
    ///
    /// Returns a validation error when a referenced status/group is unknown,
    /// a name/list is empty, thresholds are zero or inverted, or admission rule
    /// names collide case-insensitively.
    pub fn validate_capacity(&self) -> Result<()> {
        // Counting configuration is validated even when no limit is active:
        // a typo'd weights block must fail loudly rather than lie dormant
        // until an operator adds their first hard limit. Exemption
        // authorization gets the same treatment.
        validate_capacity_counting(&self.capacity.counting)?;
        validate_capacity_exemptions(&self.capacity.exemptions)?;
        if !self.capacity.is_active() {
            return Ok(());
        }
        if self.statuses.is_empty() {
            return Err(capacity_validation_error(
                "capacity requires workflow.statuses to declare every status it references",
            ));
        }

        let declared_statuses = declared_capacity_statuses(&self.statuses)?;
        validate_status_capacities(&self.capacity.statuses, &declared_statuses)?;
        let group_names = validate_capacity_groups(&self.capacity.groups, &declared_statuses)?;
        validate_capacity_admission_rules(
            &self.capacity.admission,
            &declared_statuses,
            &group_names,
        )?;
        validate_capacity_scopes(&self.capacity.scopes, &declared_statuses)?;
        Ok(())
    }

    /// True when `status` (case-insensitively) is in the configured set.
    /// Comparison mirrors [`crate::model::Status`] parsing: canonical names
    /// are matched case-insensitively, so a config entry of `In_Progress`
    /// still admits the canonical `in_progress`.
    #[must_use]
    pub fn allows(&self, status: &str) -> bool {
        let target = status.to_lowercase();
        self.statuses
            .iter()
            .any(|allowed| allowed.to_lowercase() == target)
    }

    /// Comma-separated, source-order list of the allowed statuses for error
    /// messages. Empty string when nothing is configured.
    #[must_use]
    pub fn allowed_list(&self) -> String {
        self.statuses.join(", ")
    }

    /// Validate a target status against the workflow policy. Returns `Ok(())`
    /// when enforcement is off, the status set is empty, or the status is in
    /// the set. Returns a [`BeadsError::Validation`] naming the allowed values
    /// otherwise.
    ///
    /// # Errors
    ///
    /// Returns a validation error when strict enforcement is configured and
    /// `status` is not in the allowed set.
    pub fn validate_status(&self, status: &str) -> Result<()> {
        if !self.is_enforced() || self.allows(status) {
            return Ok(());
        }
        Err(BeadsError::validation(
            "status",
            format!(
                "status '{status}' is not permitted by the project workflow policy \
                 (.beads/policy.yaml workflow.strict). Allowed statuses: {}.",
                self.allowed_list()
            ),
        ))
    }

    /// True when transition enforcement is configured: `strict` is on *and*
    /// at least one `from -> [to...]` rule is listed. Enforcement
    /// short-circuits on `false`.
    #[must_use]
    pub fn transitions_enforced(&self) -> bool {
        self.strict && !self.transitions.is_empty()
    }

    /// Case-insensitive lookup of the to-statuses listed for `from`. Returns
    /// `None` when `from` has no entry in the map.
    fn transitions_from(&self, from: &str) -> Option<&Vec<String>> {
        let target = from.to_lowercase();
        self.transitions
            .iter()
            .find(|(key, _)| key.to_lowercase() == target)
            .map(|(_, tos)| tos)
    }

    /// Collect every to-status reachable from `from`, merging the explicit
    /// `from` entry with the wildcard `any` entry. Returns the source-order,
    /// de-duplicated list used for error messages and membership checks.
    #[must_use]
    pub fn allowed_targets_from(&self, from: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let push_unique = |list: &Vec<String>, out: &mut Vec<String>| {
            for value in list {
                if !out.iter().any(|seen| seen.eq_ignore_ascii_case(value)) {
                    out.push(value.clone());
                }
            }
        };
        if let Some(explicit) = self.transitions_from(from) {
            push_unique(explicit, &mut out);
        }
        if let Some(wildcard) = self.transitions_from(TRANSITION_ANY_FROM) {
            push_unique(wildcard, &mut out);
        }
        out
    }

    /// True when moving `from -> to` is permitted by the configured
    /// transition rules. A no-op (`from == to`, case-insensitive) is always
    /// allowed. Otherwise the target must appear either under the explicit
    /// `from` key or under the wildcard `any` key.
    #[must_use]
    pub fn allows_transition(&self, from: &str, to: &str) -> bool {
        if from.eq_ignore_ascii_case(to) {
            return true;
        }
        self.allowed_targets_from(from)
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(to))
    }

    /// True when `to` is permitted as an initial status (no recorded `from`).
    /// When no `initial` key is configured, any initial status is accepted —
    /// there is no prior state to validate against.
    #[must_use]
    pub fn allows_initial(&self, to: &str) -> bool {
        match self.transitions_from(TRANSITION_INITIAL) {
            None => true,
            Some(allowed) => allowed.iter().any(|s| s.eq_ignore_ascii_case(to)),
        }
    }

    /// Validate a status *change* against the workflow transition rules.
    ///
    /// `from` is the issue's current status, or `None` when there is no
    /// recorded current status (a create, or an unresolved current status —
    /// validated against the reserved `initial` key).
    ///
    /// Returns `Ok(())` when transition enforcement is off, the move is a
    /// no-op, or the move is permitted. Returns a [`BeadsError::Validation`]
    /// naming the current status, the attempted status, and the valid next
    /// statuses otherwise — mirroring the `validate_status` error style.
    ///
    /// # Errors
    ///
    /// Returns a validation error when transition enforcement is configured
    /// and the `from -> to` move is not in the allowed set.
    pub fn validate_transition(&self, from: Option<&str>, to: &str) -> Result<()> {
        if !self.transitions_enforced() {
            return Ok(());
        }

        match from {
            None => {
                if self.allows_initial(to) {
                    return Ok(());
                }
                let allowed = self
                    .transitions_from(TRANSITION_INITIAL)
                    .map(|tos| tos.join(", "))
                    .unwrap_or_default();
                Err(BeadsError::validation(
                    "status",
                    format!(
                        "initial status '{to}' is not permitted by the project workflow policy \
                         (.beads/policy.yaml workflow.transitions, key 'initial'). \
                         Allowed initial statuses: {allowed}."
                    ),
                ))
            }
            Some(from) => {
                if self.allows_transition(from, to) {
                    return Ok(());
                }
                let allowed = self.allowed_targets_from(from);
                let allowed_list = if allowed.is_empty() {
                    "(none)".to_string()
                } else {
                    allowed.join(", ")
                };
                Err(BeadsError::validation(
                    "status",
                    format!(
                        "transition '{from}' -> '{to}' is not permitted by the project workflow \
                         policy (.beads/policy.yaml workflow.transitions). \
                         Valid next statuses from '{from}': {allowed_list}."
                    ),
                ))
            }
        }
    }
}

fn capacity_validation_error(reason: impl Into<String>) -> BeadsError {
    BeadsError::validation("workflow.capacity", reason)
}

fn validate_capacity_counting(counting: &CapacityCounting) -> Result<()> {
    if counting.weights.is_configured() && counting.hierarchy != CapacityCountingMode::Weighted {
        return Err(capacity_validation_error(format!(
            "counting.weights requires counting.hierarchy: weighted (found '{}')",
            counting.hierarchy.as_str()
        )));
    }
    let mut type_names = std::collections::HashSet::new();
    for type_name in counting.weights.types.keys() {
        if type_name.trim().is_empty() {
            return Err(capacity_validation_error(
                "counting.weights.types keys cannot be empty",
            ));
        }
        if !type_names.insert(type_name.trim().to_lowercase()) {
            return Err(capacity_validation_error(format!(
                "counting.weights.types entry '{type_name}' is duplicated case-insensitively"
            )));
        }
    }
    for issue_id in counting.weights.issues.keys() {
        if issue_id.trim().is_empty() {
            return Err(capacity_validation_error(
                "counting.weights.issues keys cannot be empty",
            ));
        }
    }
    Ok(())
}

fn validate_capacity_exemptions(exemptions: &CapacityExemptionPolicy) -> Result<()> {
    let mut providers = std::collections::HashSet::new();
    for provider in &exemptions.providers {
        if provider.trim().is_empty() {
            return Err(capacity_validation_error(
                "exemptions.providers entries cannot be empty",
            ));
        }
        if !providers.insert(provider.trim().to_lowercase()) {
            return Err(capacity_validation_error(format!(
                "exemptions.providers entry '{provider}' is duplicated case-insensitively"
            )));
        }
    }
    if exemptions.max_ttl_seconds == Some(0) {
        return Err(capacity_validation_error(
            "exemptions.max_ttl_seconds must be greater than zero when set",
        ));
    }
    Ok(())
}

fn declared_capacity_statuses(statuses: &[String]) -> Result<std::collections::HashSet<String>> {
    let declared: std::collections::HashSet<String> = statuses
        .iter()
        .map(|status| status.trim().to_lowercase())
        .filter(|status| !status.is_empty())
        .collect();
    if declared.len() != statuses.len() {
        return Err(capacity_validation_error(
            "workflow.statuses must be non-empty and unique (case-insensitive) when capacity is configured",
        ));
    }
    Ok(declared)
}

fn validate_status_capacities(
    capacities: &std::collections::BTreeMap<String, CapacityLimit>,
    declared_statuses: &std::collections::HashSet<String>,
) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for (status, limit) in capacities {
        validate_capacity_name(status, "status")?;
        validate_declared_capacity_status(status, declared_statuses)?;
        if !names.insert(status.trim().to_lowercase()) {
            return Err(capacity_validation_error(format!(
                "capacity status '{status}' is duplicated case-insensitively"
            )));
        }
        validate_capacity_limit(limit, &format!("statuses.{status}"), true)?;
    }
    Ok(())
}

fn validate_capacity_groups(
    groups: &std::collections::BTreeMap<String, CapacityGroup>,
    declared_statuses: &std::collections::HashSet<String>,
) -> Result<std::collections::HashSet<String>> {
    let mut names = std::collections::HashSet::new();
    for (name, group) in groups {
        validate_capacity_name(name, "group")?;
        if !names.insert(name.trim().to_lowercase()) {
            return Err(capacity_validation_error(format!(
                "capacity group name '{name}' is duplicated case-insensitively"
            )));
        }
        if group.statuses.is_empty() {
            return Err(capacity_validation_error(format!(
                "capacity group '{name}' must include at least one status"
            )));
        }
        let mut members = std::collections::HashSet::new();
        for status in &group.statuses {
            validate_declared_capacity_status(status, declared_statuses)?;
            if !members.insert(status.trim().to_lowercase()) {
                return Err(capacity_validation_error(format!(
                    "capacity group '{name}' repeats status '{status}'"
                )));
            }
        }
        validate_capacity_limit(&group.limit(), &format!("groups.{name}"), false)?;
    }
    Ok(names)
}

fn validate_capacity_scopes(
    scopes: &std::collections::BTreeMap<String, CapacityScopePolicy>,
    declared_statuses: &std::collections::HashSet<String>,
) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for (name, scope) in scopes {
        let Some(kind) = CapacityScopeKind::parse(name) else {
            return Err(capacity_validation_error(format!(
                "capacity scope '{name}' is not recognized; valid scopes are \
                 repository, actor, assignee, harness, session, and subtree"
            )));
        };
        if !names.insert(kind.as_str()) {
            return Err(capacity_validation_error(format!(
                "capacity scope '{name}' is duplicated case-insensitively"
            )));
        }
        if !scope.is_active() {
            return Err(capacity_validation_error(format!(
                "capacity scope '{name}' must declare at least one status or group limit"
            )));
        }
        validate_status_capacities(&scope.statuses, declared_statuses)
            .map_err(|err| capacity_scope_validation_context(name, err))?;
        validate_capacity_groups(&scope.groups, declared_statuses)
            .map_err(|err| capacity_scope_validation_context(name, err))?;
    }
    Ok(())
}

fn capacity_scope_validation_context(scope: &str, err: BeadsError) -> BeadsError {
    match err {
        BeadsError::Validation { reason, .. } => {
            capacity_validation_error(format!("scope '{scope}': {reason}"))
        }
        other => other,
    }
}

fn validate_capacity_admission_rules(
    rules: &[CapacityAdmissionRule],
    declared_statuses: &std::collections::HashSet<String>,
    group_names: &std::collections::HashSet<String>,
) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for rule in rules {
        validate_capacity_name(&rule.name, "admission rule")?;
        if !names.insert(rule.name.trim().to_lowercase()) {
            return Err(capacity_validation_error(format!(
                "capacity admission rule name '{}' is duplicated case-insensitively",
                rule.name
            )));
        }
        validate_capacity_admission_rule(rule, declared_statuses, group_names)?;
    }
    Ok(())
}

fn validate_capacity_admission_rule(
    rule: &CapacityAdmissionRule,
    declared_statuses: &std::collections::HashSet<String>,
    group_names: &std::collections::HashSet<String>,
) -> Result<()> {
    if rule.transitions.from.is_empty() || rule.transitions.to.is_empty() {
        return Err(capacity_validation_error(format!(
            "capacity admission rule '{}' requires non-empty transitions.from and transitions.to lists",
            rule.name
        )));
    }
    for status in rule
        .transitions
        .from
        .iter()
        .chain(&rule.transitions.to)
        .chain(rule.require_below.statuses.keys())
    {
        validate_declared_capacity_status(status, declared_statuses)?;
    }
    if rule.require_below.statuses.is_empty() && rule.require_below.groups.is_empty() {
        return Err(capacity_validation_error(format!(
            "capacity admission rule '{}' must inspect at least one status or group",
            rule.name
        )));
    }
    validate_capacity_admission_statuses(rule)?;
    validate_capacity_admission_groups(rule, group_names)
}

fn validate_capacity_admission_statuses(rule: &CapacityAdmissionRule) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for (status, threshold) in &rule.require_below.statuses {
        if !names.insert(status.trim().to_lowercase()) {
            return Err(capacity_validation_error(format!(
                "capacity admission rule '{}' repeats required status '{status}' case-insensitively",
                rule.name
            )));
        }
        if *threshold == 0 {
            return Err(capacity_validation_error(format!(
                "capacity admission rule '{}' has zero threshold for status '{status}'",
                rule.name
            )));
        }
    }
    Ok(())
}

fn validate_capacity_admission_groups(
    rule: &CapacityAdmissionRule,
    group_names: &std::collections::HashSet<String>,
) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for (group, threshold) in &rule.require_below.groups {
        let canonical = group.trim().to_lowercase();
        if !names.insert(canonical.clone()) {
            return Err(capacity_validation_error(format!(
                "capacity admission rule '{}' repeats required group '{group}' case-insensitively",
                rule.name
            )));
        }
        if !group_names.contains(&canonical) {
            return Err(capacity_validation_error(format!(
                "capacity admission rule '{}' references undeclared group '{group}'",
                rule.name
            )));
        }
        if *threshold == 0 {
            return Err(capacity_validation_error(format!(
                "capacity admission rule '{}' has zero threshold for group '{group}'",
                rule.name
            )));
        }
    }
    Ok(())
}

fn validate_capacity_name(name: &str, kind: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(capacity_validation_error(format!(
            "capacity {kind} name cannot be empty"
        )));
    }
    Ok(())
}

fn validate_declared_capacity_status(
    status: &str,
    declared_statuses: &std::collections::HashSet<String>,
) -> Result<()> {
    let canonical = status.trim().to_lowercase();
    if canonical.is_empty() || !declared_statuses.contains(&canonical) {
        return Err(capacity_validation_error(format!(
            "capacity references undeclared workflow status '{status}'"
        )));
    }
    Ok(())
}

fn validate_capacity_limit(
    limit: &CapacityLimit,
    path: &str,
    require_threshold: bool,
) -> Result<()> {
    if require_threshold && limit.soft.is_none() && limit.hard.is_none() {
        return Err(capacity_validation_error(format!(
            "capacity {path} must configure soft and/or hard"
        )));
    }
    if limit.soft == Some(0) || limit.hard == Some(0) {
        return Err(capacity_validation_error(format!(
            "capacity {path} thresholds must be greater than zero"
        )));
    }
    if let (Some(soft), Some(hard)) = (limit.soft, limit.hard)
        && soft > hard
    {
        return Err(capacity_validation_error(format!(
            "capacity {path} soft limit ({soft}) cannot exceed hard limit ({hard})"
        )));
    }
    Ok(())
}

/// Typed-references gate (capability #3 of issue #274).
///
/// When `enabled`, the close reason must contain at least one
/// `kind:value` token matching one of `required_kinds`. Built-in kinds are
/// `commit`, `pr`, `reviewer`, `investigation`, `agent-mail`, and `dashboard`;
/// projects can add custom kinds by listing them in `required_kinds`.
///
/// The matcher is lenient: the kind/value pair can appear anywhere
/// in the reason text (start, end, embedded in prose) as long as
/// it's a contiguous `kind:value` token with no whitespace in the
/// value. We deliberately don't require URL-like values — the point
/// is queryability, not URL validation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequireTypedReferences {
    pub enabled: bool,
    /// Reference kinds the close reason must contain (logical OR — any one
    /// satisfies the gate). When empty, the gate accepts any built-in
    /// reference kind; custom kinds must be listed explicitly.
    #[serde(default)]
    pub required_kinds: Vec<String>,
}

const BUILTIN_TYPED_REFERENCE_KINDS: &[&str] = &[
    "commit",
    "pr",
    "reviewer",
    "investigation",
    "agent-mail",
    "dashboard",
];

/// Bare on/off toggle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToggleGate {
    pub enabled: bool,
}

/// Required close-reason gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequireCloseReason {
    pub enabled: bool,
    /// Minimum trimmed character count. `0` disables length checking.
    pub min_length: usize,
    /// Optional regex (anchored as the user wrote it). `None` disables.
    pub regex: Option<String>,
}

impl Default for RequireCloseReason {
    fn default() -> Self {
        Self {
            enabled: false,
            min_length: 20,
            regex: None,
        }
    }
}

/// Agent-attribution capture configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Attribution {
    pub tier: AttributionTier,
    /// Optional list of fields the project wants captured. When omitted, all
    /// known fields are captured. Phase 1 only honours
    /// `agent_name` / `harness` / `model`; unknown values are tolerated to
    /// keep forward compatibility for Tier 2/3 fields landing later.
    #[serde(default)]
    pub fields: Vec<String>,
}

/// Attribution tier. Phase 1 ships `Off` and `Capture` only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionTier {
    #[default]
    Off,
    Capture,
}

/// Agent attribution values resolved from CLI flags + env vars.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttributionValues {
    pub agent_name: Option<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
}

impl AttributionValues {
    /// Resolve attribution values using the precedence: CLI flag > env var > absent.
    #[must_use]
    pub fn resolve(
        cli_agent_name: Option<&str>,
        cli_harness: Option<&str>,
        cli_model: Option<&str>,
        env_lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        Self {
            agent_name: cli_agent_name
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .or_else(|| env_lookup(ENV_AGENT_NAME).filter(|s| !s.trim().is_empty())),
            harness: cli_harness
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .or_else(|| env_lookup(ENV_HARNESS).filter(|s| !s.trim().is_empty())),
            model: cli_model
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .or_else(|| env_lookup(ENV_MODEL).filter(|s| !s.trim().is_empty())),
        }
    }

    /// Resolve from process env. Convenience wrapper.
    #[must_use]
    pub fn resolve_from_env(
        cli_agent_name: Option<&str>,
        cli_harness: Option<&str>,
        cli_model: Option<&str>,
    ) -> Self {
        Self::resolve(cli_agent_name, cli_harness, cli_model, &|key| {
            std::env::var(key).ok()
        })
    }

    /// True when all attribution fields are absent.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.agent_name.is_none() && self.harness.is_none() && self.model.is_none()
    }
}

/// A single policy violation discovered while evaluating gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyViolation {
    /// Stable machine identifier for the gate (e.g. `close_reason_min_length`).
    pub gate: String,
    /// Human-readable explanation. Always present.
    pub message: String,
    /// Optional structured detail (counts, items, expected vs actual).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// Issue-level evidence that gates evaluate against. Carved into a struct so
/// the evaluator stays pure and easy to test.
#[derive(Debug, Clone, Default)]
pub struct CloseEvidence<'a> {
    pub issue_id: &'a str,
    /// The reason text supplied via `--reason`. `None` when the user did not
    /// pass `--reason`.
    pub close_reason: Option<&'a str>,
    /// Issue body sections we scan for unchecked acceptance criteria.
    pub description: Option<&'a str>,
    pub design: Option<&'a str>,
    pub acceptance_criteria: Option<&'a str>,
    pub notes: Option<&'a str>,
    /// The actor performing the close.
    pub close_actor: &'a str,
    /// The actor who last set status to `in_progress` (if any).
    pub in_progress_actor: Option<&'a str>,
}

/// Evaluate every enabled gate against the supplied evidence.
///
/// Tier 1 attribution is intentionally NOT evaluated here: capture happens
/// regardless of any rejection logic, and Phase 1 explicitly never rejects
/// on missing attribution.
#[must_use]
pub fn evaluate(policy: &ClosePolicy, evidence: &CloseEvidence<'_>) -> Vec<PolicyViolation> {
    let mut violations = Vec::new();

    if policy.require_close_reason.enabled {
        evaluate_close_reason(&policy.require_close_reason, evidence, &mut violations);
    }

    if policy.require_acceptance_criteria_satisfied.enabled {
        evaluate_acceptance_criteria(evidence, &mut violations);
    }

    if policy.forbid_self_close_after_in_progress.enabled {
        evaluate_self_close(evidence, &mut violations);
    }

    if policy.require_typed_references.enabled {
        evaluate_typed_references(&policy.require_typed_references, evidence, &mut violations);
    }

    violations
}

/// Match `kind:value` tokens inside a close-reason string. We require
/// the value to be non-empty and not start with whitespace — this
/// rules out accidental matches like `note: foo` in prose. The kind
/// itself must start with a lowercase ASCII letter and may then contain
/// lowercase ASCII letters / hyphens / digits, which
/// matches the canonical `commit:` / `pr:` / `agent-mail:` shapes
/// the issue calls out.
fn extract_typed_references(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Find start of a candidate kind: alpha character preceded
        // either by start-of-string, whitespace, or a punctuation
        // boundary character (so we don't pick up `xyz` in `foo-xyz:`).
        let preceding_ok =
            i == 0 || matches!(bytes[i - 1], b' ' | b'\n' | b'\t' | b'(' | b'[' | b',');
        if !preceding_ok {
            i += 1;
            continue;
        }
        if !bytes[i].is_ascii_lowercase() {
            i += 1;
            continue;
        }
        let kind_start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_lowercase() || bytes[i] == b'-' || bytes[i].is_ascii_digit())
        {
            i += 1;
        }
        if i == kind_start || i >= bytes.len() || bytes[i] != b':' {
            i += 1;
            continue;
        }
        let kind = &text[kind_start..i];
        i += 1; // skip the ':'
        let value_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b',' {
            i += 1;
        }
        let value = &text[value_start..i];
        if !value.is_empty() && kind.len() >= 2 {
            out.push((kind.to_string(), value.to_string()));
        }
    }
    out
}

fn required_typed_reference_description(rule: &RequireTypedReferences) -> String {
    if rule.required_kinds.is_empty() {
        BUILTIN_TYPED_REFERENCE_KINDS.join(", ")
    } else {
        rule.required_kinds.join(", ")
    }
}

fn typed_reference_kind_satisfies_rule(kind: &str, rule: &RequireTypedReferences) -> bool {
    if rule.required_kinds.is_empty() {
        return BUILTIN_TYPED_REFERENCE_KINDS.contains(&kind);
    }
    rule.required_kinds.iter().any(|required| required == kind)
}

fn evaluate_typed_references(
    rule: &RequireTypedReferences,
    evidence: &CloseEvidence<'_>,
    out: &mut Vec<PolicyViolation>,
) {
    let reason_text = evidence.close_reason.unwrap_or("");
    let refs = extract_typed_references(reason_text);
    let required_description = required_typed_reference_description(rule);

    if refs.is_empty() {
        out.push(PolicyViolation {
            gate: "typed_references_required".to_string(),
            message: format!(
                "close_reason has no typed references; policy requires at least one of: {}",
                required_description
            ),
            detail: Some(serde_json::json!({
                "required_kinds": &rule.required_kinds,
                "issue_id": evidence.issue_id,
            })),
        });
        return;
    }

    let found_kinds: Vec<&str> = refs
        .iter()
        .map(|(kind, _)| kind.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let satisfied = found_kinds
        .iter()
        .any(|kind| typed_reference_kind_satisfies_rule(kind, rule));
    if !satisfied {
        out.push(PolicyViolation {
            gate: "typed_references_required_kind_missing".to_string(),
            message: format!(
                "close_reason has typed refs ({}) but none satisfy the required kinds: {}",
                found_kinds.join(", "),
                required_description
            ),
            detail: Some(serde_json::json!({
                "required_kinds": &rule.required_kinds,
                "found_kinds": found_kinds,
                "issue_id": evidence.issue_id,
            })),
        });
    }
}

fn evaluate_close_reason(
    rule: &RequireCloseReason,
    evidence: &CloseEvidence<'_>,
    out: &mut Vec<PolicyViolation>,
) {
    let reason_text = evidence.close_reason.map(str::trim).unwrap_or("");
    let actual_len = reason_text.chars().count();

    if rule.min_length > 0 && actual_len < rule.min_length {
        out.push(PolicyViolation {
            gate: "close_reason_min_length".to_string(),
            message: format!(
                "close_reason fails policy: minimum length is {} chars (got {})",
                rule.min_length, actual_len
            ),
            detail: Some(serde_json::json!({
                "min_length": rule.min_length,
                "actual_length": actual_len,
                "issue_id": evidence.issue_id,
            })),
        });
    }

    if let Some(pattern) = rule.regex.as_deref() {
        match Regex::new(pattern) {
            Ok(re) => {
                if !re.is_match(reason_text) {
                    out.push(PolicyViolation {
                        gate: "close_reason_regex".to_string(),
                        message: format!(
                            "close_reason fails policy: must match pattern '{pattern}'"
                        ),
                        detail: Some(serde_json::json!({
                            "pattern": pattern,
                            "issue_id": evidence.issue_id,
                        })),
                    });
                }
            }
            Err(err) => {
                out.push(PolicyViolation {
                    gate: "close_reason_regex_invalid".to_string(),
                    message: format!(
                        "policy.yaml close_reason regex is invalid ('{pattern}'): {err}"
                    ),
                    detail: Some(serde_json::json!({
                        "pattern": pattern,
                        "parse_error": err.to_string(),
                    })),
                });
            }
        }
    }
}

fn evaluate_acceptance_criteria(evidence: &CloseEvidence<'_>, out: &mut Vec<PolicyViolation>) {
    // Pull from every body field so users with criteria in description (legacy)
    // are still gated. acceptance_criteria column is the canonical home.
    let candidates = [
        evidence.acceptance_criteria,
        evidence.description,
        evidence.design,
        evidence.notes,
    ];

    let mut unchecked: Vec<String> = Vec::new();
    for body in candidates.into_iter().flatten() {
        unchecked.extend(find_unchecked_acceptance_criteria(body));
    }
    // De-dupe across fields: copy/paste between description and
    // acceptance_criteria shouldn't double-count.
    unchecked.sort();
    unchecked.dedup();

    if !unchecked.is_empty() {
        let preview: Vec<String> = unchecked.iter().take(3).cloned().collect();
        let suffix = if unchecked.len() > preview.len() {
            format!(" (+{} more)", unchecked.len() - preview.len())
        } else {
            String::new()
        };
        out.push(PolicyViolation {
            gate: "acceptance_criteria_unchecked".to_string(),
            message: format!(
                "acceptance criteria policy: {} unchecked criteria remain: {}{}",
                unchecked.len(),
                preview.join("; "),
                suffix
            ),
            detail: Some(serde_json::json!({
                "unchecked_count": unchecked.len(),
                "unchecked_items": unchecked,
                "issue_id": evidence.issue_id,
            })),
        });
    }
}

fn evaluate_self_close(evidence: &CloseEvidence<'_>, out: &mut Vec<PolicyViolation>) {
    let Some(in_progress_actor) = evidence.in_progress_actor else {
        // No in_progress transition recorded — gate is satisfied by definition
        // (open → closed is unconstrained for this rule).
        return;
    };
    if in_progress_actor.is_empty() || evidence.close_actor.is_empty() {
        return;
    }
    if in_progress_actor == evidence.close_actor {
        out.push(PolicyViolation {
            gate: "forbid_self_close_after_in_progress".to_string(),
            message: format!(
                "actor policy: close.actor '{}' matches the actor who set in_progress; cross-validation required",
                evidence.close_actor
            ),
            detail: Some(serde_json::json!({
                "close_actor": evidence.close_actor,
                "in_progress_actor": in_progress_actor,
                "issue_id": evidence.issue_id,
            })),
        });
    }
}

/// Gate identifier emitted by [`deferred_dependents_violation`] and recorded
/// in close metadata. Mirrors the `policy.yaml` key for grep-ability.
pub const GATE_FORBID_CLOSE_WITH_DEFERRED_DEPENDENTS: &str =
    "forbid_close_with_deferred_dependents";

/// Build the policy violation for the `forbid_close_with_deferred_dependents`
/// gate (beads_rust#303), given the issue being closed and the IDs of its
/// dependents (issues with a `blocks` edge *from* `issue_id`) that are
/// currently in `deferred` status.
///
/// Returns `None` when there are no deferred dependents (gate satisfied).
/// The storage-backed dependent lookup lives in the close command — this
/// function is the pure, unit-testable message/shape builder so the gate's
/// error contract is covered without a database.
///
/// The error names every offending dependent ID and instructs the closer to
/// either reopen each (`br update <dep> --status=open`) or close-as-superseded
/// with a `duplicate_of` edge before closing the prereq.
#[must_use]
pub fn deferred_dependents_violation(
    issue_id: &str,
    deferred_dependent_ids: &[String],
) -> Option<PolicyViolation> {
    if deferred_dependent_ids.is_empty() {
        return None;
    }

    // Stable, de-duplicated ordering so the message and detail are
    // deterministic regardless of query/iteration order.
    let mut ids: Vec<String> = deferred_dependent_ids.to_vec();
    ids.sort();
    ids.dedup();

    let id_list = ids.join(", ");
    let message = format!(
        "deferred-dependents policy: cannot close {issue_id}: it has {count} deferred dependent(s): {id_list}. \
         Reopen each (`br update <dep> --status=open`) or close-as-superseded with a duplicate_of edge \
         before closing {issue_id}.",
        count = ids.len(),
    );

    Some(PolicyViolation {
        gate: GATE_FORBID_CLOSE_WITH_DEFERRED_DEPENDENTS.to_string(),
        message,
        detail: Some(serde_json::json!({
            "issue_id": issue_id,
            "deferred_dependents": ids,
            "deferred_dependent_count": ids.len(),
        })),
    })
}

/// Locate unchecked `- [ ]` items inside the canonical acceptance criteria
/// section. Recognises both an `## Acceptance Criteria` header (any heading
/// level) and a body that is itself the acceptance-criteria block (the case
/// when `acceptance_criteria` column carries the list directly).
#[must_use]
pub fn find_unchecked_acceptance_criteria(body: &str) -> Vec<String> {
    let body = body.trim();
    if body.is_empty() {
        return Vec::new();
    }

    let mut in_section = false;
    let mut found_header = false;
    let mut out: Vec<String> = Vec::new();

    // If the body has no markdown headers, treat the whole body as the
    // acceptance criteria section. acceptance_criteria column flow.
    let has_any_header = has_markdown_heading_outside_fences(body);
    if !has_any_header {
        in_section = true;
    }

    let mut fence_marker = None;
    for line in body.lines() {
        if update_code_fence(line, &mut fence_marker) || fence_marker.is_some() {
            continue;
        }
        let trimmed = line.trim_start();
        if let Some(header_text) = markdown_heading_text(trimmed) {
            // Determine if this header opens / closes the acceptance criteria block.
            let lower = header_text.to_ascii_lowercase();
            // Match a header that contains "acceptance criteria" (allows variants
            // like "Acceptance Criteria (Phase 1)").
            if lower.contains("acceptance criteria") {
                in_section = true;
                found_header = true;
                continue;
            }
            // Any other header closes the section once we have entered it.
            if found_header {
                in_section = false;
            }
            continue;
        }

        if !in_section {
            continue;
        }

        if let Some(item) = parse_unchecked_box(trimmed) {
            out.push(item);
        }
    }

    out
}

/// Locate every unchecked markdown checklist item outside fenced code.
///
/// Unlike [`find_unchecked_acceptance_criteria`], this deliberately ignores
/// section headings. It is used only for the dedicated `acceptance_criteria`
/// field, where the entire value is criteria by definition.
fn find_unchecked_checklist_items(body: &str) -> Vec<String> {
    let mut fence_marker = None;
    let mut out = Vec::new();
    for line in body.lines() {
        if update_code_fence(line, &mut fence_marker) || fence_marker.is_some() {
            continue;
        }
        if let Some(item) = parse_unchecked_box(line.trim_start()) {
            out.push(item);
        }
    }
    out
}

fn has_markdown_heading_outside_fences(body: &str) -> bool {
    let mut fence_marker = None;
    for line in body.lines() {
        if update_code_fence(line, &mut fence_marker) || fence_marker.is_some() {
            continue;
        }
        if markdown_heading_text(line).is_some() {
            return true;
        }
    }
    false
}

fn update_code_fence(line: &str, fence_marker: &mut Option<char>) -> bool {
    let trimmed = line.trim_start();
    let Some(marker @ ('`' | '~')) = trimmed.chars().next() else {
        return false;
    };
    let marker_len = trimmed.chars().take_while(|ch| *ch == marker).count();
    if marker_len < 3 {
        return false;
    }

    if fence_marker.is_some_and(|open_marker| open_marker == marker) {
        *fence_marker = None;
    } else if fence_marker.is_none() {
        *fence_marker = Some(marker);
    }
    true
}

fn markdown_heading_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let level = trimmed
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == b'#')
        .count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = trimmed.get(level..)?;
    if rest.chars().next().is_some_and(|ch| !ch.is_whitespace()) {
        return None;
    }
    Some(rest.trim())
}

/// Parse a single line for an unchecked checkbox. Accepts `- [ ]`, `* [ ]`,
/// `+ [ ]`, optional leading whitespace, and any space (or lack thereof)
/// between the marker and the bracket.
fn parse_unchecked_box(line: &str) -> Option<String> {
    let mut chars = line.chars().peekable();
    let bullet = chars.next()?;
    if !matches!(bullet, '-' | '*' | '+') {
        return None;
    }
    // Skip whitespace between bullet and `[`.
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
    if chars.next()? != '[' {
        return None;
    }
    // Inner char must be whitespace/empty for "unchecked".
    let inner = chars.next()?;
    let inner_is_unchecked = inner.is_whitespace() || inner == ' ';
    if !inner_is_unchecked {
        return None;
    }
    if chars.next()? != ']' {
        return None;
    }
    let rest: String = chars.collect();
    let rest = rest.trim().to_string();
    Some(rest)
}

/// Load the policy document from `.beads/policy.yaml`. Returns the default
/// (all gates off) when the file does not exist. Returns an error only if the
/// file exists but cannot be read or parsed — never silently downgrades a
/// broken config to "permissive."
///
/// # Unknown fields (beads_rust#302)
///
/// Close-policy structs deliberately accept unknown fields rather than
/// hard-failing the parse. A typo or project-local experimental gate
/// previously took down `br close` for every operator on the project,
/// with `--bypass-policy` powerless to recover because the parse fires
/// before bypass logic runs. We now warn instead of erroring; the trade
/// is loss of typo-at-parse-time detection, but the cost (full project
/// close-pathway outage from one typo) was much worse.
///
/// Unknown fields are surfaced exactly once per load via
/// [`detect_unknown_policy_fields`] and emitted as a `tracing::warn!`
/// event. The warning lists every unknown path with a dotted scope
/// (e.g. `close_policy.require_new_experimental_field`) so operators
/// can find typos without re-reading the file.
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_for_beads_dir(beads_dir: &Path) -> Result<PolicyDocument> {
    let path = beads_dir.join(POLICY_FILE_NAME);
    if !path.exists() {
        return Ok(PolicyDocument::default());
    }
    let raw = fs::read_to_string(&path).map_err(BeadsError::from)?;
    let document: PolicyDocument = serde_yml::from_str(&raw).map_err(|err| {
        BeadsError::Config(format!("failed to parse {}: {}", path.display(), err))
    })?;
    document.workflow.validate_capacity()?;
    document.workflow.validate_required_fields()?;

    // Re-parse the raw YAML into a free-form value tree so we can diff it
    // against the typed schema and surface unknown fields without failing
    // the load. Failure to re-parse as a `Value` is impossible here (the
    // typed parse above already succeeded), but if it ever did, we'd
    // rather skip the warning than spurious-error the load — that's the
    // whole point of #302.
    if let Ok(raw_value) = serde_yml::from_str::<serde_yml::Value>(&raw) {
        let unknown = detect_unknown_policy_fields(&raw_value);
        if !unknown.is_empty() {
            tracing::warn!(
                policy_path = %path.display(),
                unknown_fields = ?unknown,
                "policy.yaml contains {} unknown field(s) under close_policy structs; \
                 these were ignored (beads_rust#302). Check for typos: {}",
                unknown.len(),
                unknown.join(", "),
            );
        }
    }

    Ok(document)
}

/// Walk a parsed `policy.yaml` value tree and collect dotted paths to any
/// keys not recognized by the typed close-policy schema.
///
/// We use a hard-coded recursive walk (rather than `serde(flatten)` with
/// `extras` fields on every struct) so the typed public API stays simple
/// and the extras maps don't leak into round-trip serialization. Adding
/// a new canonical field becomes a one-line update in [`PolicyNode`].
///
/// Returns a sorted, de-duplicated list of dotted paths
/// (e.g. `["close_policy.require_new_experimental_field"]`). Empty when
/// the document only uses canonical fields.
#[must_use]
pub fn detect_unknown_policy_fields(root: &serde_yml::Value) -> Vec<String> {
    let mut unknown = Vec::new();
    walk_policy_node(root, PolicyNode::Document, "", &mut unknown);
    unknown.sort();
    unknown.dedup();
    unknown
}

/// Schema-tree node used by [`detect_unknown_policy_fields`] to recognise
/// which keys are canonical at each depth of `policy.yaml`. Leaves (`Scalar`)
/// terminate the walk; mappings descend per the `key -> child-node` table.
#[derive(Clone, Copy, Debug)]
enum PolicyNode {
    /// Top-level `policy.yaml` mapping.
    Document,
    /// `close_policy:` block.
    ClosePolicy,
    /// `close_policy.require_close_reason:` block.
    RequireCloseReason,
    /// Any plain `{enabled: bool}` toggle gate.
    ToggleGate,
    /// `close_policy.attribution:` block.
    Attribution,
    /// `close_policy.require_typed_references:` block.
    RequireTypedReferences,
    /// `workflow:` block (issue #311).
    Workflow,
    /// `workflow.status_groups:` block (issue #354).
    StatusGroups,
    /// Terminal scalar / list — descent stops here.
    Scalar,
}

impl PolicyNode {
    /// Canonical keys at this depth, plus the child node each key descends
    /// into. Keys absent from this table are reported as unknown.
    const fn child_table(self) -> &'static [(&'static str, Self)] {
        match self {
            Self::Document => &[
                ("close_policy", Self::ClosePolicy),
                ("workflow", Self::Workflow),
                ("allow_bypass", Self::Scalar),
            ],
            Self::ClosePolicy => &[
                ("require_close_reason", Self::RequireCloseReason),
                ("require_acceptance_criteria_satisfied", Self::ToggleGate),
                ("forbid_self_close_after_in_progress", Self::ToggleGate),
                ("forbid_close_with_deferred_dependents", Self::ToggleGate),
                ("attribution", Self::Attribution),
                ("require_typed_references", Self::RequireTypedReferences),
            ],
            Self::RequireCloseReason => &[
                ("enabled", Self::Scalar),
                ("min_length", Self::Scalar),
                ("regex", Self::Scalar),
            ],
            Self::ToggleGate => &[("enabled", Self::Scalar)],
            Self::Attribution => &[("tier", Self::Scalar), ("fields", Self::Scalar)],
            Self::RequireTypedReferences => {
                &[("enabled", Self::Scalar), ("required_kinds", Self::Scalar)]
            }
            Self::Workflow => &[
                ("strict", Self::Scalar),
                ("statuses", Self::Scalar),
                ("transitions", Self::Scalar),
                // The gate rules are a free-form map of `"from -> to"` keys to
                // gate specs; we don't descend into them for unknown-field
                // detection (their shape is validated at parse time by the
                // typed `GateRule`/`GateSpec` deserialisers).
                ("gates", Self::Scalar),
                ("required_fields", Self::Scalar),
                ("status_groups", Self::StatusGroups),
                // Capacity owns a strict typed schema with
                // `deny_unknown_fields`; its nested keys are validated by
                // serde plus `Workflow::validate_capacity` rather than the
                // legacy permissive unknown-field walker.
                ("capacity", Self::Scalar),
            ],
            Self::StatusGroups => &[("ready", Self::Scalar)],
            Self::Scalar => &[],
        }
    }
}

fn walk_policy_node(
    value: &serde_yml::Value,
    node: PolicyNode,
    scope: &str,
    out: &mut Vec<String>,
) {
    if matches!(node, PolicyNode::Scalar) {
        return;
    }
    let Some(map) = value.as_mapping() else {
        return;
    };
    let table = node.child_table();
    for (key, sub) in map {
        let Some(key_str) = key.as_str() else {
            continue;
        };
        let path = if scope.is_empty() {
            key_str.to_string()
        } else {
            format!("{scope}.{key_str}")
        };
        match table.iter().find(|(k, _)| *k == key_str) {
            Some((_, child)) => walk_policy_node(sub, *child, &path, out),
            None => out.push(path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence_with_reason<'a>(reason: &'a str, issue_id: &'a str) -> CloseEvidence<'a> {
        CloseEvidence {
            issue_id,
            close_reason: Some(reason),
            close_actor: "alice",
            ..Default::default()
        }
    }

    #[test]
    fn ready_status_group_defaults_to_open() {
        // #354: unconfigured workflow → ready group is [open].
        let workflow = Workflow::default();
        assert_eq!(workflow.ready_status_group(), vec!["open".to_string()]);
    }

    #[test]
    fn ready_status_group_uses_configured_values_normalized() {
        // #354: configured group is normalized (lowercased, trimmed, de-duped,
        // source order preserved).
        let mut workflow = Workflow::default();
        workflow.status_groups.ready = vec![
            "Open".to_string(),
            "  rework ".to_string(),
            "open".to_string(),
        ];
        assert_eq!(
            workflow.ready_status_group(),
            vec!["open".to_string(), "rework".to_string()]
        );
    }

    #[test]
    fn transition_required_fields_compose_exact_and_target_rules() {
        // Required fields are opt-in by the presence of the map itself. They
        // intentionally do not depend on strict status-vocabulary enforcement.
        let mut workflow = Workflow::default();
        workflow.required_fields.insert(
            "in_review".to_string(),
            vec![TransitionRequiredField::TransitionComment],
        );
        workflow.required_fields.insert(
            "in_progress -> in_review".to_string(),
            vec![
                TransitionRequiredField::AcceptanceCriteria,
                TransitionRequiredField::TransitionComment,
            ],
        );

        assert_eq!(
            workflow.required_fields_for(Some("IN_PROGRESS"), "In_Review"),
            vec![
                TransitionRequiredField::AcceptanceCriteria,
                TransitionRequiredField::TransitionComment,
            ]
        );
        assert_eq!(
            workflow.required_fields_for(Some("open"), "in_review"),
            vec![TransitionRequiredField::TransitionComment]
        );
        assert!(
            workflow
                .required_fields_for(Some("open"), "closed")
                .is_empty()
        );
    }

    #[test]
    fn transition_required_fields_report_missing_and_unchecked_values() {
        let mut workflow = Workflow::default();
        workflow.required_fields.insert(
            "in_progress -> in_review".to_string(),
            vec![
                TransitionRequiredField::AcceptanceCriteria,
                TransitionRequiredField::TransitionComment,
            ],
        );

        let missing = evaluate_transition_required_fields(
            &workflow,
            "bd-1",
            Some("in_progress"),
            "in_review",
            None,
            Some("  "),
        );
        assert_eq!(missing.len(), 2);
        assert_eq!(missing[0].gate, "transition_acceptance_criteria_missing");
        assert_eq!(missing[1].gate, "transition_comment_missing");

        let unchecked = evaluate_transition_required_fields(
            &workflow,
            "bd-1",
            Some("in_progress"),
            "in_review",
            Some("## Phase one\n- [x] Built\n## Phase two\n- [ ] Verified\n"),
            Some("Ready for a fresh review"),
        );
        assert_eq!(unchecked.len(), 1);
        assert_eq!(
            unchecked[0].gate,
            "transition_acceptance_criteria_unchecked"
        );
        assert_eq!(
            unchecked[0].detail.as_ref().unwrap()["unchecked"][0],
            "Verified"
        );

        assert!(
            evaluate_transition_required_fields(
                &workflow,
                "bd-1",
                Some("in_progress"),
                "in_review",
                Some("- [x] Built\n- [X] Verified\n"),
                Some("Ready for a fresh review"),
            )
            .is_empty()
        );
    }

    #[test]
    fn transition_required_fields_reject_malformed_multi_arrow_rule() {
        let mut workflow = Workflow {
            strict: true,
            ..Default::default()
        };
        workflow.required_fields.insert(
            "open -> review -> closed".to_string(),
            vec![TransitionRequiredField::TransitionComment],
        );
        let error = workflow.validate_required_fields().unwrap_err();
        assert!(error.to_string().contains("exactly one"));
    }

    #[test]
    fn validate_ready_group_accepts_subset_when_strict() {
        // #354: strict mode, group is a subset of statuses → ok.
        let mut workflow = Workflow {
            strict: true,
            statuses: vec![
                "open".to_string(),
                "rework".to_string(),
                "closed".to_string(),
            ],
            ..Workflow::default()
        };
        workflow.status_groups.ready = vec!["open".to_string(), "rework".to_string()];
        assert!(workflow.validate_ready_status_group().is_ok());
    }

    #[test]
    fn validate_ready_group_rejects_out_of_vocab_when_strict() {
        // #354: strict mode, group has a status outside statuses → rejected with
        // a clear error naming the offending value.
        let mut workflow = Workflow {
            strict: true,
            statuses: vec!["open".to_string(), "closed".to_string()],
            ..Workflow::default()
        };
        workflow.status_groups.ready = vec!["open".to_string(), "rework".to_string()];
        let err = workflow
            .validate_ready_status_group()
            .expect_err("out-of-vocab group must be rejected under strict");
        let msg = err.to_string();
        assert!(
            msg.contains("rework"),
            "error should name the bad status: {msg}"
        );
    }

    #[test]
    fn validate_ready_group_accepts_anything_when_not_strict() {
        // #354: without strict (or without a statuses vocabulary), the group is
        // accepted as-is.
        let mut workflow = Workflow {
            strict: false,
            statuses: vec!["open".to_string()],
            ..Workflow::default()
        };
        workflow.status_groups.ready = vec!["open".to_string(), "rework".to_string()];
        assert!(workflow.validate_ready_status_group().is_ok());

        // strict but empty statuses → enforcement off → accepted.
        workflow.strict = true;
        workflow.statuses = vec![];
        assert!(workflow.validate_ready_status_group().is_ok());
    }

    #[test]
    fn status_groups_parse_from_yaml_and_not_flagged_unknown() {
        // #354: the new keys deserialize and are recognized by the
        // unknown-field detector.
        let raw = "workflow:\n  status_groups:\n    ready: [open, rework]\n";
        let doc: PolicyDocument = serde_yml::from_str(raw).unwrap();
        assert_eq!(
            doc.workflow.status_groups.ready,
            vec!["open".to_string(), "rework".to_string()]
        );
        let value: serde_yml::Value = serde_yml::from_str(raw).unwrap();
        let unknown = detect_unknown_policy_fields(&value);
        assert!(
            unknown.is_empty(),
            "status_groups.ready must not be flagged unknown: {unknown:?}"
        );
    }

    #[test]
    fn default_policy_is_inactive() {
        let policy = ClosePolicy::default();
        assert!(!policy.is_active());
        let evidence = evidence_with_reason("anything", "bd-1");
        assert!(evaluate(&policy, &evidence).is_empty());
    }

    #[test]
    fn close_reason_min_length_rejects_short_reason() {
        let mut policy = ClosePolicy::default();
        policy.require_close_reason.enabled = true;
        policy.require_close_reason.min_length = 20;

        let violations = evaluate(&policy, &evidence_with_reason("too short", "bd-1"));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].gate, "close_reason_min_length");
        assert!(violations[0].message.contains("minimum length is 20"));
        assert!(violations[0].message.contains("got 9"));
    }

    #[test]
    fn close_reason_min_length_counts_unicode_chars_not_bytes() {
        let mut policy = ClosePolicy::default();
        policy.require_close_reason.enabled = true;
        policy.require_close_reason.min_length = 4;

        // 4 emoji = 4 chars (16 bytes UTF-8). Must satisfy a 4-char minimum.
        let violations = evaluate(&policy, &evidence_with_reason("😀😀😀😀", "bd-1"));
        assert!(violations.is_empty(), "{:?}", violations);
    }

    #[test]
    fn close_reason_min_length_zero_disables_length_check() {
        let mut policy = ClosePolicy::default();
        policy.require_close_reason.enabled = true;
        policy.require_close_reason.min_length = 0;

        let violations = evaluate(&policy, &evidence_with_reason("", "bd-1"));
        assert!(violations.is_empty());
    }

    #[test]
    fn close_reason_min_length_treats_missing_reason_as_empty() {
        let mut policy = ClosePolicy::default();
        policy.require_close_reason.enabled = true;
        policy.require_close_reason.min_length = 5;

        let evidence = CloseEvidence {
            issue_id: "bd-1",
            close_reason: None,
            close_actor: "alice",
            ..Default::default()
        };
        let violations = evaluate(&policy, &evidence);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].gate, "close_reason_min_length");
    }

    #[test]
    fn close_reason_regex_rejects_non_match() {
        let mut policy = ClosePolicy::default();
        policy.require_close_reason.enabled = true;
        policy.require_close_reason.min_length = 0;
        policy.require_close_reason.regex = Some(r"^[A-Z][a-z]+: ".to_string());

        let violations = evaluate(&policy, &evidence_with_reason("done", "bd-1"));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].gate, "close_reason_regex");
    }

    #[test]
    fn close_reason_regex_accepts_match() {
        let mut policy = ClosePolicy::default();
        policy.require_close_reason.enabled = true;
        policy.require_close_reason.min_length = 0;
        policy.require_close_reason.regex = Some(r"^Fix: ".to_string());

        let violations = evaluate(&policy, &evidence_with_reason("Fix: race in foo", "bd-1"));
        assert!(violations.is_empty());
    }

    #[test]
    fn close_reason_invalid_regex_surfaces_a_violation() {
        let mut policy = ClosePolicy::default();
        policy.require_close_reason.enabled = true;
        policy.require_close_reason.min_length = 0;
        policy.require_close_reason.regex = Some("(unclosed".to_string());

        let violations = evaluate(&policy, &evidence_with_reason("anything goes here", "bd-1"));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].gate, "close_reason_regex_invalid");
    }

    #[test]
    fn acceptance_criteria_unchecked_blocks_close() {
        let mut policy = ClosePolicy::default();
        policy.require_acceptance_criteria_satisfied.enabled = true;
        let body = "## Acceptance Criteria\n- [x] First\n- [ ] Second\n- [ ] Third\n";
        let mut evidence = evidence_with_reason("done done done done done", "bd-1");
        evidence.description = Some(body);

        let violations = evaluate(&policy, &evidence);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].gate, "acceptance_criteria_unchecked");
        assert!(
            violations[0].message.contains("2 unchecked"),
            "{}",
            violations[0].message
        );
    }

    #[test]
    fn acceptance_criteria_passes_when_all_checked() {
        let mut policy = ClosePolicy::default();
        policy.require_acceptance_criteria_satisfied.enabled = true;
        let body = "## Acceptance Criteria\n- [x] First\n- [X] Second\n";
        let mut evidence = evidence_with_reason("done done done done done", "bd-1");
        evidence.description = Some(body);

        assert!(evaluate(&policy, &evidence).is_empty());
    }

    #[test]
    fn acceptance_criteria_only_scans_section_under_header() {
        let mut policy = ClosePolicy::default();
        policy.require_acceptance_criteria_satisfied.enabled = true;
        let body = "## Notes\n- [ ] random todo not under AC\n## Acceptance Criteria\n- [x] all good\n## Out of section\n- [ ] this is ignored\n";
        let mut evidence = evidence_with_reason("done done done done done", "bd-1");
        evidence.description = Some(body);

        assert!(
            evaluate(&policy, &evidence).is_empty(),
            "TODO outside AC section should NOT block close"
        );
    }

    #[test]
    fn acceptance_criteria_does_not_treat_hash_references_as_section_headers() {
        let mut policy = ClosePolicy::default();
        policy.require_acceptance_criteria_satisfied.enabled = true;
        let body =
            "## Acceptance Criteria\n#123 tracks the rollout\n- [ ] Finish after the reference\n";
        let mut evidence = evidence_with_reason("done done done done done", "bd-1");
        evidence.description = Some(body);

        let violations = evaluate(&policy, &evidence);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].gate, "acceptance_criteria_unchecked");
        assert!(violations[0].message.contains("Finish after the reference"));
    }

    #[test]
    fn acceptance_criteria_ignores_section_headers_inside_fenced_code() {
        let mut policy = ClosePolicy::default();
        policy.require_acceptance_criteria_satisfied.enabled = true;
        let body = "## Notes\n```markdown\n## Acceptance Criteria\n- [ ] Example only\n```\n## Acceptance Criteria\n- [x] Real criterion\n";
        let mut evidence = evidence_with_reason("done done done done done", "bd-1");
        evidence.description = Some(body);

        assert!(
            evaluate(&policy, &evidence).is_empty(),
            "unchecked boxes inside fenced examples should not block close"
        );
    }

    #[test]
    fn acceptance_criteria_ignores_unchecked_boxes_inside_fenced_code() {
        let mut policy = ClosePolicy::default();
        policy.require_acceptance_criteria_satisfied.enabled = true;
        let body = "## Acceptance Criteria\n- [x] Real criterion\n```sh\n- [ ] Example only\n```\n";
        let mut evidence = evidence_with_reason("done done done done done", "bd-1");
        evidence.description = Some(body);

        assert!(
            evaluate(&policy, &evidence).is_empty(),
            "unchecked boxes inside fenced examples should not block close"
        );
    }

    #[test]
    fn acceptance_criteria_without_markdown_headers_scans_hash_prefixed_body() {
        let mut policy = ClosePolicy::default();
        policy.require_acceptance_criteria_satisfied.enabled = true;
        let mut evidence = evidence_with_reason("done done done done done", "bd-1");
        evidence.acceptance_criteria = Some("#123 follow-up\n- [ ] Finish referenced work\n");

        let violations = evaluate(&policy, &evidence);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].gate, "acceptance_criteria_unchecked");
        assert!(violations[0].message.contains("Finish referenced work"));
    }

    #[test]
    fn acceptance_criteria_dedupes_across_fields() {
        let mut policy = ClosePolicy::default();
        policy.require_acceptance_criteria_satisfied.enabled = true;
        // Same item in description AND acceptance_criteria column — should
        // count once, not twice.
        let mut evidence = evidence_with_reason("done done done done done", "bd-1");
        evidence.description = Some("## Acceptance Criteria\n- [ ] First\n");
        evidence.acceptance_criteria = Some("- [ ] First\n");

        let violations = evaluate(&policy, &evidence);
        assert_eq!(violations.len(), 1);
        let detail = violations[0].detail.as_ref().unwrap();
        assert_eq!(detail["unchecked_count"], 1);
    }

    #[test]
    fn acceptance_criteria_handles_acceptance_criteria_column_without_header() {
        let mut policy = ClosePolicy::default();
        policy.require_acceptance_criteria_satisfied.enabled = true;
        let mut evidence = evidence_with_reason("done done done done done", "bd-1");
        // The acceptance_criteria column is dedicated — there's no `##` header.
        evidence.acceptance_criteria = Some("- [x] First\n- [ ] Second\n");

        let violations = evaluate(&policy, &evidence);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].message.contains("Second"));
    }

    #[test]
    fn forbid_self_close_blocks_when_actors_match() {
        let mut policy = ClosePolicy::default();
        policy.forbid_self_close_after_in_progress.enabled = true;
        let mut evidence = evidence_with_reason("done done done done done", "bd-1");
        evidence.close_actor = "alice";
        evidence.in_progress_actor = Some("alice");

        let violations = evaluate(&policy, &evidence);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].gate, "forbid_self_close_after_in_progress");
    }

    #[test]
    fn forbid_self_close_passes_when_actors_differ() {
        let mut policy = ClosePolicy::default();
        policy.forbid_self_close_after_in_progress.enabled = true;
        let mut evidence = evidence_with_reason("done done done done done", "bd-1");
        evidence.close_actor = "alice";
        evidence.in_progress_actor = Some("bob");

        assert!(evaluate(&policy, &evidence).is_empty());
    }

    #[test]
    fn forbid_self_close_passes_when_no_in_progress_recorded() {
        let mut policy = ClosePolicy::default();
        policy.forbid_self_close_after_in_progress.enabled = true;
        let mut evidence = evidence_with_reason("done done done done done", "bd-1");
        evidence.close_actor = "alice";
        evidence.in_progress_actor = None;

        assert!(evaluate(&policy, &evidence).is_empty());
    }

    // =========================================================================
    // Deferred-dependents gate (beads_rust#303)
    // =========================================================================

    #[test]
    fn deferred_dependents_gate_default_off() {
        let policy = ClosePolicy::default();
        assert!(!policy.forbid_close_with_deferred_dependents.enabled);
        assert!(!policy.is_active());
    }

    #[test]
    fn deferred_dependents_gate_makes_policy_active_when_enabled() {
        let policy = ClosePolicy {
            forbid_close_with_deferred_dependents: ToggleGate { enabled: true },
            ..Default::default()
        };
        assert!(policy.is_active());
    }

    #[test]
    fn deferred_dependents_violation_none_when_empty() {
        assert!(deferred_dependents_violation("bd-1", &[]).is_none());
    }

    #[test]
    fn deferred_dependents_violation_names_offending_ids() {
        let violation =
            deferred_dependents_violation("bd-1", &["bd-3".to_string(), "bd-2".to_string()])
                .expect("violation expected");
        assert_eq!(violation.gate, GATE_FORBID_CLOSE_WITH_DEFERRED_DEPENDENTS);
        // IDs are sorted deterministically and both are named.
        assert!(violation.message.contains("bd-2"), "{}", violation.message);
        assert!(violation.message.contains("bd-3"), "{}", violation.message);
        assert!(
            violation.message.contains("2 deferred dependent"),
            "{}",
            violation.message
        );
        // Remediation guidance is present.
        assert!(
            violation.message.contains("br update <dep> --status=open"),
            "{}",
            violation.message
        );
        assert!(
            violation.message.contains("duplicate_of"),
            "{}",
            violation.message
        );

        let detail = violation.detail.as_ref().unwrap();
        assert_eq!(detail["issue_id"], "bd-1");
        assert_eq!(detail["deferred_dependent_count"], 2);
        assert_eq!(
            detail["deferred_dependents"],
            serde_json::json!(["bd-2", "bd-3"])
        );
    }

    #[test]
    fn deferred_dependents_violation_dedupes_ids() {
        let violation = deferred_dependents_violation(
            "bd-1",
            &["bd-2".to_string(), "bd-2".to_string(), "bd-3".to_string()],
        )
        .expect("violation expected");
        let detail = violation.detail.as_ref().unwrap();
        assert_eq!(detail["deferred_dependent_count"], 2);
        assert!(violation.message.contains("2 deferred dependent"));
    }

    #[test]
    fn attribution_resolve_prefers_cli_over_env() {
        let env = |key: &str| match key {
            ENV_AGENT_NAME => Some("env-agent".to_string()),
            ENV_HARNESS => Some("env-harness".to_string()),
            ENV_MODEL => Some("env-model".to_string()),
            _ => None,
        };
        let values = AttributionValues::resolve(Some("cli-agent"), Some("cli-harness"), None, &env);
        assert_eq!(values.agent_name.as_deref(), Some("cli-agent"));
        assert_eq!(values.harness.as_deref(), Some("cli-harness"));
        // Falls back to env when CLI absent.
        assert_eq!(values.model.as_deref(), Some("env-model"));
    }

    #[test]
    fn attribution_resolve_treats_blank_strings_as_absent() {
        let env = |key: &str| {
            if key == ENV_HARNESS {
                Some("   ".to_string())
            } else {
                None
            }
        };
        let values = AttributionValues::resolve(Some(""), None, None, &env);
        assert!(values.agent_name.is_none());
        assert!(
            values.harness.is_none(),
            "blank env var should not populate"
        );
        assert!(values.model.is_none());
        assert!(values.is_empty());
    }

    #[test]
    fn multiple_gates_aggregate_violations() {
        let mut policy = ClosePolicy::default();
        policy.require_close_reason.enabled = true;
        policy.require_close_reason.min_length = 50;
        policy.forbid_self_close_after_in_progress.enabled = true;
        policy.require_acceptance_criteria_satisfied.enabled = true;

        let body = "## Acceptance Criteria\n- [ ] Outstanding\n";
        let mut evidence = evidence_with_reason("short", "bd-1");
        evidence.description = Some(body);
        evidence.close_actor = "alice";
        evidence.in_progress_actor = Some("alice");

        let violations = evaluate(&policy, &evidence);
        assert_eq!(violations.len(), 3);
        let gates: Vec<&str> = violations.iter().map(|v| v.gate.as_str()).collect();
        assert!(gates.contains(&"close_reason_min_length"));
        assert!(gates.contains(&"acceptance_criteria_unchecked"));
        assert!(gates.contains(&"forbid_self_close_after_in_progress"));
    }

    #[test]
    fn loader_returns_default_when_file_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = load_for_beads_dir(dir.path()).expect("load");
        assert_eq!(policy, PolicyDocument::default());
        assert!(!policy.close_policy.is_active());
        assert!(policy.allow_bypass);
    }

    #[test]
    fn loader_parses_full_document() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml = r#"
close_policy:
  require_close_reason:
    enabled: true
    min_length: 30
    regex: "^Fix: "
  require_acceptance_criteria_satisfied:
    enabled: true
  forbid_self_close_after_in_progress:
    enabled: true
  require_typed_references:
    enabled: true
    required_kinds: ["commit", "reviewer"]
  attribution:
    tier: capture
    fields: ["agent_name", "harness", "model"]
allow_bypass: false
"#;
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("load");
        assert!(policy.close_policy.require_close_reason.enabled);
        assert_eq!(policy.close_policy.require_close_reason.min_length, 30);
        assert_eq!(
            policy.close_policy.require_close_reason.regex.as_deref(),
            Some("^Fix: ")
        );
        assert!(
            policy
                .close_policy
                .require_acceptance_criteria_satisfied
                .enabled
        );
        assert!(
            policy
                .close_policy
                .forbid_self_close_after_in_progress
                .enabled
        );
        assert!(policy.close_policy.require_typed_references.enabled);
        assert_eq!(
            policy.close_policy.require_typed_references.required_kinds,
            vec!["commit".to_string(), "reviewer".to_string()]
        );
        assert_eq!(
            policy.close_policy.attribution.tier,
            AttributionTier::Capture
        );
        assert!(!policy.allow_bypass);
        assert!(policy.close_policy.is_active());
    }

    /// beads_rust#302: unknown fields used to hard-fail and take down `br
    /// close` project-wide. They are now tolerated — the parse succeeds and
    /// the unknown keys surface via [`detect_unknown_policy_fields`].
    #[test]
    fn loader_tolerates_unknown_top_level_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml = "unknown_key: 1\nclose_policy:\n  require_close_reason:\n    enabled: true\n";
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("load must succeed");
        assert!(
            policy.close_policy.require_close_reason.enabled,
            "known fields must still parse"
        );

        let raw: serde_yml::Value = serde_yml::from_str(yaml).unwrap();
        let unknown = detect_unknown_policy_fields(&raw);
        assert_eq!(unknown, vec!["unknown_key".to_string()]);
    }

    /// beads_rust#302: unknown fields nested under `close_policy:` (the
    /// regression class the issue specifically called out — a typo or
    /// experimental gate) must also be tolerated and surfaced as a dotted
    /// path so operators can find them.
    #[test]
    fn loader_tolerates_unknown_field_under_close_policy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml = r"
close_policy:
  require_close_reason:
    enabled: true
    min_length: 20
  require_new_experimental_field:
    enabled: true
";
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("load must succeed");
        assert!(policy.close_policy.require_close_reason.enabled);
        assert_eq!(policy.close_policy.require_close_reason.min_length, 20);

        let raw: serde_yml::Value = serde_yml::from_str(yaml).unwrap();
        let unknown = detect_unknown_policy_fields(&raw);
        assert_eq!(
            unknown,
            vec!["close_policy.require_new_experimental_field".to_string()]
        );
    }

    /// Typos buried deeper (under known sub-blocks) also surface with
    /// their full dotted path.
    #[test]
    fn detect_unknown_policy_fields_walks_nested_structs() {
        let yaml = r#"
close_policy:
  require_close_reason:
    enabled: true
    min_lenght: 20            # typo: should be min_length
  attribution:
    tier: capture
    fileds: ["agent_name"]    # typo: should be fields
"#;
        let raw: serde_yml::Value = serde_yml::from_str(yaml).unwrap();
        let unknown = detect_unknown_policy_fields(&raw);
        assert_eq!(
            unknown,
            vec![
                "close_policy.attribution.fileds".to_string(),
                "close_policy.require_close_reason.min_lenght".to_string(),
            ]
        );
    }

    /// A fully-canonical document produces no unknown-field reports.
    #[test]
    fn detect_unknown_policy_fields_is_empty_for_canonical_doc() {
        let yaml = r#"
allow_bypass: false
close_policy:
  require_close_reason:
    enabled: true
    min_length: 30
    regex: "^Fix: "
  require_acceptance_criteria_satisfied:
    enabled: true
  forbid_self_close_after_in_progress:
    enabled: true
  require_typed_references:
    enabled: true
    required_kinds: ["commit", "reviewer"]
  attribution:
    tier: capture
    fields: ["agent_name", "harness", "model"]
"#;
        let raw: serde_yml::Value = serde_yml::from_str(yaml).unwrap();
        assert!(detect_unknown_policy_fields(&raw).is_empty());
    }

    #[test]
    fn loader_accepts_empty_document() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(POLICY_FILE_NAME), "{}\n").unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("load");
        assert_eq!(policy, PolicyDocument::default());
    }

    #[test]
    fn parse_unchecked_box_recognises_variants() {
        assert_eq!(
            parse_unchecked_box("- [ ] todo item").as_deref(),
            Some("todo item")
        );
        assert_eq!(
            parse_unchecked_box("* [ ] starred").as_deref(),
            Some("starred")
        );
        assert_eq!(parse_unchecked_box("+ [ ] plus").as_deref(), Some("plus"));
        assert!(parse_unchecked_box("- [x] checked").is_none());
        assert!(parse_unchecked_box("- [X] checked").is_none());
        assert!(parse_unchecked_box("plain text").is_none());
        assert!(parse_unchecked_box("- not a box").is_none());
    }

    // =========================================================================
    // Typed-references gate (capability #3 of issue #274)
    // =========================================================================

    #[test]
    fn extract_typed_references_finds_kind_value_pairs() {
        let refs = extract_typed_references("Fixed in commit:abc123 per reviewer:bob");
        assert_eq!(
            refs,
            vec![
                ("commit".to_string(), "abc123".to_string()),
                ("reviewer".to_string(), "bob".to_string()),
            ]
        );
    }

    #[test]
    fn extract_typed_references_handles_hyphenated_kinds() {
        let refs = extract_typed_references("see agent-mail:thread-xyz for context");
        assert_eq!(
            refs,
            vec![("agent-mail".to_string(), "thread-xyz".to_string())]
        );
    }

    #[test]
    fn extract_typed_references_skips_prose_with_colons() {
        // `note:` followed by whitespace is prose, not a typed reference.
        let refs = extract_typed_references("note: this is a regular sentence");
        assert!(refs.is_empty(), "got {refs:?}");
    }

    #[test]
    fn extract_typed_references_requires_letter_start() {
        let refs = extract_typed_references("bad refs: 12:abc and -kind:value");
        assert!(refs.is_empty(), "got {refs:?}");
    }

    #[test]
    fn typed_references_gate_rejects_when_none_present() {
        let policy = ClosePolicy {
            require_typed_references: RequireTypedReferences {
                enabled: true,
                required_kinds: vec![],
            },
            ..Default::default()
        };
        let evidence = evidence_with_reason("just plain prose with no refs at all", "bd-1");
        let violations = evaluate(&policy, &evidence);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].gate, "typed_references_required");
    }

    #[test]
    fn typed_references_gate_empty_required_kinds_accepts_builtin_kind() {
        let policy = ClosePolicy {
            require_typed_references: RequireTypedReferences {
                enabled: true,
                required_kinds: vec![],
            },
            ..Default::default()
        };
        let evidence = evidence_with_reason("Fixed in reviewer:bob", "bd-1");
        assert!(evaluate(&policy, &evidence).is_empty());
    }

    #[test]
    fn typed_references_gate_empty_required_kinds_rejects_unknown_kind() {
        let policy = ClosePolicy {
            require_typed_references: RequireTypedReferences {
                enabled: true,
                required_kinds: vec![],
            },
            ..Default::default()
        };
        let evidence = evidence_with_reason("Captured in tracker:ABC-123", "bd-1");
        let violations = evaluate(&policy, &evidence);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].gate, "typed_references_required_kind_missing");
    }

    #[test]
    fn typed_references_gate_empty_required_kinds_rejects_bare_url() {
        let policy = ClosePolicy {
            require_typed_references: RequireTypedReferences {
                enabled: true,
                required_kinds: vec![],
            },
            ..Default::default()
        };
        let evidence = evidence_with_reason("Evidence: https://example.invalid/report", "bd-1");
        let violations = evaluate(&policy, &evidence);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].gate, "typed_references_required_kind_missing");
    }

    #[test]
    fn typed_references_gate_accepts_when_kind_matches() {
        let policy = ClosePolicy {
            require_typed_references: RequireTypedReferences {
                enabled: true,
                required_kinds: vec!["commit".to_string()],
            },
            ..Default::default()
        };
        let evidence = evidence_with_reason("Fixed in commit:abc12345", "bd-1");
        assert!(evaluate(&policy, &evidence).is_empty());
    }

    #[test]
    fn typed_references_gate_rejects_wrong_kind() {
        let policy = ClosePolicy {
            require_typed_references: RequireTypedReferences {
                enabled: true,
                required_kinds: vec!["commit".to_string()],
            },
            ..Default::default()
        };
        let evidence = evidence_with_reason("see investigation:linear-XYZ-42 for details", "bd-1");
        let violations = evaluate(&policy, &evidence);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].gate, "typed_references_required_kind_missing");
    }

    /// Drift guard for beads_rust#302: `PolicyNode::child_table()` is a
    /// hand-maintained mirror of the typed close-policy struct fields. If a
    /// new field is added to one of the structs without also being added
    /// to the table, `detect_unknown_policy_fields` will fire a
    /// **false-positive** "unknown field" warning on every canonical
    /// document containing that field. The owner explicitly acknowledged
    /// this sync hazard in the commit message — this test makes the drift
    /// impossible to ship: it serialises a `Default` instance of every
    /// close-policy struct and asserts the produced key set is a subset of
    /// the corresponding `PolicyNode`'s `child_table()` keys.
    ///
    /// We assert "subset" rather than "equality" because table keys may
    /// intentionally list `Option<T>` fields that serialise to nothing in
    /// the default form (none today, but future-proofing).
    #[test]
    fn policy_node_child_table_covers_every_typed_struct_field() {
        fn field_names_of<T: serde::Serialize + Default>() -> Vec<String> {
            let value =
                serde_yml::to_value(T::default()).expect("default struct must serialise to value");
            let mapping = value
                .as_mapping()
                .expect("default struct must serialise as a mapping");
            mapping
                .iter()
                .filter_map(|(k, _)| k.as_str().map(String::from))
                .collect()
        }

        fn assert_table_covers(node: PolicyNode, struct_fields: &[String], struct_name: &str) {
            let table_keys: std::collections::HashSet<&'static str> =
                node.child_table().iter().map(|(k, _)| *k).collect();
            for field in struct_fields {
                assert!(
                    table_keys.contains(field.as_str()),
                    "PolicyNode::{node:?}::child_table() is missing key `{field}` declared on \
                     struct `{struct_name}`. `detect_unknown_policy_fields` would emit a \
                     FALSE-POSITIVE 'unknown field' warning on every canonical policy.yaml that \
                     uses this field. Add the entry to `child_table()` (see beads_rust#302).",
                );
            }
        }

        assert_table_covers(
            PolicyNode::Document,
            &field_names_of::<PolicyDocument>(),
            "PolicyDocument",
        );
        assert_table_covers(
            PolicyNode::ClosePolicy,
            &field_names_of::<ClosePolicy>(),
            "ClosePolicy",
        );
        assert_table_covers(
            PolicyNode::RequireCloseReason,
            &field_names_of::<RequireCloseReason>(),
            "RequireCloseReason",
        );
        assert_table_covers(
            PolicyNode::ToggleGate,
            &field_names_of::<ToggleGate>(),
            "ToggleGate",
        );
        assert_table_covers(
            PolicyNode::Attribution,
            &field_names_of::<Attribution>(),
            "Attribution",
        );
        assert_table_covers(
            PolicyNode::RequireTypedReferences,
            &field_names_of::<RequireTypedReferences>(),
            "RequireTypedReferences",
        );
        assert_table_covers(
            PolicyNode::Workflow,
            &field_names_of::<Workflow>(),
            "Workflow",
        );
    }

    /// Inverse drift guard: every key listed in `PolicyNode::child_table()`
    /// must correspond to an actual field on the typed struct. Otherwise a
    /// stale entry would silently SUPPRESS the unknown-field warning for a
    /// field that no longer exists (false negative: typo in YAML matches a
    /// dead table entry → no warning, but the field is also not honoured by
    /// the typed parse).
    ///
    /// `regex: Option<String>` is in the default serialised mapping as a
    /// null entry, so it counts as "present" for this check.
    #[test]
    fn policy_node_child_table_has_no_stale_entries() {
        fn field_names_of<T: serde::Serialize + Default>() -> std::collections::HashSet<String> {
            let value =
                serde_yml::to_value(T::default()).expect("default struct must serialise to value");
            let mapping = value
                .as_mapping()
                .expect("default struct must serialise as a mapping");
            mapping
                .iter()
                .filter_map(|(k, _)| k.as_str().map(String::from))
                .collect()
        }

        fn assert_no_stale(
            node: PolicyNode,
            struct_fields: &std::collections::HashSet<String>,
            struct_name: &str,
        ) {
            for (key, _) in node.child_table() {
                assert!(
                    struct_fields.contains(*key),
                    "PolicyNode::{node:?}::child_table() lists key `{key}` that does not exist \
                     on struct `{struct_name}`. A typo of this key in policy.yaml would NOT be \
                     reported as unknown even though it is silently ignored by the typed parse \
                     (see beads_rust#302).",
                );
            }
        }

        assert_no_stale(
            PolicyNode::Document,
            &field_names_of::<PolicyDocument>(),
            "PolicyDocument",
        );
        assert_no_stale(
            PolicyNode::ClosePolicy,
            &field_names_of::<ClosePolicy>(),
            "ClosePolicy",
        );
        assert_no_stale(
            PolicyNode::RequireCloseReason,
            &field_names_of::<RequireCloseReason>(),
            "RequireCloseReason",
        );
        assert_no_stale(
            PolicyNode::ToggleGate,
            &field_names_of::<ToggleGate>(),
            "ToggleGate",
        );
        assert_no_stale(
            PolicyNode::Attribution,
            &field_names_of::<Attribution>(),
            "Attribution",
        );
        assert_no_stale(
            PolicyNode::RequireTypedReferences,
            &field_names_of::<RequireTypedReferences>(),
            "RequireTypedReferences",
        );
        assert_no_stale(
            PolicyNode::Workflow,
            &field_names_of::<Workflow>(),
            "Workflow",
        );
    }

    // =========================================================================
    // Status-workflow policy (issue #311)
    // =========================================================================

    fn strict_workflow() -> Workflow {
        Workflow {
            strict: true,
            statuses: vec![
                "open".to_string(),
                "in_progress".to_string(),
                "closed".to_string(),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn workflow_default_is_not_enforced() {
        let workflow = Workflow::default();
        assert!(!workflow.is_enforced());
        // With enforcement off every status is permitted.
        assert!(workflow.validate_status("anything-at-all").is_ok());
    }

    #[test]
    fn workflow_strict_but_empty_statuses_is_not_enforced() {
        let workflow = Workflow {
            strict: true,
            statuses: vec![],
            ..Default::default()
        };
        assert!(!workflow.is_enforced());
        assert!(workflow.validate_status("bogus").is_ok());
    }

    #[test]
    fn workflow_rejects_status_outside_the_set() {
        let workflow = strict_workflow();
        let err = workflow
            .validate_status("completed")
            .expect_err("out-of-set status must be rejected");
        let message = err.to_string();
        assert!(message.contains("completed"), "{message}");
        // The error names the allowed values so the user can self-correct.
        assert!(message.contains("open"), "{message}");
        assert!(message.contains("in_progress"), "{message}");
        assert!(message.contains("closed"), "{message}");
    }

    #[test]
    fn workflow_allows_status_in_the_set() {
        let workflow = strict_workflow();
        assert!(workflow.validate_status("open").is_ok());
        assert!(workflow.validate_status("in_progress").is_ok());
        assert!(workflow.validate_status("closed").is_ok());
    }

    #[test]
    fn workflow_status_match_is_case_insensitive() {
        let workflow = Workflow {
            strict: true,
            statuses: vec!["In_Progress".to_string()],
            ..Default::default()
        };
        assert!(workflow.allows("in_progress"));
        assert!(workflow.validate_status("in_progress").is_ok());
    }

    #[test]
    fn workflow_supports_custom_statuses() {
        let workflow = Workflow {
            strict: true,
            statuses: vec!["open".to_string(), "in_review".to_string()],
            ..Default::default()
        };
        assert!(workflow.validate_status("in_review").is_ok());
        assert!(workflow.validate_status("blocked").is_err());
    }

    #[test]
    fn loader_parses_workflow_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml = r#"
workflow:
  strict: true
  statuses: ["open", "in_progress", "closed"]
"#;
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("load");
        assert!(policy.workflow.is_enforced());
        assert_eq!(
            policy.workflow.statuses,
            vec![
                "open".to_string(),
                "in_progress".to_string(),
                "closed".to_string()
            ]
        );
        assert!(policy.workflow.validate_status("open").is_ok());
        assert!(policy.workflow.validate_status("completed").is_err());
    }

    #[test]
    fn loader_absent_workflow_section_is_permissive() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A policy.yaml that configures close gates but NO workflow section
        // must not enforce any status set.
        let yaml = "close_policy:\n  forbid_self_close_after_in_progress:\n    enabled: true\n";
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("load");
        assert!(!policy.workflow.is_enforced());
        assert!(policy.workflow.validate_status("whatever").is_ok());
    }

    #[test]
    fn detect_unknown_policy_fields_walks_workflow_typos() {
        let yaml = r#"
workflow:
  strict: true
  statusses: ["open"]   # typo: should be statuses
"#;
        let raw: serde_yml::Value = serde_yml::from_str(yaml).unwrap();
        let unknown = detect_unknown_policy_fields(&raw);
        assert_eq!(unknown, vec!["workflow.statusses".to_string()]);
    }

    #[test]
    fn detect_unknown_policy_fields_accepts_canonical_workflow() {
        let yaml = r#"
workflow:
  strict: true
  statuses: ["open", "closed"]
"#;
        let raw: serde_yml::Value = serde_yml::from_str(yaml).unwrap();
        assert!(detect_unknown_policy_fields(&raw).is_empty());
    }

    #[test]
    fn loader_parses_and_validates_repository_capacity_policy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml = r"
workflow:
  statuses: [open, in_progress, in_review, rework, closed]
  capacity:
    statuses:
      in_progress:
        soft: 2
        hard: 3
    groups:
      active_work:
        statuses: [in_progress, in_review, rework]
        hard: 5
    admission:
      - name: drain_downstream
        transitions:
          from: [open]
          to: [in_progress]
        require_below:
          statuses:
            in_review: 2
          groups:
            active_work: 5
";
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("capacity policy must load");
        assert!(policy.workflow.capacity.is_active());
        assert_eq!(
            policy.workflow.capacity.statuses["in_progress"].hard,
            Some(3)
        );
        assert_eq!(
            policy.workflow.capacity.groups["active_work"].statuses,
            vec!["in_progress", "in_review", "rework"]
        );
        assert_eq!(
            policy.workflow.capacity.admission[0].name,
            "drain_downstream"
        );
    }

    #[test]
    fn loader_parses_capacity_exemption_policy_and_authorizes_case_insensitively() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml = r"
workflow:
  statuses: [open, in_progress, closed]
  capacity:
    statuses:
      in_progress:
        hard: 3
    exemptions:
      providers: [Operator, release-manager]
      require_expiry: true
      max_ttl_seconds: 86400
";
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("exemption policy must load");
        let exemptions = &policy.workflow.capacity.exemptions;
        assert!(exemptions.is_enabled());
        assert!(exemptions.require_expiry);
        assert_eq!(exemptions.max_ttl_seconds, Some(86400));
        assert!(exemptions.authorizes("operator"));
        assert!(exemptions.authorizes("  RELEASE-MANAGER  "));
        assert!(!exemptions.authorizes("intruder"));
        assert!(!exemptions.authorizes(""));
    }

    #[test]
    fn capacity_exemptions_validation_rejects_duplicates_and_zero_ttl() {
        let workflow: Workflow = serde_yml::from_str(
            r"
statuses: [open, in_progress]
capacity:
  exemptions:
    providers: [operator, OPERATOR]
",
        )
        .unwrap();
        let error = workflow.validate_capacity().unwrap_err();
        assert!(
            error.to_string().contains("duplicated case-insensitively"),
            "{error}"
        );

        let workflow: Workflow = serde_yml::from_str(
            r"
statuses: [open, in_progress]
capacity:
  exemptions:
    providers: [operator]
    max_ttl_seconds: 0
",
        )
        .unwrap();
        let error = workflow.validate_capacity().unwrap_err();
        assert!(error.to_string().contains("max_ttl_seconds"), "{error}");

        // An exemptions block alone (no limits) must still validate loudly
        // rather than lie dormant, mirroring the counting block.
        let workflow: Workflow = serde_yml::from_str(
            r"
capacity:
  exemptions:
    providers: ['  ']
",
        )
        .unwrap();
        let error = workflow.validate_capacity().unwrap_err();
        assert!(error.to_string().contains("cannot be empty"), "{error}");
    }

    #[test]
    fn capacity_evidence_display_appends_exempt_total_only_when_present() {
        let mut warning = WorkflowCapacityWarning {
            issue_id: "bd-1".to_string(),
            from_status: Some("open".to_string()),
            to_status: "in_progress".to_string(),
            capacity_kind: "status".to_string(),
            capacity_name: "in_progress".to_string(),
            scope: "repository".to_string(),
            scope_key: None,
            counting_mode: "all".to_string(),
            aggregate_parents_excluded: None,
            exempt: None,
            current: 2,
            prospective: 3,
            soft_limit: 2,
            hard_limit: None,
            policy_path: "workflow.capacity.statuses.in_progress".to_string(),
        };
        assert!(
            !warning.to_string().contains("exempt"),
            "no-exemption text must stay byte-stable: {warning}"
        );
        warning.exempt = Some(2);
        assert!(warning.to_string().contains(", exempt: 2"), "{warning}");
    }

    #[test]
    fn capacity_rejects_undeclared_status_and_inverted_thresholds() {
        let workflow: Workflow = serde_yml::from_str(
            r"
statuses: [open, in_progress]
capacity:
  statuses:
    in_review:
      soft: 4
      hard: 2
",
        )
        .unwrap();
        let error = workflow.validate_capacity().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("undeclared workflow status 'in_review'"),
            "{error}"
        );

        let workflow: Workflow = serde_yml::from_str(
            r"
statuses: [open, in_progress]
capacity:
  statuses:
    in_progress:
      soft: 4
      hard: 2
",
        )
        .unwrap();
        let error = workflow.validate_capacity().unwrap_err();
        assert!(error.to_string().contains("cannot exceed"), "{error}");
    }

    #[test]
    fn capacity_rejects_unknown_nested_policy_keys() {
        let error = serde_yml::from_str::<Workflow>(
            r"
statuses: [open, in_progress]
capacity:
  statuses:
    in_progress:
      herd: 2
",
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn capacity_counting_defaults_to_all_and_parses_every_mode() {
        let workflow: Workflow = serde_yml::from_str(
            r"
statuses: [open, in_progress]
capacity:
  statuses:
    in_progress:
      hard: 2
",
        )
        .unwrap();
        assert_eq!(
            workflow.capacity.counting.hierarchy,
            CapacityCountingMode::All
        );
        workflow.validate_capacity().unwrap();

        for (configured, expected) in [
            ("leaf_work", CapacityCountingMode::LeafWork),
            ("roots", CapacityCountingMode::Roots),
            ("all", CapacityCountingMode::All),
        ] {
            let workflow: Workflow = serde_yml::from_str(&format!(
                r"
statuses: [open, in_progress]
capacity:
  counting:
    hierarchy: {configured}
  statuses:
    in_progress:
      hard: 2
"
            ))
            .unwrap();
            assert_eq!(workflow.capacity.counting.hierarchy, expected);
            assert_eq!(workflow.capacity.counting.hierarchy.as_str(), configured);
            workflow.validate_capacity().unwrap();
        }
    }

    #[test]
    fn capacity_counting_rejects_unknown_modes_and_misplaced_weights() {
        let error = serde_yml::from_str::<Workflow>(
            r"
statuses: [open, in_progress]
capacity:
  counting:
    hierarchy: leafwork
",
        )
        .unwrap_err();
        assert!(error.to_string().contains("leafwork"), "{error}");

        // Weights without `hierarchy: weighted` are a silent no-op unless
        // validation rejects them.
        let workflow: Workflow = serde_yml::from_str(
            r"
statuses: [open, in_progress]
capacity:
  counting:
    hierarchy: leaf_work
    weights:
      types:
        epic: 0
  statuses:
    in_progress:
      hard: 2
",
        )
        .unwrap();
        let error = workflow.validate_capacity().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("counting.weights requires counting.hierarchy: weighted"),
            "{error}"
        );

        // Counting is validated even with no limits configured at all.
        let workflow: Workflow = serde_yml::from_str(
            r"
statuses: [open, in_progress]
capacity:
  counting:
    weights:
      default: 2
",
        )
        .unwrap();
        assert!(!workflow.capacity.is_active());
        assert!(workflow.validate_capacity().is_err());
    }

    #[test]
    fn capacity_weight_resolution_prefers_issue_then_type_then_default() {
        let mut weights = CapacityWeights {
            default: Some(3),
            ..CapacityWeights::default()
        };
        weights.types.insert("Epic".to_string(), 0);
        weights.issues.insert("bd-1".to_string(), 7);

        assert_eq!(weights.weight_for("bd-1", "task"), 7, "per-issue wins");
        assert_eq!(weights.weight_for("bd-2", "epic"), 0, "type is next");
        assert_eq!(
            weights.weight_for("bd-2", "EPIC"),
            0,
            "type is case-insensitive"
        );
        assert_eq!(weights.weight_for("bd-2", "task"), 3, "default applies");
        assert_eq!(
            CapacityWeights::default().weight_for("bd-2", "task"),
            1,
            "unconfigured weight is 1"
        );
    }

    #[test]
    fn capacity_rejects_case_ambiguous_map_keys() {
        let workflow: Workflow = serde_yml::from_str(
            r"
statuses: [open, in_progress]
capacity:
  statuses:
    in_progress:
      hard: 1
    IN_PROGRESS:
      hard: 2
",
        )
        .unwrap();
        let error = workflow.validate_capacity().unwrap_err();
        assert!(
            error.to_string().contains("duplicated case-insensitively"),
            "{error}"
        );
    }

    #[test]
    fn loader_parses_and_validates_capacity_scopes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml = r"
workflow:
  statuses: [open, in_progress, in_review, closed]
  capacity:
    scopes:
      actor:
        statuses:
          in_progress:
            hard: 2
      harness:
        groups:
          active_work:
            statuses: [in_progress, in_review]
            hard: 3
      subtree:
        statuses:
          in_progress:
            soft: 1
";
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("scope policy must load");
        let capacity = &policy.workflow.capacity;
        assert!(capacity.is_active(), "scopes alone activate capacity");
        assert_eq!(
            capacity.scopes["actor"].statuses["in_progress"].hard,
            Some(2)
        );
        assert_eq!(
            capacity.scopes["harness"].groups["active_work"].hard,
            Some(3)
        );
        assert_eq!(
            capacity.scopes["subtree"].statuses["in_progress"].soft,
            Some(1)
        );
    }

    #[test]
    fn capacity_scope_validation_rejects_bad_shapes() {
        let expect_error = |yaml: &str, needle: &str| {
            let workflow: Workflow = serde_yml::from_str(yaml).unwrap();
            let error = workflow.validate_capacity().unwrap_err();
            assert!(
                error.to_string().contains(needle),
                "expected '{needle}' in: {error}"
            );
        };

        expect_error(
            r"
statuses: [open, in_progress]
capacity:
  scopes:
    fleet:
      statuses:
        in_progress:
          hard: 1
",
            "not recognized",
        );
        expect_error(
            r"
statuses: [open, in_progress]
capacity:
  scopes:
    actor: {}
",
            "at least one status or group limit",
        );
        expect_error(
            r"
statuses: [open, in_progress]
capacity:
  scopes:
    actor:
      statuses:
        shipping:
          hard: 1
",
            "scope 'actor'",
        );
        expect_error(
            r"
statuses: [open, in_progress]
capacity:
  scopes:
    actor:
      statuses:
        in_progress:
          hard: 1
    ACTOR:
      statuses:
        in_progress:
          hard: 2
",
            "duplicated case-insensitively",
        );
    }

    #[test]
    fn capacity_scope_evidence_display_names_the_partition_key() {
        let violation = WorkflowCapacityViolation {
            issue_id: "bd-1".to_string(),
            from_status: Some("open".to_string()),
            to_status: "in_progress".to_string(),
            capacity_kind: "status".to_string(),
            capacity_name: "in_progress".to_string(),
            scope: "actor".to_string(),
            scope_key: Some("alice".to_string()),
            counting_mode: "all".to_string(),
            aggregate_parents_excluded: None,
            exempt: None,
            current: 2,
            prospective: 3,
            soft_limit: None,
            hard_limit: 2,
            policy_path: "workflow.capacity.scopes.actor.statuses.in_progress".to_string(),
        };
        let rendered = violation.to_string();
        assert!(
            rendered.contains("actor status capacity 'in_progress' for 'alice'"),
            "{rendered}"
        );

        // Repository-scope evidence keeps the pre-scope text byte-stable.
        let mut repository = violation;
        repository.scope = "repository".to_string();
        repository.scope_key = None;
        repository.policy_path = "workflow.capacity.statuses.in_progress".to_string();
        let rendered = repository.to_string();
        assert!(
            rendered.contains("repository status capacity 'in_progress' would exceed"),
            "{rendered}"
        );
        assert!(!rendered.contains("for '"), "{rendered}");
    }

    // =========================================================================
    // Status-transition rules (issue #312, layer 1)
    // =========================================================================

    fn transition_workflow() -> Workflow {
        let mut transitions = std::collections::BTreeMap::new();
        transitions.insert(
            "open".to_string(),
            vec!["in_progress".to_string(), "closed".to_string()],
        );
        transitions.insert(
            "in_progress".to_string(),
            vec!["in_review".to_string(), "blocked".to_string()],
        );
        transitions.insert("in_review".to_string(), vec!["closed".to_string()]);
        Workflow {
            strict: true,
            statuses: vec![],
            transitions,
            ..Default::default()
        }
    }

    #[test]
    fn transitions_default_is_not_enforced() {
        let workflow = Workflow::default();
        assert!(!workflow.transitions_enforced());
        // With enforcement off every transition is permitted.
        assert!(workflow.validate_transition(Some("open"), "bogus").is_ok());
        assert!(workflow.validate_transition(None, "bogus").is_ok());
    }

    #[test]
    fn transitions_present_but_not_strict_is_not_enforced() {
        let mut workflow = transition_workflow();
        workflow.strict = false;
        assert!(!workflow.transitions_enforced());
        // Not strict => transitions advisory only, nothing rejected.
        assert!(
            workflow
                .validate_transition(Some("open"), "deferred")
                .is_ok()
        );
    }

    #[test]
    fn transitions_strict_but_empty_map_is_not_enforced() {
        let workflow = Workflow {
            strict: true,
            statuses: vec![],
            transitions: std::collections::BTreeMap::new(),
            ..Default::default()
        };
        assert!(!workflow.transitions_enforced());
        assert!(
            workflow
                .validate_transition(Some("open"), "blocked")
                .is_ok()
        );
    }

    #[test]
    fn transition_valid_move_is_allowed() {
        let workflow = transition_workflow();
        assert!(workflow.allows_transition("open", "in_progress"));
        assert!(
            workflow
                .validate_transition(Some("open"), "in_progress")
                .is_ok()
        );
        assert!(
            workflow
                .validate_transition(Some("in_progress"), "in_review")
                .is_ok()
        );
        assert!(
            workflow
                .validate_transition(Some("in_review"), "closed")
                .is_ok()
        );
    }

    #[test]
    fn transition_invalid_move_is_rejected_with_actionable_error() {
        let workflow = transition_workflow();
        let err = workflow
            .validate_transition(Some("open"), "in_review")
            .expect_err("open -> in_review is not configured");
        let message = err.to_string();
        // Names current, attempted, and the valid next statuses.
        assert!(message.contains("'open'"), "{message}");
        assert!(message.contains("'in_review'"), "{message}");
        assert!(message.contains("in_progress"), "{message}");
        assert!(message.contains("closed"), "{message}");
        assert!(message.contains("workflow.transitions"), "{message}");
    }

    #[test]
    fn transition_from_status_with_no_rule_rejects_with_none_targets() {
        let workflow = transition_workflow();
        // `blocked` has no entry and there is no `any` wildcard.
        let err = workflow
            .validate_transition(Some("blocked"), "open")
            .expect_err("blocked has no allowed targets");
        let message = err.to_string();
        assert!(message.contains("(none)"), "{message}");
    }

    #[test]
    fn transition_no_op_is_always_allowed() {
        let workflow = transition_workflow();
        // Same status, even when there is no explicit self-loop rule.
        assert!(workflow.validate_transition(Some("open"), "open").is_ok());
        // Even for a status with no rule at all.
        assert!(
            workflow
                .validate_transition(Some("blocked"), "blocked")
                .is_ok()
        );
    }

    #[test]
    fn transition_is_case_insensitive() {
        let workflow = transition_workflow();
        assert!(
            workflow
                .validate_transition(Some("OPEN"), "In_Progress")
                .is_ok()
        );
    }

    #[test]
    fn transition_any_wildcard_allows_target_from_every_status() {
        let mut workflow = transition_workflow();
        workflow
            .transitions
            .insert("any".to_string(), vec!["deferred".to_string()]);
        // `deferred` now allowed from any from-status, including ones with
        // their own rules and ones with none.
        assert!(
            workflow
                .validate_transition(Some("open"), "deferred")
                .is_ok()
        );
        assert!(
            workflow
                .validate_transition(Some("blocked"), "deferred")
                .is_ok()
        );
        // Wildcard targets are merged into the error message's "valid next".
        let err = workflow
            .validate_transition(Some("open"), "bogus")
            .expect_err("bogus is not reachable");
        assert!(err.to_string().contains("deferred"), "{err}");
    }

    #[test]
    fn transition_initial_key_restricts_creates() {
        let mut workflow = transition_workflow();
        workflow.transitions.insert(
            "initial".to_string(),
            vec!["open".to_string(), "draft".to_string()],
        );
        // No prior status => validated against `initial`.
        assert!(workflow.validate_transition(None, "open").is_ok());
        assert!(workflow.validate_transition(None, "draft").is_ok());
        let err = workflow
            .validate_transition(None, "in_progress")
            .expect_err("in_progress is not an allowed initial status");
        let message = err.to_string();
        assert!(message.contains("initial"), "{message}");
        assert!(message.contains("'in_progress'"), "{message}");
        assert!(message.contains("open"), "{message}");
        assert!(message.contains("draft"), "{message}");
    }

    #[test]
    fn transition_absent_initial_key_accepts_any_initial_status() {
        // `transition_workflow()` has no `initial` key — any starting status
        // is accepted since there is no prior state to validate against.
        let workflow = transition_workflow();
        assert!(workflow.validate_transition(None, "open").is_ok());
        assert!(workflow.validate_transition(None, "anything").is_ok());
    }

    #[test]
    fn allowed_targets_from_dedupes_and_merges_wildcard() {
        let mut workflow = transition_workflow();
        // Overlap between explicit `open` targets and the wildcard.
        workflow.transitions.insert(
            "any".to_string(),
            vec!["closed".to_string(), "deferred".to_string()],
        );
        let targets = workflow.allowed_targets_from("open");
        // explicit: in_progress, closed; wildcard adds deferred (closed deduped).
        assert_eq!(
            targets,
            vec![
                "in_progress".to_string(),
                "closed".to_string(),
                "deferred".to_string()
            ]
        );
    }

    #[test]
    fn loader_parses_workflow_transitions_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml = r"
workflow:
  strict: true
  transitions:
    open: [in_progress, deferred, closed]
    in_progress: [in_review, blocked, open]
    any: [closed]
    initial: [open, draft]
";
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("load");
        let workflow = &policy.workflow;
        assert!(workflow.transitions_enforced());
        assert!(
            workflow
                .validate_transition(Some("open"), "in_progress")
                .is_ok()
        );
        assert!(
            workflow
                .validate_transition(Some("open"), "in_review")
                .is_err()
        );
        // `any: [closed]` allows close from a from-status without an explicit rule.
        assert!(
            workflow
                .validate_transition(Some("blocked"), "closed")
                .is_ok()
        );
        // `initial` gates creates.
        assert!(workflow.validate_transition(None, "open").is_ok());
        assert!(workflow.validate_transition(None, "closed").is_err());
    }

    #[test]
    fn loader_absent_transitions_is_not_enforced() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Strict status enforcement but NO transitions map: status moves are
        // unconstrained (backward compatible with #311-only configs).
        let yaml = "workflow:\n  strict: true\n  statuses: [open, in_progress, closed]\n";
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("load");
        assert!(policy.workflow.is_enforced());
        assert!(!policy.workflow.transitions_enforced());
        assert!(
            policy
                .workflow
                .validate_transition(Some("closed"), "open")
                .is_ok()
        );
    }

    #[test]
    fn detect_unknown_policy_fields_accepts_canonical_transitions() {
        let yaml = r"
workflow:
  strict: true
  transitions:
    open: [in_progress]
";
        let raw: serde_yml::Value = serde_yml::from_str(yaml).unwrap();
        assert!(detect_unknown_policy_fields(&raw).is_empty());
    }

    // =========================================================================
    // Workflow gate engine (issue #312, layer 2 / beads_rust#319)
    // =========================================================================

    /// Parse the full gate schema from the issue spec and assert it round-trips
    /// into the typed structs (named gates, min_reviewers, conditional gates by
    /// label and by priority).
    #[test]
    fn gates_config_parses_full_issue_schema() {
        let yaml = r#"
workflow:
  strict: true
  gates:
    "in_review -> closed":
      require_all:
        - ci_green
        - min_reviewers: 2
      require_if:
        - label: security-sensitive
          gate: security_sign_off
        - priority: [0, 1]
          gate: security_sign_off
"#;
        let doc: PolicyDocument = serde_yml::from_str(yaml).expect("parse gates");
        assert!(doc.workflow.gates_enforced());
        let rule = doc
            .workflow
            .gate_rule_for("in_review", "closed")
            .expect("rule present");
        assert_eq!(rule.require_all.len(), 2);
        assert_eq!(rule.require_all[0], GateSpec::Named("ci_green".to_string()));
        assert_eq!(rule.require_all[1], GateSpec::MinReviewers(2));
        assert_eq!(rule.require_if.len(), 2);
        assert_eq!(
            rule.require_if[0].label.as_deref(),
            Some("security-sensitive")
        );
        assert_eq!(rule.require_if[1].priority, vec![0, 1]);
        assert_eq!(
            rule.require_if[1].gate,
            GateSpec::Named("security_sign_off".to_string())
        );
    }

    #[test]
    fn gates_disabled_when_strict_false_or_empty() {
        // strict false → not enforced even with a gate rule
        let yaml =
            "workflow:\n  strict: false\n  gates:\n    \"a -> b\":\n      require_all: [ci]\n";
        let doc: PolicyDocument = serde_yml::from_str(yaml).unwrap();
        assert!(!doc.workflow.gates_enforced());
        // strict true but no gates → not enforced (backward compatible)
        let doc2: PolicyDocument = serde_yml::from_str("workflow:\n  strict: true\n").unwrap();
        assert!(!doc2.workflow.gates_enforced());
    }

    #[test]
    fn gates_key_parsing_is_whitespace_and_case_insensitive() {
        let yaml = "workflow:\n  strict: true\n  gates:\n    \"In_Review->Closed\":\n      require_all: [ci_green]\n";
        let doc: PolicyDocument = serde_yml::from_str(yaml).unwrap();
        assert!(doc.workflow.gate_rule_for("in_review", "closed").is_some());
        assert!(doc.workflow.gate_rule_for("IN_REVIEW", "CLOSED").is_some());
        assert!(doc.workflow.gate_rule_for("open", "closed").is_none());
    }

    fn gate_workflow() -> Workflow {
        let yaml = r#"
strict: true
gates:
  "in_review -> closed":
    require_all:
      - ci_green
      - min_reviewers: 1
    require_if:
      - label: security-sensitive
        gate: security_sign_off
      - priority: [0]
        gate: p0_sign_off
"#;
        serde_yml::from_str(yaml).expect("parse gate workflow")
    }

    #[test]
    fn required_gates_includes_unconditional_only_when_no_condition_matches() {
        let workflow = gate_workflow();
        let required = workflow.required_gates_for("in_review", "closed", &[], 3);
        let ids: Vec<&str> = required.iter().map(GateSpec::id).collect();
        assert_eq!(ids, vec!["ci_green", "min_reviewers"]);
    }

    #[test]
    fn required_gates_adds_conditional_on_label_match() {
        let workflow = gate_workflow();
        let labels = vec!["security-sensitive".to_string()];
        let required = workflow.required_gates_for("in_review", "closed", &labels, 3);
        assert!(
            required
                .iter()
                .map(GateSpec::id)
                .any(|id| id == "security_sign_off")
        );
    }

    #[test]
    fn required_gates_adds_conditional_on_priority_match() {
        let workflow = gate_workflow();
        let required = workflow.required_gates_for("in_review", "closed", &[], 0);
        assert!(
            required
                .iter()
                .map(GateSpec::id)
                .any(|id| id == "p0_sign_off")
        );
        // Non-P0 does not pick it up.
        let required_p3 = workflow.required_gates_for("in_review", "closed", &[], 3);
        assert!(
            !required_p3
                .iter()
                .map(GateSpec::id)
                .any(|id| id == "p0_sign_off")
        );
    }

    #[test]
    fn evaluate_gates_blocks_when_required_named_gate_missing() {
        let workflow = gate_workflow();
        let violations = evaluate_gates(&workflow, "bd-1", "in_review", "closed", &[], 3, &[]);
        // ci_green (missing) + min_reviewers (0 reviewers) both fire.
        let gate_ids: Vec<&str> = violations.iter().map(|v| v.gate.as_str()).collect();
        assert!(gate_ids.contains(&"gate_ci_green"));
        assert!(gate_ids.contains(&"gate_min_reviewers"));
    }

    #[test]
    fn evaluate_gates_passes_when_named_gate_has_a_pass() {
        let workflow = gate_workflow();
        let results = vec![
            GateResult {
                gate: "ci_green".to_string(),
                provider: "ci".to_string(),
                passed: true,
                note: None,
            },
            GateResult {
                gate: "min_reviewers".to_string(),
                provider: "reviewer:alice".to_string(),
                passed: true,
                note: None,
            },
        ];
        let violations = evaluate_gates(&workflow, "bd-1", "in_review", "closed", &[], 3, &results);
        assert!(violations.is_empty(), "{violations:?}");
    }

    #[test]
    fn evaluate_gates_named_gate_fail_does_not_satisfy() {
        let workflow = gate_workflow();
        let results = vec![GateResult {
            gate: "ci_green".to_string(),
            provider: "ci".to_string(),
            passed: false,
            note: None,
        }];
        let violations = evaluate_gates(&workflow, "bd-1", "in_review", "closed", &[], 3, &results);
        assert!(violations.iter().any(|v| v.gate == "gate_ci_green"));
        // The message should name the failing provider for actionability.
        let ci_violation = violations
            .iter()
            .find(|v| v.gate == "gate_ci_green")
            .unwrap();
        assert!(
            ci_violation.message.contains("ci"),
            "{}",
            ci_violation.message
        );
    }

    #[test]
    fn min_reviewers_counts_distinct_reviewer_providers() {
        let yaml =
            "strict: true\ngates:\n  \"a -> b\":\n    require_all:\n      - min_reviewers: 2\n";
        let workflow: Workflow = serde_yml::from_str(yaml).unwrap();
        // One distinct reviewer → still short.
        let one = vec![GateResult {
            gate: "min_reviewers".to_string(),
            provider: "reviewer:alice".to_string(),
            passed: true,
            note: None,
        }];
        assert_eq!(
            evaluate_gates(&workflow, "bd-1", "a", "b", &[], 0, &one).len(),
            1
        );
        // Two distinct reviewers → satisfied.
        let two = vec![
            GateResult {
                gate: "min_reviewers".to_string(),
                provider: "reviewer:alice".to_string(),
                passed: true,
                note: None,
            },
            GateResult {
                gate: "min_reviewers".to_string(),
                provider: "reviewer:bob".to_string(),
                passed: true,
                note: None,
            },
        ];
        assert!(evaluate_gates(&workflow, "bd-1", "a", "b", &[], 0, &two).is_empty());
    }

    #[test]
    fn min_reviewers_dedupes_same_reviewer() {
        let yaml =
            "strict: true\ngates:\n  \"a -> b\":\n    require_all:\n      - min_reviewers: 2\n";
        let workflow: Workflow = serde_yml::from_str(yaml).unwrap();
        // Same reviewer reporting twice counts once.
        let dupes = vec![
            GateResult {
                gate: "min_reviewers".to_string(),
                provider: "reviewer:alice".to_string(),
                passed: true,
                note: Some("first".to_string()),
            },
            GateResult {
                gate: "min_reviewers".to_string(),
                provider: "reviewer:alice".to_string(),
                passed: true,
                note: Some("again".to_string()),
            },
        ];
        assert_eq!(
            evaluate_gates(&workflow, "bd-1", "a", "b", &[], 0, &dupes).len(),
            1
        );
    }

    #[test]
    fn is_reviewer_provider_recognises_namespaced_forms() {
        assert!(is_reviewer_provider("reviewer"));
        assert!(is_reviewer_provider("reviewer:alice"));
        assert!(is_reviewer_provider("reviewer-bob"));
        assert!(is_reviewer_provider("Reviewer:Carol"));
        assert!(!is_reviewer_provider("ci"));
        assert!(!is_reviewer_provider("reviewers")); // no separator
    }

    #[test]
    fn evaluate_gates_no_rule_is_backward_compatible() {
        let workflow = gate_workflow();
        // No rule guards open -> in_progress → no violations regardless of state.
        let violations = evaluate_gates(&workflow, "bd-1", "open", "in_progress", &[], 0, &[]);
        assert!(violations.is_empty());
        // A default (no gates) workflow never produces violations.
        let empty = Workflow::default();
        assert!(evaluate_gates(&empty, "bd-1", "in_review", "closed", &[], 0, &[]).is_empty());
    }

    #[test]
    fn unknown_gate_spec_key_is_rejected_at_parse() {
        let yaml = "strict: true\ngates:\n  \"a -> b\":\n    require_all:\n      - bogus_key: 3\n";
        let parsed: std::result::Result<Workflow, _> = serde_yml::from_str(yaml);
        assert!(parsed.is_err(), "unknown gate spec key must fail parse");
    }

    #[test]
    fn gates_key_recognised_by_unknown_field_detector() {
        let yaml = r#"
workflow:
  strict: true
  gates:
    "in_review -> closed":
      require_all: [ci_green]
"#;
        let raw: serde_yml::Value = serde_yml::from_str(yaml).unwrap();
        assert!(detect_unknown_policy_fields(&raw).is_empty());
    }
}
