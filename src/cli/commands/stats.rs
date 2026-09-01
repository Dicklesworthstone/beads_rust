//! Stats command implementation.
//!
//! Shows project statistics including issue counts by status, type, priority,
//! assignee, and label. Also supports recent activity tracking via git.

use super::auto_import_external_projects_if_stale;
use crate::cli::{OutputFormat, StatsArgs, resolve_output_format_basic_with_outer_mode};
use crate::config;
use crate::error::Result;
use crate::format::{
    Breakdown, BreakdownEntry, RecentActivity, Statistics, StatsSummary, sanitize_terminal_inline,
    truncate_title,
};
use crate::model::{Issue, IssueType, Status};
use crate::output::{OutputContext, OutputMode};
use crate::storage::{SqliteStorage, StatsIssueRow};
use chrono::Utc;
use rich_rust::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use tracing::{debug, info};

/// Execute the stats command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or queries fail.
pub fn execute(
    args: &StatsArgs,
    _json: bool,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    execute_inner(args, cli, outer_ctx, &beads_dir, None, None)
}

/// Execute stats using storage that was already opened by the caller.
///
/// # Errors
///
/// Returns an error if queries fail.
pub fn execute_with_storage(
    args: &StatsArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    storage: &SqliteStorage,
) -> Result<()> {
    execute_inner(args, cli, outer_ctx, beads_dir, Some(storage), None)
}

/// Execute stats using the caller's preopened storage context.
///
/// # Errors
///
/// Returns an error if queries fail.
pub fn execute_with_storage_ctx(
    args: &StatsArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    execute_inner(args, cli, outer_ctx, beads_dir, None, Some(storage_ctx))
}

fn execute_inner(
    args: &StatsArgs,
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
        .expect("stats should have an open storage handle");
    let jsonl_path = preloaded_storage_ctx
        .or(owned_storage_ctx.as_ref())
        .map_or_else(
            || beads_dir.join("issues.jsonl"),
            |ctx| ctx.paths.jsonl_path.clone(),
        );
    let output_format = resolve_output_format_basic_with_outer_mode(
        args.format,
        outer_ctx.inherited_output_mode(),
        args.robot,
    );
    let quiet = cli.quiet.unwrap_or(false);
    let early_ctx = OutputContext::from_output_format(output_format, quiet, true);
    let storage_ctx_for_config = preloaded_storage_ctx.or(owned_storage_ctx.as_ref());
    let mut config_layer: Option<config::ConfigLayer> = None;

    info!("Computing project statistics");

    let now = Utc::now();
    let all_issues = list_issues_for_stats(storage, args)?;
    debug!(total = all_issues.len(), "Loaded issues for stats");
    let has_potential_ready_candidates = all_issues
        .iter()
        .any(|issue| is_potential_ready_candidate(issue, &now));
    let external_blockers = resolve_stats_external_blockers(
        storage,
        beads_dir,
        storage_ctx_for_config,
        cli,
        &mut config_layer,
        has_potential_ready_candidates,
    )?;

    let summary = compute_summary(
        storage,
        &all_issues,
        external_blockers.as_ref(),
        &now,
        has_potential_ready_candidates,
    )?;

    // Compute breakdowns if requested
    let mut breakdowns = Vec::new();

    if args.by_type {
        breakdowns.push(compute_type_breakdown(&all_issues));
    }
    if args.by_priority {
        breakdowns.push(compute_priority_breakdown(&all_issues));
    }
    if args.by_assignee {
        breakdowns.push(compute_assignee_breakdown(&all_issues));
    }
    if args.by_label {
        breakdowns.push(compute_label_breakdown(storage, &all_issues)?);
    }

    let recent_activity = if should_collect_activity(args, early_ctx.mode()) {
        compute_recent_activity(Some(storage), &jsonl_path, args.activity_hours)
    } else {
        None
    };

    let capacity = capacity_stats(storage)?;

    let output = Statistics {
        summary,
        breakdowns,
        recent_activity,
        capacity,
    };

    if matches!(early_ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    let ctx = stats_output_context(
        output_format,
        quiet,
        beads_dir,
        storage,
        storage_ctx_for_config,
        cli,
        &mut config_layer,
    )?;

    // Output based on mode
    match output_format {
        OutputFormat::Json => {
            ctx.json_pretty(&output);
        }
        OutputFormat::Toon => {
            ctx.toon_with_stats(&output, args.stats);
        }
        OutputFormat::Text | OutputFormat::Csv => {
            if matches!(ctx.mode(), OutputMode::Rich) {
                render_stats_rich(&output, &ctx);
            } else {
                print_text_output(&output);
            }
        }
    }

    Ok(())
}

fn resolve_stats_external_blockers(
    storage: &SqliteStorage,
    beads_dir: &Path,
    storage_ctx: Option<&config::OpenStorageResult>,
    cli: &config::CliOverrides,
    config_layer: &mut Option<config::ConfigLayer>,
    has_potential_ready_candidates: bool,
) -> Result<Option<HashMap<String, Vec<String>>>> {
    if !has_potential_ready_candidates || !storage.has_external_dependencies(true)? {
        return Ok(None);
    }

    let config_layer =
        ensure_stats_config_layer(beads_dir, storage, storage_ctx, cli, config_layer)?;
    auto_import_external_projects_if_stale(config_layer, beads_dir, cli);
    let external_db_paths = config::external_project_db_paths(config_layer, beads_dir);
    let external_statuses =
        storage.resolve_external_dependency_statuses(&external_db_paths, true)?;
    Ok(Some(storage.external_blockers(&external_statuses)?))
}

fn stats_output_context(
    output_format: OutputFormat,
    quiet: bool,
    beads_dir: &Path,
    storage: &SqliteStorage,
    storage_ctx: Option<&config::OpenStorageResult>,
    cli: &config::CliOverrides,
    config_layer: &mut Option<config::ConfigLayer>,
) -> Result<OutputContext> {
    let use_color = if matches!(output_format, OutputFormat::Text | OutputFormat::Csv) {
        config::should_use_color(ensure_stats_config_layer(
            beads_dir,
            storage,
            storage_ctx,
            cli,
            config_layer,
        )?)
    } else {
        false
    };
    Ok(OutputContext::from_output_format(
        output_format,
        quiet,
        !use_color,
    ))
}

fn ensure_stats_config_layer<'a>(
    beads_dir: &Path,
    storage: &SqliteStorage,
    storage_ctx: Option<&config::OpenStorageResult>,
    cli: &config::CliOverrides,
    config_layer: &'a mut Option<config::ConfigLayer>,
) -> Result<&'a config::ConfigLayer> {
    if config_layer.is_none() {
        *config_layer = Some(load_stats_config_layer(
            beads_dir,
            storage,
            storage_ctx,
            cli,
        )?);
    }
    Ok(config_layer
        .as_ref()
        .expect("stats config layer loaded before use"))
}

fn load_stats_config_layer(
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

const fn should_include_activity(args: &StatsArgs) -> bool {
    !args.no_activity
}

const fn should_collect_activity(args: &StatsArgs, output_mode: OutputMode) -> bool {
    should_include_activity(args) && !matches!(output_mode, OutputMode::Quiet)
}

const fn needs_stats_issue_rows(args: &StatsArgs) -> bool {
    args.by_type || args.by_priority || args.by_assignee || args.by_label
}

fn list_issues_for_stats(storage: &SqliteStorage, args: &StatsArgs) -> Result<Vec<StatsIssueRow>> {
    if needs_stats_issue_rows(args) {
        storage.list_stats_issues()
    } else {
        storage.list_stats_summary_issues()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ActivityCounts {
    created: usize,
    closed: usize,
    updated: usize,
    reopened: usize,
}

impl ActivityCounts {
    const fn total_changes(self) -> usize {
        self.created + self.closed + self.updated + self.reopened
    }

    fn merge(&mut self, other: Self) {
        self.created += other.created;
        self.closed += other.closed;
        self.updated += other.updated;
        self.reopened += other.reopened;
    }

    fn record_transition(&mut self, previous: Option<&Issue>, current: Option<&Issue>) {
        match (previous, current) {
            (None, Some(issue)) => {
                if issue.status != Status::Tombstone {
                    self.created += 1;
                }
            }
            (Some(before), Some(after)) => {
                if !matches!(before.status, Status::Closed | Status::Tombstone)
                    && after.status == Status::Closed
                {
                    self.closed += 1;
                    return;
                }

                if before.status == Status::Closed
                    && !matches!(after.status, Status::Closed | Status::Tombstone)
                {
                    self.reopened += 1;
                    return;
                }

                if !before.sync_equals(after) {
                    self.updated += 1;
                }
            }
            (_, None) => {}
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct GitActivitySummary {
    commit_count: usize,
    counts: ActivityCounts,
    earliest_commit_ts: Option<i64>,
}

const GIT_ACTIVITY_COMMIT_MARKER: &str = "__BR_ACTIVITY_COMMIT__:";
const RECENT_ACTIVITY_CACHE_KEY_PREFIX: &str = "stats_recent_activity";

#[derive(Debug, Clone)]
struct GitRepoContext {
    repo_root: PathBuf,
    head: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecentActivityCacheEntry {
    repo_root: String,
    repo_head: String,
    pathspec: String,
    hours: u32,
    valid_until_epoch: Option<i64>,
    activity: RecentActivity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecentActivityCachePolicy {
    Cache { valid_until_epoch: Option<i64> },
    Skip,
}

/// Compute summary statistics.
#[allow(clippy::cast_precision_loss)]
fn compute_summary(
    storage: &SqliteStorage,
    issues: &[StatsIssueRow],
    external_blockers: Option<&std::collections::HashMap<String, Vec<String>>>,
    now: &chrono::DateTime<Utc>,
    has_potential_ready_candidates: bool,
) -> Result<StatsSummary> {
    let mut open = 0;
    let mut in_progress = 0;
    let mut closed = 0;
    let mut status_blocked_ids = HashSet::new();
    let mut deferred = 0;
    let mut draft = 0;
    let mut tombstone = 0;
    let mut pinned = 0;
    let mut epics = Vec::new();
    let mut lead_times = Vec::new();

    let active_issue_ids: HashSet<&str> = issues
        .iter()
        .filter(|i| !matches!(i.status, Status::Closed | Status::Tombstone))
        .map(|i| i.id.as_str())
        .collect();

    // Reuse the storage blocked-ID path for both blocked and ready counts.
    // It reads the materialized cache when healthy and falls back to direct
    // graph computation when needed; keep the active filter local so status
    // accounting remains anchored to the rows already loaded for stats.
    let dependency_blocked_ids: HashSet<String> = if active_issue_ids.is_empty() {
        HashSet::new()
    } else {
        storage
            .get_blocked_ids()?
            .into_iter()
            .filter(|issue_id| active_issue_ids.contains(issue_id.as_str()))
            .collect()
    };

    for issue in issues {
        match issue.status {
            Status::Open => open += 1,
            Status::InProgress => in_progress += 1,
            Status::Closed => {
                closed += 1;
                // Calculate lead time for closed issues
                if let Some(closed_at) = issue.closed_at {
                    let lead_time = closed_at.signed_duration_since(issue.created_at);
                    lead_times.push(lead_time.num_hours() as f64);
                }
            }
            Status::Blocked => {
                status_blocked_ids.insert(issue.id.as_str());
            }
            Status::Deferred => deferred += 1,
            Status::Draft => draft += 1,
            Status::Tombstone => tombstone += 1,
            Status::Pinned | Status::Custom(_) => {}
        }
        if issue.pinned || issue.status == Status::Pinned {
            pinned += 1;
        }

        // Track epics for eligible-for-closure calculation
        if issue.issue_type == IssueType::Epic
            && !matches!(issue.status, Status::Closed | Status::Tombstone)
            && !issue.is_template
        {
            epics.push(issue.id.clone());
        }
    }

    // Ready count: status=open (not in_progress), no blockers (full definition).
    let ready = if has_potential_ready_candidates {
        issues
            .iter()
            .filter(|i| {
                is_potential_ready_candidate(i, now)
                    && !dependency_blocked_ids.contains(&i.id)
                    && external_blockers.is_none_or(|eb| !eb.contains_key(&i.id))
            })
            .count()
    } else {
        0
    };

    // Blocked count includes both dependency-blocked issues and manual
    // Status::Blocked issues, deduped by ID when both conditions apply.
    status_blocked_ids.extend(dependency_blocked_ids.iter().map(String::as_str));
    let blocked = status_blocked_ids.len();

    // Epics eligible for closure: all children closed
    let epics_eligible = count_epics_eligible_for_closure(storage, &epics)?;

    // Average lead time
    let avg_lead_time = if lead_times.is_empty() {
        None
    } else {
        let sum: f64 = lead_times.iter().sum();
        Some(sum / lead_times.len() as f64)
    };

    // Total excludes tombstones
    let total = issues
        .iter()
        .filter(|i| i.status != Status::Tombstone)
        .count();

    Ok(StatsSummary {
        total_issues: total,
        open_issues: open,
        in_progress_issues: in_progress,
        closed_issues: closed,
        blocked_issues: blocked,
        deferred_issues: deferred,
        draft_issues: draft,
        ready_issues: ready,
        tombstone_issues: tombstone,
        pinned_issues: pinned,
        epics_eligible_for_closure: epics_eligible,
        average_lead_time_hours: avg_lead_time,
    })
}

fn is_wisp_issue_id(id: &str) -> bool {
    id.contains("-wisp-")
}

fn is_potential_ready_candidate(issue: &StatsIssueRow, now: &chrono::DateTime<Utc>) -> bool {
    issue.status == Status::Open
        && !issue.ephemeral
        && !is_wisp_issue_id(&issue.id)
        && !issue.pinned
        && !issue.is_template
        && issue
            .defer_until
            .as_ref()
            .is_none_or(|defer_until| defer_until <= now)
}

/// Count epics that have all children closed.
fn count_epics_eligible_for_closure(storage: &SqliteStorage, epic_ids: &[String]) -> Result<usize> {
    if epic_ids.is_empty() {
        return Ok(0);
    }

    let mut eligible = 0;
    let counts = storage.get_epic_counts()?;

    for epic_id in epic_ids {
        if let Some(&(total, closed)) = counts.get(epic_id)
            && total > 0
            && total == closed
        {
            eligible += 1;
        }
    }

    Ok(eligible)
}

/// Compute breakdown by issue type.
fn compute_type_breakdown(issues: &[StatsIssueRow]) -> Breakdown {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    for issue in issues {
        if issue.status == Status::Tombstone {
            continue;
        }
        let key = issue.issue_type.as_str().to_string();
        *counts.entry(key).or_insert(0) += 1;
    }

    Breakdown {
        dimension: "type".to_string(),
        counts: counts
            .into_iter()
            .map(|(key, count)| BreakdownEntry { key, count })
            .collect(),
    }
}

/// Compute breakdown by priority.
fn compute_priority_breakdown(issues: &[StatsIssueRow]) -> Breakdown {
    let mut counts: BTreeMap<i32, usize> = BTreeMap::new();

    for issue in issues {
        if issue.status == Status::Tombstone {
            continue;
        }
        *counts.entry(issue.priority.0).or_insert(0) += 1;
    }

    Breakdown {
        dimension: "priority".to_string(),
        counts: counts
            .into_iter()
            .map(|(p, count)| BreakdownEntry {
                key: format!("P{p}"),
                count,
            })
            .collect(),
    }
}

/// Compute breakdown by assignee.
fn compute_assignee_breakdown(issues: &[StatsIssueRow]) -> Breakdown {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    for issue in issues {
        if issue.status == Status::Tombstone {
            continue;
        }
        let key = issue
            .assignee
            .as_deref()
            .unwrap_or("(unassigned)")
            .to_string();
        *counts.entry(key).or_insert(0) += 1;
    }

    Breakdown {
        dimension: "assignee".to_string(),
        counts: counts
            .into_iter()
            .map(|(key, count)| BreakdownEntry { key, count })
            .collect(),
    }
}

/// Compute breakdown by label.
fn compute_label_breakdown(storage: &SqliteStorage, issues: &[StatsIssueRow]) -> Result<Breakdown> {
    let active_issue_ids: HashSet<&str> = issues
        .iter()
        .filter(|issue| issue.status != Status::Tombstone)
        .map(|issue| issue.id.as_str())
        .collect();
    let mut labeled_issue_ids: HashSet<String> = HashSet::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    for (issue_id, label) in storage.list_label_pairs_unordered()? {
        if active_issue_ids.contains(issue_id.as_str()) {
            *counts.entry(label).or_insert(0) += 1;
            labeled_issue_ids.insert(issue_id);
        }
    }

    let unlabeled_count = active_issue_ids
        .len()
        .saturating_sub(labeled_issue_ids.len());
    if unlabeled_count > 0 {
        counts.insert("(no labels)".to_string(), unlabeled_count);
    }

    Ok(Breakdown {
        dimension: "label".to_string(),
        counts: counts
            .into_iter()
            .map(|(key, count)| BreakdownEntry { key, count })
            .collect(),
    })
}

/// Compute recent activity from git log on the active JSONL file.
fn compute_recent_activity(
    storage: Option<&SqliteStorage>,
    jsonl_path: &Path,
    hours: u32,
) -> Option<RecentActivity> {
    if !jsonl_path.exists() {
        debug!("No issues.jsonl found for activity tracking");
        return None;
    }

    let repo_ctx = git_repo_context(jsonl_path.parent()?)?;
    let pathspec = repo_relative_git_path(jsonl_path, &repo_ctx.repo_root)?;
    let pathspec_str = git_pathspec_string(&pathspec);
    let cache_key = recent_activity_cache_key(&pathspec_str, hours);
    let now_epoch = Utc::now().timestamp();

    if let Some(storage) = storage
        && let Some(activity) = load_recent_activity_cache(
            storage,
            &cache_key,
            &repo_ctx,
            &pathspec_str,
            hours,
            now_epoch,
        )
    {
        debug!(hours, pathspec = %pathspec_str, "Using cached stats recent activity");
        return Some(activity);
    }

    let since = format!("{hours} hours ago");
    let GitActivitySummary {
        commit_count,
        counts,
        earliest_commit_ts,
    } = git_recent_activity(&repo_ctx.repo_root, &pathspec_str, &since)?;

    let activity = RecentActivity {
        hours_tracked: hours,
        commit_count,
        issues_created: counts.created,
        issues_closed: counts.closed,
        issues_updated: counts.updated,
        issues_reopened: counts.reopened,
        total_changes: counts.total_changes(),
    };

    if let Some(storage) = storage
        && let RecentActivityCachePolicy::Cache { valid_until_epoch } =
            recent_activity_cache_policy(commit_count, earliest_commit_ts, hours)
    {
        let entry = RecentActivityCacheEntry {
            repo_root: repo_ctx.repo_root.to_string_lossy().into_owned(),
            repo_head: repo_ctx.head,
            pathspec: pathspec_str,
            hours,
            valid_until_epoch,
            activity: activity.clone(),
        };
        store_recent_activity_cache(storage, &cache_key, &entry);
    }

    Some(activity)
}

fn git_recent_activity(
    repo_root: &Path,
    pathspec: &str,
    since: &str,
) -> Option<GitActivitySummary> {
    use std::io::{BufReader, Read};
    use std::process::Stdio;

    let mut child = git_command()
        .args([
            "log",
            "--format=__BR_ACTIVITY_COMMIT__:%H\t%ct",
            "--patch",
            "--unified=0",
            "--no-color",
            "--no-ext-diff",
            "--since",
            since,
            "--",
            pathspec,
        ])
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let stderr_handle = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut err_msg = String::new();
            let _ = stderr.read_to_string(&mut err_msg);
            err_msg
        })
    });

    let stdout = child.stdout.take()?;
    let reader = BufReader::new(stdout);

    let summary = parse_issue_activity_stream(reader);

    let status = child.wait().ok()?;
    let err_msg = stderr_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    if !status.success() {
        debug!(stderr = %err_msg.trim(), "Git log failed for activity diff");
        return None;
    }

    Some(summary)
}

fn parse_issue_activity_stream<R: std::io::BufRead>(reader: R) -> GitActivitySummary {
    let mut summary = GitActivitySummary::default();
    let mut removed = BTreeMap::new();
    let mut added = BTreeMap::new();
    let mut in_commit = false;

    for line_result in reader.lines() {
        let Ok(line) = line_result else { continue };

        if line.starts_with(GIT_ACTIVITY_COMMIT_MARKER) {
            if in_commit {
                summary
                    .counts
                    .merge(count_issue_activity_transitions(&removed, &added));
                removed.clear();
                added.clear();
            }

            in_commit = true;
            summary.commit_count += 1;
            if let Some(commit_ts) = parse_activity_commit_marker(&line) {
                summary.earliest_commit_ts = Some(
                    summary
                        .earliest_commit_ts
                        .map_or(commit_ts, |earliest| earliest.min(commit_ts)),
                );
            }
            continue;
        }

        record_issue_activity_patch_line(&line, &mut removed, &mut added);
    }

    if in_commit {
        summary
            .counts
            .merge(count_issue_activity_transitions(&removed, &added));
    }

    summary
}

#[cfg(test)]
fn parse_issue_activity_patch(patch: &str) -> ActivityCounts {
    let mut removed = BTreeMap::new();
    let mut added = BTreeMap::new();

    for line in patch.lines() {
        record_issue_activity_patch_line(line, &mut removed, &mut added);
    }

    count_issue_activity_transitions(&removed, &added)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IssuePatchMarker {
    Added,
    Removed,
}

fn record_issue_activity_patch_line(
    line: &str,
    removed: &mut BTreeMap<String, Issue>,
    added: &mut BTreeMap<String, Issue>,
) {
    let Some((marker, payload)) = parse_issue_patch_line(line) else {
        return;
    };

    match serde_json::from_str::<Issue>(payload) {
        Ok(issue) => match marker {
            IssuePatchMarker::Added => {
                added.insert(issue.id.clone(), issue);
            }
            IssuePatchMarker::Removed => {
                removed.insert(issue.id.clone(), issue);
            }
        },
        Err(err) => {
            debug!(%err, "Skipping unparsable issue line from git diff");
        }
    }
}

fn count_issue_activity_transitions(
    removed: &BTreeMap<String, Issue>,
    added: &BTreeMap<String, Issue>,
) -> ActivityCounts {
    let mut counts = ActivityCounts::default();
    let mut issue_ids: HashSet<&str> = removed.keys().map(String::as_str).collect();
    issue_ids.extend(added.keys().map(String::as_str));

    for issue_id in issue_ids {
        counts.record_transition(removed.get(issue_id), added.get(issue_id));
    }

    counts
}

fn parse_issue_patch_line(line: &str) -> Option<(IssuePatchMarker, &str)> {
    if line.starts_with("+++ ")
        || line.starts_with("--- ")
        || line.starts_with("@@")
        || line.starts_with("diff --git")
        || line.starts_with("index ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("\\ No newline at end of file")
    {
        return None;
    }

    let marker = *line.as_bytes().first()? as char;
    let marker = match marker {
        '+' => IssuePatchMarker::Added,
        '-' => IssuePatchMarker::Removed,
        _ => return None,
    };

    let payload = &line[1..];
    if !payload.starts_with('{') {
        return None;
    }

    Some((marker, payload))
}

fn parse_activity_commit_marker(line: &str) -> Option<i64> {
    let payload = line.strip_prefix(GIT_ACTIVITY_COMMIT_MARKER)?;
    let (_, ts) = payload.rsplit_once('\t')?;
    ts.parse::<i64>().ok()
}

fn git_repo_context(start: &Path) -> Option<GitRepoContext> {
    git_repo_context_from_filesystem(start).or_else(|| git_repo_context_from_command(start))
}

fn git_repo_context_from_filesystem(start: &Path) -> Option<GitRepoContext> {
    let mut repo_root = dunce::canonicalize(start).ok()?;

    loop {
        let git_entry = repo_root.join(".git");
        if git_entry.is_dir() {
            let head = read_git_head(&git_entry)?;
            return Some(GitRepoContext { repo_root, head });
        }

        if git_entry.is_file() {
            let git_dir = read_gitdir_file(&git_entry, &repo_root)?;
            let head = read_git_head(&git_dir)?;
            return Some(GitRepoContext { repo_root, head });
        }

        if !repo_root.pop() {
            return None;
        }
    }
}

fn read_gitdir_file(git_file: &Path, repo_root: &Path) -> Option<PathBuf> {
    let raw = fs::read_to_string(git_file).ok()?;
    let gitdir = raw.trim().strip_prefix("gitdir:")?.trim();
    if gitdir.is_empty() {
        return None;
    }

    let path = PathBuf::from(gitdir);
    let path = if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    };
    dunce::canonicalize(path).ok()
}

fn read_git_head(git_dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = raw.trim();
    let Some(reference) = head.strip_prefix("ref:") else {
        return non_empty_string(head);
    };
    let reference = reference.trim();
    if reference.is_empty() {
        return None;
    }

    let common_dir = git_common_dir(git_dir);
    read_git_ref(git_dir, reference)
        .or_else(|| read_git_ref(&common_dir, reference))
        .or_else(|| read_packed_git_ref(&common_dir, reference))
}

fn git_common_dir(git_dir: &Path) -> PathBuf {
    let Ok(raw) = fs::read_to_string(git_dir.join("commondir")) else {
        return git_dir.to_path_buf();
    };
    let common_dir = raw.trim();
    if common_dir.is_empty() {
        return git_dir.to_path_buf();
    }

    let path = PathBuf::from(common_dir);
    if path.is_absolute() {
        path
    } else {
        git_dir.join(path)
    }
}

fn read_git_ref(git_dir: &Path, reference: &str) -> Option<String> {
    let ref_path = safe_git_ref_path(git_dir, reference)?;
    let raw = fs::read_to_string(ref_path).ok()?;
    non_empty_string(raw.trim())
}

fn read_packed_git_ref(git_dir: &Path, reference: &str) -> Option<String> {
    let raw = fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }

        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else { continue };
        if parts.next() == Some(reference) {
            return non_empty_string(hash);
        }
    }
    None
}

fn safe_git_ref_path(git_dir: &Path, reference: &str) -> Option<PathBuf> {
    let ref_path = Path::new(reference);
    if ref_path.is_absolute()
        || ref_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(git_dir.join(ref_path))
}

fn non_empty_string(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn git_repo_context_from_command(start: &Path) -> Option<GitRepoContext> {
    let output = git_command()
        .args(["rev-parse", "--show-toplevel", "HEAD"])
        .current_dir(start)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let repo_root = lines.next()?.trim();
    let head = lines.next()?.trim();
    if repo_root.is_empty() || head.is_empty() {
        return None;
    }

    Some(GitRepoContext {
        repo_root: PathBuf::from(repo_root),
        head: head.to_string(),
    })
}

fn git_command() -> Command {
    Command::new(git_executable())
}

fn git_executable() -> &'static Path {
    static GIT_EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
    GIT_EXECUTABLE.get_or_init(resolve_git_executable)
}

fn resolve_git_executable() -> PathBuf {
    let binary_name = if cfg!(windows) { "git.exe" } else { "git" };
    let Some(path_var) = std::env::var_os("PATH") else {
        return PathBuf::from(binary_name);
    };

    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() || !dir.is_absolute() {
            continue;
        }

        let candidate = dir.join(binary_name);
        if is_executable_file(&candidate) {
            return candidate;
        }
    }

    PathBuf::from(binary_name)
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn repo_relative_git_path(path: &Path, repo_root: &Path) -> Option<PathBuf> {
    let canonical_repo_root = dunce::canonicalize(repo_root).ok()?;
    let canonical_path = dunce::canonicalize(path).ok()?;
    canonical_path
        .strip_prefix(&canonical_repo_root)
        .ok()
        .map(Path::to_path_buf)
}

fn git_pathspec_string(path: &Path) -> String {
    let pathspec = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        pathspec.replace('\\', "/")
    } else {
        pathspec
    }
}

fn recent_activity_cache_key(pathspec: &str, hours: u32) -> String {
    format!("{RECENT_ACTIVITY_CACHE_KEY_PREFIX}:{hours}:{pathspec}")
}

fn load_recent_activity_cache(
    storage: &SqliteStorage,
    cache_key: &str,
    repo_ctx: &GitRepoContext,
    pathspec: &str,
    hours: u32,
    now_epoch: i64,
) -> Option<RecentActivity> {
    let raw = storage.get_metadata(cache_key).ok()??;
    parse_recent_activity_cache(&raw, repo_ctx, pathspec, hours, now_epoch)
}

fn parse_recent_activity_cache(
    raw: &str,
    repo_ctx: &GitRepoContext,
    pathspec: &str,
    hours: u32,
    now_epoch: i64,
) -> Option<RecentActivity> {
    let entry = serde_json::from_str::<RecentActivityCacheEntry>(raw)
        .map_err(|error| {
            debug!(%error, "Ignoring invalid recent activity cache entry");
            error
        })
        .ok()?;

    if entry.repo_root != repo_ctx.repo_root.to_string_lossy()
        || entry.pathspec != pathspec
        || entry.hours != hours
        || entry
            .valid_until_epoch
            .is_some_and(|valid_until| now_epoch > valid_until)
    {
        return None;
    }

    if entry.repo_head == repo_ctx.head
        || zero_activity_cache_covers_current_head(&entry, repo_ctx, pathspec)
    {
        Some(entry.activity)
    } else {
        None
    }
}

fn zero_activity_cache_covers_current_head(
    entry: &RecentActivityCacheEntry,
    repo_ctx: &GitRepoContext,
    pathspec: &str,
) -> bool {
    if entry.valid_until_epoch.is_some()
        || entry.activity.commit_count != 0
        || entry.activity.total_changes != 0
        || !looks_like_git_oid(&entry.repo_head)
        || !looks_like_git_oid(&repo_ctx.head)
    {
        return false;
    }

    let range = format!("{}..{}", entry.repo_head, repo_ctx.head);
    let Ok(status) = git_command()
        .args(["diff", "--quiet", &range, "--", pathspec])
        .current_dir(&repo_ctx.repo_root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    else {
        return false;
    };

    if status.success() {
        debug!(
            cached_head = %entry.repo_head,
            current_head = %repo_ctx.head,
            pathspec,
            "Reusing zero-activity stats cache across a code-only HEAD change"
        );
        true
    } else {
        false
    }
}

fn looks_like_git_oid(value: &str) -> bool {
    (4..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn recent_activity_cache_policy(
    commit_count: usize,
    earliest_commit_ts: Option<i64>,
    hours: u32,
) -> RecentActivityCachePolicy {
    if commit_count == 0 {
        return RecentActivityCachePolicy::Cache {
            valid_until_epoch: None,
        };
    }

    earliest_commit_ts.map_or(RecentActivityCachePolicy::Skip, |ts| {
        RecentActivityCachePolicy::Cache {
            valid_until_epoch: Some(ts.saturating_add(i64::from(hours) * 3600)),
        }
    })
}

fn store_recent_activity_cache(
    storage: &SqliteStorage,
    cache_key: &str,
    entry: &RecentActivityCacheEntry,
) {
    let Ok(serialized) = serde_json::to_string(entry).map_err(|error| {
        debug!(%error, cache_key, "Failed to serialize recent activity cache entry");
        error
    }) else {
        return;
    };

    if let Err(error) = storage.set_metadata_shared(cache_key, &serialized) {
        debug!(%error, cache_key, "Failed to persist recent activity cache entry");
    }
}

/// Print text output for stats.
fn print_text_output(output: &Statistics) {
    // Match bd format: 📊 Issue Database Status
    println!("📊 Issue Database Status\n");

    let s = &output.summary;
    println!("Summary:");
    // Match bd alignment (right-aligned numbers, 18-char label width)
    println!("  Total Issues:           {}", s.total_issues);
    println!("  Open:                   {}", s.open_issues);
    println!("  In Progress:            {}", s.in_progress_issues);
    println!("  Blocked:                {}", s.blocked_issues);
    println!("  Closed:                 {}", s.closed_issues);
    println!("  Ready to Work:          {}", s.ready_issues);

    // Optional fields (only show if non-zero)
    if s.deferred_issues > 0 {
        println!("  Deferred:               {}", s.deferred_issues);
    }
    if s.tombstone_issues > 0 {
        println!("  Tombstones:             {}", s.tombstone_issues);
    }
    if s.pinned_issues > 0 {
        println!("  Pinned:                 {}", s.pinned_issues);
    }
    if s.epics_eligible_for_closure > 0 {
        println!("  Epics ready to close:   {}", s.epics_eligible_for_closure);
    }

    // Extended section (matches bd format)
    if s.average_lead_time_hours.is_some() || s.tombstone_issues > 0 {
        println!("\nExtended:");
        if let Some(avg_hours) = s.average_lead_time_hours {
            // Format like bd: "N.N hours" or "N days" for large values
            let formatted = if avg_hours >= 24.0 {
                let avg_days = avg_hours / 24.0;
                format!("{avg_days:.1} days")
            } else {
                format!("{avg_hours:.1} hours")
            };
            println!("  Avg Lead Time:          {formatted}");
        }
        if s.tombstone_issues > 0 {
            println!(
                "  Deleted:                {} (tombstones)",
                s.tombstone_issues
            );
        }
    }

    for breakdown in &output.breakdowns {
        println!("\nBy {}:", breakdown.dimension);
        for entry in &breakdown.counts {
            println!(
                "  {}: {}",
                sanitize_terminal_inline(&entry.key),
                entry.count
            );
        }
    }

    if let Some(activity) = &output.recent_activity {
        println!("\nRecent Activity (last {} hours):", activity.hours_tracked);
        println!("  Commits:                {}", activity.commit_count);
        println!("  Total Changes:          {}", activity.total_changes);
        println!("  Issues Created:         {}", activity.issues_created);
        println!("  Issues Closed:          {}", activity.issues_closed);
        println!("  Issues Reopened:        {}", activity.issues_reopened);
        println!("  Issues Updated:         {}", activity.issues_updated);
    }

    print_capacity_table(&output.capacity);

    // Match bd footer
    println!("\nFor more details, use 'br list' to see individual issues.");
}

/// Capacity occupancy for the stats payload (GitHub #384 phase 6), absent
/// when no capacity is configured.
fn capacity_stats(storage: &SqliteStorage) -> Result<Vec<crate::format::CapacityStat>> {
    Ok(storage
        .capacity_snapshot()?
        .into_iter()
        .map(crate::format::CapacityStat::from_snapshot)
        .collect())
}

/// Print the GitHub #384 capacity occupancy table. Silent when no capacity
/// is configured, keeping the pre-capacity stats text byte-stable.
fn print_capacity_table(capacity: &[crate::format::CapacityStat]) {
    if capacity.is_empty() {
        return;
    }
    println!("\nCapacity:");
    println!(
        "  {:<24} {:>7} {:>10} {:>6} {:>5} {:>5} {:>9}  STATE",
        "CAPACITY", "COUNTED", "AGGREGATES", "EXEMPT", "SOFT", "HARD", "REMAINING"
    );
    for stat in capacity {
        let label = stat.scope_key.as_ref().map_or_else(
            || {
                if stat.scope == "repository" {
                    stat.name.clone()
                } else {
                    format!("{} [{}]", stat.name, stat.scope)
                }
            },
            |key| format!("{} [{}:{}]", stat.name, stat.scope, key),
        );
        let opt = |value: Option<u32>| value.map_or_else(|| "-".to_string(), |v| v.to_string());
        println!(
            "  {:<24} {:>7} {:>10} {:>6} {:>5} {:>5} {:>9}  {}",
            sanitize_terminal_inline(&label),
            stat.counted,
            opt(stat.aggregate_parents_excluded),
            opt(stat.exempt),
            opt(stat.soft_limit),
            opt(stat.hard_limit),
            opt(stat.remaining),
            stat.state,
        );
    }
}

/// Render stats with rich formatting.
#[allow(clippy::cast_precision_loss)]
fn render_stats_rich(output: &Statistics, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    // Build content as Text with multiple sections
    let mut content = Text::new("");

    // === Overview Section ===
    content.append_styled("\u{1f4ca} Overview\n", theme.section.clone());

    let s = &output.summary;

    // Main stats row
    content.append_styled("   Total: ", theme.dimmed.clone());
    content.append_styled(&s.total_issues.to_string(), theme.emphasis.clone());

    content.append_styled("    Ready: ", theme.dimmed.clone());
    content.append_styled(&s.ready_issues.to_string(), theme.success.clone());
    content.append_styled(" \u{2713}", theme.success.clone());

    content.append_styled("    Blocked: ", theme.dimmed.clone());
    content.append_styled(&s.blocked_issues.to_string(), theme.warning.clone());
    if s.blocked_issues > 0 {
        content.append_styled(" \u{26a0}", theme.warning.clone());
    }
    content.append("\n\n");

    // === Status Breakdown ===
    content.append_styled("\u{1f4c8} By Status\n", theme.section.clone());
    render_status_bars(&mut content, s, theme);
    content.append("\n");

    // === Optional Breakdowns ===
    for breakdown in &output.breakdowns {
        content.append_styled(
            &format!("\u{1f4c8} By {}\n", capitalize(&breakdown.dimension)),
            theme.section.clone(),
        );
        render_breakdown_bars(&mut content, breakdown, s.total_issues, theme);
        content.append("\n");
    }

    // === Recent Activity ===
    if let Some(activity) = &output.recent_activity {
        content.append_styled(
            &format!(
                "\u{1f4c5} Activity (last {} hours)\n",
                activity.hours_tracked
            ),
            theme.section.clone(),
        );
        content.append_styled("   Commits: ", theme.dimmed.clone());
        content.append(&activity.commit_count.to_string());
        if activity.total_changes > 0 {
            content.append_styled("    Changes: ", theme.dimmed.clone());
            content.append(&activity.total_changes.to_string());
        }
        content.append("\n\n");
    }

    append_capacity_section_rich(&mut content, &output.capacity, theme);

    // === Health Warnings ===
    let mut warnings = Vec::new();
    if s.blocked_issues > 5 {
        warnings.push(format!("{} issues blocked", s.blocked_issues));
    }
    if s.epics_eligible_for_closure > 0 {
        warnings.push(format!(
            "{} epic{} ready to close",
            s.epics_eligible_for_closure,
            if s.epics_eligible_for_closure == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    if s.deferred_issues > 10 {
        warnings.push(format!("{} issues deferred", s.deferred_issues));
    }

    if !warnings.is_empty() {
        content.append_styled("\u{26a0} Health Warnings\n", theme.warning.clone());
        for warning in &warnings {
            content.append_styled("   \u{2022} ", theme.warning.clone());
            content.append(warning);
            content.append("\n");
        }
    }

    // Wrap in panel
    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Project Health", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Append the GitHub #384 capacity section to the rich stats panel.
/// Silent when no capacity is configured so pre-capacity panel goldens
/// stay byte-stable.
fn append_capacity_section_rich(
    content: &mut Text,
    capacity: &[crate::format::CapacityStat],
    theme: &crate::output::Theme,
) {
    if capacity.is_empty() {
        return;
    }
    content.append_styled("\u{1f6a6} Capacity\n", theme.section.clone());
    for stat in capacity {
        let label = stat.scope_key.as_ref().map_or_else(
            || {
                if stat.scope == "repository" {
                    stat.name.clone()
                } else {
                    format!("{} [{}]", stat.name, stat.scope)
                }
            },
            |key| format!("{} [{}:{}]", stat.name, stat.scope, key),
        );
        content.append_styled("   ", theme.dimmed.clone());
        content.append(&sanitize_terminal_inline(&label));
        content.append_styled(": ", theme.dimmed.clone());
        content.append(&stat.counted.to_string());
        if let Some(hard) = stat.hard_limit {
            content.append(&format!("/{hard}"));
        }
        if let Some(exempt) = stat.exempt {
            content.append_styled(&format!(" (+{exempt} exempt)"), theme.dimmed.clone());
        }
        let state_style = match stat.state.as_str() {
            "healthy" => theme.success.clone(),
            _ => theme.warning.clone(),
        };
        content.append_styled(&format!("  {}", stat.state), state_style);
        content.append("\n");
    }
    content.append("\n");
}

/// Render status distribution as progress bars.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn render_status_bars(content: &mut Text, summary: &StatsSummary, theme: &crate::output::Theme) {
    let total = summary.total_issues.max(1);
    let bar_width: usize = 24;

    let statuses = [
        ("Open", summary.open_issues, &theme.status_open),
        (
            "In Progress",
            summary.in_progress_issues,
            &theme.status_in_progress,
        ),
        ("Blocked", summary.blocked_issues, &theme.status_blocked),
        ("Closed", summary.closed_issues, &theme.status_closed),
    ];

    for (label, count, style) in statuses {
        if count == 0 {
            continue;
        }
        let pct = (count as f64 / total as f64) * 100.0;
        let filled = ((count as f64 / total as f64) * bar_width as f64).round() as usize;
        let empty = bar_width.saturating_sub(filled);

        content.append_styled(&format!("   {:<12}", label), style.clone());
        content.append_styled(&"\u{2588}".repeat(filled), style.clone());
        content.append_styled(&"\u{2591}".repeat(empty), theme.dimmed.clone());
        content.append_styled(
            &format!(" {:>3} ({:.0}%)", count, pct),
            theme.dimmed.clone(),
        );
        content.append("\n");
    }
}

/// Render a breakdown as progress bars.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn render_breakdown_bars(
    content: &mut Text,
    breakdown: &Breakdown,
    total: usize,
    theme: &crate::output::Theme,
) {
    let total = total.max(1);
    let bar_width: usize = 24;

    for entry in &breakdown.counts {
        let pct = (entry.count as f64 / total as f64) * 100.0;
        let filled = ((entry.count as f64 / total as f64) * bar_width as f64).round() as usize;
        let empty = bar_width.saturating_sub(filled);

        // Choose style based on key
        let style = match breakdown.dimension.as_str() {
            "priority" => match entry.key.as_str() {
                "P0" => theme.priority_critical.clone(),
                "P1" => theme.priority_high.clone(),
                "P2" => theme.priority_medium.clone(),
                "P3" => theme.priority_low.clone(),
                _ => theme.priority_backlog.clone(),
            },
            "type" => match entry.key.as_str() {
                "task" => theme.type_task.clone(),
                "bug" => theme.type_bug.clone(),
                "feature" => theme.type_feature.clone(),
                "epic" => theme.type_epic.clone(),
                "chore" => theme.type_chore.clone(),
                "docs" => theme.type_docs.clone(),
                "question" => theme.type_question.clone(),
                _ => theme.dimmed.clone(),
            },
            _ => theme.accent.clone(),
        };

        content.append_styled(
            &format!("   {:<12}", truncate_title(&entry.key, 12)),
            style.clone(),
        );
        content.append_styled(&"\u{2588}".repeat(filled), style.clone());
        content.append_styled(&"\u{2591}".repeat(empty), theme.dimmed.clone());
        content.append_styled(
            &format!(" {:>3} ({:.0}%)", entry.count, pct),
            theme.dimmed.clone(),
        );
        content.append("\n");
    }
}

/// Capitalize the first letter of a string.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;
    use chrono::{Duration, TimeZone, Utc};
    use std::fs;
    use tempfile::TempDir;

    fn make_issue(id: &str, status: Status, issue_type: IssueType) -> Issue {
        Issue {
            id: id.to_string(),
            title: format!("Issue {id}"),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status,
            priority: Priority::MEDIUM,
            issue_type,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: Utc::now(),
            created_by: None,
            updated_at: Utc::now(),
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            bypassed_policy: None,
            bypass_reason: None,
            policy_gates_fired: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
            content_hash: None,
        }
    }

    fn stats_row(issue: &Issue) -> StatsIssueRow {
        StatsIssueRow {
            id: issue.id.clone(),
            status: issue.status.clone(),
            priority: issue.priority,
            issue_type: issue.issue_type.clone(),
            assignee: issue.assignee.clone(),
            created_at: issue.created_at,
            closed_at: issue.closed_at,
            defer_until: issue.defer_until,
            ephemeral: issue.ephemeral,
            pinned: issue.pinned,
            is_template: issue.is_template,
        }
    }

    fn compute_test_summary(storage: &SqliteStorage, issues: &[StatsIssueRow]) -> StatsSummary {
        let now = Utc::now();
        let has_potential_ready_candidates = issues
            .iter()
            .any(|issue| is_potential_ready_candidate(issue, &now));
        compute_summary(storage, issues, None, &now, has_potential_ready_candidates).unwrap()
    }

    #[test]
    fn test_compute_type_breakdown() {
        let test_issues = [
            make_issue("t-1", Status::Open, IssueType::Task),
            make_issue("t-2", Status::Open, IssueType::Task),
            make_issue("t-3", Status::Open, IssueType::Bug),
            make_issue("t-4", Status::Tombstone, IssueType::Feature), // Excluded
        ]
        .iter()
        .map(stats_row)
        .collect::<Vec<_>>();

        let breakdown = compute_type_breakdown(&test_issues);
        assert_eq!(breakdown.dimension, "type");

        let mut map: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &breakdown.counts {
            map.insert(entry.key.clone(), entry.count);
        }

        assert_eq!(map.get("task"), Some(&2));
        assert_eq!(map.get("bug"), Some(&1));
        assert_eq!(map.get("feature"), None); // Tombstone excluded
    }

    #[test]
    fn test_compute_priority_breakdown() {
        let mut test_issues = [
            make_issue("t-1", Status::Open, IssueType::Task),
            make_issue("t-2", Status::Open, IssueType::Task),
            make_issue("t-3", Status::Open, IssueType::Bug),
        ];
        test_issues[0].priority = Priority::CRITICAL;
        test_issues[1].priority = Priority::CRITICAL;
        test_issues[2].priority = Priority::LOW;
        let test_issues = test_issues.iter().map(stats_row).collect::<Vec<_>>();

        let breakdown = compute_priority_breakdown(&test_issues);
        assert_eq!(breakdown.dimension, "priority");

        let mut map: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &breakdown.counts {
            map.insert(entry.key.clone(), entry.count);
        }

        assert_eq!(map.get("P0"), Some(&2));
        assert_eq!(map.get("P3"), Some(&1));
    }

    #[test]
    fn test_compute_assignee_breakdown() {
        let mut test_issues = [
            make_issue("t-1", Status::Open, IssueType::Task),
            make_issue("t-2", Status::Open, IssueType::Task),
            make_issue("t-3", Status::Open, IssueType::Bug),
        ];
        test_issues[0].assignee = Some("alice".to_string());
        test_issues[1].assignee = Some("alice".to_string());
        let test_issues = test_issues.iter().map(stats_row).collect::<Vec<_>>();

        let breakdown = compute_assignee_breakdown(&test_issues);
        assert_eq!(breakdown.dimension, "assignee");

        let mut map: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &breakdown.counts {
            map.insert(entry.key.clone(), entry.count);
        }

        assert_eq!(map.get("alice"), Some(&2));
        assert_eq!(map.get("(unassigned)"), Some(&1));
    }

    #[test]
    fn test_compute_summary_basic() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let first_issue = make_issue("t-1", Status::Open, IssueType::Task);
        let second_issue = make_issue("t-2", Status::InProgress, IssueType::Task);
        let mut third_issue = make_issue("t-3", Status::Closed, IssueType::Bug);
        third_issue.closed_at = Some(Utc::now());

        storage.create_issue(&first_issue, "tester").unwrap();
        storage.create_issue(&second_issue, "tester").unwrap();
        storage.create_issue(&third_issue, "tester").unwrap();

        let all_issues = [&first_issue, &second_issue, &third_issue]
            .into_iter()
            .map(stats_row)
            .collect::<Vec<_>>();
        let summary = compute_test_summary(&storage, &all_issues);

        assert_eq!(summary.total_issues, 3);
        assert_eq!(summary.open_issues, 1);
        assert_eq!(summary.in_progress_issues, 1);
        assert_eq!(summary.closed_issues, 1);
    }

    #[test]
    fn test_blocked_by_blocks_deps() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let blocking_issue = make_issue("t-1", Status::Open, IssueType::Task);
        let dependent_issue = make_issue("t-2", Status::Open, IssueType::Task);

        storage.create_issue(&blocking_issue, "tester").unwrap();
        storage.create_issue(&dependent_issue, "tester").unwrap();
        storage
            .add_dependency("t-2", "t-1", "blocks", "tester")
            .unwrap();

        let all_issues = [&blocking_issue, &dependent_issue]
            .into_iter()
            .map(stats_row)
            .collect::<Vec<_>>();
        let summary = compute_test_summary(&storage, &all_issues);

        assert_eq!(summary.blocked_issues, 1);
        assert_eq!(summary.ready_issues, 1); // t-1 is ready, t-2 is blocked
    }

    #[test]
    fn test_blocked_by_parent_child_deps() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let epic = make_issue("t-epic", Status::Open, IssueType::Epic);
        let child = make_issue("t-child", Status::Open, IssueType::Task);

        storage.create_issue(&epic, "tester").unwrap();
        storage.create_issue(&child, "tester").unwrap();
        storage
            .add_dependency("t-child", "t-epic", "parent-child", "tester")
            .unwrap();

        let all_issues = [&epic, &child]
            .into_iter()
            .map(stats_row)
            .collect::<Vec<_>>();
        let summary = compute_test_summary(&storage, &all_issues);

        assert_eq!(summary.blocked_issues, 1);
    }

    #[test]
    fn test_blocked_by_status_counts_without_dependency_blocker() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let blocked_issue = make_issue("t-1", Status::Blocked, IssueType::Task);

        storage.create_issue(&blocked_issue, "tester").unwrap();

        let all_issues = std::iter::once(&blocked_issue)
            .map(stats_row)
            .collect::<Vec<_>>();
        let summary = compute_test_summary(&storage, &all_issues);

        assert_eq!(summary.blocked_issues, 1);
        assert_eq!(summary.ready_issues, 0);
    }

    #[test]
    fn test_compute_summary_ready_excludes_templates() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let regular_issue = make_issue("t-1", Status::Open, IssueType::Task);
        let mut template_issue = make_issue("t-2", Status::Open, IssueType::Task);
        template_issue.is_template = true;

        storage.create_issue(&regular_issue, "tester").unwrap();
        storage.create_issue(&template_issue, "tester").unwrap();

        let all_issues = [&regular_issue, &template_issue]
            .into_iter()
            .map(stats_row)
            .collect::<Vec<_>>();
        let summary = compute_test_summary(&storage, &all_issues);

        assert_eq!(summary.ready_issues, 1);
    }

    #[test]
    fn test_compute_summary_ready_excludes_wisp_ids() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let regular_issue = make_issue("t-1", Status::Open, IssueType::Task);
        let wisp_issue = make_issue("t-wisp-2", Status::Open, IssueType::Task);

        storage.create_issue(&regular_issue, "tester").unwrap();
        storage.create_issue(&wisp_issue, "tester").unwrap();

        let all_issues = [&regular_issue, &wisp_issue]
            .into_iter()
            .map(stats_row)
            .collect::<Vec<_>>();
        let summary = compute_test_summary(&storage, &all_issues);

        assert_eq!(summary.ready_issues, 1);
    }

    #[test]
    fn test_compute_summary_excludes_template_epics_from_close_eligible_count() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let mut epic = make_issue("bd-epic-template", Status::Open, IssueType::Epic);
        epic.is_template = true;

        let mut child = make_issue("bd-task-closed", Status::Closed, IssueType::Task);
        child.closed_at = Some(Utc::now());

        storage.create_issue(&epic, "tester").unwrap();
        storage.create_issue(&child, "tester").unwrap();
        storage
            .add_dependency(&child.id, &epic.id, "parent-child", "tester")
            .unwrap();

        let all_issues = [&epic, &child]
            .into_iter()
            .map(stats_row)
            .collect::<Vec<_>>();
        let summary = compute_test_summary(&storage, &all_issues);

        assert_eq!(summary.epics_eligible_for_closure, 0);
    }

    #[test]
    fn test_blocked_cleared_when_blocker_closed() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let mut blocking_issue = make_issue("t-1", Status::Closed, IssueType::Task);
        blocking_issue.closed_at = Some(Utc::now());
        let dependent_issue = make_issue("t-2", Status::Open, IssueType::Task);

        storage.create_issue(&blocking_issue, "tester").unwrap();
        storage.create_issue(&dependent_issue, "tester").unwrap();
        storage
            .add_dependency("t-2", "t-1", "blocks", "tester")
            .unwrap();

        let all_issues = [&blocking_issue, &dependent_issue]
            .into_iter()
            .map(stats_row)
            .collect::<Vec<_>>();
        let summary = compute_test_summary(&storage, &all_issues);

        // t-2 should NOT be blocked because t-1 is closed
        assert_eq!(summary.blocked_issues, 0);
    }

    #[test]
    fn test_stats_summary_rows_match_full_summary() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc::now();

        let open_issue = make_issue("t-open", Status::Open, IssueType::Task);
        let in_progress_issue = make_issue("t-progress", Status::InProgress, IssueType::Task);
        let status_blocked_issue = make_issue("t-status-blocked", Status::Blocked, IssueType::Task);
        let blocking_issue = make_issue("t-blocker", Status::Open, IssueType::Task);
        let dependent_issue = make_issue("t-dependent", Status::Open, IssueType::Task);
        let mut closed_issue = make_issue("t-closed", Status::Closed, IssueType::Bug);
        closed_issue.created_at = now - Duration::hours(48);
        closed_issue.closed_at = Some(now);
        let deferred_issue = make_issue("t-deferred", Status::Deferred, IssueType::Task);
        let draft_issue = make_issue("t-draft", Status::Draft, IssueType::Task);
        let mut pinned_issue = make_issue("t-pinned", Status::Open, IssueType::Task);
        pinned_issue.pinned = true;
        let mut template_issue = make_issue("t-template", Status::Open, IssueType::Task);
        template_issue.is_template = true;
        let wisp_issue = make_issue("t-wisp-1", Status::Open, IssueType::Task);
        let epic_issue = make_issue("t-epic", Status::Open, IssueType::Epic);
        let mut epic_child = make_issue("t-epic-child", Status::Closed, IssueType::Task);
        epic_child.created_at = now - Duration::hours(1);
        epic_child.closed_at = Some(now);
        let tombstone_issue = make_issue("t-tombstone", Status::Open, IssueType::Task);

        for issue in [
            &open_issue,
            &in_progress_issue,
            &status_blocked_issue,
            &blocking_issue,
            &dependent_issue,
            &closed_issue,
            &deferred_issue,
            &draft_issue,
            &pinned_issue,
            &template_issue,
            &wisp_issue,
            &epic_issue,
            &epic_child,
            &tombstone_issue,
        ] {
            storage.create_issue(issue, "tester").unwrap();
        }
        storage
            .add_dependency("t-dependent", "t-blocker", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("t-epic-child", "t-epic", "parent-child", "tester")
            .unwrap();
        storage
            .delete_issue("t-tombstone", "tester", "summary parity", None)
            .unwrap();

        let full_rows = storage.list_stats_issues().unwrap();
        let summary_rows = storage.list_stats_summary_issues().unwrap();
        let mut external_blockers = std::collections::HashMap::new();
        external_blockers.insert("t-open".to_string(), vec!["external-1".to_string()]);
        let has_potential_ready_candidates = full_rows
            .iter()
            .any(|issue| is_potential_ready_candidate(issue, &now));
        let full = compute_summary(
            &storage,
            &full_rows,
            Some(&external_blockers),
            &now,
            has_potential_ready_candidates,
        )
        .unwrap();
        let lean = compute_summary(
            &storage,
            &summary_rows,
            Some(&external_blockers),
            &now,
            has_potential_ready_candidates,
        )
        .unwrap();

        assert_eq!(
            serde_json::to_value(&lean).unwrap(),
            serde_json::to_value(&full).unwrap()
        );
    }

    #[test]
    fn test_label_breakdown() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let first_issue = make_issue("t-1", Status::Open, IssueType::Task);
        let second_issue = make_issue("t-2", Status::Open, IssueType::Task);
        let third_issue = make_issue("t-3", Status::Open, IssueType::Task);
        let tombstone_issue = make_issue("t-4", Status::Open, IssueType::Task);
        let mut closed_issue = make_issue("t-5", Status::Closed, IssueType::Task);
        closed_issue.closed_at = Some(Utc::now());
        let mut template_issue = make_issue("t-6", Status::Open, IssueType::Task);
        template_issue.is_template = true;

        storage.create_issue(&first_issue, "tester").unwrap();
        storage.create_issue(&second_issue, "tester").unwrap();
        storage.create_issue(&third_issue, "tester").unwrap();
        storage.create_issue(&tombstone_issue, "tester").unwrap();
        storage.create_issue(&closed_issue, "tester").unwrap();
        storage.create_issue(&template_issue, "tester").unwrap();

        storage.add_label("t-1", "backend", "tester").unwrap();
        storage.add_label("t-1", "urgent", "tester").unwrap();
        storage.add_label("t-2", "backend", "tester").unwrap();
        storage.add_label("t-4", "backend", "tester").unwrap();
        storage.add_label("t-5", "closed", "tester").unwrap();
        storage.add_label("t-6", "template", "tester").unwrap();
        storage
            .delete_issue("t-4", "tester", "tombstone label count target", None)
            .unwrap();

        let test_issues = storage.list_stats_issues().unwrap();
        let breakdown = compute_label_breakdown(&storage, &test_issues).unwrap();

        let mut map: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &breakdown.counts {
            map.insert(entry.key.clone(), entry.count);
        }

        assert_eq!(map.get("backend"), Some(&2));
        assert_eq!(map.get("urgent"), Some(&1));
        assert_eq!(map.get("closed"), Some(&1));
        assert_eq!(map.get("template"), Some(&1));
        assert_eq!(map.get("(no labels)"), Some(&1));
    }

    #[test]
    fn test_truncate_title_ascii() {
        assert_eq!(truncate_title("short", 12), "short");
        assert_eq!(truncate_title("exactly_twelve", 14), "exactly_twelve");
        assert_eq!(
            truncate_title("this_is_too_long_for_column", 12),
            "this_is_t..."
        );
    }

    #[test]
    fn test_truncate_title_multibyte() {
        // Multi-byte characters should not cause panics.
        // 10 chars, 20 visual width; truncate to 5 visual width.
        let emoji = "😊".repeat(10);
        let result = truncate_title(&emoji, 5);
        assert!(result.ends_with("..."));
        // 5 visual width: "😊" (2) + "..." (3) = 5
        assert_eq!(result, "😊...");

        // Mixed ASCII and emoji
        let mixed = "abc😊def";
        // "abc" (3) + "😊" (2) + "def" (3) = 8 width
        assert_eq!(truncate_title(mixed, 8), "abc😊def");
        // "abc" (3) + "..." (3) = 6
        assert_eq!(truncate_title(mixed, 6), "abc...");
    }

    #[test]
    fn test_capitalize() {
        assert_eq!(capitalize("type"), "Type");
        assert_eq!(capitalize("priority"), "Priority");
        assert_eq!(capitalize(""), "");
        assert_eq!(capitalize("ALREADY"), "ALREADY");
    }

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("run git");
        assert!(status.success(), "git {:?} failed", args);
    }

    fn git_stdout(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {:?} failed", args);
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn write_issue_jsonl(path: &Path, issue: &Issue) {
        let line = serde_json::to_string(issue).expect("serialize issue");
        fs::write(path, format!("{line}\n")).expect("write issue jsonl");
    }

    #[test]
    fn test_zero_activity_cache_reused_across_code_only_head_change() {
        let temp = TempDir::new().expect("tempdir");
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "tester@example.com"]);
        git(temp.path(), &["config", "user.name", "Tester"]);

        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let issue = make_issue("bd-cache", Status::Open, IssueType::Task);
        write_issue_jsonl(&jsonl_path, &issue);
        git(temp.path(), &["add", ".beads/issues.jsonl"]);
        git(temp.path(), &["commit", "-q", "-m", "seed issue jsonl"]);
        let cached_head = git_stdout(temp.path(), &["rev-parse", "HEAD"]);

        fs::write(temp.path().join("README.md"), "code-only change\n").expect("write readme");
        git(temp.path(), &["add", "README.md"]);
        git(temp.path(), &["commit", "-q", "-m", "code only"]);
        let repo_ctx = GitRepoContext {
            repo_root: temp.path().to_path_buf(),
            head: git_stdout(temp.path(), &["rev-parse", "HEAD"]),
        };

        let activity = RecentActivity {
            hours_tracked: 24,
            commit_count: 0,
            issues_created: 0,
            issues_closed: 0,
            issues_updated: 0,
            issues_reopened: 0,
            total_changes: 0,
        };
        let cache_entry = RecentActivityCacheEntry {
            repo_root: temp.path().to_string_lossy().into_owned(),
            repo_head: cached_head,
            pathspec: ".beads/issues.jsonl".to_string(),
            hours: 24,
            valid_until_epoch: None,
            activity,
        };
        let raw = serde_json::to_string(&cache_entry).expect("serialize cache entry");

        assert!(
            parse_recent_activity_cache(
                &raw,
                &repo_ctx,
                ".beads/issues.jsonl",
                24,
                Utc::now().timestamp(),
            )
            .is_some(),
            "code-only commits should not invalidate a zero-activity cache"
        );

        let mut changed_issue = issue;
        changed_issue.title = "Issue bd-cache updated".to_string();
        write_issue_jsonl(&jsonl_path, &changed_issue);
        git(temp.path(), &["add", ".beads/issues.jsonl"]);
        git(temp.path(), &["commit", "-q", "-m", "touch issue jsonl"]);
        let repo_ctx = GitRepoContext {
            repo_root: temp.path().to_path_buf(),
            head: git_stdout(temp.path(), &["rev-parse", "HEAD"]),
        };

        assert!(
            parse_recent_activity_cache(
                &raw,
                &repo_ctx,
                ".beads/issues.jsonl",
                24,
                Utc::now().timestamp(),
            )
            .is_none(),
            "issue JSONL changes must force a fresh activity scan"
        );
    }

    #[test]
    fn test_compute_recent_activity_uses_resolved_jsonl_path() {
        let temp = TempDir::new().expect("tempdir");
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "tester@example.com"]);
        git(temp.path(), &["config", "user.name", "Tester"]);

        let jsonl_dir = temp.path().join("tracking").join("custom");
        fs::create_dir_all(&jsonl_dir).expect("create jsonl dir");
        let jsonl_path = jsonl_dir.join("issues.snapshot.jsonl");
        fs::write(&jsonl_path, "{\"id\":\"bd-abc\",\"title\":\"Example\"}\n").expect("write jsonl");

        git(
            temp.path(),
            &["add", "tracking/custom/issues.snapshot.jsonl"],
        );
        git(
            temp.path(),
            &["commit", "-q", "-m", "Track bd-abc in custom issues file"],
        );

        let activity = compute_recent_activity(None, &jsonl_path, 24)
            .expect("activity for committed custom jsonl");
        assert_eq!(activity.commit_count, 1);
        assert_eq!(activity.hours_tracked, 24);
    }

    #[test]
    fn test_compute_recent_activity_counts_issue_transitions_from_git_history() {
        let temp = TempDir::new().expect("tempdir");
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "tester@example.com"]);
        git(temp.path(), &["config", "user.name", "Tester"]);

        let jsonl_dir = temp.path().join(".beads");
        fs::create_dir_all(&jsonl_dir).expect("create beads dir");
        let jsonl_path = jsonl_dir.join("issues.jsonl");

        let base_time = Utc.with_ymd_and_hms(2026, 3, 11, 0, 0, 0).unwrap();
        let mut issue = make_issue("bd-activity", Status::Open, IssueType::Task);
        issue.title = "Track recent activity".to_string();
        issue.created_at = base_time;
        issue.updated_at = base_time;

        write_issue_jsonl(&jsonl_path, &issue);
        git(temp.path(), &["add", ".beads/issues.jsonl"]);
        git(temp.path(), &["commit", "-q", "-m", "Create bd-activity"]);

        issue.title = "Track recent activity better".to_string();
        issue.updated_at = base_time + chrono::Duration::hours(1);
        write_issue_jsonl(&jsonl_path, &issue);
        git(temp.path(), &["add", ".beads/issues.jsonl"]);
        git(temp.path(), &["commit", "-q", "-m", "Update bd-activity"]);

        issue.status = Status::Closed;
        issue.updated_at = base_time + chrono::Duration::hours(2);
        issue.closed_at = Some(issue.updated_at);
        issue.close_reason = Some("done".to_string());
        write_issue_jsonl(&jsonl_path, &issue);
        git(temp.path(), &["add", ".beads/issues.jsonl"]);
        git(temp.path(), &["commit", "-q", "-m", "Close bd-activity"]);

        issue.status = Status::Open;
        issue.updated_at = base_time + chrono::Duration::hours(3);
        issue.closed_at = None;
        issue.close_reason = None;
        write_issue_jsonl(&jsonl_path, &issue);
        git(temp.path(), &["add", ".beads/issues.jsonl"]);
        git(temp.path(), &["commit", "-q", "-m", "Reopen bd-activity"]);

        let activity = compute_recent_activity(None, &jsonl_path, 24).expect("recent activity");
        assert_eq!(activity.commit_count, 4);
        assert_eq!(activity.issues_created, 1);
        assert_eq!(activity.issues_updated, 1);
        assert_eq!(activity.issues_closed, 1);
        assert_eq!(activity.issues_reopened, 1);
        assert_eq!(activity.total_changes, 4);
    }

    #[test]
    fn test_parse_recent_activity_cache_returns_matching_entry() {
        let repo_ctx = GitRepoContext {
            repo_root: PathBuf::from("/tmp/repo"),
            head: "head-1".to_string(),
        };
        let cached_activity = RecentActivity {
            hours_tracked: 24,
            commit_count: 99,
            issues_created: 88,
            issues_closed: 77,
            issues_updated: 66,
            issues_reopened: 55,
            total_changes: 286,
        };
        let cache_entry = RecentActivityCacheEntry {
            repo_root: repo_ctx.repo_root.to_string_lossy().into_owned(),
            repo_head: repo_ctx.head.clone(),
            pathspec: ".beads/issues.jsonl".to_string(),
            hours: 24,
            valid_until_epoch: Some(Utc::now().timestamp() + 3600),
            activity: cached_activity.clone(),
        };
        let raw = serde_json::to_string(&cache_entry).expect("serialize cache entry");

        let activity = parse_recent_activity_cache(
            &raw,
            &repo_ctx,
            ".beads/issues.jsonl",
            24,
            Utc::now().timestamp(),
        )
        .expect("cached recent activity");
        assert_eq!(activity.commit_count, cached_activity.commit_count);
        assert_eq!(activity.total_changes, cached_activity.total_changes);
        assert_eq!(activity.issues_created, cached_activity.issues_created);
        assert_eq!(activity.issues_closed, cached_activity.issues_closed);
    }

    #[test]
    fn test_activity_counts_tombstone_transition_as_update_not_reopen() {
        let mut before = make_issue("bd-closed", Status::Closed, IssueType::Task);
        before.closed_at = Some(Utc::now());

        let mut after = before.clone();
        after.status = Status::Tombstone;
        after.deleted_at = Some(Utc::now());
        after.delete_reason = Some("purged".to_string());

        let mut counts = ActivityCounts::default();
        counts.record_transition(Some(&before), Some(&after));

        assert_eq!(counts.reopened, 0);
        assert_eq!(counts.updated, 1);
    }

    #[test]
    fn test_parse_issue_activity_patch_counts_created_issue() {
        let issue = make_issue("bd-created", Status::Open, IssueType::Task);
        let patch = format!(
            "+{}\n",
            serde_json::to_string(&issue).expect("serialize created issue"),
        );

        let counts = parse_issue_activity_patch(&patch);
        assert_eq!(counts.created, 1);
        assert_eq!(counts.closed, 0);
        assert_eq!(counts.updated, 0);
        assert_eq!(counts.reopened, 0);
        assert_eq!(counts.total_changes(), 1);
    }

    #[test]
    fn test_parse_issue_activity_patch_skips_non_issue_markers() {
        let issue = make_issue("bd-skipped", Status::Open, IssueType::Task);
        let issue_json = serde_json::to_string(&issue).expect("serialize skipped issue");
        let patch = format!(
            "diff --git a/.beads/issues.jsonl b/.beads/issues.jsonl\n\
             @@ -1 +1 @@\n\
             !{issue_json}\n\
             ~{issue_json}\n\
             context line\n"
        );

        assert_eq!(parse_issue_patch_line(&format!("!{issue_json}")), None);
        let counts = parse_issue_activity_patch(&patch);
        assert_eq!(counts.created, 0);
        assert_eq!(counts.closed, 0);
        assert_eq!(counts.updated, 0);
        assert_eq!(counts.reopened, 0);
        assert_eq!(counts.total_changes(), 0);
    }

    #[test]
    fn test_parse_issue_activity_stream_keeps_commit_boundaries_separate() {
        let base_time = Utc.with_ymd_and_hms(2026, 3, 11, 0, 0, 0).unwrap();
        let mut original = make_issue("bd-activity", Status::Open, IssueType::Task);
        original.title = "Track recent activity".to_string();
        original.created_at = base_time;
        original.updated_at = base_time;

        let mut updated = original.clone();
        updated.title = "Track recent activity better".to_string();
        updated.updated_at = base_time + chrono::Duration::hours(1);

        let mut closed = updated.clone();
        closed.status = Status::Closed;
        closed.updated_at = base_time + chrono::Duration::hours(2);
        closed.closed_at = Some(closed.updated_at);
        closed.close_reason = Some("done".to_string());

        let log = format!(
            "{marker}commit-1\t{first_ts}\n-{original}\n+{updated}\n{marker}commit-2\t{second_ts}\n-{updated}\n+{closed}\n",
            marker = GIT_ACTIVITY_COMMIT_MARKER,
            first_ts = base_time.timestamp(),
            second_ts = (base_time + chrono::Duration::hours(2)).timestamp(),
            original = serde_json::to_string(&original).expect("serialize original issue"),
            updated = serde_json::to_string(&updated).expect("serialize updated issue"),
            closed = serde_json::to_string(&closed).expect("serialize closed issue"),
        );

        let activity = parse_issue_activity_stream(std::io::BufReader::new(log.as_bytes()));
        assert_eq!(activity.commit_count, 2);
        assert_eq!(activity.counts.created, 0);
        assert_eq!(activity.counts.updated, 1);
        assert_eq!(activity.counts.closed, 1);
        assert_eq!(activity.counts.reopened, 0);
        assert_eq!(activity.earliest_commit_ts, Some(base_time.timestamp()));
        assert_eq!(activity.counts.total_changes(), 2);
    }

    #[test]
    fn test_recent_activity_cache_valid_until_expires_at_boundary() {
        let earliest_commit_ts = 1_000_i64;
        let policy = recent_activity_cache_policy(1, Some(earliest_commit_ts), 24);
        assert!(
            matches!(policy, RecentActivityCachePolicy::Cache { .. }),
            "expected cache entry"
        );
        let RecentActivityCachePolicy::Cache { valid_until_epoch } = policy else {
            return;
        };
        let valid_until = valid_until_epoch.expect("expiry timestamp");
        assert_eq!(valid_until, earliest_commit_ts + 24 * 3600);

        let repo_ctx = GitRepoContext {
            repo_root: PathBuf::from("/tmp/repo"),
            head: "head-1".to_string(),
        };
        let activity = RecentActivity {
            hours_tracked: 24,
            commit_count: 1,
            issues_created: 1,
            issues_closed: 0,
            issues_updated: 0,
            issues_reopened: 0,
            total_changes: 1,
        };
        let cache_entry = RecentActivityCacheEntry {
            repo_root: repo_ctx.repo_root.to_string_lossy().into_owned(),
            repo_head: repo_ctx.head.clone(),
            pathspec: ".beads/issues.jsonl".to_string(),
            hours: 24,
            valid_until_epoch: Some(valid_until),
            activity,
        };
        let raw = serde_json::to_string(&cache_entry).expect("serialize cache entry");

        assert!(
            parse_recent_activity_cache(
                &raw,
                &repo_ctx,
                ".beads/issues.jsonl",
                24,
                valid_until - 1,
            )
            .is_some()
        );
        assert!(
            parse_recent_activity_cache(&raw, &repo_ctx, ".beads/issues.jsonl", 24, valid_until,)
                .is_some()
        );
        assert!(
            parse_recent_activity_cache(
                &raw,
                &repo_ctx,
                ".beads/issues.jsonl",
                24,
                valid_until + 1,
            )
            .is_none()
        );
    }

    #[test]
    fn test_should_include_activity_defaults_on() {
        assert!(should_include_activity(&StatsArgs::default()));
        assert!(should_include_activity(&StatsArgs {
            activity: true,
            ..StatsArgs::default()
        }));
        assert!(!should_include_activity(&StatsArgs {
            activity: true,
            no_activity: true,
            ..StatsArgs::default()
        }));
    }

    #[test]
    fn test_should_collect_activity_skips_true_quiet_mode() {
        assert!(should_collect_activity(
            &StatsArgs::default(),
            OutputMode::Json
        ));
        assert!(!should_collect_activity(
            &StatsArgs::default(),
            OutputMode::Quiet
        ));
        assert!(!should_collect_activity(
            &StatsArgs {
                no_activity: true,
                ..StatsArgs::default()
            },
            OutputMode::Json
        ));
    }

    #[test]
    fn test_repo_relative_git_path_rejects_path_outside_repo() {
        let temp = TempDir::new().expect("tempdir");
        let repo_root = temp.path().join("repo");
        let outside_root = temp.path().join("outside");
        fs::create_dir_all(&repo_root).expect("create repo root");
        fs::create_dir_all(&outside_root).expect("create outside root");

        let outside_path = outside_root.join("issues.jsonl");
        fs::write(&outside_path, "").expect("write outside jsonl");

        assert!(repo_relative_git_path(&outside_path, &repo_root).is_none());
    }

    #[test]
    fn test_git_pathspec_string_normalizes_separators() {
        assert_eq!(
            git_pathspec_string(&PathBuf::from(".beads").join("issues.jsonl")),
            ".beads/issues.jsonl"
        );
        assert_eq!(
            git_pathspec_string(&PathBuf::from(".beads/issues.jsonl")),
            ".beads/issues.jsonl"
        );
    }

    #[test]
    fn test_git_repo_context_from_filesystem_reads_loose_head() {
        let temp = TempDir::new().expect("tempdir");
        let git_dir = temp.path().join(".git");
        let refs_dir = git_dir.join("refs").join("heads");
        fs::create_dir_all(&refs_dir).expect("create refs");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("write head");
        fs::write(
            refs_dir.join("main"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .expect("write ref");

        let repo_ctx = git_repo_context_from_filesystem(temp.path()).expect("repo context");
        assert_eq!(repo_ctx.repo_root, temp.path());
        assert_eq!(repo_ctx.head, "0123456789abcdef0123456789abcdef01234567");
    }

    #[test]
    fn test_git_repo_context_from_filesystem_reads_gitdir_file() {
        let temp = TempDir::new().expect("tempdir");
        let repo_root = temp.path().join("worktree");
        let git_dir = temp.path().join("actual.git");
        fs::create_dir_all(&repo_root).expect("create worktree");
        fs::create_dir_all(git_dir.join("refs").join("heads")).expect("create refs");
        fs::write(
            repo_root.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .expect("write gitdir");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("write head");
        fs::write(
            git_dir.join("refs").join("heads").join("main"),
            "fedcba9876543210fedcba9876543210fedcba98\n",
        )
        .expect("write ref");

        let repo_ctx = git_repo_context_from_filesystem(&repo_root).expect("repo context");
        assert_eq!(repo_ctx.repo_root, repo_root);
        assert_eq!(repo_ctx.head, "fedcba9876543210fedcba9876543210fedcba98");
    }

    #[test]
    fn test_git_repo_context_reads_root_and_head() {
        let temp = TempDir::new().expect("tempdir");
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "tester@example.com"]);
        git(temp.path(), &["config", "user.name", "Tester"]);
        fs::write(temp.path().join("README.md"), "stats\n").expect("write readme");
        git(temp.path(), &["add", "README.md"]);
        git(temp.path(), &["commit", "-q", "-m", "init"]);

        let repo_ctx = git_repo_context(temp.path()).expect("repo context");
        assert_eq!(repo_ctx.repo_root, temp.path());
        assert_eq!(
            repo_ctx.head,
            git_stdout(temp.path(), &["rev-parse", "HEAD"])
        );
    }
}
